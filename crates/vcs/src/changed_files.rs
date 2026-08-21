use miette::IntoDiagnostic;
use moon_common::path::{PathExt, WorkspaceRelativePathBuf};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFiles<T: Hash + Eq + PartialEq = WorkspaceRelativePathBuf> {
    pub files: FxHashMap<T, Vec<ChangedStatus>>,
}

impl<T: Hash + Eq + PartialEq> Default for ChangedFiles<T> {
    fn default() -> Self {
        Self {
            files: FxHashMap::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImpactCompleteness {
    #[default]
    Exact,
    Conservative,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFilesObservation<T: Hash + Eq + PartialEq = WorkspaceRelativePathBuf> {
    pub files: ChangedFiles<T>,
    pub completeness: ImpactCompleteness,
    pub diagnostics: Vec<String>,
}

impl<T: Hash + Eq + PartialEq> Default for ChangedFilesObservation<T> {
    fn default() -> Self {
        Self {
            files: ChangedFiles::default(),
            completeness: ImpactCompleteness::Exact,
            diagnostics: vec![],
        }
    }
}

impl<T: Hash + Eq + PartialEq> ChangedFilesObservation<T> {
    pub fn exact(files: ChangedFiles<T>) -> Self {
        Self {
            files,
            ..Default::default()
        }
    }

    pub fn unavailable(diagnostic: impl Into<String>) -> Self {
        Self {
            completeness: ImpactCompleteness::Unavailable,
            diagnostics: vec![diagnostic.into()],
            ..Default::default()
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.files.merge(other.files);
        for statuses in self.files.files.values_mut() {
            statuses.sort();
            statuses.dedup();
        }
        self.completeness = self.completeness.max(other.completeness);
        self.diagnostics.extend(other.diagnostics);
        self.diagnostics.sort();
        self.diagnostics.dedup();
    }
}

impl<T: Hash + Eq + PartialEq> ChangedFiles<T> {
    pub fn all(&self) -> Vec<&T> {
        self.files.keys().collect()
    }

    pub fn added(&self) -> Vec<&T> {
        self.select(ChangedStatus::Added)
    }

    pub fn deleted(&self) -> Vec<&T> {
        self.select(ChangedStatus::Deleted)
    }

    pub fn modified(&self) -> Vec<&T> {
        self.select(ChangedStatus::Modified)
    }

    pub fn staged(&self) -> Vec<&T> {
        self.select(ChangedStatus::Staged)
    }

    pub fn unstaged(&self) -> Vec<&T> {
        self.select(ChangedStatus::Unstaged)
    }

    pub fn untracked(&self) -> Vec<&T> {
        self.select(ChangedStatus::Untracked)
    }

    pub fn merge(&mut self, other: ChangedFiles<T>) {
        for (file, statuses) in other.files {
            self.files.entry(file).or_default().extend(statuses);
        }
    }

    pub fn select(&self, status: ChangedStatus) -> Vec<&T> {
        self.files
            .iter()
            .filter_map(|(file, statuses)| {
                if statuses.contains(&status) {
                    Some(file)
                } else {
                    None
                }
            })
            .collect()
    }
}

impl ChangedFiles<PathBuf> {
    pub fn into_workspace_relative(
        self,
        workspace_root: &Path,
    ) -> miette::Result<ChangedFiles<WorkspaceRelativePathBuf>> {
        let mut files = ChangedFiles::default();

        for (file, statuses) in self.files {
            files.files.insert(
                file.relative_to(workspace_root).into_diagnostic()?,
                statuses,
            );
        }

        Ok(files)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangedStatus {
    Added,
    #[default]
    All,
    Deleted,
    Modified,
    Staged,
    Unstaged,
    Untracked,
}

impl fmt::Display for ChangedStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(
            f,
            "{}",
            match self {
                ChangedStatus::Added => "added",
                ChangedStatus::All => "all",
                ChangedStatus::Deleted => "deleted",
                ChangedStatus::Modified => "modified",
                ChangedStatus::Staged => "staged",
                ChangedStatus::Unstaged => "unstaged",
                ChangedStatus::Untracked => "untracked",
            }
        )?;

        Ok(())
    }
}

impl FromStr for ChangedStatus {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value.to_lowercase().as_str() {
            "added" => Self::Added,
            "all" => Self::All,
            "deleted" => Self::Deleted,
            "modified" => Self::Modified,
            "staged" => Self::Staged,
            "unstaged" => Self::Unstaged,
            "untracked" => Self::Untracked,
            other => return Err(miette::miette!("Unknown changed status {}", other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_merge_with_worst_completeness_and_stable_diagnostics() {
        let mut exact = ChangedFilesObservation::exact(ChangedFiles {
            files: FxHashMap::from_iter([(
                WorkspaceRelativePathBuf::from("a.txt"),
                vec![ChangedStatus::Modified],
            )]),
        });
        let conservative = ChangedFilesObservation {
            files: ChangedFiles {
                files: FxHashMap::from_iter([(
                    WorkspaceRelativePathBuf::from("b.txt"),
                    vec![ChangedStatus::Added],
                )]),
            },
            completeness: ImpactCompleteness::Conservative,
            diagnostics: vec!["z diagnostic".into(), "a diagnostic".into()],
        };

        exact.merge(conservative);
        exact.merge(ChangedFilesObservation::unavailable("a diagnostic"));

        assert_eq!(exact.completeness, ImpactCompleteness::Unavailable);
        assert_eq!(exact.files.files.len(), 2);
        assert_eq!(exact.diagnostics, ["a diagnostic", "z diagnostic"]);
    }
}
