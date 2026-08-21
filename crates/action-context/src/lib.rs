use moon_affected::Affected;
use moon_common::path::WorkspaceRelativePathBuf;
use moon_target::{Target, TargetLocator, TaskKey};
use rustc_hash::{FxHashMap, FxHashSet};
use scc::hash_map::Entry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", content = "hash", rename_all = "kebab-case")]
pub enum TargetState {
    Passed(String), // hash
    Passthrough,    // no hash (cache off)
    Failed,
    Skipped,                    // skipped due to dependency failure
    SkippedConditional(String), // skipped because conditions passed
}

impl TargetState {
    pub fn from_hash(hash: Option<&str>) -> Self {
        match hash {
            Some(hash) => TargetState::Passed(hash.to_string()),
            None => TargetState::Passthrough,
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(
            self,
            TargetState::Passed(_) | TargetState::Passthrough | TargetState::SkippedConditional(_)
        )
    }

    pub fn is_skipped(&self) -> bool {
        matches!(
            self,
            TargetState::Skipped | TargetState::SkippedConditional(_)
        )
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionContext {
    /// Projects and tasks that are affected (via `--affected`).
    pub affected: Option<Affected>,

    /// Initial target locators passed to `moon run`, `moon ci`, etc.
    pub initial_targets: FxHashSet<TargetLocator>,

    /// Active mutexes for tasks to acquire locks against.
    /// @mutable
    #[serde(skip)]
    pub named_mutexes: scc::HashMap<String, Arc<Mutex<()>>>,

    /// Dependency edges that were intentionally ignored by graph options.
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub ignored_dependencies: FxHashMap<TaskKey, FxHashSet<TaskKey>>,

    /// Additional arguments passed after `--` to passthrough.
    pub passthrough_args: Vec<String>,

    /// Targets to run after the initial locators have been resolved.
    pub primary_targets: FxHashSet<TaskKey>,

    /// The current state of running tasks (via their canonical key).
    /// @mutable
    pub target_states: scc::HashMap<TaskKey, TargetState>,

    /// Files that have currently been changed.
    pub changed_files: FxHashSet<WorkspaceRelativePathBuf>,
}

impl ActionContext {
    pub async fn get_or_create_mutex(&self, name: &str) -> Arc<Mutex<()>> {
        match self.named_mutexes.entry_async(name.to_owned()).await {
            Entry::Occupied(entry) => Arc::clone(&entry),
            Entry::Vacant(entry) => {
                let mutex = Arc::new(Mutex::new(()));
                entry.insert_entry(Arc::clone(&mutex));
                mutex
            }
        }
    }

    pub fn get_target_prefix(&self, target: &Target) -> String {
        target.to_prefix(
            self.primary_targets
                .iter()
                .map(|key| key.project_key().project_id().len() + key.task_id().len() + 1)
                .max(),
        )
    }

    pub fn get_target_states(&self) -> FxHashMap<TaskKey, TargetState> {
        let mut map = FxHashMap::default();
        self.target_states.iter_sync(|k, v| {
            map.insert(k.to_owned(), v.to_owned());
            true
        });
        map
    }

    pub fn is_primary_task(&self, key: &TaskKey) -> bool {
        self.primary_targets.contains(key)
    }

    pub fn is_dependency_ignored(&self, key: &TaskKey, dependency: &TaskKey) -> bool {
        self.ignored_dependencies
            .get(key)
            .is_some_and(|dependencies| dependencies.contains(dependency))
    }

    pub fn set_task_state(&self, key: TaskKey, state: TargetState) {
        let _ = self.target_states.insert_sync(key, state);
    }

    pub fn should_inherit_args(&self, key: &TaskKey, target: &Target) -> bool {
        if self.passthrough_args.is_empty() {
            return false;
        }

        // scope:task == scope:task
        if self.primary_targets.contains(key) {
            return true;
        }

        // :task == scope:task
        for other_target in &self.initial_targets {
            if let Ok(task_id) = target.get_task_id()
                && let TargetLocator::Qualified(other_target) = other_target
                && other_target.is_all_task(task_id)
            {
                return true;
            }
        }

        false
    }
}
