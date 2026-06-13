use crate::{VcsPlugin, VcsPluginInitialization, adapter::VcsPluginAdapter};
use miette::IntoDiagnostic;
use moon_pdk_api::{InitializeVcsInput, MoonContext, VirtualPath};
use moon_plugin::{MoonHostData, PluginLocator, PluginRegistry, PluginType, PluginsConfig};
use moon_vcs::{BoxedVcs, WorkspaceFiles, git::Git};
use serde::{Deserialize, Serialize};
use starbase_utils::hash;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;
use warpgate::{DataLocator, Id};

const USER_CONFIG_FILE: &str = "vcs.json";

#[derive(Debug, Default)]
struct VcsPluginsConfig;

impl PluginsConfig for VcsPluginsConfig {
    fn get_ids(&self) -> Vec<&Id> {
        vec![]
    }

    fn get_locator(&self, _id: &Id) -> Option<&PluginLocator> {
        None
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VcsPluginConfig {
    pub enabled: bool,
    pub plugin: PluginLocator,
    pub sha256: String,
}

pub fn get_user_vcs_config_path(host_data: &MoonHostData) -> PathBuf {
    host_data.moon_env.store_root.join(USER_CONFIG_FILE)
}

pub fn load_user_vcs_config(path: &Path) -> miette::Result<Option<VcsPluginConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path).into_diagnostic()?;
    let mut config = serde_json::from_str::<VcsPluginConfig>(&content).into_diagnostic()?;
    if config.enabled {
        validate_config(&mut config)?;
    }

    Ok(Some(config))
}

pub async fn load_vcs_adapter(
    host_data: MoonHostData,
    working_dir: &Path,
    workspace_root: &Path,
    baseline: &str,
    remote_candidates: &[String],
) -> miette::Result<BoxedVcs> {
    let config_path = get_user_vcs_config_path(&host_data);
    let config = load_user_vcs_config(&config_path)?;

    if let Some(config) = config.filter(|config| config.enabled) {
        let plugin = load_verified_vcs_plugin(
            host_data.clone(),
            Id::raw("user-vcs"),
            config.plugin,
            &config.sha256,
        )
        .await?;

        return activate_provider(
            plugin,
            working_dir,
            workspace_root,
            baseline,
            remote_candidates,
            true,
        )
        .await;
    }

    if !Git::is_repository(workspace_root) {
        return Ok(Box::new(Git::load(
            workspace_root,
            baseline,
            remote_candidates,
        )?));
    }

    let plugin = load_bundled_git_plugin(host_data).await?;

    activate_provider(
        plugin,
        working_dir,
        workspace_root,
        baseline,
        remote_candidates,
        false,
    )
    .await
}

async fn activate_provider(
    plugin: Arc<VcsPlugin>,
    working_dir: &Path,
    workspace_root: &Path,
    baseline: &str,
    remote_candidates: &[String],
    require_active: bool,
) -> miette::Result<BoxedVcs> {
    let context = MoonContext {
        // VCS guests have no filesystem access, so preserve native paths for
        // root discovery while the host independently confines command cwd.
        working_dir: VirtualPath::new(working_dir),
        workspace_root: VirtualPath::new(workspace_root),
    };
    let provider_name = plugin.metadata.name.clone();
    let plugin_id = plugin.id.clone();
    let initialization = plugin
        .initialize(InitializeVcsInput {
            baseline: Some(baseline.to_owned()),
            remote_candidates: remote_candidates.to_owned(),
            context: context.clone(),
        })
        .await?;
    let plugin = match initialization {
        VcsPluginInitialization::NotDetected { reason } => {
            let label = if require_active {
                "configured VCS provider"
            } else {
                "VCS provider"
            };

            return Err(miette::miette!(
                "{label} `{plugin_id}` ({provider_name}) is not active: {reason}"
            ));
        }
        VcsPluginInitialization::Initialized(plugin) => plugin,
    };

    debug!(plugin = provider_name, "Activated source-control provider");

    Ok(Box::new(VcsPluginAdapter::new(
        baseline.to_owned(),
        WorkspaceFiles::new(workspace_root)?,
        plugin,
    )))
}

pub async fn load_verified_vcs_plugin(
    host_data: MoonHostData,
    id: Id,
    locator: PluginLocator,
    expected_sha256: &str,
) -> miette::Result<Arc<VcsPlugin>> {
    let registry = PluginRegistry::new(PluginType::Vcs, host_data, VcsPluginsConfig)?;
    let expected_sha256 = expected_sha256.to_owned();

    registry
        .load_verified_without_config(id, locator, move |wasm_file, bytes| {
            verify_sha256(wasm_file, bytes, &expected_sha256)
        })
        .await
}

async fn load_bundled_git_plugin(host_data: MoonHostData) -> miette::Result<Arc<VcsPlugin>> {
    let mut moon_env = (*host_data.moon_env).clone();
    let cache_root = std::env::temp_dir().join("moon-vcs-plugins");
    moon_env.plugins_dir = cache_root.join("plugins");
    moon_env.temp_dir = cache_root.join("temp");
    let host_data = MoonHostData {
        moon_env: Arc::new(moon_env),
        ..host_data
    };
    let registry = PluginRegistry::new(PluginType::Vcs, host_data, VcsPluginsConfig)?;
    let locator = PluginLocator::Data(Box::new(DataLocator {
        data: "data://vcs_git".into(),
        bytes: Some(include_bytes!("../res/vcs_git.wasm").to_vec()),
    }));

    registry.load_without_config(Id::raw("git"), locator).await
}

fn verify_sha256(wasm_file: &Path, bytes: &[u8], expected: &str) -> miette::Result<()> {
    let actual = hash::sha256::from_bytes(bytes);

    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(miette::miette!(
            "VCS plugin integrity check failed for {}: expected SHA-256 {expected}, received {actual}",
            wasm_file.display()
        ))
    }
}

fn validate_config(config: &mut VcsPluginConfig) -> miette::Result<()> {
    if config.sha256.len() != 64
        || !config
            .sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(miette::miette!(
            "trusted plugin SHA-256 must contain exactly 64 hexadecimal characters"
        ));
    }

    match &mut config.plugin {
        PluginLocator::File(file) => {
            let path = file.get_unresolved_path();

            if !path.is_absolute() {
                return Err(miette::miette!(
                    "user VCS plugin file locators must use an absolute path"
                ));
            }

            file.path = Some(path);
        }
        PluginLocator::Url(url) if !url.url.starts_with("https://") => {
            return Err(miette::miette!("user VCS plugin URLs must use HTTPS"));
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::path::WorkspaceRelativePathBuf;
    use moon_plugin::{MoonEnvironment, ProtoEnvironment};
    use starbase_sandbox::create_empty_sandbox;
    use std::process::Command;
    use std::sync::Arc;
    use warpgate::FileLocator;

    #[cfg(unix)]
    fn create_executable(path: &Path, content: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    struct KillPidOnDrop {
        identity: String,
        pid_file: PathBuf,
    }

    #[cfg(unix)]
    impl KillPidOnDrop {
        fn new(pid_file: PathBuf, identity: &Path) -> Self {
            Self {
                identity: identity.to_string_lossy().into_owned(),
                pid_file,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for KillPidOnDrop {
        fn drop(&mut self) {
            let Ok(pid) = fs::read_to_string(&self.pid_file) else {
                return;
            };

            if !Command::new("ps")
                .args(["-p", pid.trim(), "-o", "command="])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains(&self.identity)
                })
            {
                return;
            }

            let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);

            while Command::new("ps")
                .args(["-p", pid.trim(), "-o", "command="])
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains(&self.identity)
                })
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    fn create_host_data(sandbox: &Path) -> MoonHostData {
        MoonHostData {
            moon_env: Arc::new(MoonEnvironment::new_testing(sandbox)),
            proto_env: Arc::new(ProtoEnvironment::new_testing(sandbox).unwrap()),
            ..Default::default()
        }
    }

    fn create_nested_host_data(sandbox: &Path, workspace_root: &Path) -> MoonHostData {
        let mut moon_env = MoonEnvironment::new_testing(sandbox);
        moon_env.working_dir = workspace_root.to_owned();
        moon_env.workspace_root = workspace_root.to_owned();

        MoonHostData {
            moon_env: Arc::new(moon_env),
            proto_env: Arc::new(ProtoEnvironment::new_testing(workspace_root).unwrap()),
            ..Default::default()
        }
    }

    fn create_git_repository() -> starbase_sandbox::Sandbox {
        create_git_repository_with_global_config(None)
    }

    fn apply_global_git_config(command: &mut Command, global_config: Option<&Path>) {
        if let Some(global_config) = global_config {
            let home = global_config
                .parent()
                .expect("global Git config must have a parent directory");
            command
                .env("HOME", home)
                .env("XDG_CONFIG_HOME", home.join("xdg"))
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
    }

    fn create_git_repository_with_global_config(
        global_config: Option<&Path>,
    ) -> starbase_sandbox::Sandbox {
        let sandbox = create_empty_sandbox();
        sandbox.create_file(".gitignore", "node_modules\ntarget\n");
        sandbox.create_file("initial.txt", "initial");
        sandbox.run_git(|command| {
            apply_global_git_config(command, global_config);
            command.args(["init", "--initial-branch", "master"]);
        });
        sandbox.run_git(|command| {
            apply_global_git_config(command, global_config);
            command.args(["config", "commit.gpgSign", "false"]);
        });
        sandbox.run_git(|command| {
            apply_global_git_config(command, global_config);
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            apply_global_git_config(command, global_config);
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "initial",
            ]);
        });

        sandbox
    }

    fn git_output(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(output.status.success());

        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn loads_valid_user_config() {
        let sandbox = create_empty_sandbox();
        let plugin_file = sandbox.path().join("plugin.wasm");
        let config_file = sandbox.path().join(USER_CONFIG_FILE);
        let config = VcsPluginConfig {
            enabled: true,
            plugin: PluginLocator::File(Box::new(FileLocator {
                file: format!("file://{}", plugin_file.display()),
                path: Some(plugin_file),
            })),
            sha256: "a".repeat(64),
        };
        fs::write(&config_file, serde_json::to_string(&config).unwrap()).unwrap();

        assert_eq!(load_user_vcs_config(&config_file).unwrap(), Some(config));
    }

    #[test]
    fn rejects_invalid_user_config() {
        let sandbox = create_empty_sandbox();
        let config_file = sandbox.path().join(USER_CONFIG_FILE);
        let config = VcsPluginConfig {
            enabled: true,
            plugin: PluginLocator::File(Box::new(warpgate::FileLocator {
                file: "file://relative.wasm".into(),
                path: Some("relative.wasm".into()),
            })),
            sha256: "invalid".into(),
        };
        fs::write(&config_file, serde_json::to_string(&config).unwrap()).unwrap();

        assert!(load_user_vcs_config(&config_file).is_err());
    }

    #[test]
    fn allows_disabled_user_config_with_an_invalid_pin() {
        let sandbox = create_empty_sandbox();
        let config_file = sandbox.path().join(USER_CONFIG_FILE);
        let config = VcsPluginConfig {
            enabled: false,
            plugin: PluginLocator::File(Box::new(warpgate::FileLocator {
                file: "file://relative.wasm".into(),
                path: Some("relative.wasm".into()),
            })),
            sha256: "invalid".into(),
        };
        fs::write(&config_file, serde_json::to_string(&config).unwrap()).unwrap();

        let loaded = load_user_vcs_config(&config_file).unwrap().unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.sha256, config.sha256);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_supplies_complete_source_control() {
        let sandbox = create_git_repository();
        sandbox.create_file("working.txt", "working");
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();
        sandbox.create_file("after-initialization.txt", "later");

        assert!(adapter.is_enabled());
        assert!(
            !adapter
                .get_local_branch_revision()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            adapter.get_default_branch().await.unwrap().as_str(),
            "master"
        );
        let changed = adapter.get_changed_files().await.unwrap();
        sandbox.create_file("after-query.txt", "later");
        let repeated = adapter.get_changed_files().await.unwrap();

        assert!(
            changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("working.txt"))
        );
        assert!(
            changed
                .files
                .keys()
                .all(|path| !path.as_str().contains("plugins/vcs"))
        );
        assert!(
            !changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("after-initialization.txt"))
        );
        assert_eq!(repeated, changed);
        assert!(
            !repeated
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("after-query.txt"))
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_disables_watchman_style_fsmonitor() {
        let sandbox = create_git_repository();
        let helper = sandbox.path().join("fsmonitor-helper.sh");
        let descendant = sandbox.path().join("fsmonitor-descendant.sh");
        let marker = sandbox.path().join("fsmonitor-invoked");
        let child_pid = sandbox.path().join("fsmonitor-child.pid");
        let _child = KillPidOnDrop::new(child_pid.clone(), &descendant);
        create_executable(&descendant, "#!/bin/sh\nsleep 30\nexit 0\n");
        create_executable(
            &helper,
            r#"#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$dir/fsmonitor-descendant.sh" &
printf '%s\n' "$!" > "$dir/fsmonitor-child.pid"
: > "$dir/fsmonitor-invoked"
printf 'token\n'
"#,
        );
        sandbox.run_git(|command| {
            command.args([
                "config",
                "core.fsmonitor",
                helper.to_str().expect("helper path must be UTF-8"),
            ]);
        });
        sandbox.create_file("working.txt", "working");

        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();
        let changed = adapter.get_changed_files().await.unwrap();

        assert!(
            changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("working.txt"))
        );
        assert!(!marker.exists(), "configured fsmonitor was invoked");
        assert!(!child_pid.exists(), "fsmonitor spawned a descendant");
    }

    #[cfg(unix)]
    #[test]
    fn git_repository_fixtures_disable_inherited_commit_signing() {
        let config = create_empty_sandbox();
        let global_config = config.path().join(".gitconfig");
        let helper = config.path().join("signing-helper.sh");
        let marker = config.path().join("signer-invoked");
        fs::create_dir(config.path().join("xdg")).unwrap();
        create_executable(
            &helper,
            r#"#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
: > "$dir/signer-invoked"
exit 1
"#,
        );
        for (key, value) in [
            ("commit.gpgSign", "true"),
            ("gpg.format", "openpgp"),
            (
                "gpg.program",
                helper.to_str().expect("helper path must be UTF-8"),
            ),
        ] {
            assert!(
                Command::new("git")
                    .args(["config", "--file"])
                    .arg(&global_config)
                    .args([key, value])
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let repository = create_git_repository_with_global_config(Some(&global_config));
        repository.create_file("second.txt", "second");
        repository.run_git(|command| {
            apply_global_git_config(command, Some(&global_config));
            command.args(["add", "second.txt"]);
        });
        repository.run_git(|command| {
            apply_global_git_config(command, Some(&global_config));
            command.args(["commit", "-m", "second"]);
        });

        assert_eq!(
            git_output(repository.path(), &["rev-list", "--count", "HEAD"]),
            "2"
        );
        assert!(!marker.exists(), "inherited commit signer was invoked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_instance_initializes_only_once() {
        let sandbox = create_git_repository();
        let plugin = load_bundled_git_plugin(create_host_data(sandbox.path()))
            .await
            .unwrap();
        let duplicate = Arc::clone(&plugin);
        let context = MoonContext {
            working_dir: plugin.to_virtual_path(sandbox.path()),
            workspace_root: plugin.to_virtual_path(sandbox.path()),
        };
        let input = InitializeVcsInput {
            baseline: Some("master".into()),
            remote_candidates: vec![],
            context,
        };

        let VcsPluginInitialization::Initialized(initialized) =
            plugin.initialize(input.clone()).await.unwrap()
        else {
            panic!("Git provider was not detected");
        };

        assert!(
            initialized
                .get_impacts(moon_pdk_api::VcsImpactIntent::Working)
                .await
                .is_ok()
        );
        assert!(duplicate.initialize(input).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skips_the_bundled_provider_without_a_git_repository() {
        let sandbox = create_empty_sandbox();
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert!(!adapter.is_enabled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enabled_configured_provider_must_be_active() {
        let sandbox = create_empty_sandbox();
        let host_data = create_host_data(sandbox.path());
        let plugin_file = sandbox.path().join("configured-vcs.wasm");
        let plugin_bytes = include_bytes!("../res/vcs_git.wasm");
        fs::write(&plugin_file, plugin_bytes).unwrap();
        let config_file = get_user_vcs_config_path(&host_data);
        fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        let config = VcsPluginConfig {
            enabled: true,
            plugin: PluginLocator::File(Box::new(FileLocator {
                file: format!("file://{}", plugin_file.display()),
                path: Some(plugin_file),
            })),
            sha256: hash::sha256::from_bytes(plugin_bytes),
        };
        fs::write(&config_file, serde_json::to_string(&config).unwrap()).unwrap();

        let error = load_vcs_adapter(host_data, sandbox.path(), sandbox.path(), "master", &[])
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("user-vcs"), "unexpected error: {error}");
        assert!(error.contains("Git"), "unexpected error: {error}");
        assert!(
            error.contains("Git did not detect a repository"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_configured_provider_retains_non_git_behavior() {
        let sandbox = create_empty_sandbox();
        let host_data = create_host_data(sandbox.path());
        let config_file = get_user_vcs_config_path(&host_data);
        fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        let config = VcsPluginConfig {
            enabled: false,
            plugin: PluginLocator::File(Box::new(FileLocator {
                file: "file://missing.wasm".into(),
                path: Some("missing.wasm".into()),
            })),
            sha256: "invalid".into(),
        };
        fs::write(&config_file, serde_json::to_string(&config).unwrap()).unwrap();

        let adapter = load_vcs_adapter(host_data, sandbox.path(), sandbox.path(), "master", &[])
            .await
            .unwrap();

        assert!(!adapter.is_enabled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_tolerates_a_missing_baseline() {
        let sandbox = create_git_repository();
        sandbox.create_file("working.txt", "working");
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "not-fetched",
            &[],
        )
        .await
        .unwrap();

        assert!(adapter.is_enabled());
        assert!(
            adapter
                .get_changed_files()
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("working.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_reports_no_previous_changes_for_a_root_commit() {
        let sandbox = create_git_repository();
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert!(
            adapter
                .get_changed_files_against_previous_revision("master")
                .await
                .unwrap()
                .files
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_does_not_treat_a_shallow_boundary_as_a_root_commit() {
        let repository = create_git_repository();
        repository.create_file("second.txt", "second");
        repository.run_git(|command| {
            command.args(["add", "."]);
        });
        repository.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "second",
            ]);
        });
        let clone_parent = create_empty_sandbox();
        let shallow = clone_parent.path().join("shallow");
        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                &format!("file://{}", repository.path().display()),
                shallow.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let adapter = load_vcs_adapter(
            create_host_data(&shallow),
            &shallow,
            &shallow,
            "master",
            &[],
        )
        .await
        .unwrap();

        let error = adapter
            .get_changed_files_against_previous_revision("master")
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("incomplete-history boundary"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_supports_an_unborn_repository() {
        let sandbox = create_empty_sandbox();
        sandbox.run_git(|command| {
            command.args(["init", "--initial-branch", "master"]);
        });
        sandbox.run_git(|command| {
            command.args(["config", "commit.gpgSign", "false"]);
        });
        sandbox.create_file("working.txt", "working");
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert!(adapter.is_enabled());
        assert!(
            adapter
                .get_changed_files()
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("working.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_distinguishes_linked_worktree_roots() {
        let repository = create_git_repository();
        let worktree_parent = create_empty_sandbox();
        let worktree = worktree_parent.path().join("checkout");
        repository.run_git(|command| {
            command.args([
                "worktree",
                "add",
                "-b",
                "linked",
                worktree.to_str().unwrap(),
            ]);
        });
        let adapter = load_vcs_adapter(
            create_host_data(&worktree),
            &worktree,
            &worktree,
            "master",
            &[],
        )
        .await
        .unwrap();

        assert_eq!(
            adapter
                .get_repository_root()
                .unwrap()
                .canonicalize()
                .unwrap(),
            repository.path().canonicalize().unwrap()
        );
        assert_eq!(
            adapter.get_working_root().unwrap().canonicalize().unwrap(),
            worktree.canonicalize().unwrap()
        );
        assert!(adapter.is_worktree());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_uses_configured_remote_candidates() {
        let sandbox = create_git_repository();
        sandbox.run_git(|command| {
            command.args([
                "remote",
                "add",
                "fork",
                "https://github.com/moonrepo/custom.git",
            ]);
        });
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &["fork".into()],
        )
        .await
        .unwrap();

        assert_eq!(
            adapter.get_repository_slug().await.unwrap().as_str(),
            "moonrepo/custom"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_resolves_full_branch_refs_from_remotes() {
        let sandbox = create_git_repository();
        let revision = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        sandbox.run_git(|command| {
            command.args(["update-ref", "refs/remotes/fork/master", &revision]);
        });
        sandbox.run_git(|command| {
            command.args(["checkout", "--detach"]);
        });
        sandbox.run_git(|command| {
            command.args(["branch", "-D", "master"]);
        });
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "refs/heads/master",
            &["fork".into()],
        )
        .await
        .unwrap();

        assert_eq!(
            adapter
                .get_default_branch_revision()
                .await
                .unwrap()
                .as_str(),
            revision
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_combines_stale_local_and_remote_baselines() {
        let sandbox = create_git_repository();
        let initial = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        sandbox.create_file("landed.txt", "landed");
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "landed",
            ]);
        });
        let remote = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        sandbox.run_git(|command| {
            command.args(["update-ref", "refs/remotes/origin/master", &remote]);
        });
        sandbox.run_git(|command| {
            command.args(["checkout", "-b", "feature"]);
        });
        sandbox.create_file("feature.txt", "feature");
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "feature",
            ]);
        });
        sandbox.run_git(|command| {
            command.args(["branch", "-f", "master", &initial]);
        });
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &["origin".into()],
        )
        .await
        .unwrap();
        let changed = adapter
            .get_changed_files_between_revisions("master", "HEAD")
            .await
            .unwrap();

        assert!(
            changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("feature.txt"))
        );
        assert!(
            !changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("landed.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_pins_movable_references_during_initialization() {
        let sandbox = create_git_repository();
        sandbox.run_git(|command| {
            command.args(["branch", "moving"]);
        });
        sandbox.create_file("recorded.txt", "recorded");
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "recorded",
            ]);
        });
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();
        sandbox.run_git(|command| {
            command.args(["branch", "-f", "moving", "HEAD"]);
        });

        assert!(
            adapter
                .get_changed_files_between_revisions("moving~0", "HEAD")
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("recorded.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_includes_changes_inside_submodules() {
        let submodule = create_git_repository();
        let sandbox = create_git_repository();
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    submodule.path().to_str().unwrap(),
                    "modules/child",
                ])
                .current_dir(sandbox.path())
                .status()
                .unwrap()
                .success()
        );
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "add submodule",
            ]);
        });
        sandbox.create_file("modules/child/initial.txt", "changed");
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert!(
            adapter
                .get_changed_files()
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("modules/child/initial.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_includes_revision_changes_inside_submodules() {
        let submodule = create_git_repository();
        let sandbox = create_git_repository();
        let before_add = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    submodule.path().to_str().unwrap(),
                    "modules/child",
                ])
                .current_dir(sandbox.path())
                .status()
                .unwrap()
                .success()
        );
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "add submodule",
            ]);
        });
        let base = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        let checked_out_submodule = sandbox.path().join("modules/child");
        sandbox.create_file("modules/child/initial.txt", "changed");
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Moon",
                    "-c",
                    "user.email=moon@example.com",
                    "commit",
                    "-am",
                    "change submodule",
                ])
                .current_dir(&checked_out_submodule)
                .status()
                .unwrap()
                .success()
        );
        sandbox.run_git(|command| {
            command.args(["add", "modules/child"]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "update submodule",
            ]);
        });
        let head = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert!(
            adapter
                .get_changed_files_between_revisions(&before_add, &base)
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("modules/child/initial.txt"))
        );

        assert!(
            adapter
                .get_changed_files_between_revisions(&base, &head)
                .await
                .unwrap()
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("modules/child/initial.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_exposes_optional_hooks() {
        let sandbox = create_git_repository();
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();
        let hook_names = vec!["pre-commit".into()];
        let hooks = adapter.setup_hooks(&hook_names).await.unwrap().unwrap();

        assert_eq!(hooks.hooks_dir, sandbox.path().join(".moon/hooks"));
        adapter.teardown_hooks(&hook_names).await.unwrap();
        adapter.teardown_hooks(&hook_names).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_rejects_unsupported_hooks_atomically() {
        let sandbox = create_git_repository();
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        let error = adapter
            .setup_hooks(&["post-push".into()])
            .await
            .err()
            .unwrap()
            .to_string();
        let configured_hooks = Command::new("git")
            .args(["config", "--get", "core.hooksPath"])
            .current_dir(sandbox.path())
            .output()
            .unwrap();

        assert!(error.contains("post-push"), "{error}");
        assert_eq!(configured_hooks.status.code(), Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_uses_the_workspace_config_directory_for_hooks() {
        let sandbox = create_git_repository();
        sandbox.create_file(".config/moon/workspace.yml", "projects: {}\n");
        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();
        let hook_names = vec!["pre-commit".into()];
        let hooks = adapter.setup_hooks(&hook_names).await.unwrap().unwrap();

        assert_eq!(hooks.hooks_dir, sandbox.path().join(".config/moon/hooks"));
        adapter.teardown_hooks(&hook_names).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_git_provider_scopes_changes_to_a_nested_workspace() {
        let sandbox = create_git_repository();
        sandbox.create_file("workspace/inside.txt", "inside");
        sandbox.run_git(|command| {
            command.args(["add", "."]);
        });
        sandbox.run_git(|command| {
            command.args([
                "-c",
                "user.name=Moon",
                "-c",
                "user.email=moon@example.com",
                "commit",
                "-m",
                "nested workspace",
            ]);
        });
        let workspace_root = sandbox.path().join("workspace");
        sandbox.create_file("outside.txt", "outside");
        sandbox.create_file("workspace/inside.txt", "changed");
        let adapter = load_vcs_adapter(
            create_nested_host_data(sandbox.path(), &workspace_root),
            &workspace_root,
            &workspace_root,
            "master",
            &[],
        )
        .await
        .unwrap();

        let changed = adapter.get_changed_files().await.unwrap();

        assert_eq!(
            changed.files.len(),
            1,
            "unexpected files: {:?}",
            changed.files
        );
        assert!(
            changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("inside.txt"))
        );

        let hook_names = vec!["pre-commit".into()];
        let hooks = adapter.setup_hooks(&hook_names).await.unwrap().unwrap();
        let configured_hooks = Command::new("git")
            .args(["config", "--get", "core.hooksPath"])
            .current_dir(sandbox.path())
            .output()
            .unwrap();

        assert_eq!(hooks.hooks_dir, workspace_root.join(".moon/hooks"));
        assert_eq!(
            hooks.working_dir.canonicalize().unwrap(),
            sandbox.path().canonicalize().unwrap()
        );
        assert_eq!(
            String::from_utf8_lossy(&configured_hooks.stdout).trim(),
            "workspace/.moon/hooks"
        );
        adapter.teardown_hooks(&hook_names).await.unwrap();
        let configured_hooks = Command::new("git")
            .args(["config", "--get", "core.hooksPath"])
            .current_dir(sandbox.path())
            .output()
            .unwrap();

        assert_eq!(configured_hooks.status.code(), Some(1));
    }
}
