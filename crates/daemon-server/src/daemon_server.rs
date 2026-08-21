use crate::daemon_server_error::DaemonServerError;
use crate::daemon_watcher::{start_file_listener, start_file_watcher};
use moon_app_context::{AppContext, SourceRuntimeRegistry, SourceRuntimeRegistryError};
use moon_cache_storage::{
    Manifest, ManifestFile, ManifestSource, ManifestUnpacker, StorageOptions,
};
use moon_common::path::WorkspaceRelativePathBuf;
use moon_common::{SourceRegistry, SourceRootId, color, format_error_chain};
use moon_daemon_proto::{
    moon_daemon_server::{MoonDaemon, MoonDaemonServer},
    *,
};
use moon_daemon_utils::endpoint::*;
use moon_daemon_utils::lock::DaemonLock;
use moon_file_watcher::{BoxedFileWatcher, FileEvent};
use moon_hash::{Digest, InternalDigestExt};
use moon_notifier::notify_webhook;
use moon_process::ProcessRegistry;
use moon_target::TaskKey;
use moon_workspace_graph::WorkspaceGraph;
use starbase_utils::fs;
use std::env;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock, broadcast};
use tokio::time::timeout;
use tokio_util::task::TaskTracker;
use tonic::{Request, Response, Status, transport::Server};
use tracing::{debug, error, info, warn};

/// How often the lifecycle monitor checks whether the daemon should retire.
const MONITOR_INTERVAL: Duration = Duration::from_secs(60);

/// How long the daemon may sit without any RPC before it exits on its own, so
/// an abandoned workspace doesn't leave a daemon running indefinitely.
const IDLE_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Capacity of the file-event broadcast. Sized to absorb a large burst — a
/// branch switch touching many files — without the listener lagging and
/// dropping events, which could miss a config change. The watcher already
/// excludes `node_modules`/`.git`, so the burst is bounded by tracked files.
const EVENT_CHANNEL_CAPACITY: usize = 16_384;

/// Maximum time to wait during shutdown for queued background work to finish.
const BACKGROUND_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to wait for the watcher/listener/monitor tasks to unwind.
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct DaemonState {
    pub app_context: Arc<AppContext>,
    pub sources: Arc<SourceRegistry>,
    pub source_runtime_registry: Arc<SourceRuntimeRegistry>,
    pub topology_changed: Arc<Notify>,
    pub workspace_graph: Arc<WorkspaceGraph>,
}

impl DaemonState {
    pub fn new(app_context: Arc<AppContext>, workspace_graph: Arc<WorkspaceGraph>) -> Self {
        Self {
            sources: Arc::new(SourceRegistry::single(app_context.workspace_root.clone())),
            source_runtime_registry: Arc::new(SourceRuntimeRegistry::single(Arc::clone(
                &app_context,
            ))),
            topology_changed: Arc::new(Notify::new()),
            app_context,
            workspace_graph,
        }
    }
}

pub type AtomicDaemonState = Arc<RwLock<DaemonState>>;

struct DaemonServiceInner {
    endpoint: String,
    pid: u32,
    shutdown_tx: broadcast::Sender<()>,
    started_at: Instant,
    last_activity: Arc<AtomicU64>,
    background: TaskTracker,
}

pub struct DaemonService {
    inner: Arc<DaemonServiceInner>,
    state: AtomicDaemonState,
}

impl DaemonService {
    pub fn new(
        state: AtomicDaemonState,
        endpoint: String,
        pid: u32,
        shutdown_tx: broadcast::Sender<()>,
    ) -> Self {
        Self {
            inner: Arc::new(DaemonServiceInner {
                endpoint,
                pid,
                shutdown_tx,
                started_at: Instant::now(),
                last_activity: Arc::new(AtomicU64::new(0)),
                background: TaskTracker::new(),
            }),
            state,
        }
    }

    fn last_activity(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.inner.last_activity)
    }

    fn track_activity(&self, procedure: &str) {
        debug!("Received {} procedure", color::property(procedure));

        self.inner.last_activity.store(
            self.inner.started_at.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    fn run_in_background<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        debug!("Spawning procedure in the background");

        self.inner.background.spawn(future);
    }

    pub fn background_tasks(&self) -> TaskTracker {
        self.inner.background.clone()
    }
}

fn parse_source_runtime(
    registry: &SourceRuntimeRegistry,
    source_id: &str,
) -> Result<(SourceRootId, Arc<AppContext>), Status> {
    let source_id = source_id
        .parse::<SourceRootId>()
        .map_err(|error| Status::invalid_argument(format!("Invalid source ID: {error}")))?;
    let app_context = registry
        .get(&source_id)
        .map(Arc::clone)
        .map_err(|error| match error {
            SourceRuntimeRegistryError::UnknownSource { .. } => {
                Status::not_found(error.to_string())
            }
            SourceRuntimeRegistryError::UnavailableSource { .. } => {
                Status::failed_precondition(error.to_string())
            }
            _ => Status::internal(error.to_string()),
        })?;

    Ok((source_id, app_context))
}

fn parse_task_key(task_key: &str, source_id: &SourceRootId) -> Result<TaskKey, Status> {
    let task_key = task_key
        .parse::<TaskKey>()
        .map_err(|error| Status::invalid_argument(format!("Invalid task key: {error}")))?;

    if task_key.project_key().source_id() != source_id {
        return Err(Status::invalid_argument(format!(
            "Task key source {} does not match request source {source_id}.",
            task_key.project_key().source_id()
        )));
    }

    Ok(task_key)
}

fn parse_manifest_path(path: &str) -> Result<WorkspaceRelativePathBuf, Status> {
    if Path::new(path).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(Status::invalid_argument(format!(
            "Manifest path must be source-root-relative: {path}"
        )));
    }

    Ok(WorkspaceRelativePathBuf::from(path))
}

fn validate_manifest_paths(manifest: &Manifest) -> Result<(), Status> {
    for file in &manifest.files {
        parse_manifest_path(file.path.as_str())?;
    }

    for link in &manifest.symlinks {
        parse_manifest_path(link.path.as_str())?;
    }

    Ok(())
}

#[tonic::async_trait]
impl MoonDaemon for DaemonService {
    async fn archive_task_outputs(
        &self,
        request: Request<ArchiveTaskOutputsRequest>,
    ) -> Result<Response<ArchiveTaskOutputsResponse>, Status> {
        self.track_activity("ArchiveTaskOutputs");

        let request = request.into_inner();
        let registry = Arc::clone(&self.state.read().await.source_runtime_registry);
        let (source_id, app_context) = parse_source_runtime(&registry, &request.source_id)?;
        let task_key = parse_task_key(&request.task_key, &source_id)?;

        let digest = Digest::from_external(
            request
                .digest
                .ok_or_else(|| Status::invalid_argument("Missing digest"))?,
        )
        .map_err(|error| Status::unknown(error.to_string()))?;
        let mut manifest = Manifest::from_bazel_action_result(
            request
                .manifest
                .ok_or_else(|| Status::invalid_argument("Missing manifest"))?,
        )
        .map_err(|error| Status::unknown(error.to_string()))?;
        validate_manifest_paths(&manifest)?;

        if let Some(source) = request.digest_source {
            let source_digest = Digest::from_external(
                source
                    .digest
                    .ok_or_else(|| Status::invalid_argument("Missing digest source digest"))?,
            )
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

            if source_digest != digest {
                return Err(Status::invalid_argument(
                    "Digest source does not match the task digest.",
                ));
            }

            if Digest::from_bytes(&source.bytes)
                .map_err(|error| Status::invalid_argument(error.to_string()))?
                != source_digest
            {
                return Err(Status::invalid_argument(
                    "Digest source bytes do not match its digest.",
                ));
            }

            manifest.digest_source = Some(ManifestFile {
                bytes: Some(source.bytes),
                digest: Some(source_digest),
                path: parse_manifest_path(&source.path)?,
                ..Default::default()
            });
        }

        self.run_in_background(async move {
            if let Err(error) = app_context
                .cache_engine
                .storage
                .with_options(StorageOptions {
                    include_local: request.include_local,
                    include_remote: request.include_remote,
                    ..Default::default()
                })
                .archive_manifest(&digest, manifest)
                .await
            {
                warn!(
                    task_target = &request.task_target,
                    task_key = %task_key,
                    hash = digest.hash.as_str(),
                    error = format_error_chain(&error),
                    "Failed to archive task outputs",
                );
            }
        });

        Ok(Response::new(ArchiveTaskOutputsResponse { archived: true }))
    }

    async fn hydrate_task_outputs(
        &self,
        request: Request<HydrateTaskOutputsRequest>,
    ) -> Result<Response<HydrateTaskOutputsResponse>, Status> {
        self.track_activity("HydrateTaskOutputs");

        let request = request.into_inner();
        let registry = Arc::clone(&self.state.read().await.source_runtime_registry);
        let (source_id, app_context) = parse_source_runtime(&registry, &request.source_id)?;
        let task_key = parse_task_key(&request.task_key, &source_id)?;

        let digest = Digest::from_external(
            request
                .digest
                .ok_or_else(|| Status::invalid_argument("Missing digest"))?,
        )
        .map_err(|error| Status::unknown(error.to_string()))?;
        let manifest = Manifest::from_bazel_action_result(
            request
                .manifest
                .ok_or_else(|| Status::invalid_argument("Missing manifest"))?,
        )
        .map_err(|error| Status::unknown(error.to_string()))?;
        validate_manifest_paths(&manifest)?;

        let storage = app_context
            .cache_engine
            .storage
            .with_options(StorageOptions {
                include_local: request.include_local,
                include_remote: request.include_remote,
                ..Default::default()
            });

        let backend = storage
            .get_backends()
            .iter()
            .find(|backend| backend.get_id() == &request.backend_id)
            .map(|backend| Arc::clone(backend))
            .ok_or_else(|| Status::invalid_argument("Missing storage backend"))?;

        let source = ManifestSource {
            // This is questionable, but we'll see how it pans out
            remote: backend.get_id().contains("remote") || !backend.get_id().contains("local"),
            backend,
            manifest,
        };

        match storage.hydrate_manifest(&digest, source).await {
            Ok(mut maybe_manifest) => {
                if let Some(manifest) = &mut maybe_manifest {
                    ManifestUnpacker::new(manifest, app_context.workspace_root.clone())
                        .unpack()
                        .map_err(|error| Status::unknown(error.to_string()))?;

                    // Remove file contents from the manifest before returning to the client,
                    // since they are not needed (we just unpacked them) and can be quite large
                    for file in &mut manifest.files {
                        file.bytes = None;
                    }
                }

                Ok(Response::new(HydrateTaskOutputsResponse {
                    hydrated: maybe_manifest.is_some(),
                    // Keep stdout/stderr as it's required for hydrating the terminal output
                    manifest: maybe_manifest
                        .map(|manifest| manifest.into_bazel_action_result(true)),
                }))
            }
            Err(error) => {
                warn!(
                    task_target = &request.task_target,
                    task_key = %task_key,
                    hash = digest.hash.as_str(),
                    error = format_error_chain(&error),
                    "Failed to hydrate task outputs",
                );

                Ok(Response::new(HydrateTaskOutputsResponse {
                    hydrated: false,
                    manifest: None,
                }))
            }
        }
    }

    async fn clean_cache(
        &self,
        request: Request<CleanCacheRequest>,
    ) -> Result<Response<CleanCacheResponse>, Status> {
        self.track_activity("CleanCache");

        let request = request.into_inner();
        let registry = Arc::clone(&self.state.read().await.source_runtime_registry);
        let (_, app_context) = parse_source_runtime(&registry, &request.source_id)?;

        let (files_deleted, bytes_saved) = app_context
            .cache_engine
            .clean_stale_cache(&request.lifetime, request.all)
            .await
            .map_err(|error| Status::unknown(error.to_string()))?;

        Ok(Response::new(CleanCacheResponse {
            files_deleted: files_deleted as u32,
            bytes_saved,
        }))
    }

    async fn hash_files(
        &self,
        request: Request<HashFilesRequest>,
    ) -> Result<Response<HashFilesResponse>, Status> {
        self.track_activity("HashFiles");

        let request = request.into_inner();
        let registry = Arc::clone(&self.state.read().await.source_runtime_registry);
        let (_, app_context) = parse_source_runtime(&registry, &request.source_id)?;
        let files = request
            .files
            .into_iter()
            .map(|file| parse_manifest_path(&file))
            .collect::<Result<Vec<_>, _>>()?;

        let hashed_files = app_context
            .hash_files(&files)
            .await
            .map_err(|error| Status::unknown(error.to_string()))?;

        Ok(Response::new(HashFilesResponse {
            files: hashed_files
                .into_iter()
                .map(|(path, hash)| (path.to_string(), hash))
                .collect(),
        }))
    }

    async fn send_webhook(
        &self,
        request: Request<SendWebhookRequest>,
    ) -> Result<Response<SendWebhookResponse>, Status> {
        self.track_activity("SendWebhook");

        let SendWebhookRequest { url, body } = request.into_inner();

        self.run_in_background(async move {
            match notify_webhook(&url, body, false).await {
                Ok(response) if !response.status().is_success() => {
                    warn!(
                        url = &url,
                        status = response.status().as_u16(),
                        "Webhook endpoint responded with a failure"
                    );
                }
                Err(error) => {
                    warn!(
                        url = &url,
                        error = error.to_string(),
                        "Failed to send webhook"
                    );
                }
                _ => {}
            }
        });

        Ok(Response::new(SendWebhookResponse { success: true }))
    }

    async fn start(
        &self,
        _request: Request<StartRequest>,
    ) -> Result<Response<StartResponse>, Status> {
        self.track_activity("Start");

        Ok(Response::new(StartResponse {
            already_running: true,
            endpoint: self.inner.endpoint.clone(),
            pid: self.inner.pid,
        }))
    }

    async fn stop(&self, _request: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        self.track_activity("Stop");

        self.inner
            .shutdown_tx
            .send(())
            .map_err(|_| Status::internal("Failed to send shutdown signal"))?;

        Ok(Response::new(StopResponse { stopped: true }))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        self.track_activity("Status");

        let state = self.state.read().await;
        let uptime_secs = self.inner.started_at.elapsed().as_secs();

        Ok(Response::new(StatusResponse {
            endpoint: self.inner.endpoint.clone(),
            moon_version: state.app_context.cli_version.to_string(),
            pid: self.inner.pid,
            protocol_version: PROTOCOL_VERSION,
            running: true,
            uptime_secs,
            workspace_root: state.app_context.workspace_root.to_string_lossy().into(),
        }))
    }
}

/// Start the gRPC daemon server, listening on a platform-specific endpoint.
///
/// - Unix: binds a Unix domain socket
/// - Windows: creates a named pipe server
///
/// The server shuts down cleanly on:
/// - A `Stop` RPC call from a client
/// - `SIGINT` or `SIGTERM` (Unix) / `Ctrl+C` (Windows)
///
/// On shutdown the state file and socket are removed and the ownership lock
/// is released.
pub async fn start_daemon_server(
    state: DaemonState,
    watchers: Vec<BoxedFileWatcher<AtomicDaemonState>>,
) -> miette::Result<()> {
    let daemon_dir = state.app_context.daemon_dir.clone();
    let sources = Arc::clone(&state.sources);
    let topology_changed = Arc::clone(&state.topology_changed);
    let version = state.app_context.cli_version.to_string();
    let endpoint = get_endpoint(&daemon_dir);

    fs::create_dir_all(&daemon_dir)?;

    // Take exclusive ownership of this workspace's daemon. The lock is held
    // for our entire lifetime and released automatically when we exit — even
    // on a crash — so the running daemon is whoever holds it, not a PID we'd
    // have to probe. If another daemon already owns it, defer to it.
    let _lock = match DaemonLock::try_acquire(&get_lock_path(&daemon_dir)).map_err(|error| {
        DaemonServerError::EndpointBindFailed {
            endpoint: endpoint.clone(),
            error: Box::new(error),
        }
    })? {
        Some(lock) => lock,
        None => {
            info!("Another daemon already owns this workspace, exiting");

            return Ok(());
        }
    };

    // We own the workspace now, so any leftover socket is stale (no live
    // owner could still hold the lock) and safe to remove before binding.
    #[cfg(unix)]
    {
        let sock = std::path::Path::new(&endpoint);

        if sock.exists() {
            fs::remove_file(sock)?;
        }
    }

    // Move out of the workspace so we don't pin it — on Windows an open working
    // directory blocks the folder from being deleted or renamed. Everything
    // uses the explicit workspace root, not the process cwd.
    if let Err(error) = env::set_current_dir(env::temp_dir()) {
        warn!("Failed to move out of the workspace directory: {error}");
    }

    let pid = std::process::id();

    write_state(&daemon_dir, DaemonInfo::new(pid, version, endpoint.clone()))?;

    // Create a new atomic state
    let atomic_state = Arc::new(RwLock::new(state));

    // Single broadcast channel for shutdown
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);
    let mut signal_rx = ProcessRegistry::instance().receive_signal();

    // Spawn the file watcher and listener in the background
    let (event_tx, event_rx) = broadcast::channel::<FileEvent>(EVENT_CHANNEL_CAPACITY);
    let watcher_handle = tokio::spawn(start_file_watcher(
        Arc::clone(&sources),
        event_tx,
        shutdown_tx.subscribe(),
    ));
    let watcher_abort_handle = watcher_handle.abort_handle();
    let listener_handle = tokio::spawn(start_file_listener(
        atomic_state.clone(),
        watchers,
        event_rx,
        shutdown_tx.subscribe(),
    ));

    // Create the gRPC service
    let service = DaemonService::new(atomic_state, endpoint.clone(), pid, shutdown_tx.clone());
    let service_background = service.background_tasks();

    // Retire the daemon on its own when the workspace disappears or it goes
    // unused, so an abandoned workspace doesn't leak a daemon forever.
    let monitor_handle = tokio::spawn(monitor_lifecycle(
        sources,
        service.last_activity(),
        shutdown_tx.clone(),
        shutdown_tx.subscribe(),
    ));

    // Merge the RPC-driven shutdown with OS signals so the daemon
    // cleans up regardless of how it is stopped
    let shutdown_signal = async move {
        tokio::select! {
            biased;

            _ = shutdown_rx.recv() => {
                info!("Shutdown requested via RPC");
            }
            _ = signal_rx.recv() => {
                // Broadcast so the watcher also receives it
                let _ = shutdown_tx.send(());

                info!("Shutdown requested via OS signal");
            }
            _ = topology_changed.notified() => {
                let _ = shutdown_tx.send(());

                info!("Daemon restarting because the source root topology changed");
            }
            _ = supervise_file_watcher(watcher_handle, shutdown_tx.clone()) => {}
        }
    };

    info!(pid, endpoint, "Daemon server starting");

    let serve_result = serve(&endpoint, service, shutdown_signal).await;

    if let Err(error) = &serve_result {
        error!(error = format_error_chain(error), "Daemon server failed");
    }

    // Stop the background tasks. Abort them rather than only signalling and
    // awaiting: these tasks hold no critical state, and aborting guarantees
    // shutdown can't hang on one that's slow to observe the signal — which
    // would strand the daemon holding its lock but no longer serving, wedging
    // the workspace and blocking every later start.
    watcher_abort_handle.abort();
    listener_handle.abort();
    monitor_handle.abort();

    // Give the aborted tasks a moment to unwind, but don't block shutdown on a
    // slow teardown (dropping a recursive OS watch over a large tree can't be
    // preempted). Past this bound we exit anyway and let the OS clean up.
    let _ = timeout(TASK_SHUTDOWN_TIMEOUT, async {
        let _ = listener_handle.await;
        let _ = monitor_handle.await;
    })
    .await;

    // Drain queued background work before exiting, but don't let a stuck task
    // (e.g. a webhook to an unreachable host) hang shutdown. `wait` only
    // resolves once the tracker is closed, so close it first — otherwise the
    // daemon hangs here forever, holding its lock but no longer serving, which
    // wedges the workspace and blocks the next start.
    service_background.close();

    if timeout(BACKGROUND_DRAIN_TIMEOUT, service_background.wait())
        .await
        .is_err()
    {
        warn!("Timed out draining background work during shutdown");
    }

    info!("Daemon server stopped");

    // Remove our endpoint files, then release the lock as `_ownership` drops.
    let _ = cleanup_daemon_files(&daemon_dir);

    serve_result
}

async fn supervise_file_watcher(
    watcher_handle: tokio::task::JoinHandle<miette::Result<()>>,
    shutdown_tx: broadcast::Sender<()>,
) {
    match watcher_handle.await {
        Ok(Ok(())) => warn!("File watcher stopped unexpectedly"),
        Ok(Err(error)) => error!(
            error = format_error_chain(&error),
            "File watcher failed unexpectedly"
        ),
        Err(error) => error!(error = %error, "File watcher task failed unexpectedly"),
    }

    let _ = shutdown_tx.send(());
}

fn get_missing_source_root(sources: &SourceRegistry) -> Option<(SourceRootId, PathBuf)> {
    sources
        .iter()
        .find(|(_, root)| !root.exists())
        .map(|(source_id, root)| (source_id.clone(), root.to_path_buf()))
}

/// Retire the daemon when any source root is deleted or it goes unused for
/// [`IDLE_TTL`], by triggering the shared shutdown. Runs until shutdown.
async fn monitor_lifecycle(
    sources: Arc<SourceRegistry>,
    last_activity: Arc<AtomicU64>,
    shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    // The daemon started ~now, and `last_activity` is measured from the same
    // point, so `reference.elapsed() - last_activity` is the idle duration.
    let reference = Instant::now();
    let mut interval = tokio::time::interval(MONITOR_INTERVAL);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let idle = reference
                    .elapsed()
                    .saturating_sub(Duration::from_millis(last_activity.load(Ordering::Relaxed)));

                if let Some((source_id, root)) = get_missing_source_root(&sources) {
                    info!(source = %source_id, path = ?root, "Daemon shutting down because a source root was removed");
                } else if idle >= IDLE_TTL {
                    info!("Daemon shutting down because it has been idle too long");
                } else {
                    continue;
                }

                let _ = shutdown_tx.send(());
                break;
            }
            _ = shutdown_rx.recv() => {
                break;
            }
        }
    }
}

pub async fn serve(
    endpoint: &str,
    service: DaemonService,
    shutdown_signal: impl std::future::Future<Output = ()>,
) -> miette::Result<()> {
    #[cfg(unix)]
    {
        serve_unix(endpoint, service, shutdown_signal).await
    }

    #[cfg(windows)]
    {
        serve_windows(endpoint, service, shutdown_signal).await
    }
}

#[cfg(unix)]
pub async fn serve_unix(
    endpoint: &str,
    service: DaemonService,
    shutdown_signal: impl std::future::Future<Output = ()>,
) -> miette::Result<()> {
    use moon_daemon_utils::sys::UnixListenerStream;
    use tokio::net::UnixListener;

    let listener =
        UnixListener::bind(endpoint).map_err(|error| DaemonServerError::EndpointBindFailed {
            endpoint: endpoint.to_owned(),
            error: Box::new(error),
        })?;

    let incoming = UnixListenerStream::new(listener);

    Server::builder()
        .serve_with_incoming_shutdown(MoonDaemonServer::new(service), incoming, shutdown_signal)
        .await
        .map_err(|error| DaemonServerError::ServerFailed {
            error: Box::new(error),
        })?;

    Ok(())
}

#[cfg(windows)]
pub async fn serve_windows(
    endpoint: &str,
    service: DaemonService,
    shutdown_signal: impl std::future::Future<Output = ()>,
) -> miette::Result<()> {
    use moon_daemon_utils::sys::get_named_pipe_server_stream;

    Server::builder()
        .serve_with_incoming_shutdown(
            MoonDaemonServer::new(service),
            get_named_pipe_server_stream(endpoint),
            shutdown_signal,
        )
        .await
        .map_err(|error| DaemonServerError::ServerFailed {
            error: Box::new(error),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_source_registry() -> (PathBuf, SourceRegistry, SourceRootId) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            env::temp_dir().join(format!("moon-daemon-roots-{}-{unique}", std::process::id()));
        let primary_root = base.join("primary");
        let child_root = base.join("child");
        std::fs::create_dir_all(&primary_root).unwrap();
        std::fs::create_dir_all(&child_root).unwrap();

        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::single(primary_root);
        sources.register(child_id.clone(), child_root).unwrap();

        (base, sources, child_id)
    }

    #[test]
    fn detects_removed_child_source_root() {
        let (base, sources, child_id) = create_source_registry();
        std::fs::remove_dir_all(sources.get(&child_id).unwrap()).unwrap();

        assert_eq!(get_missing_source_root(&sources).unwrap().0, child_id);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn detects_renamed_child_source_root() {
        let (base, sources, child_id) = create_source_registry();
        std::fs::rename(sources.get(&child_id).unwrap(), base.join("renamed-child")).unwrap();

        assert_eq!(get_missing_source_root(&sources).unwrap().0, child_id);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn unexpected_watcher_failure_requests_shutdown() {
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let watcher_handle = tokio::spawn(async { Err(miette::miette!("watch failed")) });

        supervise_file_watcher(watcher_handle, shutdown_tx).await;

        tokio::time::timeout(Duration::from_secs(1), shutdown_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
}
