use crate::event_emitter::{Event, Subscriber};
use async_trait::async_trait;
use moon_action::ActionPipelineStatus;
use moon_app_context::{SourceRuntime, SourceRuntimeRegistry};
use moon_daemon_client::DaemonClient;
use std::sync::Arc;
use tracing::debug;

pub struct CleanupSubscriber {
    source_runtime_registry: Arc<SourceRuntimeRegistry>,
    daemon_client: Option<DaemonClient>,
    lifetime: String,
}

impl CleanupSubscriber {
    pub fn new(
        source_runtime_registry: Arc<SourceRuntimeRegistry>,
        daemon_client: Option<DaemonClient>,
        lifetime: &str,
    ) -> Self {
        CleanupSubscriber {
            source_runtime_registry,
            daemon_client,
            lifetime: lifetime.to_owned(),
        }
    }

    async fn clean_caches(&self) -> miette::Result<()> {
        for (source_id, runtime) in self.source_runtime_registry.iter() {
            let SourceRuntime::Available(context) = runtime else {
                continue;
            };

            if let Some(mut daemon) = self.daemon_client.clone() {
                // Automatic cleanup matches direct mode and preserves states,
                // temporary files, and storage artifacts.
                daemon
                    .clean_cache(source_id, self.lifetime.clone(), false)
                    .await?;
            } else {
                context
                    .cache_engine
                    .clean_stale_cache(&self.lifetime, false)
                    .await?;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Subscriber for CleanupSubscriber {
    async fn on_emit<'data>(&mut self, event: &Event<'data>) -> miette::Result<()> {
        if matches!(
            event,
            Event::PipelineCompleted {
                status: ActionPipelineStatus::Completed,
                ..
            }
        ) {
            debug!("Cleaning stale cache");

            self.clean_caches().await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::SourceRootId;
    use moon_test_utils::WorkspaceMocker;
    use starbase_sandbox::create_empty_sandbox;

    #[tokio::test]
    async fn cleans_every_available_source_cache() {
        let sandbox = create_empty_sandbox();
        let primary = Arc::new(WorkspaceMocker::new(sandbox.path()).mock_app_context());
        let child_root = sandbox.path().join("child");
        std::fs::create_dir_all(&child_root).unwrap();
        let mut child = WorkspaceMocker::new(&child_root).mock_app_context();
        child.source_id = SourceRootId::new("child").unwrap();
        let child = Arc::new(child);
        let registry = Arc::new(
            SourceRuntimeRegistry::new(
                Arc::clone(&primary),
                [
                    (
                        child.source_id.clone(),
                        SourceRuntime::Available(Arc::clone(&child)),
                    ),
                    (
                        SourceRootId::new("unavailable").unwrap(),
                        SourceRuntime::Unavailable("failed to initialize".into()),
                    ),
                ],
            )
            .unwrap(),
        );
        let primary_file = primary.cache_engine.hash.hashes_dir.join("stale.json");
        let child_file = child.cache_engine.hash.hashes_dir.join("stale.json");
        std::fs::write(&primary_file, "primary").unwrap();
        std::fs::write(&child_file, "child").unwrap();

        CleanupSubscriber::new(registry, None, "0 seconds")
            .clean_caches()
            .await
            .unwrap();

        assert!(!primary_file.exists());
        assert!(!child_file.exists());
    }
}
