use crate::session::MoonSession;
use moon_daemon::AtomicDaemonState;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

    pub fn rebuild_context(
        &self,
        state: AtomicDaemonState,
        generation: Arc<AtomicU64>,
        expected_generation: u64,
    ) -> JoinHandle<()> {
        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(registry) = session.get_source_runtime_registry().await {
                if generation.load(Ordering::Acquire) != expected_generation {
                    return;
                }

                let mut state = state.write().await;

                if generation.load(Ordering::Acquire) != expected_generation {
                    return;
                }

                state.app_context = Arc::clone(registry.get_primary());
                state.source_runtime_registry = registry;
            }
        })
    }

    pub fn rebuild_graphs(
        &self,
        state: AtomicDaemonState,
        generation: Arc<AtomicU64>,
        expected_generation: u64,
    ) -> JoinHandle<()> {
        debug!("Rebuilding project and task graphs");

        let session = self.clone();

        tokio::spawn(async move {
            if let Ok(graph) = session.get_aggregate_workspace_graph().await
                && let Ok(registry) = session.get_source_runtime_registry().await
            {
                if generation.load(Ordering::Acquire) != expected_generation {
                    return;
                }

                let mut state = state.write().await;

                if generation.load(Ordering::Acquire) != expected_generation {
                    return;
                }

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

    pub(crate) fn reset_source_composition(&mut self) {
        self.aggregate_workspace_graph.take();
        self.source_runtime_registry.take();
    }

    pub fn reset_vcs(&mut self) {
        debug!("Resetting VCS adapter");

        self.reset_runtime_contexts();
        self.vcs_adapter.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn stale_rebuild_generation_cannot_publish() {
        let generation = Arc::new(AtomicU64::new(7));
        let published = Arc::new(AtomicBool::new(false));
        let stale = generation.load(Ordering::Acquire);
        let task_generation = Arc::clone(&generation);
        let task_published = Arc::clone(&published);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;

            if task_generation.load(Ordering::Acquire) == stale {
                task_published.store(true, Ordering::Release);
            }
        });

        generation.fetch_add(1, Ordering::AcqRel);
        handle.await.unwrap();

        assert!(!published.load(Ordering::Acquire));
    }
}
