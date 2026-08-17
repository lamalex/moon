use crate::SourceVcsState;
use crate::session::{MoonSession, SessionResult};
use clap::Args;
use iocraft::prelude::{Size, element};
use moon_console::ui::*;
use serde::Serialize;
use starbase_utils::json;
use tracing::instrument;

#[derive(Args, Clone, Debug)]
pub struct SourcesArgs {
    #[arg(long, help = "Print in JSON format")]
    json: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderDiagnostic {
    message: Option<String>,
    status: String,
    version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceDiagnostic {
    aliases: Vec<String>,
    failures: Vec<crate::SourceLoadFailure>,
    id: Option<String>,
    path: String,
    primary: bool,
    provider: ProviderDiagnostic,
}

#[instrument(skip(session))]
pub async fn sources(session: MoonSession, args: SourcesArgs) -> SessionResult {
    let primary_id = session.sources.primary_id();
    let mut diagnostics = Vec::with_capacity(session.source_workspaces.len());

    for (id, workspace) in session.source_workspaces.iter() {
        let mut aliases = session
            .source_aliases
            .iter()
            .filter(|(_, source_id)| *source_id == id)
            .map(|(alias, _)| alias.as_str().to_owned())
            .collect::<Vec<_>>();
        aliases.sort();

        let (failures, provider) = if id == primary_id {
            let provider = match session.get_vcs_adapter().await {
                Ok(vcs) => ProviderDiagnostic {
                    message: None,
                    status: "ready".into(),
                    version: vcs
                        .get_version()
                        .await
                        .ok()
                        .map(|version| version.to_string()),
                },
                Err(error) => ProviderDiagnostic {
                    message: Some(error.to_string()),
                    status: "failed".into(),
                    version: None,
                },
            };

            (Vec::new(), provider)
        } else {
            let context = &session.source_contexts[id];
            let provider = match context.vcs_state() {
                Some(SourceVcsState::Ready(vcs)) => ProviderDiagnostic {
                    message: None,
                    status: "ready".into(),
                    version: vcs
                        .get_version()
                        .await
                        .ok()
                        .map(|version| version.to_string()),
                },
                Some(SourceVcsState::Failed(error)) => ProviderDiagnostic {
                    message: Some(error.to_string()),
                    status: "failed".into(),
                    version: None,
                },
                None => ProviderDiagnostic {
                    message: None,
                    status: "pending".into(),
                    version: None,
                },
            };

            (context.failures.as_ref().clone(), provider)
        };

        diagnostics.push(SourceDiagnostic {
            aliases,
            failures,
            id: Some(id.to_string()),
            path: workspace.root.display().to_string(),
            primary: id == primary_id,
            provider,
        });
    }

    for failure in session.source_discovery_failures.iter() {
        diagnostics.push(SourceDiagnostic {
            aliases: vec![failure.alias.to_string()],
            failures: vec![crate::SourceLoadFailure {
                message: failure.message.clone(),
                stage: failure.stage.clone(),
            }],
            id: None,
            path: failure.root.display().to_string(),
            primary: false,
            provider: ProviderDiagnostic {
                message: None,
                status: "unavailable".into(),
                version: None,
            },
        });
    }

    diagnostics.sort_by_key(|source| {
        source
            .id
            .clone()
            .or_else(|| source.aliases.first().cloned())
            .unwrap_or_default()
    });

    if args.json {
        session
            .console
            .out
            .write_line(json::format(&diagnostics, true)?)?;

        return Ok(None);
    }

    let id_width = diagnostics
        .iter()
        .map(|source| source.id.as_deref().unwrap_or("(unidentified)").len())
        .max()
        .unwrap_or(6)
        .max(6);
    let alias_width = diagnostics
        .iter()
        .map(|source| source.aliases.join(", ").len())
        .max()
        .unwrap_or(7)
        .max(7);

    session.console.render(element! {
        Container {
            Table(
                headers: vec![
                    TableHeader::new("Source", Size::Length((id_width + 5) as u32)),
                    TableHeader::new("Aliases", Size::Length((alias_width + 5) as u32)),
                    TableHeader::new("Provider", Size::Length(14)),
                    TableHeader::new("Path", Size::Length(40)),
                    TableHeader::new("Diagnostics", Size::Auto),
                ]
            ) {
                #(diagnostics.into_iter().enumerate().map(|(index, source)| {
                    let provider = source.provider.status;
                    let mut details = source
                        .failures
                        .iter()
                        .map(|failure| format!("{}: {}", failure.stage, failure.message))
                        .collect::<Vec<_>>();

                    if let Some(message) = source.provider.message {
                        details.push(format!("provider: {message}"));
                    }

                    element! {
                        TableRow(row: index as i32) {
                            TableCol(col: 0) {
                                StyledText(
                                    content: source.id.unwrap_or_else(|| "(unidentified)".into()),
                                    style: Style::Id
                                )
                            }
                            TableCol(col: 1) {
                                StyledText(content: source.aliases.join(", "))
                            }
                            TableCol(col: 2) {
                                StyledText(content: provider)
                            }
                            TableCol(col: 3) {
                                StyledText(content: source.path, style: Style::File)
                            }
                            TableCol(col: 4) {
                                StyledText(content: details.join("; "))
                            }
                        }
                    }
                }))
            }
        }
    })?;

    Ok(None)
}
