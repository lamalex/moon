use crate::host::*;
use crate::plugin::{Plugin, PluginRegistration, PluginType};
use crate::plugin_error::PluginError;
use crate::plugin_registry::*;
use futures::StreamExt;
use futures::stream::FuturesOrdered;
use miette::IntoDiagnostic;
use moon_common::{Id, IdExt};
use scc::hash_map::Entry;
use starbase_utils::fs;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, instrument};
use warpgate::{PluginContainer, PluginLocator, PluginManifest, Wasm, host::HostData};

impl<Cfg: PluginsConfig, Inst: Plugin> PluginRegistry<Cfg, Inst> {
    pub async fn load<I>(&self, id: I) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str>,
    {
        let id = Id::raw(id.as_ref());

        if !self.is_registered(&id).await {
            if self.config_data.get_locator(&id).is_none() {
                return Err(PluginError::UnknownId {
                    id: id.to_string(),
                    ty: self.type_of,
                }
                .into());
            }

            return Ok(self.load_many([&id]).await?.remove(0));
        }

        self.get_instance(&id).await
    }

    pub async fn load_all(&self) -> miette::Result<Vec<Arc<Inst>>> {
        let ids = self.config_data.get_ids();

        if ids.is_empty() {
            return Ok(vec![]);
        }

        debug!(
            plugin_type = self.type_of.get_label(),
            "Loading all plugins"
        );

        self.load_many(ids).await
    }

    pub async fn load_many<It, I>(&self, ids: It) -> miette::Result<Vec<Arc<Inst>>>
    where
        It: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let ids = ids
            .into_iter()
            .map(|id| Id::raw(id.as_ref()))
            .collect::<Vec<_>>();
        let mut list = vec![];

        // First check if all of the requested plugins are already registered,
        // and if so, return them immediately
        for id in &ids {
            if self.is_registered(id).await {
                list.push(self.get_instance(id).await?);
            }
        }

        if list.len() == ids.len() {
            return Ok(list);
        } else {
            list.clear();
        }

        // Otherwise load all the plugins in parallel. Use ordered futures
        // (over spawned tasks) so that results are returned in the order
        // they were requested, which downstream operations rely on for
        // determinism, like hashing
        let mut futures = FuturesOrdered::new();

        for id in ids {
            let Some(locator) = self.config_data.get_locator(&id) else {
                continue;
            };

            let registry = self.to_owned();
            let locator = locator.to_owned();

            futures.push_back(tokio::spawn(Box::pin(async move {
                registry.do_load(&id, locator).await
            })));
        }

        while let Some(result) = futures.next().await {
            list.push(result.into_diagnostic()??);
        }

        Ok(list)
    }

    #[instrument(skip(self))]
    pub async fn do_load<I, L>(&self, id: I, locator: L) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
    {
        self.load_with_config_and_verifier(
            id,
            locator,
            false,
            |_, _| Ok(()),
            |id, host_data, manifest| self.config_data.configure_manifest(id, host_data, manifest),
        )
        .await
    }

    #[instrument(skip(self, op))]
    pub async fn load_with_config<I, L, F>(
        &self,
        id: I,
        locator: L,
        op: F,
    ) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
        F: FnMut(&mut PluginManifest) -> miette::Result<()>,
    {
        let mut op = op;

        self.load_with_config_and_verifier(
            id,
            locator,
            false,
            |_, _| Ok(()),
            |_, _, manifest| op(manifest),
        )
        .await
    }

    pub async fn load_verified_with_config<I, L, V, F>(
        &self,
        id: I,
        locator: L,
        verify: V,
        op: F,
    ) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
        V: FnMut(&Path, &[u8]) -> miette::Result<()>,
        F: FnMut(&mut PluginManifest) -> miette::Result<()>,
    {
        let mut op = op;

        self.load_with_config_and_verifier(id, locator, true, verify, |_, _, manifest| op(manifest))
            .await
    }

    async fn load_with_config_and_verifier<I, L, V, F>(
        &self,
        id: I,
        locator: L,
        verify_registered: bool,
        mut verify: V,
        mut op: F,
    ) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
        V: FnMut(&Path, &[u8]) -> miette::Result<()>,
        F: FnMut(&Id, &MoonHostData, &mut PluginManifest) -> miette::Result<()>,
    {
        let id = Id::raw(id.as_ref());
        let locator = locator.as_ref();

        // Return early if already registered. We must NOT hold a map lock (an
        // scc entry guard) across the expensive, multi-second WASM load below:
        // doing so serializes loads that collide on a bucket and can deadlock
        // under concurrent loads (e.g. `load_many`), since a guard held across
        // an `.await` blocks other tasks (and map resizes) from making progress.
        let existing = self
            .plugins
            .get_async(&id)
            .await
            .map(|entry| Arc::clone(entry.get()));

        if !verify_registered && let Some(existing) = existing {
            return Ok(existing);
        }

        // Verified loads must check the acquired file even if an instance is
        // already registered under this ID.
        let plugin_file = self.loader.load_plugin(&id, locator).await?;
        let verified_bytes = if verify_registered {
            let bytes = std::fs::read(&plugin_file).into_diagnostic()?;
            verify(&plugin_file, &bytes)?;
            Some(bytes)
        } else {
            None
        };

        if existing.is_some() {
            return Err(PluginError::ExistingId {
                id: id.to_string(),
                ty: self.type_of,
            }
            .into());
        }

        debug!(
            plugin_type = self.type_of.get_label(),
            id = id.as_str(),
            "Attempting to load and register plugin",
        );

        let process_host_access =
            matches!(self.type_of, PluginType::Vcs).then(ProcessHostAccess::default);

        // Create host functions (provided by warpgate)
        let functions = create_host_functions(
            self.type_of,
            self.host_data.clone(),
            HostData {
                cache_dir: self.host_data.moon_env.cache_dir.clone(),
                http_client: self.loader.get_http_client()?.clone(),
                virtual_paths: self.virtual_paths.clone(),
                working_dir: self.host_data.moon_env.working_dir.clone(),
            },
            process_host_access.clone(),
        );

        // Create the manifest and let the consumer configure it
        let mut manifest = if let Some(bytes) = verified_bytes {
            self.create_manifest_with_wasm(&id, Wasm::data(bytes))?
        } else {
            self.create_manifest(&id, plugin_file.clone())?
        };

        // VCS sandbox policy is host-owned. Configure it before the callback so
        // consumers can inspect it, then reject attempts to change it.
        if matches!(self.type_of, PluginType::Vcs) {
            manifest.allowed_hosts = Some(vec![]);
            manifest.allowed_paths = Some(Default::default());
            manifest.timeout_ms = Some(VCS_PLUGIN_TIMEOUT_MS);
        }

        let vcs_policy = matches!(self.type_of, PluginType::Vcs).then(|| {
            (
                manifest.allowed_hosts.clone(),
                manifest.allowed_paths.clone(),
                manifest.timeout_ms,
            )
        });

        op(&id, &self.host_data, &mut manifest)?;

        if let Some((allowed_hosts, allowed_paths, timeout_ms)) = vcs_policy
            && (manifest.allowed_hosts != allowed_hosts
                || manifest.allowed_paths != allowed_paths
                || manifest.timeout_ms != timeout_ms)
        {
            return Err(miette::miette!(
                "VCS plugin network, filesystem, and timeout policy is host-owned"
            ));
        }

        // Ensure the final set of virtual host paths exists, otherwise WASI
        // (via extism) will throw a cryptic file/directory not found error.
        if let Some(paths) = &manifest.allowed_paths {
            for host_path in paths.keys() {
                fs::create_dir_all(host_path)?;
            }
        }

        debug!(
            plugin_type = self.type_of.get_label(),
            id = id.as_str(),
            "Updated plugin manifest, attempting to register plugin",
        );

        // Create a new ID for the WASM manifest if it's prefixed with
        // "unstable_". The reason for this is that proto's built-in tools
        // expect a specific ID, for example "rust", and if we provide
        // "unstable_rust", it breaks in weird ways.
        let stable_id = Id::stable(id.as_str());

        // Combine everything into the container and register
        let plugin = Inst::new(PluginRegistration {
            container: PluginContainer::new(stable_id.clone(), manifest, functions)?,
            locator: locator.to_owned(),
            id: id.clone(),
            id_stable: stable_id,
            moon_env: Arc::clone(&self.host_data.moon_env),
            proto_env: Arc::clone(&self.host_data.proto_env),
            process_host_access,
            wasm_file: plugin_file,
        })
        .await?;

        debug!(
            plugin_type = self.type_of.get_label(),
            id = id.as_str(),
            "Registered plugin",
        );

        let instance = Arc::new(plugin);

        // Insert into the registry, holding the bucket lock only around the
        // synchronous insert (never across an `.await`). If another task loaded
        // the same plugin concurrently, discard ours and use the race winner.
        match self.plugins.entry_async(id.clone()).await {
            Entry::Occupied(_) if verify_registered => Err(PluginError::ExistingId {
                id: id.to_string(),
                ty: self.type_of,
            }
            .into()),
            Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
            Entry::Vacant(entry) => {
                entry.insert_entry(Arc::clone(&instance));
                Ok(instance)
            }
        }
    }

    pub async fn load_without_config<I, L>(&self, id: I, locator: L) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
    {
        self.load_with_config(id, locator, |_| Ok(())).await
    }

    pub async fn load_verified_without_config<I, L, V>(
        &self,
        id: I,
        locator: L,
        verify: V,
    ) -> miette::Result<Arc<Inst>>
    where
        I: AsRef<str> + Debug,
        L: AsRef<PluginLocator> + Debug,
        V: FnMut(&Path, &[u8]) -> miette::Result<()>,
    {
        self.load_verified_with_config(id, locator, verify, |_| Ok(()))
            .await
    }
}
