use crate::commands::graph::run_server;
use crate::session::{MoonSession, SessionResult};
use clap::Args;
use moon_common::Id;
use moon_project_graph::{GraphToDot, GraphToJson};
use moon_target::ProjectKey;
use std::str::FromStr;
use std::sync::Arc;
use tracing::instrument;

#[derive(Clone, Debug)]
enum ProjectGraphFocus {
    Primary(Id),
    Qualified(ProjectKey),
}

impl FromStr for ProjectGraphFocus {
    type Err = miette::Report;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.contains("::") {
            Ok(Self::Qualified(value.parse()?))
        } else {
            Ok(Self::Primary(Id::new(value)?))
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct ProjectGraphArgs {
    #[arg(help = "Project ID or qualified source::project to *only* graph")]
    id: Option<ProjectGraphFocus>,

    #[arg(long, help = "Include direct dependents of the focused project")]
    dependents: bool,

    #[arg(
        long,
        help = "The host address",
        env = "MOON_HOST",
        default_value = "127.0.0.1"
    )]
    host: String,

    #[arg(
        long,
        help = "The port to bind to",
        env = "MOON_PORT",
        default_value = "0"
    )]
    port: u16,

    #[arg(long, help = "Print the graph in DOT format")]
    dot: bool,

    #[arg(long, help = "Print the graph in JSON format")]
    json: bool,
}

#[instrument(skip(session))]
pub async fn project_graph(session: MoonSession, args: ProjectGraphArgs) -> SessionResult {
    let mut project_graph = session
        .get_aggregate_workspace_graph()
        .await?
        .projects
        .clone();

    if let Some(focus) = &args.id {
        project_graph = Arc::new(match focus {
            ProjectGraphFocus::Primary(id) => project_graph.focus_for(id, args.dependents)?,
            ProjectGraphFocus::Qualified(key) => {
                project_graph.focus_for_key(key, args.dependents)?
            }
        });
    }

    // Force expand all projects
    project_graph.get_all()?;

    if args.dot {
        session.console.out.write_line(project_graph.to_dot())?;

        return Ok(None);
    }

    if args.json {
        session
            .console
            .out
            .write_line(project_graph.to_json(true)?)?;

        return Ok(None);
    }

    run_server(
        "Project graph",
        project_graph.to_json(false)?,
        args.host,
        args.port,
    )
    .await?;

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primary_and_qualified_focus() {
        assert!(matches!(
            "app".parse::<ProjectGraphFocus>().unwrap(),
            ProjectGraphFocus::Primary(id) if id.as_str() == "app"
        ));
        assert!(matches!(
            "child::app".parse::<ProjectGraphFocus>().unwrap(),
            ProjectGraphFocus::Qualified(key) if key.to_string() == "child::app"
        ));
    }
}
