use moon_common::{Id, SourcePathBuf};
use moon_config::Output;
use moon_hash::fingerprint;
use moon_process::OutputInfo;
use moon_project::Project;
use moon_target::TaskInvocationKey;
use moon_task::{ProjectKey, Task, TaskKey};
use std::collections::BTreeMap;

fingerprint!(
    pub struct TaskFingerprint<'task> {
        // Task option `cacheKey`
        #[serde(skip_serializing_if = "Option::is_none")]
        pub cache_key: Option<&'task str>,

        // Task `command`
        pub command: &'task str,

        // Task `args`
        pub args: Vec<&'task str>,

        // Task `deps` mapped to their hash
        pub deps: BTreeMap<TaskInvocationKey, String>,

        // Environment variables
        pub env: BTreeMap<&'task str, Option<&'task str>>,

        // Input files and globs mapped to a unique hash
        pub inputs: BTreeMap<SourcePathBuf, String>,

        // Input environment variables
        pub input_env: BTreeMap<&'task str, String>,

        // Relative output paths
        pub outputs: Vec<&'task Output>,

        // Project `dependsOn`
        pub project_deps: Vec<ProjectKey>,

        // Task `script`
        #[serde(skip_serializing_if = "Option::is_none")]
        pub script: Option<&'task str>,

        // Task `target`
        pub target: TaskKey,

        // Task `toolchains`
        pub toolchains: Vec<&'task Id>,

        // Bump this to invalidate all caches
        pub version: String,
    }
);

impl<'task> TaskFingerprint<'task> {
    pub fn new(project: &'task Project, task: &'task Task) -> Self {
        Self {
            cache_key: task.options.cache_key.as_deref(),
            command: &task.command,
            args: task.args.iter().map(|a| a.get_value()).collect(),
            deps: BTreeMap::new(),
            env: task
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_deref()))
                .collect(),
            inputs: BTreeMap::new(),
            input_env: BTreeMap::new(),
            outputs: task.outputs.iter().collect(),
            project_deps: project
                .dependencies
                .iter()
                .map(|dep| {
                    ProjectKey::new(
                        dep.source_root
                            .clone()
                            .unwrap_or_else(|| project.source_id.clone()),
                        dep.id.clone(),
                    )
                    .expect("Project dependency IDs must be valid canonical identities.")
                })
                .collect(),
            script: task.script.as_deref(),
            target: task.key(),
            toolchains: task.toolchains.iter().collect(),
            // 1 - Original implementation
            // 2 - New task runner crate, tarball structure changed
            // 3 - New action pipeline
            // 4 - Source-qualified task, dependency, project, and input identities
            // 5 - Invocation-qualified dependency identities
            version: "5".into(),
        }
    }
}

fingerprint!(
    #[derive(Default)]
    pub struct TaskChecksFingerprint {
        // Check script to their executed output
        pub checks: BTreeMap<String, OutputInfo>,
    }
);
