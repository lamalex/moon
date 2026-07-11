//! PROTOTYPE: Executable conformance report for Git and Jujutsu semantics.

use crate::plugin::{VcsPlugin, load_prototype_plugin};
use crate::policy::{PrototypeVcsSelection, PrototypeVcsUserPolicy, activate};
use miette::{IntoDiagnostic, miette};
use moon_pdk_api::*;
use moon_plugin::{MoonEnvironment, MoonHostData, PluginLocator, ProtoEnvironment};
use moon_vcs::{ChangedFiles, ChangedStatus, Vcs, git::Git};
use moon_vcs_plugin::{VcsPluginConfig, get_user_vcs_config_path, load_user_vcs_adapter};
use serde::Serialize;
use starbase_utils::hash;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use warpgate::FileLocator;

const DEFAULT_BRANCH: &str = "master";

type ChangeSet = BTreeSet<(String, String)>;

#[derive(Debug, Serialize)]
struct ScenarioReport {
    name: String,
    passed: bool,
    expected: ChangeSet,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<ChangeSet>,
    jj: ChangeSet,
}

struct Fixture {
    root: PathBuf,
    initial: String,
    merged: String,
}

struct CrissCrossFixture {
    root: PathBuf,
    left_merge: String,
    right_merge: String,
}

pub async fn run(moon_root: &Path) -> miette::Result<()> {
    let fixture = create_fixture()?;
    let plugin = load_prototype_plugin(moon_root, &fixture.root).await?;
    let context = MoonContext {
        working_dir: plugin.to_virtual_path(&fixture.root),
        workspace_root: plugin.to_virtual_path(&fixture.root),
    };
    let prepared = prepare(&plugin, &context).await?;
    let git = load_git(&fixture.root)?;
    let mut reports = vec![];

    reports.push(
        compare(
            "between divergent default and feature uses merge base",
            changes([("projects/feature.txt", "added")]),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::Between {
                base: VcsRevision::Default,
                head: VcsRevision::Current,
            },
        )
        .await?,
    );

    reports.push(
        compare(
            "between default and merge commit includes merged feature",
            changes([("projects/feature.txt", "added")]),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::Between {
                base: VcsRevision::Default,
                head: VcsRevision::Named(fixture.merged.clone()),
            },
        )
        .await?,
    );

    reports.push(
        compare(
            "previous merge commit compares against first parent",
            changes([("projects/feature.txt", "added")]),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::Previous {
                revision: VcsRevision::Named(fixture.merged.clone()),
            },
        )
        .await?,
    );

    reports.push(
        compare(
            "previous root commit has no prior changes",
            ChangeSet::new(),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::Previous {
                revision: VcsRevision::Named(fixture.initial.clone()),
            },
        )
        .await?,
    );

    reports.push(
        compare(
            "clean working copy is empty",
            ChangeSet::new(),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::WorkingCopy,
        )
        .await?,
    );

    let ambiguous_revision = plugin
        .get_changed_files(GetVcsChangedFilesInput {
            context: context.clone(),
            default_branch: DEFAULT_BRANCH.into(),
            query: VcsChangeQuery::Between {
                base: VcsRevision::Named("all()".into()),
                head: VcsRevision::Current,
            },
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await;
    reports.push(ScenarioReport {
        name: "ambiguous named revision is rejected".into(),
        passed: ambiguous_revision.is_err(),
        expected: ChangeSet::new(),
        git: None,
        jj: ChangeSet::new(),
    });

    for path in [
        "working.txt",
        "space name.txt",
        "line\nbreak.txt",
        "unicode-\u{00e9}.txt",
    ] {
        fs::write(fixture.root.join(path), "working\n").into_diagnostic()?;
    }
    let prepared = prepare(&plugin, &context).await?;
    let git = load_git(&fixture.root)?;

    reports.push(
        compare(
            "working copy preserves machine-sensitive paths",
            changes([
                ("line\nbreak.txt", "added"),
                ("space name.txt", "added"),
                ("unicode-\u{00e9}.txt", "added"),
                ("working.txt", "added"),
            ]),
            &git,
            &plugin,
            &context,
            &prepared,
            VcsChangeQuery::WorkingCopy,
        )
        .await?,
    );

    let rename_fixture = create_fixture()?;
    let rename_plugin = load_prototype_plugin(moon_root, &rename_fixture.root).await?;
    let rename_context = MoonContext {
        working_dir: rename_plugin.to_virtual_path(&rename_fixture.root),
        workspace_root: rename_plugin.to_virtual_path(&rename_fixture.root),
    };
    fs::create_dir_all(rename_fixture.root.join("projects/b")).into_diagnostic()?;
    fs::rename(
        rename_fixture.root.join("projects/a/old.txt"),
        rename_fixture.root.join("projects/b/new.txt"),
    )
    .into_diagnostic()?;
    let rename_git = query_git(
        &load_git(&rename_fixture.root)?,
        &VcsChangeQuery::WorkingCopy,
    )
    .await?;
    let rename_prepared = prepare(&rename_plugin, &rename_context).await?;

    reports.push(
        compare_with_git(
            "cross-project rename affects source and destination",
            changes([
                ("projects/a/old.txt", "deleted"),
                ("projects/b/new.txt", "added"),
            ]),
            rename_git,
            &rename_plugin,
            &rename_context,
            &rename_prepared,
            VcsChangeQuery::WorkingCopy,
        )
        .await?,
    );

    let criss_cross = create_criss_cross_fixture()?;
    let criss_cross_plugin = load_prototype_plugin(moon_root, &criss_cross.root).await?;
    let criss_cross_context = MoonContext {
        working_dir: criss_cross_plugin.to_virtual_path(&criss_cross.root),
        workspace_root: criss_cross_plugin.to_virtual_path(&criss_cross.root),
    };
    let criss_cross_prepared = prepare(&criss_cross_plugin, &criss_cross_context).await?;
    let criss_cross_query = VcsChangeQuery::Between {
        base: VcsRevision::Named(criss_cross.left_merge.clone()),
        head: VcsRevision::Named(criss_cross.right_merge.clone()),
    };

    let criss_cross_git = query_git(&load_git(&criss_cross.root)?, &criss_cross_query).await?;
    let valid_git_single_base = criss_cross_git == changes([("left.txt", "added")])
        || criss_cross_git == changes([("right.txt", "added")]);
    let criss_cross_expected = ChangeSet::new();
    let criss_cross_jj = query_jj(
        &criss_cross_plugin,
        &criss_cross_context,
        &criss_cross_prepared,
        criss_cross_query,
    )
    .await?;
    let criss_cross_live = criss_cross_plugin
        .prepare(PrepareVcsInput {
            context: criss_cross_context.clone(),
            consistency: VcsConsistency::ExistingSnapshot,
        })
        .await?;
    reports.push(ScenarioReport {
        name: "criss-cross history uses an isolated virtual merge base".into(),
        passed: valid_git_single_base
            && criss_cross_jj == criss_cross_expected
            && criss_cross_live.snapshot_id == criss_cross_prepared.snapshot_id,
        expected: criss_cross_expected,
        git: Some(criss_cross_git),
        jj: criss_cross_jj,
    });

    let secondary_fixture = create_fixture()?;
    let secondary_root = temp_fixture_root("secondary-workspace")?;
    command(
        &secondary_fixture.root,
        "jj",
        [
            "workspace",
            "add",
            "--name",
            "moon-secondary",
            "--revision",
            "@",
            secondary_root
                .to_str()
                .ok_or_else(|| miette!("secondary workspace path is not UTF-8"))?,
        ],
    )?;

    let secondary_plugin = load_prototype_plugin(moon_root, &secondary_root).await?;
    let secondary_context = MoonContext {
        working_dir: secondary_plugin.to_virtual_path(&secondary_root),
        workspace_root: secondary_plugin.to_virtual_path(&secondary_root),
    };
    let detected = secondary_plugin
        .detect(DetectVcsInput {
            context: secondary_context.clone(),
        })
        .await?;

    fs::write(secondary_root.join("secondary.txt"), "secondary\n").into_diagnostic()?;
    let secondary_prepared = prepare(&secondary_plugin, &secondary_context).await?;
    let secondary_state =
        get_jj_state(&secondary_plugin, &secondary_context, &secondary_prepared).await?;

    fs::write(secondary_root.join("later.txt"), "later\n").into_diagnostic()?;
    command(&secondary_root, "jj", ["status"])?;

    let pinned_state =
        get_jj_state(&secondary_plugin, &secondary_context, &secondary_prepared).await?;
    let mut pinned_report = compare_jj_only(
        "secondary workspace preserves a prepared snapshot",
        changes([("secondary.txt", "added")]),
        &secondary_plugin,
        &secondary_context,
        &secondary_prepared,
        VcsChangeQuery::WorkingCopy,
    )
    .await?;
    pinned_report.passed &= detected.active
        && secondary_state.current_revision.is_some()
        && pinned_state.current_revision == secondary_state.current_revision;
    reports.push(pinned_report);

    let secondary_fresh = prepare(&secondary_plugin, &secondary_context).await?;
    let mut fresh_report = compare_jj_only(
        "secondary workspace fresh snapshot observes later changes",
        changes([("later.txt", "added"), ("secondary.txt", "added")]),
        &secondary_plugin,
        &secondary_context,
        &secondary_fresh,
        VcsChangeQuery::WorkingCopy,
    )
    .await?;
    fresh_report.passed &= secondary_fresh.snapshot_id != secondary_prepared.snapshot_id;
    reports.push(fresh_report);

    let wasm_file = moon_root.join("wasm/target/wasm32-wasip1/release/vcs_jj_prototype.wasm");
    let plugin_sha256 = hash::sha256::from_file(&wasm_file)?;
    let plugin_locator = PluginLocator::File(Box::new(FileLocator {
        file: wasm_file.to_string_lossy().into_owned(),
        path: Some(wasm_file),
    }));
    let enabled_policy = PrototypeVcsUserPolicy {
        enabled: true,
        plugin: plugin_locator.clone(),
        sha256: plugin_sha256.clone(),
    };

    reports.push(policy_report(
        "absent user policy selects Git",
        matches!(
            activate(None, &fixture.root).await?,
            PrototypeVcsSelection::Git { .. }
        ),
    ));
    reports.push(policy_report(
        "disabled user policy selects Git without loading",
        matches!(
            activate(
                Some(&PrototypeVcsUserPolicy {
                    enabled: false,
                    plugin: plugin_locator.clone(),
                    sha256: plugin_sha256.clone(),
                }),
                &fixture.root,
            )
            .await?,
            PrototypeVcsSelection::Git { .. }
        ),
    ));
    reports.push(policy_report(
        "plugin integrity mismatch fails closed",
        activate(
            Some(&PrototypeVcsUserPolicy {
                enabled: true,
                plugin: plugin_locator.clone(),
                sha256: "0".repeat(64),
            }),
            &fixture.root,
        )
        .await
        .is_err(),
    ));

    let active_selection = activate(Some(&enabled_policy), &fixture.root).await?;
    let active_policy_works = if let PrototypeVcsSelection::Overlay {
        plugin,
        context,
        detection,
    } = active_selection
    {
        detection.active && plugin.detect(DetectVcsInput { context }).await?.active
    } else {
        false
    };
    reports.push(policy_report(
        "trusted user policy activates the detected overlay",
        active_policy_works,
    ));

    let policy_root = temp_fixture_root("production-policy")?;
    fs::create_dir_all(&policy_root).into_diagnostic()?;
    let mut moon_env = MoonEnvironment::from(&policy_root)?;
    moon_env.working_dir = fixture.root.clone();
    moon_env.workspace_root = fixture.root.clone();
    let mut proto_env = ProtoEnvironment::new()?;
    proto_env.working_dir = fixture.root.clone();
    let host_data = MoonHostData {
        moon_env: Arc::new(moon_env),
        proto_env: Arc::new(proto_env),
        ..Default::default()
    };
    let config_file = get_user_vcs_config_path(&host_data);
    fs::write(
        &config_file,
        serde_json::to_string(&VcsPluginConfig {
            enabled: true,
            plugin: plugin_locator.clone(),
            sha256: plugin_sha256.clone(),
        })
        .into_diagnostic()?,
    )
    .into_diagnostic()?;
    let production_adapter = load_user_vcs_adapter(
        load_git(&fixture.root)?,
        host_data,
        &fixture.root,
        &fixture.root,
    )
    .await?;
    let production_changes = changed_files_to_set(production_adapter.get_changed_files().await?);
    let expected_production_changes = changes([
        ("line\nbreak.txt", "added"),
        ("space name.txt", "added"),
        ("unicode-\u{00e9}.txt", "added"),
        ("working.txt", "added"),
    ]);
    reports.push(ScenarioReport {
        name: "production Moon adapter routes VCS queries through the active plugin".into(),
        passed: production_changes == expected_production_changes,
        expected: expected_production_changes,
        git: None,
        jj: production_changes,
    });

    let git_only_root = create_git_only_fixture()?;
    reports.push(policy_report(
        "trusted overlay selects Git when jj is not detected",
        matches!(
            activate(Some(&enabled_policy), &git_only_root).await?,
            PrototypeVcsSelection::Git { .. }
        ),
    ));

    println!(
        "{}",
        serde_json::to_string_pretty(&reports).into_diagnostic()?
    );

    let passed = reports.iter().all(|report| report.passed);
    fs::remove_dir_all(&fixture.root).into_diagnostic()?;
    fs::remove_dir_all(&rename_fixture.root).into_diagnostic()?;
    fs::remove_dir_all(&criss_cross.root).into_diagnostic()?;
    fs::remove_dir_all(&secondary_root).into_diagnostic()?;
    fs::remove_dir_all(&secondary_fixture.root).into_diagnostic()?;
    fs::remove_dir_all(&git_only_root).into_diagnostic()?;
    fs::remove_dir_all(&policy_root).into_diagnostic()?;

    if passed {
        Ok(())
    } else {
        Err(miette!("VCS conformance report contains failures"))
    }
}

async fn compare(
    name: &str,
    expected: ChangeSet,
    git: &Git,
    plugin: &VcsPlugin,
    context: &MoonContext,
    prepared: &PreparedVcs,
    query: VcsChangeQuery,
) -> miette::Result<ScenarioReport> {
    let git = query_git(git, &query).await?;

    compare_with_git(name, expected, git, plugin, context, prepared, query).await
}

async fn compare_with_git(
    name: &str,
    expected: ChangeSet,
    git: ChangeSet,
    plugin: &VcsPlugin,
    context: &MoonContext,
    prepared: &PreparedVcs,
    query: VcsChangeQuery,
) -> miette::Result<ScenarioReport> {
    let jj = query_jj(plugin, context, prepared, query).await?;

    Ok(ScenarioReport {
        passed: git == expected && jj == expected,
        name: name.into(),
        expected,
        git: Some(git),
        jj,
    })
}

async fn compare_jj_only(
    name: &str,
    expected: ChangeSet,
    plugin: &VcsPlugin,
    context: &MoonContext,
    prepared: &PreparedVcs,
    query: VcsChangeQuery,
) -> miette::Result<ScenarioReport> {
    let jj = query_jj(plugin, context, prepared, query).await?;

    Ok(ScenarioReport {
        passed: jj == expected,
        name: name.into(),
        expected,
        git: None,
        jj,
    })
}

async fn query_jj(
    plugin: &VcsPlugin,
    context: &MoonContext,
    prepared: &PreparedVcs,
    query: VcsChangeQuery,
) -> miette::Result<ChangeSet> {
    Ok(plugin
        .get_changed_files(GetVcsChangedFilesInput {
            context: context.clone(),
            default_branch: DEFAULT_BRANCH.into(),
            query,
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await?
        .files
        .into_iter()
        .map(|file| (file.path, status_name(file.status).into()))
        .collect())
}

async fn get_jj_state(
    plugin: &VcsPlugin,
    context: &MoonContext,
    prepared: &PreparedVcs,
) -> miette::Result<VcsStatePatch> {
    plugin
        .get_state(GetVcsStateInput {
            context: context.clone(),
            default_branch: DEFAULT_BRANCH.into(),
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await
}

async fn prepare(plugin: &VcsPlugin, context: &MoonContext) -> miette::Result<PreparedVcs> {
    plugin
        .prepare(PrepareVcsInput {
            context: context.clone(),
            consistency: VcsConsistency::FreshSnapshot,
        })
        .await
}

async fn query_git(git: &Git, query: &VcsChangeQuery) -> miette::Result<ChangeSet> {
    let files = match query {
        VcsChangeQuery::WorkingCopy => git.get_changed_files().await?,
        VcsChangeQuery::Previous { revision } => {
            git.get_changed_files_against_previous_revision(&git_revision(revision))
                .await?
        }
        VcsChangeQuery::Between { base, head } => {
            git.get_changed_files_between_revisions(&git_revision(base), &git_revision(head))
                .await?
        }
    };

    Ok(changed_files_to_set(files))
}

fn changed_files_to_set(files: ChangedFiles) -> ChangeSet {
    files
        .files
        .into_iter()
        .map(|(path, statuses)| {
            let status = if statuses.contains(&ChangedStatus::Added)
                || statuses.contains(&ChangedStatus::Untracked)
            {
                "added"
            } else if statuses.contains(&ChangedStatus::Deleted) {
                "deleted"
            } else {
                "modified"
            };

            (path.to_string(), status.into())
        })
        .collect()
}

fn git_revision(revision: &VcsRevision) -> String {
    match revision {
        VcsRevision::Current => "HEAD".into(),
        VcsRevision::Default => DEFAULT_BRANCH.into(),
        VcsRevision::Named(value) => value.clone(),
    }
}

fn status_name(status: VcsChangedStatus) -> &'static str {
    match status {
        VcsChangedStatus::Added => "added",
        VcsChangedStatus::Deleted => "deleted",
        VcsChangedStatus::Modified => "modified",
    }
}

fn changes<const N: usize>(values: [(&str, &str); N]) -> ChangeSet {
    values
        .into_iter()
        .map(|(path, status)| (path.into(), status.into()))
        .collect()
}

fn policy_report(name: &str, passed: bool) -> ScenarioReport {
    ScenarioReport {
        name: name.into(),
        passed,
        expected: ChangeSet::new(),
        git: None,
        jj: ChangeSet::new(),
    }
}

fn load_git(root: &Path) -> miette::Result<Git> {
    Git::load(root, DEFAULT_BRANCH, &["origin".into(), "upstream".into()])
}

fn create_fixture() -> miette::Result<Fixture> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .into_diagnostic()?
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "moon-vcs-conformance-{}-{nonce}",
        std::process::id()
    ));

    fs::create_dir_all(root.join("projects/a")).into_diagnostic()?;
    command(&root, "git", ["init", "-b", DEFAULT_BRANCH])?;
    command(&root, "git", ["config", "user.name", "Moon VCS POC"])?;
    command(
        &root,
        "git",
        ["config", "user.email", "moon-vcs@example.com"],
    )?;

    fs::write(root.join("common.txt"), "common\n").into_diagnostic()?;
    fs::write(root.join("projects/a/old.txt"), "old\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "initial"])?;
    let initial = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "-c", "feature"])?;
    fs::write(root.join("projects/feature.txt"), "feature\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "feature"])?;

    command(&root, "git", ["switch", DEFAULT_BRANCH])?;
    fs::write(root.join("main.txt"), "main\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "main"])?;

    command(&root, "git", ["switch", "-c", "merged"])?;
    command(
        &root,
        "git",
        ["merge", "--no-ff", "feature", "-m", "merge feature"],
    )?;
    let merged = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "feature"])?;
    command(&root, "jj", ["git", "init", "--colocate", "."])?;

    Ok(Fixture {
        root,
        initial,
        merged,
    })
}

fn create_git_only_fixture() -> miette::Result<PathBuf> {
    let root = temp_fixture_root("git-only")?;

    fs::create_dir_all(&root).into_diagnostic()?;
    initialize_git(&root)?;
    fs::write(root.join("README.md"), "git only\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "initial"])?;

    Ok(root)
}

fn create_criss_cross_fixture() -> miette::Result<CrissCrossFixture> {
    let root = temp_fixture_root("criss-cross")?;

    fs::create_dir_all(&root).into_diagnostic()?;
    initialize_git(&root)?;

    fs::write(root.join("common.txt"), "common\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "initial"])?;
    let initial = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "-c", "left"])?;
    fs::write(root.join("left.txt"), "left\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "left"])?;
    let left = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "-c", "right", &initial])?;
    fs::write(root.join("right.txt"), "right\n").into_diagnostic()?;
    command(&root, "git", ["add", "."])?;
    command(&root, "git", ["commit", "-m", "right"])?;
    let right = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "left"])?;
    command(
        &root,
        "git",
        ["merge", "--no-ff", "right", "-m", "left merge"],
    )?;
    let left_merge = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "git", ["switch", "-c", "right-merge", &right])?;
    command(
        &root,
        "git",
        ["merge", "--no-ff", &left, "-m", "right merge"],
    )?;
    let right_merge = command(&root, "git", ["rev-parse", "HEAD"])?;

    command(&root, "jj", ["git", "init", "--colocate", "."])?;

    Ok(CrissCrossFixture {
        root,
        left_merge,
        right_merge,
    })
}

fn temp_fixture_root(name: &str) -> miette::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .into_diagnostic()?
        .as_nanos();

    Ok(std::env::temp_dir().join(format!("moon-vcs-{name}-{}-{nonce}", std::process::id())))
}

fn initialize_git(root: &Path) -> miette::Result<()> {
    command(root, "git", ["init", "-b", DEFAULT_BRANCH])?;
    command(root, "git", ["config", "user.name", "Moon VCS POC"])?;
    command(
        root,
        "git",
        ["config", "user.email", "moon-vcs@example.com"],
    )?;

    Ok(())
}

fn command<I, S>(root: &Path, program: &str, args: I) -> miette::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .into_diagnostic()?;

    if !output.status.success() {
        return Err(miette!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
