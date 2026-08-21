use super::{HEADING_AFFECTED, HEADING_FILTERS};
use crate::app_options::AffectedOption;
use crate::queries::changed_files::*;
use crate::queries::projects::*;
use crate::session::{MoonSession, SessionResult};
use clap::Args;
use moon_affected::{AffectedTracker, AggregateAffectedTracker, DownstreamScope, UpstreamScope};
use starbase_utils::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::instrument;

#[derive(Args, Clone, Debug)]
pub struct QueryProjectsArgs {
    #[arg(help = "Filter projects using a query (takes precedence over options)")]
    query: Option<String>,

    // Affected
    #[arg(
        long,
        help = "Filter projects that are affected based on changed files",
        help_heading = HEADING_AFFECTED,
        group = "affected-args"
    )]
    affected: Option<Option<AffectedOption>>,

    #[arg(
        long,
        default_value_t,
        visible_alias = "dependents",
        help = "Include downstream dependents of queried projects",
        help_heading = HEADING_AFFECTED,
        requires = "affected-args",
    )]
    downstream: DownstreamScope,

    #[arg(
        long,
        default_value_t,
        visible_alias = "dependencies",
        help = "Include upstream dependencies of queried projects",
        help_heading = HEADING_AFFECTED,
        requires = "affected-args",
    )]
    upstream: UpstreamScope,

    // Filters
    #[arg(long, help = "Filter projects that match this alias", help_heading = HEADING_FILTERS)]
    alias: Option<String>,

    #[arg(long, help = "Filter projects that match this ID", help_heading = HEADING_FILTERS)]
    id: Option<String>,

    #[arg(long, help = "Filter projects of this programming language", help_heading = HEADING_FILTERS)]
    language: Option<String>,

    #[arg(long, help = "Filter projects of this layer", help_heading = HEADING_FILTERS)]
    layer: Option<String>,

    #[arg(long, help = "Filter projects of this tech stack", help_heading = HEADING_FILTERS)]
    stack: Option<String>,

    #[arg(long, help = "Filter projects that match this source path", help_heading = HEADING_FILTERS)]
    source: Option<String>,

    #[arg(long, help = "Filter projects that have the following tags", help_heading = HEADING_FILTERS)]
    tags: Option<String>,

    #[arg(long, help = "Filter projects that have the following tasks", help_heading = HEADING_FILTERS)]
    tasks: Option<String>,
}

#[instrument(skip(session))]
pub async fn projects(session: MoonSession, args: QueryProjectsArgs) -> SessionResult {
    let mut options = QueryProjectsOptions {
        alias: args.alias,
        affected: None,
        id: args.id,
        language: args.language,
        layer: args.layer,
        query: args.query,
        stack: args.stack,
        source: args.source,
        tags: args.tags,
        tasks: args.tasks,
    };

    // Filter down to affected projects only
    if let Some(by) = &args.affected {
        if session.sources.len() > 1 {
            let workspace_graph = session.get_aggregate_workspace_graph().await?;
            let observations = query_source_changed_files_for_affected(
                session.get_source_runtime_registry().await?.as_ref(),
                by.as_ref(),
            )
            .await?;
            let mut tracker = AggregateAffectedTracker::new(
                Arc::clone(&workspace_graph),
                observations.observations,
            )?;
            tracker.set_project_scopes(args.upstream, args.downstream);

            if session.workspace_config.experiments.async_affected_tracking {
                tracker.track_projects_async().await?;
            } else {
                tracker.track_projects()?;
            }

            let affected = tracker.build();
            let projects = query_projects_with_keys(&workspace_graph, &options)
                .await?
                .into_iter()
                .filter(|(key, _)| affected.is_project_affected(key))
                .collect::<BTreeMap<_, _>>();

            session.console.out.write_line(json::format(
                &QueryProjectsByKeyResult { projects, options },
                true,
            )?)?;

            return Ok(None);
        }

        let workspace_graph = session.get_workspace_graph().await?;
        let vcs = session.get_vcs_adapter().await?;
        let changed_files = query_changed_files_for_affected(&vcs, by.as_ref()).await?;

        let mut affected_tracker = AffectedTracker::new(workspace_graph.clone(), changed_files);
        affected_tracker.set_project_scopes(args.upstream, args.downstream);

        if session.workspace_config.experiments.async_affected_tracking {
            affected_tracker.track_projects_async().await?;
        } else {
            affected_tracker.track_projects()?;
        }

        options.affected = Some(affected_tracker.build());

        let local_projects = query_projects(&workspace_graph, &options).await?;
        let aggregate_graph = session.get_aggregate_workspace_graph().await?;
        let projects = local_projects
            .into_iter()
            .map(|project| {
                aggregate_graph
                    .get_project_with_tasks_by_key(&project.key())
                    .map(Arc::new)
            })
            .collect::<miette::Result<Vec<_>>>()?;

        session.console.out.write_line(json::format(
            &QueryProjectsResult { projects, options },
            true,
        )?)?;

        return Ok(None);
    }

    let workspace_graph = session.get_aggregate_workspace_graph().await?;
    let projects = query_projects_with_keys(&workspace_graph, &options).await?;

    if workspace_graph.sources.len() > 1 {
        session.console.out.write_line(json::format(
            &QueryProjectsByKeyResult {
                projects: projects.into_iter().collect::<BTreeMap<_, _>>(),
                options,
            },
            true,
        )?)?;
    } else {
        session.console.out.write_line(json::format(
            &QueryProjectsResult {
                projects: projects.into_iter().map(|(_, project)| project).collect(),
                options,
            },
            true,
        )?)?;
    }

    Ok(None)
}
