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
use tracing::{debug, warn};
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

enum ProviderActivation {
    NotDetected { reason: String },
    Activated(BoxedVcs),
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
    load_vcs_adapter_with_jj_plugin(
        host_data,
        working_dir,
        workspace_root,
        baseline,
        remote_candidates,
        None,
    )
    .await
}

async fn load_vcs_adapter_with_jj_plugin(
    host_data: MoonHostData,
    working_dir: &Path,
    workspace_root: &Path,
    baseline: &str,
    remote_candidates: &[String],
    jj_plugin_override: Option<miette::Result<Arc<VcsPlugin>>>,
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
        let plugin_id = plugin.id.clone();
        let plugin_name = plugin.metadata.name.clone();

        return match activate_provider(
            plugin,
            working_dir,
            workspace_root,
            baseline,
            remote_candidates,
        )
        .await?
        {
            ProviderActivation::Activated(adapter) => Ok(adapter),
            ProviderActivation::NotDetected { reason } => Err(miette::miette!(
                "configured VCS provider `{plugin_id}` ({plugin_name}) is not active: {reason}"
            )),
        };
    }

    let is_git_repository = Git::is_repository(workspace_root);

    if is_jj_workspace(workspace_root) {
        let jj_plugin = match jj_plugin_override {
            Some(plugin) => plugin,
            None => {
                load_bundled_vcs_plugin(
                    host_data.clone(),
                    "jj",
                    "data://vcs_jj",
                    include_bytes!("../res/vcs_jj.wasm"),
                )
                .await
            }
        };

        match jj_plugin {
            Ok(plugin) => match activate_provider(
                plugin,
                working_dir,
                workspace_root,
                baseline,
                remote_candidates,
            )
            .await
            {
                Ok(ProviderActivation::Activated(adapter)) => return Ok(adapter),
                Ok(ProviderActivation::NotDetected { reason }) if !is_git_repository => {
                    return Err(miette::miette!(
                        "detected a Jujutsu workspace, but the bundled Jujutsu provider was not active: {reason}"
                    ));
                }
                Ok(ProviderActivation::NotDetected { reason }) => {
                    warn!(%reason, "Jujutsu provider was not active, falling back to Git");
                }
                Err(error) if is_git_repository => {
                    warn!(
                        error = %error,
                        "Jujutsu provider activation failed, falling back to Git"
                    );
                }
                Err(error) => return Err(error),
            },
            Err(error) if is_git_repository => {
                warn!(
                    error = %error,
                    "Jujutsu provider failed to load, falling back to Git"
                );
            }
            Err(error) => {
                return Err(miette::miette!(
                    "detected a Jujutsu workspace, but the bundled Jujutsu provider failed to load: {error}"
                ));
            }
        }
    }

    if !is_git_repository {
        return Ok(Box::new(Git::load(
            workspace_root,
            baseline,
            remote_candidates,
        )?));
    }

    let plugin = load_bundled_vcs_plugin(
        host_data,
        "git",
        "data://vcs_git",
        include_bytes!("../res/vcs_git.wasm"),
    )
    .await?;

    match activate_provider(
        plugin,
        working_dir,
        workspace_root,
        baseline,
        remote_candidates,
    )
    .await?
    {
        ProviderActivation::Activated(adapter) => Ok(adapter),
        ProviderActivation::NotDetected { reason } => Err(miette::miette!(
            "bundled Git provider did not detect the repository: {reason}"
        )),
    }
}

fn is_jj_workspace(workspace_root: &Path) -> bool {
    workspace_root
        .ancestors()
        .any(|directory| directory.join(".jj").exists())
}

async fn activate_provider(
    plugin: Arc<VcsPlugin>,
    working_dir: &Path,
    workspace_root: &Path,
    baseline: &str,
    remote_candidates: &[String],
) -> miette::Result<ProviderActivation> {
    let context = MoonContext {
        // VCS guests have no filesystem access, so preserve native paths for
        // root discovery while the host independently confines command cwd.
        working_dir: VirtualPath::new(working_dir),
        workspace_root: VirtualPath::new(workspace_root),
    };
    let provider_name = plugin.metadata.name.clone();
    let initialization = plugin
        .initialize(InitializeVcsInput {
            baseline: Some(baseline.to_owned()),
            remote_candidates: remote_candidates.to_owned(),
            context: context.clone(),
        })
        .await?;
    let plugin = match initialization {
        VcsPluginInitialization::NotDetected { reason } => {
            return Ok(ProviderActivation::NotDetected { reason });
        }
        VcsPluginInitialization::Initialized(plugin) => plugin,
    };

    debug!(plugin = provider_name, "Activated source-control provider");

    Ok(ProviderActivation::Activated(Box::new(
        VcsPluginAdapter::new(
            baseline.to_owned(),
            WorkspaceFiles::new(workspace_root)?,
            plugin,
        ),
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

async fn load_bundled_vcs_plugin(
    host_data: MoonHostData,
    id: &str,
    data_url: &str,
    bytes: &[u8],
) -> miette::Result<Arc<VcsPlugin>> {
    let expected_sha256 = hash::sha256::from_bytes(bytes);
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
        data: data_url.into(),
        bytes: Some(bytes.to_vec()),
    }));

    registry
        .load_verified_without_config(Id::raw(id), locator, move |wasm_file, bytes| {
            verify_sha256(wasm_file, bytes, &expected_sha256)
        })
        .await
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
    use moon_vcs::ChangedStatus;
    use starbase_sandbox::create_empty_sandbox;
    use std::collections::BTreeMap;
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

    fn require_jj() {
        let output = Command::new("jj")
            .arg("--version")
            .output()
            .expect("Jujutsu must be installed to run VCS provider tests");
        assert!(
            output.status.success(),
            "Jujutsu failed to report its version"
        );
    }

    fn run_jj(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(repository)
            .env("JJ_CONFIG", "")
            .env("NO_COLOR", "1")
            .env("PAGER", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "jj failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn create_jj_repository(colocated: bool) -> starbase_sandbox::Sandbox {
        require_jj();
        let sandbox = create_empty_sandbox();
        let mode = if colocated {
            "--colocate"
        } else {
            "--no-colocate"
        };
        run_jj(sandbox.path(), &["git", "init", mode, "."]);
        sandbox
    }

    async fn initialize_jj_plugin(
        repository_root: &Path,
        workspace_root: &Path,
        baseline: &str,
    ) -> Arc<crate::InitializedVcsPlugin> {
        let plugin = load_bundled_vcs_plugin(
            create_nested_host_data(repository_root, workspace_root),
            "jj",
            "data://vcs_jj",
            include_bytes!("../res/vcs_jj.wasm"),
        )
        .await
        .unwrap();
        let context = MoonContext {
            working_dir: plugin.to_virtual_path(workspace_root),
            workspace_root: plugin.to_virtual_path(workspace_root),
        };

        match plugin
            .initialize(InitializeVcsInput {
                baseline: Some(baseline.into()),
                remote_candidates: vec![],
                context,
            })
            .await
            .unwrap()
        {
            VcsPluginInitialization::Initialized(plugin) => plugin,
            VcsPluginInitialization::NotDetected { reason } => {
                panic!("Jujutsu provider was not detected: {reason}")
            }
        }
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

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_provider_rejects_a_poisoned_shared_cache_entry() {
        let id = "poisoned-bundled-git";
        let bytes = include_bytes!("../res/vcs_git.wasm");
        let digest = hash::sha256::from_bytes(bytes);
        let cache_file = std::env::temp_dir()
            .join("moon-vcs-plugins/plugins/vcs")
            .join(format!("{id}-{digest}.wasm"));
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, b"attacker-controlled wasm").unwrap();
        let sandbox = create_empty_sandbox();

        let error = load_bundled_vcs_plugin(
            create_host_data(sandbox.path()),
            id,
            "data://poisoned_bundled_git",
            bytes,
        )
        .await
        .unwrap_err()
        .to_string();

        let _ = fs::remove_file(cache_file);
        assert!(error.contains("integrity check failed"), "{error}");
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
    async fn bundled_jj_provider_supplies_complete_source_control() {
        require_jj();

        let sandbox = create_git_repository();
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
            command.args([
                "remote",
                "add",
                "origin",
                "https://github.com/moonrepo/moon.git",
            ]);
        });
        sandbox.run_git(|command| {
            command.args(["update-ref", "refs/remotes/origin/master", "master"]);
        });
        sandbox.run_git(|command| {
            command.args(["branch", "-D", "master"]);
        });
        run_jj(sandbox.path(), &["git", "init", "--colocate", "."]);
        sandbox.create_file("working.txt", "working");

        let plugin = load_bundled_vcs_plugin(
            create_host_data(sandbox.path()),
            "jj",
            "data://vcs_jj",
            include_bytes!("../res/vcs_jj.wasm"),
        )
        .await
        .unwrap();
        let duplicate = Arc::clone(&plugin);
        let context = MoonContext {
            working_dir: plugin.to_virtual_path(sandbox.path()),
            workspace_root: plugin.to_virtual_path(sandbox.path()),
        };
        let input = InitializeVcsInput {
            baseline: Some("master".into()),
            remote_candidates: vec!["origin".into()],
            context,
        };
        let VcsPluginInitialization::Initialized(plugin) =
            plugin.initialize(input.clone()).await.unwrap()
        else {
            panic!("Jujutsu provider was not detected");
        };
        let initialization = plugin.initialization();
        assert_eq!(initialization.client.as_str(), "jj");
        assert_ne!(initialization.current.id, initialization.recorded.id);
        assert!(initialization.baseline.is_some());
        assert_eq!(
            initialization.repository_slug.as_deref(),
            Some("moonrepo/moon")
        );
        let baseline = initialization
            .baseline
            .as_ref()
            .unwrap()
            .id
            .clone()
            .unwrap();

        let working = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Working)
            .await
            .unwrap();
        let submission = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some(baseline.clone()),
                head: None,
                include_working: true,
            })
            .await
            .unwrap();
        let recorded = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some(baseline.clone()),
                head: None,
                include_working: false,
            })
            .await
            .unwrap();

        assert!(working.changes.contains_key(Path::new("working.txt")));
        assert!(submission.changes.contains_key(Path::new("feature.txt")));
        assert!(submission.changes.contains_key(Path::new("working.txt")));
        assert!(recorded.changes.contains_key(Path::new("feature.txt")));
        assert!(!recorded.changes.contains_key(Path::new("working.txt")));

        sandbox.create_file("after-initialization.txt", "later");
        let pinned = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some(baseline),
                head: initialization.current.id.clone(),
                include_working: true,
            })
            .await
            .unwrap();
        assert!(
            !pinned
                .changes
                .contains_key(Path::new("after-initialization.txt"))
        );
        assert!(duplicate.initialize(input).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn production_loader_prefers_jj_for_a_colocated_repository() {
        require_jj();

        let sandbox = create_git_repository();
        run_jj(sandbox.path(), &["git", "init", "--colocate", "."]);
        let jj_revision = run_jj(
            sandbox.path(),
            &["log", "--no-graph", "-r", "@", "-T", "commit_id"],
        );
        let git_revision = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        assert_ne!(jj_revision, git_revision);

        let adapter = load_vcs_adapter(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
        )
        .await
        .unwrap();

        assert_eq!(
            adapter.get_local_branch_revision().await.unwrap().as_str(),
            jj_revision
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn production_loader_falls_back_to_git_when_jj_fails_to_load() {
        let sandbox = create_git_repository();
        fs::create_dir(sandbox.path().join(".jj")).unwrap();
        let git_revision = git_output(sandbox.path(), &["rev-parse", "HEAD"]);
        let adapter = load_vcs_adapter_with_jj_plugin(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
            Some(Err(miette::miette!(
                "unable to resolve executable `jj` for process capability `jj`"
            ))),
        )
        .await
        .unwrap();

        assert_eq!(
            adapter.get_local_branch_revision().await.unwrap().as_str(),
            git_revision
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn production_loader_reports_jj_load_failures_without_git() {
        let sandbox = create_empty_sandbox();
        fs::create_dir(sandbox.path().join(".jj")).unwrap();
        let error = load_vcs_adapter_with_jj_plugin(
            create_host_data(sandbox.path()),
            sandbox.path(),
            sandbox.path(),
            "master",
            &[],
            Some(Err(miette::miette!(
                "unable to resolve executable `jj` for process capability `jj`"
            ))),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("detected a Jujutsu workspace"), "{error}");
        assert!(
            error.contains("unable to resolve executable `jj`"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn production_loader_supports_a_non_colocated_jj_repository() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("working.txt", "working");
        assert!(!sandbox.path().join(".git").exists());

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
        assert_eq!(
            adapter.get_repository_root().unwrap(),
            sandbox.path().canonicalize().unwrap()
        );
        assert_eq!(
            adapter.get_working_root().unwrap(),
            sandbox.path().canonicalize().unwrap()
        );
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
    async fn bundled_jj_provider_scopes_changes_to_a_nested_workspace() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("outside.txt", "base outside");
        sandbox.create_file("workspace/inside.txt", "base inside");
        run_jj(sandbox.path(), &["describe", "-m", "base"]);
        run_jj(sandbox.path(), &["bookmark", "create", "master", "-r", "@"]);
        run_jj(sandbox.path(), &["new", "master", "-m", "working"]);
        sandbox.create_file("outside.txt", "changed outside");
        sandbox.create_file("workspace/inside.txt", "changed inside");

        let plugin =
            initialize_jj_plugin(sandbox.path(), &sandbox.path().join("workspace"), "master").await;
        let impacts = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Working)
            .await
            .unwrap();

        assert_eq!(
            impacts.changes,
            BTreeMap::from([(
                PathBuf::from("inside.txt"),
                moon_pdk_api::VcsChangeMask::MODIFIED | moon_pdk_api::VcsChangeMask::WORKING,
            )])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_jj_provider_supports_nested_non_colocated_submission_impacts() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("outside.txt", "base outside");
        sandbox.create_file("workspace/inside.txt", "base inside");
        run_jj(sandbox.path(), &["describe", "-m", "base"]);
        run_jj(sandbox.path(), &["bookmark", "create", "master", "-r", "@"]);
        run_jj(sandbox.path(), &["new", "master", "-m", "recorded"]);
        sandbox.create_file("outside-recorded.txt", "outside");
        sandbox.create_file("workspace/recorded.txt", "inside");
        run_jj(sandbox.path(), &["new", "@", "-m", "working"]);

        let plugin =
            initialize_jj_plugin(sandbox.path(), &sandbox.path().join("workspace"), "master").await;
        let baseline = plugin
            .initialization()
            .baseline
            .as_ref()
            .and_then(|state| state.id.clone())
            .unwrap();

        assert_eq!(
            plugin.initialization().history,
            moon_pdk_api::VcsHistoryCompleteness::Complete
        );

        let impacts = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some(baseline),
                head: None,
                include_working: false,
            })
            .await
            .unwrap();

        assert_eq!(
            impacts.completeness,
            moon_pdk_api::VcsImpactCompleteness::Exact
        );
        assert!(impacts.changes.contains_key(Path::new("recorded.txt")));
        assert!(
            !impacts
                .changes
                .contains_key(Path::new("outside-recorded.txt"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_jj_provider_tolerates_a_missing_baseline() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("working.txt", "working");
        let plugin = initialize_jj_plugin(sandbox.path(), sandbox.path(), "not-fetched").await;

        assert!(plugin.initialization().baseline.is_none());
        assert_eq!(
            plugin
                .get_impacts(moon_pdk_api::VcsImpactIntent::Working)
                .await
                .unwrap()
                .changes[Path::new("working.txt")],
            moon_pdk_api::VcsChangeMask::ADDED | moon_pdk_api::VcsChangeMask::WORKING
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_jj_provider_synthesizes_recorded_state_for_merge_parents() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("base.txt", "base");
        run_jj(sandbox.path(), &["describe", "-m", "base"]);
        run_jj(sandbox.path(), &["bookmark", "create", "master", "-r", "@"]);

        run_jj(sandbox.path(), &["new", "master", "-m", "left"]);
        sandbox.create_file("left.txt", "left");
        run_jj(sandbox.path(), &["bookmark", "create", "left", "-r", "@"]);

        run_jj(sandbox.path(), &["new", "master", "-m", "right"]);
        sandbox.create_file("right.txt", "right");
        run_jj(sandbox.path(), &["bookmark", "create", "right", "-r", "@"]);

        run_jj(
            sandbox.path(),
            &["new", "left", "right", "-m", "merge-working"],
        );
        sandbox.create_file("merge-only.txt", "merge");
        let operation_before = run_jj(
            sandbox.path(),
            &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
        );

        let plugin = initialize_jj_plugin(sandbox.path(), sandbox.path(), "master").await;
        assert_ne!(
            plugin.initialization().current.id,
            plugin.initialization().recorded.id
        );
        assert!(plugin.initialization().recorded.label.is_none());

        let working = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Working)
            .await
            .unwrap();
        assert_eq!(
            working.changes[Path::new("merge-only.txt")],
            moon_pdk_api::VcsChangeMask::ADDED | moon_pdk_api::VcsChangeMask::WORKING
        );

        let recorded = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some("master".into()),
                head: None,
                include_working: false,
            })
            .await
            .unwrap();
        for path in ["left.txt", "right.txt"] {
            assert_eq!(
                recorded.changes[Path::new(path)],
                moon_pdk_api::VcsChangeMask::ADDED | moon_pdk_api::VcsChangeMask::RECORDED
            );
        }
        assert!(!recorded.changes.contains_key(Path::new("merge-only.txt")));
        assert_eq!(
            run_jj(
                sandbox.path(),
                &["op", "log", "--no-graph", "-n", "1", "-T", "id"]
            ),
            operation_before
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_jj_provider_reports_actual_rename_and_copy_impacts() {
        let sandbox = create_jj_repository(false);
        sandbox.create_file("old.txt", "rename source");
        sandbox.create_file("source.txt", "copy source");
        run_jj(sandbox.path(), &["describe", "-m", "base"]);
        run_jj(sandbox.path(), &["bookmark", "create", "master", "-r", "@"]);
        run_jj(sandbox.path(), &["new", "master", "-m", "changes"]);
        fs::rename(
            sandbox.path().join("old.txt"),
            sandbox.path().join("renamed.txt"),
        )
        .unwrap();
        fs::copy(
            sandbox.path().join("source.txt"),
            sandbox.path().join("copied.txt"),
        )
        .unwrap();
        run_jj(sandbox.path(), &["describe", "-m", "rename-and-copy"]);
        run_jj(sandbox.path(), &["new", "-m", "working"]);

        let plugin = initialize_jj_plugin(sandbox.path(), sandbox.path(), "master").await;
        let impacts = plugin
            .get_impacts(moon_pdk_api::VcsImpactIntent::Submission {
                base: Some("master".into()),
                head: None,
                include_working: false,
            })
            .await
            .unwrap();

        assert_eq!(
            impacts.changes[Path::new("old.txt")],
            moon_pdk_api::VcsChangeMask::DELETED | moon_pdk_api::VcsChangeMask::RECORDED
        );
        for path in ["renamed.txt", "copied.txt"] {
            assert_eq!(
                impacts.changes[Path::new(path)],
                moon_pdk_api::VcsChangeMask::ADDED | moon_pdk_api::VcsChangeMask::RECORDED
            );
        }
        assert!(!impacts.changes.contains_key(Path::new("source.txt")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_instance_initializes_only_once() {
        let sandbox = create_git_repository();
        let plugin = load_bundled_vcs_plugin(
            create_host_data(sandbox.path()),
            "git",
            "data://vcs_git",
            include_bytes!("../res/vcs_git.wasm"),
        )
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
    async fn bundled_git_provider_includes_clean_submodule_commit_changes() {
        let submodule = create_git_repository();
        let previous = git_output(submodule.path(), &["rev-parse", "HEAD"]);
        submodule.create_file("second.txt", "second");
        submodule.run_git(|command| {
            command.args(["add", "."]);
        });
        submodule.run_git(|command| {
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
        let checked_out_submodule = sandbox.path().join("modules/child");
        assert!(
            Command::new("git")
                .args(["checkout", "--detach", &previous])
                .current_dir(&checked_out_submodule)
                .status()
                .unwrap()
                .success()
        );
        assert!(git_output(&checked_out_submodule, &["status", "--porcelain"]).is_empty());

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

        assert_eq!(
            changed
                .files
                .get(&WorkspaceRelativePathBuf::from("modules/child/second.txt")),
            Some(&vec![ChangedStatus::Deleted, ChangedStatus::Unstaged])
        );
        assert!(
            !changed
                .files
                .contains_key(&WorkspaceRelativePathBuf::from("modules/child"))
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

        assert_eq!(
            adapter
                .get_repository_root()
                .unwrap()
                .canonicalize()
                .unwrap(),
            sandbox.path().canonicalize().unwrap()
        );
        assert_eq!(
            adapter.get_working_root().unwrap().canonicalize().unwrap(),
            sandbox.path().canonicalize().unwrap()
        );
        assert!(!adapter.is_worktree());

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
