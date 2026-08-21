use crate::{DownstreamScope, UpstreamScope};
use moon_common::{SourcePathBuf, SourceRootId};
use moon_env_var::GlobalEnvBag;
use moon_target::{ProjectKey, TaskKey};
use moon_task::TaskOptionRunInCI;
use moon_vcs::{ChangedFilesObservation, ImpactCompleteness};
use moon_workspace_graph::{GraphConnections, WorkspaceGraph};
use serde::Serialize;
use starbase_utils::fs;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Debug, Default, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AggregateAffectedProjectState {
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub files: BTreeSet<SourcePathBuf>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub tasks: BTreeSet<TaskKey>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub upstream: BTreeSet<ProjectKey>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub downstream: BTreeSet<ProjectKey>,
    pub other: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AggregateAffectedTaskState {
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub env: BTreeSet<String>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub files: BTreeSet<SourcePathBuf>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub projects: BTreeSet<ProjectKey>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub upstream: BTreeSet<TaskKey>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub downstream: BTreeSet<TaskKey>,
    pub other: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AggregateAffected {
    pub completeness: ImpactCompleteness,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<ProjectKey, AggregateAffectedProjectState>,
    #[serde(skip)]
    pub should_check: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tasks: BTreeMap<TaskKey, AggregateAffectedTaskState>,
}

impl AggregateAffected {
    pub fn is_project_affected(&self, key: &ProjectKey) -> bool {
        self.should_check && self.projects.contains_key(key)
    }

    pub fn is_task_affected(&self, key: &TaskKey) -> bool {
        self.should_check && self.tasks.contains_key(key)
    }
}

enum ProjectCause {
    File(SourcePathBuf),
    Unavailable,
}

enum TaskCause {
    Env(String),
    File(SourcePathBuf),
    Unavailable,
}

pub struct AggregateAffectedTracker {
    ci: bool,
    workspace_graph: Arc<WorkspaceGraph>,
    observations: BTreeMap<SourceRootId, ChangedFilesObservation<SourcePathBuf>>,
    affected: AggregateAffected,
    project_downstream: DownstreamScope,
    project_upstream: UpstreamScope,
    task_downstream: DownstreamScope,
    task_upstream: UpstreamScope,
}

impl AggregateAffectedTracker {
    pub fn new(
        workspace_graph: Arc<WorkspaceGraph>,
        mut observations: BTreeMap<SourceRootId, ChangedFilesObservation<SourcePathBuf>>,
    ) -> miette::Result<Self> {
        if !workspace_graph.is_aggregate() {
            return Err(miette::miette!(
                "Aggregate affected tracking requires an aggregate read-only workspace graph."
            ));
        }

        for (source_id, _) in workspace_graph.sources.iter() {
            observations.entry(source_id.clone()).or_insert_with(|| {
                ChangedFilesObservation::unavailable("No changed-file observation was returned.")
            });
        }

        let completeness = observations
            .values()
            .map(|observation| observation.completeness)
            .max()
            .unwrap_or_default();
        let mut diagnostics = observations
            .iter()
            .flat_map(|(source, observation)| {
                observation
                    .diagnostics
                    .iter()
                    .map(move |diagnostic| format!("{source}: {diagnostic}"))
            })
            .collect::<Vec<_>>();
        diagnostics.sort();
        diagnostics.dedup();

        Ok(Self {
            ci: false,
            workspace_graph,
            observations,
            affected: AggregateAffected {
                completeness,
                diagnostics,
                ..Default::default()
            },
            project_downstream: DownstreamScope::None,
            project_upstream: UpstreamScope::Deep,
            task_downstream: DownstreamScope::None,
            task_upstream: UpstreamScope::Deep,
        })
    }

    pub fn build(mut self) -> AggregateAffected {
        self.affected.should_check = self.observations.values().any(|observation| {
            observation.completeness != ImpactCompleteness::Exact
                || !observation.files.files.is_empty()
        });
        self.affected
    }

    pub fn set_ci_check(&mut self, ci: bool) -> &mut Self {
        self.ci = ci;
        self
    }

    pub fn set_project_scopes(
        &mut self,
        upstream: UpstreamScope,
        downstream: DownstreamScope,
    ) -> &mut Self {
        self.project_upstream = upstream;
        self.project_downstream = downstream;
        self
    }

    pub fn set_task_scopes(
        &mut self,
        upstream: UpstreamScope,
        downstream: DownstreamScope,
    ) -> &mut Self {
        self.task_upstream = upstream;
        self.task_downstream = downstream;
        self
    }

    pub fn set_scopes(
        &mut self,
        upstream: UpstreamScope,
        downstream: DownstreamScope,
    ) -> &mut Self {
        self.set_project_scopes(upstream, downstream);
        self.set_task_scopes(upstream, downstream)
    }

    pub fn track_projects(&mut self) -> miette::Result<&mut Self> {
        let mut keys = self.workspace_graph.projects.get_node_keys();
        keys.sort();

        for key in keys {
            let project = self.workspace_graph.get_project_by_key(&key)?;
            let Some(observation) = self.observations.get(key.source_id()) else {
                continue;
            };
            let mut files = observation.files.files.keys().collect::<Vec<_>>();
            files.sort();
            let cause = if observation.completeness == ImpactCompleteness::Unavailable {
                Some(ProjectCause::Unavailable)
            } else if project.is_root_level() {
                files
                    .iter()
                    .find(|file| !file.path.as_str().starts_with('.'))
                    .map(|file| (*file).clone())
                    .map(ProjectCause::File)
            } else {
                files
                    .iter()
                    .find(|file| file.path.starts_with(&project.source))
                    .map(|file| (*file).clone())
                    .map(ProjectCause::File)
            };

            if let Some(cause) = cause {
                self.mark_project(&key, cause)?;
            }
        }

        Ok(self)
    }

    pub async fn track_projects_async(&mut self) -> miette::Result<&mut Self> {
        self.track_projects()
    }

    pub fn track_tasks(&mut self) -> miette::Result<&mut Self> {
        let mut tasks = self.workspace_graph.get_all_tasks_with_keys()?;
        tasks.sort_by(|a, b| a.0.cmp(&b.0));

        for (key, task) in tasks {
            let Some(observation) = self.observations.get(key.project_key().source_id()) else {
                continue;
            };
            let cause = if observation.completeness == ImpactCompleteness::Unavailable
                || self.ci && matches!(task.options.run_in_ci, TaskOptionRunInCI::Always)
            {
                Some(TaskCause::Unavailable)
            } else if matches!(
                (self.ci, &task.options.run_in_ci),
                (
                    true,
                    TaskOptionRunInCI::Enabled(false) | TaskOptionRunInCI::Skip
                ) | (false, TaskOptionRunInCI::Only)
            ) || task.state.empty_inputs
            {
                None
            } else if let Some(name) = task.input_env.iter().find(|name| {
                GlobalEnvBag::instance()
                    .get(name)
                    .is_some_and(|value| !value.is_empty())
            }) {
                Some(TaskCause::Env(name.clone()))
            } else {
                let globset = task.create_globset()?;
                let root = self
                    .workspace_graph
                    .sources
                    .get(key.project_key().source_id())?;
                let mut matched = None;
                let mut files = observation.files.files.keys().collect::<Vec<_>>();
                files.sort();

                for file in files {
                    let affected = if let Some(params) = task.input_files.get(&file.path) {
                        if let Some(matcher) = &params.content {
                            let absolute = file.path.to_logical_path(root);
                            absolute.exists() && matcher.is_match(&fs::read_file(absolute)?)
                        } else {
                            true
                        }
                    } else {
                        globset.matches(file.path.as_str())
                    };

                    if affected {
                        matched = Some(TaskCause::File(file.clone()));
                        break;
                    }
                }

                matched
            };

            if let Some(cause) = cause {
                self.mark_task(&key, cause)?;
            }
        }

        Ok(self)
    }

    pub async fn track_tasks_async(&mut self) -> miette::Result<&mut Self> {
        self.track_tasks()
    }

    fn mark_project(&mut self, key: &ProjectKey, cause: ProjectCause) -> miette::Result<()> {
        let state = self.affected.projects.entry(key.clone()).or_default();
        match cause {
            ProjectCause::File(file) => {
                state.files.insert(file);
            }
            ProjectCause::Unavailable => state.other = true,
        }

        self.track_project_relations(key, true, 0, &mut BTreeSet::new())?;
        self.track_project_relations(key, false, 0, &mut BTreeSet::new())
    }

    fn track_project_relations(
        &mut self,
        key: &ProjectKey,
        upstream: bool,
        depth: u8,
        visited: &mut BTreeSet<ProjectKey>,
    ) -> miette::Result<()> {
        if !visited.insert(key.clone()) {
            return Ok(());
        }
        let in_scope = if upstream {
            self.project_upstream.is_in_scope(depth)
        } else {
            self.project_downstream.is_in_scope(depth)
        };
        if !in_scope {
            return Ok(());
        }

        let project = self.workspace_graph.get_project_by_key(key)?;
        let mut related = if upstream {
            self.workspace_graph
                .projects
                .direct_dependencies_with_scopes(key)?
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>()
        } else {
            self.workspace_graph.projects.dependents_of(&project)
        };
        related.sort();

        for related_key in related {
            let state = self
                .affected
                .projects
                .entry(related_key.clone())
                .or_default();
            if upstream {
                state.downstream.insert(key.clone());
            } else {
                state.upstream.insert(key.clone());
            }
            self.track_project_relations(&related_key, upstream, depth + 1, visited)?;
        }

        Ok(())
    }

    fn mark_task(&mut self, key: &TaskKey, cause: TaskCause) -> miette::Result<()> {
        let state = self.affected.tasks.entry(key.clone()).or_default();
        match cause {
            TaskCause::Env(name) => {
                state.env.insert(name);
            }
            TaskCause::File(file) => {
                state.files.insert(file);
            }
            TaskCause::Unavailable => state.other = true,
        }
        self.affected
            .projects
            .entry(key.project_key().clone())
            .or_default()
            .tasks
            .insert(key.clone());

        self.track_task_relations(key, true, 0, &mut BTreeSet::new())?;
        self.track_task_relations(key, false, 0, &mut BTreeSet::new())
    }

    fn track_task_relations(
        &mut self,
        key: &TaskKey,
        upstream: bool,
        depth: u8,
        visited: &mut BTreeSet<TaskKey>,
    ) -> miette::Result<()> {
        if !visited.insert(key.clone()) {
            return Ok(());
        }
        let in_scope = if upstream {
            self.task_upstream.is_in_scope(depth)
        } else {
            self.task_downstream.is_in_scope(depth)
        };
        if !in_scope {
            return Ok(());
        }

        let task = self.workspace_graph.get_task_by_key(key)?;
        let mut related = if upstream {
            self.workspace_graph.tasks.dependencies_of(&task)
        } else {
            self.workspace_graph.tasks.dependents_of(&task)
        };
        related.sort();

        for related_key in related {
            let state = self.affected.tasks.entry(related_key.clone()).or_default();
            if upstream {
                state.downstream.insert(key.clone());
            } else {
                state.upstream.insert(key.clone());
            }
            self.track_task_relations(&related_key, upstream, depth + 1, visited)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::{Id, SourceRegistry, path::WorkspaceRelativePathBuf};
    use moon_config::DependencyScope;
    use moon_project::Project;
    use moon_target::Target;
    use moon_task::Task;
    use moon_workspace_graph::{
        GraphExpanderContext,
        projects::{ProjectGraph, ProjectNode},
        tasks::{TaskGraph, TaskNode},
    };
    use petgraph::graph::DiGraph;

    fn local_graph(source_id: SourceRootId, root: &str) -> (Arc<ProjectGraph>, Arc<TaskGraph>) {
        let sources = Arc::new(SourceRegistry::new(source_id.clone(), root.into()));
        let context = GraphExpanderContext {
            sources,
            working_dir: root.into(),
            workspace_root: root.into(),
            ..Default::default()
        };
        let project = Project {
            id: Id::raw("app"),
            source_id: source_id.clone(),
            source: WorkspaceRelativePathBuf::from("packages/app"),
            task_targets: vec![Target::new("app", "build").unwrap()],
            ..Project::default()
        };
        let project_key = project.key();
        let mut projects = ProjectGraph::new(context.clone());
        let mut project_graph = DiGraph::<_, DependencyScope>::new();
        let project_index = project_graph.add_node(Default::default());
        projects.indexes.insert(project_index, project_key.clone());
        projects.nodes.insert(
            project_key.clone(),
            ProjectNode {
                index: project_index,
                project,
            },
        );
        projects.set_graph(project_graph).unwrap();
        let projects = Arc::new(projects);

        let task = Task {
            id: Id::raw("build"),
            source_id,
            target: Target::new("app", "build").unwrap(),
            ..Task::default()
        };
        let task_key = task.key();
        let mut tasks = TaskGraph::new(context, Arc::clone(&projects));
        let task_index = tasks.graph.add_node(Default::default());
        tasks.indexes.insert(task_index, task_key.clone());
        tasks.nodes.insert(
            task_key,
            TaskNode {
                index: task_index,
                task,
            },
        );

        (projects, Arc::new(tasks))
    }

    fn aggregate_graph() -> Arc<WorkspaceGraph> {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/primary".into());
        sources.register(child_id.clone(), "/child".into()).unwrap();
        let sources = Arc::new(sources);
        let (primary_projects, primary_tasks) = local_graph(primary_id, "/primary");
        let (child_projects, child_tasks) = local_graph(child_id, "/child");
        let projects = Arc::new(
            ProjectGraph::compose(
                Arc::clone(&sources),
                &Default::default(),
                [primary_projects, child_projects],
            )
            .unwrap(),
        );

        Arc::new(
            WorkspaceGraph::new_aggregate(projects, sources, [primary_tasks, child_tasks]).unwrap(),
        )
    }

    fn observation(
        source: SourceRootId,
        completeness: ImpactCompleteness,
        path: Option<&str>,
    ) -> ChangedFilesObservation<SourcePathBuf> {
        let mut observation = ChangedFilesObservation {
            completeness,
            ..Default::default()
        };
        if let Some(path) = path {
            observation.files.files.insert(
                SourcePathBuf::new(source, path),
                vec![moon_vcs::ChangedStatus::Modified],
            );
        }
        observation
    }

    #[test]
    fn identical_relative_paths_only_affect_the_owning_source() {
        let graph = aggregate_graph();
        let primary = SourceRootId::new("primary").unwrap();
        let child = SourceRootId::new("child").unwrap();
        let mut tracker = AggregateAffectedTracker::new(
            graph,
            BTreeMap::from([
                (
                    primary.clone(),
                    observation(primary.clone(), ImpactCompleteness::Exact, None),
                ),
                (
                    child.clone(),
                    observation(
                        child.clone(),
                        ImpactCompleteness::Conservative,
                        Some("packages/app/file.rs"),
                    ),
                ),
            ]),
        )
        .unwrap();
        tracker.track_projects().unwrap();
        let affected = tracker.build();

        assert_eq!(affected.completeness, ImpactCompleteness::Conservative);
        assert!(!affected.is_project_affected(&ProjectKey::new(primary, Id::raw("app")).unwrap()));
        assert!(affected.is_project_affected(&ProjectKey::new(child, Id::raw("app")).unwrap()));
    }

    #[test]
    fn unavailable_marks_only_its_duplicate_canonical_identities() {
        let graph = aggregate_graph();
        let primary = SourceRootId::new("primary").unwrap();
        let child = SourceRootId::new("child").unwrap();
        let mut tracker = AggregateAffectedTracker::new(
            graph,
            BTreeMap::from([
                (
                    primary.clone(),
                    observation(primary.clone(), ImpactCompleteness::Exact, None),
                ),
                (
                    child.clone(),
                    ChangedFilesObservation::unavailable("provider failed"),
                ),
            ]),
        )
        .unwrap();
        tracker.track_projects().unwrap().track_tasks().unwrap();
        let affected = tracker.build();
        let primary_task = TaskKey::new(
            ProjectKey::new(primary, Id::raw("app")).unwrap(),
            Id::raw("build"),
        )
        .unwrap();
        let child_task = TaskKey::new(
            ProjectKey::new(child, Id::raw("app")).unwrap(),
            Id::raw("build"),
        )
        .unwrap();

        assert!(!affected.is_task_affected(&primary_task));
        assert!(affected.is_task_affected(&child_task));
        assert_ne!(primary_task, child_task);
        assert_eq!(affected.diagnostics, ["child: provider failed"]);
    }

    #[test]
    fn exact_empty_observations_mark_nothing() {
        let graph = aggregate_graph();
        let primary = SourceRootId::new("primary").unwrap();
        let child = SourceRootId::new("child").unwrap();
        let mut tracker = AggregateAffectedTracker::new(
            graph,
            BTreeMap::from([
                (
                    primary.clone(),
                    observation(primary, ImpactCompleteness::Exact, None),
                ),
                (
                    child.clone(),
                    observation(child, ImpactCompleteness::Exact, None),
                ),
            ]),
        )
        .unwrap();
        tracker.track_projects().unwrap().track_tasks().unwrap();
        let affected = tracker.build();

        assert_eq!(affected.completeness, ImpactCompleteness::Exact);
        assert!(affected.projects.is_empty());
        assert!(affected.tasks.is_empty());
        assert!(!affected.should_check);
    }
}
