mod query_projects;
mod query_tasks;

use moon_common::{SourceRegistry, SourceRootId};
use moon_project_graph::{Project, ProjectGraph};
use moon_target::{ProjectKey, TaskKey};
use moon_task_graph::{Target, Task, TaskGraph};
use scc::HashMap;
use std::path::PathBuf;
use std::{path::Path, sync::Arc};

pub use moon_graph_utils::*;
pub use moon_project_graph as projects;
pub use moon_task_graph as tasks;

#[derive(Clone, Copy, Debug)]
pub(crate) enum QueryScope {
    Primary,
    All,
}

impl QueryScope {
    pub(crate) fn cache_key(self, input: &str) -> String {
        format!("{self:?}:{input}")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorkspaceGraphPurpose {
    #[default]
    ExecutionLocal,
    AggregateReadOnly,
}

#[derive(Default)]
pub struct WorkspaceGraph {
    pub projects: Arc<ProjectGraph>,
    pub sources: Arc<SourceRegistry>,
    pub tasks: Arc<TaskGraph>,
    /// Root of the primary source. Retained for single-source compatibility.
    pub root: PathBuf,

    purpose: WorkspaceGraphPurpose,

    /// Canonical query caches. Scope is encoded into each cache key.
    project_query_cache: HashMap<String, Arc<Vec<ProjectKey>>>,
    task_query_cache: HashMap<String, Arc<Vec<TaskKey>>>,
}

impl WorkspaceGraph {
    pub fn new(projects: Arc<ProjectGraph>, tasks: Arc<TaskGraph>, root: PathBuf) -> Self {
        Self::new_with_sources(projects, tasks, Arc::new(SourceRegistry::single(root)))
    }

    pub fn new_with_sources(
        projects: Arc<ProjectGraph>,
        tasks: Arc<TaskGraph>,
        sources: Arc<SourceRegistry>,
    ) -> Self {
        Self::create(
            projects,
            tasks,
            sources,
            WorkspaceGraphPurpose::ExecutionLocal,
        )
    }

    pub fn new_aggregate(
        projects: Arc<ProjectGraph>,
        sources: Arc<SourceRegistry>,
        task_graphs: impl IntoIterator<Item = Arc<TaskGraph>>,
    ) -> miette::Result<Self> {
        let tasks = Arc::new(TaskGraph::compose(Arc::clone(&projects), task_graphs)?);

        Ok(Self::create(
            projects,
            tasks,
            sources,
            WorkspaceGraphPurpose::AggregateReadOnly,
        ))
    }

    fn create(
        projects: Arc<ProjectGraph>,
        tasks: Arc<TaskGraph>,
        sources: Arc<SourceRegistry>,
        purpose: WorkspaceGraphPurpose,
    ) -> Self {
        let root = sources.get_primary().to_path_buf();

        Self {
            projects,
            sources,
            tasks,
            root,
            purpose,
            project_query_cache: HashMap::default(),
            task_query_cache: HashMap::default(),
        }
    }

    pub fn ensure_execution_local(&self) -> miette::Result<()> {
        if self.purpose == WorkspaceGraphPurpose::AggregateReadOnly {
            return Err(miette::miette!(
                "Aggregate workspace graphs are read-only and cannot be used for execution."
            ));
        }

        Ok(())
    }

    pub fn is_aggregate(&self) -> bool {
        self.purpose == WorkspaceGraphPurpose::AggregateReadOnly
    }

    pub fn get_primary_source_id(&self) -> &SourceRootId {
        self.sources.primary_id()
    }

    pub fn get_default_project(&self) -> miette::Result<Arc<Project>> {
        self.projects.get_default()
    }

    pub fn get_project(&self, id_or_alias: impl AsRef<str>) -> miette::Result<Arc<Project>> {
        self.projects.get(id_or_alias.as_ref())
    }

    pub fn get_project_by_key(&self, key: &ProjectKey) -> miette::Result<Arc<Project>> {
        self.projects.get_by_key(key)
    }

    pub fn get_project_from_path(
        &self,
        starting_file: Option<&Path>,
    ) -> miette::Result<Arc<Project>> {
        self.projects.get_from_path(starting_file)
    }

    pub fn get_project_with_tasks(&self, id_or_alias: impl AsRef<str>) -> miette::Result<Project> {
        let base_project = self.get_project(id_or_alias)?;
        self.attach_tasks(base_project)
    }

    pub fn get_project_with_tasks_by_key(&self, key: &ProjectKey) -> miette::Result<Project> {
        let base_project = self.get_project_by_key(key)?;
        let mut project = base_project.as_ref().to_owned();

        for (_, base_task) in self.get_tasks_from_project_by_key(key)? {
            project
                .tasks
                .insert(base_task.id.clone(), base_task.as_ref().to_owned());
        }

        Ok(project)
    }

    fn attach_tasks(&self, base_project: Arc<Project>) -> miette::Result<Project> {
        let mut project = base_project.as_ref().to_owned();

        for base_task in self.get_tasks_from_project(&project.id)? {
            project
                .tasks
                .insert(base_task.id.clone(), base_task.as_ref().to_owned());
        }

        Ok(project)
    }

    pub fn get_projects(&self) -> miette::Result<Vec<Arc<Project>>> {
        self.projects.get_all()
    }

    pub fn get_projects_unexpanded(&self) -> Vec<&Project> {
        self.projects.get_all_unexpanded()
    }

    pub fn get_projects_by_id<I, T>(&self, ids: I) -> miette::Result<Vec<Arc<Project>>>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut projects = vec![];

        for id in ids {
            projects.push(self.get_project(id.as_ref())?);
        }

        Ok(projects)
    }

    pub fn get_task(&self, target: &Target) -> miette::Result<Arc<Task>> {
        self.tasks.get(target)
    }

    /// Return a task by its canonical source-qualified identity.
    pub fn get_task_by_key(&self, key: &TaskKey) -> miette::Result<Arc<Task>> {
        let source_id = self.canonical_source_id(key.project_key().source_id());
        let key = TaskKey::new(
            ProjectKey::new(source_id.clone(), key.project_key().project_id().clone())?,
            key.task_id().clone(),
        )?;

        self.tasks.get_by_key(&key)
    }

    /// Return all non-internal tasks with canonical source-qualified identities.
    pub fn get_all_tasks_with_keys(&self) -> miette::Result<Vec<(TaskKey, Arc<Task>)>> {
        let mut tasks = self
            .tasks
            .get_all()?
            .into_iter()
            .filter(|task| !task.is_internal())
            .map(|task| (task.key(), task))
            .collect::<Vec<_>>();

        tasks.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(tasks)
    }

    /// Return non-internal tasks belonging to a canonical project identity.
    pub fn get_tasks_from_project_by_key(
        &self,
        project_key: &ProjectKey,
    ) -> miette::Result<Vec<(TaskKey, Arc<Task>)>> {
        let project = self.get_project_by_key(project_key)?;
        let source_id = self.canonical_source_id(project_key.source_id());
        let mut tasks = vec![];

        for target in &project.task_targets {
            let key = TaskKey::from_target(source_id.clone(), target)?;
            let task = self.tasks.get_by_key(&key)?;

            if !task.is_internal() {
                tasks.push((task.key(), task));
            }
        }

        tasks.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(tasks)
    }

    pub fn get_task_from_project(
        &self,
        project_id_or_alias: impl AsRef<str>,
        task_id: impl AsRef<str>,
    ) -> miette::Result<Arc<Task>> {
        let project_id = self.projects.resolve_id(project_id_or_alias.as_ref());
        let target = Target::new(project_id, task_id)?;

        self.get_task(&target)
    }

    pub fn get_tasks_from_project(
        &self,
        project_id_or_alias: impl AsRef<str>,
    ) -> miette::Result<Vec<Arc<Task>>> {
        let project = self.get_project(project_id_or_alias)?;
        let mut all = vec![];

        for target in &project.task_targets {
            let task = self.get_task(target)?;

            if !task.is_internal() {
                all.push(task);
            }
        }

        Ok(all)
    }

    /// Get all non-internal tasks.
    pub fn get_tasks(&self) -> miette::Result<Vec<Arc<Task>>> {
        Ok(self
            .tasks
            .get_all()?
            .into_iter()
            .filter(|task| task.source_id == *self.sources.primary_id() && !task.is_internal())
            .collect())
    }

    pub fn get_tasks_unexpanded(&self) -> miette::Result<Vec<&Task>> {
        Ok(self
            .tasks
            .get_all_unexpanded()?
            .into_iter()
            .filter(|task| task.source_id == *self.sources.primary_id() && !task.is_internal())
            .collect())
    }

    /// Get all tasks, including internal.
    pub fn get_tasks_with_internal(&self) -> miette::Result<Vec<Arc<Task>>> {
        Ok(self
            .tasks
            .get_all()?
            .into_iter()
            .filter(|task| task.source_id == *self.sources.primary_id())
            .collect())
    }

    pub fn get_tasks_unexpanded_with_internal(&self) -> miette::Result<Vec<&Task>> {
        Ok(self
            .tasks
            .get_all_unexpanded()?
            .into_iter()
            .filter(|task| task.source_id == *self.sources.primary_id())
            .collect())
    }

    fn canonical_source_id(&self, source_id: &SourceRootId) -> SourceRootId {
        if source_id == &SourceRootId::primary() {
            self.sources.primary_id().clone()
        } else {
            source_id.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::Id;
    use moon_config::TaskType;
    use moon_project_graph::{ProjectAlias, ProjectNode};
    use moon_task_graph::TaskNode;

    fn local_graph(
        source_id: SourceRootId,
        root: &str,
        alias: &str,
        task_tag: &str,
        task_toolchain: &str,
        task_type: TaskType,
    ) -> (Arc<ProjectGraph>, Arc<TaskGraph>) {
        let sources = Arc::new(SourceRegistry::new(source_id.clone(), root.into()));
        let context = GraphExpanderContext {
            sources,
            working_dir: root.into(),
            workspace_root: root.into(),
            ..Default::default()
        };
        let target = Target::new("app", "build").unwrap();
        let project = Project {
            aliases: vec![ProjectAlias {
                alias: alias.into(),
                plugin: Id::raw("test"),
            }],
            id: Id::raw("app"),
            source_id: source_id.clone(),
            task_targets: vec![target.clone()],
            ..Project::default()
        };
        let mut projects = ProjectGraph::new(context.clone());
        projects.nodes.insert(
            project.key(),
            ProjectNode {
                index: Default::default(),
                project,
            },
        );
        let projects = Arc::new(projects);
        let task = Task {
            id: Id::raw("build"),
            source_id,
            tags: vec![Id::raw(task_tag)],
            target: target.clone(),
            toolchains: vec![Id::raw(task_toolchain)],
            type_of: task_type,
            ..Task::default()
        };
        let mut tasks = TaskGraph::new(context, Arc::clone(&projects));
        tasks.nodes.insert(
            task.key(),
            TaskNode {
                index: Default::default(),
                task,
            },
        );

        (projects, Arc::new(tasks))
    }

    fn aggregate_graph() -> WorkspaceGraph {
        let primary_id = SourceRootId::new("primary").unwrap();
        let child_id = SourceRootId::new("child").unwrap();
        let mut sources = SourceRegistry::new(primary_id.clone(), "/workspace/primary".into());
        sources
            .register(child_id.clone(), "/workspace/child".into())
            .unwrap();
        let sources = Arc::new(sources);
        let (primary_projects, primary_tasks) = local_graph(
            primary_id.clone(),
            "/workspace/primary",
            "primary-app",
            "primary-tag",
            "primary-toolchain",
            TaskType::Build,
        );
        let (child_projects, child_tasks) = local_graph(
            child_id.clone(),
            "/workspace/child",
            "child-app",
            "child-tag",
            "child-toolchain",
            TaskType::Run,
        );
        let projects = Arc::new(
            ProjectGraph::compose(
                Arc::clone(&sources),
                &Default::default(),
                [primary_projects, child_projects],
            )
            .unwrap(),
        );
        WorkspaceGraph::new_aggregate(projects, sources, [primary_tasks, child_tasks]).unwrap()
    }

    #[test]
    fn graph_purpose_distinguishes_execution_from_aggregate_queries() {
        let aggregate = aggregate_graph();
        let local = WorkspaceGraph::new(
            Arc::clone(&aggregate.projects),
            Arc::clone(&aggregate.tasks),
            aggregate.root.clone(),
        );

        assert!(local.ensure_execution_local().is_ok());
        assert!(!local.is_aggregate());
        assert!(aggregate.ensure_execution_local().is_err());
        assert!(aggregate.is_aggregate());
        assert_eq!(aggregate.get_all_tasks_with_keys().unwrap().len(), 2);
    }

    #[test]
    fn aggregate_tasks_preserve_duplicate_canonical_identities() {
        let graph = aggregate_graph();
        let tasks = graph.get_all_tasks_with_keys().unwrap();

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].0.to_string(), "child::app:build");
        assert_eq!(tasks[1].0.to_string(), "primary::app:build");
        assert_eq!(graph.get_tasks().unwrap().len(), 1);
        assert_eq!(
            graph
                .get_task_by_key(&TaskKey::primary(Id::raw("app"), Id::raw("build")).unwrap())
                .unwrap()
                .target,
            Target::new("app", "build").unwrap()
        );
    }

    #[test]
    fn aggregate_task_queries_resolve_source_local_project_aliases() {
        let graph = aggregate_graph();
        let tasks = graph
            .query_tasks_with_keys(moon_query::build_query("project=child-app").unwrap())
            .unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0.to_string(), "child::app:build");

        let duplicates = graph
            .query_tasks_with_keys(moon_query::build_query("task=build").unwrap())
            .unwrap();
        assert_eq!(duplicates.len(), 2);
        assert_ne!(duplicates[0].0, duplicates[1].0);

        let primary = graph
            .query_tasks(moon_query::build_query("task=build").unwrap())
            .unwrap();
        assert_eq!(primary.len(), 1);
    }

    #[test]
    fn aggregate_project_queries_preserve_keys_and_use_source_local_tasks() {
        let graph = aggregate_graph();
        let duplicates = graph
            .query_projects_with_keys(moon_query::build_query("project=app").unwrap())
            .unwrap();

        assert_eq!(duplicates.len(), 2);
        assert_eq!(duplicates[0].0.to_string(), "child::app");
        assert_eq!(duplicates[1].0.to_string(), "primary::app");

        let primary = graph
            .query_projects(moon_query::build_query("project=app").unwrap())
            .unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].source_id.as_str(), "primary");

        let by_tag = graph
            .query_projects_with_keys(moon_query::build_query("taskTag=child-tag").unwrap())
            .unwrap();
        assert_eq!(by_tag.len(), 1);
        assert_eq!(by_tag[0].0.to_string(), "child::app");

        let by_toolchain = graph
            .query_projects_with_keys(
                moon_query::build_query("taskToolchain=child-toolchain").unwrap(),
            )
            .unwrap();
        assert_eq!(by_toolchain.len(), 1);
        assert_eq!(by_toolchain[0].0.to_string(), "child::app");

        let by_type = graph
            .query_projects_with_keys(moon_query::build_query("taskType=run").unwrap())
            .unwrap();
        assert_eq!(by_type.len(), 1);
        assert_eq!(by_type[0].0.to_string(), "child::app");
    }
}
