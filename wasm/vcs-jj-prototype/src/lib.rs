//! PROTOTYPE: Jujutsu implementation of the proposed VCS overlay interface.

use extism_pdk::*;
use moon_pdk::*;
use moon_pdk_api::*;

#[plugin_fn]
pub fn register_vcs(Json(input): Json<RegisterVcsInput>) -> FnResult<Json<VcsPluginMetadata>> {
    if input.host_protocol_version != VCS_PLUGIN_PROTOCOL_VERSION {
        return Err(anyhow!("unsupported host protocol {}", input.host_protocol_version).into());
    }

    Ok(Json(VcsPluginMetadata {
        name: "Jujutsu prototype overlay".into(),
        description: Some("Overrides jj-sensitive Git facts and change queries".into()),
        plugin_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: VCS_PLUGIN_PROTOCOL_VERSION,
    }))
}

#[plugin_fn]
pub fn detect_vcs(Json(input): Json<DetectVcsInput>) -> FnResult<Json<DetectVcsOutput>> {
    let output = match run_jj(
        &input.context,
        vec!["--ignore-working-copy".into(), "root".into()],
    ) {
        Ok(output) => output,
        Err(error) => {
            return Ok(Json(DetectVcsOutput {
                active: false,
                reason: format!("jj is unavailable; use the Git fallback ({error})"),
            }));
        }
    };

    Ok(Json(DetectVcsOutput {
        active: output.exit_code == 0,
        reason: if output.exit_code == 0 {
            format!("jj workspace at {}", output.stdout.trim())
        } else {
            "jj root did not detect a workspace; use the Git fallback".into()
        },
    }))
}

#[plugin_fn]
pub fn prepare_vcs(Json(input): Json<PrepareVcsInput>) -> FnResult<Json<PreparedVcs>> {
    let mut args = vec![];

    if input.consistency == VcsConsistency::ExistingSnapshot {
        args.push("--ignore-working-copy".into());
    }

    args.extend([
        "op".into(),
        "log".into(),
        "--no-graph".into(),
        "-n".into(),
        "1".into(),
        "-T".into(),
        "id".into(),
    ]);

    let output = run_jj(&input.context, args)?;
    require_success(&output)?;
    let snapshot_id = output.stdout.trim();

    if snapshot_id.is_empty() {
        return Err(anyhow!("jj returned an empty operation ID").into());
    }

    Ok(Json(PreparedVcs {
        snapshot_id: snapshot_id.into(),
    }))
}

#[plugin_fn]
pub fn get_vcs_state(Json(input): Json<GetVcsStateInput>) -> FnResult<Json<VcsStatePatch>> {
    let args = vec![
        format!("--at-operation={}", input.snapshot_id),
        "log".into(),
        "--no-graph".into(),
        "--color".into(),
        "never".into(),
        "-r".into(),
        "@".into(),
        "-T".into(),
        "bookmarks.map(|bookmark| bookmark.name()).join(\" \") ++ \"\\0\" ++ change_id.short(8) ++ \"\\0\" ++ commit_id".into(),
    ];

    let output = run_jj(&input.context, args)?;
    require_success(&output)?;

    let mut fields = output.stdout.trim().split('\0');
    let bookmarks = fields.next().unwrap_or_default().trim();
    let change_id = fields.next().unwrap_or_default().trim();
    let commit_id = fields.next().unwrap_or_default().trim();
    let label = bookmarks
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(change_id);

    Ok(Json(VcsStatePatch {
        adapter: Some("jj over git".into()),
        current_label: Some(label.into()),
        current_revision: Some(commit_id.into()),
        is_default: Some(
            bookmarks
                .split_whitespace()
                .any(|bookmark| bookmark == input.default_branch),
        ),
        ..Default::default()
    }))
}

#[plugin_fn]
pub fn get_vcs_changed_files(
    Json(input): Json<GetVcsChangedFilesInput>,
) -> FnResult<Json<GetVcsChangedFilesOutput>> {
    let mut args = vec![];

    match input.query {
        VcsChangeQuery::WorkingCopy => {
            args.extend(["-r".into(), "@".into()]);
        }
        VcsChangeQuery::Previous { revision } => {
            let revision = resolve_revision(
                &input.context,
                &input.snapshot_id,
                &revision,
                &input.default_branch,
            )?;
            let previous =
                resolve_previous_revision(&input.context, &input.snapshot_id, &revision)?;

            args.extend(["--from".into(), previous, "--to".into(), revision]);
        }
        VcsChangeQuery::Between { base, head } => {
            let base = resolve_revision(
                &input.context,
                &input.snapshot_id,
                &base,
                &input.default_branch,
            )?;
            let head = resolve_revision(
                &input.context,
                &input.snapshot_id,
                &head,
                &input.default_branch,
            )?;
            let merge_bases =
                resolve_merge_bases(&input.context, &input.snapshot_id, &base, &head)?;
            let (operation_id, merge_base) = if merge_bases.len() == 1 {
                (input.snapshot_id.clone(), merge_bases[0].clone())
            } else {
                create_virtual_merge(&input.context, &input.snapshot_id, merge_bases)?
            };
            let output = run_diff(
                &input.context,
                &operation_id,
                vec!["--from".into(), merge_base, "--to".into(), head],
            )?;

            return Ok(Json(GetVcsChangedFilesOutput {
                files: parse_changed_files(&output.stdout),
            }));
        }
    }

    let output = run_diff(&input.context, &input.snapshot_id, args)?;

    Ok(Json(GetVcsChangedFilesOutput {
        files: parse_changed_files(&output.stdout),
    }))
}

fn run_diff(
    context: &MoonContext,
    snapshot_id: &str,
    mut args: Vec<String>,
) -> AnyResult<ExecCommandOutput> {
    let mut command_args = vec![format!("--at-operation={snapshot_id}"), "diff".into()];
    command_args.append(&mut args);
    command_args.extend([
        "-T".into(),
        "status_char ++ \"\\0\" ++ source.path() ++ \"\\0\" ++ target.path() ++ \"\\0\"".into(),
        "--color".into(),
        "never".into(),
    ]);

    let output = run_jj(context, command_args)?;
    require_success(&output)?;

    Ok(output)
}

fn run_jj(context: &MoonContext, args: Vec<String>) -> AnyResult<ExecCommandOutput> {
    let mut command = ExecCommandInput::pipe("jj", args);
    command.cwd = Some(context.workspace_root.clone());
    exec(command)
}

fn require_success(output: &ExecCommandOutput) -> AnyResult<()> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(anyhow!(output.get_output()))
    }
}

fn resolve_revision(
    context: &MoonContext,
    snapshot_id: &str,
    revision: &VcsRevision,
    default_branch: &str,
) -> AnyResult<String> {
    let expression = match revision {
        VcsRevision::Current => "@".into(),
        VcsRevision::Default => default_branch.into(),
        VcsRevision::Named(value) => value.clone(),
    };
    let output = run_jj(
        context,
        vec![
            format!("--at-operation={snapshot_id}"),
            "log".into(),
            "--no-graph".into(),
            "-r".into(),
            expression.clone(),
            "-T".into(),
            "commit_id ++ \"\\0\"".into(),
        ],
    )?;
    require_success(&output)?;

    let commit_ids = output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    if commit_ids.len() != 1 {
        return Err(anyhow!(
            "revision `{expression}` resolved to {} commits; expected exactly one",
            commit_ids.len()
        ));
    }

    Ok(commit_ids[0].into())
}

fn resolve_previous_revision(
    context: &MoonContext,
    snapshot_id: &str,
    revision: &str,
) -> AnyResult<String> {
    let output = run_jj(
        context,
        vec![
            format!("--at-operation={snapshot_id}"),
            "log".into(),
            "--no-graph".into(),
            "-r".into(),
            revision.into(),
            "-T".into(),
            "if(root, commit_id, if(parents.first().root(), commit_id, parents.first().commit_id()))"
                .into(),
        ],
    )?;
    require_success(&output)?;

    Ok(output.stdout.trim().into())
}

fn resolve_merge_bases(
    context: &MoonContext,
    snapshot_id: &str,
    base: &str,
    head: &str,
) -> AnyResult<Vec<String>> {
    let output = run_jj(
        context,
        vec![
            format!("--at-operation={snapshot_id}"),
            "log".into(),
            "--no-graph".into(),
            "-r".into(),
            format!("heads(::({base}) & ::({head}))"),
            "-T".into(),
            "commit_id ++ \"\\0\"".into(),
        ],
    )?;
    require_success(&output)?;

    let commit_ids = output
        .stdout
        .split('\0')
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect::<Vec<_>>();

    if commit_ids.is_empty() {
        return Err(anyhow!(
            "revisions `{base}` and `{head}` have no common ancestor"
        ));
    }

    Ok(commit_ids)
}

fn create_virtual_merge(
    context: &MoonContext,
    snapshot_id: &str,
    merge_bases: Vec<String>,
) -> AnyResult<(String, String)> {
    let mut args = vec![
        format!("--at-operation={snapshot_id}"),
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
        .last()
        .or_else(|| output.stderr.split_whitespace().last())
        .unwrap_or_default()
        .trim()
        .to_owned();

    if operation_id.is_empty() || !operation_id.chars().all(|char| char.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "jj returned no isolated virtual-merge operation: {}",
            output.get_output()
        ));
    }

    let output = run_jj(
        context,
        vec![
            format!("--at-operation={operation_id}"),
            "log".into(),
            "--no-graph".into(),
            "-r".into(),
            "@".into(),
            "-T".into(),
            "commit_id".into(),
        ],
    )?;
    require_success(&output)?;
    let commit_id = output.stdout.trim().to_owned();

    if commit_id.is_empty() {
        return Err(anyhow!("jj returned no virtual merge commit"));
    }

    Ok((operation_id, commit_id))
}

fn parse_changed_files(output: &str) -> Vec<VcsChangedFile> {
    let mut files = vec![];
    let mut fields = output.split('\0');

    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }

        let source = fields.next().unwrap_or_default();
        let target = fields.next().unwrap_or_default();

        if status == "R" {
            files.push(VcsChangedFile {
                path: source.into(),
                status: VcsChangedStatus::Deleted,
            });
            files.push(VcsChangedFile {
                path: target.into(),
                status: VcsChangedStatus::Added,
            });
            continue;
        }

        files.push(VcsChangedFile {
            path: if status == "D" { source } else { target }.into(),
            status: match status {
                "A" | "C" => VcsChangedStatus::Added,
                "D" => VcsChangedStatus::Deleted,
                _ => VcsChangedStatus::Modified,
            },
        });
    }

    files
}
