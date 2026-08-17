use moon_common::{Id, SourceRootId};
use moon_target::{ProjectKey, TaskKey};

#[test]
fn project_keys_include_the_source_root() {
    let first = ProjectKey::new(SourceRootId::new("first").unwrap(), Id::raw("app")).unwrap();
    let second = ProjectKey::new(SourceRootId::new("second").unwrap(), Id::raw("app")).unwrap();

    assert_ne!(first, second);
    assert_eq!(first.to_string(), "first::app");
    assert_eq!(first.to_string().parse::<ProjectKey>().unwrap(), first);
}

#[test]
fn task_keys_include_the_source_root() {
    let key = TaskKey::new(
        ProjectKey::new(SourceRootId::new("frontend").unwrap(), Id::raw("app")).unwrap(),
        Id::raw("build"),
    )
    .unwrap();

    assert_eq!(key.to_string(), "frontend::app:build");
    assert_eq!(key.to_string().parse::<TaskKey>().unwrap(), key);
}

#[test]
fn primary_keys_have_stable_qualified_values() {
    assert_eq!(
        ProjectKey::primary(Id::raw("app")).unwrap().to_string(),
        "workspace::app"
    );
    assert_eq!(
        TaskKey::primary(Id::raw("app"), Id::raw("build"))
            .unwrap()
            .to_string(),
        "workspace::app:build"
    );
}

#[test]
fn keys_serialize_as_strings() {
    let project = ProjectKey::new(SourceRootId::new("frontend").unwrap(), Id::raw("app")).unwrap();
    let task = TaskKey::new(project.clone(), Id::raw("build")).unwrap();

    assert_eq!(
        serde_json::to_string(&project).unwrap(),
        "\"frontend::app\""
    );
    assert_eq!(
        serde_json::from_str::<ProjectKey>("\"frontend::app\"").unwrap(),
        project
    );
    assert_eq!(
        serde_json::to_string(&task).unwrap(),
        "\"frontend::app:build\""
    );
    assert_eq!(
        serde_json::from_str::<TaskKey>("\"frontend::app:build\"").unwrap(),
        task
    );
}

#[test]
fn rejects_unchecked_ids_that_break_qualified_serialization() {
    assert!(ProjectKey::new(SourceRootId::primary(), Id::raw("app:invalid")).is_err());
    assert!(TaskKey::primary(Id::raw("app"), Id::raw("build:invalid")).is_err());
}
