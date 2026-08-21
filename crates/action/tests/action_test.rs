use moon_action::{
    Action, ActionNode, ActionStatus, InstallDependenciesNode, Operation, RunTaskNode,
    SetupEnvironmentNode, SetupToolchainNode, SyncProjectNode,
};
use moon_common::{Id, SourceRootId, path::WorkspaceRelativePathBuf};
use moon_target::{ProjectKey, Target, TaskKey};
use moon_toolchain::{ToolchainSpec, VersionSpec};
use rustc_hash::FxHashSet;

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
    assert_eq!(
        first.source_id(),
        Some(&SourceRootId::new("first").unwrap())
    );
    assert_eq!(
        second.source_id(),
        Some(&SourceRootId::new("second").unwrap())
    );
}

#[test]
fn run_task_variants_share_scheduler_identity_but_not_action_identity() {
    let target = Target::parse("app:build").unwrap();
    let key = TaskKey::primary(Id::raw("app"), Id::raw("build")).unwrap();
    let mut first = RunTaskNode::new_with_key(key.clone(), target.clone());
    first.args.push("--mode=a".into());
    let mut second = RunTaskNode::new_with_key(key, target);
    second.env.insert("MODE".into(), Some("b".into()));
    let first = ActionNode::run_task(first);
    let second = ActionNode::run_task(second);

    assert_ne!(first, second);
    assert_eq!(
        FxHashSet::from_iter([first.clone(), second.clone()]).len(),
        2
    );
    assert_eq!(first.get_id(), second.get_id());
}

#[test]
fn run_task_scheduler_identity_retains_execution_semantics() {
    let target = Target::parse("app:build").unwrap();
    let key = TaskKey::primary(Id::raw("app"), Id::raw("build")).unwrap();
    let standard = ActionNode::run_task(RunTaskNode::new_with_key(key.clone(), target.clone()));
    let mut persistent = RunTaskNode::new_with_key(key.clone(), target.clone());
    persistent.persistent = true;
    let persistent = ActionNode::run_task(persistent);
    let mut interactive = RunTaskNode::new_with_key(key, target);
    interactive.interactive = true;
    let interactive = ActionNode::run_task(interactive);

    assert_ne!(standard.get_id(), persistent.get_id());
    assert_ne!(standard.get_id(), interactive.get_id());
    assert_ne!(persistent.get_id(), interactive.get_id());
}

fn assert_source_qualified(create: impl Fn(SourceRootId) -> ActionNode) {
    let first_source = SourceRootId::new("first").unwrap();
    let second_source = SourceRootId::new("second").unwrap();
    let first = create(first_source.clone());
    let second = create(second_source.clone());

    assert_eq!(first.label(), second.label());
    assert_eq!(first.source_id(), Some(&first_source));
    assert_eq!(second.source_id(), Some(&second_source));
    assert_ne!(first, second);
    assert_eq!(FxHashSet::from_iter([first, second]).len(), 2);
}

#[test]
fn root_sensitive_action_identity_is_source_qualified() {
    assert_source_qualified(|source_id| {
        ActionNode::install_dependencies(InstallDependenciesNode {
            members: None,
            project_key: None,
            root: WorkspaceRelativePathBuf::new(),
            source_id,
            toolchain_id: Id::raw("node"),
        })
    });

    assert_source_qualified(|source_id| {
        ActionNode::setup_environment(SetupEnvironmentNode {
            project_key: None,
            root: WorkspaceRelativePathBuf::new(),
            source_id,
            toolchain_id: Id::raw("node"),
        })
    });

    assert_source_qualified(|source_id| {
        ActionNode::setup_proto(source_id, VersionSpec::parse("1.2.3").unwrap())
    });

    assert_source_qualified(|source_id| {
        ActionNode::setup_toolchain(SetupToolchainNode {
            source_id,
            toolchain: ToolchainSpec::new_global(Id::raw("node")),
        })
    });

    assert_source_qualified(|source_id| {
        ActionNode::sync_project(SyncProjectNode {
            project_key: ProjectKey::new(source_id, Id::raw("app")).unwrap(),
        })
    });

    assert_source_qualified(ActionNode::sync_workspace);
}

#[test]
fn none_has_no_source() {
    assert_eq!(ActionNode::None.source_id(), None);
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
