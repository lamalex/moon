use async_trait::async_trait;
use miette::IntoDiagnostic;
use moon_pdk_api::*;
use moon_plugin::{Plugin, PluginContainer, PluginRegistration, PluginType};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use warpgate::{Id, VirtualPath};

const QUERY_CACHE_CAPACITY: usize = 32;

#[derive(Default)]
struct QueryCache {
    impacts: VecDeque<(String, GetVcsImpactsOutput)>,
}

pub struct VcsPlugin {
    pub id: Id,
    pub metadata: RegisterVcsOutput,
    supports_hook_environment: bool,
    operation_lock: Mutex<()>,
    plugin: Arc<PluginContainer>,
    initialization_claimed: Mutex<bool>,
}

/// A VCS plugin that has completed its single invocation-scoped initialization.
///
/// Only this handle exposes state-dependent operations, so typed host callers
/// cannot query impacts or manage hooks before initialization.
pub struct InitializedVcsPlugin {
    plugin: Arc<VcsPlugin>,
    initialization: VcsInitialization,
    context: MoonContext,
    query_cache: Mutex<QueryCache>,
}

pub enum VcsPluginInitialization {
    NotDetected { reason: String },
    Initialized(Arc<InitializedVcsPlugin>),
}

#[async_trait]
impl Plugin for VcsPlugin {
    async fn new(mut registration: PluginRegistration) -> miette::Result<Self> {
        let process_host_access = registration.take_process_host_access()?;
        let workspace_root = registration.moon_env.workspace_root.clone();
        let plugin = Arc::new(registration.container);
        let metadata: RegisterVcsOutput = plugin
            .cache_func_with(
                "register_vcs",
                RegisterVcsInput {
                    id: registration.id.clone(),
                    host_protocol_version: VCS_PLUGIN_PROTOCOL_VERSION,
                },
            )
            .await?;

        let supports_hook_environment = validate_metadata(&plugin, &metadata).await?;
        process_host_access.configure(&metadata.process_capabilities, &workspace_root)?;

        Ok(Self {
            id: registration.id,
            metadata,
            supports_hook_environment,
            operation_lock: Mutex::new(()),
            plugin,
            initialization_claimed: Mutex::new(false),
        })
    }

    fn get_id(&self) -> &Id {
        &self.id
    }

    fn get_type() -> PluginType {
        PluginType::Vcs
    }

    async fn has_func(&self, name: &str) -> bool {
        self.plugin.has_func(name).await
    }
}

async fn validate_metadata(
    plugin: &PluginContainer,
    metadata: &RegisterVcsOutput,
) -> miette::Result<bool> {
    validate_protocol_version(metadata)?;

    if metadata.name.trim().is_empty() || metadata.plugin_version.trim().is_empty() {
        return Err(miette::miette!(
            "VCS plugin registration must provide a name and plugin version"
        ));
    }

    for function in ["initialize_vcs", "get_vcs_impacts"] {
        if !plugin.has_func(function).await {
            return Err(miette::miette!(
                "VCS plugin `{}` does not export required function `{function}`",
                metadata.name
            ));
        }
    }

    let setup_hooks = plugin.has_func("setup_vcs_hook_environment").await;
    let teardown_hooks = plugin.has_func("teardown_vcs_hook_environment").await;

    if setup_hooks != teardown_hooks {
        return Err(miette::miette!(
            "VCS plugin `{}` must export both hook-environment functions or neither",
            metadata.name
        ));
    }

    Ok(setup_hooks)
}

fn validate_protocol_version(metadata: &RegisterVcsOutput) -> miette::Result<()> {
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
    pub async fn initialize(
        self: Arc<Self>,
        input: InitializeVcsInput,
    ) -> miette::Result<VcsPluginInitialization> {
        let _operation = self.operation_lock.lock().await;
        let mut claimed = self.initialization_claimed.lock().await;

        if *claimed {
            return Err(miette::miette!(
                "VCS plugin `{}` has already attempted initialization; load a new plugin instance to initialize new state",
                self.id
            ));
        }

        // Initialization may mutate provider state, so even a failed
        // attempt cannot safely be retried on this plugin instance.
        *claimed = true;
        drop(claimed);

        let context = input.context.clone();
        let output: InitializeVcsOutput =
            self.plugin.call_func_with("initialize_vcs", input).await?;
        let initialization = match output {
            InitializeVcsOutput::NotDetected { reason } => {
                return Ok(VcsPluginInitialization::NotDetected { reason });
            }
            InitializeVcsOutput::Initialized { initialization } => *initialization,
        };
        validate_initialization(&context, &initialization)?;
        drop(_operation);

        Ok(VcsPluginInitialization::Initialized(Arc::new(
            InitializedVcsPlugin {
                plugin: self,
                initialization,
                context,
                query_cache: Mutex::new(QueryCache::default()),
            },
        )))
    }

    pub fn to_virtual_path(&self, path: impl AsRef<Path> + fmt::Debug) -> VirtualPath {
        self.plugin.to_virtual_path(path)
    }
}

impl InitializedVcsPlugin {
    pub fn initialization(&self) -> &VcsInitialization {
        &self.initialization
    }

    pub fn context(&self) -> &MoonContext {
        &self.context
    }

    pub fn metadata(&self) -> &RegisterVcsOutput {
        &self.plugin.metadata
    }

    pub fn supports_hook_environment(&self) -> bool {
        self.plugin.supports_hook_environment
    }

    pub fn from_virtual_path(&self, path: impl AsRef<Path> + fmt::Debug) -> PathBuf {
        path.as_ref().to_path_buf()
    }

    pub fn to_virtual_path(&self, path: impl AsRef<Path> + fmt::Debug) -> VirtualPath {
        self.plugin.plugin.to_virtual_path(path)
    }

    pub async fn get_impacts(
        &self,
        intent: VcsImpactIntent,
    ) -> miette::Result<GetVcsImpactsOutput> {
        let key = serde_json::to_string(&intent).into_diagnostic()?;
        let mut cache = self.query_cache.lock().await;

        if let Some(output) = get_cached(&mut cache.impacts, &key) {
            return Ok(output);
        }

        let _operation = self.plugin.operation_lock.lock().await;
        let output: GetVcsImpactsOutput = self
            .plugin
            .plugin
            .call_func_with(
                "get_vcs_impacts",
                GetVcsImpactsInput {
                    context: self.context.clone(),
                    intent,
                },
            )
            .await?;
        validate_impacts(&output)?;
        insert_cached(&mut cache.impacts, key, output.clone());

        Ok(output)
    }

    pub async fn setup_hook_environment(
        &self,
        hooks_dir: VirtualPath,
        hooks: Vec<String>,
    ) -> miette::Result<SetupVcsHookEnvironmentOutput> {
        let _operation = self.plugin.operation_lock.lock().await;
        Ok(self
            .plugin
            .plugin
            .call_func_with(
                "setup_vcs_hook_environment",
                SetupVcsHookEnvironmentInput {
                    context: self.context.clone(),
                    hooks_dir,
                    hooks,
                },
            )
            .await?)
    }

    pub async fn teardown_hook_environment(
        &self,
        hooks_dir: VirtualPath,
        hooks: Vec<String>,
    ) -> miette::Result<TeardownVcsHookEnvironmentOutput> {
        let _operation = self.plugin.operation_lock.lock().await;
        Ok(self
            .plugin
            .plugin
            .call_func_with(
                "teardown_vcs_hook_environment",
                TeardownVcsHookEnvironmentInput {
                    context: self.context.clone(),
                    hooks_dir,
                    hooks,
                },
            )
            .await?)
    }
}

fn validate_initialization(
    context: &MoonContext,
    initialization: &VcsInitialization,
) -> miette::Result<()> {
    if initialization.client.as_str().trim().is_empty() {
        return Err(miette::miette!(
            "source-control provider returned an empty client kind"
        ));
    }

    let workspace_root = context
        .workspace_root
        .as_path()
        .canonicalize()
        .into_diagnostic()?;
    let working_root = initialization
        .roots
        .working_root
        .as_path()
        .canonicalize()
        .into_diagnostic()?;
    initialization
        .roots
        .repository_root
        .as_path()
        .canonicalize()
        .into_diagnostic()?;

    if !workspace_root.starts_with(&working_root) {
        return Err(miette::miette!(
            "source-control provider returned a working root that does not contain the workspace"
        ));
    }

    if initialization.current.id.is_none() != initialization.recorded.id.is_none() {
        return Err(miette::miette!(
            "source-control provider returned inconsistent empty current and recorded state IDs"
        ));
    }

    if initialization
        .current
        .id
        .as_deref()
        .is_some_and(str::is_empty)
        || initialization
            .recorded
            .id
            .as_deref()
            .is_some_and(str::is_empty)
        || initialization
            .baseline
            .as_ref()
            .is_some_and(|state| state.id.as_deref().is_none_or(str::is_empty))
    {
        return Err(miette::miette!(
            "source-control provider returned an empty state ID"
        ));
    }

    Ok(())
}

fn validate_impacts(output: &GetVcsImpactsOutput) -> miette::Result<()> {
    for (path, mask) in &output.changes {
        validate_impact_path(path)?;

        if mask.bits() & !VcsChangeMask::KNOWN_BITS.bits() != 0
            || !mask.intersects(VcsChangeMask::CHANGE_BITS)
            || !mask.intersects(VcsChangeMask::LOCATION_BITS)
        {
            return Err(miette::miette!(
                "source-control provider returned invalid change mask {} for `{}`",
                mask.bits(),
                path.display()
            ));
        }
    }

    Ok(())
}

fn validate_impact_path(path: &Path) -> miette::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| miette::miette!("source-control provider returned a non-UTF-8 path"))?;
    let has_windows_prefix = path
        .as_bytes()
        .get(..2)
        .is_some_and(|prefix| prefix[0].is_ascii_alphabetic() && prefix[1] == b':');

    if path.is_empty()
        || path.starts_with('/')
        || has_windows_prefix
        || path.contains('\\')
        || path.contains('\0')
        || path.split('/').any(|component| {
            matches!(component, "" | "." | "..") || invalid_host_component(component)
        })
    {
        return Err(miette::miette!(
            "source-control provider returned non-canonical workspace-relative path `{path}`"
        ));
    }

    Ok(())
}

#[cfg(not(windows))]
fn invalid_host_component(_component: &str) -> bool {
    false
}

#[cfg(windows)]
fn invalid_host_component(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });

    reserved
        || component.ends_with(' ')
        || component.ends_with('.')
        || component.chars().any(|char| {
            char.is_control() || matches!(char, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
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

impl fmt::Debug for VcsPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VcsPlugin")
            .field("id", &self.id)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl fmt::Debug for InitializedVcsPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitializedVcsPlugin")
            .field("id", &self.plugin.id)
            .field("metadata", &self.plugin.metadata)
            .field("initialization", &self.initialization)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incompatible_protocol_versions() {
        let metadata = RegisterVcsOutput {
            protocol_version: VCS_PLUGIN_PROTOCOL_VERSION + 1,
            ..Default::default()
        };

        assert!(validate_protocol_version(&metadata).is_err());
    }

    #[test]
    fn reuses_cached_query_results() {
        let mut entries = VecDeque::new();
        let output = GetVcsImpactsOutput {
            diagnostics: vec!["cached".into()],
            ..Default::default()
        };

        insert_cached(&mut entries, "request".into(), output.clone());

        assert_eq!(get_cached(&mut entries, "request"), Some(output));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn rejects_non_canonical_impact_paths() {
        for path in [
            "",
            "/absolute",
            "C:/absolute",
            "../outside",
            "nested/../../outside",
            "nested//file",
            "nested/./file",
            "nested\\file",
        ] {
            assert!(
                validate_impact_path(Path::new(path)).is_err(),
                "accepted `{path}`"
            );
        }

        assert!(validate_impact_path(Path::new("project/src/main.rs")).is_ok());

        #[cfg(not(windows))]
        assert!(validate_impact_path(Path::new("file.txt:stream")).is_ok());

        #[cfg(windows)]
        for path in ["file.txt:stream", "NUL", "COM1.txt", "trailing."] {
            assert!(
                validate_impact_path(Path::new(path)).is_err(),
                "accepted `{path}`"
            );
        }
    }

    #[test]
    fn rejects_unknown_or_incomplete_change_masks() {
        for mask in [
            VcsChangeMask::from_bits_retain(128),
            VcsChangeMask::ADDED,
            VcsChangeMask::WORKING,
            VcsChangeMask::from_bits_retain(
                VcsChangeMask::ADDED.bits() | VcsChangeMask::WORKING.bits() | 128,
            ),
        ] {
            let output = GetVcsImpactsOutput {
                changes: [(PathBuf::from("file.txt"), mask)].into(),
                completeness: VcsImpactCompleteness::Exact,
                diagnostics: vec![],
            };

            assert!(validate_impacts(&output).is_err());
        }
    }
}
