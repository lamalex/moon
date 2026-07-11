//! PROTOTYPE: Interactive driver for the VCS WASM overlay seam.
//!
//! Question: can Moon load a packageable overlay, merge partial `jj` state
//! over Git, and route change queries while keeping file hashing host-owned?

mod benchmark;
mod conformance;
mod model;
mod plugin;
mod policy;

use miette::{IntoDiagnostic, miette};
use model::*;
use moon_pdk_api::*;
use moon_vcs::{ChangedStatus, Vcs, git::Git};
use plugin::load_prototype_plugin;
use serde::Serialize;
use std::io;

#[derive(Debug, Serialize)]
struct ScreenState {
    question: &'static str,
    plugin: VcsPluginMetadata,
    detection: DetectVcsOutput,
    forced_fallback: bool,
    prepared: Option<PreparedVcs>,
    prepared_consistency: Option<VcsConsistency>,
    base: AdapterState,
    overlay: Option<VcsStatePatch>,
    effective: AdapterState,
    last_query: Option<VcsChangeQuery>,
    change_source: Option<String>,
    changed_files: Vec<VcsChangedFile>,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let workspace_root = std::env::current_dir().into_diagnostic()?;
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    match args.as_slice() {
        [arg] if arg == "--conformance" => return conformance::run(&workspace_root).await,
        [arg] if arg == "--benchmark" => return benchmark::run(&workspace_root, false).await,
        [arg] if arg == "--benchmark-check" => return benchmark::run(&workspace_root, true).await,
        [
            arg,
            master_binary,
            current_binary,
            master_fixture,
            current_fixture,
        ] if arg == "--benchmark-git-comparison" => {
            return benchmark::run_git_comparison(
                std::path::Path::new(master_binary),
                std::path::Path::new(current_binary),
                std::path::Path::new(master_fixture),
                std::path::Path::new(current_fixture),
            );
        }
        [arg] if arg == "--user-policy" => return policy::run_user_policy(&workspace_root).await,
        [arg] if arg == "--user-policy-enable" => {
            return policy::set_user_policy_enabled(true);
        }
        [arg] if arg == "--user-policy-disable" => {
            return policy::set_user_policy_enabled(false);
        }
        [arg, plugin_file] if arg == "--user-policy-install-local" => {
            return policy::install_local_user_policy(std::path::Path::new(plugin_file));
        }
        [arg, manifest, signature, public_key] if arg == "--user-policy-install-signed" => {
            return policy::install_signed_user_policy(
                std::path::Path::new(manifest),
                std::path::Path::new(signature),
                std::path::Path::new(public_key),
            );
        }
        [arg, locator, sha256] if arg == "--user-policy-install" => {
            return policy::install_user_policy(locator, sha256);
        }
        [] => {}
        _ => return Err(miette!("unknown prototype arguments: {}", args.join(" "))),
    }

    let default_branch = std::env::var("MOON_DEFAULT_BRANCH").unwrap_or_else(|_| "master".into());
    let plugin = load_prototype_plugin(&workspace_root, &workspace_root).await?;
    let context = MoonContext {
        working_dir: plugin.to_virtual_path(&workspace_root),
        workspace_root: plugin.to_virtual_path(&workspace_root),
    };
    let git = Git::load(
        &workspace_root,
        &default_branch,
        &["origin".into(), "upstream".into()],
    )?;
    let base = load_base_state(&git).await?;
    let detection = plugin
        .detect(DetectVcsInput {
            context: context.clone(),
        })
        .await?;
    let prepared = if detection.active {
        Some(
            plugin
                .prepare(PrepareVcsInput {
                    context: context.clone(),
                    consistency: VcsConsistency::ExistingSnapshot,
                })
                .await?,
        )
    } else {
        None
    };

    let mut screen = ScreenState {
        question: "Can a WASM VCS overlay replace jj-sensitive facts while Git remains the fallback?",
        plugin: plugin.metadata.clone(),
        forced_fallback: false,
        prepared,
        prepared_consistency: detection.active.then_some(VcsConsistency::ExistingSnapshot),
        effective: base.clone(),
        base,
        detection,
        overlay: None,
        last_query: None,
        change_source: None,
        changed_files: vec![],
    };

    loop {
        render(&screen)?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).into_diagnostic()?;

        match input.trim() {
            "d" => {
                screen.detection = plugin
                    .detect(DetectVcsInput {
                        context: context.clone(),
                    })
                    .await?;

                if !screen.detection.active {
                    screen.prepared = None;
                    screen.prepared_consistency = None;
                    screen.overlay = None;
                    screen.effective = screen.base.clone();
                }
            }
            "f" => {
                screen.forced_fallback = !screen.forced_fallback;
                screen.effective = compose_state(
                    &screen.base,
                    active(&screen).then_some(screen.overlay.as_ref()).flatten(),
                );
            }
            "e" | "n" => {
                let consistency = if input.trim() == "n" {
                    VcsConsistency::FreshSnapshot
                } else {
                    VcsConsistency::ExistingSnapshot
                };
                screen.prepared = if active(&screen) {
                    Some(
                        plugin
                            .prepare(PrepareVcsInput {
                                context: context.clone(),
                                consistency,
                            })
                            .await?,
                    )
                } else {
                    None
                };
                screen.prepared_consistency = screen.prepared.as_ref().map(|_| consistency);
                screen.overlay = None;
                screen.effective = screen.base.clone();
                screen.changed_files.clear();
                screen.last_query = None;
                screen.change_source = None;
            }
            "s" => {
                screen.overlay = if active(&screen) {
                    if let Some(prepared) = &screen.prepared {
                        Some(
                            plugin
                                .get_state(GetVcsStateInput {
                                    context: context.clone(),
                                    default_branch: default_branch.clone(),
                                    snapshot_id: prepared.snapshot_id.clone(),
                                })
                                .await?,
                        )
                    } else {
                        None
                    }
                } else {
                    None
                };
                screen.effective = compose_state(&screen.base, screen.overlay.as_ref());
            }
            "w" | "p" | "b" => {
                let query = match input.trim() {
                    "p" => VcsChangeQuery::Previous {
                        revision: VcsRevision::Current,
                    },
                    "b" => VcsChangeQuery::Between {
                        base: VcsRevision::Default,
                        head: VcsRevision::Current,
                    },
                    _ => VcsChangeQuery::WorkingCopy,
                };

                let snapshot_id = screen
                    .prepared
                    .as_ref()
                    .map(|prepared| prepared.snapshot_id.clone());

                screen.changed_files = if active(&screen)
                    && let Some(snapshot_id) = snapshot_id
                {
                    screen.change_source = Some("jj WASM overlay".into());
                    plugin
                        .get_changed_files(GetVcsChangedFilesInput {
                            context: context.clone(),
                            default_branch: default_branch.clone(),
                            query: query.clone(),
                            snapshot_id,
                        })
                        .await?
                        .files
                } else {
                    screen.change_source = Some("Git fallback".into());
                    load_git_changes(&git, &query, &default_branch).await?
                };
                screen.last_query = Some(query);
            }
            "q" => break,
            _ => {}
        }
    }

    Ok(())
}

fn active(screen: &ScreenState) -> bool {
    screen.detection.active && !screen.forced_fallback
}

async fn load_base_state(git: &Git) -> miette::Result<AdapterState> {
    let current_label = git.get_local_branch().await?.to_string();
    let default_label = git.get_default_branch().await?.to_string();

    Ok(AdapterState {
        adapter: "git".into(),
        current_revision: git.get_local_branch_revision().await?.to_string(),
        default_revision: git.get_default_branch_revision().await?.to_string(),
        is_default: git.is_default_branch(&current_label),
        repository_root: git.get_repository_root().await?.display().to_string(),
        working_root: git.get_working_root().await?.display().to_string(),
        current_label,
        default_label,
    })
}

async fn load_git_changes(
    git: &Git,
    query: &VcsChangeQuery,
    default_branch: &str,
) -> miette::Result<Vec<VcsChangedFile>> {
    let changes = match query {
        VcsChangeQuery::WorkingCopy => git.get_changed_files().await?,
        VcsChangeQuery::Previous { revision } => {
            git.get_changed_files_against_previous_revision(&resolve_git_revision(
                revision,
                default_branch,
            ))
            .await?
        }
        VcsChangeQuery::Between { base, head } => {
            git.get_changed_files_between_revisions(
                &resolve_git_revision(base, default_branch),
                &resolve_git_revision(head, default_branch),
            )
            .await?
        }
    };

    Ok(changes
        .files
        .into_iter()
        .map(|(path, statuses)| VcsChangedFile {
            path: path.to_string(),
            status: if statuses.contains(&ChangedStatus::Added) {
                VcsChangedStatus::Added
            } else if statuses.contains(&ChangedStatus::Deleted) {
                VcsChangedStatus::Deleted
            } else {
                VcsChangedStatus::Modified
            },
        })
        .collect())
}

fn resolve_git_revision(revision: &VcsRevision, default_branch: &str) -> String {
    match revision {
        VcsRevision::Current => "HEAD".into(),
        VcsRevision::Default => default_branch.into(),
        VcsRevision::Named(value) => value.clone(),
    }
}

fn render(screen: &ScreenState) -> miette::Result<()> {
    print!("\x1b[2J\x1b[H");
    println!("\x1b[1mVCS overlay prototype\x1b[0m");
    println!("\x1b[2m{}\x1b[0m\n", screen.question);
    println!(
        "{}",
        serde_json::to_string_pretty(screen).into_diagnostic()?
    );
    println!();
    println!(
        "[d] detect  [e] prepare existing  [n] prepare fresh  [s] pinned state  [w] working changes  [p] previous  [b] base..current  [f] force fallback  [q] quit"
    );

    Ok(())
}
