use moon_common::{Id, SourceRootId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectKey {
    source: SourceRootId,
    project: Id,
}

impl ProjectKey {
    pub fn new(source: SourceRootId, project: Id) -> miette::Result<Self> {
        Id::new(project.as_str())?;

        Ok(Self { source, project })
    }

    pub fn primary(project: Id) -> miette::Result<Self> {
        Self::new(SourceRootId::primary(), project)
    }

    pub fn project_id(&self) -> &Id {
        &self.project
    }

    pub fn source_id(&self) -> &SourceRootId {
        &self.source
    }
}

impl fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.source, self.project)
    }
}

impl FromStr for ProjectKey {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((source, project)) = value.split_once("::") else {
            return Err(miette::miette!(
                "Qualified project key must use the format <source>::<project>."
            ));
        };

        Self::new(source.parse()?, Id::new(project)?)
    }
}

impl<'de> Deserialize<'de> for ProjectKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for ProjectKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
