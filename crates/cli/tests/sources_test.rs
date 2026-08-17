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
projects: []
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
        sandbox.create_file("web/apps/app/moon.yml", "{}");
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
    fn existing_project_listing_remains_primary_scoped() {
        let sandbox = create_sources_sandbox();

        sandbox
            .run_bin(|cmd| {
                cmd.arg("projects").arg("--json");
            })
            .success()
            .stdout(predicate::eq("[]\n"));
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
