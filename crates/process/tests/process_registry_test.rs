#![cfg(unix)]

use moon_process::{ProcessRegistry, SignalType};
use std::sync::Arc;
use tokio::process::{Child, Command};

fn spawn_sleep() -> Child {
    Command::new("sleep").arg("30").spawn().unwrap()
}

mod process_registry {
    use super::*;

    #[tokio::test]
    async fn instance_is_a_singleton() {
        assert!(Arc::ptr_eq(
            &ProcessRegistry::instance(),
            &ProcessRegistry::instance()
        ));
    }

    #[tokio::test]
    async fn registers_and_unregisters_children() {
        let registry = ProcessRegistry::instance();
        let shared = registry.add_running(spawn_sleep()).await;
        let pid = shared.id();

        assert!(registry.get_running_by_pid(pid).await.is_some());

        registry.remove_running(shared.clone()).await;

        assert!(registry.get_running_by_pid(pid).await.is_none());

        let _ = shared.kill().await;
    }

    #[tokio::test]
    async fn terminates_running_children() {
        let registry = ProcessRegistry::instance();
        let shared = registry.add_running(spawn_sleep()).await;
        let pid = shared.id();

        registry.shutdown_running(SignalType::Terminate).await;
        registry.wait_for_running_to_shutdown().await;

        assert!(registry.get_running_by_pid(pid).await.is_none());
    }

    #[tokio::test]
    async fn internal_shutdown_does_not_broadcast_an_os_signal() {
        let registry = ProcessRegistry::new(2000);
        let mut signals = registry.receive_signal();
        let shared = registry.add_running(spawn_sleep()).await;
        let pid = shared.id();

        registry.shutdown_running(SignalType::Terminate).await;

        assert!(registry.get_running_by_pid(pid).await.is_none());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), signals.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_wait_returns_immediately_when_empty() {
        ProcessRegistry::instance()
            .wait_for_running_to_shutdown()
            .await;
    }
}
