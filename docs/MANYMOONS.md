# Manymoons

## Status

Manymoons is a design and implementation effort. The external configuration and command syntax
described by this document are intentionally undecided unless explicitly marked otherwise.

Milestone 1, internal source-root qualification, Milestone 2, direct multi-workspace discovery,
Milestone 3, aggregated querying, Milestone 4, cross-source project dependencies, and Milestone 5,
the unified task graph, are complete. Milestone 6, cross-source hashing and cache safety, is next.

## End Goal

A logical Moon project, task, and action graph may span multiple independently versioned source
control roots. Projects in one source root may depend on projects in another source root, and their
tasks may participate in one execution graph.

Repository topology must not be a project-graph boundary:

```text
many source roots
  -> one ProjectGraph
  -> cross-source project dependencies
  -> one TaskGraph
  -> one ActionGraph
  -> the existing action pipeline and dispatcher
```

Manymoons should generalize Moon's existing graph machinery. It should not introduce a parallel
project graph, task graph, action graph, or scheduler solely for cross-repository operation.

In this document, a **source root** is a participating Moon configuration and execution root. It is
not necessarily the VCS repository root or provider working root. A VCS repository containing
multiple participating Moon workspaces contributes multiple source roots with distinct source IDs.
The #2620 provider context attached to each source root reports its repository and working roots
separately.

## Compatibility Invariants

Manymoons must be additive for users even though it requires significant internal changes.

1. When only one source root is registered, graph output, target resolution, task selection,
   affected behavior, caching, and execution must retain current Moon semantics.
2. Registering another source root must not silently broaden an existing unqualified command.
3. Existing workspace and project configuration must remain valid for a single-source workspace.
4. Existing target syntax remains source-local unless the user explicitly opts into broader or
   qualified resolution.
5. Absolute checkout paths must not become part of persisted logical identity or cache keys.

For example, the current meaning of these targets should remain local to the current or primary
source:

```text
app:build
~:build
:build
```

The syntax for explicitly selecting another source or all sources remains an open design question.

## Architectural Prerequisite

The VCS abstraction work in [moonrepo/moon#2620](https://github.com/moonrepo/moon/issues/2620) is
an architectural prerequisite and integration seam. It does not block the single-source identity
work in Milestone 1, but it blocks multi-source discovery in Milestone 2.

Manymoons requires a source-control provider and observation to be instantiated and addressed per
source root. It should not add another Git-shaped abstraction. Consistent with #2620, Moon should
continue to own ordinary file discovery and hashing, while providers own source-control history,
tracked and ignored state, and changed-file calculation.

Before Milestone 2, #2620 must provide per-root provider instantiation, provider identity, an
observation lifecycle, changed-file completeness, and explicit unavailable/error behavior.

## Current Boundaries

Moon currently turns a working directory into one process-wide workspace context:

```text
working directory
  -> one workspace root
  -> one configuration and VCS context
  -> one ProjectGraph
  -> one TaskGraph
  -> one ActionGraph
```

The principal assumptions are:

- `MoonSession`, `AppContext`, `WorkspaceBuilderContext`, and `GraphExpanderContext` carry one
  workspace root and one VCS context.
- Project discovery walks one workspace root and produces paths in one workspace-relative path
  namespace.
- `ProjectGraph` is keyed by an unqualified project `Id`.
- Project dependencies contain an unqualified project `Id` and resolve within one workspace
  builder.
- `TaskGraph` and task state are keyed by an unqualified `Target`.
- `ActionGraphBuilder` builds through one `AppContext` and one `WorkspaceGraph`.
- Root-sensitive action nodes carry unqualified project IDs or workspace-relative paths.
- File inputs, fingerprints, manifests, archives, daemon requests, and hydration use paths relative
  to one workspace root.

The scheduler is not the primary boundary. `JobDispatcher` schedules an arbitrary `ActionGraph`
using graph edges and node state. Source-specific routing should happen when actions are built and
executed, not by replacing the dispatcher.

## Minimal Internal Model

The foundational model makes source ownership explicit wherever identity or a relative path can
cross a source boundary.

```rust
struct SourceRootId(Id);

struct SourceAlias(Id);

struct SourcePathBuf {
    source: SourceRootId,
    path: WorkspaceRelativePathBuf,
}

struct ProjectKey {
    source: SourceRootId,
    project: Id,
}

struct TaskKey {
    project: ProjectKey,
    task: Id,
}

struct SourceRegistry {
    primary: SourceRootId,
    roots: Map<SourceRootId, PathBuf>,
}

struct SourceDeclaration {
    alias: SourceAlias,
    path: PathBuf,
    expected_id: Option<SourceRootId>,
}
```

These types have distinct roles:

- `SourceRootId` is a stable logical identifier. It is not an absolute checkout path or VCS
  revision. A workspace declares its own canonical ID.
- `SourceAlias` is a local label assigned by the workspace that discovers another workspace. It is
  not canonical identity and must not appear in persisted graph keys or cache keys.
- `SourcePathBuf` pairs a relative path with the source root required to interpret it.
- `ProjectKey` is canonical graph identity. A bare project `Id` remains an input to source-aware
  resolution.
- `TaskKey` is canonical executable-task identity.
- `Target` remains user-facing and potentially scoped or unresolved. Resolution converts a target
  into one or more `TaskKey` values.
- `SourceRegistry` maps stable identities to runtime roots and eventually to their source-specific
  configuration, VCS observation, cache, and execution services.

Qualified identities should have stable serialization suitable for graph caches, fingerprints,
state paths, and JSON map keys. Persisted identities must not contain machine-specific absolute
paths.

The primary single-source compatibility ID is `workspace`. A workspace may instead self-declare a
canonical ID in its own workspace configuration. Additional sources must self-declare explicit,
unique logical IDs before they can participate in discovery, persisted references, fingerprints,
or graph caches. Discovery-map keys are local aliases only. Path-derived, alias-derived, and
insertion-order IDs must not silently become durable identity. Renaming a source ID is an identity
migration rather than a directory rename.

For the initial direct-discovery model:

```yaml
# platform/.moon/workspace.yml
id: acme/platform

workspaces:
  frontend:
    path: ../web
    id: acme/web # Optional expected identity pin
```

```yaml
# web/.moon/workspace.yml
id: acme/web
```

`frontend` is local to `acme/platform`; `acme/web` is canonical. Moon loads the discovered
workspace's own configuration and verifies the optional expected ID. A declaration does not assign
identity to the discovered workspace.

## Required End-State Changes

The following changes are architectural requirements, not optional milestones.

### Identity and Resolution

- Key `ProjectGraph` nodes, aliases, expansions, and edges by `ProjectKey`.
- Make bare project-ID and alias resolution source-aware.
- Resolve project dependency configuration to `ProjectKey` after all source-local projects have
  been loaded.
- Key `TaskGraph`, task expansion, action state, and execution state by `TaskKey`.
- Treat a root project and `DependencyScope::Root` as relative to a specific source root.

### Paths and Runtime Context

- Qualify every relative path that may cross a source boundary with `SourceRootId`.
- Maintain source-specific configuration, VCS observation, hashing, cache, and execution contexts.
- Route actions to the appropriate source context at execution time.
- Replace graph-global workspace setup actions with source-qualified actions where the operation
  is root-sensitive.
- Preserve global deduplication only for operations that are genuinely process-global, such as a
  shared tool installation.

### Hashing and Caching

- Include stable source identity alongside relative input paths in task fingerprints.
- Use canonical task keys for dependency hash inputs and target state paths.
- Preserve the producing source when dependency outputs become consumer inputs.
- Pack and hydrate outputs through the owning task's source context.
- Include source identity in daemon requests that currently inherit one daemon workspace root.
- Continue sharing content-addressed blobs globally where safe; CAS does not need to be split by
  source root.

### Graph and Execution Composition

- Compose source-local projects into one `ProjectGraph`.
- Represent cross-source project dependencies as ordinary project graph edges.
- Build one `TaskGraph` from the composed project graph.
- Represent cross-source task dependencies as ordinary task graph edges.
- Build one `ActionGraph` and run it through the existing `ActionPipeline` and `JobDispatcher`.

## Milestones

Each milestone should be independently shippable and preserve single-source behavior.

Cross-source behavior remains feature-gated until its milestone exit criteria are met. Every
milestone must include single-source compatibility tests and must treat an unsupported persisted
format as a cache miss rather than a command failure.

### Milestone 1: Internal Source-Root Qualification

User-visible multi-source behavior is not enabled.

- Introduce `SourceRootId`, `SourcePathBuf`, `ProjectKey`, `TaskKey`, and `SourceRegistry`.
- Initialize a registry containing exactly one primary source.
- Thread the registry through graph boundaries while preserving existing root accessors.
- Keep current command syntax, target resolution, project graph keys, and task graph keys unchanged
  until their migrations can be made atomically.
- Add stable serialization and tests for disambiguation, root resolution, duplicate roots, and path
  containment.

This milestone creates the types and compatibility seam required by every later change without
altering command behavior.

Exit criteria: the complete existing single-source command test suite remains semantically
unchanged, and no multi-source command behavior is enabled.

### Milestone 2: Multi-Workspace Discovery

- Allow a workspace to self-declare its canonical source ID.
- Configure direct workspace discovery through a map of local aliases to paths and optional
  expected identity pins.
- Require every discovered workspace to self-declare its canonical ID.
- Resolve declaration paths relative to the declaring workspace root.
- Keep discovery non-recursive initially; discovered workspace declarations are not traversed.
- Load source-local configuration independently.
- Create source-specific configuration, plugin, toolchain, cache, and runtime service contexts.
- Instantiate one #2620 provider and observation per source root.
- Expose diagnostics that list source IDs, physical paths, provider state, and load failures.
- Do not allow cross-source dependencies or implicitly broaden commands yet.

Discovery may temporarily coordinate source-local loaders, but it must not establish permanently
separate graph or scheduler architectures.

Canonical IDs are unique within one composed invocation. Discovering the same canonical ID through
multiple aliases may deduplicate only when it resolves to the same physical root. The same ID at
different roots, or different IDs at the same root, is an error. Local aliases are scoped to the
declaring workspace and may be reused elsewhere.

Exit criteria: multiple sources can be listed and diagnosed, while all existing unqualified
commands remain scoped to the current or primary source.

Live child-root watcher routing remains deferred to Milestone 7. Milestone 2 loads and isolates
every direct source for each session, while daemon file events remain primary-root scoped.

### Milestone 3: Aggregated Querying

- Compose discovered source-local projects into one canonical `ProjectGraph`.
- Migrate project nodes, aliases, expansions, dependency edges, and lookups from bare `Id` values to
  `ProjectKey`.
- Initially add only intra-source dependency edges.
- Aggregate project and task queries, tags, aliases, and path lookup.
- Display qualified identities when an unqualified project ID or alias is ambiguous.
- Keep unqualified resolution local to the current or primary source.
- Version the workspace graph cache and treat older key formats as cache misses.
- Version plugin graph-extension inputs so plugins can receive source-qualified project identity
  without changing single-source behavior.

This milestone validates discovery, identity, composition, and user-facing disambiguation without
the execution and cache risks of cross-source task dependencies.

Exit criteria: duplicate project IDs can coexist across sources in queries and graph output, but
no cross-source dependency or execution edge exists.

### Milestone 4: Cross-Source Project Dependencies

- Define configuration syntax that can identify a project in another source.
- Resolve cross-source references after all source-local projects are known.
- Insert cross-source dependencies as ordinary `ProjectGraph` edges.
- Apply existing cycle, partition, scope, and constraint semantics across source roots.
- Make root-project semantics source-relative.
- Expose the dependencies through graph and query commands before enabling execution.

Cross-source dependencies use the existing dependency object with `sourceRoot`, which accepts a
direct workspace alias or canonical source-root ID. Source-local dependencies continue to omit this
field.

Exit criteria: graph and query commands expose validated cross-source project edges, while run
commands reject or ignore those edges behind the feature gate.

### Milestone 5: Unified Task Graph

- Migrate canonical task identity from `Target` to `TaskKey` internally.
- Resolve task dependencies against the composed project graph.
- Allow task dependency edges to cross source roots.
- Preserve existing task graph traversal and cycle detection.
- Keep `Target` as the command and configuration selector language.

Action construction and execution for cross-source edges must remain disabled until Milestones 6
and 7 are complete. Milestone 5 is a graph and query capability, not an uncached execution mode.

The composed task graph is keyed by `TaskKey` and retains source-specific expansion contexts.
Source-local `Target` selectors remain compatibility adapters at command and configuration
boundaries. Configured dependency selectors are preserved before local expansion, then `^:task`
and scoped variants are resolved again against canonical composed project edges. Aggregate graph
and query commands use this single graph. Action construction consults it only to reject task
closures containing cross-source edges with an explicit unsupported-feature error.

Exit criteria: task graph output can represent cross-source edges, but attempting to execute one
produces an explicit unsupported-feature error.

### Milestone 6: Cross-Source Hashing and Cache Safety

- Route input collection, changed-file lookup, file hashing, output packing, and hydration by
  source.
- Make source-specific runtime services addressable independently of action execution.
- Qualify fingerprint paths, dependency identities, and target state keys.
- Verify manifest and shared-CAS portability across source roots.
- Route daemon hashing and hydration requests using explicit source identity.
- Ensure affected calculations combine source-specific changed-file observations without losing
  completeness information from #2620.

Cached cross-source execution should not ship before this milestone is complete.

Exit criteria: source-qualified hashing, state, packing, hydration, and daemon operations pass
cross-source tests without enabling cross-source action dispatch.

### Milestone 7: Cross-Source Action Execution

- Build one `ActionGraph` from the unified task graph.
- Source-qualify root-sensitive run, sync, setup, and dependency-install actions.
- Route each action to its source-specific runtime context.
- Invalidate and rebuild only affected source contexts in daemon and watcher flows where possible.
- Preserve the existing pipeline, topological ordering, priority handling, concurrency semaphore,
  interactive behavior, and persistent-task behavior.

This milestone completes the end-to-end path without introducing a second scheduler.

Exit criteria: cross-source dependencies execute and cache correctly through one action graph, and
single-source commands produce the same target selection and action semantics as before.

## Requirement Ownership

| Requirement | Owning milestone |
| --- | --- |
| Qualified identity and path primitives | 1 |
| Per-source configuration, VCS, plugin, toolchain, cache, and runtime contexts | 2 |
| Canonical project keys, graph-cache migration, and plugin graph identity | 3 |
| Cross-source project dependency resolution | 4 |
| Canonical task keys and cross-source task edges | 5 |
| Hashing, affected, cache, manifest, hydration, and daemon routing | 6 |
| Action routing, execution, watcher invalidation, and end-to-end compatibility | 7 |

## Non-Goals

- Replacing Moon's project, task, or action graph implementations.
- Building a graph of independent workspace graphs as the permanent execution model.
- Introducing a repository-aware scheduler alongside `JobDispatcher`.
- Encoding absolute checkout paths or VCS revisions into logical project identity.
- Making source-control providers responsible for Moon's ordinary file hashing.
- Silently changing the scope of existing unqualified commands.
- Settling external source qualification syntax during the internal identity milestone.

## Risks

### Identity Ambiguity

Duplicate project IDs, aliases, tags, and task targets are valid across different source roots but
ambiguous without a resolution scope. Internal code must stop using display strings as canonical
identity before multiple sources are composed.

### Cache Compatibility

Changing graph map keys or fingerprint schemas can invalidate persisted workspace graph and task
caches. Cache formats or filenames must be versioned, and deserialization failures should degrade
to cache misses rather than command failures.

### Partial Graph Loading

The synchronous and asynchronous workspace builders currently differ in how they load unknown or
dependency projects. Cross-source resolution requires one consistent composition and finalization
model.

### Root-Sensitive Global Actions

`SyncWorkspace`, project sync, dependency installation, environment setup, and daemon operations
currently inherit one workspace root. Incorrect deduplication could run an operation in the wrong
source or skip a required operation.

### Affected Completeness

Each source may have a different provider, baseline, observation, or completeness result. Aggregated
affected behavior must remain conservative when any source cannot provide an exact answer.

### Plugin Compatibility

Plugin contexts and project-graph extension inputs currently expose one workspace root and
unqualified project IDs. Protocol evolution must be versioned and should preserve single-source
plugin behavior.

### Execution Isolation

Source roots may contribute commands, environment configuration, plugins, toolchains, credentials,
and dependency installation. Runtime routing must preserve working-directory and environment
boundaries, avoid exposing source-specific credentials to unrelated actions, and distinguish
source-local synchronization from genuinely process-global setup.

## Open Questions

1. What syntax qualifies a project or target with a canonical source ID or local alias?
2. Should all-source execution require a flag, a target scope, or both?
3. How are source IDs and ambiguous project IDs rendered in existing JSON and DOT graph output?
4. When should recursive discovery be enabled, and how is it explicitly opted into?
5. How are plugin registries and toolchain configuration shared or isolated between sources?
6. Which setup actions are source-local, and which remain process-global?
7. How should graph cache composition work when only one source changes?
8. How should watchers and the daemon invalidate one source without rebuilding unrelated sources?
9. What explicit migration experience is provided when a durable source ID is renamed?
10. Should two checked-out versions of the same canonical workspace ever coexist in one graph?

## Success Criteria

Manymoons is complete when:

- Projects from independently versioned source roots coexist in one project graph.
- Project and task dependencies can cross source roots.
- Cross-source tasks execute through one action graph and the existing dispatcher.
- Affected, hashing, caching, hydration, daemon, and watcher behavior route through the correct
  source context.
- Single-source workspaces retain existing command and configuration semantics.
- Adding a source does not broaden an unqualified command without explicit user intent.
