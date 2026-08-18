use crate::session::{MoonSession, SessionResult};
use clap::Args;
use iocraft::prelude::{Size, element};
use moon_console::ui::*;
use starbase_utils::json;
use std::collections::BTreeMap;
use tracing::instrument;

#[derive(Args, Clone, Debug)]
pub struct ProjectsArgs {
    #[arg(long, help = "Print in JSON format")]
    json: bool,
}

#[instrument(skip(session))]
pub async fn projects(session: MoonSession, args: ProjectsArgs) -> SessionResult {
    let mut projects = session
        .get_aggregate_workspace_graph()
        .await?
        .get_projects()?;

    projects.sort_by_key(|project| project.key());

    if args.json {
        session
            .console
            .out
            .write_line(json::format(&projects, true)?)?;

        return Ok(None);
    }

    if projects.is_empty() {
        session.console.render(element! {
            Container {
                Notice(variant: Variant::Info) {
                    StyledText(content: "No projects exist. Have any been configured?")
                }
            }
        })?;

        return Ok(None);
    }

    let mut id_counts = BTreeMap::new();

    for project in &projects {
        *id_counts.entry(project.id.clone()).or_insert(0) += 1;
    }

    let projects = projects
        .into_iter()
        .map(|project| {
            let display_id = if id_counts[&project.id] > 1 {
                project.key().to_string()
            } else {
                project.id.to_string()
            };

            (display_id, project)
        })
        .collect::<Vec<_>>();

    let id_width = projects
        .iter()
        .fold(0, |acc, (id, _)| acc.max(id.len()))
        .max(3);
    let source_width = projects
        .iter()
        .fold(0, |acc, (_, project)| {
            acc.max(project.source.as_str().len())
        })
        .max(3);

    session.console.render(element! {
        Container {
            Table(
                headers: vec![
                    TableHeader::new("Project", Size::Length((id_width + 5).max(10) as u32)),
                    TableHeader::new("Source", Size::Length((source_width + 5) as u32)),
                    TableHeader::new("Stack", Size::Length(16)).hide_below(160),
                    TableHeader::new("Layer", Size::Length(16)).hide_below(130),
                    TableHeader::new("Toolchains", Size::Length(40)),
                    TableHeader::new("Description", Size::Auto).hide_below(100),
                ]
            ) {
                #(projects.into_iter().enumerate().map(|(i, (id, project))| {
                    element! {
                        TableRow(row: i as i32) {
                            TableCol(col: 0) {
                                StyledText(
                                    content: id,
                                    style: Style::Id
                                )
                            }
                            TableCol(col: 1) {
                                StyledText(
                                    content: project.source.to_string(),
                                    style: Style::File
                                )
                            }
                            TableCol(col: 2) {
                                StyledText(
                                    content: project.stack.to_string(),
                                )
                            }
                            TableCol(col: 3) {
                                StyledText(
                                    content: project.layer.to_string(),
                                )
                            }
                            TableCol(col: 4) {
                                StyledText(
                                    content: project.toolchains.join(", "),
                                )
                            }
                            TableCol(col: 5) {
                                StyledText(
                                    content: project
                                        .config
                                        .project
                                        .as_ref()
                                        .and_then(|cfg| cfg.description.as_deref())
                                        .unwrap_or(""),
                                )
                            }
                        }
                    }
                }))
            }
        }
    })?;

    Ok(None)
}
