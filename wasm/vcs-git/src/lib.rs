//! Built-in Git source-control provider.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use extism_pdk::*;
use moon_pdk::exec_process_command;
use moon_pdk_api::*;

const INITIALIZATION_KEY: &str = "initialization";

struct GitInitialization {
    baseline_candidates: Vec<String>,
    current: String,
    history: VcsHistoryCompleteness,
    references: BTreeMap<String, String>,
    workspace_prefix: String,
    pinned_working: BTreeMap<String, VcsChangeMask>,
    remote_candidates: Vec<String>,
    working_root: String,
}

#[plugin_fn]
pub fn register_vcs(Json(input): Json<RegisterVcsInput>) -> FnResult<Json<RegisterVcsOutput>> {
    if input.host_protocol_version != VCS_PLUGIN_PROTOCOL_VERSION {
        return Err(anyhow!("unsupported host protocol {}", input.host_protocol_version).into());
    }

    Ok(Json(RegisterVcsOutput {
        name: "Git".into(),
        description: Some("moon's bundled Git source-control provider".into()),
        plugin_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: VCS_PLUGIN_PROTOCOL_VERSION,
        process_capabilities: vec![ProcessCapabilityDeclaration {
            id: Id::raw("git"),
            executable: "git".into(),
        }],
    }))
}

#[plugin_fn]
pub fn initialize_vcs(
    Json(input): Json<InitializeVcsInput>,
) -> FnResult<Json<InitializeVcsOutput>> {
    if var::get::<String>(INITIALIZATION_KEY)?.is_some() {
        return Err(anyhow!(
            "Git provider already initialized; load a new plugin instance to initialize new state"
        )
        .into());
    }

    if let Some(baseline) = &input.baseline {
        validate_ref(baseline.as_str())?;
    }

    let mut revisions = vec!["HEAD^{commit}".to_owned()];

    if let Some(baseline) = &input.baseline {
        revisions.push(format!("{baseline}^{{commit}}"));
    }

    let state = run_git(&input.context, repository_state_args(revisions), false)?;

    if state.exit_code != 0
        && run_git(
            &input.context,
            git_args(["rev-parse", "--show-toplevel"]),
            false,
        )?
        .exit_code
            != 0
    {
        return Ok(Json(InitializeVcsOutput::NotDetected {
            reason: "Git did not detect a repository".into(),
        }));
    }

    let (repository_root, current_id, baseline, shallow, workspace_prefix) = if state.exit_code == 0
    {
        let mut values = stdout_text(&state)?.lines();
        let repository_root = values
            .next()
            .ok_or_else(|| anyhow!("Git did not return a repository root"))?
            .to_owned();
        let current_id = values
            .next()
            .ok_or_else(|| anyhow!("Git did not return the current state"))?
            .to_owned();
        let baseline = input
            .baseline
            .as_ref()
            .map(|label| {
                values
                    .next()
                    .map(|id| VcsState {
                        id: Some(id.into()),
                        label: Some(label.as_str().to_owned()),
                    })
                    .ok_or_else(|| anyhow!("Git did not return the baseline state"))
            })
            .transpose()?;
        let shallow = values
            .next()
            .ok_or_else(|| anyhow!("Git did not return the history state"))?
            .to_owned();
        let workspace_prefix = values.next().unwrap_or_default().to_owned();

        (
            repository_root,
            current_id,
            baseline,
            shallow,
            workspace_prefix,
        )
    } else {
        let fallback = run_git(&input.context, repository_state_args(vec![]), false)?;
        require_success(&fallback)?;

        let mut values = stdout_text(&fallback)?.lines();
        let repository_root = values
            .next()
            .ok_or_else(|| anyhow!("Git did not return a repository root"))?
            .to_owned();
        let shallow = values
            .next()
            .ok_or_else(|| anyhow!("Git did not return the history state"))?
            .to_owned();
        let workspace_prefix = values.next().unwrap_or_default().to_owned();
        let current_id = successful_stdout(run_git(
            &input.context,
            git_args(["rev-parse", "--verify", "HEAD^{commit}"]),
            false,
        )?)
        .unwrap_or_default();
        let baseline = input.baseline.as_ref().and_then(|label| {
            resolve_ref(&input.context, label.as_str(), &input.remote_candidates)
                .ok()
                .map(|id| VcsState {
                    id: Some(id),
                    label: Some(label.as_str().to_owned()),
                })
        });

        (
            repository_root,
            current_id,
            baseline,
            shallow,
            workspace_prefix,
        )
    };
    let label = successful_stdout(run_git(
        &input.context,
        git_args(["branch", "--show-current"]),
        false,
    )?)
    .filter(|value| !value.is_empty());
    let references_before = capture_references(&input.context)?;
    let pinned_working = working_changes(&input.context, &workspace_prefix)?;
    let verified_current = successful_stdout(run_git(
        &input.context,
        git_args(["rev-parse", "--verify", "HEAD^{commit}"]),
        false,
    )?)
    .unwrap_or_default();
    let verified_label = successful_stdout(run_git(
        &input.context,
        git_args(["branch", "--show-current"]),
        false,
    )?)
    .filter(|value| !value.is_empty());

    if verified_current != current_id || verified_label != label {
        return Err(anyhow!("Git repository state changed during initialization").into());
    }

    if let Some(baseline) = &baseline
        && resolve_ref(
            &input.context,
            baseline.label.as_deref().unwrap_or_default(),
            &input.remote_candidates,
        )? != require_state_id(baseline)?
    {
        return Err(anyhow!("Git baseline changed during initialization").into());
    }

    let history = match shallow.as_str() {
        "true" => VcsHistoryCompleteness::Incomplete,
        "false" => VcsHistoryCompleteness::Complete,
        _ => VcsHistoryCompleteness::Unknown,
    };
    let mut references = capture_references(&input.context)?;

    if references != references_before {
        return Err(anyhow!("Git references changed during initialization").into());
    }

    references.insert("HEAD".into(), current_id.clone());

    if let Some(label) = &label {
        references.insert(label.clone(), current_id.clone());
    }

    if let Some(baseline) = &baseline
        && let Some(label) = &baseline.label
        && let Some(id) = &baseline.id
    {
        references.insert(label.clone(), id.as_str().to_owned());
    }
    let baseline_candidates = if let Some(baseline) = &baseline {
        resolve_observed_refs(
            &input.context,
            baseline
                .label
                .as_deref()
                .unwrap_or(require_state_id(baseline)?),
            &input.remote_candidates,
            &references,
        )?
    } else {
        vec![]
    };

    let final_current = successful_stdout(run_git(
        &input.context,
        git_args(["rev-parse", "--verify", "HEAD^{commit}"]),
        false,
    )?)
    .unwrap_or_default();
    let final_label = successful_stdout(run_git(
        &input.context,
        git_args(["branch", "--show-current"]),
        false,
    )?)
    .filter(|value| !value.is_empty());

    if final_current != current_id || final_label != label {
        return Err(anyhow!("Git repository state changed during initialization").into());
    }

    if let Some(baseline) = &baseline
        && resolve_ref(
            &input.context,
            baseline.label.as_deref().unwrap_or_default(),
            &input.remote_candidates,
        )? != require_state_id(baseline)?
    {
        return Err(anyhow!("Git baseline changed during initialization").into());
    }

    let current = VcsState {
        id: (!current_id.is_empty()).then(|| current_id.clone()),
        label: label.or_else(|| (!current_id.is_empty()).then(|| short_id(&current_id))),
    };
    let working_root = repository_root;
    let repository_root = git_repository_root(&input.context, &working_root)?;
    let initialization = VcsInitialization {
        client: Id::raw("git"),
        client_version: git_version(&input.context),
        roots: VcsRoots {
            repository_root: input.context.get_absolute_path(&repository_root),
            working_root: input.context.get_absolute_path(&working_root),
        },
        current: current.clone(),
        recorded: current,
        baseline,
        repository_slug: repository_slug(&input.context, &input.remote_candidates),
        history,
    };
    var::set(
        INITIALIZATION_KEY,
        format!(
            "{current_id}\0{workspace_prefix}\0{working_root}\0{}\0{}\0{}\0{}\0{}",
            json::to_string(&pinned_working)?,
            json::to_string(&input.remote_candidates)?,
            history_label(history),
            json::to_string(&references)?,
            json::to_string(&baseline_candidates)?,
        ),
    )?;

    Ok(Json(InitializeVcsOutput::Initialized {
        initialization: Box::new(initialization),
    }))
}

#[plugin_fn]
pub fn get_vcs_impacts(
    Json(input): Json<GetVcsImpactsInput>,
) -> FnResult<Json<GetVcsImpactsOutput>> {
    let diagnostics = vec![];
    let completeness = VcsImpactCompleteness::Exact;
    let GitInitialization {
        current: observed_current,
        baseline_candidates,
        history,
        references,
        workspace_prefix,
        pinned_working,
        remote_candidates,
        ..
    } = load_initialization()?;
    let changes = match input.intent {
        VcsImpactIntent::Working => pinned_working,
        VcsImpactIntent::Submission {
            base,
            head,
            include_working,
        } => {
            let head = if let Some(head) = head {
                resolve_observed_ref(
                    &input.context,
                    head.as_str(),
                    &remote_candidates,
                    &references,
                )?
            } else {
                observed_current
            };
            let mut changes = if let Some(base) = base {
                let bases = if baseline_candidates
                    .first()
                    .is_some_and(|candidate| candidate == base.as_str())
                {
                    baseline_candidates
                } else {
                    resolve_observed_refs(
                        &input.context,
                        base.as_str(),
                        &remote_candidates,
                        &references,
                    )?
                };
                let Some(from) = resolve_merge_base(&input.context, &bases, &head)? else {
                    return Ok(Json(GetVcsImpactsOutput {
                        changes: BTreeMap::new(),
                        completeness: VcsImpactCompleteness::Unavailable,
                        diagnostics: vec![format!(
                            "no common state for {} and {head}; cannot guarantee a conservative impact result",
                            bases.join(", ")
                        )],
                    }));
                };

                diff_changes(&input.context, &from, &head, &workspace_prefix)?
            } else {
                let Some(changes) =
                    current_changes(&input.context, &head, &workspace_prefix, history)?
                else {
                    return Ok(Json(GetVcsImpactsOutput {
                        changes: BTreeMap::new(),
                        completeness: VcsImpactCompleteness::Unavailable,
                        diagnostics: vec![format!(
                            "cannot distinguish a root state from an incomplete-history boundary at {head}"
                        )],
                    }));
                };

                changes
            };

            if include_working {
                merge_changes(&mut changes, pinned_working);
            }

            changes
        }
    };

    Ok(Json(GetVcsImpactsOutput {
        changes: changes
            .into_iter()
            .map(|(path, mask)| (PathBuf::from(path), mask))
            .collect(),
        completeness,
        diagnostics,
    }))
}

#[plugin_fn]
pub fn setup_vcs_hook_environment(
    Json(input): Json<SetupVcsHookEnvironmentInput>,
) -> FnResult<Json<SetupVcsHookEnvironmentOutput>> {
    validate_git_hooks(&input.hooks)?;

    let initialization = load_initialization()?;
    let hooks_dir = hook_dir(
        &initialization.workspace_prefix,
        &input.context,
        &input.hooks_dir,
    )?;
    let set_hooks_args = || git_args(["config", "--worktree", "core.hooksPath", &hooks_dir]);
    let mut output = run_git(&input.context, set_hooks_args(), false)?;

    if output.exit_code != 0 {
        require_success(&run_git(
            &input.context,
            git_args(["config", "extensions.worktreeConfig", "true"]),
            false,
        )?)?;
        output = run_git(&input.context, set_hooks_args(), false)?;
    }
    require_success(&output)?;

    Ok(Json(SetupVcsHookEnvironmentOutput {
        working_dir: Some(input.context.get_absolute_path(initialization.working_root)),
    }))
}

#[plugin_fn]
pub fn teardown_vcs_hook_environment(
    Json(input): Json<TeardownVcsHookEnvironmentInput>,
) -> FnResult<Json<TeardownVcsHookEnvironmentOutput>> {
    let initialization = load_initialization()?;
    let hooks_dir = hook_dir(
        &initialization.workspace_prefix,
        &input.context,
        &input.hooks_dir,
    )?;

    let existing = run_git(
        &input.context,
        git_args(["config", "--worktree", "--get", "core.hooksPath"]),
        false,
    )?;

    if existing.exit_code == 0
        && stdout_text(&existing)?.trim_end_matches(['\r', '\n']) == hooks_dir
    {
        require_success(&run_git(
            &input.context,
            git_args(["config", "--worktree", "--unset", "core.hooksPath"]),
            false,
        )?)?;
    } else if existing.exit_code != 1 {
        require_success(&existing)?;
    }

    Ok(Json(TeardownVcsHookEnvironmentOutput {}))
}

fn git_version(context: &MoonContext) -> Option<String> {
    successful_stdout(run_git(context, git_args(["--version"]), false).ok()?).map(|output| {
        output
            .split_whitespace()
            .find(|part| {
                part.chars()
                    .next()
                    .is_some_and(|char| char.is_ascii_digit())
            })
            .unwrap_or(&output)
            .to_owned()
    })
}

const GIT_HOOKS: &[&str] = &[
    "applypatch-msg",
    "commit-msg",
    "p4-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "p4-prepare-changelist",
    "post-applypatch",
    "post-checkout",
    "post-commit",
    "post-index-change",
    "post-merge",
    "post-receive",
    "post-rewrite",
    "post-update",
    "pre-applypatch",
    "pre-auto-gc",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "pre-receive",
    "prepare-commit-msg",
    "proc-receive",
    "push-to-checkout",
    "reference-transaction",
    "sendemail-validate",
    "update",
];

fn validate_git_hooks(hooks: &[String]) -> AnyResult<()> {
    let mut unsupported = hooks
        .iter()
        .filter(|hook| !GIT_HOOKS.contains(&hook.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unsupported.sort();
    unsupported.dedup();

    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "Git does not support the following hooks: {}",
            unsupported.join(", ")
        ))
    }
}

fn git_repository_root(context: &MoonContext, working_root: &str) -> AnyResult<String> {
    let output = run_git(context, git_args(["rev-parse", "--git-common-dir"]), false)?;
    require_success(&output)?;
    let common_dir = Path::new(stdout_text(&output)?.trim());
    let common_dir = if common_dir.is_absolute() {
        common_dir.to_path_buf()
    } else {
        Path::new(working_root).join(common_dir)
    };
    let repository_root = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir.parent().unwrap_or(&common_dir)
    } else {
        &common_dir
    };

    Ok(repository_root.to_string_lossy().replace('\\', "/"))
}

fn repository_slug(context: &MoonContext, remote_candidates: &[String]) -> Option<String> {
    for remote in remote_candidates {
        let Some(url) = successful_stdout(
            run_git(context, git_args(["remote", "get-url", remote]), false).ok()?,
        ) else {
            continue;
        };

        let url = url.trim_end_matches('/').trim_end_matches(".git");
        let path = if let Some((_, path)) = url.rsplit_once(':') {
            path
        } else if let Some((_, path)) = url.split_once("://") {
            path.split_once('/').map(|(_, path)| path).unwrap_or(path)
        } else {
            url
        };
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

fn resolve_ref(
    context: &MoonContext,
    value: &str,
    remote_candidates: &[String],
) -> AnyResult<String> {
    validate_ref(value)?;

    for candidate in std::iter::once(value.to_owned()).chain(
        remote_candidates
            .iter()
            .map(|remote| remote_reference(remote, value)),
    ) {
        let revision = format!("{candidate}^{{commit}}");
        let output = run_git(
            context,
            git_args(["rev-parse", "--verify", &revision]),
            false,
        )?;

        if let Some(value) = successful_stdout(output) {
            return Ok(value);
        }
    }

    Err(anyhow!("reference `{value}` did not resolve to one state"))
}

fn capture_references(context: &MoonContext) -> AnyResult<BTreeMap<String, String>> {
    let output = run_git(
        context,
        git_args(["show-ref", "--head", "--dereference"]),
        false,
    )?;

    if output.exit_code != 0 && !stdout_text(&output)?.trim().is_empty() {
        require_success(&output)?;
    }

    let mut references = BTreeMap::new();

    for line in stdout_text(&output)?.lines() {
        let Some((id, reference)) = line.split_once(' ') else {
            continue;
        };
        let reference = reference.strip_suffix("^{}").unwrap_or(reference);
        references.insert(reference.into(), id.into());

        for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
            if let Some(short) = reference.strip_prefix(prefix) {
                references.insert(short.into(), id.into());
            }
        }
    }

    Ok(references)
}

fn resolve_observed_ref(
    context: &MoonContext,
    value: &str,
    remote_candidates: &[String],
    references: &BTreeMap<String, String>,
) -> AnyResult<String> {
    resolve_observed_refs(context, value, remote_candidates, references)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("reference `{value}` did not resolve to one state"))
}

fn resolve_observed_refs(
    context: &MoonContext,
    value: &str,
    remote_candidates: &[String],
    references: &BTreeMap<String, String>,
) -> AnyResult<Vec<String>> {
    validate_ref(value)?;
    let mut resolved = vec![];

    for candidate in std::iter::once(value.to_owned()).chain(
        remote_candidates
            .iter()
            .map(|remote| remote_reference(remote, value)),
    ) {
        if let Some(id) = references.get(&candidate) {
            if !resolved.contains(id) {
                resolved.push(id.clone());
            }

            continue;
        }

        if let Some((id, suffix)) = references
            .iter()
            .filter_map(|(reference, id)| {
                candidate
                    .strip_prefix(reference)
                    .filter(|suffix| valid_relative_revision_suffix(suffix))
                    .map(|suffix| (id, suffix, reference.len()))
            })
            .max_by_key(|(_, _, reference_len)| *reference_len)
            .map(|(id, suffix, _)| (id, suffix))
        {
            let id = resolve_ref(context, &format!("{id}{suffix}"), &[])?;

            if !resolved.contains(&id) {
                resolved.push(id);
            }
        }
    }

    if !resolved.is_empty() {
        return Ok(resolved);
    }

    for length in [64, 40] {
        if let Some((id, suffix)) = value.split_at_checked(length)
            && id.chars().all(|char| char.is_ascii_hexdigit())
            && (suffix.is_empty() || valid_relative_revision_suffix(suffix))
        {
            return if suffix.is_empty() {
                Ok(vec![id.into()])
            } else {
                resolve_ref(context, &format!("{id}{suffix}"), &[]).map(|id| vec![id])
            };
        }
    }

    Err(anyhow!(
        "reference `{value}` was not resolved during initialization"
    ))
}

fn valid_relative_revision_suffix(suffix: &str) -> bool {
    suffix.starts_with(['~', '^'])
        && suffix
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '~' | '^' | '{' | '}'))
}

fn remote_reference(remote: &str, value: &str) -> String {
    format!(
        "{remote}/{}",
        value.strip_prefix("refs/heads/").unwrap_or(value)
    )
}

fn validate_ref(value: &str) -> AnyResult<()> {
    if value.starts_with('-') || value.contains('\0') {
        Err(anyhow!("invalid source-control reference `{value}`"))
    } else {
        Ok(())
    }
}

fn require_state_id(state: &VcsState) -> AnyResult<&str> {
    state
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("Git returned a state without an exact identity"))
}

fn current_changes(
    context: &MoonContext,
    head: &str,
    workspace_prefix: &str,
    history: VcsHistoryCompleteness,
) -> AnyResult<Option<BTreeMap<String, VcsChangeMask>>> {
    if head.is_empty() {
        return Ok(Some(BTreeMap::new()));
    }

    let parents = run_git(
        context,
        git_args(["rev-list", "--parents", "-n", "1", head]),
        false,
    )?;
    require_success(&parents)?;
    let ids = stdout_text(&parents)?
        .split_whitespace()
        .collect::<Vec<_>>();

    if ids.len() > 1 {
        diff_changes(context, ids[1], head, workspace_prefix).map(Some)
    } else if history != VcsHistoryCompleteness::Complete {
        Ok(None)
    } else {
        Ok(Some(BTreeMap::new()))
    }
}

fn resolve_merge_base(
    context: &MoonContext,
    bases: &[String],
    head: &str,
) -> AnyResult<Option<String>> {
    let mut viable = vec![];
    let mut first_merge_base = None;

    for base in bases {
        let output = run_git(context, git_args(["merge-base", base, head]), false)?;
        let stdout = stdout_text(&output)?.trim();

        if output.exit_code == 0 && !stdout.is_empty() {
            first_merge_base.get_or_insert_with(|| stdout.to_owned());
            viable.push(base.clone());
        }
    }

    if viable.len() <= 1 {
        return Ok(first_merge_base);
    }

    let mut args = git_args(["merge-base", head]);
    args.extend(viable);
    let output = run_git(context, args, false)?;

    Ok(
        if output.exit_code == 0 && !stdout_text(&output)?.trim().is_empty() {
            Some(stdout_text(&output)?.trim().into())
        } else {
            first_merge_base
        },
    )
}

fn diff_changes(
    context: &MoonContext,
    from: &str,
    to: &str,
    workspace_prefix: &str,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    let output = run_git(context, diff_args(from, to), false)?;
    require_success(&output)?;

    let mut changes = parse_diff(stdout_text(&output)?);

    for submodule in historical_submodule_paths(context, from, to)? {
        let Some(relative) = submodule.strip_prefix(workspace_prefix) else {
            continue;
        };
        let (base, head) = (
            submodule_commit(context, from, &submodule)?,
            submodule_commit(context, to, &submodule)?,
        );

        if base == head {
            continue;
        }

        let cwd = context.workspace_root.join(relative);
        let nested_changes = match (base, head) {
            (Some(base), Some(head)) => {
                let output = run_git_at(cwd, diff_args(&base, &head), false)?;
                require_success(&output)?;

                parse_diff(stdout_text(&output)?)
            }
            (None, Some(head)) => {
                submodule_tree_changes(context, cwd, &head, VcsChangeMask::ADDED)?
            }
            (Some(base), None) => {
                submodule_tree_changes(context, cwd, &base, VcsChangeMask::DELETED)?
            }
            (None, None) => continue,
        };

        changes.remove(&submodule);
        merge_changes(&mut changes, prefix_changes(nested_changes, &submodule));
    }

    Ok(scope_changes(changes, workspace_prefix))
}

fn submodule_tree_changes(
    _context: &MoonContext,
    cwd: VirtualPath,
    revision: &str,
    kind: VcsChangeMask,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    let output = run_git_at(cwd, list_tree_args(revision, ".", true), false)?;
    require_success(&output)?;

    Ok(stdout_text(&output)?
        .split('\0')
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path))
        .map(|path| (path.into(), kind | VcsChangeMask::RECORDED))
        .collect())
}

fn submodule_commit(
    context: &MoonContext,
    revision: &str,
    path: &str,
) -> AnyResult<Option<String>> {
    let output = run_git(context, list_tree_args(revision, path, false), false)?;
    require_success(&output)?;

    Ok(stdout_text(&output)?
        .split_once('\t')
        .and_then(|(metadata, _)| {
            let mut fields = metadata.split_whitespace();
            (fields.next() == Some("160000") && fields.next() == Some("commit"))
                .then(|| fields.next().map(String::from))
                .flatten()
        }))
}

fn working_changes(
    context: &MoonContext,
    workspace_prefix: &str,
) -> AnyResult<BTreeMap<String, VcsChangeMask>> {
    let output = run_git(context, status_args(), true)?;
    require_success(&output)?;

    let mut changes = parse_status(stdout_text(&output)?);

    for submodule in submodule_paths(context)? {
        let Some(relative) = submodule.strip_prefix(workspace_prefix) else {
            continue;
        };
        let output = run_git_at(context.workspace_root.join(relative), status_args(), true)?;

        require_success(&output)?;
        merge_changes(
            &mut changes,
            prefix_changes(parse_status(stdout_text(&output)?), &submodule),
        );
    }

    Ok(scope_changes(changes, workspace_prefix))
}

fn submodule_paths(context: &MoonContext) -> AnyResult<Vec<String>> {
    let output = run_git(
        context,
        git_args(["ls-files", "--stage", "--full-name", "-z", "--", "."]),
        false,
    )?;
    require_success(&output)?;

    Ok(stdout_text(&output)?
        .split('\0')
        .filter_map(|entry| {
            let (metadata, path) = entry.split_once('\t')?;
            metadata.starts_with("160000 ").then(|| path.to_owned())
        })
        .collect())
}

fn historical_submodule_paths(
    context: &MoonContext,
    from: &str,
    to: &str,
) -> AnyResult<BTreeSet<String>> {
    let mut paths = submodule_paths_at(context, from)?;
    paths.extend(submodule_paths_at(context, to)?);

    Ok(paths)
}

fn submodule_paths_at(context: &MoonContext, revision: &str) -> AnyResult<BTreeSet<String>> {
    let output = run_git(context, list_tree_args(revision, ".", true), false)?;
    require_success(&output)?;

    Ok(stdout_text(&output)?
        .split('\0')
        .filter_map(|entry| {
            let (metadata, path) = entry.split_once('\t')?;
            metadata.starts_with("160000 ").then(|| path.to_owned())
        })
        .collect())
}

fn prefix_changes(
    changes: BTreeMap<String, VcsChangeMask>,
    prefix: &str,
) -> BTreeMap<String, VcsChangeMask> {
    changes
        .into_iter()
        .map(|(path, mask)| (format!("{prefix}/{path}"), mask))
        .collect()
}

fn hook_dir(
    workspace_prefix: &str,
    context: &MoonContext,
    hooks_dir: &VirtualPath,
) -> AnyResult<String> {
    let relative = hooks_dir
        .as_path()
        .strip_prefix(context.workspace_root.as_path())?;

    Ok(format!(
        "{workspace_prefix}{}",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn load_initialization() -> AnyResult<GitInitialization> {
    let initialization = var::get::<String>(INITIALIZATION_KEY)?
        .ok_or_else(|| anyhow!("Git provider has not initialized"))?;
    let mut fields = initialization.splitn(8, '\0');
    let invalid = || anyhow!("Git provider has invalid initialization state");

    Ok(GitInitialization {
        current: fields.next().ok_or_else(invalid)?.into(),
        workspace_prefix: fields.next().ok_or_else(invalid)?.into(),
        working_root: fields.next().ok_or_else(invalid)?.into(),
        pinned_working: json::from_str(fields.next().ok_or_else(invalid)?)?,
        remote_candidates: json::from_str(fields.next().ok_or_else(invalid)?)?,
        history: parse_history_label(fields.next().ok_or_else(invalid)?)?,
        references: json::from_str(fields.next().ok_or_else(invalid)?)?,
        baseline_candidates: json::from_str(fields.next().ok_or_else(invalid)?)?,
    })
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
        _ => Err(anyhow!("Git provider has an invalid history state")),
    }
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
                .map(|path| (path.to_owned(), mask))
        })
        .collect()
}

fn parse_diff(output: &str) -> BTreeMap<String, VcsChangeMask> {
    let mut changes = BTreeMap::new();
    let mut fields = output.split('\0');

    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }

        let Some(path) = fields.next().filter(|path| !path.is_empty()) else {
            break;
        };
        let layer = VcsChangeMask::RECORDED;

        if status.starts_with('R') || status.starts_with('C') {
            let Some(target) = fields.next().filter(|path| !path.is_empty()) else {
                break;
            };
            if status.starts_with('R') {
                add_change(&mut changes, path, VcsChangeMask::DELETED | layer);
            }
            add_change(&mut changes, target, VcsChangeMask::ADDED | layer);
        } else {
            let kind = match status.as_bytes()[0] {
                b'A' => VcsChangeMask::ADDED,
                b'D' => VcsChangeMask::DELETED,
                _ => VcsChangeMask::MODIFIED,
            };
            add_change(&mut changes, path, kind | layer);
        }
    }

    changes
}

fn parse_status(output: &str) -> BTreeMap<String, VcsChangeMask> {
    let mut changes = BTreeMap::new();
    let mut fields = output.split('\0');

    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }

        let mut chars = entry.chars();
        let index = chars.next().unwrap_or(' ');
        let working = chars.next().unwrap_or(' ');
        let path = &entry[3..];
        if index == '?' && working == '?' {
            add_change(
                &mut changes,
                path,
                VcsChangeMask::ADDED | VcsChangeMask::UNTRACKED,
            );
            continue;
        }

        let source = if index == 'R' || index == 'C' || working == 'R' || working == 'C' {
            fields.next().filter(|path| !path.is_empty())
        } else {
            None
        };

        if index != ' ' {
            add_status_changes(&mut changes, index, path, source, VcsChangeMask::STAGED);
        }
        if working != ' ' {
            add_status_changes(&mut changes, working, path, source, VcsChangeMask::WORKING);
        }
    }

    changes
}

fn add_status_changes(
    changes: &mut BTreeMap<String, VcsChangeMask>,
    status: char,
    path: &str,
    source: Option<&str>,
    layer: VcsChangeMask,
) {
    if status == 'R' {
        if let Some(source) = source {
            add_change(changes, source, VcsChangeMask::DELETED | layer);
        }
        add_change(changes, path, VcsChangeMask::ADDED | layer);
    } else {
        let kind = match status {
            'A' | 'C' | '?' => VcsChangeMask::ADDED,
            'D' => VcsChangeMask::DELETED,
            _ => VcsChangeMask::MODIFIED,
        };
        add_change(changes, path, kind | layer);
    }
}

fn add_change(changes: &mut BTreeMap<String, VcsChangeMask>, path: &str, mask: VcsChangeMask) {
    // Git keeps embedded repositories collapsed even with --untracked-files=all. They are
    // separate workspaces rather than files in this repository.
    if path.ends_with('/') {
        return;
    }

    match changes.entry(path.to_owned()) {
        std::collections::btree_map::Entry::Occupied(mut entry) => *entry.get_mut() |= mask,
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(mask);
        }
    }
}

fn merge_changes(
    changes: &mut BTreeMap<String, VcsChangeMask>,
    incoming: BTreeMap<String, VcsChangeMask>,
) {
    for (path, mask) in incoming {
        add_change(changes, &path, mask);
    }
}

struct GitOutput {
    exit_code: i32,
    stderr: Vec<u8>,
    stdout: Vec<u8>,
}

fn git_args<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.into_iter().map(String::from).collect()
}

fn repository_state_args(revisions: Vec<String>) -> Vec<String> {
    let mut args = git_args(["rev-parse", "--show-toplevel"]);
    args.extend(revisions);
    args.extend(git_args(["--is-shallow-repository", "--show-prefix"]));
    args
}

fn diff_args(from: &str, to: &str) -> Vec<String> {
    git_args([
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--name-status",
        "-z",
        "--find-renames",
        from,
        to,
        "--",
        ".",
    ])
}

fn list_tree_args(revision: &str, path: &str, recursive: bool) -> Vec<String> {
    let mut args = git_args(["ls-tree"]);
    if recursive {
        args.push("-r".into());
    }
    args.extend(git_args(["-z", revision, "--", path]));
    args
}

fn status_args() -> Vec<String> {
    git_args([
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignore-submodules",
        "-z",
        "--",
        ".",
    ])
}

fn git_env(isolate_config: bool) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ("GIT_PAGER".into(), "".into()),
        ("GIT_PROTOCOL_FROM_USER".into(), "0".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
    ]);

    if isolate_config {
        let null_file = if cfg!(windows) { "NUL" } else { "/dev/null" };
        env.insert("GIT_CONFIG".into(), null_file.into());
        env.insert("GIT_CONFIG_GLOBAL".into(), null_file.into());
        env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    }

    env
}

fn run_git(context: &MoonContext, args: Vec<String>, isolate_config: bool) -> AnyResult<GitOutput> {
    run_git_at(context.workspace_root.clone(), args, isolate_config)
}

fn run_git_at(cwd: VirtualPath, args: Vec<String>, isolate_config: bool) -> AnyResult<GitOutput> {
    let output = exec_process_command(git_command(cwd, args, isolate_config))?;

    Ok(GitOutput {
        exit_code: output.exit_code,
        stderr: output.stderr,
        stdout: output.stdout,
    })
}

fn git_command(
    cwd: VirtualPath,
    mut args: Vec<String>,
    isolate_config: bool,
) -> ProcessCommandInput {
    args.splice(
        ..0,
        git_args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "status.relativePaths=false",
        ]),
    );

    ProcessCommandInput {
        capability: Id::raw("git"),
        args,
        cwd: Some(cwd),
        env: git_env(isolate_config),
    }
}

fn successful_stdout(output: GitOutput) -> Option<String> {
    if output.exit_code == 0 {
        String::from_utf8(output.stdout)
            .ok()
            .map(|stdout| stdout.trim().to_owned())
    } else {
        None
    }
}

fn require_success(output: &GitOutput) -> AnyResult<()> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!(
            format!("{} {}", stdout.trim(), stderr.trim())
                .trim()
                .to_owned()
        ))
    }
}

fn stdout_text(output: &GitOutput) -> AnyResult<&str> {
    std::str::from_utf8(&output.stdout).map_err(|_| anyhow!("Git returned non-UTF-8 text output"))
}

fn short_id(id: &str) -> String {
    id[..id.len().min(8)].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_provider_owned_process_commands() {
        let input = git_command(
            VirtualPath::new("/workspace/project"),
            diff_args("base", "head"),
            true,
        );

        assert_eq!(input.capability, Id::raw("git"));
        assert_eq!(input.cwd, Some(VirtualPath::new("/workspace/project")));
        assert_eq!(
            input.args,
            git_args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "status.relativePaths=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "--find-renames",
                "base",
                "head",
                "--",
                ".",
            ])
        );
        assert_eq!(input.env.get("GIT_ALLOW_PROTOCOL").unwrap(), "");
        assert_eq!(input.env.get("GIT_NO_LAZY_FETCH").unwrap(), "1");
        assert_eq!(input.env.get("GIT_TERMINAL_PROMPT").unwrap(), "0");
        assert_eq!(input.env.get("GIT_CONFIG_NOSYSTEM").unwrap(), "1");
    }

    #[test]
    fn preserves_mixed_staged_and_working_changes() {
        assert_eq!(
            parse_status("MM modified.txt\0AM added.txt\0MD deleted.txt\0"),
            BTreeMap::from([
                (
                    "added.txt".into(),
                    VcsChangeMask::ADDED
                        | VcsChangeMask::MODIFIED
                        | VcsChangeMask::STAGED
                        | VcsChangeMask::WORKING,
                ),
                (
                    "deleted.txt".into(),
                    VcsChangeMask::MODIFIED
                        | VcsChangeMask::DELETED
                        | VcsChangeMask::STAGED
                        | VcsChangeMask::WORKING,
                ),
                (
                    "modified.txt".into(),
                    VcsChangeMask::MODIFIED | VcsChangeMask::STAGED | VcsChangeMask::WORKING,
                ),
            ])
        );
    }

    #[test]
    fn ignores_collapsed_untracked_directories() {
        assert!(parse_status("?? wt/\0").is_empty());
    }

    #[test]
    fn flattens_renames_into_deleted_and_added_paths() {
        assert_eq!(
            parse_diff("R100\0old.txt\0new.txt\0"),
            BTreeMap::from([
                (
                    "new.txt".into(),
                    VcsChangeMask::ADDED | VcsChangeMask::RECORDED,
                ),
                (
                    "old.txt".into(),
                    VcsChangeMask::DELETED | VcsChangeMask::RECORDED,
                ),
            ])
        );

        assert_eq!(
            parse_status("R  new.txt\0old.txt\0"),
            BTreeMap::from([
                (
                    "new.txt".into(),
                    VcsChangeMask::ADDED | VcsChangeMask::STAGED,
                ),
                (
                    "old.txt".into(),
                    VcsChangeMask::DELETED | VcsChangeMask::STAGED,
                ),
            ])
        );

        assert_eq!(
            parse_status(" R new.txt\0old.txt\0"),
            BTreeMap::from([
                (
                    "new.txt".into(),
                    VcsChangeMask::ADDED | VcsChangeMask::WORKING,
                ),
                (
                    "old.txt".into(),
                    VcsChangeMask::DELETED | VcsChangeMask::WORKING,
                ),
            ])
        );
    }

    #[test]
    fn does_not_delete_the_source_of_a_copy() {
        assert_eq!(
            parse_diff("C100\0source.txt\0copy.txt\0"),
            BTreeMap::from([(
                "copy.txt".into(),
                VcsChangeMask::ADDED | VcsChangeMask::RECORDED,
            )])
        );
        assert_eq!(
            parse_status("C  copy.txt\0source.txt\0"),
            BTreeMap::from([(
                "copy.txt".into(),
                VcsChangeMask::ADDED | VcsChangeMask::STAGED,
            )])
        );
        assert_eq!(
            parse_status(" C copy.txt\0source.txt\0"),
            BTreeMap::from([(
                "copy.txt".into(),
                VcsChangeMask::ADDED | VcsChangeMask::WORKING,
            )])
        );
    }
}
