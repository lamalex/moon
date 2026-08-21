use moon_action::{Action, ActionNode, ActionStatus, Operation, RunTaskNode};
use moon_common::{Id, SourceRootId};
use moon_target::{ProjectKey, Target, TaskKey};

fn task_op(exit_code: Option<i32>, status: ActionStatus) -> Operation {
    let mut op = Operation::task_execution("cmd");

    if let Some(output) = op.get_exec_output_mut() {
        output.exit_code = exit_code;
    }

    op.finish(status);
    op
}

#[test]
fn run_task_identity_is_source_qualified() {
    let target = Target::parse("app:build").unwrap();
    let first_key = TaskKey::new(
        ProjectKey::new(SourceRootId::new("first").unwrap(), Id::raw("app")).unwrap(),
        Id::raw("build"),
    )
    .unwrap();
    let second_key = TaskKey::new(
        ProjectKey::new(SourceRootId::new("second").unwrap(), Id::raw("app")).unwrap(),
        Id::raw("build"),
    )
    .unwrap();

    let first = ActionNode::run_task(RunTaskNode::new_with_key(first_key, target.clone()));
    let second = ActionNode::run_task(RunTaskNode::new_with_key(second_key, target));

    assert_ne!(first, second);
    assert_ne!(first.get_id(), second.get_id());
    assert_eq!(first.label(), second.label());
}

mod get_exit_code {
    use super::*;

    #[test]
    fn none_when_no_operations() {
        let action = Action::default();

        assert_eq!(action.get_exit_code(), None);
    }

    #[test]
    fn none_when_no_execution_operation() {
        let mut action = Action::default();
        action.operations.push(Operation::hash_generation());

        assert_eq!(action.get_exit_code(), None);
    }

    #[test]
    fn returns_the_execution_exit_code() {
        let mut action = Action::default();
        action
            .operations
            .push(task_op(Some(80), ActionStatus::Failed));

        assert_eq!(action.get_exit_code(), Some(80));
    }

    #[test]
    fn returns_zero_for_passing_execution() {
        let mut action = Action::default();
        action
            .operations
            .push(task_op(Some(0), ActionStatus::Passed));

        assert_eq!(action.get_exit_code(), Some(0));
    }

    #[test]
    fn returns_the_last_execution_code_when_retried() {
        let mut action = Action::default();
        action
            .operations
            .push(task_op(Some(1), ActionStatus::Failed));
        action
            .operations
            .push(task_op(Some(70), ActionStatus::Failed));

        assert_eq!(action.get_exit_code(), Some(70));
    }

    #[test]
    fn returns_negative_one_for_injected_aborts() {
        let mut action = Action::default();
        action
            .operations
            .push(task_op(Some(-1), ActionStatus::Aborted));

        assert_eq!(action.get_exit_code(), Some(-1));
    }

    // Signal deaths and timeouts produce an execution operation
    // without a captured code
    #[test]
    fn none_when_execution_has_no_exit_code() {
        let mut action = Action::default();
        action.operations.push(task_op(None, ActionStatus::Failed));

        assert_eq!(action.get_exit_code(), None);
    }

    #[test]
    fn ignores_operations_after_the_last_execution() {
        let mut action = Action::default();
        action
            .operations
            .push(task_op(Some(6), ActionStatus::Failed));
        action.operations.push(Operation::hash_generation());

        assert_eq!(action.get_exit_code(), Some(6));
    }
}
