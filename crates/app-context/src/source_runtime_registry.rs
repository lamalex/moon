use crate::AppContext;
use miette::Diagnostic;
use moon_common::{SourceRootId, path::WorkspaceRelativePathBuf};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum SourceRuntimeRegistryError {
    #[error("Source runtime {id} has already been registered.")]
    DuplicateSource { id: SourceRootId },

    #[error("Source runtime {id} contains a context for {context_id}.")]
    IdentityMismatch {
        id: SourceRootId,
        context_id: SourceRootId,
    },

    #[error("Source runtime {id} is not registered.")]
    UnknownSource { id: SourceRootId },

    #[error("Source runtime {id} is unavailable: {reason}")]
    UnavailableSource { id: SourceRootId, reason: Arc<str> },
}

#[derive(Clone, Debug)]
pub enum SourceRuntime {
    Available(Arc<AppContext>),
    Unavailable(Arc<str>),
}

#[derive(Clone, Debug)]
pub struct SourceRuntimeRegistry {
    entries: BTreeMap<SourceRootId, SourceRuntime>,
    primary_id: SourceRootId,
}

impl SourceRuntimeRegistry {
    pub fn single(primary: Arc<AppContext>) -> Self {
        let primary_id = primary.source_id.clone();
        let mut entries = BTreeMap::new();
        entries.insert(primary_id.clone(), SourceRuntime::Available(primary));

        Self {
            entries,
            primary_id,
        }
    }

    pub fn new(
        primary: Arc<AppContext>,
        runtimes: impl IntoIterator<Item = (SourceRootId, SourceRuntime)>,
    ) -> miette::Result<Self> {
        let primary_id = primary.source_id.clone();
        Self::from_entries(
            primary_id.clone(),
            std::iter::once((primary_id, SourceRuntime::Available(primary))).chain(runtimes),
        )
    }

    fn from_entries(
        primary_id: SourceRootId,
        runtimes: impl IntoIterator<Item = (SourceRootId, SourceRuntime)>,
    ) -> miette::Result<Self> {
        let mut entries = BTreeMap::new();

        for (id, runtime) in runtimes {
            if entries.contains_key(&id) {
                return Err(SourceRuntimeRegistryError::DuplicateSource { id }.into());
            }

            if let SourceRuntime::Available(context) = &runtime
                && context.source_id != id
            {
                return Err(SourceRuntimeRegistryError::IdentityMismatch {
                    id,
                    context_id: context.source_id.clone(),
                }
                .into());
            }

            entries.insert(id, runtime);
        }

        Ok(Self {
            entries,
            primary_id,
        })
    }

    pub fn get(&self, id: &SourceRootId) -> Result<&Arc<AppContext>, SourceRuntimeRegistryError> {
        match self.entries.get(id) {
            Some(SourceRuntime::Available(context)) => Ok(context),
            Some(SourceRuntime::Unavailable(reason)) => {
                Err(SourceRuntimeRegistryError::UnavailableSource {
                    id: id.clone(),
                    reason: Arc::clone(reason),
                })
            }
            None => Err(SourceRuntimeRegistryError::UnknownSource { id: id.clone() }),
        }
    }

    pub fn get_primary(&self) -> &Arc<AppContext> {
        self.get(&self.primary_id)
            .expect("Primary source runtime must always be available.")
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SourceRootId, &SourceRuntime)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub async fn hash_files_for_source(
        &self,
        source_id: &SourceRootId,
        files: &[WorkspaceRelativePathBuf],
    ) -> miette::Result<BTreeMap<WorkspaceRelativePathBuf, String>> {
        self.get(source_id)?.hash_files(files).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_unknown_and_unavailable_sources() {
        let primary_id = SourceRootId::primary();
        let child_id = SourceRootId::new("child").unwrap();
        let unknown_id = SourceRootId::new("unknown").unwrap();
        let mut registry = SourceRuntimeRegistry {
            entries: BTreeMap::new(),
            primary_id,
        };
        registry.entries.insert(
            child_id.clone(),
            SourceRuntime::Unavailable("VCS failed".into()),
        );

        assert!(
            registry
                .get(&child_id)
                .unwrap_err()
                .to_string()
                .contains("VCS failed")
        );
        assert!(
            registry
                .get(&unknown_id)
                .unwrap_err()
                .to_string()
                .contains("not registered")
        );
    }

    #[test]
    fn rejects_duplicate_entries_before_becoming_read_only() {
        let id = SourceRootId::new("child").unwrap();
        let entries = [
            (id.clone(), SourceRuntime::Unavailable("first".into())),
            (id, SourceRuntime::Unavailable("second".into())),
        ];

        assert!(SourceRuntimeRegistry::from_entries(SourceRootId::primary(), entries).is_err());
    }
}
