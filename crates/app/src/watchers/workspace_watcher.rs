use crate::session::MoonSession;
use crate::systems::startup;
use async_trait::async_trait;
use moon_common::SourceRootId;
use moon_common::path::WorkspaceRelativePath;
use moon_config::{WorkspaceConfig, WorkspaceProjects};
use moon_daemon::AtomicDaemonState;
use moon_file_watcher::*;
use moon_workspace::{STATE_CACHE_FILE_NAME, STATE_GRAPH_FILE_NAME};
use proto_core::ProtoEnvironment;
use regex::Regex;
use starbase_utils::fs;
use starbase_utils::glob::GlobSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::JoinHandle;
use tracing::debug;

pub struct WorkspaceWatcher {
    context_handle: Option<JoinHandle<()>>,
    graph_handle: Option<JoinHandle<()>>,
    rebuild_generation: Arc<AtomicU64>,
    session: MoonSession,

    project_config_regex: Regex,
    tasks_config_regex: Regex,
    workspace_config_regex: Regex,
}

impl WorkspaceWatcher {
    pub fn new(session: MoonSession) -> Self {
        let exts_group = format!("({})", session.config_loader.extensions.join("|"));

        Self {
            context_handle: None,
            graph_handle: None,
            rebuild_generation: Arc::new(AtomicU64::new(0)),
            session,
            project_config_regex: Regex::new(&format!(r"(^|/)moon\.{exts_group}$")).unwrap(),
            tasks_config_regex: Regex::new(&format!(r"^(\.moon|\.config/moon)/.*\.{exts_group}$"))
                .unwrap(),
            workspace_config_regex: Regex::new(&format!(
                r"^(\.moon|\.config/moon)/(?<name>\w+)\.{exts_group}$"
            ))
            .unwrap(),
        }
    }
}

#[async_trait]
impl FileWatcher<AtomicDaemonState> for WorkspaceWatcher {
    async fn on_init(&mut self, _state: AtomicDaemonState) -> miette::Result<()> {
        Ok(())
    }

    async fn on_file_event(
        &mut self,
        state: AtomicDaemonState,
        event: &FileEvent,
    ) -> miette::Result<()> {
        if Self::should_retire_for_event(event) {
            state.read().await.topology_changed.notify_one();

            return Ok(());
        }

        if !event.is_mutated() {
            return Ok(());
        }

        if &event.source_id != self.session.sources.primary_id() {
            return self.on_child_file_event(state, event).await;
        }

        // Handle `.prototools` changes
        if event.path.ends_with(".prototools") {
            self.reset_proto(&state).await?;

            return Ok(());
        }

        // Handle `.moon/*.config` changes
        if let Some(caps) = self.workspace_config_regex.captures(event.path.as_str()) {
            match caps.name("name").map(|cap| cap.as_str()) {
                Some("extensions") => self.reset_extensions(&state).await?,
                Some("toolchains") => self.reset_toolchains(&state).await?,
                Some("workspace") => self.reset_workspace(&state).await?,
                _ => {}
            };

            return Ok(());
        }

        // Handle `.moon/tasks/**/*.config` changes
        if self.tasks_config_regex.is_match(event.path.as_str()) {
            self.reset_tasks(&state).await?;

            return Ok(());
        }

        // Handle `moon.config` changes
        if self.project_config_regex.is_match(event.path.as_str()) {
            self.reset_projects(&state).await?;

            return Ok(());
        }

        // Handle the creation/removal of project directories
        if event.is_mutated_directory()
            && Self::is_a_project_root(&event.path, &self.session.workspace_config)?
        {
            self.reset_projects(&state).await?;

            return Ok(());
        }

        Ok(())
    }
}

impl WorkspaceWatcher {
    fn should_retire_for_event(event: &FileEvent) -> bool {
        event.is_source_root_removed_or_renamed()
    }

    fn is_a_project_root(
        path: &WorkspaceRelativePath,
        workspace_config: &WorkspaceConfig,
    ) -> miette::Result<bool> {
        let (sources, globs): (Vec<_>, Vec<_>) = match &workspace_config.projects {
            WorkspaceProjects::Sources(sources) => (sources.values().collect(), Vec::new()),
            WorkspaceProjects::Globs(globs) => (Vec::new(), globs.iter().collect()),
            WorkspaceProjects::Both(inner) => (
                inner.sources.values().collect(),
                inner.globs.iter().collect(),
            ),
        };

        for source in sources {
            if path == source {
                return Ok(true);
            }
        }

        if !globs.is_empty() {
            return Ok(GlobSet::new(globs)?.matches(path.as_str()));
        }

        Ok(false)
    }

    async fn rebuild_context(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        let generation = self.rebuild_generation.fetch_add(1, Ordering::AcqRel) + 1;

        // A context rebuild supersedes graph publication as well. Otherwise a
        // slow graph build can publish state created from the previous context.
        if let Some(handle) = self.graph_handle.take() {
            handle.abort();
        }

        if let Some(handle) = self.context_handle.take() {
            handle.abort();
        }

        self.context_handle = Some(self.session.rebuild_context(
            Arc::clone(state),
            Arc::clone(&self.rebuild_generation),
            generation,
        ));

        Ok(())
    }

    async fn rebuild_graphs(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        self.schedule_graph_rebuild(state, true).await
    }

    async fn recompose_graphs(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        self.schedule_graph_rebuild(state, false).await
    }

    async fn schedule_graph_rebuild(
        &mut self,
        state: &AtomicDaemonState,
        clear_primary_cache: bool,
    ) -> miette::Result<()> {
        let generation = self.rebuild_generation.fetch_add(1, Ordering::AcqRel) + 1;

        // Abort any existing graph or context building
        if let Some(handle) = self.graph_handle.take() {
            handle.abort();
        }

        if let Some(handle) = self.context_handle.take() {
            handle.abort();
        }

        if clear_primary_cache {
            let cache_engine = self.session.get_cache_engine().await?;

            fs::remove_file(cache_engine.state.resolve_path(STATE_GRAPH_FILE_NAME))?;
            fs::remove_file(cache_engine.state.resolve_path(STATE_CACHE_FILE_NAME))?;
        }

        // Rebuild the graphs in a background thread
        self.graph_handle = Some(self.session.rebuild_graphs(
            Arc::clone(state),
            Arc::clone(&self.rebuild_generation),
            generation,
        ));

        Ok(())
    }

    async fn reset_proto(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        debug!("Updating proto environment");

        let mut env = ProtoEnvironment::new()?;
        env.working_dir = self.session.working_dir.clone();

        self.session.proto_env = Arc::new(env);
        self.session.reset_components();
        self.rebuild_context(state).await?;

        Ok(())
    }

    async fn reset_extensions(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        debug!("Updating extensions config");

        let extensions_config = self
            .session
            .config_loader
            .load_extensions_config(&self.session.workspace_root)?;
        let invalidate = self
            .session
            .extensions_config
            .should_invalidate(&extensions_config);

        self.session.extensions_config = Arc::new(extensions_config);
        self.session.reset_runtime_contexts();

        // Invalidate the extensions registry if the extensions config changed
        if invalidate {
            self.session.reset_components();
            self.session.download_extensions();
        }

        self.rebuild_context(state).await?;

        Ok(())
    }

    async fn reset_projects(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        // Always invalidate the workspace graph if a project config changes
        self.session.reset_components();
        self.rebuild_graphs(state).await?;

        Ok(())
    }

    async fn reset_tasks(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        debug!("Updating inherited tasks config");

        let tasks_config = self
            .session
            .config_loader
            .load_tasks_manager(&self.session.workspace_root)?;
        let invalidate = self.session.tasks_config.should_invalidate(&tasks_config);

        self.session.tasks_config = Arc::new(tasks_config);

        // Invalidate the workspace graphs if the tasks config changed,
        // so that task inheritance is properly reflected
        if invalidate {
            self.session.reset_components();
            self.rebuild_graphs(state).await?;
        } else {
            self.rebuild_context(state).await?;
        }

        Ok(())
    }

    async fn reset_toolchains(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        debug!("Updating toolchains config");

        let toolchains_config = self.session.config_loader.load_toolchains_config(
            &self.session.workspace_root,
            self.session.proto_env.load_config()?,
        )?;
        let invalidate = self
            .session
            .toolchains_config
            .should_invalidate(&toolchains_config);

        self.session.toolchains_config = Arc::new(toolchains_config);
        self.session.reset_runtime_contexts();
        moon_env_var::GlobalEnvBag::instance().set(
            "PROTO_CLI_VERSION",
            self.session.toolchains_config.proto.version.to_string(),
        );

        // Invalidate the toolchain registry if the toolchains config changed
        if invalidate {
            self.session.reset_components();
            self.session.download_toolchains();
        }

        self.rebuild_context(state).await?;

        Ok(())
    }

    async fn reset_workspace(&mut self, state: &AtomicDaemonState) -> miette::Result<()> {
        debug!("Updating workspace config");

        let workspace_config = Arc::new(
            self.session
                .config_loader
                .load_workspace_config(&self.session.workspace_root)?,
        );
        let mut rebuild = false;
        let rediscover = workspace_config.id != self.session.workspace_config.id
            || workspace_config.workspaces != self.session.workspace_config.workspaces;
        if rediscover {
            state.read().await.topology_changed.notify_one();

            return Ok(());
        }
        // Invalidate the VCS adapter if the VCS config changed
        if self
            .session
            .workspace_config
            .vcs
            .should_invalidate(&workspace_config.vcs)
        {
            self.session.reset_vcs();
        }

        // Invalidate the workspace graphs if the project configs changed
        if workspace_config.projects != self.session.workspace_config.projects
            || workspace_config.default_project != self.session.workspace_config.default_project
        {
            self.session.reset_components();
            rebuild = true;
        }

        // If the daemon has been turned off, attempt to stop it via the client
        if !workspace_config.daemon
            && let Ok(Some(mut client)) = self.session.connect_to_daemon().await
        {
            let _ = client.stop().await;
        }

        self.session.workspace_config = workspace_config;
        self.session.reset_runtime_contexts();

        if let Some(primary) = Arc::make_mut(&mut self.session.source_workspaces)
            .get_mut(self.session.sources.primary_id())
        {
            primary.workspace_config = Arc::clone(&self.session.workspace_config);
        }

        // Must run after the new config has been set!
        if rebuild {
            self.rebuild_graphs(state).await?;
        } else {
            self.rebuild_context(state).await?;
        }

        Ok(())
    }

    async fn on_child_file_event(
        &mut self,
        state: AtomicDaemonState,
        event: &FileEvent,
    ) -> miette::Result<()> {
        let Some(source) = self.session.source_workspaces.get(&event.source_id) else {
            // The root set and session have diverged. Retire instead of silently
            // continuing with a source that can no longer be invalidated.
            state.read().await.topology_changed.notify_one();

            return Ok(());
        };
        let workspace_config = &source.workspace_config;
        let is_runtime_config = event.path.ends_with(".prototools")
            || self.workspace_config_regex.is_match(event.path.as_str())
            || self.tasks_config_regex.is_match(event.path.as_str());
        let is_project_change = self.project_config_regex.is_match(event.path.as_str())
            || (event.is_mutated_directory()
                && Self::is_a_project_root(&event.path, workspace_config)?);

        if !is_runtime_config && !is_project_change {
            return Ok(());
        }

        self.reload_child_context(state, &event.source_id).await
    }

    async fn reload_child_context(
        &mut self,
        state: AtomicDaemonState,
        source_id: &SourceRootId,
    ) -> miette::Result<()> {
        let mut source = self
            .session
            .source_workspaces
            .get(source_id)
            .cloned()
            .expect("Child source must be discovered before it can be reloaded.");
        let loader = self.session.config_loader.for_workspace_root(&source.root);
        let workspace_config = startup::load_workspace_config(loader, &source.root).await?;

        if workspace_config.id.as_ref().map(|id| id.as_str()) != Some(source_id.as_str()) {
            state.read().await.topology_changed.notify_one();

            return Ok(());
        }

        source.workspace_config = workspace_config;
        let context =
            startup::load_source_context(&self.session.config_loader, &source, false).await?;

        Arc::make_mut(&mut self.session.source_contexts).insert(source_id.clone(), context);
        Arc::make_mut(&mut self.session.source_workspaces).insert(source_id.clone(), source);
        self.session.reset_source_composition();
        self.recompose_graphs(&state).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::path::WorkspaceRelativePathBuf;
    use std::path::PathBuf;

    fn child_root_event(kind: EventKind) -> FileEvent {
        FileEvent {
            source_id: SourceRootId::new("child").unwrap(),
            path_original: PathBuf::from("/workspace/child"),
            path: WorkspaceRelativePathBuf::default(),
            kind,
        }
    }

    #[test]
    fn retires_for_empty_child_root_removal_event() {
        assert!(WorkspaceWatcher::should_retire_for_event(
            &child_root_event(EventKind::Remove(RemoveKind::Folder),)
        ));
    }

    #[test]
    fn retires_for_empty_child_root_rename_event() {
        assert!(WorkspaceWatcher::should_retire_for_event(
            &child_root_event(EventKind::Modify(ModifyKind::Name(RenameMode::From)),)
        ));
    }
}
