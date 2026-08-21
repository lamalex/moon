use moon_affected::{Affected, AggregateAffected};
use moon_common::{SourceRootId, path::WorkspaceRelativePathBuf};
use moon_target::{Target, TargetLocator, TaskInvocationKey, TaskKey};
use rustc_hash::{FxHashMap, FxHashSet};
use scc::hash_map::Entry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionContext {
    /// Projects and tasks that are affected (via `--affected`).
    pub affected: Option<Affected>,

    /// Source-qualified projects and tasks that are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_affected: Option<AggregateAffected>,

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

    /// Canonical ID of the primary source.
    #[serde(default = "SourceRootId::primary")]
    pub primary_source_id: SourceRootId,

    /// Targets to run after the initial locators have been resolved.
    pub primary_targets: FxHashSet<TaskKey>,

    /// The current state of running task invocations.
    /// @mutable
    pub target_states: scc::HashMap<TaskInvocationKey, TargetState>,

    /// Files that have currently been changed.
    pub changed_files: FxHashSet<WorkspaceRelativePathBuf>,

    /// Changed files grouped by their source root.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_changed_files: BTreeMap<SourceRootId, FxHashSet<WorkspaceRelativePathBuf>>,
}

impl ActionContext {
    pub fn changed_files_for_source(
        &self,
        source_id: &SourceRootId,
    ) -> &FxHashSet<WorkspaceRelativePathBuf> {
        if let Some(files) = self.source_changed_files.get(source_id) {
            return files;
        }

        if self.source_changed_files.is_empty() {
            return &self.changed_files;
        }

        static EMPTY: std::sync::LazyLock<FxHashSet<WorkspaceRelativePathBuf>> =
            std::sync::LazyLock::new(FxHashSet::default);
        &EMPTY
    }

    pub fn is_affected(&self) -> bool {
        self.affected.is_some() || self.aggregate_affected.is_some()
    }

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
            if k.variant().is_none() {
                map.insert(k.task_key().to_owned(), v.to_owned());
            }
            true
        });
        map
    }

    pub fn get_invocation_states(&self) -> FxHashMap<TaskInvocationKey, TargetState> {
        let mut map = FxHashMap::default();
        self.target_states.iter_sync(|key, state| {
            map.insert(key.to_owned(), state.to_owned());
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
        self.set_invocation_state(key.into(), state);
    }

    pub fn set_invocation_state(&self, key: TaskInvocationKey, state: TargetState) {
        match self.target_states.entry_sync(key) {
            Entry::Occupied(mut entry) => {
                entry.insert(state);
            }
            Entry::Vacant(entry) => {
                entry.insert_entry(state);
            }
        }
    }

    pub fn get_task_state(&self, key: &TaskKey) -> Option<TargetState> {
        self.target_states
            .get_sync(&TaskInvocationKey::from(key.clone()))
            .map(|state| state.get().clone())
    }

    pub fn get_invocation_state(&self, key: &TaskInvocationKey) -> Option<TargetState> {
        self.target_states
            .get_sync(key)
            .map(|state| state.get().clone())
    }

    pub fn should_inherit_args(&self, key: &TaskKey, target: &Target) -> bool {
        if self.passthrough_args.is_empty() {
            return false;
        }

        // Canonical top-level targets are authoritative. An unqualified
        // `:task` only applies to the primary source, even when aggregate
        // resolution found matching foreign tasks.
        if self.primary_targets.contains(key) {
            if key.project_key().source_id() != &self.primary_source_id
                && self.initial_targets.iter().any(|locator| {
                    matches!(locator, TargetLocator::Qualified(other) if target.get_task_id().is_ok_and(|task_id| other.is_all_task(task_id)))
                })
            {
                return false;
            }

            return true;
        }

        false
    }
}

impl Default for ActionContext {
    fn default() -> Self {
        Self {
            affected: None,
            aggregate_affected: None,
            initial_targets: FxHashSet::default(),
            named_mutexes: scc::HashMap::default(),
            ignored_dependencies: FxHashMap::default(),
            passthrough_args: vec![],
            primary_source_id: SourceRootId::primary(),
            primary_targets: FxHashSet::default(),
            target_states: scc::HashMap::default(),
            changed_files: FxHashSet::default(),
            source_changed_files: BTreeMap::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::Id;
    use moon_target::ProjectKey;

    #[test]
    fn legacy_changed_files_remain_available_for_single_source_contexts() {
        let mut context = ActionContext::default();
        context.changed_files.insert("project/file.txt".into());

        assert_eq!(
            context.changed_files_for_source(&SourceRootId::new("local").unwrap()),
            &context.changed_files
        );
    }

    #[test]
    fn populated_source_files_do_not_fall_back_to_primary_paths() {
        let mut context = ActionContext::default();
        context
            .changed_files
            .insert("project/primary-only.txt".into());
        context
            .source_changed_files
            .insert(SourceRootId::primary(), context.changed_files.clone());

        assert!(
            context
                .changed_files_for_source(&SourceRootId::new("child").unwrap())
                .is_empty()
        );
    }

    #[test]
    fn unqualified_task_args_do_not_inherit_into_foreign_primary_targets() {
        let source_id = SourceRootId::new("child").unwrap();
        let key = TaskKey::new(
            ProjectKey::new(source_id, Id::raw("app")).unwrap(),
            Id::raw("build"),
        )
        .unwrap();
        let target = Target::new("app", "build").unwrap();
        let mut context = ActionContext::default();
        context.passthrough_args.push("--watch".into());
        context.primary_targets.insert(key.clone());
        context
            .initial_targets
            .insert(TargetLocator::Qualified(Target::parse(":build").unwrap()));

        assert!(!context.should_inherit_args(&key, &target));
    }

    #[test]
    fn explicitly_selected_foreign_primary_targets_inherit_args() {
        let source_id = SourceRootId::new("child").unwrap();
        let key = TaskKey::new(
            ProjectKey::new(source_id, Id::raw("app")).unwrap(),
            Id::raw("build"),
        )
        .unwrap();
        let target = Target::new("app", "build").unwrap();
        let mut context = ActionContext::default();
        context.passthrough_args.push("--watch".into());
        context.primary_targets.insert(key.clone());
        context
            .initial_targets
            .insert(TargetLocator::Qualified(target.clone()));

        assert!(context.should_inherit_args(&key, &target));
    }
}
