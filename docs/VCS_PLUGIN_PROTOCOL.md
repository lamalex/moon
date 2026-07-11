# VCS plugin protocol

Status: proposed version 1 implementation contract

This document specifies the version-1 overlay described by
[`VCS_PLUGIN.md`](./VCS_PLUGIN.md). The architecture proposal discusses possible standalone
providers; version 1 implements only Git-backed overlays. Standalone providers require a separate
protocol because Moon must first define non-Git implementations of file inventory, ignore checks,
hashing, remote metadata, and hooks. Version 1 also chooses user-scoped activation only; workspace
activation remains a future consent and precedence design problem.

## Scope

Moon supports one user-scoped WebAssembly plugin that overlays VCS state and changed-file queries on
its built-in Git adapter. Git remains responsible for repository discovery, default-branch and
remote metadata, file inventory, ignore checks, file hashing, hooks, and shallow-checkout detection.

The initial proof guest targets Jujutsu, but neither Jujutsu commands nor revsets are part of the
guest interface.

## Requirements

- Plain Git behavior and startup cost must remain unchanged when no plugin is configured.
- Activation must be explicit and user-scoped; repository configuration cannot activate a plugin.
- All plugin queries in one Moon process must observe one prepared repository snapshot.
- Messages must describe Moon intent without exposing provider-specific query languages.
- Plugin bytes must be verified before execution.
- A plugin receives no network or direct filesystem access.
- An inactive plugin may select Git fallback; an active plugin fails closed on later errors.

## Activation policy

Moon reads `~/.moon/vcs.json`:

```json
{
  "enabled": true,
  "plugin": "file:///absolute/path/to/vcs-plugin.wasm",
  "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

The `plugin` field is a Warpgate locator. File locators must be absolute, URL locators must use
HTTPS, and `sha256` must contain exactly 64 hexadecimal characters. Moon verifies the digest against
the exact byte buffer passed to Extism.

When the file is absent or `enabled` is false, Moon returns its built-in Git adapter without loading
Extism or probing for another VCS.

## Lifecycle

Moon initializes its VCS adapter at most once per process:

1. Construct the built-in Git adapter.
2. Read and validate the user policy.
3. Return Git when policy is absent or disabled.
4. Resolve and verify the configured plugin.
5. Instantiate it with the restricted VCS host capability set.
6. Call `register_vcs` and require protocol version 1.
7. Call `detect_vcs` for the process context.
8. Return Git when detection returns `active: false`.
9. Call `prepare_vcs` once with `fresh-snapshot`.
10. Read one state patch for the prepared snapshot.
11. Return a `Vcs` adapter that overlays state and change queries and delegates other behavior to
    Git.

Configuration, integrity, loading, registration, and detection-call errors are fatal for an enabled
policy. A successful inactive detection is not an error. Once detection returns `active: true`,
preparation and query errors are fatal; Moon must not mix plugin and Git answers within a process.

## Message types

Messages are defined in `crates/pdk-api/src/vcs.rs`. The protocol version is
`VCS_PLUGIN_PROTOCOL_VERSION`.

### `register_vcs`

Input contains the plugin ID and host protocol version. Output contains a human-readable name,
optional description, plugin version, and selected protocol version. Guest and host both reject an
unsupported version.

### `detect_vcs`

Input contains a `MoonContext` with virtual working-directory and workspace-root paths. Output
contains `active` and a diagnostic reason. Detection must not mutate repository or working-copy
state. Unsupported repositories and unavailable provider executables return `active: false`.

### `prepare_vcs`

Input contains the context and a consistency requirement:

- `existing-snapshot` observes the provider's current immutable operation without refreshing the
  working copy.
- `fresh-snapshot` observes current filesystem state and produces an immutable operation.

Output contains an opaque `snapshot_id`. Every later query receives this token and must observe that
exact snapshot. Preparation is explicit because snapshots created independently by each query can
produce internally inconsistent affected-file calculations.

### `get_vcs_state`

Input contains the context, Git's configured default branch, and snapshot ID. Output is a partial
patch that may replace:

- diagnostic adapter label;
- current branch, bookmark, or change label;
- current immutable revision;
- whether the current label represents the default state;
- repository root;
- working-copy root.

Missing values retain Git's answer. Patched roots must be absolute paths. Default branch and default
branch revision remain Git-owned in version 1.

### `get_vcs_changed_files`

Input contains the context, configured default branch, snapshot ID, and one semantic query:

- `working-copy`: changes visible in the prepared working copy;
- `previous`: changes introduced by one revision relative to its first parent;
- `between`: changes reachable from `head` relative to the merge base with `base`.

Revisions are `current`, `default`, or an opaque named revision. A named revision is data, not a
provider query expression. A guest must resolve it as exactly one revision and reject zero results,
multiple results, and query-language injection.

For multiple merge bases, `between` compares against a virtual merge of all merge bases. For a root
revision, `previous` compares against an empty tree.

Output paths must be normalized UTF-8 paths relative to Moon's workspace root. Empty, absolute,
current-directory, and parent-directory paths are invalid and rejected by the host. Paths preserve
spaces, tabs, newlines, Unicode, and both sides of a rename. A rename is represented by a deleted
source and added destination. Statuses are `added`, `deleted`, and `modified`.

## Host composition

The adapter routes these existing `Vcs` methods through the prepared plugin snapshot:

- `get_local_branch`;
- `get_local_branch_revision`;
- `get_repository_root` when patched;
- `get_working_root` when patched;
- `is_default_branch` for the patched current label;
- all three changed-file queries.

All other methods delegate to the Git adapter created before activation. File hashes and file trees
never cross the WASM seam.

The host keeps bounded 32-entry least-recently-used caches for state and changed-file outputs. Keys
are complete serialized inputs, including context and snapshot ID. Entries do not cross process or
snapshot boundaries.

## Host capabilities

VCS plugins receive logging and one guarded `exec_command` host function. They do not receive the
general Moon host function set, network access, or preopened filesystem paths.

Version 1's command host supports the installed `jj` executable. It rejects shells, streaming,
executable replacement, environment overrides, path arguments, repository overrides, configuration
overrides, and tool overrides. It permits these shapes within the current workspace:

- read-only `root`, `log`, `diff`, and `op log` calls;
- `new` only with `--no-integrate-operation`, for an isolated fresh snapshot;
- global `--ignore-working-copy` and `--at-operation=<id>` selectors.

Adding another executable or command shape changes the security model and requires a protocol spec
change.

## Performance acceptance

The inactive path must produce output identical to the same Moon revision without VCS plugin support
and pass these release-binary gates against `master`:

- p95 process startup regression no greater than 5% or 2 milliseconds, whichever is larger;
- p95 Git working-copy regression no greater than 5% or 2 milliseconds;
- p95 Git revision-range regression no greater than 5% or 2 milliseconds.

The comparison uses isolated `MOON_HOME`, separate byte-identical Git-only fixtures, alternating
baseline and candidate samples, and output equality checks before timing.

Active-plugin reports separate first cold loading, subsequent loading, direct provider execution,
WASM-routed execution, and host-cached execution. No active-provider latency is attributed to the
WASM boundary without a direct-provider comparison.

## Conformance acceptance

A version-1 guest must cover:

- inactive fallback in a plain Git repository;
- integrity mismatch and protocol mismatch rejection;
- one pinned snapshot across repeated queries;
- fresh versus existing snapshot behavior;
- working-copy, first-parent previous, and merge-base between semantics;
- root revisions and multiple merge bases;
- exact named-revision resolution and injection rejection;
- added, deleted, modified, and renamed files;
- spaces, tabs, newlines, and Unicode in paths;
- colocated and secondary workspaces;
- fail-closed behavior after active detection;
- unchanged Git output and performance when policy is absent.

## Versioning

Version 1 requires an exact integer match during registration. Additive message changes require
defaults for older guests and must preserve version-1 semantics. Removing fields, changing query
semantics, adding standalone providers, expanding host capabilities, or changing fallback behavior
requires a new protocol version.

## Implementation map

- `crates/pdk-api/src/vcs.rs`: serialized guest interface.
- `crates/plugin/src/host.rs`: restricted VCS host capability set.
- `crates/plugin/src/plugin_registry.rs`: verified byte loading.
- `crates/vcs-plugin`: user policy, lifecycle, caching, and Git-overlay adapter.
- `crates/app/src/session.rs`: process-scoped adapter initialization.
- `wasm/vcs-jj-prototype`: non-upstream Jujutsu proof guest.
- `crates/vcs-plugin-prototype`: conformance and benchmark harness.
