use crate::project_graph_error::ProjectGraphError;
use daggy::Dag;
use miette::IntoDiagnostic;
use moon_common::path::{PathExt, WorkspaceRelativePathBuf};
use moon_common::{Id, SourceAlias, SourcePathBuf, SourceRootId};
use moon_config::{DependencyScope, ProjectDependencyConfig};
use moon_graph_utils::*;
use moon_project::Project;
use moon_project_constraints::{enforce_layer_relationships, enforce_tag_relationships};
use moon_project_expander::{ProjectExpander, ProjectExpanderContext};
use moon_target::ProjectKey;
use petgraph::Direction;
use petgraph::algo::{has_path_connecting, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::{EdgeFiltered, EdgeRef};
use rustc_hash::{FxHashMap, FxHashSet};
use scc::hash_map::Entry;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, instrument};

/// The internal graph partition that a dependency scope belongs to. Each
/// partition is individually acyclic, while their union may contain cycles
/// that cross the partition boundary. Anything that requires an acyclic
/// graph (ordering, unguarded recursion) should operate on a partition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopePartition {
    /// Production and peer scoped dependencies.
    Production,

    /// Development, build, and root scoped dependencies.
    Development,
}

impl ScopePartition {
    /// Return the partition that the provided scope belongs to.
    pub fn of(scope: &DependencyScope) -> ScopePartition {
        if scope.is_production_group() {
            ScopePartition::Production
        } else {
            ScopePartition::Development
        }
    }
}

/// Return true if adding an edge from source to target with the provided scope
/// would introduce a cycle within the partition that the scope belongs to.
/// Edges belonging to the other partition are ignored, so cycles that cross
/// the production/development boundary are allowed.
pub fn would_cycle_in_scope<N>(
    graph: &DiGraph<N, DependencyScope>,
    source: NodeIndex,
    target: NodeIndex,
    scope: &DependencyScope,
) -> bool {
    let partition = scope.is_production_group();
    let partitioned_graph = EdgeFiltered::from_fn(graph, |edge| {
        edge.weight().is_production_group() == partition
    });

    has_path_connecting(&partitioned_graph, target, source, None)
}

#[derive(Clone, Debug)]
pub struct ProjectNode {
    pub index: NodeIndex,
    pub project: Project,
}

#[derive(Debug, Default)]
pub struct ProjectGraph {
    pub context: GraphExpanderContext,

    /// Expansion contexts indexed by the source that owns each project.
    contexts: FxHashMap<SourceRootId, GraphExpanderContext>,

    /// Map of source-qualified aliases to canonical project keys.
    pub aliases: FxHashMap<(SourceRootId, String), ProjectKey>,

    /// Canonical key of the default project.
    pub default_key: Option<ProjectKey>,

    /// Union graph of projects (by index) and their dependencies across every
    /// scope. Powers all read APIs. May contain cycles that cross the
    /// production/development boundary; each partitioned graph below is
    /// individually acyclic. Populate with [`ProjectGraph::set_graph`].
    graph: DiGraph<NodeIndex, DependencyScope>,

    /// Directed-acyclic graph (DAG) of production and peer dependencies.
    production_graph: Dag<NodeIndex, DependencyScope>,

    /// Directed-acyclic graph (DAG) of development, build, and root dependencies.
    development_graph: Dag<NodeIndex, DependencyScope>,

    /// Map of node indexes to canonical project keys.
    pub indexes: FxHashMap<NodeIndex, ProjectKey>,

    /// Map of project nodes by canonical key.
    pub nodes: FxHashMap<ProjectKey, ProjectNode>,

    /// Cache of file path lookups, mapped by starting path to canonical project key.
    fs_cache: Arc<scc::HashMap<SourcePathBuf, Arc<ProjectKey>>>,

    /// Map of expanded projects by canonical key.
    projects: Arc<scc::HashMap<ProjectKey, Arc<Project>>>,
}

impl ProjectGraph {
    pub fn new(context: GraphExpanderContext) -> Self {
        debug!("Creating project graph");

        let mut contexts = FxHashMap::default();
        contexts.insert(context.sources.primary_id().clone(), context.clone());

        Self {
            context,
            contexts,
            ..Default::default()
        }
    }

    /// Compose finalized source-local project graphs into a read-only aggregate graph.
    pub fn compose(
        sources: Arc<moon_common::SourceRegistry>,
        source_aliases: &FxHashMap<SourceAlias, SourceRootId>,
        graphs: impl IntoIterator<Item = Arc<ProjectGraph>>,
    ) -> miette::Result<Self> {
        let mut graphs = graphs.into_iter().collect::<Vec<_>>();
        graphs.sort_by(|a, b| {
            a.context
                .sources
                .primary_id()
                .cmp(b.context.sources.primary_id())
        });

        let primary_id = sources.primary_id().clone();
        let primary = graphs
            .iter()
            .find(|graph| graph.context.sources.primary_id() == &primary_id)
            .ok_or_else(|| ProjectGraphError::UnconfiguredID {
                id: primary_id.to_string(),
            })?;
        let mut context = primary.context.clone();
        context.sources = Arc::clone(&sources);
        context.workspace_root = sources.get_primary().to_path_buf();

        let mut aggregate = Self::new(context);
        aggregate.contexts.clear();
        aggregate.default_key = primary.default_key.clone();

        let mut graph = DiGraph::new();
        let mut indexes_by_key = FxHashMap::default();
        let mut local_edges = vec![];
        let mut cross_dependencies = vec![];

        for local in graphs {
            let source_id = local.context.sources.primary_id().clone();
            aggregate
                .contexts
                .insert(source_id.clone(), local.context.clone());

            let mut keys = local.nodes.keys().cloned().collect::<Vec<_>>();
            keys.sort();

            for key in keys {
                let mut project = local.nodes[&key].project.clone();
                cross_dependencies.extend(
                    std::mem::take(&mut project.cross_source_dependencies)
                        .into_iter()
                        .map(|dependency| (key.clone(), dependency)),
                );
                let index = graph.add_node(NodeIndex::new(graph.node_count()));

                aggregate.indexes.insert(index, key.clone());
                aggregate
                    .nodes
                    .insert(key.clone(), ProjectNode { index, project });
                indexes_by_key.insert(key, index);
            }

            aggregate.aliases.extend(
                local
                    .aliases
                    .iter()
                    .filter(|((alias_source, _), key)| {
                        alias_source == &source_id && key.source_id() == &source_id
                    })
                    .map(|(alias, key)| (alias.clone(), key.clone())),
            );

            for edge in local.graph.edge_references() {
                let source_key = &local.indexes[&edge.source()];
                let target_key = &local.indexes[&edge.target()];

                if source_key.source_id() != target_key.source_id() {
                    return Err(ProjectGraphError::UnsupportedCrossSourceEdge {
                        source_key: source_key.to_string(),
                        target_key: target_key.to_string(),
                    }
                    .into());
                }

                local_edges.push((source_key.clone(), target_key.clone(), *edge.weight()));
            }
        }

        local_edges.sort();

        for (source_key, target_key, scope) in local_edges {
            graph.add_edge(
                indexes_by_key[&source_key],
                indexes_by_key[&target_key],
                scope,
            );
        }

        let mut resolved_dependencies = vec![];

        for (order, (owner_key, mut dependency)) in cross_dependencies.into_iter().enumerate() {
            let requested_source = dependency
                .source_root
                .clone()
                .expect("Cross-source dependencies must declare a source root.");
            let selected_source = if owner_key.source_id() == sources.primary_id() {
                source_aliases
                    .iter()
                    .find_map(|(alias, source_id)| {
                        (alias.as_str() == requested_source.as_str()).then(|| source_id.clone())
                    })
                    .unwrap_or_else(|| requested_source.clone())
            } else {
                requested_source.clone()
            };
            let selected_source = if selected_source == SourceRootId::primary() {
                sources.primary_id().clone()
            } else {
                selected_source
            };

            if sources.get(&selected_source).is_err() {
                return Err(ProjectGraphError::UnknownDependencySource {
                    project_key: owner_key.to_string(),
                    source_id: requested_source.to_string(),
                }
                .into());
            }

            if &selected_source == owner_key.source_id() {
                return Err(ProjectGraphError::RedundantDependencySource {
                    project_key: owner_key.to_string(),
                    source_id: selected_source.to_string(),
                }
                .into());
            }

            let target_key = ProjectKey::new(selected_source.clone(), dependency.id.clone())?;
            let target_key = if aggregate.nodes.contains_key(&target_key) {
                target_key
            } else if let Some(alias_key) = aggregate
                .aliases
                .get(&(selected_source.clone(), dependency.id.to_string()))
            {
                alias_key.clone()
            } else {
                return Err(ProjectGraphError::UnknownCrossSourceTarget {
                    project_key: owner_key.to_string(),
                    source_id: selected_source.to_string(),
                    target_id: dependency.id.to_string(),
                }
                .into());
            };

            dependency.source_root = Some(selected_source);
            dependency.id = target_key.project_id().clone();
            resolved_dependencies.push((owner_key, target_key, dependency, order));
        }

        // Source-local dependency construction is last-wins. Apply the same rule
        // after canonical resolution, then sort so graph output is input-order independent.
        resolved_dependencies.sort_by(|a, b| (&a.0, &a.1, a.3).cmp(&(&b.0, &b.1, b.3)));
        let mut deduplicated_dependencies: Vec<(
            ProjectKey,
            ProjectKey,
            ProjectDependencyConfig,
            usize,
        )> = vec![];

        for dependency in resolved_dependencies {
            if let Some(previous) = deduplicated_dependencies.last_mut()
                && previous.0 == dependency.0
                && previous.1 == dependency.1
            {
                *previous = dependency;
            } else {
                deduplicated_dependencies.push(dependency);
            }
        }

        deduplicated_dependencies.sort_by(|a, b| (&a.0, a.3).cmp(&(&b.0, b.3)));

        for (owner_key, target_key, dependency, _) in deduplicated_dependencies {
            if !dependency.is_root_scope() {
                let owner = &aggregate.nodes[&owner_key].project;
                let target = &aggregate.nodes[&target_key].project;
                let constraints = &aggregate.contexts[owner_key.source_id()]
                    .workspace_config
                    .constraints;

                if constraints.enforce_layer_relationships {
                    enforce_layer_relationships(owner, target, &dependency.scope)?;
                }

                for (source_tag, required_tags) in &constraints.tag_relationships {
                    enforce_tag_relationships(owner, source_tag, target, required_tags)?;
                }
            }

            aggregate
                .nodes
                .get_mut(&owner_key)
                .unwrap()
                .project
                .dependencies
                .push(dependency.clone());

            if !dependency.is_root_scope() {
                graph.add_edge(
                    indexes_by_key[&owner_key],
                    indexes_by_key[&target_key],
                    dependency.scope,
                );
            }
        }

        aggregate.set_graph(graph)?;

        Ok(aggregate)
    }

    /// Return a map of aliases to their project IDs. Projects without aliases are omitted.
    pub fn aliases(&self) -> FxHashMap<&str, &Id> {
        self.aliases_for_source(self.context.sources.primary_id())
            .into_iter()
            .map(|(alias, key)| (alias, key.project_id()))
            .collect()
    }

    /// Return source-local aliases mapped to canonical project keys.
    pub fn aliases_for_source(&self, source_id: &SourceRootId) -> FxHashMap<&str, &ProjectKey> {
        let source_id = self.canonical_source_id(source_id);

        self.aliases
            .iter()
            .filter_map(|((alias_source, alias), key)| {
                (alias_source == source_id).then_some((alias.as_str(), key))
            })
            .collect()
    }

    /// Return a project with the provided ID or alias from the graph.
    /// If the project does not exist or has been misconfigured, return an error.
    #[instrument(name = "get_project", skip(self))]
    pub fn get(&self, id_or_alias: &str) -> miette::Result<Arc<Project>> {
        let key = self.resolve_key(self.context.sources.primary_id(), id_or_alias)?;

        if !self.nodes.contains_key(&key) {
            return Err(ProjectGraphError::UnconfiguredID {
                id: key.project_id().to_string(),
            }
            .into());
        }

        self.internal_get(&key)
    }

    /// Return a project by its canonical source-qualified identity.
    pub fn get_by_key(&self, key: &ProjectKey) -> miette::Result<Arc<Project>> {
        let key = self.normalize_key(key)?;
        self.internal_get(&key)
    }

    /// Return an unexpanded project with the provided ID or alias from the graph.
    pub fn get_unexpanded(&self, id_or_alias: &str) -> miette::Result<&Project> {
        let key = self.resolve_key(self.context.sources.primary_id(), id_or_alias)?;
        let node = self
            .nodes
            .get(&key)
            .ok_or_else(|| ProjectGraphError::UnconfiguredID {
                id: key.project_id().to_string(),
            })?;

        Ok(&node.project)
    }

    /// Return an unexpanded project by its canonical source-qualified identity.
    pub fn get_unexpanded_by_key(&self, key: &ProjectKey) -> miette::Result<&Project> {
        let key = self.normalize_key(key)?;
        let node = self
            .nodes
            .get(&key)
            .ok_or_else(|| ProjectGraphError::UnconfiguredID {
                id: key.to_string(),
            })?;

        Ok(&node.project)
    }

    /// Return all projects from the graph.
    #[instrument(name = "get_all_projects", skip(self))]
    pub fn get_all(&self) -> miette::Result<Vec<Arc<Project>>> {
        let mut all = vec![];

        for key in self.nodes.keys() {
            all.push(self.internal_get(key)?);
        }

        Ok(all)
    }

    /// Return all unexpanded projects from the graph.
    pub fn get_all_unexpanded(&self) -> Vec<&Project> {
        self.nodes.values().map(|node| &node.project).collect()
    }

    /// Return the default project if it has been configured and exists.
    pub fn get_default(&self) -> miette::Result<Arc<Project>> {
        if let Some(key) = &self.default_key {
            return self.get(key.project_id());
        }

        Err(ProjectGraphError::NoDefaultProject.into())
    }

    /// Return the canonical default project if it has been configured and exists.
    pub fn get_default_by_key(&self) -> miette::Result<Arc<Project>> {
        if let Some(key) = &self.default_key {
            return self.get_by_key(key);
        }

        Err(ProjectGraphError::NoDefaultProject.into())
    }

    /// Find and return a project based on the initial path location.
    /// This will attempt to find the closest matching project source.
    #[instrument(name = "get_project_from_path", skip(self))]
    pub fn get_from_path(&self, starting_file: Option<&Path>) -> miette::Result<Arc<Project>> {
        let current_file = starting_file.unwrap_or(&self.context.working_dir);

        let source_path = if current_file.is_absolute() {
            self.context.sources.qualify(current_file)?
        } else {
            SourcePathBuf::new(
                self.context.sources.primary_id().clone(),
                WorkspaceRelativePathBuf::from_path(current_file).into_diagnostic()?,
            )
        };

        self.get_from_source_path(&source_path)
    }

    /// Find a project from a canonical source-qualified path.
    pub fn get_from_source_path(
        &self,
        source_path: &SourcePathBuf,
    ) -> miette::Result<Arc<Project>> {
        let key = self.internal_search(source_path)?;

        self.get_by_key(&key)
    }

    /// Return a map of project IDs to their file source paths.
    pub fn sources(&self) -> FxHashMap<&Id, &WorkspaceRelativePathBuf> {
        let primary = self.context.sources.primary_id();

        self.nodes
            .iter()
            .filter_map(|(key, node)| {
                (key.source_id() == primary).then_some((key.project_id(), &node.project.source))
            })
            .collect()
    }

    /// Return a map of canonical project keys to their file source paths.
    pub fn sources_by_key(&self) -> FxHashMap<&ProjectKey, &WorkspaceRelativePathBuf> {
        self.nodes
            .iter()
            .map(|(key, node)| (key, &node.project.source))
            .collect()
    }

    /// Return the graph of production and peer dependencies.
    pub fn production_graph(&self) -> &DiGraph<NodeIndex, DependencyScope> {
        self.partitioned_graph(ScopePartition::Production)
    }

    /// Return the graph of development, build, and root dependencies.
    pub fn development_graph(&self) -> &DiGraph<NodeIndex, DependencyScope> {
        self.partitioned_graph(ScopePartition::Development)
    }

    /// Return the graph of dependencies for the provided partition. Unlike
    /// the unioned graph, partitioned graphs are guaranteed to be acyclic.
    pub fn partitioned_graph(
        &self,
        partition: ScopePartition,
    ) -> &DiGraph<NodeIndex, DependencyScope> {
        match partition {
            ScopePartition::Production => self.production_graph.graph(),
            ScopePartition::Development => self.development_graph.graph(),
        }
    }

    /// Return a list of direct project IDs that the provided project depends on,
    /// only traversing edges within the provided partition.
    pub fn partitioned_dependencies_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<Id> {
        self.partitioned_dependency_keys_of(project, partition)
            .into_iter()
            .map(|key| key.project_id().clone())
            .collect()
    }

    /// Return canonical keys for direct dependencies in the provided partition.
    pub fn partitioned_dependency_keys_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<ProjectKey> {
        self.partitioned_neighbors_of(project, partition, Direction::Outgoing)
    }

    /// Return a list of direct project IDs that depend on the provided project,
    /// only traversing edges within the provided partition.
    pub fn partitioned_dependents_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<Id> {
        self.partitioned_dependent_keys_of(project, partition)
            .into_iter()
            .map(|key| key.project_id().clone())
            .collect()
    }

    /// Return canonical keys for direct dependents in the provided partition.
    pub fn partitioned_dependent_keys_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<ProjectKey> {
        self.partitioned_neighbors_of(project, partition, Direction::Incoming)
    }

    /// Return a list of all project IDs that the provided project depends on,
    /// only traversing edges within the provided partition.
    pub fn partitioned_deep_dependencies_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<Id> {
        self.partitioned_deep_dependency_keys_of(project, partition)
            .into_iter()
            .map(|key| key.project_id().clone())
            .collect()
    }

    /// Return canonical keys for all dependencies in the provided partition.
    pub fn partitioned_deep_dependency_keys_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<ProjectKey> {
        self.partitioned_traverse(project, partition, Direction::Outgoing)
    }

    /// Return a list of all project IDs that depend on the provided project,
    /// only traversing edges within the provided partition.
    pub fn partitioned_deep_dependents_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<Id> {
        self.partitioned_deep_dependent_keys_of(project, partition)
            .into_iter()
            .map(|key| key.project_id().clone())
            .collect()
    }

    /// Return canonical keys for all dependents in the provided partition.
    pub fn partitioned_deep_dependent_keys_of(
        &self,
        project: &Project,
        partition: ScopePartition,
    ) -> Vec<ProjectKey> {
        self.partitioned_traverse(project, partition, Direction::Incoming)
    }

    /// Return all project IDs sorted topologically in dependency-first order
    /// (dependencies before the projects that depend on them), using only
    /// edges within the provided partition. Projects without edges in the
    /// partition are included. Sorting is only possible for partitioned
    /// graphs, as the unioned graph may contain cycles across partitions.
    pub fn partitioned_toposort(&self, partition: ScopePartition) -> Vec<Id> {
        self.partitioned_toposort_keys(partition)
            .into_iter()
            .map(|key| key.project_id().clone())
            .collect()
    }

    /// Return all canonical project keys in dependency-first topological order.
    pub fn partitioned_toposort_keys(&self, partition: ScopePartition) -> Vec<ProjectKey> {
        let mut indices = toposort(self.partitioned_graph(partition), None)
            .expect("Partitioned graphs are always acyclic!");

        // Edges point from a project to its dependency,
        // so reverse the order to get dependencies first
        indices.reverse();

        indices
            .into_iter()
            .map(|index| self.indexes[&index].clone())
            .collect()
    }

    fn partitioned_neighbors_of(
        &self,
        project: &Project,
        partition: ScopePartition,
        direction: Direction,
    ) -> Vec<ProjectKey> {
        self.partitioned_graph(partition)
            .neighbors_directed(self.nodes[&project.key()].index, direction)
            .map(|index| self.indexes[&index].clone())
            .collect()
    }

    fn partitioned_traverse(
        &self,
        project: &Project,
        partition: ScopePartition,
        direction: Direction,
    ) -> Vec<ProjectKey> {
        let graph = self.partitioned_graph(partition);
        let start = self.nodes[&project.key()].index;
        let mut visited = FxHashSet::from_iter([start]);
        let mut queue = VecDeque::from([start]);
        let mut results = vec![];

        while let Some(index) = queue.pop_front() {
            for next_index in graph.neighbors_directed(index, direction) {
                if visited.insert(next_index) {
                    results.push(self.indexes[&next_index].clone());
                    queue.push_back(next_index);
                }
            }
        }

        results
    }

    /// Set the union graph of all dependency edges, and derive the production
    /// (production/peer) and development (development/build/root) graphs from it,
    /// with each partition enforcing acyclicity for its own edges. Cycles within
    /// a single partition return an error, while cycles that cross the partition
    /// boundary are allowed. The `indexes` map should be populated beforehand,
    /// so that cycle errors can report project IDs.
    pub fn set_graph(
        &mut self,
        mut graph: DiGraph<NodeIndex, DependencyScope>,
    ) -> miette::Result<()> {
        let mut production_graph = Dag::with_capacity(graph.node_count(), graph.edge_count());
        let mut development_graph = Dag::with_capacity(graph.node_count(), graph.edge_count());

        // Weight-based lookups require each node's weight to be its own
        // index, but the builders may provide stale pre-filtered indices
        // when placeholder nodes were dropped, so rewrite them
        for index in 0..graph.node_count() {
            let index = NodeIndex::new(index);
            graph[index] = index;
        }

        // Mirror the nodes into both graphs, in the same order,
        // so that all node indexes align
        for index in graph.node_indices() {
            production_graph.add_node(graph[index]);
            development_graph.add_node(graph[index]);
        }

        // Then route each edge into the graph its scope belongs to,
        // relying on daggy's insertion checks to detect cycles
        for edge in graph.edge_references() {
            let scope = *edge.weight();

            let partitioned_graph = if scope.is_production_group() {
                &mut production_graph
            } else {
                &mut development_graph
            };

            partitioned_graph
                .add_edge(edge.source(), edge.target(), scope)
                .map_err(|_| ProjectGraphError::WouldCycle {
                    source_id: self.label_index(edge.source()),
                    target_id: self.label_index(edge.target()),
                })?;
        }

        self.graph = graph;
        self.production_graph = production_graph;
        self.development_graph = development_graph;

        Ok(())
    }

    /// Focus the graph for a specific primary-source project by ID.
    pub fn focus_for(&self, id_or_alias: &Id, with_dependents: bool) -> miette::Result<Self> {
        let project = self.get(id_or_alias)?;
        self.focus_for_key(&project.key(), with_dependents)
    }

    /// Focus the graph for a project by its canonical key.
    pub fn focus_for_key(&self, key: &ProjectKey, with_dependents: bool) -> miette::Result<Self> {
        let project = self.get_by_key(key)?;
        let focused_graph = self.to_focused_graph(&project, with_dependents);
        let (nodes, edges) = focused_graph.into_nodes_edges();

        let mut graph = DiGraph::with_capacity(nodes.len(), edges.len());
        let mut indexes = FxHashMap::default();
        let mut projects = FxHashMap::default();

        // The focused graph has different node inndexes,
        // so we need to update our internal structures to match
        for (i, node) in nodes.into_iter().enumerate() {
            let new_index = NodeIndex::from(i as u32);
            let old_index = node.weight;
            let key = &self.indexes[&old_index];

            indexes.insert(new_index, key.to_owned());

            projects.insert(
                key.to_owned(),
                ProjectNode {
                    index: new_index,
                    project: self.get_node_by_index(&old_index).to_owned(),
                },
            );

            graph.add_node(new_index);
        }

        for edge in edges {
            graph.update_edge(edge.source(), edge.target(), edge.weight);
        }

        let aliases = self
            .aliases
            .iter()
            .filter_map(|(alias, key)| {
                if projects.contains_key(key) {
                    Some((alias.to_owned(), key.to_owned()))
                } else {
                    None
                }
            })
            .collect();

        let mut focused = Self {
            aliases,
            context: self.context.clone(),
            contexts: self.contexts.clone(),
            indexes,
            default_key: self.default_key.clone(),
            fs_cache: Arc::clone(&self.fs_cache),
            nodes: projects,
            projects: Arc::clone(&self.projects),
            ..Default::default()
        };

        // The focused edges are a subset of already validated partitions,
        // so deriving them again cannot fail
        focused.set_graph(graph)?;

        Ok(focused)
    }

    fn label_index(&self, index: NodeIndex) -> String {
        self.indexes
            .get(&index)
            .map(|id| id.to_string())
            .unwrap_or_else(|| index.index().to_string())
    }

    fn internal_get(&self, key: &ProjectKey) -> miette::Result<Arc<Project>> {
        let project = match self.projects.entry_sync(key.clone()) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let context = self
                    .contexts
                    .get(entry.key().source_id())
                    .unwrap_or(&self.context);
                let expander = ProjectExpander::new(ProjectExpanderContext {
                    aliases: self.aliases_for_source(entry.key().source_id()),
                    source_id: entry.key().source_id(),
                    workspace_root: &context.workspace_root,
                });

                let project = Arc::new(expander.expand(self.get_unexpanded_by_key(entry.key())?)?);

                entry.insert_entry(Arc::clone(&project));

                project
            }
        };

        Ok(project)
    }

    fn internal_search(&self, search: &SourcePathBuf) -> miette::Result<Arc<ProjectKey>> {
        let source_id = self.canonical_source_id(&search.source);
        let search_path = if search.path.as_str().is_empty() {
            "."
        } else {
            search.path.as_str()
        };

        let cache = match self
            .fs_cache
            .entry_sync(SourcePathBuf::new(source_id.clone(), search.path.clone()))
        {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                // Find the deepest matching path in case sub-projects are being used
                let mut remaining_length = 1000; // Start with a really fake number
                let mut possible_key = None;

                for (key, node) in &self.nodes {
                    if key.source_id() != source_id
                        || !Path::new(search_path).starts_with(node.project.source.as_str())
                    {
                        continue;
                    }

                    if let Ok(diff) =
                        Path::new(search_path).relative_to(node.project.source.as_str())
                    {
                        let diff_comps = diff.components().count();

                        // Exact match, abort
                        if diff_comps == 0 {
                            possible_key = Some(key.clone());
                            break;
                        }

                        if diff_comps < remaining_length {
                            remaining_length = diff_comps;
                            possible_key = Some(key.clone());
                        }
                    }
                }

                let Some(possible_key) = possible_key else {
                    return Err(ProjectGraphError::MissingFromPath {
                        dir: PathBuf::from(search_path),
                    }
                    .into());
                };

                let key = Arc::new(possible_key);

                entry.insert_entry(Arc::clone(&key));

                key
            }
        };

        Ok(cache)
    }

    pub fn resolve_id(&self, id_or_alias: &str) -> Id {
        self.resolve_key(self.context.sources.primary_id(), id_or_alias)
            .map(|key| key.project_id().clone())
            .unwrap_or_else(|_| Id::raw(id_or_alias))
    }

    /// Resolve an ID or alias within a source to its canonical key.
    pub fn resolve_key(
        &self,
        source_id: &SourceRootId,
        id_or_alias: &str,
    ) -> miette::Result<ProjectKey> {
        let source_id = self.canonical_source_id(source_id);
        let candidate = ProjectKey::new(source_id.clone(), Id::raw(id_or_alias))?;

        if self.nodes.contains_key(&candidate) {
            Ok(candidate)
        } else if let Some(key) = self
            .aliases
            .get(&(source_id.clone(), id_or_alias.to_owned()))
        {
            Ok(key.clone())
        } else {
            Ok(candidate)
        }
    }

    fn canonical_source_id<'a>(&'a self, source_id: &'a SourceRootId) -> &'a SourceRootId {
        if source_id == &SourceRootId::primary() {
            self.context.sources.primary_id()
        } else {
            source_id
        }
    }

    fn normalize_key(&self, key: &ProjectKey) -> miette::Result<ProjectKey> {
        ProjectKey::new(
            self.canonical_source_id(key.source_id()).clone(),
            key.project_id().clone(),
        )
    }
}

impl GraphData<Project, DependencyScope, ProjectKey> for ProjectGraph {
    fn get_graph(&self) -> &DiGraph<NodeIndex, DependencyScope> {
        &self.graph
    }

    fn get_nodes(&self) -> FxHashMap<NodeIndex, &Project> {
        self.nodes
            .values()
            .map(|node| (node.index, &node.project))
            .collect()
    }

    fn get_node_by_index(&self, index: &NodeIndex) -> &Project {
        &self.nodes[&self.indexes[index]].project
    }

    fn get_node_key(&self, node: &Project) -> ProjectKey {
        node.key()
    }
}

impl GraphConnections<Project, DependencyScope, ProjectKey> for ProjectGraph {
    fn get_node_index(&self, node: &Project) -> NodeIndex {
        self.nodes[&node.key()].index
    }
}

impl GraphConversions<Project, DependencyScope, ProjectKey> for ProjectGraph {
    fn to_labeled_graph(&self) -> DiGraph<String, String> {
        let mut id_counts = FxHashMap::default();

        for node in self.nodes.values() {
            *id_counts.entry(&node.project.id).or_insert(0) += 1;
        }

        self.graph.map(
            |_, index| {
                let project = self.get_node_by_index(index);

                if id_counts[&project.id] > 1 {
                    project.key().to_string()
                } else {
                    project.id.to_string()
                }
            },
            |_, edge| edge.to_string(),
        )
    }
}

impl GraphToDot<Project, DependencyScope, ProjectKey> for ProjectGraph {}

impl GraphToJson<Project, DependencyScope, ProjectKey> for ProjectGraph {}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::SourceRegistry;
    use moon_config::{LayerType, ProjectDependencyConfig, StackType};
    use moon_graph_utils::GraphConnections;

    fn project(source_id: SourceRootId, id: &str) -> Project {
        Project {
            id: Id::raw(id),
            source_id,
            ..Project::default()
        }
    }

    fn cross_dependency(
        source_root: SourceRootId,
        id: &str,
        scope: DependencyScope,
    ) -> ProjectDependencyConfig {
        ProjectDependencyConfig {
            id: Id::raw(id),
            scope,
            source_root: Some(source_root),
            ..Default::default()
        }
    }

    fn local_graph(
        source_id: SourceRootId,
        root: &str,
        projects: Vec<Project>,
        edges: &[(usize, usize)],
    ) -> ProjectGraph {
        let context = GraphExpanderContext {
            sources: Arc::new(SourceRegistry::new(source_id, PathBuf::from(root))),
            working_dir: PathBuf::from(root),
            workspace_root: PathBuf::from(root),
            ..Default::default()
        };
        let mut project_graph = ProjectGraph::new(context);
        let mut graph = DiGraph::new();

        for project in projects {
            let index = graph.add_node(NodeIndex::new(graph.node_count()));
            let key = project.key();
            project_graph.indexes.insert(index, key.clone());
            project_graph
                .nodes
                .insert(key, ProjectNode { index, project });
        }

        for (source, target) in edges {
            graph.add_edge(
                NodeIndex::new(*source),
                NodeIndex::new(*target),
                DependencyScope::Production,
            );
        }

        project_graph.set_graph(graph).unwrap();
        project_graph
    }

    fn loop_graph(
        scope_ab: DependencyScope,
        scope_ba: DependencyScope,
    ) -> DiGraph<NodeIndex, DependencyScope> {
        let mut graph = DiGraph::new();
        let a = graph.add_node(NodeIndex::new(0));
        let b = graph.add_node(NodeIndex::new(1));

        graph.add_edge(a, b, scope_ab);
        graph.add_edge(b, a, scope_ba);
        graph
    }

    #[test]
    fn routes_scopes_into_expected_partitions() {
        assert_eq!(
            ScopePartition::of(&DependencyScope::Production),
            ScopePartition::Production
        );
        assert_eq!(
            ScopePartition::of(&DependencyScope::Peer),
            ScopePartition::Production
        );
        assert_eq!(
            ScopePartition::of(&DependencyScope::Build),
            ScopePartition::Development
        );
        assert_eq!(
            ScopePartition::of(&DependencyScope::Development),
            ScopePartition::Development
        );
        assert_eq!(
            ScopePartition::of(&DependencyScope::Root),
            ScopePartition::Development
        );
    }

    #[test]
    fn stores_duplicate_local_ids_without_key_collisions() {
        let primary = SourceRootId::primary();
        let secondary = SourceRootId::new("secondary").unwrap();
        let mut graph = ProjectGraph::default();

        for (index, project) in [
            project(primary.clone(), "app"),
            project(secondary.clone(), "app"),
        ]
        .into_iter()
        .enumerate()
        {
            let index = NodeIndex::new(index);
            graph
                .nodes
                .insert(project.key(), ProjectNode { index, project });
        }

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.get_unexpanded("app").unwrap().source_id, primary);
        assert_eq!(
            graph
                .get_unexpanded_by_key(&ProjectKey::new(secondary.clone(), Id::raw("app")).unwrap())
                .unwrap()
                .source_id,
            secondary
        );
    }

    #[test]
    fn composes_source_local_graphs_deterministically() {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let primary_root = "/workspace/primary";
        let child_root = "/workspace/child";
        let mut sources = SourceRegistry::new(primary_id.clone(), primary_root.into());
        sources
            .register(child_id.clone(), child_root.into())
            .unwrap();

        let mut primary_app = project(primary_id.clone(), "app");
        primary_app.source = "packages/app".into();
        primary_app.dependencies.push(ProjectDependencyConfig {
            id: Id::raw("shared"),
            ..Default::default()
        });
        let mut primary_dep = project(primary_id.clone(), "primary-dep");
        primary_dep.source = "packages/dep".into();
        let mut primary = local_graph(
            primary_id.clone(),
            primary_root,
            vec![primary_app, primary_dep],
            &[(0, 1)],
        );
        primary.aliases.insert(
            (primary_id.clone(), "shared".into()),
            ProjectKey::new(primary_id.clone(), Id::raw("primary-dep")).unwrap(),
        );
        primary.default_key = Some(ProjectKey::new(primary_id.clone(), Id::raw("app")).unwrap());

        let mut child_app = project(child_id.clone(), "app");
        child_app.source = "packages/app".into();
        child_app.dependencies.push(ProjectDependencyConfig {
            id: Id::raw("shared"),
            ..Default::default()
        });
        let mut child_dep = project(child_id.clone(), "child-dep");
        child_dep.source = "packages/dep".into();
        let mut child = local_graph(
            child_id.clone(),
            child_root,
            vec![child_app, child_dep],
            &[(0, 1)],
        );
        child.aliases.insert(
            (child_id.clone(), "shared".into()),
            ProjectKey::new(child_id.clone(), Id::raw("child-dep")).unwrap(),
        );
        child.default_key = Some(ProjectKey::new(child_id.clone(), Id::raw("app")).unwrap());

        // Reverse input order to verify source-ID ordering is internal to composition.
        let aggregate = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [Arc::new(primary), Arc::new(child)].into_iter().rev(),
        )
        .unwrap();
        let child_app_key = ProjectKey::new(child_id.clone(), Id::raw("app")).unwrap();
        let primary_app = aggregate.get("app").unwrap();
        let child_app = aggregate.get_by_key(&child_app_key).unwrap();

        assert_eq!(aggregate.nodes.len(), 4);
        assert_eq!(primary_app.source_id, primary_id);
        assert_eq!(child_app.source_id, child_id);
        assert_eq!(
            aggregate.dependencies_of(&child_app),
            [ProjectKey::new(child_id.clone(), Id::raw("child-dep")).unwrap()]
        );
        assert_eq!(child_app.dependencies[0].id, Id::raw("child-dep"));
        assert_eq!(aggregate.get_default().unwrap().id, Id::raw("app"));
        assert_eq!(
            aggregate.contexts[&child_id].workspace_root,
            Path::new(child_root)
        );
        assert_eq!(
            aggregate
                .get_from_path(Some(Path::new("/workspace/primary/packages/app")))
                .unwrap()
                .source_id,
            primary_id
        );
        assert_eq!(
            aggregate
                .get_from_path(Some(Path::new("/workspace/child/packages/app")))
                .unwrap()
                .source_id,
            child_id
        );

        let dot = aggregate.to_dot();
        assert!(dot.contains("child::app"));
        assert!(dot.contains("primary::app"));
        assert!(dot.contains("child-dep"));
        assert!(!dot.contains("child::child-dep"));
    }

    #[test]
    fn rejects_cross_source_edges_during_composition() {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let malformed = local_graph(
            primary_id.clone(),
            "/workspace/primary",
            vec![project(primary_id, "app"), project(child_id, "app")],
            &[(0, 1)],
        );

        let error = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [Arc::new(malformed)],
        )
        .unwrap_err();

        assert!(
            error
                .downcast_ref::<ProjectGraphError>()
                .is_some_and(|error| matches!(
                    error,
                    ProjectGraphError::UnsupportedCrossSourceEdge { .. }
                ))
        );
    }

    #[test]
    fn resolves_cross_source_ids_and_aliases_after_composition() {
        let primary_id = SourceRootId::new("acme/platform").unwrap();
        let child_id = SourceRootId::new("acme/web").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();

        let mut primary_app = project(primary_id.clone(), "app");
        primary_app.cross_source_dependencies.push(cross_dependency(
            SourceRootId::new("frontend").unwrap(),
            "app",
            DependencyScope::Production,
        ));
        let mut canonical_duplicate =
            cross_dependency(child_id.clone(), "app", DependencyScope::Production);
        canonical_duplicate.via = Some("canonical".into());
        primary_app
            .cross_source_dependencies
            .push(canonical_duplicate);
        primary_app.cross_source_dependencies.push(cross_dependency(
            child_id.clone(),
            "shared",
            DependencyScope::Build,
        ));
        let primary = Arc::new(local_graph(
            primary_id.clone(),
            "/workspace/primary",
            vec![primary_app],
            &[],
        ));

        let mut child = local_graph(
            child_id.clone(),
            "/workspace/child",
            vec![
                project(child_id.clone(), "app"),
                project(child_id.clone(), "lib"),
            ],
            &[],
        );
        child.aliases.insert(
            (child_id.clone(), "shared".into()),
            ProjectKey::new(child_id.clone(), Id::raw("lib")).unwrap(),
        );
        let child = Arc::new(child);
        let aliases =
            FxHashMap::from_iter([(SourceAlias::new("frontend").unwrap(), child_id.clone())]);

        let aggregate = ProjectGraph::compose(
            Arc::new(sources),
            &aliases,
            [Arc::clone(&child), Arc::clone(&primary)],
        )
        .unwrap();
        let app = aggregate.get("app").unwrap();
        let dependencies = aggregate.dependencies_of(&app);

        assert_eq!(
            dependencies,
            [
                ProjectKey::new(child_id.clone(), Id::raw("lib")).unwrap(),
                ProjectKey::new(child_id.clone(), Id::raw("app")).unwrap(),
            ]
        );
        assert!(app.cross_source_dependencies.is_empty());
        assert_eq!(app.dependencies.len(), 2);
        assert_eq!(
            app.dependencies
                .iter()
                .map(|dependency| dependency.id.as_str())
                .collect::<Vec<_>>(),
            ["app", "lib"]
        );
        assert!(app.dependencies.iter().all(|dep| {
            dep.source_root.as_ref() == Some(&child_id) && matches!(dep.id.as_str(), "app" | "lib")
        }));
        assert_eq!(
            app.dependencies
                .iter()
                .find(|dep| dep.id == Id::raw("app"))
                .unwrap()
                .via
                .as_deref(),
            Some("canonical")
        );

        let reversed = ProjectGraph::compose(
            Arc::clone(&aggregate.context.sources),
            &aliases,
            [primary, child],
        )
        .unwrap();
        assert_eq!(aggregate.to_dot(), reversed.to_dot());
    }

    #[test]
    fn resolves_workspace_compatibility_id_to_the_primary_source() {
        let primary_id = SourceRootId::new("acme/platform").unwrap();
        let child_id = SourceRootId::new("acme/web").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let primary = local_graph(
            primary_id.clone(),
            "/workspace/primary",
            vec![project(primary_id.clone(), "lib")],
            &[],
        );
        let mut child_app = project(child_id.clone(), "app");
        child_app.cross_source_dependencies.push(cross_dependency(
            SourceRootId::primary(),
            "lib",
            DependencyScope::Production,
        ));
        let child = local_graph(child_id.clone(), "/workspace/child", vec![child_app], &[]);

        let aggregate = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [Arc::new(primary), Arc::new(child)],
        )
        .unwrap();
        let child_app = aggregate
            .get_by_key(&ProjectKey::new(child_id, Id::raw("app")).unwrap())
            .unwrap();

        assert_eq!(
            aggregate.dependencies_of(&child_app),
            [ProjectKey::new(primary_id.clone(), Id::raw("lib")).unwrap()]
        );
        assert_eq!(
            child_app.dependencies[0].source_root.as_ref(),
            Some(&primary_id)
        );
    }

    #[test]
    fn rejects_redundant_same_source_qualified_dependencies() {
        let source_id = SourceRootId::new("acme/platform").unwrap();
        let sources = Arc::new(SourceRegistry::new(
            source_id.clone(),
            "/workspace/primary".into(),
        ));
        let mut app = project(source_id.clone(), "app");
        app.dependencies
            .push(ProjectDependencyConfig::new(Id::raw("lib")));
        app.cross_source_dependencies.push(cross_dependency(
            source_id.clone(),
            "lib",
            DependencyScope::Development,
        ));
        let local = local_graph(
            source_id.clone(),
            "/workspace/primary",
            vec![app, project(source_id.clone(), "lib")],
            &[(0, 1)],
        );

        let error =
            ProjectGraph::compose(sources, &FxHashMap::default(), [Arc::new(local)]).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<ProjectGraphError>(),
            Some(ProjectGraphError::RedundantDependencySource { .. })
        ));
    }

    #[test]
    fn rejects_same_partition_cross_source_cycles() {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let mut primary_app = project(primary_id.clone(), "app");
        primary_app.cross_source_dependencies.push(cross_dependency(
            child_id.clone(),
            "app",
            DependencyScope::Production,
        ));
        let mut child_app = project(child_id.clone(), "app");
        child_app.cross_source_dependencies.push(cross_dependency(
            primary_id.clone(),
            "app",
            DependencyScope::Peer,
        ));

        let error = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [
                Arc::new(local_graph(
                    primary_id,
                    "/workspace/primary",
                    vec![primary_app],
                    &[],
                )),
                Arc::new(local_graph(
                    child_id,
                    "/workspace/child",
                    vec![child_app],
                    &[],
                )),
            ],
        )
        .unwrap_err();

        assert!(error.to_string().contains("would introduce a cycle"));
    }

    #[test]
    fn allows_cross_partition_cross_source_cycles() {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let mut primary_app = project(primary_id.clone(), "app");
        primary_app.cross_source_dependencies.push(cross_dependency(
            child_id.clone(),
            "app",
            DependencyScope::Production,
        ));
        let mut child_app = project(child_id.clone(), "app");
        child_app.cross_source_dependencies.push(cross_dependency(
            primary_id.clone(),
            "app",
            DependencyScope::Development,
        ));

        let aggregate = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [
                Arc::new(local_graph(
                    primary_id,
                    "/workspace/primary",
                    vec![primary_app],
                    &[],
                )),
                Arc::new(local_graph(
                    child_id,
                    "/workspace/child",
                    vec![child_app],
                    &[],
                )),
            ],
        )
        .unwrap();

        assert_eq!(aggregate.production_graph().edge_count(), 1);
        assert_eq!(aggregate.development_graph().edge_count(), 1);
    }

    #[test]
    fn enforces_constraints_from_the_declaring_source() {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let mut primary_app = project(primary_id.clone(), "app");
        primary_app.layer = LayerType::Application;
        primary_app.config.stack = StackType::Frontend;
        primary_app.cross_source_dependencies.push(cross_dependency(
            child_id.clone(),
            "app",
            DependencyScope::Production,
        ));
        let mut child_app = project(child_id.clone(), "app");
        child_app.layer = LayerType::Application;
        child_app.config.stack = StackType::Frontend;
        let mut primary = local_graph(primary_id, "/workspace/primary", vec![primary_app], &[]);
        Arc::make_mut(&mut primary.context.workspace_config)
            .constraints
            .enforce_layer_relationships = true;
        let mut child = local_graph(child_id, "/workspace/child", vec![child_app], &[]);
        Arc::make_mut(&mut child.context.workspace_config)
            .constraints
            .enforce_layer_relationships = false;

        let error = ProjectGraph::compose(
            Arc::new(sources),
            &FxHashMap::default(),
            [Arc::new(primary), Arc::new(child)],
        )
        .unwrap_err();

        assert!(error.to_string().contains("Layering violation"));
    }

    #[test]
    fn toposorts_partitions_in_dependency_first_order() {
        let mut project_graph = ProjectGraph::default();

        // a -> b (production), b -> a (development)
        project_graph
            .set_graph(loop_graph(
                DependencyScope::Production,
                DependencyScope::Development,
            ))
            .unwrap();
        project_graph.indexes.insert(
            NodeIndex::new(0),
            ProjectKey::primary(Id::raw("a")).unwrap(),
        );
        project_graph.indexes.insert(
            NodeIndex::new(1),
            ProjectKey::primary(Id::raw("b")).unwrap(),
        );

        // The union cycles, but each partition can still be sorted
        assert_eq!(
            project_graph.partitioned_toposort(ScopePartition::Production),
            [Id::raw("b"), Id::raw("a")]
        );
        assert_eq!(
            project_graph.partitioned_toposort(ScopePartition::Development),
            [Id::raw("a"), Id::raw("b")]
        );
    }

    #[test]
    fn would_cycle_for_self_loops_in_either_partition() {
        let mut graph = DiGraph::<(), DependencyScope>::new();
        let a = graph.add_node(());

        assert!(would_cycle_in_scope(
            &graph,
            a,
            a,
            &DependencyScope::Production
        ));
        assert!(would_cycle_in_scope(&graph, a, a, &DependencyScope::Build));
    }

    #[test]
    fn set_graph_allows_cross_partition_cycles() {
        let mut project_graph = ProjectGraph::default();

        project_graph
            .set_graph(loop_graph(
                DependencyScope::Production,
                DependencyScope::Development,
            ))
            .unwrap();

        assert_eq!(project_graph.production_graph().edge_count(), 1);
        assert_eq!(project_graph.development_graph().edge_count(), 1);
    }

    #[test]
    fn set_graph_errors_for_production_partition_cycles() {
        let mut project_graph = ProjectGraph::default();

        let error = project_graph
            .set_graph(loop_graph(
                DependencyScope::Production,
                DependencyScope::Peer,
            ))
            .unwrap_err();

        assert!(error.to_string().contains("would introduce a cycle"));
    }

    #[test]
    fn set_graph_errors_for_development_partition_cycles() {
        let mut project_graph = ProjectGraph::default();

        let error = project_graph
            .set_graph(loop_graph(
                DependencyScope::Build,
                DependencyScope::Development,
            ))
            .unwrap_err();

        assert!(error.to_string().contains("would introduce a cycle"));
    }
}
