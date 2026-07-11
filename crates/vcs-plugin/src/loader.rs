use crate::{VcsPlugin, adapter::VcsPluginAdapter};
use miette::IntoDiagnostic;
use moon_pdk_api::{
    DetectVcsInput, GetVcsStateInput, MoonContext, PrepareVcsInput, VcsConsistency,
};
use moon_plugin::{MoonHostData, PluginLocator, PluginRegistry, PluginType};
use moon_vcs::{BoxedVcs, Vcs, git::Git};
use serde::{Deserialize, Serialize};
use starbase_utils::hash;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};
use warpgate::Id;

const USER_CONFIG_FILE: &str = "vcs.json";

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
    validate_config(&mut config)?;

    Ok(Some(config))
}

pub async fn load_user_vcs_adapter(
    base: Git,
    host_data: MoonHostData,
    working_dir: &Path,
    workspace_root: &Path,
) -> miette::Result<BoxedVcs> {
    let config_path = get_user_vcs_config_path(&host_data);
    let Some(config) = load_user_vcs_config(&config_path)? else {
        return Ok(Box::new(base));
    };

    if !config.enabled {
        return Ok(Box::new(base));
    }

    let plugin = load_verified_vcs_plugin(host_data, config.plugin, &config.sha256).await?;
    let context = MoonContext {
        working_dir: plugin.to_virtual_path(working_dir),
        workspace_root: plugin.to_virtual_path(workspace_root),
    };
    let detection = plugin
        .detect(DetectVcsInput {
            context: context.clone(),
        })
        .await?;

    if !detection.active {
        debug!(reason = detection.reason, "VCS plugin is not active");

        return Ok(Box::new(base));
    }

    let default_branch = base.get_default_branch().await?;
    let prepared = plugin
        .prepare(PrepareVcsInput {
            context: context.clone(),
            consistency: VcsConsistency::FreshSnapshot,
        })
        .await?;
    let state = plugin
        .get_state(GetVcsStateInput {
            context: context.clone(),
            default_branch: (*default_branch).clone(),
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await?;

    debug!(
        plugin = plugin.metadata.name,
        reason = detection.reason,
        "Activated user VCS plugin"
    );

    Ok(Box::new(VcsPluginAdapter::new(
        base,
        context,
        default_branch,
        plugin,
        prepared,
        state,
    )))
}

pub async fn load_verified_vcs_plugin(
    host_data: MoonHostData,
    locator: PluginLocator,
    expected_sha256: &str,
) -> miette::Result<Arc<VcsPlugin>> {
    let registry = PluginRegistry::new(PluginType::Vcs, host_data)?;
    let expected_sha256 = expected_sha256.to_owned();

    registry
        .load_verified_with_config(
            Id::raw("user-vcs"),
            locator,
            move |wasm_file, bytes| verify_sha256(wasm_file, bytes, &expected_sha256),
            |manifest| {
                if manifest.allowed_hosts.is_some() || manifest.allowed_paths.is_some() {
                    warn!("Ignoring filesystem and network access requested by VCS plugin");
                }

                manifest.allowed_hosts = Some(vec![]);
                manifest.allowed_paths = Some(Default::default());
                manifest.timeout_ms = Some(120_000);

                Ok(())
            },
        )
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
    use starbase_sandbox::create_empty_sandbox;
    use warpgate::FileLocator;

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
}
