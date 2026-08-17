use crate::ProjectKey;
use moon_common::{Id, SourceRootId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskKey {
    project: ProjectKey,
    task: Id,
}

impl TaskKey {
    pub fn new(project: ProjectKey, task: Id) -> miette::Result<Self> {
        Id::new(task.as_str())?;

        Ok(Self { project, task })
    }

    pub fn primary(project: Id, task: Id) -> miette::Result<Self> {
        Self::new(ProjectKey::new(SourceRootId::primary(), project)?, task)
    }

    pub fn project_key(&self) -> &ProjectKey {
        &self.project
    }

    pub fn task_id(&self) -> &Id {
        &self.task
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.project, self.task)
    }
}

impl FromStr for TaskKey {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((project, task)) = value.rsplit_once(':') else {
            return Err(miette::miette!(
                "Qualified task key must use the format <source>::<project>:<task>."
            ));
        };

        Self::new(project.parse()?, Id::new(task)?)
    }
}

impl<'de> Deserialize<'de> for TaskKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for TaskKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
