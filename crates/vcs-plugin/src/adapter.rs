use crate::VcsPlugin;
use async_trait::async_trait;
use moon_common::path::{Component, WorkspaceRelativePath, WorkspaceRelativePathBuf};
use moon_pdk_api::{
    GetVcsChangedFilesInput, MoonContext, PreparedVcs, VcsChangeQuery, VcsChangedStatus,
    VcsRevision, VcsStatePatch,
};
use moon_vcs::{ChangedFiles, ChangedStatus, Vcs, VcsHookEnvironment, git::Git};
use semver::Version;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct VcsPluginAdapter {
    base: Git,
    context: MoonContext,
    default_branch: Arc<String>,
    plugin: Arc<VcsPlugin>,
    prepared: PreparedVcs,
    state: VcsStatePatch,
}

impl VcsPluginAdapter {
    pub fn new(
        base: Git,
        context: MoonContext,
        default_branch: Arc<String>,
        plugin: Arc<VcsPlugin>,
        prepared: PreparedVcs,
        state: VcsStatePatch,
    ) -> Self {
        Self {
            base,
            context,
            default_branch,
            plugin,
            prepared,
            state,
        }
    }

    async fn changed_files(&self, query: VcsChangeQuery) -> miette::Result<ChangedFiles> {
        let output = self
            .plugin
            .get_changed_files(GetVcsChangedFilesInput {
                context: self.context.clone(),
                default_branch: (*self.default_branch).clone(),
                query,
                snapshot_id: self.prepared.snapshot_id.clone(),
            })
            .await?;
        let mut changed = ChangedFiles::default();

        for file in output.files {
            let path = validate_changed_file_path(file.path)?;

            changed
                .files
                .entry(path)
                .or_default()
                .push(match file.status {
                    VcsChangedStatus::Added => ChangedStatus::Added,
                    VcsChangedStatus::Deleted => ChangedStatus::Deleted,
                    VcsChangedStatus::Modified => ChangedStatus::Modified,
                });
        }

        Ok(changed)
    }

    fn revision(&self, revision: &str) -> VcsRevision {
        if revision.is_empty() || revision == "HEAD" {
            VcsRevision::Current
        } else if revision == self.default_branch.as_str() {
            VcsRevision::Default
        } else {
            VcsRevision::Named(revision.to_owned())
        }
    }
}

#[async_trait]
impl Vcs for VcsPluginAdapter {
    async fn get_local_branch(&self) -> miette::Result<Arc<String>> {
        if let Some(label) = &self.state.current_label {
            Ok(Arc::new(label.clone()))
        } else {
            self.base.get_local_branch().await
        }
    }

    async fn get_local_branch_revision(&self) -> miette::Result<Arc<String>> {
        if let Some(revision) = &self.state.current_revision {
            Ok(Arc::new(revision.clone()))
        } else {
            self.base.get_local_branch_revision().await
        }
    }

    async fn get_default_branch(&self) -> miette::Result<Arc<String>> {
        Ok(Arc::clone(&self.default_branch))
    }

    async fn get_default_branch_revision(&self) -> miette::Result<Arc<String>> {
        self.base.get_default_branch_revision().await
    }

    async fn get_file_hashes(
        &self,
        files: &[WorkspaceRelativePathBuf],
        allow_ignored: bool,
    ) -> miette::Result<BTreeMap<WorkspaceRelativePathBuf, String>> {
        self.base.get_file_hashes(files, allow_ignored).await
    }

    async fn get_file_tree(
        &self,
        dir: &WorkspaceRelativePath,
    ) -> miette::Result<Vec<WorkspaceRelativePathBuf>> {
        self.base.get_file_tree(dir).await
    }

    async fn get_repository_root(&self) -> miette::Result<PathBuf> {
        if let Some(root) = &self.state.repository_root {
            validate_root_path("repository", root)
        } else {
            self.base.get_repository_root().await
        }
    }

    async fn get_repository_slug(&self) -> miette::Result<Arc<String>> {
        self.base.get_repository_slug().await
    }

    async fn get_changed_files(&self) -> miette::Result<ChangedFiles> {
        self.changed_files(VcsChangeQuery::WorkingCopy).await
    }

    async fn get_changed_files_against_previous_revision(
        &self,
        revision: &str,
    ) -> miette::Result<ChangedFiles> {
        self.changed_files(VcsChangeQuery::Previous {
            revision: self.revision(revision),
        })
        .await
    }

    async fn get_changed_files_between_revisions(
        &self,
        base_revision: &str,
        revision: &str,
    ) -> miette::Result<ChangedFiles> {
        self.changed_files(VcsChangeQuery::Between {
            base: self.revision(base_revision),
            head: self.revision(revision),
        })
        .await
    }

    async fn get_version(&self) -> miette::Result<Version> {
        self.base.get_version().await
    }

    async fn get_working_root(&self) -> miette::Result<PathBuf> {
        if let Some(root) = &self.state.working_root {
            validate_root_path("working-copy", root)
        } else {
            self.base.get_working_root().await
        }
    }

    fn is_default_branch(&self, branch: &str) -> bool {
        if self.state.current_label.as_deref() == Some(branch)
            && let Some(is_default) = self.state.is_default
        {
            is_default
        } else {
            self.base.is_default_branch(branch)
        }
    }

    fn is_enabled(&self) -> bool {
        self.base.is_enabled()
    }

    fn is_ignored(&self, file: &Path) -> bool {
        self.base.is_ignored(file)
    }

    async fn is_shallow_checkout(&self) -> miette::Result<bool> {
        self.base.is_shallow_checkout().await
    }

    async fn setup_hooks(&self) -> miette::Result<Option<VcsHookEnvironment>> {
        self.base.setup_hooks().await
    }

    async fn teardown_hooks(&self) -> miette::Result<()> {
        self.base.teardown_hooks().await
    }
}

fn validate_changed_file_path(path: String) -> miette::Result<WorkspaceRelativePathBuf> {
    let relative = WorkspaceRelativePathBuf::from(path.as_str());

    if path.is_empty()
        || path.starts_with('/')
        || Path::new(&path).is_absolute()
        || !relative.is_normalized()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(miette::miette!(
            "VCS plugin returned invalid workspace-relative path {path:?}"
        ));
    }

    Ok(relative)
}

fn validate_root_path(kind: &str, path: &str) -> miette::Result<PathBuf> {
    let root = PathBuf::from(path);

    if !root.is_absolute() {
        return Err(miette::miette!(
            "VCS plugin returned non-absolute {kind} root {path:?}"
        ));
    }

    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_workspace_relative_paths() {
        assert_eq!(
            validate_changed_file_path("projects/app/file.txt".into()).unwrap(),
            WorkspaceRelativePathBuf::from("projects/app/file.txt")
        );
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        assert!(validate_changed_file_path("../outside.txt".into()).is_err());
        assert!(validate_changed_file_path("projects/../outside.txt".into()).is_err());
        assert!(validate_changed_file_path("/outside.txt".into()).is_err());
    }

    #[test]
    fn requires_absolute_roots() {
        assert!(validate_root_path("repository", "relative/root").is_err());
        let absolute = std::env::current_dir().unwrap();
        assert!(validate_root_path("repository", absolute.to_str().unwrap()).is_ok());
    }
}
