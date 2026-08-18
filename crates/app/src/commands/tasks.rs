use crate::session::{MoonSession, SessionResult};
use clap::Args;
use iocraft::prelude::{Size, element};
use moon_common::Id;
use moon_console::ui::*;
use moon_target::TaskKey;
use starbase_utils::json;
use std::collections::BTreeMap;
use tracing::instrument;

#[derive(Args, Clone, Debug)]
pub struct TasksArgs {
    #[arg(help = "Filter tasks to a specific project")]
    project: Option<Id>,

    #[arg(long, help = "Print in JSON format")]
    json: bool,
}

#[instrument(skip(session))]
pub async fn tasks(session: MoonSession, args: TasksArgs) -> SessionResult {
    let aggregate = args.project.is_none();
    let workspace_graph = if aggregate {
        session.get_aggregate_workspace_graph().await?
    } else {
        session.get_workspace_graph().await?
    };

    let mut tasks: Vec<(Option<TaskKey>, _)> = if let Some(project_id) = &args.project {
        let tasks = workspace_graph.get_tasks_from_project(project_id)?;

        if tasks.is_empty() {
            session.console.render(element! {
                Container {
                    Notice(variant: Variant::Info) {
                        StyledText(
                            content: format!("There are no tasks for the project <id>{project_id}</id>")
                        )
                    }
                }
            })?;

            return Ok(None);
        }

        tasks.into_iter().map(|task| (None, task)).collect()
    } else {
        workspace_graph
            .get_all_tasks_with_keys()?
            .into_iter()
            .map(|(key, task)| (Some(key), task))
            .collect()
    };

    tasks.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(a), Some(b)) => a.cmp(b),
        _ => a.1.target.cmp(&b.1.target),
    });

    if args.json {
        if aggregate && workspace_graph.sources.len() > 1 {
            let tasks = tasks
                .into_iter()
                .filter_map(|(key, task)| key.map(|key| (key, task)))
                .collect::<BTreeMap<_, _>>();
            session
                .console
                .out
                .write_line(json::format(&tasks, true)?)?;
        } else {
            let tasks = tasks.into_iter().map(|(_, task)| task).collect::<Vec<_>>();
            session
                .console
                .out
                .write_line(json::format(&tasks, true)?)?;
        }

        return Ok(None);
    }

    if tasks.is_empty() {
        session.console.render(element! {
            Container {
                Notice(variant: Variant::Info) {
                    StyledText(content: "No tasks exist. Have any been configured?")
                }
            }
        })?;

        return Ok(None);
    }

    let id_width = tasks
        .iter()
        .fold(0, |acc, (key, task)| {
            acc.max(
                key.as_ref()
                    .filter(|_| workspace_graph.sources.len() > 1)
                    .map(|key| key.to_string().len())
                    .unwrap_or_else(|| task.target.as_str().len()),
            )
        })
        .max(3);
    let command_width = tasks
        .iter()
        .fold(0, |acc, (_, task)| acc.max(task.command.len()))
        .max(3);

    session.console.render(element! {
        Container {
            Table(
                headers: vec![
                    TableHeader::new("Task", Size::Length((id_width + 5).max(10) as u32)),
                    TableHeader::new("Command", Size::Length((command_width + 5) as u32)),
                    TableHeader::new("Type", Size::Length(10)).hide_below(130),
                    TableHeader::new("Preset", Size::Length(10)).hide_below(160),
                    TableHeader::new("Toolchains", Size::Length(40)),
                    TableHeader::new("Description", Size::Auto).hide_below(100),
                ]
            ) {
                #(tasks.into_iter().enumerate().map(|(i, (key, task))| {
                    element! {
                        TableRow(row: i as i32) {
                            TableCol(col: 0) {
                                StyledText(
                                    content: key
                                        .filter(|_| workspace_graph.sources.len() > 1)
                                        .map(|key| key.to_string())
                                        .unwrap_or_else(|| task.target.to_string()),
                                    style: Style::Id
                                )
                            }
                            TableCol(col: 1) {
                                StyledText(
                                    content: if task.script.is_some() {
                                        "(script)"
                                    } else {
                                        &task.command
                                    },
                                    style: Style::Shell
                                )
                            }
                            TableCol(col: 2) {
                                StyledText(
                                    content: task.type_of.to_string(),
                                )
                            }
                            TableCol(col: 3) {
                                #(task.preset.as_ref().map(|preset| {
                                    element! {
                                        StyledText(
                                            content: preset.to_string(),
                                        )
                                    }
                                }))
                            }
                            TableCol(col: 4) {
                                StyledText(
                                    content: task.toolchains.join(", "),
                                )
                            }
                            TableCol(col: 5) {
                                StyledText(
                                    content: task.description.as_deref().unwrap_or(""),
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
