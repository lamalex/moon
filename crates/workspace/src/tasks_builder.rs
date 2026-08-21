use crate::projects_builder::ProjectBuildData;
use daggy::Dag;
use moon_common::Id;
use moon_config::TaskDependencyType;
use moon_graph_utils::{GraphExpanderContext, NodeState};
use moon_project_graph::ProjectGraph;
use moon_target::TaskKey;
use moon_task::{Target, Task, TaskOptions};
use moon_task_graph::{TaskGraph, TaskGraphError, TaskNode};
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::instrument;

pub type TaskDag = Dag<NodeState<Task>, TaskDependencyType>;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct TaskBuildData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_index: Option<NodeIndex>,

    #[serde(skip)]
    pub options: TaskOptions,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Id>,

    #[serde(skip)]
    pub has_outputs: bool,

    pub target: Target,
}

impl TaskBuildData {
    // TODO deprecated
    pub fn resolve_target(
        target: &Target,
        project_data: &FxHashMap<Id, ProjectBuildData>,
    ) -> miette::Result<Target> {
        // Target may be using an alias!
        let project_id = ProjectBuildData::resolve_id(target.get_project_id()?, project_data);

        // IDs should be valid here, so ignore the result
        Target::new(&project_id, target.get_task_id()?)
    }
}

#[derive(Deserialize, Serialize)]
pub struct WorkspaceTasksBuilder {
    /// The task DAG.
    pub graph: TaskDag,

    /// Map of canonical task keys to their graph index.
    pub keys_to_indexes: FxHashMap<TaskKey, NodeIndex>,
}

impl WorkspaceTasksBuilder {
    pub fn get_or_insert_node(&mut self, key: &TaskKey) -> NodeIndex {
        match self.keys_to_indexes.get(key) {
            Some(index) => *index,
            None => {
                let index = self.graph.add_node(NodeState::Loading);
                self.keys_to_indexes.insert(key.to_owned(), index);
                index
            }
        }
    }

    pub fn insert_or_update_node(&mut self, task: Task) {
        let key = task.key();
        // Project node may have been inserted through an edge first,
        // so we need to update the state from loading to loaded
        if let Some(index) = self.keys_to_indexes.get(&key)
            && let Some(node) = self.graph.node_weight_mut(*index)
        {
            *node = NodeState::Loaded(task);
        }
        // Otherwise the node was inserted first, so we can set as loaded
        else {
            self.keys_to_indexes
                .insert(key, self.graph.add_node(NodeState::Loaded(task)));
        }
    }
}

impl WorkspaceTasksBuilder {
    pub fn new() -> Self {
        Self {
            graph: TaskDag::default(),
            keys_to_indexes: FxHashMap::default(),
        }
    }

    #[instrument(skip_all)]
    pub fn build(&mut self, tasks: Vec<Task>) -> miette::Result<()> {
        for task in tasks {
            let from_index = self.get_or_insert_node(&task.key());

            for dep_config in &task.deps {
                let dep_key = TaskKey::from_target(task.source_id.clone(), &dep_config.target)?;
                let to_index = self.get_or_insert_node(&dep_key);
                let scope = if dep_config.optional.is_some_and(|v| v) {
                    TaskDependencyType::Optional
                } else {
                    TaskDependencyType::Required
                };

                self.graph
                    .add_edge(from_index, to_index, scope)
                    .map_err(|_| TaskGraphError::WouldCycle {
                        source_target: task.target.to_string(),
                        target_target: dep_config.target.to_string(),
                    })?;
            }

            self.insert_or_update_node(task);
        }

        Ok(())
    }

    pub fn finalize(
        self,
        context: GraphExpanderContext,
        project_graph: Arc<ProjectGraph>,
    ) -> TaskGraph {
        let mut task_graph = TaskGraph::new(context, project_graph);
        let mut loaded_tasks = FxHashMap::default();

        // TODO switch to filter_map_owned
        task_graph.graph = self.graph.filter_map(
            |ni, node| match node {
                NodeState::Loading => None,
                NodeState::Loaded(task) => {
                    loaded_tasks.insert(ni, task.to_owned());

                    Some(ni)
                }
            },
            |_, edge| Some(*edge),
        );

        for index in task_graph.graph.graph().node_indices() {
            let old_index = *task_graph.graph.node_weight(index).unwrap();
            let task = loaded_tasks.remove(&old_index).unwrap();
            let key = task.key();

            task_graph.indexes.insert(index, key.clone());
            task_graph.nodes.insert(key, TaskNode { index, task });
        }

        // Weight-based lookups require each node's weight to be its own
        // index, which may not be the case when placeholder nodes were
        // dropped by the filter above, so rewrite them
        for index in 0..task_graph.graph.node_count() {
            let index = NodeIndex::new(index);
            *task_graph.graph.node_weight_mut(index).unwrap() = index;
        }

        task_graph
            .resolve_source_local_dependencies()
            .expect("Resolved task dependencies must use valid canonical targets.");

        task_graph
    }
}
