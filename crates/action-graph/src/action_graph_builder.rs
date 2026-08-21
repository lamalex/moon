use crate::action_graph::ActionGraph;
use crate::action_graph_error::ActionGraphError;
use daggy::{Dag, Walker};
use miette::IntoDiagnostic;
use moon_action::{
    ActionNode, InstallDependenciesNode, RunTaskNode, SetupEnvironmentNode, SetupToolchainNode,
    SyncProjectNode,
};
use moon_action_context::{ActionContext, TargetState};
use moon_affected::{AffectedTracker, AggregateAffectedTracker, DownstreamScope, UpstreamScope};
use moon_app_context::{AppContext, SourceRuntimeRegistry};
use moon_common::path::{PathExt, WorkspaceRelativePathBuf};
use moon_common::{Id, SourcePathBuf, SourceRootId, color, is_ci};
use moon_config::{EnvMap, PipelineActionSwitch, TaskDependencyConfig, TaskDependencyType};
use moon_exec_plan::{ExecutionPlan, TargetsBlock};
use moon_pdk_api::{DefineRequirementsInput, LocateDependenciesRootInput};
use moon_project::{Project, ProjectError};
use moon_query::{Criteria, build_query};
use moon_target::{ProjectKey, TaskInvocationKey};
use moon_task::{
    Target, TargetError, TargetLocator, TargetProjectScope, TargetTaskScope, Task, TaskKey,
};
use moon_toolchain::{DependenciesWorkspace, DependenciesWorkspaceRole, ToolchainSpec};
use moon_vcs::{ChangedFilesObservation, ImpactCompleteness};
use moon_workspace_graph::projects::ProjectGraphError;
use moon_workspace_graph::{GraphConnections, WorkspaceGraph};
use petgraph::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, instrument, trace};

macro_rules! insert_node_if_missing {
    ($builder:ident, $node:expr) => {{
        let node = $node;

        match $builder.get_index_from_node(&node) {
            Some(index) => index,
            None => $builder.insert_node(node),
        }
    }};
}

macro_rules! insert_node_or_exit {
    ($builder:ident, $node:expr) => {{
        let node = $node;

        match $builder.get_index_from_node(&node) {
            Some(index) => {
                return Ok(Some(index));
            }
            None => $builder.insert_node(node),
        }
    }};
}

#[derive(Clone, Debug)]
pub struct RunRequirements {
    pub ci: bool,                    // Are we in a CI environment
    pub ci_check: bool,              // Check the `runInCI` option
    pub dependencies: UpstreamScope, // Run dependency tasks
    pub dependents: DownstreamScope, // Run dependent tasks
    pub include_relations: bool,     // Include graph relations for affected
    pub interactive: bool,           // Entire pipeline is interactive
    pub job: Option<usize>,          // Current job index
    pub job_total: Option<usize>,    // Total amount of jobs
    pub skip_affected: bool,         // Skip all affected checks
}

impl Default for RunRequirements {
    fn default() -> Self {
        Self {
            ci: is_ci(),
            ci_check: false,
            dependencies: UpstreamScope::Deep,
            dependents: DownstreamScope::None,
            include_relations: false,
            interactive: false,
            job: None,
            job_total: None,
            skip_affected: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct RunPartition {
    pub targets: FxHashMap<NodeIndex, TaskKey>,
    pub size: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct RunTaskState {
    pub depth: u8,
    // Whether this task was reached by traversing a dependency (upstream)
    // edge. Dependents must not be expanded from such tasks, otherwise
    // downstream expansion restarts from inside dependency subtrees and
    // runs tasks that aren't dependents of the requested targets.
    pub via_dependency: bool,
}

pub struct ActionGraphBuilderOptions {
    pub install_dependencies: PipelineActionSwitch,
    pub setup_environment: PipelineActionSwitch,
    pub setup_toolchains: PipelineActionSwitch,
    pub sync_projects: PipelineActionSwitch,
    pub sync_project_dependencies: bool,
    pub sync_workspace: bool,
}

impl Default for ActionGraphBuilderOptions {
    fn default() -> Self {
        Self::new(true)
    }
}

impl ActionGraphBuilderOptions {
    pub fn new(state: bool) -> Self {
        Self {
            install_dependencies: state.into(),
            setup_environment: state.into(),
            setup_toolchains: state.into(),
            sync_projects: state.into(),
            sync_project_dependencies: state,
            sync_workspace: state,
        }
    }
}

pub struct ActionGraphBuilder<'query> {
    aggregate_attached: bool,
    aggregate_workspace_graph: Arc<WorkspaceGraph>,
    all_query: Option<Criteria<'query>>,
    app_context: Arc<AppContext>,
    graph: Dag<ActionNode, TaskDependencyType>,
    nodes: FxHashMap<ActionNode, NodeIndex>,
    options: ActionGraphBuilderOptions,
    source_runtime_registry: Arc<SourceRuntimeRegistry>,
    workspace_graph: Arc<WorkspaceGraph>,

    // Affected tracking
    affected: Option<AffectedTracker>,
    aggregate_affected: Option<AggregateAffectedTracker>,
    changed_files: Option<FxHashSet<WorkspaceRelativePathBuf>>,
    source_changed_files: BTreeMap<SourceRootId, FxHashSet<WorkspaceRelativePathBuf>>,
    source_changed_file_observations:
        BTreeMap<SourceRootId, ChangedFilesObservation<SourcePathBuf>>,

    // Target tracking
    ignored_dependencies: FxHashMap<TaskKey, FxHashSet<TaskKey>>,
    // Tasks whose dependents were out of scope when their node was created.
    // Consumed when the task is revisited with dependents in scope, since the
    // node-exists early return would otherwise skip the expansion entirely.
    ignored_dependents: FxHashSet<TaskKey>,
    passthrough_targets: FxHashSet<TaskInvocationKey>,
    primary_targets: FxHashSet<TaskKey>,

    // Proto and tool installs mutate manifests shared by every source using
    // the same store, so preserve insertion order across those actions.
    setup_tails: FxHashMap<PathBuf, (SourceRootId, NodeIndex)>,

    // Serial ordering edges added by `try_link_requirements`. Tracked so the
    // serial subtree walk doesn't follow them as if they were real dependency
    // edges (both use `TaskDependencyType::Required`), which would let it escape
    // into unrelated subtrees when nodes are shared across serial parents.
    serial_edges: FxHashSet<EdgeIndex>,
}

impl<'query> ActionGraphBuilder<'query> {
    pub fn new(
        app_context: Arc<AppContext>,
        workspace_graph: Arc<WorkspaceGraph>,
        options: ActionGraphBuilderOptions,
    ) -> miette::Result<Self> {
        debug!("Building action graph");
        workspace_graph.ensure_execution_local()?;

        let source_runtime_registry =
            Arc::new(SourceRuntimeRegistry::single(Arc::clone(&app_context)));

        Ok(ActionGraphBuilder {
            aggregate_attached: false,
            aggregate_workspace_graph: Arc::clone(&workspace_graph),
            affected: None,
            aggregate_affected: None,
            all_query: None,
            app_context,
            graph: Dag::new(),
            nodes: FxHashMap::default(),
            options,
            source_runtime_registry,
            ignored_dependencies: FxHashMap::default(),
            ignored_dependents: FxHashSet::default(),
            passthrough_targets: FxHashSet::default(),
            primary_targets: FxHashSet::default(),
            setup_tails: FxHashMap::default(),
            serial_edges: FxHashSet::default(),
            changed_files: None,
            source_changed_files: BTreeMap::new(),
            source_changed_file_observations: BTreeMap::new(),
            workspace_graph,
        })
    }

    /// Attach the aggregate graph and source-local services used for execution expansion.
    pub fn with_aggregate_workspace_graph(
        mut self,
        workspace_graph: Arc<WorkspaceGraph>,
        source_runtime_registry: Arc<SourceRuntimeRegistry>,
    ) -> Self {
        self.aggregate_attached = true;
        self.aggregate_workspace_graph = workspace_graph;
        self.source_runtime_registry = source_runtime_registry;
        self
    }

    pub fn build(mut self) -> (ActionContext, ActionGraph) {
        let mut context = ActionContext {
            affected: self.affected.take().map(|affected| affected.build()),
            aggregate_affected: self
                .aggregate_affected
                .take()
                .map(|affected| affected.build()),
            primary_source_id: self.app_context.source_id.clone(),
            ..ActionContext::default()
        };

        if !self.passthrough_targets.is_empty() {
            for target in mem::take(&mut self.passthrough_targets) {
                context.set_invocation_state(target, TargetState::Passthrough);
            }
        }

        if !self.ignored_dependencies.is_empty() {
            context.ignored_dependencies = mem::take(&mut self.ignored_dependencies);
        }

        if !self.primary_targets.is_empty() {
            context.primary_targets = mem::take(&mut self.primary_targets);
        }

        if let Some(files) = self.changed_files.take() {
            context.changed_files = files.to_owned();
        }

        context.source_changed_files = mem::take(&mut self.source_changed_files);

        // Reduce unncessary edges
        if let Some(index) = self.get_index_from_node(&ActionNode::sync_workspace(
            self.app_context.source_id.clone(),
        )) {
            self.graph.transitive_reduce(vec![index]);
        }

        let mut nodes = FxHashMap::default();

        // TODO switch to map_owned
        let graph = self.graph.map(
            |ni, node| {
                nodes.insert(ni, node.clone());
                ni
            },
            |_, edge| edge.to_owned(),
        );

        (context, ActionGraph::new(graph, nodes))
    }

    pub fn get_spec(&self, toolchain_id: &Id, project: Option<&Project>) -> Option<ToolchainSpec> {
        match project {
            Some(project) => self.get_project_spec(toolchain_id, project),
            None => self.get_workspace_spec(toolchain_id),
        }
    }

    pub fn get_project_spec(&self, toolchain_id: &Id, project: &Project) -> Option<ToolchainSpec> {
        let app_context = self.source_runtime_registry.get(&project.source_id).ok()?;

        self.get_project_spec_for(app_context, toolchain_id, project)
    }

    fn get_project_spec_for(
        &self,
        app_context: &AppContext,
        toolchain_id: &Id,
        project: &Project,
    ) -> Option<ToolchainSpec> {
        if let Some(config) = project.config.toolchains.get_plugin_config(toolchain_id) {
            if !config.is_enabled() {
                return None;
            }

            if let Some(version) = config.get_version() {
                return Some(ToolchainSpec::new(
                    toolchain_id.to_owned(),
                    version.to_owned(),
                ));
            }
        }

        self.get_workspace_spec_for(app_context, toolchain_id)
    }

    pub fn get_workspace_spec(&self, toolchain_id: &Id) -> Option<ToolchainSpec> {
        self.get_workspace_spec_for(&self.app_context, toolchain_id)
    }

    fn get_workspace_spec_for(
        &self,
        app_context: &AppContext,
        toolchain_id: &Id,
    ) -> Option<ToolchainSpec> {
        if let Some(config) = app_context
            .toolchains_config
            .get_plugin_config(toolchain_id)
        {
            return Some(match &config.version {
                Some(version) => ToolchainSpec::new(toolchain_id.to_owned(), version.to_owned()),
                None => ToolchainSpec::new_global(toolchain_id.to_owned()),
            });
        }

        None
    }

    pub fn set_affected(&mut self) -> miette::Result<()> {
        if self.aggregate_attached && self.aggregate_affected.is_none() {
            self.aggregate_affected = Some(AggregateAffectedTracker::new(
                Arc::clone(&self.aggregate_workspace_graph),
                mem::take(&mut self.source_changed_file_observations),
            )?);
        } else if !self.aggregate_attached && self.affected.is_none() {
            self.affected = Some(AffectedTracker::new(
                Arc::clone(&self.workspace_graph),
                self.changed_files
                    .as_ref()
                    .expect("Changed files are required for affected tracking.")
                    .to_owned(),
            ));
        }

        Ok(())
    }

    pub fn set_query(&mut self, input: &'query str) -> miette::Result<()> {
        self.all_query = Some(build_query(input)?);

        Ok(())
    }

    pub fn set_changed_files(
        &mut self,
        changed_files: FxHashSet<WorkspaceRelativePathBuf>,
    ) -> miette::Result<()> {
        self.source_changed_files
            .insert(self.app_context.source_id.clone(), changed_files.clone());
        let mut observation = ChangedFilesObservation {
            completeness: ImpactCompleteness::Exact,
            ..Default::default()
        };
        for file in &changed_files {
            observation.files.files.insert(
                SourcePathBuf::new(self.app_context.source_id.clone(), file.clone()),
                vec![],
            );
        }
        self.source_changed_file_observations
            .insert(self.app_context.source_id.clone(), observation);
        self.changed_files = Some(changed_files);

        Ok(())
    }

    pub fn set_source_changed_files(
        &mut self,
        observations: BTreeMap<SourceRootId, ChangedFilesObservation<SourcePathBuf>>,
    ) -> miette::Result<()> {
        self.source_changed_files = observations
            .iter()
            .map(|(source_id, observation)| {
                (
                    source_id.clone(),
                    observation
                        .files
                        .files
                        .keys()
                        .map(|file| file.path.clone())
                        .collect(),
                )
            })
            .collect();
        self.changed_files = Some(
            self.source_changed_files
                .get(&self.app_context.source_id)
                .cloned()
                .unwrap_or_default(),
        );
        self.source_changed_file_observations = observations;

        Ok(())
    }

    pub async fn track_affected(
        &mut self,
        upstream: UpstreamScope,
        downstream: DownstreamScope,
        ci_check: bool,
    ) -> miette::Result<()> {
        // If we require dependents, then we must load all projects into the
        // graph so that the edges are created!
        if downstream != DownstreamScope::None {
            debug!("Force loading all projects and tasks to determine relationships");

            self.aggregate_workspace_graph.get_projects()?;
            self.aggregate_workspace_graph.get_tasks_with_internal()?;
        }

        self.set_affected()?;

        if let Some(affected) = self.affected.as_mut() {
            affected.set_ci_check(ci_check);
            affected.set_scopes(upstream, downstream);

            // Projects must be tracked up front so that all marks exist
            // before tasks are inserted into the graph. Tracking lazily
            // during insertion makes the result dependent on insertion
            // order, as a project or task marked through another one's
            // relationship walk never runs its own checks and walks,
            // starving transitive dependents of marks.
            if self
                .app_context
                .workspace_config
                .experiments
                .async_affected_tracking
            {
                affected.track_projects_async().await?;
            } else {
                affected.track_projects()?;
            }
        } else if let Some(affected) = self.aggregate_affected.as_mut() {
            affected.set_ci_check(ci_check);
            affected.set_scopes(upstream, downstream);

            if self
                .app_context
                .workspace_config
                .experiments
                .async_affected_tracking
            {
                affected.track_projects_async().await?;
                affected.track_tasks_async().await?;
            } else {
                affected.track_projects()?.track_tasks()?;
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn internal_install_dependencies(
        &mut self,
        spec: &ToolchainSpec,
        project: Option<&Project>,
    ) -> miette::Result<Option<NodeIndex>> {
        let source_id = project
            .map(|project| &project.source_id)
            .unwrap_or(&self.app_context.source_id)
            .clone();
        let app_context = Arc::clone(self.source_runtime_registry.get(&source_id)?);

        // Explicitly disabled
        if spec.is_system()
            || !self.options.install_dependencies.is_enabled(&spec.id)
            || app_context
                .toolchains_config
                .get_plugin_config(&spec.id)
                .is_some_and(|cfg| !cfg.install_dependencies)
        {
            return Ok(None);
        }

        let sync_workspace_index = self.sync_workspace_for(&source_id).await?;
        let setup_toolchain_index = self.setup_toolchain(spec, project).await?;
        let toolchain_registry = &app_context.toolchain_registry;
        let toolchain = toolchain_registry.load(&spec.id).await?;

        // Toolchain does not support this action, so skip and fall through
        if !toolchain.supports_tier_2().await {
            return Ok(setup_toolchain_index);
        }

        let target_root = match project {
            Some(project) => &project.root,
            None => &app_context.workspace_root,
        };

        // Only insert this action if a root was located
        if let Some(deps_workspace) = self
            .locate_dependencies_root(&app_context, spec, project)
            .await?
            && let Some(deps_role) =
                toolchain.in_dependencies_workspace(&deps_workspace, target_root)?
        {
            // The action is scoped to the located root, so that every project
            // resolving to it collapses into the same action
            let root = deps_workspace
                .root
                .relative_to(&app_context.workspace_root)
                .into_diagnostic()?;

            // Unless there's no workspace, in which case the root is the only
            // package, and the project that owns it is associated, so that
            // project-level toolchain config is passed to the plugin. The
            // root may also not be owned by a project at all
            let project_key = match deps_role {
                DependenciesWorkspaceRole::PackageRoot => project.map(Project::key),
                _ => None,
            };

            let setup_env_index = self
                .internal_setup_environment(
                    spec,
                    &root,
                    project_key.as_ref().and(project),
                    FxHashSet::default(),
                )
                .await?;

            // Only create this action if the plugin supports it
            if toolchain.has_func("install_dependencies").await {
                let index = insert_node_if_missing!(
                    self,
                    ActionNode::install_dependencies(InstallDependenciesNode {
                        members: deps_workspace.members,
                        project_key,
                        root,
                        source_id: project
                            .map(|project| project.source_id.clone())
                            .unwrap_or_else(|| app_context.source_id.clone()),
                        toolchain_id: spec.id.clone(),
                    })
                );

                self.link_first_requirement(
                    index,
                    vec![setup_env_index, setup_toolchain_index, sync_workspace_index],
                )?;

                return Ok(Some(index));
            }

            // Otherwise pass through to setup environment
            if let Some(setup_env_index) = setup_env_index {
                self.link_first_requirement(
                    setup_env_index,
                    vec![setup_toolchain_index, sync_workspace_index],
                )?;

                return Ok(Some(setup_env_index));
            }
        }

        // Or fallback entirely to setup toolchain
        Ok(setup_toolchain_index)
    }

    #[instrument(skip(self))]
    pub async fn install_dependencies(
        &mut self,
        spec: &ToolchainSpec,
        project: &Project,
    ) -> miette::Result<Option<NodeIndex>> {
        self.internal_install_dependencies(spec, Some(project))
            .await
    }

    #[instrument(skip(self))]
    pub async fn install_dependencies_by_project(
        &mut self,
        project: &Project,
    ) -> miette::Result<Vec<Option<NodeIndex>>> {
        self.install_dependencies_by_toolchains(project, &project.toolchains)
            .await
    }

    #[instrument(skip(self))]
    pub async fn install_dependencies_by_toolchains(
        &mut self,
        project: &Project,
        toolchains: &[Id],
    ) -> miette::Result<Vec<Option<NodeIndex>>> {
        let mut indexes = vec![];

        for toolchain_id in toolchains {
            let app_context = Arc::clone(self.source_runtime_registry.get(&project.source_id)?);

            if let Some(spec) = self.get_project_spec_for(&app_context, toolchain_id, project) {
                indexes.push(self.install_dependencies(&spec, project).await?);
            }
        }

        Ok(indexes)
    }

    #[instrument(skip(self))]
    pub async fn install_dependencies_root(
        &mut self,
        spec: &ToolchainSpec,
    ) -> miette::Result<Option<NodeIndex>> {
        let app_context = Arc::clone(
            self.source_runtime_registry
                .get(&self.app_context.source_id)?,
        );

        // Explicitly disabled
        if spec.is_system()
            || !self.options.install_dependencies.is_enabled(&spec.id)
            || app_context
                .toolchains_config
                .get_plugin_config(&spec.id)
                .is_some_and(|cfg| !cfg.install_dependencies)
        {
            return Ok(None);
        }

        // Only insert actions if the dependencies root is the workspace root
        if self
            .locate_dependencies_root(&app_context, spec, None)
            .await?
            .is_none_or(|deps_workspace| deps_workspace.root != app_context.workspace_root)
        {
            return Ok(None);
        }

        self.internal_install_dependencies(spec, None).await
    }

    async fn locate_dependencies_root(
        &self,
        app_context: &AppContext,
        spec: &ToolchainSpec,
        project: Option<&Project>,
    ) -> miette::Result<Option<DependenciesWorkspace>> {
        let toolchain_registry = &app_context.toolchain_registry;
        let toolchain = toolchain_registry.load(&spec.id).await?;

        // Toolchain does not support locating a root, so return
        // an empty output instead of failing the function call
        if !toolchain.supports_tier_2().await {
            return Ok(None);
        }

        toolchain
            .locate_dependencies_root(match project {
                Some(project) => LocateDependenciesRootInput {
                    context: toolchain_registry.create_context(),
                    starting_dir: toolchain.to_virtual_path(&project.root),
                    toolchain_config: toolchain_registry
                        .create_merged_config(&toolchain.id, &project.config),
                },
                None => LocateDependenciesRootInput {
                    context: toolchain_registry.create_context(),
                    starting_dir: toolchain.to_virtual_path(&app_context.workspace_root),
                    toolchain_config: toolchain_registry.create_config(&toolchain.id),
                },
            })
            .await
    }

    #[instrument(skip(self))]
    pub async fn run_task(
        &mut self,
        task: &Task,
        reqs: &RunRequirements,
    ) -> miette::Result<Option<NodeIndex>> {
        if let Some(index) =
            Box::pin(self.internal_run_task(task, reqs, None, &mut RunTaskState::default())).await?
        {
            // Only track primary targets at the top-level run methods,
            // as these are explicitly called by pipeline consumers!
            self.primary_targets.insert(task.key());

            return Ok(Some(index));
        }

        Ok(None)
    }

    #[cfg(debug_assertions)]
    pub async fn run_task_with_config(
        &mut self,
        task: &Task,
        reqs: &RunRequirements,
        config: &TaskDependencyConfig,
    ) -> miette::Result<Option<NodeIndex>> {
        Box::pin(self.internal_run_task(task, reqs, Some(config), &mut RunTaskState::default()))
            .await
    }

    #[instrument(skip(self))]
    pub async fn run_task_by_target<T: AsRef<Target> + Debug>(
        &mut self,
        target: T,
        reqs: &RunRequirements,
    ) -> miette::Result<FxHashSet<NodeIndex>> {
        let target = target.as_ref();
        let mut indexes = FxHashSet::default();

        for task in self
            .internal_resolve_tasks_from_target(target, false)
            .await?
        {
            if let Some(index) = self.run_task(&task, reqs).await? {
                indexes.insert(index);
            }
        }

        Ok(indexes)
    }

    #[instrument(skip(self))]
    pub async fn run_task_by_target_locator<T: AsRef<TargetLocator> + Debug>(
        &mut self,
        locator: T,
        reqs: &RunRequirements,
    ) -> miette::Result<FxHashSet<NodeIndex>> {
        let locator = locator.as_ref();
        let mut indexes = FxHashSet::default();

        for task in self
            .internal_resolve_tasks_from_target_locator(locator, false)
            .await?
        {
            if let Some(index) = self.run_task(&task, reqs).await? {
                indexes.insert(index);
            }
        }

        Ok(indexes)
    }

    #[instrument(skip(self))]
    pub async fn run_tasks<I: IntoIterator<Item = T> + Debug, T: AsRef<TargetLocator> + Debug>(
        &mut self,
        locators: I,
        reqs: RunRequirements,
    ) -> miette::Result<RunPartition> {
        let mut tasks = vec![];
        let mut partition = RunPartition::default();

        for locator in locators {
            tasks.extend(
                self.internal_resolve_tasks_from_target_locator(locator.as_ref(), false)
                    .await?,
            );
        }

        // Determine affected status of each task up front, as marking
        // lazily during insertion makes the result dependent on target
        // order: a task marked through another task's relationship walk
        // never runs its own checks and walks, starving its transitive
        // dependents of marks and dropping them from the graph
        if !reqs.skip_affected
            && let Some(affected) = &mut self.affected
        {
            let is_async = self
                .app_context
                .workspace_config
                .experiments
                .async_affected_tracking;

            // When including relations, every task must be tracked, not just the
            // requested ones. A task is only marked through a relation when the
            // task on the other side of it has been marked itself, and that task
            // is quite often not one that was requested
            if reqs.include_relations {
                if is_async {
                    affected.track_tasks_async().await?;
                } else {
                    affected.track_tasks()?;
                }
            } else if is_async {
                affected.track_tasks_by_instance_async(&tasks).await?;
            } else {
                affected.track_tasks_by_instance(&tasks)?;
            }
        }

        // Now partition the tasks list based on the job information
        if let Some(job_index) = reqs.job
            && let Some(job_total) = reqs.job_total
            && job_total > 0
        {
            if job_index > job_total - 1 {
                return Err(ActionGraphError::InvalidJobIndex {
                    index: job_index,
                    total: job_total,
                }
                .into());
            }

            // If we are going to parallelize, then we need to filter the
            // tasks list based on affected state before partitioning!
            if !reqs.skip_affected {
                let mut new_tasks = vec![];

                for task in tasks {
                    if self.is_task_affected(&task, &reqs)? {
                        new_tasks.push(task);
                    }
                }

                tasks = new_tasks;
            }

            // Then slice and partition the tasks based on the job index and total
            let size = tasks.len().div_ceil(job_total);
            let (start, stop) =
                // beginning
                if job_index == 0 {
                    (0, size)
                }
                // end
                else if job_index == job_total - 1 {
                    ((size * job_index), tasks.len())
                }
                // middle
                else {
                    ((size * job_index), (size * (job_index + 1)))
                };

            if tasks.get(start).is_some() {
                if tasks.get(stop).is_some() {
                    tasks = tasks[start..stop].to_vec();
                } else {
                    tasks = tasks[start..].to_vec();
                }
            }

            partition.size = Some(size);
        }

        for task in tasks {
            if let Some(index) = self.run_task(&task, &reqs).await? {
                partition.targets.insert(index, task.key());
            }
        }

        Ok(partition)
    }

    #[instrument(skip(self, plan))]
    pub async fn run_tasks_with_plan(
        &mut self,
        plan: &ExecutionPlan,
        mut reqs: RunRequirements,
    ) -> miette::Result<RunPartition> {
        match &plan.targets {
            TargetsBlock::Partitioned { jobs } => {
                let mut partition = RunPartition::default();

                if let Some(job_index) = reqs.job
                    && let Some(job_total) = reqs.job_total
                    && job_total > 0
                {
                    if job_index > job_total - 1 {
                        return Err(ActionGraphError::InvalidJobIndex {
                            index: job_index,
                            total: job_total,
                        }
                        .into());
                    }

                    if jobs.len() != job_total {
                        return Err(ActionGraphError::MismatchedPlanJobTotals {
                            plan_total: jobs.len(),
                            total: job_total,
                        }
                        .into());
                    }

                    // Reset job handling since we already did it
                    reqs.job = None;
                    reqs.job_total = None;

                    let targets = &jobs[job_index];

                    partition.targets = self.run_tasks(targets, reqs).await?.targets;
                    partition.size = Some(targets.len());
                } else {
                    return Err(ActionGraphError::InvalidPlanJobs.into());
                }

                Ok(partition)
            }
            TargetsBlock::Filtered { include } => self.run_tasks(include, reqs).await,
            TargetsBlock::Included(targets) => self.run_tasks(targets, reqs).await,
        }
    }

    #[instrument(skip(self))]
    pub async fn run_task_dependencies(
        &mut self,
        task: &Task,
        reqs: &RunRequirements,
        state: &RunTaskState,
    ) -> miette::Result<Vec<(Option<NodeIndex>, TaskDependencyType)>> {
        let parallel = task.options.run_deps_in_parallel;
        let mut indexes = vec![];
        let mut previous_target_index: Option<NodeIndex> = None;
        let dependencies = self.resolved_dependencies_for(task);

        for dependency in dependencies {
            let dep_task = self
                .aggregate_workspace_graph
                .get_task_by_key(&dependency.task_key)?;
            let dep_config = TaskDependencyConfig {
                args: dependency.args,
                env: dependency.env,
                target: dep_task.target.clone(),
                optional: Some(dependency.dependency_type == TaskDependencyType::Optional),
                cache_strategy: Some(dependency.cache_strategy),
            };
            let mut dep_state = state.clone();
            dep_state.via_dependency = true;

            if let Some(dep_index) =
                Box::pin(self.internal_run_task(&dep_task, reqs, Some(&dep_config), &mut dep_state))
                    .await?
            {
                // When serial, this dependency's entire task subtree must
                // run after the previous dependency — not just the
                // dependency node itself. Otherwise its own transitive
                // dependencies (grandchildren) would run in parallel with
                // earlier serial dependencies. Cycle-forming edges are
                // skipped, which can happen when the same task node appears
                // in multiple serial dependency chains across parent tasks.
                if !parallel && let Some(prev) = previous_target_index {
                    self.link_serial_requirements(dep_index, prev);
                }

                // The parent always depends on each child directly, as
                // serial chain edges alone can't guarantee this ordering
                // when a chain edge is skipped for forming a cycle
                indexes.push((Some(dep_index), dependency.dependency_type));

                previous_target_index = Some(dep_index);
            }
        }

        Ok(indexes)
    }

    #[instrument(skip(self))]
    pub async fn run_task_dependents(
        &mut self,
        task: &Task,
        reqs: &RunRequirements,
        state: &RunTaskState,
    ) -> miette::Result<Vec<Option<NodeIndex>>> {
        let mut indexes = vec![];

        let dependent_keys = self.aggregate_workspace_graph.tasks.dependents_of(task);

        for dep_key in dependent_keys {
            let dep_task = self.aggregate_workspace_graph.get_task_by_key(&dep_key)?;
            // Dependent chains reset the marker, so that deep scopes
            // keep cascading through transitive dependents
            let mut dep_state = state.clone();
            dep_state.via_dependency = false;

            indexes.push(
                Box::pin(self.internal_run_task(&dep_task, reqs, None, &mut dep_state)).await?,
            );
        }

        Ok(indexes)
    }

    #[instrument(skip(self))]
    async fn internal_resolve_tasks_from_target(
        &mut self,
        target: &Target,
        allow_internal: bool,
    ) -> miette::Result<Vec<Arc<Task>>> {
        let mut tasks = vec![];

        // First, find all projects based on the target scope
        let (scope, scope_value) = target.get_project_scope();

        let (projects, bubble_error) = match scope {
            // :task
            TargetProjectScope::All => {
                let mut projects = vec![];

                if let Some(all_query) = &self.all_query {
                    projects.extend(self.workspace_graph.query_projects(all_query)?);
                } else {
                    projects.extend(self.workspace_graph.get_projects()?);
                };

                (projects, false)
            }
            // project:task
            TargetProjectScope::Id => {
                let project = self.workspace_graph.get_project(scope_value)?;

                (vec![project], true)
            }
            // #tag:task
            TargetProjectScope::Tag => {
                let projects = self
                    .workspace_graph
                    .query_projects(build_query(format!("projectTag={scope_value}").as_str())?)?;

                (projects, false)
            }
            // ^:task, ^build:task, etc.
            TargetProjectScope::Deps | TargetProjectScope::DepsOf(_) => {
                return Err(TargetError::NoDepsInRunContext.into());
            }
            // ~:task
            TargetProjectScope::OwnSelf => {
                return Err(TargetError::NoSelfInRunContext.into());
            }
        };

        // Second, find all tasks based on the task scope for each project
        let (scope, scope_value) = target.get_task_scope();

        for project in projects {
            let project_tasks = match scope {
                TargetTaskScope::Id => {
                    match self
                        .workspace_graph
                        .get_task_from_project(&project.id, scope_value)
                    {
                        Ok(task) => vec![task],
                        Err(error) => {
                            if bubble_error {
                                return Err(error);
                            } else {
                                continue;
                            }
                        }
                    }
                }
                TargetTaskScope::Tag => self
                    .workspace_graph
                    .get_tasks_from_project(&project.id)?
                    .into_iter()
                    .filter(|task| task.tags.iter().any(|tag| tag == scope_value))
                    .collect(),
            };

            for project_task in project_tasks {
                if !allow_internal && project_task.is_internal() {
                    if bubble_error {
                        return Err(ProjectError::UnknownTask {
                            task_id: project_task.id.to_string(),
                            project_id: project.id.to_string(),
                        }
                        .into());
                    } else {
                        continue;
                    }
                }

                tasks.push(project_task);
            }
        }

        Ok(tasks)
    }

    #[instrument(skip(self))]
    async fn internal_resolve_tasks_from_target_locator(
        &mut self,
        locator: &TargetLocator,
        allow_internal: bool,
    ) -> miette::Result<Vec<Arc<Task>>> {
        let mut tasks = vec![];

        match locator {
            TargetLocator::GlobMatch {
                project,
                project_glob,
                task_glob,
                ..
            } => {
                let mut is_all = false;
                let mut do_query = false;
                let mut projects = vec![];

                // Query for all applicable projects first since we can't
                // query projects + tasks at the same time
                if let Some(glob) = project_glob {
                    let query = if let Some(tag_glob) = glob.strip_prefix('#') {
                        format!("projectTag~{tag_glob}")
                    } else {
                        format!("project~{glob}")
                    };

                    projects = self.workspace_graph.query_projects(build_query(&query)?)?;
                    do_query = !projects.is_empty();
                } else {
                    match project {
                        Some(TargetProjectScope::All) => {
                            is_all = true;
                            do_query = true;
                        }
                        _ => {
                            // Don't query for the other scopes,
                            // since they're not valid from the run context
                        }
                    };
                }

                // Then query for all tasks within the queried projects
                if do_query {
                    let mut query = if let Some(tag_glob) = task_glob.strip_prefix('#') {
                        format!("taskTag~{tag_glob}")
                    } else {
                        format!("task~{task_glob}")
                    };

                    if !is_all {
                        query = format!(
                            "project=[{}] && {query}",
                            projects
                                .into_iter()
                                .map(|project| project.id.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        );
                    }

                    for task in self.workspace_graph.query_tasks(build_query(&query)?)? {
                        if !allow_internal && task.is_internal() {
                            continue;
                        }

                        tasks.push(task);
                    }
                }
            }
            TargetLocator::Qualified(target) => {
                let target = if target.project == TargetProjectScope::OwnSelf {
                    Target::new(
                        self.workspace_graph
                            .get_project_from_path(None)?
                            .id
                            .as_str(),
                        target.get_task_id()?,
                    )?
                } else {
                    target.to_owned()
                };

                tasks.extend(
                    self.internal_resolve_tasks_from_target(&target, allow_internal)
                        .await?,
                );
            }
            TargetLocator::DefaultProject(task_id) => {
                let project = self.workspace_graph.get_default_project().map_err(|_| {
                    ProjectGraphError::NoDefaultProjectForTask {
                        task_id: task_id.to_string(),
                    }
                })?;

                let target = Target::new(&project.id, task_id)?;

                tasks.extend(
                    self.internal_resolve_tasks_from_target(&target, allow_internal)
                        .await?,
                );
            }
        };

        Ok(tasks)
    }

    async fn internal_run_task(
        &mut self,
        task: &Task,
        reqs: &RunRequirements,
        config: Option<&TaskDependencyConfig>,
        state: &mut RunTaskState,
    ) -> miette::Result<Option<NodeIndex>> {
        let task = if self.aggregate_attached {
            self.aggregate_workspace_graph
                .get_task_by_key(&task.key())?
        } else {
            Arc::new(task.clone())
        };
        let task_key = task.key();
        let invocation_key = TaskInvocationKey::new(
            task_key.clone(),
            config.into_iter().flat_map(|config| &config.args),
            config
                .into_iter()
                .flat_map(|config| &config.env)
                .map(|(key, value)| (key, value.as_ref())),
        );
        let _app_context = self
            .source_runtime_registry
            .get(task_key.project_key().source_id())?;
        let project = self
            .aggregate_workspace_graph
            .get_project_by_key(task_key.project_key())?;
        let mut child_reqs = reqs.clone();

        // Abort early if not affected
        if !self.is_task_affected(&task, reqs)? {
            return Ok(None);
        }

        // These tasks shouldn't actually run, so filter them out
        if self.passthrough_targets.contains(&invocation_key) {
            debug!(
                task_target = task.target.as_str(),
                "Not running task {} because it has been marked as passthrough",
                color::id(&task.target.id),
            );

            return Ok(None);
        }

        // Track depth information before proceeding
        let should_run_dependencies = reqs.dependencies.is_in_scope(state.depth);
        let should_run_dependents =
            !state.via_dependency && reqs.dependents.is_in_scope(state.depth);
        state.depth += 1;

        // Only apply CI checks when requested
        if reqs.ci_check && !task.should_run(reqs.ci) {
            self.passthrough_targets.insert(invocation_key);

            debug!(
                task_target = task.target.as_str(),
                "Not running task {} because {} has been configured not to",
                color::id(&task.target.id),
                color::property("runInCI"),
            );

            // Dependents may still want to run though!
            if should_run_dependents {
                child_reqs.skip_affected = false;

                Box::pin(self.run_task_dependents(&task, &child_reqs, state)).await?;
            }

            return Ok(None);
        }

        // Create the node
        let mut args = vec![];
        let mut env = EnvMap::default();

        if let Some(config) = config {
            args.extend(config.args.clone());
            env.extend(config.env.clone());
        }

        let node = ActionNode::run_task(RunTaskNode {
            args,
            env,
            interactive: task.is_interactive() || reqs.interactive,
            persistent: task.is_persistent(),
            priority: task.options.priority.get_level(),
            key: task_key.clone(),
            target: task.target.to_owned(),
            id: None,
        });

        let had_ignored_dependencies = if should_run_dependencies {
            self.ignored_dependencies.remove(&task_key).is_some()
        } else {
            false
        };

        let had_ignored_dependents = if should_run_dependents {
            self.ignored_dependents.remove(&task_key)
        } else {
            false
        };

        // Check if the node exists to avoid all the overhead below
        if let Some(index) = self.get_index_from_node(&node) {
            if had_ignored_dependencies
                && !self
                    .aggregate_workspace_graph
                    .tasks
                    .resolved_dependencies_of(&task_key)
                    .is_empty()
            {
                child_reqs.skip_affected = true;

                let edges = Box::pin(self.run_task_dependencies(&task, &child_reqs, state)).await?;

                self.link_task_requirements(index, edges)?;
            }

            if had_ignored_dependents {
                child_reqs.skip_affected = false;

                Box::pin(self.run_task_dependents(&task, &child_reqs, state)).await?;
            }

            return Ok(Some(index));
        }

        // Create initial edges
        let mut prerequisite_edges = vec![self.sync_project(&project, reqs).await?];

        prerequisite_edges.extend(
            self.install_dependencies_by_toolchains(&project, &task.toolchains)
                .await?,
        );

        // If no edges created, we should at minimum sync the workspace
        if prerequisite_edges.is_empty() || prerequisite_edges.iter().all(|edge| edge.is_none()) {
            prerequisite_edges.push(self.sync_workspace_for(&project.source_id).await?);
        }

        // Insert and then link edges
        let index = self.insert_node(node);

        let dependencies = self
            .aggregate_workspace_graph
            .tasks
            .resolved_dependencies_of(&task_key);
        let has_dependencies = !dependencies.is_empty();
        let ignored_dependencies = dependencies
            .iter()
            .map(|dependency| dependency.task_key.clone())
            .collect::<FxHashSet<_>>();
        let dependency_edges = if has_dependencies && should_run_dependencies {
            child_reqs.skip_affected = true;

            Box::pin(self.run_task_dependencies(&task, &child_reqs, state)).await?
        } else {
            vec![]
        };

        if has_dependencies && !should_run_dependencies {
            self.ignored_dependencies
                .insert(task_key.clone(), ignored_dependencies);
        }

        self.link_optional_requirements(index, prerequisite_edges)?;
        self.link_task_requirements(index, dependency_edges)?;

        // And possibly dependents
        if should_run_dependents {
            child_reqs.skip_affected = false;

            Box::pin(self.run_task_dependents(&task, &child_reqs, state)).await?;
        } else {
            self.ignored_dependents.insert(task_key);
        }

        Ok(Some(index))
    }

    #[instrument(skip(self))]
    async fn internal_setup_environment(
        &mut self,
        spec: &ToolchainSpec,
        root: &WorkspaceRelativePathBuf,
        project: Option<&Project>,
        mut cycle: FxHashSet<&Id>,
    ) -> miette::Result<Option<NodeIndex>> {
        let source_id = project
            .map(|project| &project.source_id)
            .unwrap_or(&self.app_context.source_id)
            .clone();
        let app_context = Arc::clone(self.source_runtime_registry.get(&source_id)?);

        // Explicitly disabled
        if !self.options.setup_environment.is_enabled(&spec.id)
            || spec.is_system()
            || cycle.contains(&spec.id)
        {
            return Ok(None);
        }

        let toolchain_registry = &app_context.toolchain_registry;
        let toolchain = toolchain_registry.load(&spec.id).await?;
        let mut edges = vec![];

        cycle.insert(&spec.id);

        // Toolchain may depend on others
        let output = toolchain
            .define_requirements(DefineRequirementsInput {
                context: toolchain_registry.create_context(),
                toolchain_config: match project {
                    Some(project) => {
                        toolchain_registry.create_merged_config(&toolchain.id, &project.config)
                    }
                    None => toolchain_registry.create_config(&toolchain.id),
                },
            })
            .await?;

        if !output.requires.is_empty() && output.for_setup_environment {
            for require_id in output.requires {
                let require_id = Id::new(require_id)?;

                if require_id != spec.id {
                    // Skip if already in cycle
                    if cycle.contains(&require_id) {
                        continue;
                    }

                    let require_spec = match project {
                        Some(project) => {
                            self.get_project_spec_for(&app_context, &require_id, project)
                        }
                        None => self.get_workspace_spec_for(&app_context, &require_id),
                    };

                    if let Some(require_spec) = require_spec {
                        // Requires the toolchain to be setup, not the environment!
                        edges.push(Box::pin(self.setup_toolchain(&require_spec, project)).await?);
                    } else {
                        return Err(ActionGraphError::MissingToolchainRequirement {
                            id: spec.id.to_string(),
                            dep_id: require_id.to_string(),
                        }
                        .into());
                    }
                }
            }
        }

        // Toolchain does not support it
        if !toolchain.has_func("setup_environment").await && edges.is_empty() {
            return Ok(None);
        }

        edges.push(self.sync_workspace_for(&source_id).await?);
        edges.push(self.setup_toolchain(spec, project).await?);

        let index = insert_node_or_exit!(
            self,
            ActionNode::setup_environment(SetupEnvironmentNode {
                project_key: project.map(Project::key),
                root: root.clone(),
                source_id: project
                    .map(|project| project.source_id.clone())
                    .unwrap_or_else(|| app_context.source_id.clone()),
                toolchain_id: spec.id.clone(),
            })
        );

        self.link_optional_requirements(index, edges)?;

        Ok(Some(index))
    }

    #[instrument(skip(self))]
    pub async fn setup_environment(
        &mut self,
        spec: &ToolchainSpec,
        root: &WorkspaceRelativePathBuf,
        project: &Project,
    ) -> miette::Result<Option<NodeIndex>> {
        self.internal_setup_environment(spec, root, Some(project), FxHashSet::default())
            .await
    }

    /// Setup the environment in the workspace root, without an associated
    /// project. Unlike the project-based flow, this inserts no actions at
    /// all unless the located dependencies root is the workspace root.
    #[instrument(skip(self))]
    pub async fn setup_environment_root(
        &mut self,
        spec: &ToolchainSpec,
    ) -> miette::Result<Option<NodeIndex>> {
        let app_context = Arc::clone(
            self.source_runtime_registry
                .get(&self.app_context.source_id)?,
        );

        // Explicitly disabled
        if !self.options.setup_environment.is_enabled(&spec.id) || spec.is_system() {
            return Ok(None);
        }

        // Only insert actions if the dependencies root is the workspace root
        if self
            .locate_dependencies_root(&app_context, spec, None)
            .await?
            .is_none_or(|deps_workspace| deps_workspace.root != app_context.workspace_root)
        {
            return Ok(None);
        }

        self.internal_setup_environment(
            spec,
            &WorkspaceRelativePathBuf::new(),
            None,
            FxHashSet::default(),
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn setup_proto(&mut self) -> miette::Result<Option<NodeIndex>> {
        let source_id = self.app_context.source_id.clone();
        self.setup_proto_for(&source_id).await
    }

    async fn setup_proto_for(
        &mut self,
        source_id: &moon_common::SourceRootId,
    ) -> miette::Result<Option<NodeIndex>> {
        let app_context = Arc::clone(self.source_runtime_registry.get(source_id)?);
        let index = insert_node_or_exit!(
            self,
            ActionNode::setup_proto(
                source_id.clone(),
                app_context.toolchains_config.proto.version.clone(),
            )
        );

        self.order_shared_setup(index, &app_context)?;

        Ok(Some(index))
    }

    #[instrument(skip(self))]
    pub async fn setup_toolchain(
        &mut self,
        spec: &ToolchainSpec,
        project: Option<&Project>,
    ) -> miette::Result<Option<NodeIndex>> {
        Box::pin(self.internal_setup_toolchain(spec, project, FxHashSet::default())).await
    }

    async fn internal_setup_toolchain(
        &mut self,
        spec: &ToolchainSpec,
        project: Option<&Project>,
        mut cycle: FxHashSet<&Id>,
    ) -> miette::Result<Option<NodeIndex>> {
        let source_id = project
            .map(|project| &project.source_id)
            .unwrap_or(&self.app_context.source_id)
            .clone();
        let app_context = Arc::clone(self.source_runtime_registry.get(&source_id)?);

        // Explicitly disabled
        if !self.options.setup_toolchains.is_enabled(&spec.id)
            || spec.is_system()
            || cycle.contains(&spec.id)
        {
            return Ok(None);
        }

        let toolchain_registry = &app_context.toolchain_registry;
        let toolchain = toolchain_registry.load(&spec.id).await?;
        let mut edges = vec![];

        cycle.insert(&spec.id);

        // Toolchain may depend on others
        let output = toolchain
            .define_requirements(DefineRequirementsInput {
                context: toolchain_registry.create_context(),
                toolchain_config: match project {
                    Some(project) => {
                        toolchain_registry.create_merged_config(&toolchain.id, &project.config)
                    }
                    None => toolchain_registry.create_config(&toolchain.id),
                },
            })
            .await?;

        if !output.requires.is_empty() && output.for_setup_toolchain {
            for require_id in output.requires {
                let require_id = Id::new(require_id)?;

                if require_id != spec.id {
                    // Skip if already in cycle
                    if cycle.contains(&require_id) {
                        continue;
                    }

                    let require_spec = match project {
                        Some(project) => {
                            self.get_project_spec_for(&app_context, &require_id, project)
                        }
                        None => self.get_workspace_spec_for(&app_context, &require_id),
                    };

                    if let Some(require_spec) = require_spec {
                        edges.push(
                            Box::pin(self.internal_setup_toolchain(
                                &require_spec,
                                project,
                                cycle.clone(),
                            ))
                            .await?,
                        );
                    } else {
                        return Err(ActionGraphError::MissingToolchainRequirement {
                            id: spec.id.to_string(),
                            dep_id: require_id.to_string(),
                        }
                        .into());
                    }
                }
            }
        }

        // Toolchain does not support tier 3 and does not require other toolchains
        if !toolchain.supports_tier_3().await && edges.is_empty() {
            return Ok(None);
        }

        edges.push(self.sync_workspace_for(&source_id).await?);

        if spec.req.is_some() || app_context.toolchains_config.requires_proto() {
            edges.push(self.setup_proto_for(&source_id).await?);
        }

        let node = ActionNode::setup_toolchain(SetupToolchainNode {
            source_id: project
                .map(|project| project.source_id.clone())
                .unwrap_or_else(|| app_context.source_id.clone()),
            toolchain: spec.to_owned(),
        });
        let (index, inserted) = match self.get_index_from_node(&node) {
            Some(index) => (index, false),
            None => (self.insert_node(node), true),
        };

        self.link_optional_requirements(index, edges)?;

        if inserted {
            self.order_shared_setup(index, &app_context)?;
        }

        Ok(Some(index))
    }

    #[instrument(skip(self))]
    pub async fn sync_project(
        &mut self,
        project: &Project,
        reqs: &RunRequirements,
    ) -> miette::Result<Option<NodeIndex>> {
        Box::pin(self.internal_sync_project(project, reqs, FxHashSet::default())).await
    }

    async fn internal_sync_project(
        &mut self,
        project: &Project,
        reqs: &RunRequirements,
        mut cycle: FxHashSet<ProjectKey>,
    ) -> miette::Result<Option<NodeIndex>> {
        let project_key = project.key();

        // Explicitly disabled
        if !self.options.sync_projects.is_enabled(&project.id) || cycle.contains(&project_key) {
            return Ok(None);
        }

        self.source_runtime_registry.get(&project.source_id)?;

        // Return early if not affected
        if !self.is_project_affected(project, reqs)? {
            return Ok(None);
        }

        // Insert the node and edges
        let mut edges = vec![];

        cycle.insert(project_key.clone());

        if let Some(sync_workspace_index) = self.sync_workspace_for(&project.source_id).await? {
            edges.push(sync_workspace_index);
        }

        let index = insert_node_or_exit!(
            self,
            ActionNode::sync_project(SyncProjectNode { project_key })
        );

        // We should also depend on other projects
        if self.options.sync_project_dependencies {
            for dependency_key in self
                .aggregate_workspace_graph
                .projects
                .dependencies_of(project)
            {
                if cycle.contains(&dependency_key) {
                    continue;
                }

                let dep_project = self
                    .aggregate_workspace_graph
                    .get_project_by_key(&dependency_key)?;

                if let Some(dep_project_index) =
                    Box::pin(self.internal_sync_project(&dep_project, reqs, cycle.clone())).await?
                    && index != dep_project_index
                {
                    edges.push(dep_project_index);
                }
            }
        }

        if !edges.is_empty() {
            self.link_requirements(index, edges)?;
        }

        Ok(Some(index))
    }

    #[instrument(skip(self))]
    pub async fn sync_workspace(&mut self) -> miette::Result<Option<NodeIndex>> {
        let source_id = self.app_context.source_id.clone();
        self.sync_workspace_for(&source_id).await
    }

    async fn sync_workspace_for(
        &mut self,
        source_id: &moon_common::SourceRootId,
    ) -> miette::Result<Option<NodeIndex>> {
        if !self.options.sync_workspace {
            return Ok(None);
        }

        self.source_runtime_registry.get(source_id)?;

        let index = insert_node_or_exit!(self, ActionNode::sync_workspace(source_id.clone()));

        Ok(Some(index))
    }

    // PRIVATE

    fn get_index_from_node(&self, node: &ActionNode) -> Option<NodeIndex> {
        self.nodes.get(node).cloned()
    }

    fn order_shared_setup(
        &mut self,
        index: NodeIndex,
        app_context: &AppContext,
    ) -> miette::Result<()> {
        let store = app_context.proto_env.store.dir.clone();

        if let Some((previous_source, previous)) = self
            .setup_tails
            .insert(store, (app_context.source_id.clone(), index))
            && previous_source != app_context.source_id
        {
            self.link_requirements(index, vec![previous])?;
        }

        Ok(())
    }

    fn resolved_dependencies_for(
        &self,
        task: &Task,
    ) -> Vec<moon_workspace_graph::tasks::ResolvedTaskDependency> {
        let mut resolved = self
            .aggregate_workspace_graph
            .tasks
            .resolved_dependencies_of(&task.key())
            .to_vec();
        resolved.sort_by_key(|dependency| dependency.declaration_ordinal);
        resolved
    }

    fn link_first_requirement(
        &mut self,
        index: NodeIndex,
        edges: Vec<Option<NodeIndex>>,
    ) -> miette::Result<()> {
        if let Some(edge) = edges.into_iter().flatten().next() {
            self.link_requirements(index, vec![edge])?;
        }

        Ok(())
    }

    fn link_optional_requirements(
        &mut self,
        index: NodeIndex,
        edges: Vec<Option<NodeIndex>>,
    ) -> miette::Result<()> {
        self.link_requirements(index, edges.into_iter().flatten().collect())
    }

    fn link_task_requirements(
        &mut self,
        index: NodeIndex,
        edges: Vec<(Option<NodeIndex>, TaskDependencyType)>,
    ) -> miette::Result<()> {
        for (edge, dependency_type) in edges {
            let Some(edge) = edge else {
                continue;
            };

            if self.graph.find_edge(index, edge).is_none() {
                self.graph
                    .add_edge(index, edge, dependency_type)
                    .map_err(|_| ActionGraphError::WouldCycle {
                        source_action: self.graph.node_weight(index).unwrap().label(),
                        target_action: self.graph.node_weight(edge).unwrap().label(),
                    })?;
            }
        }

        Ok(())
    }

    fn link_requirements(&mut self, index: NodeIndex, edges: Vec<NodeIndex>) -> miette::Result<()> {
        if edges.is_empty() {
            return Ok(());
        }

        let mut added_edges = vec![];

        for edge in edges {
            if self.graph.find_edge(index, edge).is_none() {
                self.graph
                    .add_edge(index, edge, TaskDependencyType::Required)
                    .map_err(|_| ActionGraphError::WouldCycle {
                        source_action: self.graph.node_weight(index).unwrap().label(),
                        target_action: self.graph.node_weight(edge).unwrap().label(),
                    })?;

                added_edges.push(edge);
            }
        }

        if !added_edges.is_empty() {
            trace!(
                index = index.index(),
                requires = ?added_edges.iter().map(|edge| edge.index()).collect::<Vec<_>>(),
                "Linking requirements for index"
            );
        }

        Ok(())
    }

    /// Add serial ordering edges from every task within `index`'s dependency
    /// subtree (including `index` itself) to `previous`, so the whole subtree
    /// runs after the previous serial dependency. The subtree is discovered by
    /// walking dependency edges — from a node to the tasks it requires — at link
    /// time (rather than collected during insertion) because a subtree shared
    /// with another target is inserted once and then reused via node
    /// deduplication. Only `RunTask` nodes are ordered; non-task nodes such as
    /// project syncs must not be forced to wait on the previous dependency.
    ///
    /// Serial ordering edges (tracked in `serial_edges`) are skipped during the
    /// walk. They share the `Required` edge type with real dependencies, so
    /// following them — e.g. a `b -> a` edge left by an earlier serial parent on
    /// a shared node `b` — would let the walk escape `index`'s real subtree and
    /// wrongly order unrelated tasks. Cycle-forming edges are skipped when
    /// linked via [`Self::try_link_requirements`].
    fn link_serial_requirements(&mut self, index: NodeIndex, previous: NodeIndex) {
        let mut visited = FxHashSet::default();
        let mut ordered = vec![];
        let mut stack = vec![index];

        // Collect the subtree first, then link. Linking mutates the graph (it
        // adds `node -> previous` edges), so it must not run mid-walk or the new
        // edges would pollute the traversal. `ordered` keeps the link order
        // deterministic — and thus the graph's edge order stable — regardless
        // of set iteration order.
        while let Some(node_index) = stack.pop() {
            if !visited.insert(node_index) {
                continue;
            }

            ordered.push(node_index);

            let mut children = self.graph.children(node_index);

            while let Some((edge_index, child_index)) = children.walk_next(&self.graph) {
                if !self.serial_edges.contains(&edge_index)
                    && matches!(
                        self.graph.node_weight(child_index),
                        Some(ActionNode::RunTask(_))
                    )
                {
                    stack.push(child_index);
                }
            }
        }

        for node_index in ordered {
            self.try_link_requirements(node_index, previous);
        }
    }

    /// Try to add a serial ordering edge between two dependency nodes, recording
    /// it in `serial_edges` so the subtree walk in
    /// [`Self::link_serial_requirements`] won't mistake it for a real
    /// dependency. Silently skips the edge if it would introduce a cycle — this
    /// happens when the same task node appears in multiple serial dependency
    /// chains across different parent tasks.
    fn try_link_requirements(&mut self, index: NodeIndex, edge: NodeIndex) {
        if self.graph.find_edge(index, edge).is_none()
            && let Ok(edge_index) = self
                .graph
                .add_edge(index, edge, TaskDependencyType::Required)
        {
            self.serial_edges.insert(edge_index);

            trace!(
                index = index.index(),
                requires = ?[edge.index()],
                "Linking requirements for index"
            );
        }
    }

    fn insert_node(&mut self, node: ActionNode) -> NodeIndex {
        let label = node.label();
        let index = self.graph.add_node(node.clone());

        self.nodes.insert(node, index);

        debug!(
            index = index.index(),
            "Adding {} to graph",
            color::muted_light(label)
        );

        index
    }

    fn is_project_affected(
        &mut self,
        project: &Project,
        reqs: &RunRequirements,
    ) -> miette::Result<bool> {
        if let Some(affected) = &self.aggregate_affected
            && !reqs.skip_affected
        {
            let key = project.key();

            return Ok(if reqs.include_relations {
                affected.build_ref().is_project_affected(&key)
            } else {
                affected
                    .build_ref()
                    .is_project_affected_ignoring_relations(&key)
            });
        }

        if let Some(affected) = &mut self.affected
            && !reqs.skip_affected
        {
            // Short-circuit early if the task is already marked
            let marked = if reqs.include_relations {
                affected.is_project_marked(project)
            } else {
                affected.is_project_marked_ignoring_relations(project)
            };

            if marked {
                return Ok(true);
            }

            // Otherwise run the full affected checks
            if let Some(mark) = affected.is_project_affected(project) {
                affected.mark_project_affected(project, mark)?;

                return Ok(true);
            }

            return Ok(false);
        }

        // Always affected
        Ok(true)
    }

    fn is_task_affected(&mut self, task: &Task, reqs: &RunRequirements) -> miette::Result<bool> {
        if let Some(affected) = &self.aggregate_affected
            && !reqs.skip_affected
        {
            let key = task.key();

            return Ok(if reqs.include_relations {
                affected.build_ref().is_task_affected(&key)
            } else {
                affected
                    .build_ref()
                    .is_task_affected_ignoring_relations(&key)
            });
        }

        if let Some(affected) = &mut self.affected
            && !reqs.skip_affected
        {
            // Short-circuit early if the task is already marked
            let marked = if reqs.include_relations {
                affected.is_task_marked(task)
            } else {
                affected.is_task_marked_ignoring_relations(task)
            };

            if marked {
                return Ok(true);
            }

            // Otherwise run the full affected checks
            if let Some(mark) = affected.is_task_affected(task)? {
                affected.mark_task_affected(task, mark)?;

                return Ok(true);
            }

            return Ok(false);
        }

        // Always affected
        Ok(true)
    }
}

#[cfg(debug_assertions)]
impl ActionGraphBuilder<'_> {
    pub fn mock_affected(
        &mut self,
        changed_files: FxHashSet<WorkspaceRelativePathBuf>,
        mut op: impl FnMut(&mut AffectedTracker),
    ) {
        self.set_changed_files(changed_files).unwrap();
        self.set_affected().unwrap();

        if let Some(affected) = self.affected.as_mut() {
            op(affected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_app_context::SourceRuntime;
    use moon_common::{SourceRegistry, SourceRootId};
    use moon_config::{DependencyScope, ProjectDependencyConfig, TaskDependencyCacheStrategy};
    use moon_test_utils::WorkspaceMocker;
    use moon_workspace_graph::GraphExpanderContext;
    use moon_workspace_graph::projects::{ProjectGraph, ProjectNode};
    use moon_workspace_graph::tasks::{TaskGraph, TaskNode};
    use starbase_sandbox::create_sandbox;
    use std::fs;
    use std::path::PathBuf;

    fn create_toolchain_spec(id: &str) -> ToolchainSpec {
        ToolchainSpec::new(
            Id::raw(id),
            moon_config::UnresolvedVersionSpec::parse("1.2.3").unwrap(),
        )
    }

    async fn create_builder(root: &std::path::Path) -> ActionGraphBuilder<'static> {
        let mocker = WorkspaceMocker::new(root)
            .load_default_configs()
            .with_all_toolchains()
            .with_test_toolchains()
            .with_default_projects()
            .with_global_envs();

        ActionGraphBuilder::new(
            Arc::new(mocker.mock_app_context()),
            Arc::new(mocker.mock_workspace_graph().await),
            Default::default(),
        )
        .unwrap()
    }

    fn graph_context(source_id: SourceRootId, root: PathBuf) -> GraphExpanderContext {
        GraphExpanderContext {
            sources: Arc::new(SourceRegistry::new(source_id, root.clone())),
            working_dir: root.clone(),
            workspace_root: root,
            ..Default::default()
        }
    }

    fn local_project_graph(context: GraphExpanderContext, project: Project) -> Arc<ProjectGraph> {
        let mut projects = ProjectGraph::new(context);
        let mut graph = DiGraph::new();
        let index = graph.add_node(NodeIndex::new(0));
        let key = project.key();
        projects.indexes.insert(index, key.clone());
        projects.nodes.insert(key, ProjectNode { index, project });
        projects.set_graph(graph).unwrap();
        Arc::new(projects)
    }

    fn local_task_graph(
        context: GraphExpanderContext,
        projects: Arc<ProjectGraph>,
        tasks: Vec<Task>,
        edges: &[(usize, usize, TaskDependencyType)],
    ) -> Arc<TaskGraph> {
        let mut task_graph = TaskGraph::new(context, projects);

        for task in tasks {
            let index = task_graph
                .graph
                .add_node(NodeIndex::new(task_graph.graph.node_count()));
            let key = task.key();
            task_graph.indexes.insert(index, key.clone());
            task_graph.nodes.insert(key, TaskNode { index, task });
        }

        for (owner, dependency, dependency_type) in edges {
            task_graph
                .graph
                .add_edge(
                    NodeIndex::new(*owner),
                    NodeIndex::new(*dependency),
                    *dependency_type,
                )
                .unwrap();
        }

        task_graph.resolve_source_local_dependencies().unwrap();
        Arc::new(task_graph)
    }

    #[tokio::test]
    async fn serializes_shared_setup_across_sources() {
        let sandbox = create_sandbox("projects");
        let mut builder = create_builder(sandbox.path()).await;
        let primary_id = SourceRootId::primary();
        let child_id = SourceRootId::new("child").unwrap();
        let primary = Arc::clone(&builder.app_context);
        let mut child = (*primary).clone();
        child.source_id = child_id.clone();
        Arc::make_mut(&mut child.toolchains_config).proto.version =
            moon_toolchain::VersionSpec::parse("9.8.7").unwrap();
        let child = Arc::new(child);
        builder.source_runtime_registry = Arc::new(
            SourceRuntimeRegistry::new(
                Arc::clone(&primary),
                [(
                    child_id.clone(),
                    SourceRuntime::Available(Arc::clone(&child)),
                )],
            )
            .unwrap(),
        );
        let spec = create_toolchain_spec("tc-tier3");
        let child_project = Project {
            source_id: child_id.clone(),
            ..Project::default()
        };

        builder.setup_toolchain(&spec, None).await.unwrap();
        builder
            .setup_toolchain(&spec, Some(&child_project))
            .await
            .unwrap();

        let (_, graph) = builder.build();
        let primary_toolchain = find_node_index(
            &graph,
            |node| matches!(node, ActionNode::SetupToolchain(node) if node.source_id == primary_id),
        );
        let child_proto = find_node_index(
            &graph,
            |node| matches!(node, ActionNode::SetupProto(node) if node.source_id == child_id),
        );
        assert!(
            graph
                .get_inner_graph()
                .find_edge(child_proto, primary_toolchain)
                .is_some()
        );

        let ordered = graph
            .sort_topological()
            .unwrap()
            .into_iter()
            .filter_map(|index| match graph.get_node_from_index(&index).unwrap() {
                ActionNode::SetupProto(node) => {
                    Some(("proto", node.source_id.clone(), node.version.to_string()))
                }
                ActionNode::SetupToolchain(node) => Some((
                    "toolchain",
                    node.source_id.clone(),
                    node.toolchain.req.as_ref().unwrap().to_string(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            ordered,
            [
                (
                    "proto",
                    primary_id.clone(),
                    primary.toolchains_config.proto.version.to_string(),
                ),
                ("toolchain", primary_id, "1.2.3".into()),
                ("proto", child_id.clone(), "9.8.7".into()),
                ("toolchain", child_id, "1.2.3".into()),
            ]
        );
        assert_eq!(primary.proto_env.store.dir, child.proto_env.store.dir);
    }

    #[tokio::test]
    async fn builds_cross_source_dependencies_from_canonical_metadata() {
        let root = std::env::temp_dir().join(format!(
            "moon-action-graph-cross-source-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let child_root = root.join("child-source");
        let primary_id = SourceRootId::primary();
        let child_id = SourceRootId::new("child").unwrap();
        let primary_context = graph_context(primary_id.clone(), root.clone());
        let child_context = graph_context(child_id.clone(), child_root.clone());
        let mut primary_project = Project {
            id: Id::raw("app"),
            source_id: primary_id.clone(),
            ..Project::default()
        };
        primary_project
            .cross_source_dependencies
            .push(ProjectDependencyConfig {
                id: Id::raw("app"),
                scope: DependencyScope::Build,
                source_root: Some(child_id.clone()),
                ..Default::default()
            });
        let child_project = Project {
            id: Id::raw("app"),
            source_id: child_id.clone(),
            ..Project::default()
        };
        let primary_projects = local_project_graph(primary_context.clone(), primary_project);
        let child_projects = local_project_graph(child_context.clone(), child_project);
        let required_local = TaskDependencyConfig {
            args: vec!["--local".into()],
            target: Target::new("app", "build").unwrap(),
            cache_strategy: Some(TaskDependencyCacheStrategy::Hash),
            ..Default::default()
        };
        let required_cross = TaskDependencyConfig {
            args: vec!["--cross".into()],
            env: EnvMap::from_iter([("CROSS".into(), Some("1".into()))]),
            target: Target::parse("^:build").unwrap(),
            cache_strategy: Some(TaskDependencyCacheStrategy::Outputs),
            ..Default::default()
        };
        let optional_cross = TaskDependencyConfig {
            target: Target::parse("^:lint").unwrap(),
            optional: Some(true),
            cache_strategy: Some(TaskDependencyCacheStrategy::Ignored),
            ..Default::default()
        };
        let mut root_task = Task {
            id: Id::raw("root"),
            source_id: primary_id.clone(),
            target: Target::new("app", "root").unwrap(),
            deps: vec![required_local.clone()],
            configured_deps: vec![required_cross, optional_cross],
            ..Task::default()
        };
        root_task.options.run_deps_in_parallel = false;
        let primary_build = Task {
            id: Id::raw("build"),
            source_id: primary_id.clone(),
            target: Target::new("app", "build").unwrap(),
            ..Task::default()
        };
        let mut child_build = Task {
            id: Id::raw("build"),
            source_id: child_id.clone(),
            target: Target::new("app", "build").unwrap(),
            ..Task::default()
        };
        child_build
            .input_files
            .insert("changed.txt".into(), moon_task::TaskFileInput::default());
        let child_build_for_run = child_build.clone();
        let child_lint = Task {
            id: Id::raw("lint"),
            source_id: child_id.clone(),
            target: Target::new("app", "lint").unwrap(),
            ..Task::default()
        };
        let primary_tasks = local_task_graph(
            primary_context,
            Arc::clone(&primary_projects),
            vec![root_task.clone(), primary_build],
            &[(0, 1, TaskDependencyType::Required)],
        );
        let child_tasks = local_task_graph(
            child_context,
            Arc::clone(&child_projects),
            vec![child_build, child_lint],
            &[],
        );
        let mut sources = SourceRegistry::new(primary_id.clone(), root.clone());
        sources.register(child_id.clone(), child_root).unwrap();
        let sources = Arc::new(sources);
        let aggregate_projects = Arc::new(
            ProjectGraph::compose(
                Arc::clone(&sources),
                &FxHashMap::default(),
                [Arc::clone(&primary_projects), child_projects],
            )
            .unwrap(),
        );
        let aggregate = Arc::new(
            WorkspaceGraph::new_aggregate(
                aggregate_projects,
                Arc::clone(&sources),
                [Arc::clone(&primary_tasks), child_tasks],
            )
            .unwrap(),
        );
        let resolved = aggregate.tasks.resolved_dependencies_of(&root_task.key());
        assert!(resolved.iter().any(|dependency| {
            dependency.task_key.project_key().source_id() == &child_id
                && dependency.cache_strategy == TaskDependencyCacheStrategy::Outputs
        }));
        assert!(resolved.iter().any(|dependency| {
            dependency.task_key.project_key().source_id() == &child_id
                && dependency.cache_strategy == TaskDependencyCacheStrategy::Ignored
        }));
        let local = Arc::new(WorkspaceGraph::new_with_sources(
            primary_projects,
            primary_tasks,
            Arc::new(SourceRegistry::single(root.clone())),
        ));
        let mocker = WorkspaceMocker::new(&root);
        let primary_app = Arc::new(mocker.mock_app_context());
        let mut child_app = mocker.mock_app_context();
        child_app.source_id = child_id.clone();
        child_app.workspace_root = root.join("child-source");
        let runtimes = Arc::new(
            SourceRuntimeRegistry::new(
                Arc::clone(&primary_app),
                [(
                    child_id.clone(),
                    SourceRuntime::Available(Arc::new(child_app)),
                )],
            )
            .unwrap(),
        );
        let unavailable_runtimes = Arc::new(
            SourceRuntimeRegistry::new(
                Arc::clone(&primary_app),
                [(
                    child_id.clone(),
                    SourceRuntime::Unavailable("offline".into()),
                )],
            )
            .unwrap(),
        );
        let mut unavailable_builder = ActionGraphBuilder::new(
            Arc::clone(&primary_app),
            Arc::clone(&local),
            ActionGraphBuilderOptions::new(false),
        )
        .unwrap()
        .with_aggregate_workspace_graph(Arc::clone(&aggregate), unavailable_runtimes);
        let error = unavailable_builder
            .run_task_by_target(&root_task.target, &RunRequirements::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("child") && error.contains("offline"));

        let mut affected_builder = ActionGraphBuilder::new(
            Arc::clone(&primary_app),
            Arc::clone(&local),
            ActionGraphBuilderOptions::new(false),
        )
        .unwrap()
        .with_aggregate_workspace_graph(Arc::clone(&aggregate), Arc::clone(&runtimes));
        let mut child_observation = ChangedFilesObservation {
            completeness: ImpactCompleteness::Exact,
            ..Default::default()
        };
        child_observation
            .files
            .files
            .insert(SourcePathBuf::new(child_id.clone(), "changed.txt"), vec![]);
        affected_builder
            .set_source_changed_files(BTreeMap::from([
                (primary_id.clone(), ChangedFilesObservation::default()),
                (child_id.clone(), child_observation),
            ]))
            .unwrap();
        affected_builder
            .track_affected(UpstreamScope::None, DownstreamScope::Direct, false)
            .await
            .unwrap();
        affected_builder
            .run_task_by_target(
                &root_task.target,
                &RunRequirements {
                    include_relations: true,
                    ..RunRequirements::default()
                },
            )
            .await
            .unwrap();
        let (affected_context, affected_graph) = affected_builder.build();
        assert!(
            affected_context
                .aggregate_affected
                .as_ref()
                .unwrap()
                .is_task_affected(&root_task.key())
        );
        find_node_index(
            &affected_graph,
            |node| matches!(node, ActionNode::RunTask(inner) if inner.key == root_task.key()),
        );
        find_node_index(
            &affected_graph,
            |node| matches!(node, ActionNode::RunTask(inner) if inner.key == child_build_for_run.key()),
        );

        let mut dependent_builder = ActionGraphBuilder::new(
            Arc::clone(&primary_app),
            Arc::clone(&local),
            ActionGraphBuilderOptions::new(false),
        )
        .unwrap()
        .with_aggregate_workspace_graph(Arc::clone(&aggregate), Arc::clone(&runtimes));
        dependent_builder
            .run_task(
                &child_build_for_run,
                &RunRequirements {
                    dependencies: UpstreamScope::None,
                    dependents: DownstreamScope::Direct,
                    ..RunRequirements::default()
                },
            )
            .await
            .unwrap();
        let (_, dependent_graph) = dependent_builder.build();
        find_node_index(
            &dependent_graph,
            |node| matches!(node, ActionNode::RunTask(inner) if inner.target.as_str() == "app:root"),
        );

        let mut builder =
            ActionGraphBuilder::new(primary_app, local, ActionGraphBuilderOptions::new(false))
                .unwrap()
                .with_aggregate_workspace_graph(aggregate, runtimes);

        builder
            .run_task_by_target(&root_task.target, &RunRequirements::default())
            .await
            .unwrap();

        let (_, graph) = builder.build();
        let inner = graph.get_inner_graph();
        let nodes = graph.get_inner_nodes();
        let mut builds = inner
            .graph()
            .node_indices()
            .filter(|index| {
                matches!(nodes[index], ActionNode::RunTask(ref node) if node.target.as_str() == "app:build")
            })
            .collect::<Vec<_>>();
        builds.sort_by_key(|index| index.index());
        assert_eq!(builds.len(), 2);
        assert_ne!(nodes[&builds[0]].source_id(), nodes[&builds[1]].source_id());

        let root_index = find_node_index(
            &graph,
            |node| matches!(node, ActionNode::RunTask(inner) if inner.target.as_str() == "app:root"),
        );
        let lint_index = find_node_index(
            &graph,
            |node| matches!(node, ActionNode::RunTask(inner) if inner.target.as_str() == "app:lint"),
        );
        assert_eq!(
            inner.edge_weight(inner.find_edge(root_index, lint_index).unwrap()),
            Some(&TaskDependencyType::Optional)
        );
        assert!(builds.iter().all(|index| {
            inner
                .find_edge(root_index, *index)
                .and_then(|edge| inner.edge_weight(edge))
                == Some(&TaskDependencyType::Required)
        }));
        assert!(
            inner.find_edge(builds[1], builds[0]).is_some()
                || inner.find_edge(builds[0], builds[1]).is_some()
        );
        assert!(nodes.values().any(|node| {
            matches!(node, ActionNode::RunTask(inner) if inner.args == ["--cross"] && inner.env.get("CROSS") == Some(&Some("1".into())))
        }));
    }

    fn find_node_index(
        graph: &ActionGraph,
        mut predicate: impl FnMut(&ActionNode) -> bool,
    ) -> NodeIndex {
        let inner = graph.get_inner_graph();
        let nodes = graph.get_inner_nodes();

        inner
            .graph()
            .node_indices()
            .find(|index| nodes.get(index).is_some_and(&mut predicate))
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_toolchain_requirements_path_local_across_siblings() {
        let sandbox = create_sandbox("projects");
        let mut builder = create_builder(sandbox.path()).await;
        let id = Id::raw("root");
        let cycle = FxHashSet::from_iter([&id]);

        builder
            .internal_setup_toolchain(&create_toolchain_spec("tc-tier3-reqs"), None, cycle.clone())
            .await
            .unwrap();
        builder
            .internal_setup_toolchain(&create_toolchain_spec("tc-tier2-reqs"), None, cycle.clone())
            .await
            .unwrap();

        let (_, graph) = builder.build();
        let inner = graph.get_inner_graph();
        let child_index = find_node_index(&graph, |node| {
            matches!(
                node,
                ActionNode::SetupToolchain(inner)
                    if inner.toolchain.id.as_str() == "tc-tier2-reqs"
            )
        });
        let shared_index = find_node_index(&graph, |node| {
            matches!(
                node,
                ActionNode::SetupToolchain(inner)
                    if inner.toolchain.id.as_str() == "tc-tier3"
            )
        });

        assert!(inner.find_edge(child_index, shared_index).is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_project_dependencies_path_local_across_siblings() {
        let sandbox = create_sandbox("projects");

        fs::write(
            sandbox.path().join("foo/moon.yml"),
            "dependsOn: [bar, qux]\n",
        )
        .unwrap();
        fs::write(
            sandbox.path().join("bar/moon.yml"),
            "dependsOn: [baz]\nlanguage: javascript\n",
        )
        .unwrap();
        fs::write(
            sandbox.path().join("qux/moon.yml"),
            "dependsOn: [baz]\nlanguage: rust\n\ntoolchains:\n  rust:\n    version: '1.90.0'\n",
        )
        .unwrap();

        let mocker = WorkspaceMocker::new(sandbox.path())
            .load_default_configs()
            .with_all_toolchains()
            .with_test_toolchains()
            .with_default_projects()
            .with_global_envs();
        let workspace_graph = Arc::new(mocker.mock_workspace_graph().await);
        let mut builder = ActionGraphBuilder::new(
            Arc::new(mocker.mock_app_context()),
            Arc::clone(&workspace_graph),
            Default::default(),
        )
        .unwrap();
        let cycle =
            FxHashSet::from_iter([
                ProjectKey::new(SourceRootId::primary(), Id::raw("root")).unwrap()
            ]);

        builder
            .internal_sync_project(
                &workspace_graph.get_project("bar").unwrap(),
                &RunRequirements::default(),
                cycle.clone(),
            )
            .await
            .unwrap();
        builder
            .internal_sync_project(
                &workspace_graph.get_project("qux").unwrap(),
                &RunRequirements::default(),
                cycle.clone(),
            )
            .await
            .unwrap();

        let (_, graph) = builder.build();
        let inner = graph.get_inner_graph();
        let qux_index = find_node_index(&graph, |node| {
            matches!(
                node,
                ActionNode::SyncProject(inner)
                    if inner.project_key.project_id().as_str() == "qux"
            )
        });
        let baz_index = find_node_index(&graph, |node| {
            matches!(
                node,
                ActionNode::SyncProject(inner)
                    if inner.project_key.project_id().as_str() == "baz"
            )
        });

        assert!(inner.find_edge(qux_index, baz_index).is_some());
    }
}
