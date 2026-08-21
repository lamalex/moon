use crate::app_options::AffectedOption;
use miette::IntoDiagnostic;
use moon_app_context::{SourceRuntime, SourceRuntimeRegistry};
use moon_common::path::{WorkspaceRelativePathBuf, standardize_separators};
use moon_common::{SourcePathBuf, SourceRootId, is_ci};
use moon_env_var::GlobalEnvBag;
use moon_vcs::{
    BoxedVcs, ChangedFiles, ChangedFilesObservation, ChangedStatus, ImpactCompleteness, Vcs,
};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};
use starbase_styles::color;
use starbase_utils::json;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, stdin};
use tracing::{debug, warn};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryChangedFilesOptions {
    pub base: Option<String>,
    pub default_branch: bool,
    pub head: Option<String>,
    pub local: bool,
    pub status: Vec<ChangedStatus>,
    pub stdin: bool,
}

impl QueryChangedFilesOptions {
    pub fn apply_affected(&mut self, by: &AffectedOption) {
        let local = by.is_local();

        if self.base.is_none() {
            self.base = by.get_base();
        }

        if self.head.is_none() {
            self.head = by.get_head();
        }

        self.default_branch = !local;
        self.local = local;
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct QueryChangedFilesResult {
    pub files: FxHashSet<WorkspaceRelativePathBuf>,
    pub options: QueryChangedFilesOptions,
    pub shallow: bool,
}

#[derive(Debug, Default)]
pub struct SourceChangedFilesQuery {
    pub observations: BTreeMap<SourceRootId, ChangedFilesObservation<SourcePathBuf>>,
    pub completeness: ImpactCompleteness,
    pub diagnostics: Vec<String>,
    pub legacy_affected_fallback: bool,
}

// If we're in a shallow checkout, many diff commands will fail
macro_rules! check_shallow {
    ($vcs:ident) => {
        if $vcs.is_shallow_checkout().await? {
            warn!("Detected a shallow checkout, unable to run Git commands to determine changed files.");

            if is_ci() {
                warn!("A full Git history is required for affected checks, falling back to an empty files list.");
            } else {
                warn!("A full Git history is required for affected checks, disabling for now.");
            }

            let mut result = QueryChangedFilesResult::default();
            result.shallow = true;

            return Ok(result);
        }
    };
}

pub async fn query_changed_files(
    vcs: &BoxedVcs,
    options: QueryChangedFilesOptions,
) -> miette::Result<QueryChangedFilesResult> {
    debug!("Querying for changed files");

    if options.stdin {
        query_changed_files_with_stdin(vcs, options).await
    } else {
        query_changed_files_without_stdin(vcs, options).await
    }
}

async fn query_changed_files_without_stdin(
    vcs: &BoxedVcs,
    options: QueryChangedFilesOptions,
) -> miette::Result<QueryChangedFilesResult> {
    let bag = GlobalEnvBag::instance();
    let default_branch = vcs.get_default_branch().await?;
    let current_branch = vcs.get_local_branch().await?;
    // Treat empty values as not provided, as CI templates typically
    // pass these environment variables through unconditionally. An empty
    // environment variable must not mask an explicit option either
    let base_value = bag
        .get("MOON_BASE")
        .filter(|value| !value.is_empty())
        .or(options.base.clone())
        .filter(|value| !value.is_empty());
    let base = base_value.as_deref().unwrap_or(&default_branch);
    let head_value = bag
        .get("MOON_HEAD")
        .filter(|value| !value.is_empty())
        .or(options.head.clone())
        .filter(|value| !value.is_empty());
    let head = head_value.as_deref().unwrap_or("HEAD");

    // Determine whether we should check against the previous
    // commit using a HEAD~1 query
    let check_against_previous = base_value.is_none()
        && head_value.is_none()
        && vcs.is_default_branch(&current_branch)
        && options.default_branch;

    // Don't bail on shallow if base is set, since we can assume the
    // user knows what they're doing, but still warn them, as the diff
    // may be inaccurate without a merge base
    if base_value.is_none() {
        check_shallow!(vcs);
    } else if vcs.is_shallow_checkout().await? {
        warn!(
            "Detected a shallow checkout while comparing against an explicit base, changed files may be inaccurate. A full Git history is recommended, e.g. a fetch depth of 0."
        );
    }

    let only_local = options.local && base_value.is_none() && head_value.is_none();
    let mut changed_files_map = ChangedFiles::default();
    let mut changed_files = FxHashSet::default();

    if !only_local {
        // Compare against previous commit
        if check_against_previous {
            debug!(
                "Against previous revision, as we're on the default branch \"{}\"",
                current_branch
            );

            changed_files_map.merge(
                vcs.get_changed_files_against_previous_revision(&default_branch)
                    .await?,
            );
        }
        // Otherwise against remote between 2 revisions
        else {
            debug!(
                "Against remote using base \"{}\" with head \"{}\"",
                base, head,
            );

            changed_files_map.merge(vcs.get_changed_files_between_revisions(base, head).await?);
        }
    }

    // Only include local changes when the head is the working tree;
    // an explicit head requests a comparison between 2 revisions,
    // of which the local index is not a part of
    if head_value.is_none() {
        debug!("Against local index");

        changed_files_map.merge(vcs.get_changed_files().await?);
    }

    if options.status.is_empty() {
        debug!(
            "Filtering based on changed status {}",
            color::symbol(ChangedStatus::All.to_string())
        );

        changed_files.extend(changed_files_map.all());
    } else {
        debug!(
            "Filtering based on changed status {}",
            options
                .status
                .iter()
                .map(|status| color::symbol(status.to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        );

        for status in &options.status {
            changed_files.extend(changed_files_map.select(*status));
        }
    }

    let changed_files: FxHashSet<WorkspaceRelativePathBuf> = changed_files
        .iter()
        .map(|file| WorkspaceRelativePathBuf::from(standardize_separators(file)))
        .collect();

    debug!(
        files = ?changed_files.iter().map(|file| file.as_str()).collect::<Vec<_>>(),
        "Found changed files",
    );

    Ok(QueryChangedFilesResult {
        files: changed_files,
        options,
        shallow: false,
    })
}

async fn query_changed_files_with_stdin(
    vcs: &BoxedVcs,
    options: QueryChangedFilesOptions,
) -> miette::Result<QueryChangedFilesResult> {
    if let Some(result) = read_changed_files_from_stdin()? {
        return Ok(result);
    }

    query_changed_files_without_stdin(vcs, options).await
}

fn read_changed_files_from_stdin() -> miette::Result<Option<QueryChangedFilesResult>> {
    let mut buffer = String::new();

    if !stdin().is_terminal() {
        stdin().read_to_string(&mut buffer).into_diagnostic()?;
    }

    if !buffer.is_empty() {
        // As JSON
        if buffer.starts_with('{') {
            debug!("Received from stdin as JSON");

            let result: QueryChangedFilesResult = json::parse(&buffer)?;

            return Ok(Some(result));
        }
        // As lines
        else {
            debug!("Received from stdin as separate lines");

            let files =
                FxHashSet::from_iter(buffer.split('\n').map(WorkspaceRelativePathBuf::from));

            return Ok(Some(QueryChangedFilesResult {
                files,
                ..Default::default()
            }));
        }
    }

    Ok(None)
}

pub async fn query_changed_files_for_affected(
    vcs: &BoxedVcs,
    by: Option<&AffectedOption>,
) -> miette::Result<FxHashSet<WorkspaceRelativePathBuf>> {
    let ci = is_ci();
    let mut options = QueryChangedFilesOptions {
        default_branch: ci,
        local: !ci,
        stdin: true,
        ..Default::default()
    };

    if let Some(by) = by {
        options.apply_affected(by);
    }

    query_changed_files(vcs, options)
        .await
        .map(|result| result.files)
}

pub async fn query_source_changed_files_for_affected(
    runtimes: &SourceRuntimeRegistry,
    by: Option<&AffectedOption>,
) -> miette::Result<SourceChangedFilesQuery> {
    let ci = is_ci();
    let mut options = QueryChangedFilesOptions {
        default_branch: ci,
        local: !ci,
        stdin: true,
        ..Default::default()
    };

    if let Some(by) = by {
        options.apply_affected(by);
    }

    query_source_changed_files(runtimes, options).await
}

pub async fn query_source_changed_files(
    runtimes: &SourceRuntimeRegistry,
    options: QueryChangedFilesOptions,
) -> miette::Result<SourceChangedFilesQuery> {
    let stdin_result = if options.stdin {
        read_changed_files_from_stdin()?
    } else {
        None
    };

    let primary_id = runtimes.get_primary().source_id.clone();
    let mut result = SourceChangedFilesQuery::default();

    for (source_id, runtime) in runtimes.iter() {
        let (observation, legacy_affected_fallback) = if let Some(stdin_result) = &stdin_result {
            (
                changed_files_observation_from_stdin(source_id, &primary_id, stdin_result),
                false,
            )
        } else {
            match runtime {
                SourceRuntime::Available(context) => {
                    if context.vcs.is_enabled() {
                        match query_changed_files_observation(
                            context.vcs.as_ref().as_ref(),
                            &options,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => (
                                ChangedFilesObservation::unavailable(error.to_string()),
                                false,
                            ),
                        }
                    } else {
                        (
                            ChangedFilesObservation::unavailable(
                                "Source control is not enabled for this source.",
                            ),
                            true,
                        )
                    }
                }
                SourceRuntime::Unavailable(reason) => (
                    ChangedFilesObservation::unavailable(reason.to_string()),
                    false,
                ),
            }
        };

        if should_use_legacy_affected_fallback(
            runtimes.len(),
            source_id,
            &primary_id,
            legacy_affected_fallback,
        ) {
            result.legacy_affected_fallback = true;
        }

        let qualified = qualify_changed_files_observation(source_id, observation);

        result.completeness = result.completeness.max(qualified.completeness);
        result.diagnostics.extend(
            qualified
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{source_id}: {diagnostic}")),
        );
        result.observations.insert(source_id.clone(), qualified);
    }

    result.diagnostics.sort();
    result.diagnostics.dedup();

    Ok(result)
}

fn should_use_legacy_affected_fallback(
    source_count: usize,
    source_id: &SourceRootId,
    primary_id: &SourceRootId,
    vcs_unavailable: bool,
) -> bool {
    source_count == 1 && source_id == primary_id && vcs_unavailable
}

fn changed_files_observation_from_stdin(
    source_id: &SourceRootId,
    primary_id: &SourceRootId,
    stdin_result: &QueryChangedFilesResult,
) -> ChangedFilesObservation {
    if source_id != primary_id {
        return ChangedFilesObservation::unavailable(
            "Changed files supplied through stdin are scoped to the primary source; changed files for this source are unavailable.",
        );
    }

    let mut files = ChangedFiles::default();

    for file in &stdin_result.files {
        files.files.insert(file.clone(), vec![ChangedStatus::All]);
    }

    ChangedFilesObservation::exact(files)
}

fn qualify_changed_files_observation(
    source_id: &SourceRootId,
    observation: ChangedFilesObservation,
) -> ChangedFilesObservation<SourcePathBuf> {
    let mut qualified = ChangedFilesObservation {
        files: ChangedFiles::default(),
        completeness: observation.completeness,
        diagnostics: observation.diagnostics,
    };

    for (path, statuses) in observation.files.files {
        qualified
            .files
            .files
            .insert(SourcePathBuf::new(source_id.clone(), path), statuses);
    }

    qualified
}

async fn query_changed_files_observation(
    vcs: &(dyn Vcs + Send + Sync),
    options: &QueryChangedFilesOptions,
) -> miette::Result<(ChangedFilesObservation, bool)> {
    let bag = GlobalEnvBag::instance();
    let default_branch = vcs.get_default_branch().await?;
    let current_branch = vcs.get_local_branch().await?;
    let base_value = bag
        .get("MOON_BASE")
        .filter(|value| !value.is_empty())
        .or(options.base.clone())
        .filter(|value| !value.is_empty());
    let base = base_value.as_deref().unwrap_or(&default_branch);
    let head_value = bag
        .get("MOON_HEAD")
        .filter(|value| !value.is_empty())
        .or(options.head.clone())
        .filter(|value| !value.is_empty());
    let head = head_value.as_deref().unwrap_or("HEAD");
    let previous = base_value.is_none()
        && head_value.is_none()
        && vcs.is_default_branch(&current_branch)
        && options.default_branch;

    if base_value.is_none() && vcs.is_shallow_checkout().await? {
        return Ok((
            ChangedFilesObservation::unavailable(
                "A full source-control history is required to determine affected files.",
            ),
            true,
        ));
    }

    let only_local = options.local && base_value.is_none() && head_value.is_none();
    let mut observation = ChangedFilesObservation::default();

    if !only_local {
        observation.merge(if previous {
            vcs.observe_changed_files_against_previous_revision(&default_branch)
                .await?
        } else {
            vcs.observe_changed_files_between_revisions(base, head)
                .await?
        });
    }

    if head_value.is_none() {
        observation.merge(vcs.observe_changed_files().await?);
    }

    if !options.status.is_empty() {
        observation.files.files.retain(|_, statuses| {
            options
                .status
                .iter()
                .any(|status| statuses.contains(status))
        });
    }

    observation.files.files = observation
        .files
        .files
        .into_iter()
        .map(|(file, statuses)| {
            (
                WorkspaceRelativePathBuf::from(standardize_separators(&file)),
                statuses,
            )
        })
        .collect();

    Ok((observation, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_changed_files_remain_exact_and_primary_scoped() {
        let primary_id = SourceRootId::primary();
        let path = WorkspaceRelativePathBuf::from("packages/app/src/main.ts");
        let stdin_result = QueryChangedFilesResult {
            files: FxHashSet::from_iter([path.clone()]),
            ..Default::default()
        };

        let observation = qualify_changed_files_observation(
            &primary_id,
            changed_files_observation_from_stdin(&primary_id, &primary_id, &stdin_result),
        );

        assert_eq!(observation.completeness, ImpactCompleteness::Exact);
        assert_eq!(
            observation
                .files
                .files
                .get(&SourcePathBuf::new(primary_id, path)),
            Some(&vec![ChangedStatus::All])
        );
        assert!(observation.diagnostics.is_empty());
    }

    #[test]
    fn stdin_marks_non_primary_sources_unavailable() {
        let primary_id = SourceRootId::primary();
        let child_id = SourceRootId::new("child").unwrap();
        let stdin_result = QueryChangedFilesResult {
            files: FxHashSet::from_iter([WorkspaceRelativePathBuf::from("primary.txt")]),
            ..Default::default()
        };
        let observation = qualify_changed_files_observation(
            &child_id,
            changed_files_observation_from_stdin(&child_id, &primary_id, &stdin_result),
        );

        assert_eq!(observation.completeness, ImpactCompleteness::Unavailable);
        assert!(observation.files.files.is_empty());
        assert_eq!(observation.diagnostics.len(), 1);
        assert!(observation.diagnostics[0].contains("scoped to the primary source"));
    }

    #[test]
    fn unavailable_vcs_only_disables_affected_for_single_source_queries() {
        let primary_id = SourceRootId::new("workspace").unwrap();

        assert!(should_use_legacy_affected_fallback(
            1,
            &primary_id,
            &primary_id,
            true
        ));
        assert!(!should_use_legacy_affected_fallback(
            2,
            &primary_id,
            &primary_id,
            true
        ));
    }
}
