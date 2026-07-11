# VCS Overlay Prototype

## Question

Can Moon load a packaged WASM VCS overlay through its existing plugin machinery,
merge partial Jujutsu state over Git, and route dynamic changed-file queries
without moving file hashing into the plugin interface?

Run the prototype with:

```shell
just prototype-vcs
```

Run semantic conformance and latency measurements with:

```shell
just prototype-vcs-conformance
just prototype-vcs-benchmark
just prototype-vcs-native
```

Resolve the user-scoped activation policy with:

```shell
just prototype-vcs-policy
just prototype-vcs-policy-install '<locator>' '<trusted-sha256>'
just prototype-vcs-policy-install-local
just prototype-vcs-policy-install-signed '<manifest>' '<signature>' '<public-key>'
just prototype-vcs-policy-disable
just prototype-vcs-policy-enable
```

Remote install does not download and trust its own bytes. Its SHA-256 must come
from independent trusted release metadata. The local install convenience only
computes a digest for the explicitly selected local WASM file.
Signed install verifies a Minisign signature over a JSON manifest containing
`plugin` and `sha256`, then atomically installs that pinned policy.

The prototype reads `vcs.json` from the Moon store: `$MOON_HOME`,
`$XDG_DATA_HOME/moon`, or `~/.moon`:

```json
{
  "enabled": true,
  "plugin": "registry://moonrepo/vcs-jj:0.1.0",
  "sha256": "<64 hexadecimal characters>"
}
```

This code is throwaway. Delete it or turn the findings below into an upstream
proposal once the interface discussion is complete.

## Findings

- `PluginRegistry` can load the VCS guest without a new runtime or distribution
  mechanism. Adding a plugin type and typed PDK messages is enough for the POC.
- A partial state patch is a useful overlay interface. Jujutsu replaced the
  empty Git branch and stale Git `HEAD`, while repository roots and default
  branch facts continued to come from Git.
- Changed-file queries can be expressed as `WorkingCopy`, `Previous`, and
  `Between` intents. The guest owns revision translation and merge-base
  semantics instead of teaching Moon about `@` and revsets.
- Detection can be read-only by invoking `jj --ignore-working-copy root`.
- Snapshot policy cannot remain an implementation detail. A read-only state
  query observed commit `85d02adc`; a refreshing query snapshotted the new POC
  files and changed the working-copy commit to `916a410f`.
- `jj op log -n 1 -T id` can perform the one allowed snapshot and return the
  resulting operation ID in the same command. Later queries can use
  `--at-operation=<id>`, which also implies `--ignore-working-copy`.
- Operation IDs provide real snapshot isolation. In an isolated repository, a
  pinned query continued to see commit `cc3df1b6` after a concurrent operation
  moved the live working copy to `2860493f`.
- Forced fallback worked without changing the guest. The host stopped applying
  the patch and routed changed-file queries back to Git.
- File hashing was not needed anywhere in the guest interface.
- A criss-cross fixture produced two best common ancestors, and Git and
  `jj latest(...)` did not reliably choose the same one. The adapter now creates
  an auto-merged base under `--no-integrate-operation`, diffs its tree, and
  proves the live operation is unchanged. This avoids adapter tie-breaking and
  the conservative union's overbuild.
- `jj diff --summary` is display output and compacts rename paths with braces.
  The guest now uses a NUL-delimited `TreeDiffEntry` template with separate
  source and target paths, preserving cross-project rename semantics.
- The same transport preserves spaces, newlines, and Unicode in paths across
  the host/WASM JSON interface.
- A secondary `jj` workspace can load and detect the same adapter without a Git
  working tree. Prepared operation IDs remain pinned across later workspace
  snapshots, while a fresh preparation observes the new state.
- Release measurements show that raw `jj` process calls dominate query latency.
  Moon's Git adapter memoizes command output; the opaque prepared snapshot ID
  gives the host an equivalent safe cache key for plugin state and diff calls.
- On this workspace, the latest 20-sample release run measured p95 latency of
  22.9 ms for a subsequent plugin load, 16.5 ms to reuse an existing snapshot,
  37.0 ms to prepare a fresh snapshot, and 32.6 ms for a raw pinned working-copy
  diff. The bounded host cache reduced the pinned diff to 0.003 ms. The first
  cold plugin load in the process measured 542.7 ms from one sample and remains
  separate from the repeated-load budget. These are local directional results.
- The release budget gate checks eight p95 ceilings. The latest run passed all
  limits, including 100 ms for subsequent loading, 300 ms for fresh preparation,
  200 ms for raw pinned diffs, and 1 ms for cached pinned diffs.
- A controlled 20-sample comparison used release binaries, isolated `MOON_HOME`,
  byte-identical Git-only fixtures, alternating samples, and output equality
  checks. Current versus `master` p95 was 9.01 versus 9.30 ms for process
  startup, 120.45 versus 120.98 ms for working-copy queries, and 230.86 versus
  231.17 ms for revision-range queries. All passed the 5% or 2 ms regression
  gate.
- Adapter revisions are resolved under the prepared operation before diff
  revsets are composed. Zero- or multi-commit inputs fail instead of producing
  ambiguous paths or injecting an unintended set into merge-base selection.
- The host keeps bounded 32-entry LRUs for state and changed-file outputs,
  keyed by the complete serialized input including context and snapshot ID.
- Plugin acquisition is not plugin trust. Warpgate hashes locators for most
  cache paths, so user activation requires a separate expected SHA-256. The
  registry verifies one byte buffer and gives that exact buffer to Extism. It
  also refuses to bless a pre-existing instance under the same ID.
- User policy is intentionally outside the repository. Missing or disabled
  policy, and a successful inactive detection, select Git. Integrity, loading,
  detection-call, and active-query errors fail closed instead of silently
  changing VCS semantics mid-command.
- Install/update writes are validated and atomically replace the user policy;
  local files are canonicalized, remote URLs require HTTPS, and Unix policy
  files are created with user-only permissions. Enable/disable preserves the
  pinned locator and digest.
- Signed provenance can derive the locator and digest from publisher metadata
  after verifying its Minisign signature against an independently supplied
  public key. Tampered manifests fail before policy state changes.
- The VCS manifest removes direct WASI network and path access and applies a
  two-minute timeout. Its host capability profile exposes only logging and a
  guarded `jj` command: the working directory must remain inside the workspace;
  shell, environment, PATH, config, repository, and external-tool overrides are
  rejected; and only `root`, `log`, `diff`, and `op log` are accepted.
- A throwaway native adapter can load the CLI-produced operation ID with
  `jj-lib 0.43`, retain the repository and index in process, and implement the
  same state and changed-file interface without invoking `jj` for each query.
- Native queries passed every semantic fixture covered by the CLI guest:
  first-parent previous diffs, divergent histories, exact virtual merge bases,
  machine-sensitive paths, source/destination rename reporting, secondary
  workspaces, ambiguous revision rejection, and pinned-operation isolation.
- `merge_commit_trees()` computes a criss-cross virtual merge base directly in
  memory. Unlike the CLI guest's isolated `jj new`, it does not write an
  unpublished commit or operation object.
- The native prototype can also lock and snapshot a normal fresh working copy,
  rewrite its working-copy commit, rebase descendants, publish the operation,
  and finish the working-copy state. An operation written by `jj-lib 0.43` was
  successfully read and queried by the installed `jj 0.42.0` CLI.
- The native fresh-snapshot implementation intentionally fails on stale or
  concurrently modified working copies. It does not yet duplicate `jj-cli`'s
  stale recovery, colocated Git import/export cycle, user/repository config
  loading, immutable-commit policy, or untracked-file reporting.
- On this workspace, a 20-sample release run measured p95 latency of 4.4 ms to
  reload a pinned repository with `jj-lib`, 4.9 ms to load and run a working
  copy diff, and 17.9 ms to prepare a fresh native snapshot. The equivalent CLI
  path measured 22.5 ms to prepare an existing snapshot, 27.2 ms for a pinned
  working-copy diff, and 53.0 ms for a fresh snapshot.
- Reusing one loaded native repository measured 0.001 ms p95 for state and
  0.020 ms p95 for a working-copy diff. This is comparable to the existing host
  result cache while still executing the typed query instead of returning a
  serialized cached answer.
- The native result is not directly distributable as a community WASM plugin.
  A minimal `jj-lib 0.43` workspace load fails to compile for
  `wasm32-wasip1`: `jj-lib` only defines filesystem, locking, and executable-bit
  implementations for `cfg(unix)` and `cfg(windows)`. The WASI build reached
  `jj-lib` successfully (including `gix` and Rayon) before failing with 16
  target-platform errors concentrated in those modules.
- Even after adding WASI platform implementations, embedded `jj-lib` would need
  direct preopened access to the workspace, `.jj`, and colocated `.git`. The
  current VCS guest intentionally receives no allowed paths, so this would be a
  new declared capability and security model rather than an implementation-only
  swap.
- Locking cannot safely be stubbed out in a WASI fork. The plugin must coordinate
  with native `jj` processes touching the same operation and working-copy state,
  either through a WASI-compatible lock primitive with matching semantics or a
  dedicated host lock capability.

## Follow-ups

- Promote `prepare_vcs` and its opaque snapshot token into the proposed
  production interface. Dynamic state and change queries should require it.
- Move file inventory and ignore matching behind a separate host-owned module.
- Reassess whether a persistent `jj` command service is necessary after the
  bounded host cache is exercised by real Moon command lifecycles.
- Decide whether first-party Jujutsu support should use the native adapter while
  retaining WASM subprocess adapters as the packageable third-party extension
  model.
- Port or isolate the required `jj-cli` snapshot orchestration before treating
  native fresh snapshots as production-safe, especially Git import/export,
  stale recovery, configuration, immutable commits, and concurrent operations.
- Add repository-format compatibility fixtures across the oldest and newest
  supported `jj` versions before pinning a `jj-lib` release in Moon.
- If WASM-only distribution is required, evaluate an upstreamable `jj-lib` WASI
  port covering path encoding, symlinks, file identity, executable bits, and
  cross-process locking. Avoid carrying a no-op-lock community fork.
- Calibrate the opt-in release p95 budgets across CI machine classes; use
  `MOON_VCS_BENCH_BUDGET_SCALE` only for documented hardware differences.
- Replace the hard-coded VCS capability profile with declarative capabilities
  if a second VCS adapter needs a different executable or operation set.
- Decide how publisher public keys are distributed and rotated without moving
  the trust-on-first-use problem into Moon itself.

## Verdict

The snapshot architecture is validated. `prepare_vcs` can create one explicit
fresh snapshot, return an opaque token, and isolate every later query from
concurrent Jujutsu operations. Snapshot behavior is no longer an architectural
blocker for the plugin seam.

Native `jj-lib` embedding validates the performance opportunity and provides a
useful semantic oracle, but it is not a production route for a community plugin
that must remain WASM. The current viable distributable implementation remains
the WASM guest with guarded CLI invocation. Reaching the native performance
profile inside that guest requires both an upstreamable `jj-lib` WASI port and
a broader, explicit filesystem/locking capability model.

This implementation is still a prototype. Production work remains around
publisher key distribution, CI budget calibration, and promoting the interface
into non-prototype modules.
