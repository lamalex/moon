use bazel_remote_apis::build::bazel::remote::execution::v2::{ActionResult, Digest, OutputFile};
use moon_daemon_proto::{
    ArchiveTaskOutputsRequest, HashFilesRequest, HydrateTaskOutputsRequest, ManifestDigestSource,
    PROTOCOL_VERSION,
};
use prost::Message;

fn hex(seed: char) -> String {
    std::iter::repeat_n(seed, 64).collect()
}

fn create_request() -> ArchiveTaskOutputsRequest {
    ArchiveTaskOutputsRequest {
        task_target: "app:build".into(),
        digest: Some(Digest {
            hash: hex('a'),
            size_bytes: 12,
        }),
        manifest: Some(ActionResult {
            exit_code: 3,
            output_files: vec![OutputFile {
                path: "out/a.txt".into(),
                digest: Some(Digest {
                    hash: hex('b'),
                    size_bytes: 5,
                }),
                is_executable: true,
                ..Default::default()
            }],
            stdout_raw: b"built".to_vec(),
            stderr_raw: b"warned".to_vec(),
            ..Default::default()
        }),
        include_local: true,
        include_remote: false,
        source_id: "child".into(),
        task_key: "child::app:build".into(),
        digest_source: None,
    }
}

#[test]
fn reuses_the_bazel_action_result_type() {
    // `daemon.proto` imports the vendored RE protos and `build.rs` maps them
    // onto `bazel-remote-apis` via `extern_path`, so the request field must be
    // that crate's own type. If the mapping regressed, prost would generate a
    // parallel `ActionResult` and this wouldn't type-check.
    let result: Option<ActionResult> = create_request().manifest;

    assert!(result.is_some());
}

#[test]
fn round_trips_a_manifest_over_the_wire() {
    let request = create_request();

    let decoded = ArchiveTaskOutputsRequest::decode(request.encode_to_vec().as_slice()).unwrap();

    assert_eq!(decoded.task_target, "app:build");
    assert_eq!(decoded.digest.unwrap().size_bytes, 12);
    assert!(decoded.include_local);
    assert!(!decoded.include_remote);
    assert_eq!(decoded.source_id, "child");
    assert_eq!(decoded.task_key, "child::app:build");

    let manifest = decoded.manifest.unwrap();

    assert_eq!(manifest.exit_code, 3);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "out/a.txt");
    assert!(manifest.output_files[0].is_executable);
}

#[test]
fn round_trips_the_inlined_console_output() {
    // The daemon has no other source for stdout/stderr, so the raw fields have
    // to survive the encode — this is the one hop that populates them.
    let decoded =
        ArchiveTaskOutputsRequest::decode(create_request().encode_to_vec().as_slice()).unwrap();

    let manifest = decoded.manifest.unwrap();

    assert_eq!(manifest.stdout_raw, b"built");
    assert_eq!(manifest.stderr_raw, b"warned");
}

#[test]
fn an_absent_manifest_decodes_as_none() {
    // A message field is always optional on the wire, so the server has to
    // treat a missing manifest as a case to reject rather than a decode error.
    let request = ArchiveTaskOutputsRequest {
        task_target: "app:build".into(),
        digest: None,
        manifest: None,
        include_local: true,
        include_remote: true,
        source_id: "workspace".into(),
        task_key: "workspace::app:build".into(),
        digest_source: None,
    };

    let decoded = ArchiveTaskOutputsRequest::decode(request.encode_to_vec().as_slice()).unwrap();

    assert!(decoded.manifest.is_none());
    assert!(decoded.digest.is_none());
}

#[test]
fn round_trips_canonical_routing_fields_and_digest_source() {
    let mut request = create_request();
    request.digest_source = Some(ManifestDigestSource {
        digest: Some(Digest {
            hash: hex('c'),
            size_bytes: 11,
        }),
        path: ".moon/cache/hashes/task.json".into(),
        bytes: b"fingerprint".to_vec().into(),
    });

    let archive = ArchiveTaskOutputsRequest::decode(request.encode_to_vec().as_slice()).unwrap();
    let source = archive.digest_source.unwrap();
    assert_eq!(archive.source_id, "child");
    assert_eq!(archive.task_key, "child::app:build");
    assert_eq!(source.path, ".moon/cache/hashes/task.json");
    assert_eq!(source.bytes.as_ref(), b"fingerprint");

    let hash = HashFilesRequest {
        files: vec!["src/lib.rs".into()],
        source_id: "child".into(),
    };
    let hash = HashFilesRequest::decode(hash.encode_to_vec().as_slice()).unwrap();
    assert_eq!(hash.source_id, "child");

    let hydrate = HydrateTaskOutputsRequest {
        task_target: "app:build".into(),
        source_id: "child".into(),
        task_key: "child::app:build".into(),
        ..Default::default()
    };
    let hydrate = HydrateTaskOutputsRequest::decode(hydrate.encode_to_vec().as_slice()).unwrap();
    assert_eq!(hydrate.source_id, "child");
    assert_eq!(hydrate.task_key, "child::app:build");
    assert_eq!(PROTOCOL_VERSION, 3);
}
