use crate::{
    Id,
    path::{WorkspaceRelativePathBuf, clean_components},
};
use miette::{Diagnostic, IntoDiagnostic};
use rustc_hash::FxHashMap;
use schematic::{Schema, SchemaBuilder, Schematic};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, path::Path, path::PathBuf, str::FromStr};
use thiserror::Error;

pub const PRIMARY_SOURCE_ROOT_ID: &str = "workspace";

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceAlias(Id);

impl SourceAlias {
    pub fn new(id: impl AsRef<str>) -> miette::Result<Self> {
        Ok(Self(Id::new(id.as_ref())?))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for SourceAlias {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SourceAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceRootId(Id);

impl SourceRootId {
    pub fn new(id: impl AsRef<str>) -> miette::Result<Self> {
        Ok(Self(Id::new(id.as_ref())?))
    }

    pub fn primary() -> Self {
        Self(Id::raw(PRIMARY_SOURCE_ROOT_ID))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for SourceRootId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Default for SourceRootId {
    fn default() -> Self {
        Self::primary()
    }
}

impl fmt::Display for SourceRootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<SourceRootId> for Id {
    fn from(id: SourceRootId) -> Self {
        id.0
    }
}

impl FromStr for SourceRootId {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Schematic for SourceRootId {
    fn build_schema(mut schema: SchemaBuilder) -> Schema {
        schema.string_default()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourcePathBuf {
    pub source: SourceRootId,
    pub path: WorkspaceRelativePathBuf,
}

impl SourcePathBuf {
    pub fn new(source: SourceRootId, path: impl Into<WorkspaceRelativePathBuf>) -> Self {
        Self {
            source,
            path: path.into(),
        }
    }

    pub fn primary(path: impl Into<WorkspaceRelativePathBuf>) -> Self {
        Self::new(SourceRootId::primary(), path)
    }
}

impl fmt::Display for SourcePathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.source, self.path)
    }
}

impl FromStr for SourcePathBuf {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((source, path)) = value.split_once("::") else {
            return Err(miette::miette!(
                "Qualified source path must use the format <source>::<path>."
            ));
        };

        Ok(Self::new(source.parse()?, path))
    }
}

impl<'de> Deserialize<'de> for SourcePathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for SourcePathBuf {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum SourceRegistryError {
    #[error("Source root {id} has already been registered.")]
    DuplicateSourceRoot { id: SourceRootId },

    #[error("Source root path {root:?} has already been registered as {id}.")]
    DuplicateSourceRootPath { id: SourceRootId, root: PathBuf },

    #[error("Source root ID {id} is reserved for primary-workspace compatibility.")]
    ReservedSourceRoot { id: SourceRootId },

    #[error("Source root {id} has not been registered.")]
    #[diagnostic(help("Register the source root before resolving source-qualified paths."))]
    UnknownSourceRoot { id: SourceRootId },

    #[error("Path {path:?} is not contained by a registered source root.")]
    UnqualifiedPath { path: PathBuf },

    #[error("Path {path} escapes source root {id}.")]
    PathEscapesSourceRoot {
        id: SourceRootId,
        path: WorkspaceRelativePathBuf,
    },
}

#[derive(Clone, Debug)]
pub struct SourceRegistry {
    primary: SourceRootId,
    roots: FxHashMap<SourceRootId, PathBuf>,
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::single(PathBuf::new())
    }
}

impl SourceRegistry {
    pub fn new(primary: SourceRootId, root: PathBuf) -> Self {
        let mut roots = FxHashMap::default();
        roots.insert(primary.clone(), normalize_root(&root));

        Self { primary, roots }
    }

    pub fn single(root: PathBuf) -> Self {
        Self::new(SourceRootId::primary(), root)
    }

    pub fn get(&self, id: &SourceRootId) -> miette::Result<&Path> {
        let id = if id.as_str() == PRIMARY_SOURCE_ROOT_ID {
            &self.primary
        } else {
            id
        };

        self.roots
            .get(id)
            .map(PathBuf::as_path)
            .ok_or_else(|| SourceRegistryError::UnknownSourceRoot { id: id.clone() }.into())
    }

    pub fn get_primary(&self) -> &Path {
        self.roots
            .get(&self.primary)
            .expect("Primary source root must be registered.")
    }

    pub fn primary_id(&self) -> &SourceRootId {
        &self.primary
    }

    pub fn iter(&self) -> impl Iterator<Item = (&SourceRootId, &Path)> {
        self.roots.iter().map(|(id, root)| (id, root.as_path()))
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn qualify(&self, path: &Path) -> miette::Result<SourcePathBuf> {
        let path = clean_components(path);
        let Some((source, root)) = self
            .roots
            .iter()
            .filter(|(_, root)| path.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
        else {
            return Err(SourceRegistryError::UnqualifiedPath { path }.into());
        };

        let relative = path
            .strip_prefix(root)
            .expect("Qualified path must start with the matched source root.");

        Ok(SourcePathBuf::new(
            source.clone(),
            WorkspaceRelativePathBuf::from_path(relative).into_diagnostic()?,
        ))
    }

    pub fn register(&mut self, id: SourceRootId, root: PathBuf) -> miette::Result<()> {
        if id.as_str() == PRIMARY_SOURCE_ROOT_ID && id != self.primary {
            return Err(SourceRegistryError::ReservedSourceRoot { id }.into());
        }

        if self.roots.contains_key(&id) {
            return Err(SourceRegistryError::DuplicateSourceRoot { id }.into());
        }

        let root = normalize_root(&root);

        if let Some((existing_id, _)) = self.roots.iter().find(|(_, existing)| **existing == root) {
            return Err(SourceRegistryError::DuplicateSourceRootPath {
                id: existing_id.clone(),
                root,
            }
            .into());
        }

        self.roots.insert(id, root);

        Ok(())
    }

    pub fn resolve(&self, path: &SourcePathBuf) -> miette::Result<PathBuf> {
        let root = self.get(&path.source)?;
        let resolved = clean_components(path.path.to_logical_path(root));

        if !resolved.starts_with(root) {
            return Err(SourceRegistryError::PathEscapesSourceRoot {
                id: path.source.clone(),
                path: path.path.clone(),
            }
            .into());
        }

        Ok(resolved)
    }
}

fn normalize_root(root: &Path) -> PathBuf {
    let root = clean_components(root);

    if root == Path::new(".") {
        PathBuf::new()
    } else {
        root
    }
}
