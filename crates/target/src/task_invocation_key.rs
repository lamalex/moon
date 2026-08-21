use crate::TaskKey;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

const VARIANT_SEPARATOR: char = '@';

/// Stable identity for one invocation of a task.
///
/// Tasks without arguments or environment overrides retain their plain
/// `TaskKey` identity. Variants use a deterministic digest of their invocation
/// inputs so they can safely coexist in graphs, state, and fingerprints.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskInvocationKey {
    task: TaskKey,
    variant: Option<String>,
}

impl TaskInvocationKey {
    pub fn new<A, E, K, V>(task: TaskKey, args: A, env: E) -> Self
    where
        A: IntoIterator,
        A::Item: AsRef<str>,
        E: IntoIterator<Item = (K, Option<V>)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect::<Vec<_>>();
        let mut env = env
            .into_iter()
            .map(|(key, value)| {
                (
                    key.as_ref().to_owned(),
                    value.map(|value| value.as_ref().to_owned()),
                )
            })
            .collect::<Vec<_>>();

        if args.is_empty() && env.is_empty() {
            return Self::from(task);
        }

        env.sort();

        let mut hasher = blake3::Hasher::new();
        write_field(&mut hasher, b"moon-task-invocation-v1");
        hasher.update(&(args.len() as u64).to_le_bytes());
        for arg in &args {
            write_field(&mut hasher, arg.as_bytes());
        }
        hasher.update(&(env.len() as u64).to_le_bytes());
        for (key, value) in &env {
            write_field(&mut hasher, key.as_bytes());
            match value {
                Some(value) => {
                    hasher.update(&[1]);
                    write_field(&mut hasher, value.as_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            };
        }

        Self {
            task,
            variant: Some(hasher.finalize().to_hex().to_string()),
        }
    }

    pub fn task_key(&self) -> &TaskKey {
        &self.task
    }

    pub fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }
}

fn write_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

impl From<TaskKey> for TaskInvocationKey {
    fn from(task: TaskKey) -> Self {
        Self {
            task,
            variant: None,
        }
    }
}

impl fmt::Display for TaskInvocationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.task)?;
        if let Some(variant) = &self.variant {
            write!(f, "{VARIANT_SEPARATOR}{variant}")?;
        }
        Ok(())
    }
}

impl FromStr for TaskInvocationKey {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (task, variant) = match value.rsplit_once(VARIANT_SEPARATOR) {
            Some((task, variant))
                if variant.len() == blake3::OUT_LEN * 2
                    && variant.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                (task, Some(variant.to_owned()))
            }
            Some(_) => {
                return Err(miette::miette!(
                    "Task invocation variants must be 64 hexadecimal characters."
                ));
            }
            _ => (value, None),
        };

        Ok(Self {
            task: task.parse()?,
            variant,
        })
    }
}

impl<'de> Deserialize<'de> for TaskInvocationKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for TaskInvocationKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectKey;
    use moon_common::{Id, SourceRootId};

    fn task_key() -> TaskKey {
        TaskKey::new(
            ProjectKey::new(SourceRootId::primary(), Id::raw("app")).unwrap(),
            Id::raw("build"),
        )
        .unwrap()
    }

    #[test]
    fn has_no_variant_for_empty_inputs() {
        let key = TaskInvocationKey::new(
            task_key(),
            Vec::<String>::new(),
            Vec::<(String, Option<String>)>::new(),
        );

        assert_eq!(key.variant(), None);
        assert_eq!(key.to_string(), task_key().to_string());
    }

    #[test]
    fn canonicalizes_environment_order() {
        let first =
            TaskInvocationKey::new(task_key(), ["--release"], [("B", Some("2")), ("A", None)]);
        let second =
            TaskInvocationKey::new(task_key(), ["--release"], [("A", None), ("B", Some("2"))]);

        assert_eq!(first, second);
    }

    #[test]
    fn length_delimits_ambiguous_values() {
        let first =
            TaskInvocationKey::new(task_key(), ["ab", "c"], Vec::<(&str, Option<&str>)>::new());
        let second =
            TaskInvocationKey::new(task_key(), ["a", "bc"], Vec::<(&str, Option<&str>)>::new());

        assert_ne!(first, second);
    }
}
