use crate::app_error::AppError;
use crate::{
    DiscoveredWorkspace, DiscoveredWorkspaceFailure, SourceContext, SourceLoadFailure,
    WorkspaceDiscovery,
};
use miette::{Context, IntoDiagnostic};
use moon_common::path::{clean_components, locate_config_dir};
use moon_common::{SourceAlias, SourceRegistry, SourceRootId};
use moon_config::{ExtensionsConfig, InheritedTasksManager, ToolchainsConfig, WorkspaceConfig};
use moon_config_loader::ConfigLoader;
use moon_env::MoonEnvironment;
use moon_env_var::GlobalEnvBag;
use moon_feature_flags::FeatureFlags;
use proto_core::ProtoEnvironment;
use rustc_hash::FxHashMap;
use starbase_styles::color;
use starbase_utils::dirs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::spawn;
use tokio::task::{JoinError, block_in_place};
use tracing::{debug, instrument};

// We need to load configuration in a blocking task, because config
// loading is synchronous but uses `reqwest::blocking` under the hood,
// which triggers a panic when used in an async context...
async fn load_config_blocking<F, R>(func: F) -> Result<R, JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn(async { block_in_place(func) }).await
}

/// Recursively attempt to find the workspace root by locating the ".moon"
/// configuration folder, starting from the current working directory.
#[instrument]
pub fn find_workspace_root(working_dir: &Path) -> miette::Result<PathBuf> {
    debug!(
        working_dir = ?working_dir,
        "Attempting to find workspace root from current working directory",
    );

    let workspace_root = if let Some(root) = GlobalEnvBag::instance().get("MOON_WORKSPACE_ROOT") {
        debug!(
            env_var = root,
            "Inheriting from {} environment variable",
            color::symbol("MOON_WORKSPACE_ROOT")
        );

        let root: PathBuf = root
            .parse()
            .map_err(|_| AppError::InvalidWorkspaceRootEnvVar)?;
        let root = if root.is_absolute() {
            clean_components(root)
        } else {
            clean_components(working_dir.join(root))
        };

        if !locate_config_dir(&root).exists() {
            return Err(AppError::MissingConfigDir.into());
        }

        root
    } else {
        let mut current_dir = Some(working_dir);

        loop {
            if let Some(dir) = current_dir {
                if locate_config_dir(dir).exists() {
                    break dir.to_path_buf();
                } else {
                    current_dir = dir.parent();
                }
            } else {
                return Err(AppError::MissingConfigDir.into());
            }
        }
    };

    // Avoid finding the ~/.moon directory
    let home_dir = dirs::home_dir().ok_or(AppError::MissingHomeDir)?;

    if home_dir == workspace_root {
        return Err(AppError::MissingConfigDir.into());
    }

    debug!(
        workspace_root = ?workspace_root,
        working_dir = ?working_dir,
        "Found workspace root",
    );

    Ok(workspace_root)
}

/// Detect information for moon from the environment.
#[instrument]
pub fn detect_moon_environment(
    working_dir: &Path,
    workspace_root: &Path,
) -> miette::Result<Arc<MoonEnvironment>> {
    let mut env = MoonEnvironment::new()?;
    env.working_dir = working_dir.to_path_buf();
    env.workspace_root = workspace_root.to_path_buf();

    Ok(Arc::new(env))
}

/// Detect information for proto from the environment.
#[instrument]
pub fn detect_proto_environment(
    working_dir: &Path,
    _workspace_root: &Path,
) -> miette::Result<Arc<ProtoEnvironment>> {
    let mut env = ProtoEnvironment::new()?;
    env.working_dir = working_dir.to_path_buf();

    Ok(Arc::new(env))
}

/// Load the workspace configuration file from the `.moon` directory in the workspace root.
/// This file is required to exist, so error if not found.
#[instrument(skip(config_loader))]
pub async fn load_workspace_config(
    config_loader: ConfigLoader,
    workspace_root: &Path,
) -> miette::Result<Arc<WorkspaceConfig>> {
    let config_name = config_loader.get_debug_label_root("workspace");

    debug!("Loading {} (required)", color::file(&config_name));

    let config_files = config_loader.get_workspace_files();

    if config_files.iter().all(|file| !file.exists()) {
        return Err(AppError::MissingConfigFile(config_name).into());
    }

    let root = workspace_root.to_owned();
    let config = load_config_blocking(move || config_loader.load_workspace_config(root))
        .await
        .into_diagnostic()??;

    Ok(Arc::new(config))
}

/// Discover direct workspace declarations without traversing declarations in child workspaces.
pub async fn discover_workspaces(
    config_loader: &ConfigLoader,
    primary_root: &Path,
    primary_config: Arc<WorkspaceConfig>,
) -> miette::Result<WorkspaceDiscovery> {
    discover_workspaces_internal(config_loader, primary_root, primary_config, false).await
}

/// Discover workspaces while retaining child failures for the diagnostics command.
pub async fn discover_workspaces_for_diagnostics(
    config_loader: &ConfigLoader,
    primary_root: &Path,
    primary_config: Arc<WorkspaceConfig>,
) -> miette::Result<WorkspaceDiscovery> {
    discover_workspaces_internal(config_loader, primary_root, primary_config, true).await
}

async fn discover_workspaces_internal(
    config_loader: &ConfigLoader,
    primary_root: &Path,
    primary_config: Arc<WorkspaceConfig>,
    retain_failures: bool,
) -> miette::Result<WorkspaceDiscovery> {
    let primary_id = match &primary_config.id {
        Some(id) => SourceRootId::new(id.as_str())?,
        None => SourceRootId::primary(),
    };
    let mut sources = SourceRegistry::new(primary_id.clone(), primary_root.to_path_buf());
    let mut aliases = FxHashMap::default();
    let mut failures = Vec::new();
    let mut workspaces = FxHashMap::default();
    let primary_physical_root = primary_root
        .canonicalize()
        .into_diagnostic()
        .wrap_err("Failed to resolve the primary workspace root")?;
    let mut physical_roots = FxHashMap::default();
    let mut identity_roots = FxHashMap::default();

    physical_roots.insert(primary_physical_root.clone(), primary_id.clone());
    identity_roots.insert(primary_id.clone(), primary_physical_root);

    workspaces.insert(
        primary_id.clone(),
        DiscoveredWorkspace {
            config_dir: config_loader.dir.clone(),
            id: primary_id,
            root: primary_root.to_path_buf(),
            workspace_config: Arc::clone(&primary_config),
        },
    );

    let mut declarations = primary_config.workspaces.iter().collect::<Vec<_>>();
    declarations.sort_by_key(|(alias, _)| *alias);

    for (alias, declaration) in declarations {
        let alias = SourceAlias::new(alias.as_str())?;
        let root = clean_components(primary_root.join(&declaration.path));
        let physical_root =
            match root.canonicalize().into_diagnostic().wrap_err_with(|| {
                format!("Failed to resolve discovered workspace {alias} at {root:?}")
            }) {
                Ok(root) => root,
                Err(error) if retain_failures => {
                    failures.push(DiscoveredWorkspaceFailure {
                        alias,
                        message: error.to_string(),
                        root,
                        stage: "resolve-path".into(),
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
        let child_loader = config_loader.for_workspace_root(&root);
        let child_config = match load_workspace_config(child_loader.clone(), &root)
            .await
            .wrap_err_with(|| format!("Failed to load discovered workspace {alias} at {root:?}"))
        {
            Ok(config) => config,
            Err(error) if retain_failures => {
                failures.push(DiscoveredWorkspaceFailure {
                    alias,
                    message: error.to_string(),
                    root,
                    stage: "workspace-config".into(),
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let Some(configured_id) = &child_config.id else {
            let error: miette::Report = AppError::DiscoveredWorkspaceIdRequired {
                alias: alias.clone(),
                root: root.clone(),
            }
            .into();

            if retain_failures {
                failures.push(DiscoveredWorkspaceFailure {
                    alias,
                    message: error.to_string(),
                    root,
                    stage: "workspace-identity".into(),
                });
                continue;
            }

            return Err(error);
        };
        let id = SourceRootId::new(configured_id.as_str())?;

        if let Some(expected_id) = &declaration.id {
            let expected = SourceRootId::new(expected_id.as_str())?;

            if expected != id {
                let error: miette::Report = AppError::DiscoveredWorkspaceIdMismatch {
                    actual: id.clone(),
                    alias: alias.clone(),
                    expected,
                    root: root.clone(),
                }
                .into();

                if retain_failures {
                    failures.push(DiscoveredWorkspaceFailure {
                        alias,
                        message: error.to_string(),
                        root,
                        stage: "workspace-identity".into(),
                    });
                    continue;
                }

                return Err(error);
            }
        }

        if let Some(existing_root) = identity_roots.get(&id) {
            if existing_root == &physical_root {
                aliases.insert(alias, id);
                continue;
            }

            let error: miette::Report = AppError::DiscoveredWorkspaceIdConflict {
                alias: alias.clone(),
                existing_root: sources.get(&id)?.to_path_buf(),
                id: id.clone(),
                root: root.clone(),
            }
            .into();

            if retain_failures {
                failures.push(DiscoveredWorkspaceFailure {
                    alias,
                    message: error.to_string(),
                    root,
                    stage: "workspace-identity".into(),
                });
                continue;
            }

            return Err(error);
        }

        if let Some(existing_id) = physical_roots.get(&physical_root) {
            let error: miette::Report = AppError::DiscoveredWorkspaceRootConflict {
                alias: alias.clone(),
                existing_id: existing_id.clone(),
                id: id.clone(),
                root: root.clone(),
            }
            .into();

            if retain_failures {
                failures.push(DiscoveredWorkspaceFailure {
                    alias,
                    message: error.to_string(),
                    root,
                    stage: "workspace-identity".into(),
                });
                continue;
            }

            return Err(error);
        }

        if let Err(error) = sources.register(id.clone(), root.clone()) {
            if retain_failures {
                failures.push(DiscoveredWorkspaceFailure {
                    alias,
                    message: error.to_string(),
                    root,
                    stage: "workspace-identity".into(),
                });
                continue;
            }

            return Err(error);
        }
        aliases.insert(alias, id.clone());
        identity_roots.insert(id.clone(), physical_root.clone());
        physical_roots.insert(physical_root, id.clone());
        workspaces.insert(
            id.clone(),
            DiscoveredWorkspace {
                config_dir: child_loader.dir,
                id,
                root,
                workspace_config: child_config,
            },
        );
    }

    Ok(WorkspaceDiscovery {
        aliases,
        failures,
        sources: Arc::new(sources),
        workspaces,
    })
}

/// Load source-local configuration and service state for each discovered child workspace.
pub async fn load_source_contexts(
    config_loader: &ConfigLoader,
    discovery: &WorkspaceDiscovery,
    retain_failures: bool,
) -> miette::Result<FxHashMap<SourceRootId, SourceContext>> {
    let mut sources = discovery.workspaces.values().collect::<Vec<_>>();
    sources.sort_by_key(|source| source.id.clone());

    let mut contexts = FxHashMap::default();

    for source in sources {
        if &source.id == discovery.sources.primary_id() {
            continue;
        }

        contexts.insert(
            source.id.clone(),
            load_source_context(config_loader, source, retain_failures).await?,
        );
    }

    Ok(contexts)
}

/// Reload source-local configuration and service state for one workspace.
pub async fn load_source_context(
    config_loader: &ConfigLoader,
    source: &DiscoveredWorkspace,
    retain_failures: bool,
) -> miette::Result<SourceContext> {
    let loader = config_loader.for_workspace_root(&source.root);
    let moon_env = detect_moon_environment(&source.root, &source.root)?;
    let proto_env = detect_proto_environment(&source.root, &source.root)?;
    let (tasks_result, extensions_result, toolchains_result) = tokio::join!(
        load_tasks_configs(loader.clone(), &source.root),
        load_extensions_config(loader.clone(), &source.root),
        load_toolchains_config(
            loader.clone(),
            Arc::clone(&proto_env),
            &source.root,
            &source.root,
        ),
    );
    let mut failures = Vec::new();
    let tasks_config = match tasks_result {
        Ok(config) => config,
        Err(error) if retain_failures => {
            failures.push(SourceLoadFailure {
                message: error.to_string(),
                stage: "tasks-config".into(),
            });
            Arc::new(InheritedTasksManager::default())
        }
        Err(error) => return Err(error),
    };
    let extensions_config = match extensions_result {
        Ok(config) => config,
        Err(error) if retain_failures => {
            failures.push(SourceLoadFailure {
                message: error.to_string(),
                stage: "extensions-config".into(),
            });
            Arc::new(ExtensionsConfig::default())
        }
        Err(error) => return Err(error),
    };
    let toolchains_config = match toolchains_result {
        Ok(config) => config,
        Err(error) if retain_failures => {
            failures.push(SourceLoadFailure {
                message: error.to_string(),
                stage: "toolchains-config".into(),
            });
            Arc::new(ToolchainsConfig::default())
        }
        Err(error) => return Err(error),
    };

    Ok(SourceContext::new(
        source.id.clone(),
        source.root.clone(),
        source.root.clone(),
        loader,
        moon_env,
        proto_env,
        Arc::clone(&source.workspace_config),
        tasks_config,
        extensions_config,
        toolchains_config,
        failures,
    ))
}

/// Load the toolchain configuration file from the `.moon` directory if it exists.
#[instrument(skip(config_loader, proto_env))]
pub async fn load_toolchains_config(
    config_loader: ConfigLoader,
    proto_env: Arc<ProtoEnvironment>,
    workspace_root: &Path,
    working_dir: &Path,
) -> miette::Result<Arc<ToolchainsConfig>> {
    debug!(
        "Attempting to load {} (optional)",
        color::file(config_loader.get_debug_label_root("toolchains")),
    );

    let root = workspace_root.to_owned();
    let cwd = working_dir.to_owned();
    let config = load_config_blocking(move || {
        config_loader
            .load_toolchains_config(root, proto_env.load_file_manager()?.get_local_config(&cwd)?)
    })
    .await
    .into_diagnostic()??;

    Ok(Arc::new(config))
}

/// Load the extensions configuration file from the `.moon` directory if it exists.
#[instrument(skip(config_loader))]
pub async fn load_extensions_config(
    config_loader: ConfigLoader,
    workspace_root: &Path,
) -> miette::Result<Arc<ExtensionsConfig>> {
    debug!(
        "Attempting to load {} (optional)",
        color::file(config_loader.get_debug_label_root("extensions")),
    );

    let root = workspace_root.to_owned();
    let config = load_config_blocking(move || config_loader.load_extensions_config(root))
        .await
        .into_diagnostic()??;

    Ok(Arc::new(config))
}

/// Load the tasks configuration file from the `.moon` directory if it exists.
/// Also load all scoped tasks from the `.moon/tasks` directory and load into the manager.
#[instrument(skip(config_loader))]
pub async fn load_tasks_configs(
    config_loader: ConfigLoader,
    workspace_root: &Path,
) -> miette::Result<Arc<InheritedTasksManager>> {
    debug!(
        "Attempting to load {} (optional)",
        color::file(config_loader.get_debug_label_root("tasks/**/*")),
    );

    let root = workspace_root.to_owned();
    let manager = load_config_blocking(move || config_loader.load_tasks_manager(root))
        .await
        .into_diagnostic()??;

    debug!(
        "Loaded {} task configs for inheritance",
        manager.configs.len()
    );

    Ok(Arc::new(manager))
}

#[instrument(skip_all)]
pub fn register_feature_flags(_config: &WorkspaceConfig) -> miette::Result<()> {
    FeatureFlags::default().register();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceVcsState;
    use moon_app_context::{SourceRuntime, SourceRuntimeRegistry};
    use moon_common::{Id, path::WorkspaceRelativePathBuf};
    use moon_console::Console;
    use starbase_sandbox::create_empty_sandbox;
    use version_spec::Version;

    async fn discover(
        primary_config: &str,
        child_config: Option<&str>,
    ) -> miette::Result<WorkspaceDiscovery> {
        let sandbox = create_empty_sandbox();
        let primary_root = sandbox.path().join("platform");

        sandbox.create_file("platform/.moon/workspace.yml", primary_config);

        if let Some(config) = child_config {
            sandbox.create_file("web/.moon/workspace.yml", config);
        }

        let mut loader = ConfigLoader::default();
        loader.locate_dir(&primary_root);
        let config = load_workspace_config(loader.clone(), &primary_root).await?;
        discover_workspaces(&loader, &primary_root, config).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discovers_direct_workspaces_by_their_self_declared_id() {
        let discovery = discover(
            r"
id: acme/platform
workspaces:
  frontend:
    path: ../web
    id: acme/web
",
            Some(
                r"
id: acme/web
workspaces:
  ignored:
    path: ../ignored
",
            ),
        )
        .await
        .unwrap();

        let platform = SourceRootId::new("acme/platform").unwrap();
        let web = SourceRootId::new("acme/web").unwrap();

        assert_eq!(discovery.sources.primary_id(), &platform);
        assert_eq!(discovery.sources.len(), 2);
        assert_eq!(
            discovery
                .aliases
                .get(&SourceAlias::new("frontend").unwrap()),
            Some(&web)
        );
        assert_eq!(discovery.workspaces.len(), 2);
        assert_eq!(
            discovery.workspaces[&web].workspace_config.id,
            Some(Id::raw("acme/web"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uses_the_compatibility_id_for_an_unidentified_primary_workspace() {
        let discovery = discover("{}", None).await.unwrap();

        assert_eq!(discovery.sources.primary_id(), &SourceRootId::primary());
        assert_eq!(discovery.sources.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requires_discovered_workspaces_to_declare_an_id() {
        let error = discover(
            r"
workspaces:
  frontend:
    path: ../web
",
            Some("{}"),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("must declare a canonical `id`"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verifies_the_expected_workspace_id() {
        let error = discover(
            r"
workspaces:
  frontend:
    path: ../web
    id: acme/expected
",
            Some("id: acme/actual"),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("acme/actual"));
        assert!(error.to_string().contains("acme/expected"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deduplicates_aliases_for_the_same_canonical_workspace() {
        let discovery = discover(
            r"
workspaces:
  frontend:
    path: ../web
  ui:
    path: ../web
",
            Some("id: acme/web"),
        )
        .await
        .unwrap();

        assert_eq!(discovery.sources.len(), 2);
        assert_eq!(discovery.aliases.len(), 2);
        assert_eq!(
            discovery.aliases[&SourceAlias::new("frontend").unwrap()],
            discovery.aliases[&SourceAlias::new("ui").unwrap()]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_the_same_canonical_id_at_different_roots() {
        let sandbox = create_empty_sandbox();
        let primary_root = sandbox.path().join("platform");

        sandbox.create_file(
            "platform/.moon/workspace.yml",
            r"
workspaces:
  frontend:
    path: ../web
  frontend-copy:
    path: ../web-copy
",
        );
        sandbox.create_file("web/.moon/workspace.yml", "id: acme/web");
        sandbox.create_file("web-copy/.moon/workspace.yml", "id: acme/web");

        let mut loader = ConfigLoader::default();
        loader.locate_dir(&primary_root);
        let config = load_workspace_config(loader.clone(), &primary_root)
            .await
            .unwrap();
        let error = discover_workspaces(&loader, &primary_root, config)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("already registered"));
        assert!(error.to_string().contains("frontend-copy"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn deduplicates_real_and_symlinked_workspace_roots() {
        use std::os::unix::fs::symlink;

        let sandbox = create_empty_sandbox();
        let primary_root = sandbox.path().join("platform");

        sandbox.create_file(
            "platform/.moon/workspace.yml",
            r"
workspaces:
  frontend:
    path: ../web
  ui:
    path: ../web-link
",
        );
        sandbox.create_file("web/.moon/workspace.yml", "id: acme/web");
        symlink(sandbox.path().join("web"), sandbox.path().join("web-link")).unwrap();

        let mut loader = ConfigLoader::default();
        loader.locate_dir(&primary_root);
        let config = load_workspace_config(loader.clone(), &primary_root)
            .await
            .unwrap();
        let discovery = discover_workspaces(&loader, &primary_root, config)
            .await
            .unwrap();

        assert_eq!(discovery.sources.len(), 2);
        assert_eq!(discovery.aliases.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loads_independent_source_contexts_and_reuses_their_vcs_observation() {
        let sandbox = create_empty_sandbox();
        let primary_root = sandbox.path().join("platform");

        sandbox.create_file(
            "platform/.moon/workspace.yml",
            r"
id: acme/platform
workspaces:
  frontend:
    path: ../web
",
        );
        sandbox.create_file("web/.moon/workspace.yml", "id: acme/web");
        sandbox.create_file("web/source-only.txt", "from child");
        sandbox.create_file(
            "web/.moon/tasks/child.yml",
            r"
tasks:
  child:
    command: noop
",
        );
        sandbox.create_file(
            "web/.moon/extensions.yml",
            r"
child-extension:
  plugin: https://example.com/plugin.wasm
",
        );
        sandbox.create_file("web/.moon/toolchains.yml", "node: {}");

        let mut loader = ConfigLoader::default();
        loader.locate_dir(&primary_root);
        let config = load_workspace_config(loader.clone(), &primary_root)
            .await
            .unwrap();
        let discovery = discover_workspaces(&loader, &primary_root, config)
            .await
            .unwrap();
        let contexts = load_source_contexts(&loader, &discovery, false)
            .await
            .unwrap();
        let web = &contexts[&SourceRootId::new("acme/web").unwrap()];

        assert_eq!(web.root, sandbox.path().join("web"));
        assert_eq!(web.working_dir, web.root);
        assert_eq!(web.moon_env.workspace_root, web.root);
        assert_eq!(web.proto_env.working_dir, web.root);
        assert_eq!(web.tasks_config.configs.len(), 1);
        assert!(
            web.extensions_config
                .plugins
                .contains_key("child-extension")
        );
        assert!(web.toolchains_config.plugins.contains_key("node"));
        assert!(web.failures.is_empty());

        let first_cache = web.get_cache_engine().await.unwrap();
        let second_cache = web.get_cache_engine().await.unwrap();
        assert!(Arc::ptr_eq(&first_cache, &second_cache));

        let first_extensions = web.get_extension_registry().await.unwrap();
        let second_extensions = web.get_extension_registry().await.unwrap();
        assert!(Arc::ptr_eq(&first_extensions, &second_extensions));

        let first_toolchains = web.get_toolchain_registry().await.unwrap();
        let second_toolchains = web.get_toolchain_registry().await.unwrap();
        assert!(Arc::ptr_eq(&first_toolchains, &second_toolchains));

        match (web.initialize_vcs().await, web.initialize_vcs().await) {
            (SourceVcsState::Ready(first), SourceVcsState::Ready(second)) => {
                assert!(Arc::ptr_eq(&first, &second));
            }
            (SourceVcsState::Failed(first), SourceVcsState::Failed(second)) => {
                assert!(Arc::ptr_eq(&first, &second));
            }
            _ => panic!("Source VCS state must be cached."),
        }

        let console = Arc::new(Console::new(true));
        let first_app = web
            .get_app_context(Version::parse("1.0.0").unwrap(), Arc::clone(&console))
            .await
            .unwrap();
        let second_app = web
            .get_app_context(Version::parse("1.0.0").unwrap(), console)
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&first_app, &second_app));
        assert!(Arc::ptr_eq(&first_app.cache_engine, &first_cache));
        assert!(Arc::ptr_eq(
            &first_app.extension_registry,
            &first_extensions
        ));
        assert!(Arc::ptr_eq(
            &first_app.toolchain_registry,
            &first_toolchains
        ));
        match web.vcs_state().unwrap() {
            SourceVcsState::Ready(vcs) => assert!(Arc::ptr_eq(&first_app.vcs, &vcs)),
            SourceVcsState::Failed(_) => panic!("Expected child VCS to be available."),
        }

        let mut primary = (*first_app).clone();
        primary.source_id = discovery.sources.primary_id().clone();
        primary.workspace_root = primary_root;
        let registry = SourceRuntimeRegistry::new(
            Arc::new(primary),
            [(
                first_app.source_id.clone(),
                SourceRuntime::Available(Arc::clone(&first_app)),
            )],
        )
        .unwrap();

        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry.get_primary().source_id,
            SourceRootId::new("acme/platform").unwrap()
        );
        assert!(Arc::ptr_eq(
            registry
                .get(&SourceRootId::new("acme/web").unwrap())
                .unwrap(),
            &first_app
        ));

        let file = WorkspaceRelativePathBuf::from("source-only.txt");
        let hashes = registry
            .hash_files_for_source(
                &SourceRootId::new("acme/web").unwrap(),
                std::slice::from_ref(&file),
            )
            .await
            .unwrap();
        let direct_hashes = first_cache.hash_files(&web.root, &[file]).await.unwrap();
        assert_eq!(hashes, direct_hashes);
        assert_eq!(hashes.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_one_source_context_preserves_unchanged_runtime_identity() {
        let sandbox = create_empty_sandbox();
        let primary_root = sandbox.path().join("platform");

        sandbox.create_file(
            "platform/.moon/workspace.yml",
            r"
id: acme/platform
workspaces:
  frontend:
    path: ../web
  docs:
    path: ../docs
",
        );
        sandbox.create_file("web/.moon/workspace.yml", "id: acme/web");
        sandbox.create_file("docs/.moon/workspace.yml", "id: acme/docs");

        let mut loader = ConfigLoader::default();
        loader.locate_dir(&primary_root);
        let config = load_workspace_config(loader.clone(), &primary_root)
            .await
            .unwrap();
        let discovery = discover_workspaces(&loader, &primary_root, config)
            .await
            .unwrap();
        let mut contexts = load_source_contexts(&loader, &discovery, false)
            .await
            .unwrap();
        let web_id = SourceRootId::new("acme/web").unwrap();
        let docs_id = SourceRootId::new("acme/docs").unwrap();
        let console = Arc::new(Console::new(true));
        let version = Version::parse("1.0.0").unwrap();
        let web_runtime = contexts[&web_id]
            .get_app_context(version.clone(), Arc::clone(&console))
            .await
            .unwrap();
        let docs_runtime = contexts[&docs_id]
            .get_app_context(version.clone(), Arc::clone(&console))
            .await
            .unwrap();

        let replacement = load_source_context(&loader, &discovery.workspaces[&web_id], false)
            .await
            .unwrap();
        contexts.insert(web_id.clone(), replacement);

        let replaced_web_runtime = contexts[&web_id]
            .get_app_context(version.clone(), Arc::clone(&console))
            .await
            .unwrap();
        let unchanged_docs_runtime = contexts[&docs_id]
            .get_app_context(version, console)
            .await
            .unwrap();

        assert!(!Arc::ptr_eq(&web_runtime, &replaced_web_runtime));
        assert!(Arc::ptr_eq(&docs_runtime, &unchanged_docs_runtime));
    }
}
