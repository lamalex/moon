use moon_app_context::{AppContext, SourceRuntime, SourceRuntimeRegistry};
use moon_common::{
    Id, SourceAlias, SourcePathBuf, SourceRegistry, SourceRootId, path::WorkspaceRelativePathBuf,
};
use moon_config::{
    DependencyScope, GlobPath, HasherConfig, HasherWalkStrategy, PortablePath,
    ProjectDependencyConfig, TaskDependencyCacheStrategy, TaskDependencyConfig,
};
use moon_hash::ContentHasher;
use moon_project::Project;
use moon_project_graph::{ProjectGraph, ProjectNode};
use moon_task::{ProjectKey, Task, TaskKey};
use moon_task_graph::{GraphExpanderContext, TaskGraph, TaskNode};
use moon_task_hasher::{TaskFingerprint, TaskHasher};
use moon_test_utils::{WorkspaceGraph, WorkspaceMocker};
use petgraph::graph::{DiGraph, NodeIndex};
use starbase_sandbox::create_sandbox;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn create_out_files(project_root: &Path) {
    let out_dir = project_root.join("out");

    fs::create_dir_all(&out_dir).unwrap();

    for i in 1..=5 {
        fs::write(out_dir.join(i.to_string()), i.to_string()).unwrap();
    }
}

fn create_hasher_configs() -> (HasherConfig, HasherConfig) {
    (
        HasherConfig {
            walk_strategy: HasherWalkStrategy::Vcs,
            ..HasherConfig::default()
        },
        HasherConfig {
            walk_strategy: HasherWalkStrategy::Glob,
            ..HasherConfig::default()
        },
    )
}

async fn mock_workspace(workspace_root: &Path) -> (WorkspaceGraph, AppContext) {
    create_out_files(workspace_root);

    let mock = WorkspaceMocker::new(workspace_root)
        .load_default_configs()
        .with_default_projects()
        .with_all_toolchains()
        .with_inherited_tasks()
        .with_global_envs();

    (mock.mock_workspace_graph().await, mock.mock_app_context())
}

async fn generate_hash<'a>(
    project: &'a Project,
    task: &'a Task,
    _wg: &'a WorkspaceGraph,
    app: &'a AppContext,
    config: &'a HasherConfig,
) -> TaskFingerprint<'a> {
    let mut hasher = TaskHasher::new(app, project, task, config);
    hasher.hash_inputs().await.unwrap();
    hasher.hash()
}

fn get_input_files(mut inputs: BTreeMap<SourcePathBuf, String>) -> Vec<WorkspaceRelativePathBuf> {
    inputs.remove(&SourcePathBuf::primary(".moon/cache/CACHEDIR.TAG"));
    inputs
        .into_keys()
        .map(|input| input.path)
        .collect::<Vec<_>>()
}

fn create_canonical_output_graph(
    primary_root: &Path,
    child_root: &Path,
) -> (TaskGraph, Project, Task, TaskKey) {
    let primary_id = SourceRootId::primary();
    let child_id = SourceRootId::new("child").unwrap();
    let primary_key = ProjectKey::new(primary_id.clone(), Id::raw("app")).unwrap();
    let child_key = ProjectKey::new(child_id.clone(), Id::raw("lib")).unwrap();
    let mut sources = SourceRegistry::new(primary_id.clone(), primary_root.to_path_buf());
    sources
        .register(child_id.clone(), child_root.to_path_buf())
        .unwrap();
    let aggregate_context = GraphExpanderContext {
        sources: Arc::new(sources),
        workspace_root: primary_root.to_path_buf(),
        working_dir: primary_root.to_path_buf(),
        ..GraphExpanderContext::default()
    };
    let primary_project = Project {
        id: primary_key.project_id().clone(),
        source: ".".into(),
        source_id: primary_id.clone(),
        ..Project::default()
    };
    let child_project = Project {
        id: child_key.project_id().clone(),
        source: ".".into(),
        source_id: child_id.clone(),
        ..Project::default()
    };
    let mut projects = ProjectGraph::new(aggregate_context);

    for (index, project) in [primary_project.clone(), child_project]
        .into_iter()
        .enumerate()
    {
        let index = NodeIndex::new(index);
        projects.indexes.insert(index, project.key());
        projects
            .nodes
            .insert(project.key(), ProjectNode { index, project });
    }
    let mut project_edges = DiGraph::new();
    project_edges.add_node(NodeIndex::new(0));
    project_edges.add_node(NodeIndex::new(1));
    project_edges.add_edge(NodeIndex::new(0), NodeIndex::new(1), DependencyScope::Build);
    projects.set_graph(project_edges).unwrap();
    let projects = Arc::new(projects);

    let owner = TaskKey::new(primary_key, Id::raw("consume")).unwrap();
    let consumer = Task {
        configured_deps: vec![TaskDependencyConfig {
            cache_strategy: Some(TaskDependencyCacheStrategy::Outputs),
            ..TaskDependencyConfig::new(moon_task::Target::new("^", "build").unwrap())
        }],
        id: Id::raw("consume"),
        output_files: [(
            WorkspaceRelativePathBuf::from("same.txt"),
            Default::default(),
        )]
        .into_iter()
        .collect(),
        source_id: primary_id.clone(),
        target: moon_task::Target::new("app", "consume").unwrap(),
        ..Task::default()
    };
    let producer = Task {
        id: Id::raw("build"),
        output_files: [
            (
                WorkspaceRelativePathBuf::from("same.txt"),
                Default::default(),
            ),
            (
                WorkspaceRelativePathBuf::from("ignored.txt"),
                Default::default(),
            ),
        ]
        .into_iter()
        .collect(),
        source_id: child_id.clone(),
        target: moon_task::Target::new("lib", "build").unwrap(),
        ..Task::default()
    };
    let local_graph = |source_id: SourceRootId, root: &Path, task: Task| {
        let context = GraphExpanderContext {
            sources: Arc::new(SourceRegistry::new(source_id, root.to_path_buf())),
            workspace_root: root.to_path_buf(),
            working_dir: root.to_path_buf(),
            ..GraphExpanderContext::default()
        };
        let mut graph = TaskGraph::new(context, Arc::clone(&projects));
        let index = graph.graph.add_node(NodeIndex::new(0));
        let key = task.key();
        graph.indexes.insert(index, key.clone());
        graph.nodes.insert(key, TaskNode { index, task });

        Arc::new(graph)
    };
    let graph = TaskGraph::compose(
        Arc::clone(&projects),
        [
            local_graph(primary_id, primary_root, consumer.clone()),
            local_graph(child_id, child_root, producer),
        ],
    )
    .unwrap();

    (graph, primary_project, consumer, owner)
}

mod task_hasher {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn hashes_cross_source_outputs_despite_consumer_output_overlap() {
        let primary = create_sandbox("inputs");
        let child = create_sandbox("inputs");
        fs::write(primary.path().join("same.txt"), "primary").unwrap();
        fs::write(child.path().join("same.txt"), "child").unwrap();
        let primary_mock = WorkspaceMocker::new(primary.path()).load_default_configs();
        let child_mock = WorkspaceMocker::new(child.path()).load_default_configs();
        let mut primary_app = primary_mock.mock_app_context();
        Arc::make_mut(&mut primary_app.workspace_config)
            .experiments
            .native_file_hashing = true;
        let primary_app = Arc::new(primary_app);
        let mut child_app = child_mock.mock_app_context();
        child_app.source_id = SourceRootId::new("child").unwrap();
        Arc::make_mut(&mut child_app.workspace_config)
            .experiments
            .native_file_hashing = true;
        let child_app = Arc::new(child_app);
        let registry = SourceRuntimeRegistry::new(
            Arc::clone(&primary_app),
            [(
                SourceRootId::new("child").unwrap(),
                SourceRuntime::Available(Arc::clone(&child_app)),
            )],
        )
        .unwrap();
        let (graph, project, task, owner) =
            create_canonical_output_graph(primary.path(), child.path());
        let config = HasherConfig::default();
        let mut hasher = TaskHasher::new(&primary_app, &project, &task, &config);
        hasher
            .hash_resolved_dependency_outputs(&registry, &graph, &owner)
            .await
            .unwrap();
        let first = hasher.hash().inputs;
        let child_path = SourcePathBuf::new(
            SourceRootId::new("child").unwrap(),
            WorkspaceRelativePathBuf::from("same.txt"),
        );

        assert_eq!(first.len(), 1);
        assert_eq!(
            first[&child_path],
            child_app
                .hash_files(&[WorkspaceRelativePathBuf::from("same.txt")])
                .await
                .unwrap()[&WorkspaceRelativePathBuf::from("same.txt")]
        );

        fs::write(primary.path().join("same.txt"), "primary changed").unwrap();
        let mut hasher = TaskHasher::new(&primary_app, &project, &task, &config);
        hasher
            .hash_resolved_dependency_outputs(&registry, &graph, &owner)
            .await
            .unwrap();

        assert_eq!(hasher.hash().inputs, first);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn applies_ignore_patterns_to_cross_source_dependency_outputs() {
        let primary = create_sandbox("inputs");
        let child = create_sandbox("inputs");
        fs::write(child.path().join("same.txt"), "child").unwrap();
        fs::write(child.path().join("ignored.txt"), "ignored").unwrap();
        let primary_app = Arc::new(
            WorkspaceMocker::new(primary.path())
                .load_default_configs()
                .mock_app_context(),
        );
        let mut child_app = WorkspaceMocker::new(child.path())
            .load_default_configs()
            .mock_app_context();
        child_app.source_id = SourceRootId::new("child").unwrap();
        let registry = SourceRuntimeRegistry::new(
            Arc::clone(&primary_app),
            [(
                SourceRootId::new("child").unwrap(),
                SourceRuntime::Available(Arc::new(child_app)),
            )],
        )
        .unwrap();
        let (graph, project, task, owner) =
            create_canonical_output_graph(primary.path(), child.path());
        let config = HasherConfig {
            ignore_patterns: vec![GlobPath::parse("**/ignored.txt").unwrap()],
            ..HasherConfig::default()
        };
        let mut hasher = TaskHasher::new(&primary_app, &project, &task, &config);

        hasher
            .hash_resolved_dependency_outputs(&registry, &graph, &owner)
            .await
            .unwrap();

        let inputs = hasher.hash().inputs;
        assert_eq!(inputs.len(), 1);
        assert!(inputs.contains_key(&SourcePathBuf::new(
            SourceRootId::new("child").unwrap(),
            WorkspaceRelativePathBuf::from("same.txt"),
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_unknown_and_unavailable_dependency_output_sources() {
        let primary = create_sandbox("inputs");
        let child = create_sandbox("inputs");
        fs::write(child.path().join("same.txt"), "child").unwrap();
        let mut primary_app = WorkspaceMocker::new(primary.path())
            .load_default_configs()
            .mock_app_context();
        Arc::make_mut(&mut primary_app.workspace_config)
            .experiments
            .native_file_hashing = true;
        let primary_app = Arc::new(primary_app);
        let (graph, project, task, owner) =
            create_canonical_output_graph(primary.path(), child.path());
        let config = HasherConfig::default();

        let mut hasher = TaskHasher::new(&primary_app, &project, &task, &config);
        let unknown = hasher
            .hash_resolved_dependency_outputs(
                &SourceRuntimeRegistry::single(Arc::clone(&primary_app)),
                &graph,
                &owner,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("not registered"));

        let unavailable = SourceRuntimeRegistry::new(
            Arc::clone(&primary_app),
            [(
                SourceRootId::new("child").unwrap(),
                SourceRuntime::Unavailable("checkout failed".into()),
            )],
        )
        .unwrap();
        let mut hasher = TaskHasher::new(&primary_app, &project, &task, &config);
        let error = hasher
            .hash_resolved_dependency_outputs(&unavailable, &graph, &owner)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("checkout failed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fingerprints_use_canonical_source_qualified_identity() {
        let sandbox = create_sandbox("inputs");
        sandbox.enable_git();

        let (wg, mut app_a) = mock_workspace(sandbox.path()).await;
        let (_, mut app_b) = mock_workspace(sandbox.path()).await;
        let project = wg.get_project("root").unwrap();
        let task = wg.get_task_from_project("root", "files").unwrap();
        let source_a = SourceRootId::new("source-a").unwrap();
        let source_b = SourceRootId::new("source-b").unwrap();
        let source_alias = SourceAlias::new("checkout-alias").unwrap();

        let mut project_a = project.as_ref().clone();
        project_a.source_id = source_a.clone();
        project_a.dependencies = vec![
            ProjectDependencyConfig::new(Id::raw("local")),
            ProjectDependencyConfig {
                source_root: Some(source_b.clone()),
                ..ProjectDependencyConfig::new(Id::raw("remote"))
            },
        ];
        let mut task_a = task.as_ref().clone();
        task_a.source_id = source_a.clone();

        let mut project_b = project_a.clone();
        project_b.source_id = source_b.clone();
        let mut task_b = task_a.clone();
        task_b.source_id = source_b.clone();
        app_a.source_id = source_a.clone();
        app_b.source_id = source_b.clone();

        let config = HasherConfig::default();
        let mut hasher_a = TaskHasher::new(&app_a, &project_a, &task_a, &config);
        hasher_a.hash_inputs().await.unwrap();
        let fingerprint_a = hasher_a.hash();

        let mut hasher_b = TaskHasher::new(&app_b, &project_b, &task_b, &config);
        hasher_b.hash_inputs().await.unwrap();
        let fingerprint_b = hasher_b.hash();

        assert_ne!(fingerprint_a.target, fingerprint_b.target);
        assert_ne!(fingerprint_a.inputs, fingerprint_b.inputs);
        assert_eq!(
            fingerprint_a
                .inputs
                .iter()
                .map(|(input, hash)| (&input.path, hash))
                .collect::<Vec<_>>(),
            fingerprint_b
                .inputs
                .iter()
                .map(|(input, hash)| (&input.path, hash))
                .collect::<Vec<_>>()
        );
        assert_eq!(fingerprint_a.version, "4");
        assert_eq!(
            fingerprint_a.project_deps,
            [
                ProjectKey::new(source_a, Id::raw("local")).unwrap(),
                ProjectKey::new(source_b.clone(), Id::raw("remote")).unwrap(),
            ]
        );

        let mut content_a = ContentHasher::new("source-a");
        content_a.hash_content(&fingerprint_a).unwrap();
        let mut content_b = ContentHasher::new("source-b");
        content_b.hash_content(&fingerprint_b).unwrap();
        assert_ne!(
            content_a.generate_hash().unwrap(),
            content_b.generate_hash().unwrap()
        );

        let serialized = serde_json::to_string(&fingerprint_a).unwrap();
        assert!(serialized.contains("source-a::root:files"));
        assert!(serialized.contains("source-a::local"));
        assert!(serialized.contains("source-b::remote"));
        assert!(serialized.contains("source-a::2.txt"));
        assert!(!serialized.contains(source_alias.as_str()));
        assert!(!serialized.contains(sandbox.path().to_string_lossy().as_ref()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_hashing_project_sources_with_a_mismatched_runtime() {
        let sandbox = create_sandbox("inputs");
        sandbox.enable_git();

        let (wg, app) = mock_workspace(sandbox.path()).await;
        let project = wg.get_project("root").unwrap();
        let mut task = wg
            .get_task_from_project("root", "files")
            .unwrap()
            .as_ref()
            .clone();
        task.source_id = SourceRootId::new("other").unwrap();
        let config = HasherConfig::default();
        let mut hasher = TaskHasher::new(&app, &project, &task, &config);

        let error = hasher.hash_project_sources().await.unwrap_err().to_string();

        assert!(error.contains("Cannot hash inputs"), "{error}");
        assert!(error.contains("other"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filters_out_files_matching_ignore_pattern() {
        let sandbox = create_sandbox("ignore-patterns");
        sandbox.enable_git();

        let (wg, app) = mock_workspace(sandbox.path()).await;
        let project = wg.get_project("root").unwrap();
        let task = wg.get_task_from_project("root", "testPatterns").unwrap();

        let hasher_config = HasherConfig {
            ignore_patterns: vec![GlobPath::parse("**/out/**").unwrap()],
            ..HasherConfig::default()
        };

        let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

        assert_eq!(
            get_input_files(result.inputs),
            [".gitignore", "package.json"]
        );
    }

    mod input_aggregation {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_files() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "files").unwrap();

            let expected = ["2.txt", "dir/abc.txt"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_dirs() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "dirs").unwrap();

            let expected = ["dir/abc.txt", "dir/az.txt", "dir/xyz.txt"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_globs_star() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "globStar").unwrap();

            let expected = ["1.txt", "2.txt", "3.txt"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_globs_nested_star() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "globNestedStar").unwrap();

            let expected = [
                "1.txt",
                "2.txt",
                "3.txt",
                "dir/abc.txt",
                "dir/az.txt",
                "dir/xyz.txt",
            ];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_globs_groups() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "globGroup").unwrap();

            let expected = ["dir/az.txt", "dir/xyz.txt"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn excludes_glob_negations() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "globNegated").unwrap();

            let expected = ["2.txt", "dir/abc.txt", "dir/xyz.txt"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_none() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "none").unwrap();

            let hasher_config = HasherConfig::default();

            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), Vec::<&str>::new());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_local_changed_files() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();
            sandbox.create_file("created.txt", "");
            sandbox.create_file("filtered.txt", "");
            sandbox.run_git(|cmd| {
                cmd.args(["add", "created.txt", "filtered.txt"]);
            });

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "changed").unwrap();

            let hasher_config = HasherConfig::default();

            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), ["created.txt"]);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn excludes_unrelated_local_changed_files_for_explicit_inputs() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();
            sandbox.create_file("created.txt", "");
            sandbox.run_git(|cmd| {
                cmd.args(["add", "created.txt"]);
            });

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "files").unwrap();

            let hasher_config = HasherConfig::default();
            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), ["2.txt", "dir/abc.txt"]);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_env_file() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();
            sandbox.create_file(".env", "");

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "envFile").unwrap();

            let expected = [".env"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn includes_custom_env_files() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();
            sandbox.create_file(".env.prod", "");
            sandbox.create_file(".env.local", "");

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "envFileList").unwrap();

            let expected = [".env.local", ".env.prod"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_moon_project_config() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "moonConfig").unwrap();

            let hasher_config = HasherConfig::default();
            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), ["moon.yml"]);
        }

        #[tokio::test(flavor = "multi_thread")]
        #[should_panic(expected = "task_hasher::missing_input_file")]
        async fn errors_if_optional_false_and_file_missing() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, _) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "filesRequired").unwrap();

            generate_hash(&project, &task, &wg, &app, &vcs_config).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn doesnt_error_if_optional_true_and_file_missing() {
            let sandbox = create_sandbox("inputs");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, _) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "filesOptional").unwrap();

            let _ = generate_hash(&project, &task, &wg, &app, &vcs_config).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_external_project() {
            let sandbox = create_sandbox("projects");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("inputs").unwrap();
            let task = wg.get_task_from_project("inputs", "project").unwrap();

            let hasher_config = HasherConfig::default();
            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(
                get_input_files(result.inputs),
                [
                    "external/data.json",
                    "external/docs.md",
                    "external/moon.yml"
                ]
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_external_project_using_file_group() {
            let sandbox = create_sandbox("projects");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("inputs").unwrap();
            let task = wg.get_task_from_project("inputs", "projectGroup").unwrap();

            let hasher_config = HasherConfig::default();
            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), ["external/docs.md"]);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_external_project_using_filter_globs() {
            let sandbox = create_sandbox("projects");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let project = wg.get_project("inputs").unwrap();
            let task = wg.get_task_from_project("inputs", "projectFilter").unwrap();

            let hasher_config = HasherConfig::default();
            let result = generate_hash(&project, &task, &wg, &app, &hasher_config).await;

            assert_eq!(get_input_files(result.inputs), ["external/data.json"]);
        }

        // The consumer `lib` has a source path (`lib`) that is a string prefix
        // of the dependency `lib-extra`'s source path, without a path-segment
        // boundary. The VCS walk strategy used to filter the dependency's input
        // glob out via a raw string `starts_with`, silently dropping all of its
        // files from the hash. Both walk strategies must include them.
        // See moonrepo/moon#2554.
        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_dependency_with_prefixed_source_via_scope() {
            let sandbox = create_sandbox("projects");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("lib").unwrap();
            let task = wg.get_task_from_project("lib", "prefixedDeps").unwrap();

            let expected = [
                "lib-extra/data.json",
                "lib-extra/index.ts",
                "lib-extra/moon.yml",
            ];

            // VCS (regressed before the fix: returned zero dependency files)
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn can_include_external_project_with_prefixed_source() {
            let sandbox = create_sandbox("projects");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("lib").unwrap();
            let task = wg.get_task_from_project("lib", "prefixedProject").unwrap();

            let expected = [
                "lib-extra/data.json",
                "lib-extra/index.ts",
                "lib-extra/moon.yml",
            ];

            // VCS (regressed before the fix: returned zero dependency files)
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }
    }

    mod output_filtering {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn input_file_output_file() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inFileOutFile").unwrap();

            let expected = [
                ".moon/toolchains.yml",
                ".moon/workspace.yml",
                "out/1",
                "out/3",
            ];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn input_file_output_dir() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inFileOutDir").unwrap();

            let expected = [".moon/toolchains.yml", ".moon/workspace.yml"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn input_file_output_glob() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inFileOutGlob").unwrap();

            let expected = [".moon/toolchains.yml", ".moon/workspace.yml"];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn input_glob_output_file() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inGlobOutFile").unwrap();

            let expected = [
                ".gitignore",
                ".moon/toolchains.yml",
                ".moon/workspace.yml",
                "out/1",
                "out/3",
                "out/5",
                "package.json",
            ];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn input_glob_output_dir() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inGlobOutDir").unwrap();

            let expected = [
                ".gitignore",
                ".moon/toolchains.yml",
                ".moon/workspace.yml",
                "package.json",
            ];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn input_glob_output_glob() {
            let sandbox = create_sandbox("output-filters");
            sandbox.enable_git();

            let (wg, app) = mock_workspace(sandbox.path()).await;
            let (vcs_config, glob_config) = create_hasher_configs();
            let project = wg.get_project("root").unwrap();
            let task = wg.get_task_from_project("root", "inGlobOutGlob").unwrap();

            let expected = [
                ".gitignore",
                ".moon/toolchains.yml",
                ".moon/workspace.yml",
                "package.json",
            ];

            // VCS
            let result = generate_hash(&project, &task, &wg, &app, &vcs_config).await;

            assert_eq!(get_input_files(result.inputs), expected);

            // Glob
            let result = generate_hash(&project, &task, &wg, &app, &glob_config).await;

            assert_eq!(get_input_files(result.inputs), expected);
        }
    }
}
