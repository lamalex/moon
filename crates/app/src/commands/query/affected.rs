use crate::app_options::AffectedOption;
use crate::queries::changed_files::*;
use crate::session::{MoonSession, SessionResult};
use clap::Args;
use moon_affected::{AffectedTracker, AggregateAffectedTracker, DownstreamScope, UpstreamScope};
use starbase_utils::json;
use tracing::instrument;

#[derive(Args, Clone, Debug)]
pub struct QueryAffectedArgs {
    #[arg(help = "Conditions in which to track affected")]
    by: Option<AffectedOption>,

    #[arg(
        long,
        default_value_t,
        visible_alias = "dependents",
        help = "Include downstream dependents"
    )]
    downstream: DownstreamScope,

    #[arg(
        long,
        default_value_t,
        visible_alias = "dependencies",
        help = "Include upstream dependencies"
    )]
    upstream: UpstreamScope,
}

#[instrument(skip(session))]
pub async fn affected(session: MoonSession, args: QueryAffectedArgs) -> SessionResult {
    if session.sources.len() > 1 {
        let observations = query_source_changed_files_for_affected(
            session.get_source_runtime_registry().await?.as_ref(),
            args.by.as_ref(),
        )
        .await?;
        let mut tracker = AggregateAffectedTracker::new(
            session.get_aggregate_workspace_graph().await?,
            observations.observations,
        )?;
        tracker.set_scopes(args.upstream, args.downstream);

        if session.workspace_config.experiments.async_affected_tracking {
            tracker.track_projects_async().await?;
            tracker.track_tasks_async().await?;
        } else {
            tracker.track_projects()?;
            tracker.track_tasks()?;
        }

        session
            .console
            .out
            .write_line(json::format(&tracker.build(), true)?)?;

        return Ok(None);
    }

    let vcs = session.get_vcs_adapter().await?;

    let mut affected_tracker = AffectedTracker::new(
        session.get_workspace_graph().await?,
        query_changed_files_for_affected(&vcs, args.by.as_ref()).await?,
    );
    affected_tracker.set_scopes(args.upstream, args.downstream);

    if session.workspace_config.experiments.async_affected_tracking {
        affected_tracker.track_projects_async().await?;
        affected_tracker.track_tasks_async().await?;
    } else {
        affected_tracker.track_projects()?;
        affected_tracker.track_tasks()?;
    }

    let affected = affected_tracker.build();

    session
        .console
        .out
        .write_line(json::format(&affected, true)?)?;

    Ok(None)
}
