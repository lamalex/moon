mod utils;

use moon_test_utils::{create_empty_moon_sandbox, predicates::prelude::*};
use std::{fs, path::Path};

mod sources {
    use super::*;

    fn create_sources_sandbox() -> moon_test_utils::MoonSandbox {
        let sandbox = create_empty_moon_sandbox();
        sandbox.create_file(
            ".moon/workspace.yml",
            r"
id: acme/platform
projects:
  - apps/*
workspaces:
  frontend:
    path: web
",
        );
        sandbox.create_file(
            "web/.moon/workspace.yml",
            r"
id: acme/web
projects:
  - apps/*
",
        );
        sandbox.create_file(
            "apps/app/moon.yml",
            r"
owners:
  defaultOwner: '@primary-owner'
  paths:
    - '**/*'

dependsOn:
  - id: lib
    sourceRoot: frontend

tasks:
  build:
    command: bash
    args: primary-build.sh
    deps:
      - target: ^:build
        cacheStrategy: outputs
    inputs:
      - primary-build.sh
    outputs:
      - dist/app.txt
",
        );
        sandbox.create_file(
            "apps/app/primary-build.sh",
            r#"set -eu
test -f ../../web/apps/lib/dist/lib.txt
mkdir -p dist
cp ../../web/apps/lib/dist/lib.txt dist/app.txt
printf 'primary\n' >> ../../execution.log
"#,
        );
        sandbox.create_file(
            "web/apps/app/moon.yml",
            r"
owners:
  defaultOwner: '@child-owner'
  paths:
    - '**/*'

tasks:
  build:
    command: bash
    args: child-build.sh
    inputs:
      - child-build.sh
      - file: changed.txt
        content: child
    outputs:
      - dist/child-app.txt
",
        );
        sandbox.create_file(
            "web/apps/app/child-build.sh",
            r#"set -eu
mkdir -p dist
printf 'duplicate child app\n' > dist/child-app.txt
printf 'child-app\n' >> ../../../execution.log
"#,
        );
        sandbox.create_file(
            "web/apps/lib/moon.yml",
            r"
tasks:
  build:
    command: bash
    args: build.sh
    inputs:
      - build.sh
      - input.txt
    outputs:
      - dist/lib.txt
      - dist/cwd.txt
",
        );
        sandbox.create_file(
            "web/apps/lib/build.sh",
            r#"set -eu
mkdir -p dist
cp input.txt dist/lib.txt
pwd > dist/cwd.txt
printf 'child-lib\n' >> ../../../execution.log
"#,
        );
        sandbox.create_file("web/apps/lib/input.txt", "child v1\n");
        sandbox
    }

    fn create_codeowners_sources_sandbox() -> moon_test_utils::MoonSandbox {
        let sandbox = create_sources_sandbox();
        sandbox.create_file(
            ".moon/workspace.yml",
            r"
id: acme/platform
projects:
  - apps/*
workspaces:
  frontend:
    path: web
codeowners:
  sync: true
",
        );
        sandbox.create_file(
            "web/.moon/workspace.yml",
            r"
id: acme/web
projects:
  - apps/*
codeowners:
  sync: true
",
        );
        sandbox
    }

    fn read(path: impl AsRef<Path>) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn read_task_hash(path: impl AsRef<Path>) -> String {
        let state = read(path);
        let hash = state
            .split_once("\"hash\"")
            .unwrap()
            .1
            .split_once(':')
            .unwrap()
            .1
            .trim_start();

        hash.trim_start_matches('"')
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn lists_canonical_sources_and_aliases_as_json() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("sources").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"id\": \"acme/platform\""))
            .stdout(predicate::str::contains("\"id\": \"acme/web\""))
            .stdout(predicate::str::contains("\"frontend\""))
            .stdout(predicate::str::contains("\"status\": \"ready\""));
    }

    #[test]
    fn aggregates_duplicate_projects_with_canonical_identities() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("projects").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"sourceId\": \"acme/platform\""))
            .stdout(predicate::str::contains("\"sourceId\": \"acme/web\""));
    }

    #[test]
    fn primary_codeowners_only_include_primary_source_projects() {
        let sandbox = create_codeowners_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        let codeowners = read(sandbox.path().join(".github/CODEOWNERS"));

        assert!(codeowners.contains("@primary-owner"));
        assert!(!codeowners.contains("@child-owner"));
    }

    #[test]
    fn child_codeowners_only_include_child_source_projects() {
        let sandbox = create_codeowners_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        let codeowners = read(sandbox.path().join("web/.github/CODEOWNERS"));

        assert!(codeowners.contains("@child-owner"));
        assert!(!codeowners.contains("@primary-owner"));
    }

    #[test]
    fn aggregate_affected_queries_filter_by_owning_source() {
        let sandbox = create_sources_sandbox();
        sandbox.enable_git();
        sandbox.create_file("web/apps/app/changed.txt", "child change");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("query").arg("projects").arg("--affected");
            })
            .success()
            .stdout(predicate::str::contains("\"acme/web::app\""))
            .stdout(predicate::str::contains("\"acme/platform::app\"").not());

        sandbox
            .run_bin(|cmd| {
                cmd.arg("query").arg("tasks").arg("--affected");
            })
            .success()
            .stdout(predicate::str::contains("\"acme/web::app:build\""))
            .stdout(predicate::str::contains("\"acme/platform::app:build\"").not());
    }

    #[test]
    fn local_all_scope_only_selects_primary_source_tasks() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg(":build");
            })
            .success();

        assert_eq!(
            read(sandbox.path().join("execution.log")),
            "child-lib\nprimary\n"
        );
        assert!(sandbox.path().join("apps/app/dist/app.txt").exists());
        assert!(sandbox.path().join("web/apps/lib/dist/lib.txt").exists());
        assert!(
            !sandbox
                .path()
                .join("web/apps/app/dist/child-app.txt")
                .exists()
        );
    }

    #[test]
    fn aggregate_all_scope_selects_primary_and_child_source_tasks() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("::build");
            })
            .success();

        let lines = read(sandbox.path().join("execution.log"))
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines.iter().filter(|line| *line == "primary").count(), 1);
        assert_eq!(lines.iter().filter(|line| *line == "child-app").count(), 1);
        assert_eq!(lines.iter().filter(|line| *line == "child-lib").count(), 1);
        assert!(
            lines.iter().position(|line| line == "child-lib")
                < lines.iter().position(|line| line == "primary")
        );
        assert!(sandbox.path().join("apps/app/dist/app.txt").exists());
        assert!(
            sandbox
                .path()
                .join("web/apps/app/dist/child-app.txt")
                .exists()
        );
        assert!(sandbox.path().join("web/apps/lib/dist/lib.txt").exists());
    }

    #[test]
    fn source_alias_scope_selects_only_child_source_tasks() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("frontend::build");
            })
            .success();

        let mut lines = read(sandbox.path().join("execution.log"))
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.sort();

        assert_eq!(lines, ["child-app", "child-lib"]);
        assert!(!sandbox.path().join("apps/app/dist/app.txt").exists());
    }

    #[test]
    fn canonical_source_and_project_scope_selects_exact_child_task() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("acme/web::app:build");
            })
            .success();

        assert_eq!(read(sandbox.path().join("execution.log")), "child-app\n");
        assert!(
            sandbox
                .path()
                .join("web/apps/app/dist/child-app.txt")
                .exists()
        );
        assert!(!sandbox.path().join("apps/app/dist/app.txt").exists());
        assert!(!sandbox.path().join("web/apps/lib/dist/lib.txt").exists());
    }

    #[test]
    fn aggregate_affected_execution_preserves_source_identity() {
        let sandbox = create_sources_sandbox();
        sandbox.enable_git();
        sandbox.create_file("web/apps/app/changed.txt", "child change");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("::build").arg("--affected");
            })
            .success();

        assert_eq!(read(sandbox.path().join("execution.log")), "child-app\n");
        assert!(
            sandbox
                .path()
                .join("web/apps/app/dist/child-app.txt")
                .exists()
        );
        assert!(!sandbox.path().join("apps/app/dist/app.txt").exists());
        assert!(!sandbox.path().join("web/apps/lib/dist/lib.txt").exists());
    }

    #[test]
    fn aggregate_failure_terminates_running_sibling_tasks() {
        let sandbox = create_sources_sandbox();
        sandbox.create_file("apps/app/primary-build.sh", "set -eu\nexit 1\n");
        sandbox.create_file(
            "web/apps/app/child-build.sh",
            "set -eu\nsleep 30\nprintf 'child-app\\n' >> ../../../execution.log\n",
        );
        let start = std::time::Instant::now();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("::build");
            })
            .failure();

        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "aggregate failure did not terminate running sibling tasks"
        );
    }

    #[test]
    fn aggregate_failure_returns_after_other_tasks_are_cached() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("::build");
            })
            .success();

        sandbox.create_file("web/apps/app/child-build.sh", "set -eu\nexit 1\n");
        let start = std::time::Instant::now();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("::build");
            })
            .failure();

        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "aggregate failure did not return after cached sibling tasks"
        );
    }

    #[test]
    fn aggregates_duplicate_tasks_without_changing_positional_resolution() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("tasks").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("acme/platform::app:build"))
            .stdout(predicate::str::contains("acme/web::app:build"));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("tasks").arg("app").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("primary-build.sh"))
            .stdout(predicate::str::contains("child-build.sh").not());
    }

    #[test]
    fn aggregate_queries_preserve_duplicate_project_and_task_keys() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("query").arg("projects").arg("project=app");
            })
            .success()
            .stdout(predicate::str::contains("acme/platform::app"))
            .stdout(predicate::str::contains("acme/web::app"));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("query").arg("tasks").arg("task=build");
            })
            .success()
            .stdout(predicate::str::contains("acme/platform::app:build"))
            .stdout(predicate::str::contains("acme/web::app:build"));
    }

    #[test]
    fn resolves_and_executes_cross_source_dependencies() {
        let sandbox = create_sources_sandbox();
        sandbox.enable_git();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("query").arg("projects").arg("project=app");
            })
            .success()
            .stdout(predicate::str::contains("\"id\": \"lib\""))
            .stdout(predicate::str::contains("\"sourceRoot\": \"acme/web\""));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("project-graph").arg("acme/web::lib").arg("--dot");
            })
            .success()
            .stdout(predicate::str::contains("lib"))
            .stdout(predicate::str::contains("acme/platform::app").not());

        sandbox
            .run_bin(|cmd| {
                cmd.arg("project-graph")
                    .arg("acme/platform::app")
                    .arg("--dot");
            })
            .success()
            .stdout(predicate::str::contains("label=\"app\""))
            .stdout(predicate::str::contains("lib"))
            .stdout(predicate::str::contains("->"));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        let root = sandbox.path();
        let primary_output = root.join("apps/app/dist/app.txt");
        let child_output = root.join("web/apps/lib/dist/lib.txt");
        let child_cwd = root.join("web/apps/lib/dist/cwd.txt");
        let primary_state =
            root.join(".moon/cache/states/tasks/acme-platform/app/build/lastRun.json");
        let child_state = root.join("web/.moon/cache/states/tasks/acme-web/lib/build/lastRun.json");

        assert_eq!(read(root.join("execution.log")), "child-lib\nprimary\n");
        assert_eq!(read(&primary_output), "child v1\n");
        assert_eq!(read(&child_output), "child v1\n");
        assert_eq!(
            fs::canonicalize(read(&child_cwd).trim()).unwrap(),
            fs::canonicalize(root.join("web/apps/lib")).unwrap()
        );
        assert!(!root.join("web/apps/app/dist/child-app.txt").exists());
        assert!(primary_state.exists());
        assert!(child_state.exists());
        assert_ne!(primary_state, child_state);

        let primary_hash = read_task_hash(&primary_state);
        let child_hash = read_task_hash(&child_state);
        let primary_cache = root.join(format!(".moon/cache/outputs/{primary_hash}.tar.gz"));
        let child_cache = root.join(format!("web/.moon/cache/outputs/{child_hash}.tar.gz"));

        assert!(primary_cache.exists());
        assert!(child_cache.exists());
        assert_ne!(primary_cache, child_cache);

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        assert_eq!(read(root.join("execution.log")), "child-lib\nprimary\n");
        assert_eq!(read_task_hash(&primary_state), primary_hash);
        assert_eq!(read_task_hash(&child_state), child_hash);

        fs::remove_file(&child_output).unwrap();
        assert!(!child_output.exists());

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        assert_eq!(read(&child_output), "child v1\n");
        assert_eq!(read(root.join("execution.log")), "child-lib\nprimary\n");
        assert_eq!(read_task_hash(&primary_state), primary_hash);
        assert_eq!(read_task_hash(&child_state), child_hash);
    }

    #[test]
    fn runs_affected_primary_dependent_and_cross_source_dependency() {
        let sandbox = create_sources_sandbox();
        sandbox.enable_git();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        fs::write(sandbox.path().join("execution.log"), "").unwrap();
        sandbox.create_file("web/apps/lib/input.txt", "child v2\n");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run")
                    .arg("app:build")
                    .arg("--affected")
                    .arg("--include-relations")
                    .args(["--dependents", "deep"]);
            })
            .success();

        assert_eq!(
            read(sandbox.path().join("execution.log")),
            "child-lib\nprimary\n"
        );
        assert_eq!(
            read(sandbox.path().join("apps/app/dist/app.txt")),
            "child v2\n"
        );
        assert!(
            !sandbox
                .path()
                .join("web/apps/app/dist/child-app.txt")
                .exists()
        );
    }

    #[test]
    fn cross_source_dependency_outputs_invalidate_primary_hash() {
        let sandbox = create_sources_sandbox();
        sandbox.enable_git();
        let primary_state = sandbox
            .path()
            .join(".moon/cache/states/tasks/acme-platform/app/build/lastRun.json");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        let initial_hash = read_task_hash(&primary_state);
        sandbox.create_file("web/apps/lib/input.txt", "child v2\n");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("run").arg("app:build");
            })
            .success();

        assert_ne!(read_task_hash(primary_state), initial_hash);
        assert_eq!(
            read(sandbox.path().join("apps/app/dist/app.txt")),
            "child v2\n"
        );
        assert_eq!(
            read(sandbox.path().join("execution.log")),
            "child-lib\nprimary\nchild-lib\nprimary\n"
        );
    }

    #[test]
    fn project_graph_qualifies_ambiguous_ids() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("project-graph").arg("--dot");
            })
            .success()
            .stdout(predicate::str::contains("acme/platform::app"))
            .stdout(predicate::str::contains("acme/web::app"));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("project-graph").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"sourceId\": \"acme/platform\""))
            .stdout(predicate::str::contains("\"sourceId\": \"acme/web\""));
    }

    #[test]
    fn reports_source_local_configuration_failures() {
        let sandbox = create_sources_sandbox();
        sandbox.create_file(
            "web/.moon/extensions.yml",
            r"
broken:
  config: true
",
        );

        sandbox
            .run_bin(|cmd| {
                cmd.arg("sources").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"stage\": \"extensions-config\""));

        sandbox
            .run_bin(|cmd| {
                cmd.arg("projects").arg("--json");
            })
            .failure();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("sources");
            })
            .success()
            .stdout(predicate::str::contains("extensions-config:"));
    }

    #[test]
    fn reports_workspace_discovery_failures() {
        let sandbox = create_empty_moon_sandbox();
        sandbox.create_file(
            ".moon/workspace.yml",
            r"
projects: []
workspaces:
  missing:
    path: missing
",
        );

        sandbox
            .run_bin(|cmd| {
                cmd.arg("sources").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"id\": null"))
            .stdout(predicate::str::contains("\"missing\""))
            .stdout(predicate::str::contains("\"stage\": \"resolve-path\""));
    }

    #[test]
    fn reports_primary_provider_failures() {
        let sandbox = create_sources_sandbox();
        sandbox.create_file(".moon/vcs.json", "not-json");

        sandbox
            .run_bin(|cmd| {
                cmd.arg("sources").arg("--json");
            })
            .success()
            .stdout(predicate::str::contains("\"status\": \"failed\""))
            .stdout(predicate::str::contains("expected ident"));
    }
}
