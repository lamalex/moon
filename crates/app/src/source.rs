use moon_cache::{CacheContext, CacheEngine};
use moon_cache_local::LocalStorage;
use moon_cache_remote::{GrpcRemoteStorage, HttpRemoteStorage};
use moon_common::{SourceAlias, SourceRegistry, SourceRootId};
use moon_config::{
    ExtensionsConfig, InheritedTasksManager, RemoteApi, ToolchainsConfig, WorkspaceConfig,
};
use moon_config_loader::ConfigLoader;
use moon_env::MoonEnvironment;
use moon_env_var::GlobalEnvBag;
use moon_extension_plugin::ExtensionRegistry;
use moon_plugin::MoonHostData;
use moon_toolchain_plugin::ToolchainRegistry;
use moon_vcs::BoxedVcs;
use moon_vcs_plugin::load_vcs_adapter;
use moon_workspace::{WorkspaceBuilder, WorkspaceBuilderAsync, WorkspaceBuilderContext};
use moon_workspace_graph::WorkspaceGraph;
use proto_core::ProtoEnvironment;
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use tokio::sync::OnceCell;
use tracing::debug;

#[derive(Clone, Debug)]
pub struct DiscoveredWorkspace {
    pub config_dir: PathBuf,
    pub id: SourceRootId,
    pub root: PathBuf,
    pub workspace_config: Arc<WorkspaceConfig>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceDiscovery {
    pub aliases: FxHashMap<SourceAlias, SourceRootId>,
    pub failures: Vec<DiscoveredWorkspaceFailure>,
    pub sources: Arc<SourceRegistry>,
    pub workspaces: FxHashMap<SourceRootId, DiscoveredWorkspace>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredWorkspaceFailure {
    pub alias: SourceAlias,
    pub message: String,
    pub root: PathBuf,
    pub stage: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceLoadFailure {
    pub message: String,
    pub stage: String,
}

#[derive(Clone)]
pub enum SourceVcsState {
    Failed(Arc<String>),
    Ready(Arc<BoxedVcs>),
}

#[derive(Clone)]
pub struct SourceContext {
    pub config_loader: ConfigLoader,
    pub extensions_config: Arc<ExtensionsConfig>,
    pub failures: Arc<Vec<SourceLoadFailure>>,
    pub id: SourceRootId,
    pub moon_env: Arc<MoonEnvironment>,
    pub proto_env: Arc<ProtoEnvironment>,
    pub root: PathBuf,
    pub sources: Arc<SourceRegistry>,
    pub tasks_config: Arc<InheritedTasksManager>,
    pub toolchains_config: Arc<ToolchainsConfig>,
    pub working_dir: PathBuf,
    pub workspace_config: Arc<WorkspaceConfig>,

    cache_engine: Arc<OnceCell<Arc<CacheEngine>>>,
    extension_registry: Arc<OnceCell<Arc<ExtensionRegistry>>>,
    toolchain_registry: Arc<OnceCell<Arc<ToolchainRegistry>>>,
    vcs_state: Arc<OnceCell<SourceVcsState>>,
    workspace_graph: Arc<OnceCell<Arc<WorkspaceGraph>>>,
}

impl SourceContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: SourceRootId,
        root: PathBuf,
        working_dir: PathBuf,
        config_loader: ConfigLoader,
        moon_env: Arc<MoonEnvironment>,
        proto_env: Arc<ProtoEnvironment>,
        workspace_config: Arc<WorkspaceConfig>,
        tasks_config: Arc<InheritedTasksManager>,
        extensions_config: Arc<ExtensionsConfig>,
        toolchains_config: Arc<ToolchainsConfig>,
        failures: Vec<SourceLoadFailure>,
    ) -> Self {
        Self {
            cache_engine: Arc::new(OnceCell::new()),
            config_loader,
            extension_registry: Arc::new(OnceCell::new()),
            extensions_config,
            failures: Arc::new(failures),
            id: id.clone(),
            moon_env,
            proto_env,
            root: root.clone(),
            sources: Arc::new(SourceRegistry::new(id, root)),
            tasks_config,
            toolchain_registry: Arc::new(OnceCell::new()),
            toolchains_config,
            vcs_state: Arc::new(OnceCell::new()),
            working_dir,
            workspace_config,
            workspace_graph: Arc::new(OnceCell::new()),
        }
    }

    pub async fn get_cache_engine(&self) -> miette::Result<Arc<CacheEngine>> {
        self.cache_engine
            .get_or_try_init(async || {
                let mut context = CacheContext {
                    cache_dir: self.config_loader.dir.join("cache"),
                    cache_shared_dir: None,
                    cache_config: Arc::new(self.workspace_config.cache.clone()),
                    config_dir: self.config_loader.dir.clone(),
                    remote_config: Arc::new(self.workspace_config.remote.clone()),
                    remote_debug: GlobalEnvBag::instance().should_debug_remote(),
                    workspace_root: self.root.clone(),
                };

                if context.cache_config.shared_worktree_cache
                    && let SourceVcsState::Ready(vcs) = self.initialize_vcs().await
                    && vcs.is_worktree()
                {
                    let repo_root = vcs.get_repository_root()?;
                    let worktree_root = vcs.get_working_root()?;
                    let common_moon_dir = repo_root.join(&self.config_loader.dir_prefix);

                    context.cache_shared_dir =
                        Some(if common_moon_dir.exists() && repo_root != worktree_root {
                            common_moon_dir.join("cache")
                        } else {
                            self.moon_env.cache_dir.join("shared")
                        });
                }

                let mut engine = CacheEngine::new(context.clone())?;

                if self.workspace_config.experiments.cas_outputs_cache {
                    engine.storage.add_local_backend(LocalStorage::new(
                        context.clone(),
                        context
                            .cache_shared_dir
                            .as_deref()
                            .unwrap_or(&context.cache_dir),
                    )?);
                }

                if context.remote_config.is_enabled() {
                    match context.remote_config.api {
                        RemoteApi::Grpc => engine
                            .storage
                            .add_remote_backend(GrpcRemoteStorage::new(context.clone())?),
                        RemoteApi::Http => engine
                            .storage
                            .add_remote_backend(HttpRemoteStorage::new(context.clone())?),
                    };
                }

                Ok(Arc::new(engine))
            })
            .await
            .map(Arc::clone)
    }

    pub async fn wait_for_cache_tasks(&self) -> miette::Result<()> {
        if let Some(engine) = self.cache_engine.get() {
            engine.storage.wait_for_background_tasks().await?;
        }

        Ok(())
    }

    pub async fn get_extension_registry(&self) -> miette::Result<Arc<ExtensionRegistry>> {
        self.extension_registry
            .get_or_try_init(async || {
                Ok(Arc::new(ExtensionRegistry::new(
                    self.create_host_data(),
                    Arc::clone(&self.extensions_config),
                )?))
            })
            .await
            .map(Arc::clone)
    }

    pub async fn get_toolchain_registry(&self) -> miette::Result<Arc<ToolchainRegistry>> {
        self.toolchain_registry
            .get_or_try_init(async || {
                Ok(Arc::new(ToolchainRegistry::new(
                    self.create_host_data(),
                    Arc::clone(&self.toolchains_config),
                )?))
            })
            .await
            .map(Arc::clone)
    }

    pub async fn get_workspace_graph(&self) -> miette::Result<Arc<WorkspaceGraph>> {
        self.workspace_graph
            .get_or_try_init(async || {
                let context = WorkspaceBuilderContext {
                    cache_engine: self.get_cache_engine().await?,
                    config_loader: self.config_loader.clone(),
                    enabled_toolchains: self.toolchains_config.get_enabled(),
                    extensions_config: Arc::clone(&self.extensions_config),
                    extension_registry: self.get_extension_registry().await?,
                    inherited_tasks: Arc::clone(&self.tasks_config),
                    sources: Arc::clone(&self.sources),
                    toolchains_config: Arc::clone(&self.toolchains_config),
                    toolchain_registry: self.get_toolchain_registry().await?,
                    vcs: match self.initialize_vcs().await {
                        SourceVcsState::Ready(vcs) => Some(vcs),
                        SourceVcsState::Failed(_) => None,
                    },
                    working_dir: self.working_dir.clone(),
                    workspace_config: Arc::clone(&self.workspace_config),
                    workspace_root: self.root.clone(),
                };
                let graph = Arc::new(if self.workspace_config.experiments.async_graph_building {
                    WorkspaceBuilderAsync::new_with_cache(context)
                        .await?
                        .build()
                        .await?
                } else {
                    WorkspaceBuilder::new_with_cache(context)
                        .await?
                        .build()
                        .await?
                });

                let extensions = self.get_extension_registry().await?;
                let _ = extensions.host_data.workspace_graph.set(Arc::clone(&graph));
                let toolchains = self.get_toolchain_registry().await?;
                let _ = toolchains.host_data.workspace_graph.set(Arc::clone(&graph));

                Ok(graph)
            })
            .await
            .map(Arc::clone)
    }

    pub fn initialize_vcs(&self) -> Pin<Box<dyn Future<Output = SourceVcsState> + Send + '_>> {
        Box::pin(async {
            self.vcs_state
                .get_or_init(async || {
                    let config = &self.workspace_config.vcs;

                    match load_vcs_adapter(
                        self.create_host_data(),
                        &self.working_dir,
                        &self.root,
                        &config.default_branch,
                        &config.remote_candidates,
                    )
                    .await
                    {
                        Ok(adapter) => SourceVcsState::Ready(Arc::new(adapter)),
                        Err(error) => {
                            debug!(source = %self.id, error = ?error, "Failed to initialize source VCS provider");
                            SourceVcsState::Failed(Arc::new(error.to_string()))
                        }
                    }
                })
                .await
                .clone()
        })
    }

    pub fn vcs_state(&self) -> Option<SourceVcsState> {
        self.vcs_state.get().cloned()
    }

    fn create_host_data(&self) -> MoonHostData {
        MoonHostData {
            moon_env: Arc::clone(&self.moon_env),
            proto_env: Arc::clone(&self.proto_env),
            extensions_config: Arc::clone(&self.extensions_config),
            toolchains_config: Arc::clone(&self.toolchains_config),
            workspace_config: Arc::clone(&self.workspace_config),
            workspace_graph: Arc::new(OnceLock::new()),
        }
    }
}
