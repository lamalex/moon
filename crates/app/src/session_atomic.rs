use crate::session::MoonSession;
use moon_daemon::AtomicDaemonState;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

impl MoonSession {
    pub fn download_extensions(&self) {
        debug!("Downloading extensions");

        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(registry) = session.get_extension_registry().await {
                let _ = registry.load_all().await;
            }
        });
    }

    pub fn download_toolchains(&self) {
        debug!("Downloading toolchains");

        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(registry) = session.get_toolchain_registry().await {
                let _ = registry.load_all().await;
            }
        });
    }

    pub fn rebuild_context(&self, state: AtomicDaemonState) -> JoinHandle<()> {
        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(registry) = session.get_source_runtime_registry().await {
                let mut state = state.write().await;
                state.app_context = Arc::clone(registry.get_primary());
                state.source_runtime_registry = registry;
            }
        })
    }

    pub fn rebuild_graphs(&self, state: AtomicDaemonState) -> JoinHandle<()> {
        debug!("Rebuilding project and task graphs");

        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(graph) = session.get_workspace_graph().await
                && let Ok(registry) = session.get_source_runtime_registry().await
            {
                let mut state = state.write().await;
                state.app_context = Arc::clone(registry.get_primary());
                state.source_runtime_registry = registry;
                state.workspace_graph = graph;
            }
        })
    }

    pub fn reset_components(&mut self) {
        debug!("Resetting registries and graphs cache");

        self.aggregate_workspace_graph.take();
        self.reset_runtime_contexts();
        self.extension_registry.take();
        self.toolchain_registry.take();
        self.project_graph.take();
        self.task_graph.take();
        self.workspace_graph.take();
    }

    pub(crate) fn reset_runtime_contexts(&mut self) {
        self.app_context.take();
        self.source_runtime_registry.take();
    }

    pub fn reset_vcs(&mut self) {
        debug!("Resetting VCS adapter");

        self.reset_runtime_contexts();
        self.vcs_adapter.take();
    }
}
