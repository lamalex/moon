//! PROTOTYPE: Lightweight latency comparison for the VCS overlay seam.

use crate::plugin::load_prototype_plugin;
use miette::{IntoDiagnostic, miette};
use moon_pdk_api::*;
use moon_vcs::{Vcs, git::Git};
use serde::Serialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_BRANCH: &str = "master";
const DEFAULT_ITERATIONS: usize = 20;

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    budgets: Vec<BudgetResult>,
    iterations: usize,
    note: &'static str,
    passed: bool,
    metrics: Vec<Metric>,
}

#[derive(Debug, Serialize)]
struct GitComparisonReport {
    iterations: usize,
    max_regression_percent: f64,
    max_regression_ms: f64,
    master_binary: PathBuf,
    current_binary: PathBuf,
    passed: bool,
    workloads: Vec<GitComparisonMetric>,
}

#[derive(Debug, Serialize)]
struct GitComparisonMetric {
    name: &'static str,
    master: Metric,
    current: Metric,
    current_to_master_p95_ratio: f64,
    max_current_p95_ms: f64,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct BudgetResult {
    metric: &'static str,
    actual_p95_ms: f64,
    max_p95_ms: f64,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct Metric {
    name: &'static str,
    samples: usize,
    min_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

impl Metric {
    fn new(name: &'static str, mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();

        let milliseconds = |duration: Duration| duration.as_secs_f64() * 1_000.0;
        let total = samples.iter().sum::<Duration>();
        let p50 = samples[(samples.len() - 1) / 2];
        let p95 = samples[((samples.len() * 95).div_ceil(100) - 1).min(samples.len() - 1)];

        Self {
            name,
            samples: samples.len(),
            min_ms: milliseconds(samples[0]),
            mean_ms: milliseconds(total) / samples.len() as f64,
            p50_ms: milliseconds(p50),
            p95_ms: milliseconds(p95),
            max_ms: milliseconds(*samples.last().expect("samples cannot be empty")),
        }
    }
}

pub async fn run(moon_root: &Path, enforce_budgets: bool) -> miette::Result<()> {
    let iterations = std::env::var("MOON_VCS_BENCH_ITERATIONS")
        .ok()
        .map(|value| value.parse::<usize>().into_diagnostic())
        .transpose()?
        .unwrap_or(DEFAULT_ITERATIONS);
    let budget_scale = std::env::var("MOON_VCS_BENCH_BUDGET_SCALE")
        .ok()
        .map(|value| value.parse::<f64>().into_diagnostic())
        .transpose()?
        .unwrap_or(1.0);

    if iterations == 0 || !budget_scale.is_finite() || budget_scale <= 0.0 {
        return Err(miette!(
            "benchmark iterations and budget scale must be greater than zero"
        ));
    }

    let started = Instant::now();
    let plugin = load_prototype_plugin(moon_root, moon_root).await?;
    let cold_load = started.elapsed();
    let context = MoonContext {
        working_dir: plugin.to_virtual_path(moon_root),
        workspace_root: plugin.to_virtual_path(moon_root),
    };

    let detection = plugin
        .detect(DetectVcsInput {
            context: context.clone(),
        })
        .await?;

    if !detection.active {
        return Err(miette!("jj adapter is inactive: {}", detection.reason));
    }

    let prepared = plugin
        .prepare(PrepareVcsInput {
            context: context.clone(),
            consistency: VcsConsistency::ExistingSnapshot,
        })
        .await?;
    let git = Git::load(
        moon_root,
        DEFAULT_BRANCH,
        &["origin".into(), "upstream".into()],
    )?;

    let mut metrics = vec![Metric::new(
        "wasm_plugin_load_first_in_process",
        vec![cold_load],
    )];
    metrics.push(Metric::new(
        "wasm_plugin_load_subsequent",
        sample_async(iterations, || async {
            load_prototype_plugin(moon_root, moon_root)
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "jj_detect_direct_cli",
        sample_sync(iterations, || {
            run_jj(moon_root, ["--ignore-working-copy", "root"])
        })?,
    ));
    metrics.push(Metric::new(
        "jj_detect",
        sample_async(iterations, || async {
            plugin
                .detect(DetectVcsInput {
                    context: context.clone(),
                })
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "jj_prepare_existing_snapshot_direct_cli",
        sample_sync(iterations, || {
            run_jj(
                moon_root,
                [
                    "--ignore-working-copy",
                    "op",
                    "log",
                    "--no-graph",
                    "-n",
                    "1",
                    "-T",
                    "id",
                ],
            )
        })?,
    ));
    metrics.push(Metric::new(
        "jj_prepare_existing_snapshot",
        sample_async(iterations, || async {
            plugin
                .prepare(PrepareVcsInput {
                    context: context.clone(),
                    consistency: VcsConsistency::ExistingSnapshot,
                })
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "jj_prepare_fresh_snapshot_direct_cli",
        sample_sync(iterations, || {
            run_jj(
                moon_root,
                ["op", "log", "--no-graph", "-n", "1", "-T", "id"],
            )
        })?,
    ));
    metrics.push(Metric::new(
        "jj_prepare_fresh_snapshot",
        sample_async(iterations, || async {
            plugin
                .prepare(PrepareVcsInput {
                    context: context.clone(),
                    consistency: VcsConsistency::FreshSnapshot,
                })
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "jj_state_pinned_direct_cli",
        sample_sync(iterations, || {
            run_jj(
                moon_root,
                [
                    &format!("--at-operation={}", prepared.snapshot_id),
                    "log",
                    "--no-graph",
                    "--color",
                    "never",
                    "-r",
                    "@",
                    "-T",
                    "bookmarks.map(|bookmark| bookmark.name()).join(\" \") ++ \"\\0\" ++ change_id.short(8) ++ \"\\0\" ++ commit_id",
                ],
            )
        })?,
    ));
    metrics.push(Metric::new(
        "jj_state_pinned_raw",
        sample_async(iterations, || async {
            let _: VcsStatePatch = plugin
                .call_func_with(
                    "get_vcs_state",
                    GetVcsStateInput {
                        context: context.clone(),
                        default_branch: DEFAULT_BRANCH.into(),
                        snapshot_id: prepared.snapshot_id.clone(),
                    },
                )
                .await?;

            Ok(())
        })
        .await?,
    ));
    plugin
        .get_state(GetVcsStateInput {
            context: context.clone(),
            default_branch: DEFAULT_BRANCH.into(),
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await?;
    metrics.push(Metric::new(
        "jj_state_pinned_host_cached",
        sample_async(iterations, || async {
            plugin
                .get_state(GetVcsStateInput {
                    context: context.clone(),
                    default_branch: DEFAULT_BRANCH.into(),
                    snapshot_id: prepared.snapshot_id.clone(),
                })
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "jj_working_copy_pinned_direct_cli",
        sample_sync(iterations, || {
            run_jj(
                moon_root,
                [
                    &format!("--at-operation={}", prepared.snapshot_id),
                    "diff",
                    "-r",
                    "@",
                    "-T",
                    "status_char ++ \"\\0\" ++ source.path() ++ \"\\0\" ++ target.path() ++ \"\\0\"",
                    "--color",
                    "never",
                ],
            )
        })?,
    ));
    metrics.push(Metric::new(
        "jj_working_copy_pinned_raw",
        sample_async(iterations, || async {
            let _: GetVcsChangedFilesOutput = plugin
                .call_func_with(
                    "get_vcs_changed_files",
                    GetVcsChangedFilesInput {
                        context: context.clone(),
                        default_branch: DEFAULT_BRANCH.into(),
                        query: VcsChangeQuery::WorkingCopy,
                        snapshot_id: prepared.snapshot_id.clone(),
                    },
                )
                .await?;

            Ok(())
        })
        .await?,
    ));
    plugin
        .get_changed_files(GetVcsChangedFilesInput {
            context: context.clone(),
            default_branch: DEFAULT_BRANCH.into(),
            query: VcsChangeQuery::WorkingCopy,
            snapshot_id: prepared.snapshot_id.clone(),
        })
        .await?;
    metrics.push(Metric::new(
        "jj_working_copy_pinned_host_cached",
        sample_async(iterations, || async {
            plugin
                .get_changed_files(GetVcsChangedFilesInput {
                    context: context.clone(),
                    default_branch: DEFAULT_BRANCH.into(),
                    query: VcsChangeQuery::WorkingCopy,
                    snapshot_id: prepared.snapshot_id.clone(),
                })
                .await
                .map(|_| ())
        })
        .await?,
    ));
    metrics.push(Metric::new(
        "git_adapter_load",
        sample_sync(iterations, || {
            Git::load(
                moon_root,
                DEFAULT_BRANCH,
                &["origin".into(), "upstream".into()],
            )
            .map(|_| ())
        })?,
    ));
    metrics.push(Metric::new(
        "git_working_copy_uncached",
        sample_async(iterations, || async {
            let fresh_git = Git::load(
                moon_root,
                DEFAULT_BRANCH,
                &["origin".into(), "upstream".into()],
            )?;
            fresh_git.get_changed_files().await?;

            Ok(())
        })
        .await?,
    ));
    git.get_changed_files().await?;
    metrics.push(Metric::new(
        "git_working_copy_process_cached",
        sample_async(iterations, || async {
            git.get_changed_files().await.map(|_| ())
        })
        .await?,
    ));

    let budgets = evaluate_budgets(&metrics, budget_scale);
    let passed = budgets.iter().all(|budget| budget.passed);
    println!(
        "{}",
        serde_json::to_string_pretty(&BenchmarkReport {
            budgets,
            iterations,
            note: "Pinned host caches avoid repeated jj and Git process execution.",
            passed,
            metrics,
        })
        .into_diagnostic()?
    );

    if enforce_budgets && !passed {
        Err(miette!("VCS plugin benchmark exceeded its p95 budget"))
    } else {
        Ok(())
    }
}

pub fn run_git_comparison(
    master_binary: &Path,
    current_binary: &Path,
    master_fixture: &Path,
    current_fixture: &Path,
) -> miette::Result<()> {
    let iterations = read_env_number("MOON_VCS_BENCH_ITERATIONS", DEFAULT_ITERATIONS)?;
    let max_regression_percent = read_env_number("MOON_VCS_GIT_MAX_REGRESSION_PERCENT", 5.0)?;
    let max_regression_ms = read_env_number("MOON_VCS_GIT_MAX_REGRESSION_MS", 2.0)?;

    if iterations == 0 || max_regression_percent < 0.0 || max_regression_ms < 0.0 {
        return Err(miette!("benchmark settings must not be negative or zero"));
    }

    let workloads: [(&str, &[&str]); 3] = [
        ("process_startup", &["--version"]),
        ("git_working_copy", &["query", "changed-files", "--local"]),
        (
            "git_between_revisions",
            &["query", "changed-files", "--base", "base", "--head", "HEAD"],
        ),
    ];
    let mut comparison = vec![];

    for (name, args) in workloads {
        if name != "process_startup" {
            let master_output = run_moon_capture(master_binary, master_fixture, args)?;
            let current_output = run_moon_capture(current_binary, current_fixture, args)?;

            if master_output != current_output {
                return Err(miette!(
                    "master and current `{name}` outputs differ; refusing to compare unlike work"
                ));
            }
        }

        for _ in 0..3 {
            run_moon(master_binary, master_fixture, args)?;
            run_moon(current_binary, current_fixture, args)?;
        }

        let (master_samples, current_samples) = sample_moon_pair(
            iterations,
            master_binary,
            current_binary,
            master_fixture,
            current_fixture,
            args,
        )?;
        let master = Metric::new(name, master_samples);
        let current = Metric::new(name, current_samples);
        let percentage_limit = master.p95_ms * (1.0 + max_regression_percent / 100.0);
        let absolute_limit = master.p95_ms + max_regression_ms;
        let max_current_p95_ms = percentage_limit.max(absolute_limit);
        let passed = current.p95_ms <= max_current_p95_ms;
        let current_to_master_p95_ratio = current.p95_ms / master.p95_ms;

        comparison.push(GitComparisonMetric {
            name,
            master,
            current,
            current_to_master_p95_ratio,
            max_current_p95_ms,
            passed,
        });
    }

    let passed = comparison.iter().all(|metric| metric.passed);
    println!(
        "{}",
        serde_json::to_string_pretty(&GitComparisonReport {
            iterations,
            max_regression_percent,
            max_regression_ms,
            master_binary: master_binary.to_owned(),
            current_binary: current_binary.to_owned(),
            passed,
            workloads: comparison,
        })
        .into_diagnostic()?
    );

    if passed {
        Ok(())
    } else {
        Err(miette!(
            "current Moon exceeded the Git performance regression threshold"
        ))
    }
}

fn evaluate_budgets(metrics: &[Metric], scale: f64) -> Vec<BudgetResult> {
    [
        ("wasm_plugin_load_subsequent", 100.0),
        ("jj_detect", 150.0),
        ("jj_prepare_existing_snapshot", 200.0),
        ("jj_prepare_fresh_snapshot", 300.0),
        ("jj_state_pinned_raw", 200.0),
        ("jj_state_pinned_host_cached", 1.0),
        ("jj_working_copy_pinned_raw", 200.0),
        ("jj_working_copy_pinned_host_cached", 1.0),
    ]
    .into_iter()
    .filter_map(|(name, max_p95_ms)| {
        metrics
            .iter()
            .find(|metric| metric.name == name)
            .map(|metric| {
                let max_p95_ms = max_p95_ms * scale;
                BudgetResult {
                    metric: name,
                    actual_p95_ms: metric.p95_ms,
                    max_p95_ms,
                    passed: metric.p95_ms <= max_p95_ms,
                }
            })
    })
    .collect()
}

async fn sample_async<F, Fut>(iterations: usize, mut operation: F) -> miette::Result<Vec<Duration>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = miette::Result<()>>,
{
    let mut samples = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let started = Instant::now();
        operation().await?;
        samples.push(started.elapsed());
    }

    Ok(samples)
}

fn sample_sync<F>(iterations: usize, mut operation: F) -> miette::Result<Vec<Duration>>
where
    F: FnMut() -> miette::Result<()>,
{
    let mut samples = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let started = Instant::now();
        operation()?;
        samples.push(started.elapsed());
    }

    Ok(samples)
}

fn sample_moon_pair(
    iterations: usize,
    master_binary: &Path,
    current_binary: &Path,
    master_fixture: &Path,
    current_fixture: &Path,
    args: &[&str],
) -> miette::Result<(Vec<Duration>, Vec<Duration>)> {
    let mut master_samples = Vec::with_capacity(iterations);
    let mut current_samples = Vec::with_capacity(iterations);

    for index in 0..iterations {
        let sample = |binary, fixture, samples: &mut Vec<Duration>| {
            let started = Instant::now();
            run_moon(binary, fixture, args)?;
            samples.push(started.elapsed());
            Ok::<_, miette::Report>(())
        };

        if index % 2 == 0 {
            sample(master_binary, master_fixture, &mut master_samples)?;
            sample(current_binary, current_fixture, &mut current_samples)?;
        } else {
            sample(current_binary, current_fixture, &mut current_samples)?;
            sample(master_binary, master_fixture, &mut master_samples)?;
        }
    }

    Ok((master_samples, current_samples))
}

fn run_jj<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>) -> miette::Result<()> {
    let output = Command::new("jj")
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .into_diagnostic()?;

    if output.success() {
        Ok(())
    } else {
        Err(miette!("direct jj benchmark command failed: {output}"))
    }
}

fn run_moon(binary: &Path, fixture: &Path, args: &[&str]) -> miette::Result<()> {
    let status = moon_command(binary, fixture, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .into_diagnostic()?;

    if status.success() {
        Ok(())
    } else {
        Err(miette!("Moon benchmark command failed: {status}"))
    }
}

fn run_moon_capture(binary: &Path, fixture: &Path, args: &[&str]) -> miette::Result<Vec<u8>> {
    let output = moon_command(binary, fixture, args)
        .output()
        .into_diagnostic()?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(miette!(
            "Moon benchmark command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn moon_command(binary: &Path, fixture: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(fixture)
        .env("MOON_HOME", fixture.join(".moon-home"))
        .env("NO_COLOR", "1");
    command
}

fn read_env_number<T>(name: &str, default: T) -> miette::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<T>().into_diagnostic())
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_scaled_p95_budgets() {
        let metric = Metric::new(
            "jj_working_copy_pinned_raw",
            vec![Duration::from_millis(250)],
        );

        assert!(!evaluate_budgets(&[metric], 1.0)[0].passed);

        let metric = Metric::new(
            "jj_working_copy_pinned_raw",
            vec![Duration::from_millis(250)],
        );
        assert!(evaluate_budgets(&[metric], 2.0)[0].passed);
    }
}
