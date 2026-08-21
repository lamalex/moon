use miette::Diagnostic;
use moon_common::{SourceRootId, Style, Stylize};
use moon_task::Target;
use thiserror::Error;

#[derive(Error, Debug, Diagnostic)]
pub enum TaskHasherError {
    #[diagnostic(code(task_hasher::source_mismatch))]
    #[error(
        "Cannot hash inputs for task {} from source {task_source} using runtime source {runtime_source}.",
        .target.style(Style::Id),
    )]
    SourceMismatch {
        runtime_source: SourceRootId,
        task_source: SourceRootId,
        target: Target,
    },

    #[diagnostic(code(task_hasher::missing_input_file))]
    #[error(
        "Input file {} for task {} does not exist, but is required for hashing as the {} input parameter is configured.",
        .path.style(Style::File),
        .target.style(Style::Id),
        "optional".style(Style::Property),
    )]
    MissingInputFile { path: String, target: Target },
}
