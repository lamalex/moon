use crate::TaskGraphError;
use daggy::Dag;
use moon_config::{
    DependencyScope, EnvMap, TaskDependencyCacheStrategy, TaskDependencyConfig, TaskDependencyType,
};
use moon_graph_utils::*;
use moon_project::ProjectError;
use moon_project_graph::ProjectGraph;
use moon_target::{Target, TaskKey};
use moon_task::{
    TargetDependencyScope, TargetProjectScope, TargetTaskScope, Task,
    TaskDependencyValidationError, validate_task_dependency,
};
use moon_task_expander::{TaskExpander, TaskLookup};
use once_cell::sync::OnceCell;
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use rustc_hash::FxHashMap;
use scc::hash_map::Entry;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{debug, instrument};

#[derive(Clone, Debug, Default)]
pub struct TaskNode {
    pub index: NodeIndex,
    pub task: Task,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTaskDependency {
    pub task_key: TaskKey,
    pub cache_strategy: TaskDependencyCacheStrategy,
    pub dependency_type: TaskDependencyType,
    pub args: Vec<String>,
    pub env: EnvMap,
}

#[derive(Debug, Default)]
pub struct TaskGraph {
    pub context: GraphExpanderContext,

    /// Expansion contexts indexed by the source that owns each task.
    contexts: FxHashMap<moon_common::SourceRootId, GraphExpanderContext>,

    /// Directed-acyclic graph (DAG) of non-expanded tasks and their relationships.
    pub graph: Dag<NodeIndex, TaskDependencyType>,

    /// Map of node indexes to canonical task keys.
    pub indexes: FxHashMap<NodeIndex, TaskKey>,

    /// Map of task nodes by canonical key.
    pub nodes: FxHashMap<TaskKey, TaskNode>,

    /// Canonical resolved dependency metadata by owning task key.
    resolved_dependencies: FxHashMap<TaskKey, Vec<ResolvedTaskDependency>>,

    /// Project graph, required for expansion.
    project_graph: Arc<ProjectGraph>,

    /// Map of expanded tasks by canonical key.
    tasks: Arc<scc::HashMap<TaskKey, Arc<OnceCell<Arc<Task>>>>>,
}

impl TaskGraph {
    pub fn new(context: GraphExpanderContext, project_graph: Arc<ProjectGraph>) -> Self {
        debug!("Creating task graph");

        let mut contexts = FxHashMap::default();
        contexts.insert(context.sources.primary_id().clone(), context.clone());

        Self {
            context,
            contexts,
            project_graph,
            ..Default::default()
        }
    }

    /// Compose finalized source-local task graphs into a read-only aggregate graph.
    pub fn compose(
        project_graph: Arc<ProjectGraph>,
        graphs: impl IntoIterator<Item = Arc<TaskGraph>>,
    ) -> miette::Result<Self> {
        let mut graphs = graphs.into_iter().collect::<Vec<_>>();
        graphs.sort_by(|a, b| {
            a.context
                .sources
                .primary_id()
                .cmp(b.context.sources.primary_id())
        });

        let primary_id = project_graph.context.sources.primary_id();
        let primary = graphs
            .iter()
            .find(|graph| graph.context.sources.primary_id() == primary_id)
            .ok_or_else(|| {
                miette::miette!("No task graph has been loaded for source {primary_id}.")
            })?;
        let mut context = primary.context.clone();
        context.sources = Arc::clone(&project_graph.context.sources);
        context.workspace_root = project_graph.context.workspace_root.clone();

        let mut aggregate = Self::new(context, project_graph);
        aggregate.contexts.clear();
        let mut local_edges = vec![];

        for local in graphs {
            let source_id = local.context.sources.primary_id().clone();
            aggregate.contexts.insert(source_id, local.context.clone());

            let mut keys = local.nodes.keys().cloned().collect::<Vec<_>>();
            keys.sort();

            for key in keys {
                if aggregate.nodes.contains_key(&key) {
                    return Err(miette::miette!(
                        "Duplicate task key {key} while composing task graphs."
                    ));
                }

                let task = local.nodes[&key].task.clone();
                let index = aggregate
                    .graph
                    .add_node(NodeIndex::new(aggregate.graph.node_count()));
                aggregate.indexes.insert(index, key.clone());
                aggregate.nodes.insert(key, TaskNode { index, task });
            }

            for edge in local.graph.graph().edge_references() {
                let source_key = &local.indexes[&edge.source()];
                let target_key = &local.indexes[&edge.target()];

                if source_key.project_key().source_id() != target_key.project_key().source_id() {
                    return Err(TaskGraphError::UnsupportedCrossSourceEdge {
                        source_key: source_key.to_string(),
                        target_key: target_key.to_string(),
                    }
                    .into());
                }

                local_edges.push((source_key.clone(), target_key.clone(), *edge.weight()));
            }
        }

        local_edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

        for (source_key, target_key, dependency_type) in local_edges {
            aggregate.add_edge(&source_key, &target_key, dependency_type)?;
        }

        aggregate.resolve_source_local_dependencies()?;
        aggregate.add_composed_project_dependencies()?;

        Ok(aggregate)
    }

    fn add_composed_project_dependencies(&mut self) -> miette::Result<()> {
        let mut source_keys = self.nodes.keys().cloned().collect::<Vec<_>>();
        source_keys.sort();
        let mut edges = vec![];

        for source_key in source_keys {
            let task = &self.nodes[&source_key].task;

            for dep_config in &task.configured_deps {
                let (project_scope, _) = dep_config.target.get_project_scope();
                let required_scope = match project_scope {
                    TargetProjectScope::Deps => None,
                    TargetProjectScope::DepsOf(scope) => Some(match scope {
                        TargetDependencyScope::Build => DependencyScope::Build,
                        TargetDependencyScope::Development => DependencyScope::Development,
                        TargetDependencyScope::Peer => DependencyScope::Peer,
                        TargetDependencyScope::Production => DependencyScope::Production,
                    }),
                    _ => continue,
                };
                let dependency_projects = self
                    .project_graph
                    .direct_dependencies_with_scopes(source_key.project_key())?
                    .into_iter()
                    .filter_map(|(key, scope)| {
                        required_scope
                            .is_none_or(|required| scope == required)
                            .then_some(key)
                    })
                    .collect::<Vec<_>>();
                let (task_scope, task_value) = dep_config.target.get_task_scope();
                let mut matches = self
                    .nodes
                    .iter()
                    .filter_map(|(key, node)| {
                        if !dependency_projects.contains(key.project_key()) {
                            return None;
                        }

                        let matches = match task_scope {
                            TargetTaskScope::Id => node.task.id.as_str() == task_value,
                            TargetTaskScope::Tag => {
                                node.task.tags.iter().any(|tag| tag.as_str() == task_value)
                            }
                        };

                        matches.then(|| key.clone())
                    })
                    .collect::<Vec<_>>();
                matches.sort();
                matches.dedup();

                if matches.is_empty() && !dep_config.optional.unwrap_or(true) {
                    return Err(TaskGraphError::UnknownDepTargetParentScope {
                        dep: dep_config.target.to_string(),
                        task: source_key.to_string(),
                    }
                    .into());
                }

                for target_key in matches {
                    let dep_task = &self.nodes[&target_key].task;
                    validate_task_dependency(&task.options, &dep_task.options).map_err(
                        |error| {
                            let dep = target_key.to_string();
                            let task = source_key.to_string();

                            match error {
                                TaskDependencyValidationError::AllowFailure => {
                                    TaskGraphError::AllowFailureDepRequirement { dep, task }
                                }
                                TaskDependencyValidationError::RunInCi => {
                                    TaskGraphError::RunInCiDepRequirement { dep, task }
                                }
                                TaskDependencyValidationError::Persistent => {
                                    TaskGraphError::PersistentDepRequirement { dep, task }
                                }
                            }
                        },
                    )?;

                    let dependency_type = if dep_config.optional.is_some_and(|optional| optional) {
                        TaskDependencyType::Optional
                    } else {
                        TaskDependencyType::Required
                    };

                    edges.push((
                        source_key.clone(),
                        target_key,
                        dependency_type,
                        dep_config.clone(),
                    ));
                }
            }
        }

        edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

        for (source_key, target_key, dependency_type, config) in edges {
            self.add_edge(&source_key, &target_key, dependency_type)?;
            self.add_resolved_dependency(&source_key, &target_key, dependency_type, &config);
        }

        Ok(())
    }

    fn add_edge(
        &mut self,
        source_key: &TaskKey,
        target_key: &TaskKey,
        dependency_type: TaskDependencyType,
    ) -> miette::Result<()> {
        let source_index = self.nodes[source_key].index;
        let target_index = self.nodes[target_key].index;

        if self
            .graph
            .graph()
            .find_edge(source_index, target_index)
            .is_some_and(|edge| {
                let current = self.graph.edge_weight_mut(edge).unwrap();

                if *current == TaskDependencyType::Optional
                    && dependency_type == TaskDependencyType::Required
                {
                    *current = TaskDependencyType::Required;
                }

                true
            })
        {
            return Ok(());
        }

        self.graph
            .add_edge(source_index, target_index, dependency_type)
            .map_err(|_| TaskGraphError::WouldCycle {
                source_target: source_key.to_string(),
                target_target: target_key.to_string(),
            })?;

        Ok(())
    }

    fn add_resolved_dependency(
        &mut self,
        owner: &TaskKey,
        dependency: &TaskKey,
        dependency_type: TaskDependencyType,
        config: &TaskDependencyConfig,
    ) {
        let cache_strategy = config.cache_strategy.unwrap_or_else(|| {
            if self.nodes[dependency].task.has_outputs() {
                TaskDependencyCacheStrategy::Hash
            } else {
                TaskDependencyCacheStrategy::Ignored
            }
        });
        let resolved = ResolvedTaskDependency {
            task_key: dependency.clone(),
            cache_strategy,
            dependency_type,
            args: config.args.clone(),
            env: config.env.clone(),
        };
        let dependencies = self.resolved_dependencies.entry(owner.clone()).or_default();

        if let Some(current) = dependencies
            .iter_mut()
            .find(|current| current.task_key == *dependency)
        {
            let current_required = current.dependency_type == TaskDependencyType::Required;
            let incoming_required = dependency_type == TaskDependencyType::Required;
            let replace = incoming_required && !current_required
                || incoming_required == current_required
                    && resolved_dependency_cmp(&resolved, current) == Ordering::Less;

            if replace {
                *current = resolved;
            } else if incoming_required {
                current.dependency_type = TaskDependencyType::Required;
            }
        } else {
            dependencies.push(resolved);
        }

        dependencies.sort_by(|a, b| a.task_key.cmp(&b.task_key));
    }

    /// Populate canonical metadata for already-resolved source-local dependencies.
    pub fn resolve_source_local_dependencies(&mut self) -> miette::Result<()> {
        let mut owners = self.nodes.keys().cloned().collect::<Vec<_>>();
        owners.sort();

        for owner in owners {
            let configs = self.nodes[&owner].task.deps.clone();

            for config in configs {
                let dependency =
                    TaskKey::from_target(owner.project_key().source_id().clone(), &config.target)?;

                if !self.nodes.contains_key(&dependency) {
                    continue;
                }

                let dependency_type = if config.optional.is_some_and(|optional| optional) {
                    TaskDependencyType::Optional
                } else {
                    TaskDependencyType::Required
                };
                self.add_resolved_dependency(&owner, &dependency, dependency_type, &config);
            }
        }

        Ok(())
    }

    /// Return canonical resolved dependency metadata for an owning task.
    pub fn resolved_dependencies_of(&self, key: &TaskKey) -> &[ResolvedTaskDependency] {
        self.resolved_dependencies
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Return a task with the provided primary-source target from the graph.
    /// If the task does not exist or has been misconfigured, return an error.
    #[instrument(name = "get_task", skip(self))]
    pub fn get(&self, target: &Target) -> miette::Result<Arc<Task>> {
        self.get_by_key(&TaskKey::from_target(
            self.context.sources.primary_id().clone(),
            target,
        )?)
    }

    /// Return a task with the provided canonical key from the graph.
    pub fn get_by_key(&self, key: &TaskKey) -> miette::Result<Arc<Task>> {
        self.internal_get(key)
    }

    /// Return an unexpanded task with the provided primary-source target.
    pub fn get_unexpanded(&self, target: &Target) -> miette::Result<&Task> {
        self.get_unexpanded_by_key(&TaskKey::from_target(
            self.context.sources.primary_id().clone(),
            target,
        )?)
    }

    /// Return an unexpanded task with the provided canonical key.
    pub fn get_unexpanded_by_key(&self, key: &TaskKey) -> miette::Result<&Task> {
        let node = self
            .nodes
            .get(key)
            .ok_or_else(|| ProjectError::UnknownTask {
                task_id: key.task_id().to_string(),
                project_id: key.project_key().project_id().to_string(),
            })?;

        Ok(&node.task)
    }

    /// Return all tasks from the graph.
    #[instrument(name = "get_all_tasks", skip(self))]
    pub fn get_all(&self) -> miette::Result<Vec<Arc<Task>>> {
        let mut all = vec![];

        for key in self.nodes.keys() {
            all.push(self.internal_get(key)?);
        }

        Ok(all)
    }

    /// Return all unexpanded tasks from the graph.
    pub fn get_all_unexpanded(&self) -> miette::Result<Vec<&Task>> {
        Ok(self.nodes.values().map(|node| &node.task).collect())
    }

    /// Return many tasks from the graph by primary-source target.
    #[instrument(name = "get_many_tasks", skip(self))]
    pub fn get_many(&self, targets: &[Target]) -> miette::Result<Vec<Arc<Task>>> {
        let keys = targets
            .iter()
            .map(|target| TaskKey::from_target(self.context.sources.primary_id().clone(), target))
            .collect::<miette::Result<Vec<_>>>()?;

        self.get_many_by_key(&keys)
    }

    /// Return many tasks from the graph by canonical key.
    pub fn get_many_by_key(&self, keys: &[TaskKey]) -> miette::Result<Vec<Arc<Task>>> {
        let mut many = vec![];

        for key in keys {
            many.push(self.internal_get(key)?);
        }

        Ok(many)
    }

    /// Return many unexpanded tasks from the graph by primary-source target.
    pub fn get_many_unexpanded(&self, targets: &[Target]) -> miette::Result<Vec<&Task>> {
        let keys = targets
            .iter()
            .map(|target| TaskKey::from_target(self.context.sources.primary_id().clone(), target))
            .collect::<miette::Result<Vec<_>>>()?;

        self.get_many_unexpanded_by_key(&keys)
    }

    /// Return many unexpanded tasks from the graph by canonical key.
    pub fn get_many_unexpanded_by_key(&self, keys: &[TaskKey]) -> miette::Result<Vec<&Task>> {
        let mut many = vec![];

        for key in keys {
            many.push(self.get_unexpanded_by_key(key)?);
        }

        Ok(many)
    }

    /// Return direct dependencies that belong to another source root.
    pub fn cross_source_dependencies_of(&self, key: &TaskKey) -> Vec<TaskKey> {
        let Some(node) = self.nodes.get(key) else {
            return vec![];
        };
        let source_id = key.project_key().source_id();
        let mut dependencies = self
            .graph
            .graph()
            .neighbors_directed(node.index, Direction::Outgoing)
            .filter_map(|index| {
                let dependency = self.indexes.get(&index)?;

                (dependency.project_key().source_id() != source_id).then(|| dependency.clone())
            })
            .collect::<Vec<_>>();
        dependencies.sort();

        dependencies
    }

    /// Return the first foreign-source dependency in the complete task closure.
    pub fn cross_source_dependency_in_closure(&self, key: &TaskKey) -> Option<TaskKey> {
        let source_id = key.project_key().source_id();
        let mut visited = rustc_hash::FxHashSet::default();
        let mut pending = VecDeque::from([key.clone()]);

        while let Some(current) = pending.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }

            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            let mut dependencies = self
                .graph
                .graph()
                .neighbors_directed(node.index, Direction::Outgoing)
                .filter_map(|index| self.indexes.get(&index).cloned())
                .collect::<Vec<_>>();
            dependencies.sort();

            for dependency in dependencies {
                if dependency.project_key().source_id() != source_id {
                    return Some(dependency);
                }

                pending.push_back(dependency);
            }
        }

        None
    }

    /// Focus the graph for a primary-source target.
    pub fn focus_for(&self, target: &Target, with_dependents: bool) -> miette::Result<Self> {
        self.focus_for_key(
            &TaskKey::from_target(self.context.sources.primary_id().clone(), target)?,
            with_dependents,
        )
    }

    /// Focus the graph for a canonical task key.
    pub fn focus_for_key(&self, key: &TaskKey, with_dependents: bool) -> miette::Result<Self> {
        let task = self.get_by_key(key)?;
        let graph = self.to_focused_graph(&task, with_dependents);
        let (nodes, edges) = graph.into_nodes_edges();

        let mut dag = Dag::with_capacity(nodes.len(), edges.len());
        let mut indexes = FxHashMap::default();
        let mut tasks = FxHashMap::default();

        // The focused graph has different node inndexes,
        // so we need to update our internal structures to match
        for (i, node) in nodes.into_iter().enumerate() {
            let new_index = NodeIndex::from(i as u32);
            let old_index = node.weight;
            let key = &self.indexes[&old_index];

            indexes.insert(new_index, key.to_owned());

            tasks.insert(
                key.to_owned(),
                TaskNode {
                    index: new_index,
                    task: self.get_node_by_index(&old_index).to_owned(),
                },
            );

            dag.add_node(new_index);
        }

        for edge in edges {
            dag.update_edge(edge.source(), edge.target(), edge.weight)
                .unwrap();
        }

        let resolved_dependencies = self
            .resolved_dependencies
            .iter()
            .filter(|(owner, _)| tasks.contains_key(*owner))
            .map(|(owner, dependencies)| {
                (
                    owner.clone(),
                    dependencies
                        .iter()
                        .filter(|dependency| tasks.contains_key(&dependency.task_key))
                        .cloned()
                        .collect(),
                )
            })
            .collect();

        Ok(Self {
            indexes,
            context: self.context.clone(),
            contexts: self.contexts.clone(),
            graph: dag,
            nodes: tasks,
            resolved_dependencies,
            project_graph: self.project_graph.clone(),
            tasks: self.tasks.clone(),
        })
    }

    fn internal_get(&self, key: &TaskKey) -> miette::Result<Arc<Task>> {
        let once = match self.tasks.entry_sync(key.to_owned()) {
            Entry::Occupied(e) => Arc::clone(e.get()),
            Entry::Vacant(e) => {
                let once = Arc::new(OnceCell::new());
                e.insert_entry(Arc::clone(&once));
                once
            }
        };

        once.get_or_try_init(|| {
            let expander = TaskExpander::new(
                &self.project_graph,
                self.project_graph
                    .get_unexpanded_by_key(key.project_key())?,
                self.contexts
                    .get(key.project_key().source_id())
                    .unwrap_or(&self.context),
                self,
            );
            Ok(Arc::new(expander.expand(self.get_unexpanded_by_key(key)?)?))
        })
        .map(Arc::clone)
    }
}

fn resolved_dependency_cmp(a: &ResolvedTaskDependency, b: &ResolvedTaskDependency) -> Ordering {
    let strategy_rank = |strategy| match strategy {
        TaskDependencyCacheStrategy::Hash => 0,
        TaskDependencyCacheStrategy::Ignored => 1,
        TaskDependencyCacheStrategy::Outputs => 2,
    };
    let mut a_env = a.env.iter().collect::<Vec<_>>();
    let mut b_env = b.env.iter().collect::<Vec<_>>();
    a_env.sort();
    b_env.sort();

    strategy_rank(a.cache_strategy)
        .cmp(&strategy_rank(b.cache_strategy))
        .then_with(|| a.args.cmp(&b.args))
        .then_with(|| a_env.cmp(&b_env))
}

impl TaskLookup for TaskGraph {
    fn get_task(&self, key: &TaskKey) -> miette::Result<Arc<Task>> {
        self.internal_get(key)
    }
}

impl GraphData<Task, TaskDependencyType, TaskKey> for TaskGraph {
    fn get_graph(&self) -> &DiGraph<NodeIndex, TaskDependencyType> {
        self.graph.graph()
    }

    fn get_nodes(&self) -> FxHashMap<NodeIndex, &Task> {
        self.nodes
            .values()
            .map(|node| (node.index, &node.task))
            .collect()
    }

    fn get_node_by_index(&self, index: &NodeIndex) -> &Task {
        &self.nodes[&self.indexes[index]].task
    }

    fn get_node_key(&self, node: &Task) -> TaskKey {
        node.key()
    }
}

impl GraphConnections<Task, TaskDependencyType, TaskKey> for TaskGraph {
    fn get_node_index(&self, node: &Task) -> NodeIndex {
        self.nodes[&node.key()].index
    }
}

impl GraphConversions<Task, TaskDependencyType, TaskKey> for TaskGraph {}

impl GraphToDot<Task, TaskDependencyType, TaskKey> for TaskGraph {}

impl GraphToJson<Task, TaskDependencyType, TaskKey> for TaskGraph {}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::{Id, SourceRegistry, SourceRootId};
    use moon_config::{TaskDependencyCacheStrategy, TaskDependencyConfig};
    use moon_project::Project;
    use moon_project_graph::ProjectNode;
    use moon_target::ProjectKey;

    fn context(source_id: SourceRootId, root: &str) -> GraphExpanderContext {
        GraphExpanderContext {
            sources: Arc::new(SourceRegistry::new(source_id, root.into())),
            working_dir: root.into(),
            workspace_root: root.into(),
            ..Default::default()
        }
    }

    fn projects(
        edges: &[(usize, usize, DependencyScope)],
    ) -> (Arc<ProjectGraph>, ProjectKey, ProjectKey) {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/primary".into());
        sources.register(child_id.clone(), "/child".into()).unwrap();
        let mut graph_context = context(primary_id.clone(), "/primary");
        graph_context.sources = Arc::new(sources);
        let mut projects = ProjectGraph::new(graph_context);
        let primary_key = ProjectKey::new(primary_id.clone(), Id::raw("app")).unwrap();
        let child_key = ProjectKey::new(child_id.clone(), Id::raw("lib")).unwrap();

        for (index, key) in [&primary_key, &child_key].into_iter().enumerate() {
            let index = NodeIndex::new(index);
            let project = Project {
                id: key.project_id().clone(),
                source_id: key.source_id().clone(),
                ..Project::default()
            };
            projects.indexes.insert(index, key.clone());
            projects
                .nodes
                .insert(key.clone(), ProjectNode { index, project });
        }

        let mut graph = DiGraph::new();
        graph.add_node(NodeIndex::new(0));
        graph.add_node(NodeIndex::new(1));
        for (source, target, scope) in edges {
            graph.add_edge(NodeIndex::new(*source), NodeIndex::new(*target), *scope);
        }
        projects.set_graph(graph).unwrap();

        (Arc::new(projects), primary_key, child_key)
    }

    fn tasks(
        context: GraphExpanderContext,
        project_graph: Arc<ProjectGraph>,
        project_key: &ProjectKey,
        configs: &[(&str, Vec<TaskDependencyConfig>)],
    ) -> Arc<TaskGraph> {
        let mut graph = TaskGraph::new(context, project_graph);

        for (index, (task_id, configured_deps)) in configs.iter().enumerate() {
            let target = moon_target::Target::new(project_key.project_id(), task_id).unwrap();
            let task = Task {
                configured_deps: configured_deps.clone(),
                id: Id::raw(task_id),
                source_id: project_key.source_id().clone(),
                target,
                ..Task::default()
            };
            let key = task.key();
            let index = graph.graph.add_node(NodeIndex::new(index));
            graph.indexes.insert(index, key.clone());
            graph.nodes.insert(key, TaskNode { index, task });
        }

        Arc::new(graph)
    }

    #[test]
    fn composes_cross_source_dependency_selectors_with_exact_scopes() {
        let (projects, primary_project, child_project) =
            projects(&[(0, 1, DependencyScope::Build)]);
        let primary = tasks(
            context(primary_project.source_id().clone(), "/primary"),
            Arc::clone(&projects),
            &primary_project,
            &[(
                "build",
                vec![
                    TaskDependencyConfig::new(moon_target::Target::new("^build", "build").unwrap()),
                    TaskDependencyConfig::new(
                        moon_target::Target::new("^production", "test").unwrap(),
                    ),
                ],
            )],
        );
        let child = tasks(
            context(child_project.source_id().clone(), "/child"),
            Arc::clone(&projects),
            &child_project,
            &[("build", vec![]), ("test", vec![])],
        );
        let graph = TaskGraph::compose(projects, [primary, child]).unwrap();
        let source_key = TaskKey::new(primary_project, Id::raw("build")).unwrap();
        let dependencies = graph.dependencies_of(graph.get_unexpanded_by_key(&source_key).unwrap());

        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].to_string(), "child::lib:build");
        assert_eq!(
            graph.cross_source_dependencies_of(&source_key)[0].to_string(),
            "child::lib:build"
        );
        let resolved = graph.resolved_dependencies_of(&source_key);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].task_key.to_string(), "child::lib:build");
        assert_eq!(
            resolved[0].cache_strategy,
            TaskDependencyCacheStrategy::Ignored
        );
        assert_eq!(graph.contexts.len(), 2);
        assert_eq!(
            graph.contexts[&SourceRootId::new("child").unwrap()].workspace_root,
            std::path::PathBuf::from("/child")
        );
        assert_eq!(
            graph
                .focus_for_key(&source_key, false)
                .unwrap()
                .contexts
                .len(),
            2
        );
    }

    #[test]
    fn rejects_cross_source_task_cycles_with_qualified_keys() {
        let (projects, primary_project, child_project) = projects(&[
            (0, 1, DependencyScope::Build),
            (1, 0, DependencyScope::Production),
        ]);
        let dep = || {
            vec![TaskDependencyConfig::new(
                moon_target::Target::new("^", "build").unwrap(),
            )]
        };
        let primary = tasks(
            context(primary_project.source_id().clone(), "/primary"),
            Arc::clone(&projects),
            &primary_project,
            &[("build", dep())],
        );
        let child = tasks(
            context(child_project.source_id().clone(), "/child"),
            Arc::clone(&projects),
            &child_project,
            &[("build", dep())],
        );
        let error = TaskGraph::compose(projects, [primary, child])
            .unwrap_err()
            .to_string();

        assert!(error.contains("primary::app:build"));
        assert!(error.contains("child::lib:build"));
    }

    #[test]
    fn detects_cross_source_dependencies_in_transitive_closures() {
        let (projects, primary_project, child_project) =
            projects(&[(0, 1, DependencyScope::Build)]);
        let mut primary = tasks(
            context(primary_project.source_id().clone(), "/primary"),
            Arc::clone(&projects),
            &primary_project,
            &[
                ("root", vec![]),
                (
                    "middle",
                    vec![TaskDependencyConfig::new(
                        moon_target::Target::new("^", "build").unwrap(),
                    )],
                ),
            ],
        );
        let root_key = TaskKey::new(primary_project.clone(), Id::raw("root")).unwrap();
        let middle_key = TaskKey::new(primary_project, Id::raw("middle")).unwrap();
        Arc::get_mut(&mut primary)
            .unwrap()
            .add_edge(&root_key, &middle_key, TaskDependencyType::Required)
            .unwrap();
        let child = tasks(
            context(child_project.source_id().clone(), "/child"),
            Arc::clone(&projects),
            &child_project,
            &[("build", vec![])],
        );
        let graph = TaskGraph::compose(projects, [primary, child]).unwrap();

        assert!(graph.cross_source_dependencies_of(&root_key).is_empty());
        assert_eq!(
            graph
                .cross_source_dependency_in_closure(&root_key)
                .unwrap()
                .to_string(),
            "child::lib:build"
        );
    }

    #[test]
    fn defaults_cross_source_strategy_from_outputs_and_preserves_focused_metadata() {
        let (projects, primary_project, child_project) =
            projects(&[(0, 1, DependencyScope::Build)]);
        let primary = tasks(
            context(primary_project.source_id().clone(), "/primary"),
            Arc::clone(&projects),
            &primary_project,
            &[(
                "build",
                vec![TaskDependencyConfig::new(
                    moon_target::Target::new("^", "build").unwrap(),
                )],
            )],
        );
        let mut child = tasks(
            context(child_project.source_id().clone(), "/child"),
            Arc::clone(&projects),
            &child_project,
            &[("build", vec![])],
        );
        Arc::get_mut(&mut child)
            .unwrap()
            .nodes
            .values_mut()
            .next()
            .unwrap()
            .task
            .output_files
            .insert("dist/file.js".into(), Default::default());
        let graph = TaskGraph::compose(projects, [primary, child]).unwrap();
        let source_key = TaskKey::new(primary_project, Id::raw("build")).unwrap();
        let focused = graph.focus_for_key(&source_key, false).unwrap();
        let resolved = focused.resolved_dependencies_of(&source_key);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].task_key.to_string(), "child::lib:build");
        assert_eq!(
            resolved[0].cache_strategy,
            TaskDependencyCacheStrategy::Hash
        );
    }

    #[test]
    fn deterministically_merges_duplicate_configs_with_required_dominating() {
        let make_graph = |reverse: bool| {
            let (projects, primary_project, child_project) =
                projects(&[(0, 1, DependencyScope::Build)]);
            let target = moon_target::Target::new("^", "build").unwrap();
            let mut configs = vec![
                TaskDependencyConfig {
                    optional: Some(true),
                    cache_strategy: Some(TaskDependencyCacheStrategy::Outputs),
                    ..TaskDependencyConfig::new(target.clone())
                },
                TaskDependencyConfig {
                    optional: Some(false),
                    cache_strategy: Some(TaskDependencyCacheStrategy::Ignored),
                    ..TaskDependencyConfig::new(target)
                },
            ];
            if reverse {
                configs.reverse();
            }
            let primary = tasks(
                context(primary_project.source_id().clone(), "/primary"),
                Arc::clone(&projects),
                &primary_project,
                &[("build", configs)],
            );
            let child = tasks(
                context(child_project.source_id().clone(), "/child"),
                Arc::clone(&projects),
                &child_project,
                &[("build", vec![])],
            );
            let graph = TaskGraph::compose(projects, [primary, child]).unwrap();
            let owner = TaskKey::new(primary_project, Id::raw("build")).unwrap();

            graph.resolved_dependencies_of(&owner).to_vec()
        };

        let forward = make_graph(false);
        let reverse = make_graph(true);

        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), 1);
        assert_eq!(forward[0].dependency_type, TaskDependencyType::Required);
        assert_eq!(
            forward[0].cache_strategy,
            TaskDependencyCacheStrategy::Ignored
        );
    }

    #[test]
    fn resolves_source_local_dependency_metadata() {
        let (projects, primary_project, _) = projects(&[]);
        let dep_target = moon_target::Target::new("app", "build").unwrap();
        let mut config = TaskDependencyConfig {
            args: vec!["--release".into()],
            cache_strategy: Some(TaskDependencyCacheStrategy::Outputs),
            optional: Some(true),
            ..TaskDependencyConfig::new(dep_target)
        };
        config.env.insert("MODE".into(), Some("ci".into()));
        let mut graph = tasks(
            context(primary_project.source_id().clone(), "/primary"),
            projects,
            &primary_project,
            &[("consume", vec![]), ("build", vec![])],
        );
        let graph = Arc::get_mut(&mut graph).unwrap();
        let owner = TaskKey::new(primary_project, Id::raw("consume")).unwrap();
        graph.nodes.get_mut(&owner).unwrap().task.deps.push(config);
        graph.resolve_source_local_dependencies().unwrap();
        let resolved = graph.resolved_dependencies_of(&owner);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].task_key.to_string(), "primary::app:build");
        assert_eq!(
            resolved[0].cache_strategy,
            TaskDependencyCacheStrategy::Outputs
        );
        assert_eq!(resolved[0].dependency_type, TaskDependencyType::Optional);
        assert_eq!(resolved[0].args, ["--release"]);
        assert_eq!(resolved[0].env["MODE"], Some("ci".into()));
    }
}
