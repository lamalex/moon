use crate::daemon_server_error::DaemonServerError;
use moon_common::{SourceRegistry, format_error_chain};
use moon_file_watcher::*;
use notify_debouncer_full::{new_debouncer, notify::RecursiveMode};
use rustc_hash::FxHashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, trace, warn};

/// Debounce timeout — events within this window are coalesced
const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(500);

/// Directory names that are always ignored by the watcher
static IGNORED_DIRS: LazyLock<FxHashSet<&'static str>> =
    LazyLock::new(|| FxHashSet::from_iter([".git", ".svn", "node_modules"]));

/// Path segments (multi-component) that are ignored
static IGNORED_PATHS: LazyLock<Vec<[&'static str; 2]>> =
    LazyLock::new(|| vec![[".moon", "cache"], [".moon", "docker"]]);

/// Returns `true` if the path should be ignored by the watcher
fn is_ignored(path: &Path) -> bool {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    // Check single-component ignores
    for &dir in IGNORED_DIRS.iter() {
        if components.contains(&dir) {
            return true;
        }
    }

    // Check multi-component path ignores
    for window in components.windows(2) {
        for ignored in IGNORED_PATHS.iter() {
            if window[0] == ignored[0] && window[1] == ignored[1] {
                return true;
            }
        }
    }

    false
}

fn map_notify_error(error: notify_debouncer_full::notify::Error) -> DaemonServerError {
    DaemonServerError::WatcherFailed {
        error: Box::new(error),
    }
}

fn create_file_event(sources: &SourceRegistry, path: &Path, kind: EventKind) -> Option<FileEvent> {
    if is_ignored(path) {
        return None;
    }

    let source_path = match sources.qualify(path) {
        Ok(path) => path,
        Err(error) => {
            warn!(
                path = ?path,
                error = error.to_string(),
                "Ignoring file watcher event that cannot be converted to a relative path"
            );

            return None;
        }
    };

    Some(FileEvent {
        source_id: source_path.source,
        path_original: path.to_owned(),
        path: source_path.path,
        kind,
    })
}

fn get_watch_roots(sources: &SourceRegistry) -> Vec<PathBuf> {
    let mut roots = sources
        .iter()
        .map(|(_, root)| root.to_path_buf())
        .collect::<Vec<_>>();
    roots.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });

    // A recursive parent watch already covers nested source roots. Registering
    // both may deliver the same physical mutation more than once.
    let mut watched = Vec::<PathBuf>::new();

    for root in roots {
        if !watched.iter().any(|parent| root.starts_with(parent)) {
            watched.push(root);
        }
    }

    watched
}

/// Start watching the workspace root for file changes.
///
/// File events are debounced and broadcast on `event_tx`. The watcher
/// runs until `shutdown_rx` receives a message, at which point it
/// drops the underlying OS watcher and returns.
///
/// Errors from the `notify` backend stop the watcher so the daemon can retire
/// instead of serving with incomplete filesystem observations.
pub async fn start_file_watcher(
    sources: Arc<SourceRegistry>,
    event_tx: broadcast::Sender<FileEvent>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> miette::Result<()> {
    let (bridge_tx, mut bridge_rx) = mpsc::channel(512);

    // This closure runs on notify's internal thread
    let mut debouncer = new_debouncer(DEBOUNCE_TIMEOUT, None, move |result| {
        if bridge_tx.blocking_send(result).is_err() {
            // Receiver dropped — watcher is shutting down
        }
    })
    .map_err(map_notify_error)?;

    // Watch every distinct source tree recursively. `notify` sets this up as an
    // efficient watch (one FSEvents stream on macOS; per-directory inotify
    // watches on Linux); events inside ignored directories are filtered out
    // by `create_file_event`. Registering watches per-directory ourselves was
    // untenable — walking a real repo's `target`/build output is tens of
    // thousands of directories and never finishes setup.
    let watch_roots = get_watch_roots(&sources);

    for root in &watch_roots {
        debouncer
            .watch(root, RecursiveMode::Recursive)
            .map_err(map_notify_error)?;
    }

    debug!(roots = ?watch_roots, "File watcher started");

    loop {
        tokio::select! {
            result = bridge_rx.recv() => {
                match result {
                    Some(Ok(events)) => {
                        for event in events {
                            for path in &event.paths {
                                if let Some(file_event) =
                                    create_file_event(&sources, path, event.kind)
                                {
                                    // We only care about mutations, not access, etc
                                    if file_event.is_mutated() {
                                        trace!(
                                            path = ?file_event.path,
                                            kind = ?file_event.kind,
                                            "File change event",
                                        );

                                        // Ignore send failures
                                        let _ = event_tx.send(file_event);
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(errors)) => {
                        let error = errors
                            .into_iter()
                            .next()
                            .expect("Notify must return at least one watcher error");

                        return Err(map_notify_error(error).into());
                    }
                    None => {
                        return Err(miette::miette!("File watcher event bridge stopped unexpectedly"));
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                debug!("File watcher shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Start a file listener that receives file events from `event_rx` and
/// dispatches them to the provided `watchers`. The listener runs until
/// `shutdown_rx` receives a message, at which point it returns.
///
/// Errors from the watchers are logged but do not stop the listener —
/// only a shutdown signal does. Watchers are expected to handle their own internal
/// state and debounce as needed, since file events can arrive in bursts.
pub async fn start_file_listener<T: Clone + Send + 'static>(
    state: T,
    mut watchers: Vec<BoxedFileWatcher<T>>,
    mut event_rx: broadcast::Receiver<FileEvent>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    debug!("File listener started");

    for watcher in watchers.iter_mut() {
        if let Err(error) = watcher.on_init(state.clone()).await {
            error!(error = format_error_chain(&error), "System watcher error");
        }
    }

    loop {
        tokio::select! {
            result = event_rx.recv() => {
                match result {
                    Ok(event) => {
                        for watcher in watchers.iter_mut() {
                            if let Err(error) = watcher.on_file_event(state.clone(), &event).await {
                                error!(error = format_error_chain(&error), "System watcher error");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        warn!("File change event receiver lagged by {count} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        debug!("File listener shutting down");
                        break;
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                debug!("File listener shutting down");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::SourceRootId;

    fn sources() -> SourceRegistry {
        SourceRegistry::single(PathBuf::from("/workspace"))
    }

    #[test]
    fn test_is_ignored_git() {
        assert!(is_ignored(&PathBuf::from("/workspace/.git/objects/abc")));
    }

    #[test]
    fn test_is_ignored_node_modules() {
        assert!(is_ignored(&PathBuf::from(
            "/workspace/node_modules/foo/bar.js"
        )));
    }

    #[test]
    fn test_is_ignored_svn() {
        assert!(is_ignored(&PathBuf::from("/workspace/.svn/entries")));
    }

    #[test]
    fn test_is_ignored_moon_cache() {
        assert!(is_ignored(&PathBuf::from(
            "/workspace/.moon/cache/hashes/abc"
        )));
    }

    #[test]
    fn test_is_ignored_moon_docker() {
        assert!(is_ignored(&PathBuf::from(
            "/workspace/.moon/docker/scaffold"
        )));
    }

    #[test]
    fn test_not_ignored_source_file() {
        assert!(!is_ignored(&PathBuf::from("/workspace/src/main.rs")));
    }

    #[test]
    fn test_not_ignored_moon_config() {
        assert!(!is_ignored(&PathBuf::from(
            "/workspace/.moon/workspace.yml"
        )));
    }

    #[test]
    fn creates_workspace_relative_file_event() {
        let event = create_file_event(
            &sources(),
            Path::new("/workspace/src/main.rs"),
            EventKind::Any,
        )
        .unwrap();

        assert_eq!(event.path.as_str(), "src/main.rs");
        assert_eq!(event.source_id, SourceRootId::primary());
    }

    #[test]
    fn tags_nested_events_with_the_most_specific_source() {
        let mut sources = sources();
        let child = SourceRootId::new("child").unwrap();
        sources
            .register(child.clone(), PathBuf::from("/workspace/packages/child"))
            .unwrap();

        let event = create_file_event(
            &sources,
            Path::new("/workspace/packages/child/src/main.rs"),
            EventKind::Any,
        )
        .unwrap();

        assert_eq!(event.source_id, child);
        assert_eq!(event.path.as_str(), "src/main.rs");
        assert_eq!(get_watch_roots(&sources), vec![PathBuf::from("/workspace")]);
    }

    #[test]
    fn tags_child_root_removal_without_dropping_the_empty_path() {
        let mut sources = sources();
        let child = SourceRootId::new("child").unwrap();
        sources
            .register(child.clone(), PathBuf::from("/workspace/packages/child"))
            .unwrap();

        let event = create_file_event(
            &sources,
            Path::new("/workspace/packages/child"),
            EventKind::Remove(RemoveKind::Folder),
        )
        .unwrap();

        assert_eq!(event.source_id, child);
        assert!(event.path.as_str().is_empty());
        assert!(event.is_source_root_removed_or_renamed());
    }

    #[test]
    fn tags_child_root_rename_without_dropping_the_empty_path() {
        let mut sources = sources();
        let child = SourceRootId::new("child").unwrap();
        sources
            .register(child.clone(), PathBuf::from("/workspace/packages/child"))
            .unwrap();

        let event = create_file_event(
            &sources,
            Path::new("/workspace/packages/child"),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        )
        .unwrap();

        assert_eq!(event.source_id, child);
        assert!(event.path.as_str().is_empty());
        assert!(event.is_source_root_removed_or_renamed());
    }

    #[test]
    fn watches_disjoint_source_roots() {
        let mut sources = sources();
        sources
            .register(
                SourceRootId::new("child").unwrap(),
                PathBuf::from("/other/child"),
            )
            .unwrap();

        assert_eq!(
            get_watch_roots(&sources),
            vec![PathBuf::from("/workspace"), PathBuf::from("/other/child")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn skips_file_event_with_invalid_utf8_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = PathBuf::from(OsStr::from_bytes(b"/workspace/src/\xFF.rs"));

        assert!(create_file_event(&sources(), &path, EventKind::Any).is_none());
    }
}
