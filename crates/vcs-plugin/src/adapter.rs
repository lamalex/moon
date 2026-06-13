use crate::InitializedVcsPlugin;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use moon_common::path::{WorkspaceRelativePath, WorkspaceRelativePathBuf, locate_config_dir};
use moon_pdk_api::{
    GetVcsImpactsOutput, MoonContext, VcsChangeMask, VcsImpactCompleteness, VcsImpactIntent,
    VcsInitialization,
};
use moon_vcs::{ChangedFiles, ChangedStatus, Vcs, VcsHookEnvironment, WorkspaceFiles};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;
use version_spec::Version;

#[derive(Debug)]
pub(crate) struct VcsPluginAdapter {
    baseline_label: String,
    context: MoonContext,
    files: WorkspaceFiles,
    initialization: VcsInitialization,
    plugin: Arc<InitializedVcsPlugin>,
}

impl VcsPluginAdapter {
    pub fn new(
        baseline_label: String,
        files: WorkspaceFiles,
        plugin: Arc<InitializedVcsPlugin>,
    ) -> Self {
        let context = plugin.context().clone();
        let initialization = plugin.initialization().clone();

        Self {
            baseline_label,
            context,
            files,
            initialization,
            plugin,
        }
    }

    async fn impacts(&self, intent: VcsImpactIntent) -> miette::Result<ChangedFiles> {
        let intent = match intent {
            VcsImpactIntent::Submission {
                base,
                head,
                include_working,
            } => VcsImpactIntent::Submission {
                base: base.and_then(|value| self.pin_known_state(value)),
                head: head.and_then(|value| self.pin_known_state(value)),
                include_working,
            },
            intent => intent,
        };
        let output = self.plugin.get_impacts(intent).await?;

        ensure_impacts_available(&output)?;

        changed_files_from_impacts(output)
    }

    fn baseline(&self) -> Option<&moon_pdk_api::VcsState> {
        self.initialization.baseline.as_ref()
    }

    fn pin_known_state(&self, value: String) -> Option<String> {
        if value.as_str() == "HEAD" {
            return self.initialization.recorded.id.clone();
        }

        if self.initialization.current.id.as_deref() == Some(value.as_str())
            || self.initialization.current.label.as_deref() == Some(value.as_str())
        {
            return self.initialization.current.id.clone();
        }

        if self.initialization.recorded.id.as_deref() == Some(value.as_str())
            || self.initialization.recorded.label.as_deref() == Some(value.as_str())
        {
            return self.initialization.recorded.id.clone();
        }

        if let Some(baseline) = self.baseline()
            && (baseline.id.as_deref() == Some(value.as_str())
                || baseline.label.as_deref() == Some(value.as_str()))
        {
            return baseline.id.clone();
        }

        Some(value)
    }
}

fn ensure_impacts_available(output: &GetVcsImpactsOutput) -> miette::Result<()> {
    match output.completeness {
        VcsImpactCompleteness::Exact => Ok(()),
        VcsImpactCompleteness::Conservative => {
            warn!(
                diagnostics = ?output.diagnostics,
                "Source-control provider returned conservative impacts"
            );

            Ok(())
        }
        VcsImpactCompleteness::Unavailable => {
            let diagnostics = output.diagnostics.join("; ");

            Err(if diagnostics.is_empty() {
                miette::miette!("source-control provider could not determine impacted paths")
            } else {
                miette::miette!(
                    "source-control provider could not determine impacted paths: {diagnostics}"
                )
            })
        }
    }
}

fn validate_hook_environment(
    workspace_root: &Path,
    hooks_dir: PathBuf,
    working_dir: PathBuf,
) -> miette::Result<VcsHookEnvironment> {
    let workspace_root = workspace_root.canonicalize().into_diagnostic()?;
    let working_dir = working_dir.canonicalize().into_diagnostic()?;

    if !workspace_root.starts_with(&working_dir) {
        return Err(miette::miette!(
            "source-control provider returned a hook working directory outside the workspace"
        ));
    }

    let mut existing = hooks_dir.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            miette::miette!("source-control provider returned an invalid hooks directory")
        })?;
    }

    if !existing
        .canonicalize()
        .into_diagnostic()?
        .starts_with(&workspace_root)
    {
        return Err(miette::miette!(
            "source-control provider returned a hooks directory outside the workspace"
        ));
    }

    Ok(VcsHookEnvironment {
        hooks_dir,
        working_dir,
    })
}

fn changed_files_from_impacts(output: GetVcsImpactsOutput) -> miette::Result<ChangedFiles> {
    let mut changed = ChangedFiles::default();

    for (path, mask) in output.changes {
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

        let mut statuses = vec![];

        for (flag, status) in [
            (VcsChangeMask::ADDED, ChangedStatus::Added),
            (VcsChangeMask::DELETED, ChangedStatus::Deleted),
            (VcsChangeMask::MODIFIED, ChangedStatus::Modified),
            (VcsChangeMask::RECORDED, ChangedStatus::Staged),
            (VcsChangeMask::STAGED, ChangedStatus::Staged),
            (VcsChangeMask::WORKING, ChangedStatus::Unstaged),
            (VcsChangeMask::UNTRACKED, ChangedStatus::Untracked),
        ] {
            if mask.contains(flag) && !statuses.contains(&status) {
                statuses.push(status);
            }
        }

        changed.files.insert(
            WorkspaceRelativePathBuf::from(path.to_string_lossy().into_owned()),
            statuses,
        );
    }

    Ok(changed)
}

#[async_trait]
impl Vcs for VcsPluginAdapter {
    async fn get_local_branch(&self) -> miette::Result<String> {
        Ok(self
            .initialization
            .current
            .label
            .clone()
            .or_else(|| self.initialization.current.id.clone())
            .unwrap_or_default())
    }

    async fn get_local_branch_revision(&self) -> miette::Result<String> {
        Ok(self.initialization.current.id.clone().unwrap_or_default())
    }

    async fn get_default_branch(&self) -> miette::Result<String> {
        Ok(self
            .baseline()
            .and_then(|baseline| baseline.label.clone().or_else(|| baseline.id.clone()))
            .unwrap_or_else(|| self.baseline_label.clone()))
    }

    async fn get_default_branch_revision(&self) -> miette::Result<String> {
        self.baseline()
            .and_then(|baseline| baseline.id.clone())
            .ok_or_else(|| {
                miette::miette!(
                    "source-control provider could not resolve baseline `{}`",
                    self.baseline_label
                )
            })
    }

    async fn get_file_hashes(
        &self,
        files: &[WorkspaceRelativePathBuf],
        allow_ignored: bool,
    ) -> miette::Result<BTreeMap<WorkspaceRelativePathBuf, String>> {
        self.files.hash_files(files, allow_ignored).await
    }

    async fn get_file_tree(
        &self,
        dir: &WorkspaceRelativePath,
    ) -> miette::Result<Vec<WorkspaceRelativePathBuf>> {
        self.files.list(dir)
    }

    fn get_repository_root(&self) -> miette::Result<PathBuf> {
        self.initialization
            .roots
            .repository_root
            .as_path()
            .canonicalize()
            .into_diagnostic()
    }

    async fn get_repository_slug(&self) -> miette::Result<String> {
        self.initialization
            .repository_slug
            .clone()
            .ok_or_else(|| miette::miette!("source-control provider reported no repository slug"))
    }

    async fn get_changed_files(&self) -> miette::Result<ChangedFiles> {
        self.impacts(VcsImpactIntent::Working).await
    }

    async fn get_changed_files_against_previous_revision(
        &self,
        revision: &str,
    ) -> miette::Result<ChangedFiles> {
        let head = if self.is_default_branch(revision) {
            self.initialization.recorded.id.clone()
        } else {
            (!revision.is_empty()).then(|| revision.to_owned())
        };

        self.impacts(VcsImpactIntent::Submission {
            base: None,
            head,
            include_working: false,
        })
        .await
    }

    async fn get_changed_files_between_revisions(
        &self,
        base_revision: &str,
        revision: &str,
    ) -> miette::Result<ChangedFiles> {
        self.impacts(VcsImpactIntent::Submission {
            base: (!base_revision.is_empty()).then(|| base_revision.to_owned()),
            head: (!revision.is_empty()).then(|| revision.to_owned()),
            include_working: revision.is_empty(),
        })
        .await
    }

    async fn get_version(&self) -> miette::Result<Version> {
        let version = self
            .initialization
            .client_version
            .as_deref()
            .unwrap_or(&self.plugin.metadata().plugin_version);
        let version = version
            .split_whitespace()
            .find(|part| {
                part.chars()
                    .next()
                    .is_some_and(|char| char.is_ascii_digit())
            })
            .unwrap_or(version);

        Version::parse(version).into_diagnostic()
    }

    fn get_working_root(&self) -> miette::Result<PathBuf> {
        self.initialization
            .roots
            .working_root
            .as_path()
            .canonicalize()
            .into_diagnostic()
    }

    fn is_default_branch(&self, branch: &str) -> bool {
        is_initialized_default_branch(&self.initialization, &self.baseline_label, branch)
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn is_ignored(&self, file: &Path) -> bool {
        self.files.is_ignored(file)
    }

    fn is_worktree(&self) -> bool {
        self.initialization.roots.repository_root != self.initialization.roots.working_root
    }

    async fn is_shallow_checkout(&self) -> miette::Result<bool> {
        Ok(matches!(
            self.initialization.history,
            moon_pdk_api::VcsHistoryCompleteness::Incomplete
        ))
    }

    async fn setup_hooks(&self, hooks: &[String]) -> miette::Result<Option<VcsHookEnvironment>> {
        if !self.plugin.supports_hook_environment() {
            return Ok(None);
        }

        let workspace_root = self.context.workspace_root.to_path_buf();
        let hooks_dir = locate_config_dir(&workspace_root).join("hooks");
        let output = self
            .plugin
            .setup_hook_environment(self.plugin.to_virtual_path(&hooks_dir), hooks.to_vec())
            .await?;

        Ok(match output.working_dir {
            Some(working_dir) => Some(validate_hook_environment(
                &workspace_root,
                hooks_dir,
                working_dir.to_path_buf(),
            )?),
            None => None,
        })
    }

    async fn teardown_hooks(&self, hooks: &[String]) -> miette::Result<()> {
        if self.plugin.supports_hook_environment() {
            let workspace_root = self.context.workspace_root.to_path_buf();
            let hooks_dir = locate_config_dir(&workspace_root).join("hooks");

            self.plugin
                .teardown_hook_environment(self.plugin.to_virtual_path(hooks_dir), hooks.to_vec())
                .await?;
        }

        Ok(())
    }
}

fn is_initialized_default_branch(
    initialization: &VcsInitialization,
    baseline_label: &str,
    branch: &str,
) -> bool {
    branch == baseline_label
        || initialization.baseline.as_ref().is_some_and(|baseline| {
            baseline.label.as_deref() == Some(branch)
                || initialization.recorded.id.is_some()
                    && initialization.recorded.id == baseline.id
                    && (initialization.recorded.label.as_deref() == Some(branch)
                        || initialization.current.label.is_none()
                            && initialization.current.id.as_deref() == Some(branch))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_pdk_api::{GetVcsImpactsOutput, VcsChangeMask, VcsImpactCompleteness};
    use starbase_sandbox::create_empty_sandbox;

    #[test]
    fn converts_flattened_rename_entries() {
        let changed = changed_files_from_impacts(GetVcsImpactsOutput {
            changes: BTreeMap::from([
                (
                    "old.txt".into(),
                    VcsChangeMask::DELETED | VcsChangeMask::WORKING,
                ),
                (
                    "new.txt".into(),
                    VcsChangeMask::ADDED | VcsChangeMask::WORKING,
                ),
            ]),
            completeness: VcsImpactCompleteness::Exact,
            diagnostics: vec![],
        })
        .unwrap();

        assert_eq!(
            changed
                .files
                .get(&WorkspaceRelativePathBuf::from("old.txt")),
            Some(&vec![ChangedStatus::Deleted, ChangedStatus::Unstaged])
        );
        assert_eq!(
            changed
                .files
                .get(&WorkspaceRelativePathBuf::from("new.txt")),
            Some(&vec![ChangedStatus::Added, ChangedStatus::Unstaged])
        );
    }

    #[test]
    fn converts_mixed_masks_without_duplicate_statuses() {
        let changed = changed_files_from_impacts(GetVcsImpactsOutput {
            changes: BTreeMap::from([(
                "mixed.txt".into(),
                VcsChangeMask::MODIFIED
                    | VcsChangeMask::RECORDED
                    | VcsChangeMask::STAGED
                    | VcsChangeMask::WORKING
                    | VcsChangeMask::UNTRACKED,
            )]),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(
            changed
                .files
                .get(&WorkspaceRelativePathBuf::from("mixed.txt")),
            Some(&vec![
                ChangedStatus::Modified,
                ChangedStatus::Staged,
                ChangedStatus::Unstaged,
                ChangedStatus::Untracked,
            ])
        );
    }

    #[test]
    fn rejects_invalid_change_masks() {
        for mask in [
            VcsChangeMask::empty(),
            VcsChangeMask::from_bits_retain(128),
            VcsChangeMask::ADDED,
            VcsChangeMask::WORKING,
            VcsChangeMask::from_bits_retain(
                VcsChangeMask::ADDED.bits() | VcsChangeMask::WORKING.bits() | 128,
            ),
        ] {
            assert!(
                changed_files_from_impacts(GetVcsImpactsOutput {
                    changes: BTreeMap::from([("invalid.txt".into(), mask)]),
                    ..Default::default()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn rejects_unavailable_impacts() {
        let output = GetVcsImpactsOutput {
            completeness: VcsImpactCompleteness::Unavailable,
            diagnostics: vec!["history is unavailable".into()],
            ..Default::default()
        };

        assert!(ensure_impacts_available(&output).is_err());
    }

    #[test]
    fn recognizes_a_jj_working_change_on_the_default_branch() {
        let initialization = VcsInitialization {
            current: moon_pdk_api::VcsState {
                id: Some("working-commit".into()),
                label: None,
            },
            recorded: moon_pdk_api::VcsState {
                id: Some("default-commit".into()),
                label: Some("master".into()),
            },
            baseline: Some(moon_pdk_api::VcsState {
                id: Some("default-commit".into()),
                label: Some("master".into()),
            }),
            ..Default::default()
        };

        assert!(is_initialized_default_branch(
            &initialization,
            "master",
            "working-commit"
        ));

        let feature = VcsInitialization {
            current: moon_pdk_api::VcsState {
                id: Some("working-commit".into()),
                label: Some("feature".into()),
            },
            ..initialization
        };

        assert!(!is_initialized_default_branch(
            &feature, "master", "feature"
        ));
    }

    #[test]
    fn rejects_hook_paths_outside_the_workspace() {
        let workspace = create_empty_sandbox();
        let external = create_empty_sandbox();

        assert!(
            validate_hook_environment(
                workspace.path(),
                external.path().join("hooks"),
                workspace.path().to_owned(),
            )
            .is_err()
        );
        assert!(
            validate_hook_environment(
                workspace.path(),
                workspace.path().join(".moon/hooks"),
                external.path().to_owned(),
            )
            .is_err()
        );
    }
}
