# Pluggable VCS architecture

Status: proposed

## Problem statement

Moon's VCS behavior is built around Git. The existing abstraction covers both source-control
semantics and workspace services such as file discovery, ignore checks, and hashing, but only Git is
implemented as a supported provider. Supporting another VCS therefore requires adding its behavior,
dependencies, and maintenance burden to Moon itself.

This is especially limiting for version-control systems that coexist with Git but do not share its
view of the working copy. Jujutsu, for example, may use Git for storage while having different
working-copy snapshots, revisions, bookmarks, merge bases, and workspace roots. Asking Git for
those facts can produce incomplete or incorrect affected-file calculations even though Git remains
the right implementation for hashing and other repository operations.

Making every VCS a first-party Moon integration does not scale. It also prevents community members
from developing support independently and makes local tooling choices a concern for every member of
a repository. [#2047](https://github.com/moonrepo/moon/issues/2047) captures current interest in
Jujutsu support, including a report that Moon cannot operate when Git access is unavailable. Miles
has indicated that this is an area the community can champion.

Before hardening the current proof of concept, we need alignment on whether Moon should expose a
stable extension point for VCS implementations.

## Proposed architecture

Introduce a pluggable VCS layer with Git as Moon's built-in native provider and optional WebAssembly
plugins for other providers.

```text
Moon callers
    |
    v
VCS facade
    |-- built-in Git provider
    |-- optional VCS plugin
    |       |-- overlay mode
    |       `-- standalone mode
    `-- host-owned file services
            |-- file inventory
            |-- ignore handling
            `-- hashing
```

Git remains the default and does not move behind a plugin boundary. When no plugin is configured,
Moon follows the same native Git path it uses today and does not initialize a WebAssembly runtime.

A plugin may operate in one of two modes:

- An **overlay** replaces only the source-control facts and change semantics that differ from the
  built-in provider. Other operations continue to use Git or host-owned file services.
- A **standalone provider** supplies complete source-control semantics without requiring a Git
  repository. Workspace services that do not inherently belong to a VCS remain host-owned.

Plugins use Moon's existing plugin runtime, packaging, and registry infrastructure. The host exposes
a narrow VCS-oriented capability set rather than the general plugin host surface.

### Adapter contract

The host and plugin communicate in terms of Moon's VCS needs rather than provider-specific commands
or query languages. At a high level, an adapter must be able to:

- identify itself and negotiate a compatible protocol;
- determine whether it applies to the current workspace;
- prepare a consistent repository snapshot for the lifetime of a Moon operation;
- report current VCS state; and
- answer working-copy, previous-revision, and between-revision change queries.

Preparation returns an opaque snapshot token that is supplied to later queries. The adapter owns
the token's meaning. This gives all queries within a Moon operation a consistent view and gives the
host a safe cache boundary without teaching Moon about provider-specific concepts such as Jujutsu
operation IDs.

Change queries describe intent instead of syntax. The adapter is responsible for revision
resolution, first-parent behavior, merge-base semantics, and representing both sides of a rename.
Moon remains responsible for interpreting the resulting paths within its project and task graphs.

### Activation and trust

Both user-local and workspace-owned activation should be possible. User-local activation supports
tools that an individual adopts without requiring the rest of a team or CI to install them.
Workspace activation supports repositories that intentionally standardize on a provider. The exact
configuration, precedence, and consent model remain open design questions. In particular, checking
out a repository must not be sufficient to execute an untrusted plugin without user or CI consent.

Plugins are executable code and must be treated as a trust boundary. Moon should verify the package
selected by policy and expose only the capabilities required by the adapter. The package and trust
workflow can build on the existing plugin system, but its final shape does not need to be decided as
part of approving the VCS interface.

## Proof of concept

The current Jujutsu adapter is a community proof of concept for the proposed seam, not a proposal to
ship Jujutsu as a first-party Moon provider. It validates that:

- the existing plugin machinery can load a packaged VCS adapter without a new runtime;
- an overlay can replace Jujutsu-sensitive state while retaining Git-owned behavior;
- one explicit preparation can pin later queries to an immutable repository snapshot;
- semantic change queries can cover working-copy, previous-revision, divergent-history, and
  multiple-merge-base cases without exposing revsets to Moon;
- machine-sensitive paths, cross-project renames, colocated repositories, and secondary workspaces
  can cross the host/plugin boundary correctly;
- file hashing does not need to cross the plugin boundary; and
- plugin execution can be limited to a small, guarded host capability set.

The proof of concept also includes conformance fixtures, integrity checks, fallback behavior, and
bounded host-side query caching. These demonstrate feasibility; their exact APIs and policies are
implementation details to review in production PRs. It validates the overlay mode; a standalone
provider still needs its own implementation and conformance validation.

## Performance validation

Performance needs to be demonstrated along two separate axes.

First, the native Git path must not regress. The branch should add or extend benchmarks in Moon's
existing Criterion/CodSpeed suite so that Git startup, state queries, changed-file queries, and file
hashing are compared directly with `master`. This is the relevant baseline for users who have not
enabled a plugin. A configured plugin that reports itself inactive should be measured separately so
that its one-time loading and detection cost is visible rather than attributed to the default path.

Second, active-plugin measurements must distinguish the cost of the WebAssembly boundary from work
performed by the adapter. A deterministic fixture guest should measure cold loading and warm calls
with small and large responses without launching an external process. Separate Jujutsu measurements
should report adapter process time and host-cached time. This prevents Jujutsu CLI startup from being
misreported as WebAssembly overhead.

Current local measurements are directional: external `jj` calls dominate uncached query latency,
while queries served from the prepared-snapshot cache are negligible. They are not a substitute for
the `master` comparison. Productionization should not merge until the benchmark suite shows that the
default Git path remains within normal measurement noise and quantifies the cold and warm costs paid
by an enabled plugin.

## Decision requested

We are asking for:

- approval of the high-level VCS plugin boundary;
- agreement that Git remains Moon's native default while community providers can be distributed as
  plugins; and
- approval to evolve the current proof of concept into a production implementation.

This proposal intentionally leaves function names, serialized messages, configuration schemas,
fallback rules, cache sizes, package installation, trust distribution, and protocol evolution to the
implementation PRs. If the architecture is accepted, those details can be reviewed incrementally
without discarding the working proof of concept.

## Non-goals

- Replacing or reimplementing Moon's built-in Git provider.
- Shipping Jujutsu as a first-party provider maintained by Moon.
- Standardizing provider-specific revisions or query languages.
- Giving VCS plugins unrestricted process, shell, network, or filesystem access.
- Finalizing the production protocol or configuration in this tracking issue.
