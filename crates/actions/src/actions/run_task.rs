use moon_action::{Action, ActionStatus, RunTaskNode};
use moon_action_context::ActionContext;
use moon_app_context::{AppContext, SourceRuntimeRegistry};
use moon_common::color;
use moon_daemon_client::DaemonClient;
use moon_task_graph::TaskGraph;
use moon_task_runner::TaskRunner;
use moon_workspace_graph::WorkspaceGraph;
use std::sync::Arc;
use tracing::{instrument, warn};

#[derive(Clone)]
pub struct TaskRunnerContext {
    pub source_runtime_registry: Arc<SourceRuntimeRegistry>,
    pub task_graph: Arc<TaskGraph>,
}

#[instrument(skip(action, action_context, app_context, runner_context, workspace_graph))]
pub async fn run_task(
    action: &mut Action,
    action_context: Arc<ActionContext>,
    app_context: Arc<AppContext>,
    workspace_graph: Arc<WorkspaceGraph>,
    runner_context: Option<TaskRunnerContext>,
    daemon_client: Option<DaemonClient>,
    node: &RunTaskNode,
) -> miette::Result<ActionStatus> {
    let project = workspace_graph.get_project_by_key(node.key.project_key())?;
    let task = workspace_graph.get_task_by_key(&node.key)?;

    // Must be set before running the task in case it fails and
    // and error is bubbled up the stack
    action.allow_failure = task.options.allow_failure;

    let mut runner = if let Some(runner_context) = runner_context {
        TaskRunner::new_with_hashing_context_for_invocation(
            &app_context,
            &project,
            &task,
            node.invocation_key(),
            daemon_client,
            runner_context.task_graph,
            runner_context.source_runtime_registry,
        )?
    } else {
        TaskRunner::new_for_invocation(
            &app_context,
            &project,
            &task,
            node.invocation_key(),
            daemon_client,
        )?
    };
    let result = runner.run(&action_context, &action.node).await?;

    action.flaky = result.operations.is_flaky();
    action.status = result.operations.get_final_status();
    action.operations = result.operations;

    if action.has_failed() && action.allow_failure {
        warn!(
            "Task {} has failed, but is marked to allow failures, continuing pipeline",
            color::label(&task.target),
        );
    }

    match result.error {
        Some(error) => Err(error),
        None => Ok(action.status),
    }
}
