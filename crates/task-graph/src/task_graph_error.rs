use miette::Diagnostic;
use moon_common::{Style, Stylize};
use thiserror::Error;

#[derive(Error, Debug, Diagnostic)]
pub enum TaskGraphError {
    #[diagnostic(code(task_graph::unsupported_cross_source_edge))]
    #[error(
        "Source-local task graph contains a cross-source edge from {source_key} to {target_key}."
    )]
    UnsupportedCrossSourceEdge {
        source_key: String,
        target_key: String,
    },

    #[diagnostic(code(task_graph::would_cycle))]
    #[error(
        "Unable to create task graph, adding a relationship from {} to {} would introduce a cycle.",
        .source_target.style(Style::Id),
        .target_target.style(Style::Id),
    )]
    WouldCycle {
        source_target: String,
        target_target: String,
    },

    #[diagnostic(code(task_graph::unknown_target_in_project_deps))]
    #[error(
        "Invalid dependency {dep} for task {task}, no matching targets in project dependencies."
    )]
    UnknownDepTargetParentScope { dep: String, task: String },

    #[diagnostic(code(task_graph::dependency::no_allowed_failures))]
    #[error("Task {task} cannot depend on task {dep}, as it is allowed to fail.")]
    AllowFailureDepRequirement { dep: String, task: String },

    #[diagnostic(code(task_graph::dependency::run_in_ci_mismatch))]
    #[error("Task {task} cannot depend on task {dep}, as the dependency cannot run in CI.")]
    RunInCiDepRequirement { dep: String, task: String },

    #[diagnostic(code(task_graph::dependency::persistent_requirement))]
    #[error("Non-persistent task {task} cannot depend on persistent task {dep}.")]
    PersistentDepRequirement { dep: String, task: String },
}
