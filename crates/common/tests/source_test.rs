use moon_common::{SourcePathBuf, SourceRegistry, SourceRootId};

#[test]
fn defaults_to_a_stable_primary_id() {
    assert_eq!(SourceRootId::primary().as_str(), "workspace");
    assert_eq!(SourceRootId::default(), SourceRootId::primary());
}

#[test]
fn resolves_and_qualifies_paths() {
    let root = std::env::current_dir().unwrap().join("workspace");
    let mut sources = SourceRegistry::single(root.clone());
    let nested_id = SourceRootId::new("nested").unwrap();
    let nested_root = root.join("nested");

    sources
        .register(nested_id.clone(), nested_root.clone())
        .unwrap();

    let qualified = sources
        .qualify(&nested_root.join("project/file.txt"))
        .unwrap();

    assert_eq!(qualified.source, nested_id);
    assert_eq!(qualified.path.as_str(), "project/file.txt");
    assert_eq!(
        sources.resolve(&qualified).unwrap(),
        nested_root.join("project/file.txt")
    );
}

#[test]
fn rejects_unknown_and_duplicate_roots() {
    let root = std::env::current_dir().unwrap().join("workspace");
    let mut sources = SourceRegistry::single(root);

    assert!(
        sources
            .register(SourceRootId::primary(), "other".into())
            .is_err()
    );
    assert!(
        sources
            .resolve(&SourcePathBuf::new(
                SourceRootId::new("unknown").unwrap(),
                "file.txt",
            ))
            .is_err()
    );
}

#[test]
fn rejects_duplicate_normalized_root_paths() {
    let root = std::env::current_dir().unwrap().join("workspace");
    let mut sources = SourceRegistry::single(root.clone());

    assert!(
        sources
            .register(SourceRootId::new("other").unwrap(), root.join("."))
            .is_err()
    );
}

#[test]
fn source_paths_round_trip_through_strings() {
    let path = SourcePathBuf::new(SourceRootId::new("frontend").unwrap(), "packages/app");
    let value = path.to_string();

    assert_eq!(value, "frontend::packages/app");
    assert_eq!(value.parse::<SourcePathBuf>().unwrap(), path);
    assert_eq!(
        serde_json::from_str::<SourcePathBuf>(&serde_json::to_string(&path).unwrap()).unwrap(),
        path
    );
}

#[test]
fn rejects_paths_that_escape_their_source_root() {
    let root = std::env::current_dir().unwrap().join("workspace");
    let sources = SourceRegistry::single(root);

    assert!(
        sources
            .resolve(&SourcePathBuf::primary("../file.txt"))
            .is_err()
    );
}

#[test]
fn default_registry_resolves_relative_paths() {
    let sources = SourceRegistry::default();
    let path = SourcePathBuf::primary("file.txt");

    assert_eq!(
        sources.resolve(&path).unwrap(),
        std::path::Path::new("file.txt")
    );
}
