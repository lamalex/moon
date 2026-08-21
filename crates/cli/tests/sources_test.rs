mod utils;

use moon_test_utils::{create_empty_moon_sandbox, predicates::prelude::*};

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
dependsOn:
  - id: lib
    sourceRoot: frontend

tasks:
  build:
    command: echo primary
",
        );
        sandbox.create_file(
            "web/apps/app/moon.yml",
            r"
tasks:
  build:
    command: echo child
    inputs:
      - file: changed.txt
        content: child
",
        );
        sandbox.create_file("web/apps/lib/moon.yml", "{}");
        sandbox
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
            .stdout(predicate::str::contains("\"primary\""))
            .stdout(predicate::str::contains("\"child\"").not());
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
    fn resolves_cross_source_dependencies_without_enabling_cross_source_execution() {
        let sandbox = create_sources_sandbox();

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
            .success()
            .stdout(predicate::str::contains("primary"));
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
