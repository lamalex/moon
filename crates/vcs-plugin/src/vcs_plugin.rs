use async_trait::async_trait;
use miette::IntoDiagnostic;
use moon_pdk_api::*;
use moon_plugin::{Plugin, PluginContainer, PluginRegistration, PluginType};
use std::collections::VecDeque;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::Mutex;
use warpgate::Id;

const QUERY_CACHE_CAPACITY: usize = 32;

#[derive(Default)]
struct QueryCache {
    states: VecDeque<(String, VcsStatePatch)>,
    changed_files: VecDeque<(String, GetVcsChangedFilesOutput)>,
}

pub struct VcsPlugin {
    pub id: Id,
    pub metadata: VcsPluginMetadata,
    plugin: Arc<PluginContainer>,
    query_cache: Mutex<QueryCache>,
}

#[async_trait]
impl Plugin for VcsPlugin {
    async fn new(registration: PluginRegistration) -> miette::Result<Self> {
        let plugin = Arc::new(registration.container);
        let metadata: VcsPluginMetadata = plugin
            .cache_func_with(
                "register_vcs",
                RegisterVcsInput {
                    id: registration.id.clone(),
                    host_protocol_version: VCS_PLUGIN_PROTOCOL_VERSION,
                },
            )
            .await?;

        validate_protocol_version(&metadata)?;

        Ok(Self {
            id: registration.id,
            metadata,
            plugin,
            query_cache: Mutex::new(QueryCache::default()),
        })
    }

    fn get_id(&self) -> &Id {
        &self.id
    }

    fn get_type(&self) -> PluginType {
        PluginType::Vcs
    }
}

fn validate_protocol_version(metadata: &VcsPluginMetadata) -> miette::Result<()> {
    if metadata.protocol_version != VCS_PLUGIN_PROTOCOL_VERSION {
        return Err(miette::miette!(
            "VCS plugin protocol version {} is incompatible with host version {}",
            metadata.protocol_version,
            VCS_PLUGIN_PROTOCOL_VERSION
        ));
    }

    Ok(())
}

impl VcsPlugin {
    pub async fn detect(&self, input: DetectVcsInput) -> miette::Result<DetectVcsOutput> {
        Ok(self.plugin.call_func_with("detect_vcs", input).await?)
    }

    pub async fn prepare(&self, input: PrepareVcsInput) -> miette::Result<PreparedVcs> {
        Ok(self.plugin.call_func_with("prepare_vcs", input).await?)
    }

    pub async fn get_state(&self, input: GetVcsStateInput) -> miette::Result<VcsStatePatch> {
        let key = serde_json::to_string(&input).into_diagnostic()?;

        if let Some(output) = get_cached(&mut self.query_cache.lock().await.states, &key) {
            return Ok(output);
        }

        let output: VcsStatePatch = self.plugin.call_func_with("get_vcs_state", input).await?;
        insert_cached(
            &mut self.query_cache.lock().await.states,
            key,
            output.clone(),
        );

        Ok(output)
    }

    pub async fn get_changed_files(
        &self,
        input: GetVcsChangedFilesInput,
    ) -> miette::Result<GetVcsChangedFilesOutput> {
        let key = serde_json::to_string(&input).into_diagnostic()?;

        if let Some(output) = get_cached(&mut self.query_cache.lock().await.changed_files, &key) {
            return Ok(output);
        }

        let output: GetVcsChangedFilesOutput = self
            .plugin
            .call_func_with("get_vcs_changed_files", input)
            .await?;
        insert_cached(
            &mut self.query_cache.lock().await.changed_files,
            key,
            output.clone(),
        );

        Ok(output)
    }
}

fn get_cached<T: Clone>(entries: &mut VecDeque<(String, T)>, key: &str) -> Option<T> {
    let index = entries.iter().position(|(entry_key, _)| entry_key == key)?;
    let entry = entries.remove(index)?;
    let output = entry.1.clone();
    entries.push_back(entry);

    Some(output)
}

fn insert_cached<T>(entries: &mut VecDeque<(String, T)>, key: String, output: T) {
    if let Some(index) = entries.iter().position(|(entry_key, _)| entry_key == &key) {
        entries.remove(index);
    } else if entries.len() == QUERY_CACHE_CAPACITY {
        entries.pop_front();
    }

    entries.push_back((key, output));
}

impl Deref for VcsPlugin {
    type Target = PluginContainer;

    fn deref(&self) -> &Self::Target {
        &self.plugin
    }
}

impl fmt::Debug for VcsPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VcsPlugin")
            .field("id", &self.id)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incompatible_protocol_versions() {
        let metadata = VcsPluginMetadata {
            protocol_version: VCS_PLUGIN_PROTOCOL_VERSION + 1,
            ..Default::default()
        };

        assert!(validate_protocol_version(&metadata).is_err());
    }
}
