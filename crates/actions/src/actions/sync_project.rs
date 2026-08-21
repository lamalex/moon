use crate::plugins::*;
use crate::utils::should_skip_action_matching;
use moon_action::{Action, ActionStatus, SyncProjectNode};
use moon_action_context::ActionContext;
use moon_app_context::AppContext;
use moon_common::{color, is_ci};
use moon_pdk_api::SyncProjectInput;
use moon_workspace_graph::WorkspaceGraph;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

#[instrument(skip(action, _action_context, app_context, workspace_graph))]
pub async fn sync_project(
    action: &mut Action,
    _action_context: Arc<ActionContext>,
    app_context: Arc<AppContext>,
    workspace_graph: Arc<WorkspaceGraph>,
    node: &SyncProjectNode,
) -> miette::Result<ActionStatus> {
    let project_key = &node.project_key;
    let project_id = project_key.project_id();

    if project_key.source_id() != &app_context.source_id {
        return Err(miette::miette!(
            "Sync project action for source {} cannot run in source {}.",
            project_key.source_id(),
            app_context.source_id
        ));
    }

    // Include tasks for snapshot!
    let project = workspace_graph.get_project_with_tasks_by_key(project_key)?;

    // Create a snapshot for tasks to reference
    app_context
        .cache_engine
        .state
        .save_project_snapshot(project_id, &project)?;

    // Skip action if requested too
    if let Some(value) = should_skip_action_matching("MOON_SKIP_SYNC_PROJECT", project_id) {
        debug!(
            env = value,
            "Skipping project {} sync because {} is set",
            color::id(project_id),
            color::symbol("MOON_SKIP_SYNC_PROJECT")
        );

        return Ok(ActionStatus::Skipped);
    }

    debug!("Syncing project {}", color::id(project_id));

    // Lock the project to avoid collisions
    let _lock =
        app_context
            .cache_engine
            .create_lock(format!("{}-{}", action.get_prefix(), project_id))?;

    // Collect all project dependencies so we can pass them along
    let mut dependency_fragments = vec![];

    for (dependency_key, dependency_scope) in workspace_graph
        .projects
        .direct_dependencies_with_scopes(&project.key())?
    {
        let dep_project = workspace_graph.get_project_by_key(&dependency_key)?;

        dependency_fragments.push({
            let mut fragment = dep_project.to_fragment();
            fragment.dependency_scope = Some(dependency_scope);
            fragment
        });
    }

    // Sync the projects and return true if any files have been mutated
    let registry = &app_context.toolchain_registry;

    for sync_result in registry
        .sync_project_many(project.get_enabled_toolchains(), |toolchain| {
            SyncProjectInput {
                context: registry.create_context(),
                project_dependencies: dependency_fragments.clone(),
                project: project.to_fragment(),
                toolchain_config: registry.create_merged_config(&toolchain.id, &project.config),
                toolchain_workspace_config: registry.create_config(&toolchain.id),
                ..Default::default()
            }
        })
        .await?
    {
        action
            .operations
            .push(finalize_sync_operation(sync_result)?);
    }

    let registry = &app_context.extension_registry;

    for sync_result in registry
        .sync_project_all(|extension| SyncProjectInput {
            context: registry.create_context(),
            project_dependencies: dependency_fragments.clone(),
            project: project.to_fragment(),
            extension_config: registry.create_config(&extension.id),
            ..Default::default()
        })
        .await?
    {
        action
            .operations
            .push(finalize_sync_operation(sync_result)?);
    }

    // If files have been modified in CI, we should update the status to warning,
    // as these modifications should be committed to the repo!
    let changed_files = action.get_changed_files();

    if !changed_files.is_empty() && is_ci() {
        warn!(
            project_id = project.id.as_str(),
            changed_files = ?changed_files,
            "Files were modified during project sync that should be committed to the repository"
        );

        return Ok(ActionStatus::Invalid);
    }

    Ok(ActionStatus::Passed)
}
