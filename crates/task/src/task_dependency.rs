use crate::{TaskOptionRunInCI, TaskOptions};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskDependencyValidationError {
    AllowFailure,
    RunInCi,
    Persistent,
}

pub fn validate_task_dependency(
    task: &TaskOptions,
    dependency: &TaskOptions,
) -> Result<(), TaskDependencyValidationError> {
    if dependency.allow_failure {
        return Err(TaskDependencyValidationError::AllowFailure);
    }

    if !dependency.run_in_ci.is_enabled()
        && task.run_in_ci.is_enabled()
        && dependency.run_in_ci != TaskOptionRunInCI::Skip
        && task.run_in_ci != TaskOptionRunInCI::Skip
    {
        return Err(TaskDependencyValidationError::RunInCi);
    }

    if dependency.persistent && !task.persistent {
        return Err(TaskDependencyValidationError::Persistent);
    }

    Ok(())
}
