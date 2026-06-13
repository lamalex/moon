//! Jujutsu source-control provider.

use extism_pdk::*;
use moon_pdk::exec_process_command;
use moon_pdk_api::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const INITIALIZATION_KEY: &str = "initialization";
const JJ_COMMIT_ID_TEMPLATE: &str = "commit_id ++ \"\\0\"";
const JJ_DIFF_TEMPLATE: &str =
    "status_char ++ \"\\0\" ++ source.path() ++ \"\\0\" ++ target.path() ++ \"\\0\"";
const JJ_PARENT_IDS_TEMPLATE: &str =
    "parents.map(|parent| if(parent.root(), \"\", parent.commit_id() ++ \"\\0\")).join(\"\")";
const JJ_PREVIOUS_ID_TEMPLATE: &str =
    "if(root, commit_id, if(parents.first().root(), commit_id, parents.first().commit_id()))";
const JJ_STATE_TEMPLATE: &str = "bookmarks.map(|bookmark| bookmark.name()).join(\" \") ++ \"\\0\" ++ change_id.short(8) ++ \"\\0\" ++ commit_id ++ \"\\0\"";

struct JjInitialization {
    current: String,
    operation_id: String,
    recorded: String,
    recorded_operation_id: String,
    history: VcsHistoryCompleteness,
}

#[plugin_fn]
pub fn register_vcs(Json(input): Json<RegisterVcsInput>) -> FnResult<Json<RegisterVcsOutput>> {
    if input.host_protocol_version != VCS_PLUGIN_PROTOCOL_VERSION {
        return Err(anyhow!("unsupported host protocol {}", input.host_protocol_version).into());
    }

    Ok(Json(RegisterVcsOutput {
        name: "Jujutsu".into(),
        description: Some("moon's bundled Jujutsu source-control provider".into()),
        plugin_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: VCS_PLUGIN_PROTOCOL_VERSION,
        process_capabilities: vec![
            ProcessCapabilityDeclaration {
                id: Id::raw("jj"),
                executable: "jj".into(),
            },
            ProcessCapabilityDeclaration {
                id: Id::raw("git"),
                executable: "git".into(),
            },
        ],
    }))
}

#[plugin_fn]
pub fn initialize_vcs(
    Json(input): Json<InitializeVcsInput>,
) -> FnResult<Json<InitializeVcsOutput>> {
    if var::get::<String>(INITIALIZATION_KEY)?.is_some() {
        return Err(anyhow!(
            "Jujutsu provider already initialized; load a new plugin instance to initialize new state"
        )
        .into());
    }

    let probe = match run_jj(&input.context, ["--ignore-working-copy", "root"]) {
        Ok(output) => output,
        Err(error) => {
            return Ok(Json(InitializeVcsOutput::NotDetected {
                reason: format!("jj is unavailable ({error})"),
            }));
        }
    };

    if probe.exit_code != 0 {
        return Ok(Json(InitializeVcsOutput::NotDetected {
            reason: "Jujutsu did not detect a workspace".into(),
        }));
    }

    let operation = run_jj(
        &input.context,
        ["op", "log", "--no-graph", "-n", "1", "-T", "id"],
    )?;
    require_success(&operation)?;
    let observation_id = operation.stdout.trim().to_owned();

    if observation_id.is_empty() {
        return Err(anyhow!("jj returned an empty operation ID").into());
    }
    validate_jj_operation_id(&observation_id)?;

    let root = run_jj(
        &input.context,
        [format!("--at-operation={observation_id}"), "root".into()],
    )?;
    require_success(&root)?;
    let current = resolve_state(&input.context, &observation_id, "@")?;
    let parents =
        resolve_parent_revisions(&input.context, &observation_id, require_state_id(&current)?)?;
    let (recorded_operation_id, recorded) = match parents.as_slice() {
        [] => (
            observation_id.clone(),
            resolve_state(&input.context, &observation_id, "root()")?,
        ),
        [parent] => (
            observation_id.clone(),
            resolve_state(&input.context, &observation_id, parent)?,
        ),
        _ => {
            let (operation_id, commit) =
                create_virtual_merge(&input.context, &observation_id, parents)?;
            let state = resolve_state(&input.context, &operation_id, &commit)?;

            (operation_id, state)
        }
    };
    let baseline = input.baseline.as_ref().and_then(|label| {
        resolve_baseline(
            &input.context,
            &observation_id,
            label.as_str(),
            &input.remote_candidates,
        )
    });
    let version = run_jj(&input.context, ["--version"])?;
    let history = history_completeness(&input.context, &observation_id);
    var::set(
        INITIALIZATION_KEY,
        format!(
            "{observation_id}\0{recorded_operation_id}\0{}\0{}\0{}",
            require_state_id(&current)?,
            require_state_id(&recorded)?,
            history_label(history),
        ),
    )?;

    let initialization = VcsInitialization {
        client: Id::raw("jj"),
        client_version: (version.exit_code == 0).then(|| {
            version
                .stdout
                .split_whitespace()
                .find(|part| {
                    part.chars()
                        .next()
                        .is_some_and(|char| char.is_ascii_digit())
                })
                .unwrap_or_default()
                .to_owned()
        }),
        roots: VcsRoots {
            repository_root: input.context.get_absolute_path(root.stdout.trim()),
            working_root: input.context.get_absolute_path(root.stdout.trim()),
        },
        current,
        recorded,
        baseline,
        repository_slug: git_repository_slug(
            &input.context,
            &observation_id,
            &input.remote_candidates,
        ),
        history,
    };

    Ok(Json(InitializeVcsOutput::Initialized {
        initialization: Box::new(initialization),
    }))
}

#[plugin_fn]
pub fn get_vcs_impacts(
    Json(input): Json<GetVcsImpactsInput>,
) -> FnResult<Json<GetVcsImpactsOutput>> {
    let JjInitialization {
        current: observed_current,
        operation_id: observation_id,
        recorded: observed_recorded,
        recorded_operation_id,
        history,
    } = load_initialization()?;
    let workspace_prefix = get_workspace_prefix(&input.context, &observation_id)?;
    let changes = match input.intent {
        VcsImpactIntent::Working => {
            working_copy_changes(&input.context, &observation_id, &workspace_prefix)?
        }
        VcsImpactIntent::Submission {
            base,
            head,
            include_working,
        } => {
            if history != VcsHistoryCompleteness::Complete {
                return Ok(Json(GetVcsImpactsOutput {
                    changes: BTreeMap::new(),
                    completeness: VcsImpactCompleteness::Unavailable,
                    diagnostics: vec![
                        "cannot guarantee an exact historical comparison with incomplete history"
                            .into(),
                    ],
                }));
            }

            let head = match head {
                Some(head) => resolve_observed_revision(
                    &input.context,
                    &observation_id,
                    &observed_recorded,
                    head.as_str(),
                )?,
                None => observed_recorded.clone(),
            };
            let base = base
                .map(|base| {
                    resolve_observed_revision(
                        &input.context,
                        &observation_id,
                        &observed_recorded,
                        base.as_str(),
                    )
                })
                .transpose()?;
            let query_operation_id =
                if head == observed_recorded || base.as_deref() == Some(&observed_recorded) {
                    recorded_operation_id.as_str()
                } else {
                    observation_id.as_str()
                };
            let mut changes = if let Some(base) = base {
                if base == observed_recorded && head == observed_current {
                    diff_changes(
                        &input.context,
                        query_operation_id,
                        JjDiffQuery::Range {
                            from: base,
                            to: head,
                        },
                        VcsChangeMask::RECORDED,
                        &workspace_prefix,
                    )?
                } else if base == observed_current && head == observed_recorded {
                    BTreeMap::new()
                } else {
                    between_changes(
                        &input.context,
                        query_operation_id,
                        &base,
                        &head,
                        &workspace_prefix,
                    )?
                }
            } else {
                let previous =
                    resolve_previous_revision(&input.context, query_operation_id, &head)?;
                diff_changes(
                    &input.context,
                    query_operation_id,
                    JjDiffQuery::Range {
                        from: previous,
                        to: head,
                    },
                    VcsChangeMask::RECORDED,
                    &workspace_prefix,
                )?
            };

            if include_working {
                merge_changes(
                    &mut changes,
                    working_copy_changes(&input.context, &observation_id, &workspace_prefix)?,
                );
            }

            changes
        }
    };

    Ok(Json(GetVcsImpactsOutput {
        changes: changes
            .into_iter()
            .map(|(path, mask)| (PathBuf::from(path), mask))
            .collect(),
        completeness: VcsImpactCompleteness::Exact,
        diagnostics: vec![],
    }))
}

fn load_initialization() -> AnyResult<JjInitialization> {
    let initialization = var::get::<String>(INITIALIZATION_KEY)?
        .ok_or_else(|| anyhow!("Jujutsu provider has not initialized"))?;
    let mut fields = initialization.splitn(5, '\0');
    let invalid = || anyhow!("Jujutsu provider has invalid initialization state");

    Ok(JjInitialization {
        operation_id: fields.next().ok_or_else(invalid)?.into(),
        recorded_operation_id: fields.next().ok_or_else(invalid)?.into(),
        current: fields.next().ok_or_else(invalid)?.into(),
        recorded: fields.next().ok_or_else(invalid)?.into(),
        history: parse_history_label(fields.next().ok_or_else(invalid)?)?,
    })
}

fn history_completeness(context: &MoonContext, observation_id: &str) -> VcsHistoryCompleteness {
    if let Ok(output) = run_git(context, ["rev-parse", "--is-shallow-repository"])
        && output.exit_code == 0
    {
        return parse_git_history(&output.stdout);
    }

    let Ok(git_root) = run_jj(
        context,
        [
            format!("--at-operation={observation_id}"),
            "git".into(),
            "root".into(),
        ],
    ) else {
        return VcsHistoryCompleteness::Unknown;
    };

    // A non-Git backend has no shallow-clone boundary.
    if git_root.exit_code != 0 {
        return VcsHistoryCompleteness::Complete;
    }

    let workspace_root = context.workspace_root.as_path();
    let git_root_output = git_root.stdout.trim();
    let git_root = Path::new(git_root_output);
    let relative_git_root = git_root.strip_prefix(workspace_root).ok().or_else(|| {
        git_root_output
            .strip_prefix("/private")
            .map(Path::new)
            .and_then(|git_root| git_root.strip_prefix(workspace_root).ok())
    });
    let Some(relative_git_root) = relative_git_root else {
        return VcsHistoryCompleteness::Unknown;
    };
    let Ok(output) = run_git_at(
        context.workspace_root.join(relative_git_root),
        ["rev-parse", "--is-shallow-repository"],
    ) else {
        return VcsHistoryCompleteness::Unknown;
    };

    if output.exit_code != 0 {
        VcsHistoryCompleteness::Unknown
    } else {
        parse_git_history(&output.stdout)
    }
}

fn parse_git_history(output: &str) -> VcsHistoryCompleteness {
    match output.trim() {
        "true" => VcsHistoryCompleteness::Incomplete,
        "false" => VcsHistoryCompleteness::Complete,
        _ => VcsHistoryCompleteness::Unknown,
    }
}

fn history_label(history: VcsHistoryCompleteness) -> &'static str {
    match history {
        VcsHistoryCompleteness::Complete => "complete",
        VcsHistoryCompleteness::Incomplete => "incomplete",
        VcsHistoryCompleteness::Unknown => "unknown",
    }
}

fn parse_history_label(value: &str) -> AnyResult<VcsHistoryCompleteness> {
    match value {
        "complete" => Ok(VcsHistoryCompleteness::Complete),
        "incomplete" => Ok(VcsHistoryCompleteness::Incomplete),
        "unknown" => Ok(VcsHistoryCompleteness::Unknown),
        _ => Err(anyhow!("Jujutsu provider has an invalid history state")),
    }
}

fn resolve_state(
    context: &MoonContext,
    observation_id: &str,
    revision: &str,
) -> AnyResult<VcsState> {
    let revision = normalize_reference(revision)?;
    let output = run_jj_log(context, observation_id, &revision, JJ_STATE_TEMPLATE)?;
    require_success(&output)?;

    let mut fields = output.stdout.split('\0');
    let bookmarks = fields.next().unwrap_or_default().trim();
    let _change_id = fields.next().unwrap_or_default().trim();
    let commit_id = fields.next().unwrap_or_default().trim();

    if commit_id.is_empty() || fields.any(|field| !field.is_empty()) {
        return Err(anyhow!(
            "revision `{revision}` did not resolve to exactly one state"
        ));
    }

    Ok(VcsState {
        id: Some(commit_id.into()),
        label: bookmarks
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
            .map(String::from),
    })
}

fn require_state_id(state: &VcsState) -> AnyResult<&str> {
    state
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("Jujutsu returned a state without an exact identity"))
}

fn resolve_baseline(
    context: &MoonContext,
    observation_id: &str,
    label: &str,
    remote_candidates: &[String],
) -> Option<VcsState> {
    std::iter::once(label.to_owned())
        .chain(
            remote_candidates
                .iter()
                .map(|remote| format!("{label}@{remote}")),
        )
        .find_map(|reference| resolve_state(context, observation_id, &reference).ok())
        .map(|mut state| {
            state.label = Some(label.into());
            state
        })
}

fn normalize_reference(reference: &str) -> AnyResult<String> {
    if reference.starts_with('-') || reference.contains('\0') {
        return Err(anyhow!("invalid source-control reference `{reference}`"));
    }

    Ok(if reference == "HEAD" {
        "@".into()
    } else {
        reference.into()
    })
}

fn resolve_revision(
    context: &MoonContext,
    observation_id: &str,
    revision: &str,
) -> AnyResult<String> {
    let expression = normalize_reference(revision)?;
    let output = run_jj_log(context, observation_id, &expression, JJ_COMMIT_ID_TEMPLATE)?;
    require_success(&output)?;

    let commit_ids = output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    if commit_ids.len() != 1 {
        return Err(anyhow!(
            "revision `{expression}` resolved to {} states; expected exactly one",
            commit_ids.len()
        ));
    }

    Ok(commit_ids[0].into())
}

fn resolve_observed_revision(
    context: &MoonContext,
    observation_id: &str,
    recorded: &str,
    revision: &str,
) -> AnyResult<String> {
    if revision == recorded {
        Ok(recorded.into())
    } else {
        resolve_revision(context, observation_id, revision)
    }
}

fn working_copy_changes(
    context: &MoonContext,
    observation_id: &str,
    workspace_prefix: &str,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    diff_changes(
        context,
        observation_id,
        JjDiffQuery::Revision {
            revision: "@".into(),
        },
        VcsChangeMask::WORKING,
        workspace_prefix,
    )
}

fn between_changes(
    context: &MoonContext,
    observation_id: &str,
    base: &str,
    head: &str,
    workspace_prefix: &str,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    let merge_bases = resolve_merge_bases(context, observation_id, base, head)?;
    let (operation_id, merge_base) = if merge_bases.len() == 1 {
        (observation_id.into(), merge_bases[0].clone())
    } else {
        create_virtual_merge(context, observation_id, merge_bases)?
    };

    diff_changes(
        context,
        &operation_id,
        JjDiffQuery::Range {
            from: merge_base,
            to: head.into(),
        },
        VcsChangeMask::RECORDED,
        workspace_prefix,
    )
}

enum JjDiffQuery {
    Revision { revision: String },
    Range { from: String, to: String },
}

fn diff_changes(
    context: &MoonContext,
    observation_id: &str,
    query: JjDiffQuery,
    layer: VcsChangeMask,
    workspace_prefix: &str,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    validate_jj_operation_id(observation_id)?;
    let mut args = vec![format!("--at-operation={observation_id}"), "diff".into()];
    match query {
        JjDiffQuery::Revision { revision } => {
            validate_jj_reference(&revision)?;
            args.extend(["-r".into(), revision]);
        }
        JjDiffQuery::Range { from, to } => {
            validate_jj_reference(&from)?;
            validate_jj_reference(&to)?;
            args.extend(["--from".into(), from, "--to".into(), to]);
        }
    }
    args.extend([
        "-T".into(),
        JJ_DIFF_TEMPLATE.into(),
        "--color".into(),
        "never".into(),
        ".".into(),
    ]);
    let output = run_jj(context, args)?;
    require_success(&output)?;

    Ok(scope_changes(
        parse_changes(&output.stdout, layer),
        workspace_prefix,
    ))
}

fn get_workspace_prefix(context: &MoonContext, observation_id: &str) -> AnyResult<String> {
    validate_jj_operation_id(observation_id)?;
    let root = run_jj(
        context,
        [format!("--at-operation={observation_id}"), "root".into()],
    )?;
    require_success(&root)?;
    let workspace_root = context.workspace_root.as_path();
    let repository_root = std::path::Path::new(root.stdout.trim());
    let relative = workspace_root
        .strip_prefix(repository_root)
        .ok()
        .or_else(|| {
            root.stdout
                .trim()
                .strip_prefix("/private")
                .and_then(|root| workspace_root.strip_prefix(root).ok())
        })
        .ok_or_else(|| {
            anyhow!(
                "Moon workspace `{}` is outside the Jujutsu repository `{}`",
                workspace_root.display(),
                repository_root.display()
            )
        })?;
    let prefix = relative.to_string_lossy().replace('\\', "/");

    Ok(if prefix.is_empty() {
        prefix
    } else {
        format!("{prefix}/")
    })
}

fn scope_changes(
    changes: BTreeMap<String, VcsChangeMask>,
    workspace_prefix: &str,
) -> BTreeMap<String, VcsChangeMask> {
    if workspace_prefix.is_empty() {
        return changes;
    }

    changes
        .into_iter()
        .filter_map(|(path, mask)| {
            path.strip_prefix(workspace_prefix)
                .map(|path| (path.into(), mask))
        })
        .collect()
}

fn resolve_parent_revisions(
    context: &MoonContext,
    observation_id: &str,
    revision: &str,
) -> AnyResult<Vec<String>> {
    let output = run_jj_log(context, observation_id, revision, JJ_PARENT_IDS_TEMPLATE)?;
    require_success(&output)?;

    Ok(output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect())
}

fn resolve_previous_revision(
    context: &MoonContext,
    observation_id: &str,
    revision: &str,
) -> AnyResult<String> {
    let output = run_jj_log(context, observation_id, revision, JJ_PREVIOUS_ID_TEMPLATE)?;
    require_success(&output)?;

    Ok(output.stdout.trim().into())
}

fn resolve_merge_bases(
    context: &MoonContext,
    observation_id: &str,
    base: &str,
    head: &str,
) -> AnyResult<Vec<String>> {
    let revision = format!("heads(::({base}) & ::({head}))");
    let output = run_jj_log(context, observation_id, &revision, JJ_COMMIT_ID_TEMPLATE)?;
    require_success(&output)?;

    let mut commit_ids = output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect::<Vec<_>>();

    if commit_ids.is_empty() {
        return Err(anyhow!(
            "states `{base}` and `{head}` have no common history"
        ));
    }

    // Revsets are unordered, so make virtual merge bases deterministic.
    commit_ids.sort();

    Ok(commit_ids)
}

fn create_virtual_merge(
    context: &MoonContext,
    observation_id: &str,
    merge_bases: Vec<String>,
) -> AnyResult<(String, String)> {
    validate_jj_operation_id(observation_id)?;
    if merge_bases.is_empty() {
        return Err(anyhow!(
            "Jujutsu virtual merge requires at least one parent"
        ));
    }
    for parent in &merge_bases {
        validate_jj_reference(parent)?;
    }
    let mut args = vec![
        format!("--at-operation={observation_id}"),
        "--no-integrate-operation".into(),
        "new".into(),
    ];
    args.extend(merge_bases);
    args.extend(["-m".into(), "moon VCS virtual merge base".into()]);
    let output = run_jj(context, args)?;
    require_success(&output)?;
    let operation_id = output
        .stdout
        .split_whitespace()
        .chain(output.stderr.split_whitespace())
        .find(|value| value.len() >= 12 && value.chars().all(|char| char.is_ascii_hexdigit()))
        .unwrap_or_default()
        .to_owned();

    if operation_id.is_empty() {
        return Err(anyhow!("jj returned no isolated virtual-merge operation"));
    }

    let commit = resolve_revision(context, &operation_id, "@")?;

    Ok((operation_id, commit))
}

fn parse_changes(output: &str, layer: VcsChangeMask) -> BTreeMap<String, VcsChangeMask> {
    let mut changes = BTreeMap::new();
    let mut fields = output.split('\0');

    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }

        let source = fields.next().unwrap_or_default();
        let target = fields.next().unwrap_or_default();

        if source != target && !source.is_empty() && !target.is_empty() {
            if status != "C" {
                insert_change(&mut changes, source, VcsChangeMask::DELETED | layer);
            }
            insert_change(&mut changes, target, VcsChangeMask::ADDED | layer);
        } else {
            let kind = match status {
                "A" => VcsChangeMask::ADDED,
                "D" => VcsChangeMask::DELETED,
                _ => VcsChangeMask::MODIFIED,
            };
            let path = if status == "A" { target } else { source };
            insert_change(&mut changes, path, kind | layer);
        }
    }

    changes
}

fn insert_change(changes: &mut BTreeMap<String, VcsChangeMask>, path: &str, mask: VcsChangeMask) {
    match changes.entry(path.into()) {
        std::collections::btree_map::Entry::Occupied(mut entry) => *entry.get_mut() |= mask,
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(mask);
        }
    }
}

fn merge_changes(
    changes: &mut BTreeMap<String, VcsChangeMask>,
    additional: BTreeMap<String, VcsChangeMask>,
) {
    for (path, mask) in additional {
        insert_change(changes, &path, mask);
    }
}

fn git_repository_slug(
    context: &MoonContext,
    observation_id: &str,
    remote_candidates: &[String],
) -> Option<String> {
    let git_root = jj_git_root(context, observation_id)?;

    for remote in remote_candidates
        .iter()
        .map(String::as_str)
        .chain(["origin", "upstream"])
    {
        validate_git_revision(remote).ok()?;
        let output = run_git_at(git_root.clone(), ["remote", "get-url", remote]).ok()?;

        if output.exit_code != 0 {
            continue;
        }

        let url = output.stdout.trim().trim_end_matches(".git");
        let path = url.rsplit_once(':').map(|(_, path)| path).unwrap_or(url);
        let segments = path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();

        if segments.len() >= 2 {
            return Some(format!(
                "{}/{}",
                segments[segments.len() - 2],
                segments[segments.len() - 1]
            ));
        }
    }

    None
}

fn jj_git_root(context: &MoonContext, observation_id: &str) -> Option<VirtualPath> {
    validate_jj_operation_id(observation_id).ok()?;
    let output = run_jj(
        context,
        [
            format!("--at-operation={observation_id}"),
            "git".into(),
            "root".into(),
        ],
    )
    .ok()?;

    if output.exit_code != 0 {
        return None;
    }

    let workspace_root = context.workspace_root.as_path();
    let git_root_output = output.stdout.trim();
    let git_root = Path::new(git_root_output);
    let relative_git_root = git_root.strip_prefix(workspace_root).ok().or_else(|| {
        git_root_output
            .strip_prefix("/private")
            .map(Path::new)
            .and_then(|git_root| git_root.strip_prefix(workspace_root).ok())
    });

    if let Some(relative_git_root) = relative_git_root {
        Some(context.workspace_root.join(relative_git_root))
    } else if workspace_root.starts_with(git_root)
        || workspace_root
            .strip_prefix("/private")
            .is_ok_and(|workspace_root| workspace_root.starts_with(git_root))
    {
        Some(context.workspace_root.clone())
    } else {
        None
    }
}

struct CommandOutput {
    exit_code: i32,
    stderr: String,
    stdout: String,
}

fn run_jj<I, S>(context: &MoonContext, args: I) -> AnyResult<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    run_command_at(
        "jj",
        args,
        context.workspace_root.clone(),
        BTreeMap::from([
            ("JJ_CONFIG".into(), "".into()),
            ("NO_COLOR".into(), "1".into()),
            ("PAGER".into(), "".into()),
        ]),
    )
}

fn run_jj_log(
    context: &MoonContext,
    operation: &str,
    revision: &str,
    template: &str,
) -> AnyResult<CommandOutput> {
    validate_jj_operation_id(operation)?;
    validate_jj_reference(revision)?;
    run_jj(
        context,
        [
            format!("--at-operation={operation}"),
            "log".into(),
            "--no-graph".into(),
            "--color".into(),
            "never".into(),
            "-r".into(),
            revision.into(),
            "-T".into(),
            template.into(),
        ],
    )
}

fn run_git_at<I, S>(cwd: VirtualPath, args: I) -> AnyResult<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let output = exec_process_command(git_command(cwd, args))?;

    Ok(CommandOutput {
        exit_code: output.exit_code,
        stderr: String::from_utf8(output.stderr)?,
        stdout: String::from_utf8(output.stdout)?,
    })
}

fn git_command<I, S>(cwd: VirtualPath, args: I) -> ProcessCommandInput
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut command_args = vec![
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "status.relativePaths=false".into(),
    ];
    command_args.extend(args.into_iter().map(Into::into));

    process_command(
        "git",
        command_args,
        cwd,
        BTreeMap::from([
            ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
            ("GIT_ALLOW_PROTOCOL".into(), "".into()),
            ("GIT_NO_LAZY_FETCH".into(), "1".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_PAGER".into(), "".into()),
            ("GIT_PROTOCOL_FROM_USER".into(), "0".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ]),
    )
}

fn run_command_at<I, S>(
    executable: &str,
    args: I,
    cwd: VirtualPath,
    env: BTreeMap<String, String>,
) -> AnyResult<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let output = exec_process_command(process_command(executable, args, cwd, env))?;

    Ok(CommandOutput {
        exit_code: output.exit_code,
        stderr: String::from_utf8(output.stderr)?,
        stdout: String::from_utf8(output.stdout)?,
    })
}

fn process_command<I, S>(
    executable: &str,
    args: I,
    cwd: VirtualPath,
    env: BTreeMap<String, String>,
) -> ProcessCommandInput
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    ProcessCommandInput {
        capability: Id::raw(executable),
        args: args.into_iter().map(Into::into).collect(),
        cwd: Some(cwd),
        env,
    }
}

fn run_git<I, S>(context: &MoonContext, args: I) -> AnyResult<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    run_git_at(context.workspace_root.clone(), args)
}

fn validate_git_revision(revision: &str) -> AnyResult<()> {
    if revision.is_empty() || revision.starts_with('-') || revision.contains('\0') {
        Err(anyhow!("Git revision is empty or contains unsafe syntax"))
    } else {
        Ok(())
    }
}

fn validate_jj_operation_id(id: &str) -> AnyResult<()> {
    if !id.is_empty() && id.chars().all(|char| char.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(anyhow!("invalid Jujutsu operation ID"))
    }
}

fn validate_jj_reference(reference: &str) -> AnyResult<()> {
    if reference.is_empty() || reference.starts_with('-') || reference.contains('\0') {
        Err(anyhow!("invalid Jujutsu revision"))
    } else {
        Ok(())
    }
}

fn require_success(output: &CommandOutput) -> AnyResult<()> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            format!("{} {}", output.stdout.trim(), output.stderr.trim())
                .trim()
                .to_owned()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_provider_owned_process_commands() {
        let input = process_command(
            "jj",
            [
                "--at-operation=abc123",
                "log",
                "--no-graph",
                "--color",
                "never",
                "-r",
                "@",
                "-T",
                JJ_STATE_TEMPLATE,
            ],
            VirtualPath::new("/workspace/project"),
            BTreeMap::from([
                ("JJ_CONFIG".into(), "".into()),
                ("NO_COLOR".into(), "1".into()),
                ("PAGER".into(), "".into()),
            ]),
        );

        assert_eq!(input.capability, Id::raw("jj"));
        assert_eq!(input.cwd, Some(VirtualPath::new("/workspace/project")));
        assert_eq!(input.args[0], "--at-operation=abc123");
        assert_eq!(input.args[1], "log");
        assert_eq!(input.args.last().unwrap(), JJ_STATE_TEMPLATE);
        assert_eq!(input.env.get("JJ_CONFIG").unwrap(), "");
        assert_eq!(input.env.get("NO_COLOR").unwrap(), "1");
        assert_eq!(input.env.get("PAGER").unwrap(), "");
    }

    #[test]
    fn constructs_git_commands_with_compatible_config_overrides() {
        let input = git_command(VirtualPath::new("/workspace/project"), ["status"]);

        assert_eq!(
            input.args,
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "status.relativePaths=false",
                "status",
            ]
        );
        assert!(!input.env.contains_key("GIT_CONFIG_COUNT"));
    }

    #[test]
    fn rejects_unsafe_provider_values() {
        assert!(validate_git_revision("--output=/tmp/leak").is_err());
        assert!(validate_jj_operation_id("../bad").is_err());
        assert!(validate_jj_reference("--config=bad").is_err());
    }

    #[test]
    fn parses_changes_and_flattens_renames_and_copies() {
        let changes = parse_changes(
            "M\0same.txt\0same.txt\0R\0old.txt\0new.txt\0C\0source.txt\0copy.txt\0",
            VcsChangeMask::RECORDED,
        );

        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes["same.txt"],
            VcsChangeMask::MODIFIED | VcsChangeMask::RECORDED
        );
        assert_eq!(
            changes["old.txt"],
            VcsChangeMask::DELETED | VcsChangeMask::RECORDED
        );
        assert_eq!(
            changes["new.txt"],
            VcsChangeMask::ADDED | VcsChangeMask::RECORDED
        );
        assert_eq!(
            changes["copy.txt"],
            VcsChangeMask::ADDED | VcsChangeMask::RECORDED
        );
        assert!(!changes.contains_key("source.txt"));
    }

    #[test]
    fn merges_duplicate_change_masks() {
        let mut changes = BTreeMap::from([(
            "file.txt".into(),
            VcsChangeMask::MODIFIED | VcsChangeMask::RECORDED,
        )]);
        merge_changes(
            &mut changes,
            BTreeMap::from([(
                "file.txt".into(),
                VcsChangeMask::MODIFIED | VcsChangeMask::WORKING,
            )]),
        );

        assert_eq!(
            changes["file.txt"],
            VcsChangeMask::MODIFIED | VcsChangeMask::RECORDED | VcsChangeMask::WORKING
        );
    }
}
