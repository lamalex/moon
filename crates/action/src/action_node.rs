use indexmap::IndexMap;
use moon_common::path::WorkspaceRelativePathBuf;
use moon_common::{Id, SourceRootId, is_test_env};
use moon_target::{ProjectKey, Target, TaskInvocationKey, TaskKey};
use moon_toolchain::{ToolchainSpec, VersionSpec};
use rustc_hash::FxHasher;
use serde::Serialize;
use std::fmt;
use std::hash::{Hash, Hasher};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallDependenciesNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<ProjectKey>,

    pub root: WorkspaceRelativePathBuf,

    pub source_id: SourceRootId,

    pub toolchain_id: Id,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupEnvironmentNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_key: Option<ProjectKey>,

    pub root: WorkspaceRelativePathBuf,

    pub source_id: SourceRootId,

    pub toolchain_id: Id,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupProtoNode {
    pub source_id: SourceRootId,
    pub version: VersionSpec,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupToolchainNode {
    pub source_id: SourceRootId,
    pub toolchain: ToolchainSpec,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProjectNode {
    pub project_key: ProjectKey,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncWorkspaceNode {
    pub source_id: SourceRootId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunTaskNode {
    pub args: Vec<String>,
    pub env: IndexMap<String, Option<String>>,
    pub interactive: bool, // Interactive with stdin
    pub persistent: bool,  // Never terminates
    pub priority: u8,
    pub key: TaskKey,
    pub target: Target,
    pub id: Option<u64>, // For action graph states
}

impl RunTaskNode {
    pub fn new(target: Target) -> Self {
        Self::new_with_key(
            TaskKey::from_target(Default::default(), &target)
                .expect("Run task targets must be project and task qualified"),
            target,
        )
    }

    pub fn new_with_key(key: TaskKey, target: Target) -> Self {
        Self {
            args: vec![],
            env: IndexMap::default(),
            interactive: false,
            persistent: false,
            priority: 2, // normal
            key,
            target,
            id: None,
        }
    }

    pub fn invocation_key(&self) -> TaskInvocationKey {
        TaskInvocationKey::new(
            self.key.clone(),
            &self.args,
            self.env.iter().map(|(key, value)| (key, value.as_ref())),
        )
    }

    fn calculate_id(&mut self) {
        let mut hasher = FxHasher::default();
        self.key.hash(&mut hasher);

        if self.persistent {
            hasher.write_u8(100);
        }

        if self.interactive {
            hasher.write_u8(50);
        }

        self.id = Some(hasher.finish());
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(tag = "action", content = "params", rename_all = "kebab-case")]
pub enum ActionNode {
    #[default]
    None,

    /// Install toolchain dependencies in the closest root.
    InstallDependencies(Box<InstallDependenciesNode>),

    /// Run a project's task.
    RunTask(Box<RunTaskNode>),

    /// Setup the environment for the provided toolchain.
    SetupEnvironment(Box<SetupEnvironmentNode>),

    /// Setup and install proto.
    SetupProto(Box<SetupProtoNode>),

    /// Setup and install the provided toolchain.
    SetupToolchain(Box<SetupToolchainNode>),

    /// Sync a project with language specific semantics.
    SyncProject(Box<SyncProjectNode>),

    /// Sync the entire moon workspace and install system dependencies.
    SyncWorkspace(Box<SyncWorkspaceNode>),
}

impl ActionNode {
    pub fn install_dependencies(node: InstallDependenciesNode) -> Self {
        Self::InstallDependencies(Box::new(node))
    }

    pub fn run_task(mut node: RunTaskNode) -> Self {
        node.calculate_id();

        Self::RunTask(Box::new(node))
    }

    pub fn setup_environment(node: SetupEnvironmentNode) -> Self {
        Self::SetupEnvironment(Box::new(node))
    }

    pub fn setup_proto(source_id: SourceRootId, version: VersionSpec) -> Self {
        Self::SetupProto(Box::new(SetupProtoNode { source_id, version }))
    }

    pub fn setup_toolchain(node: SetupToolchainNode) -> Self {
        Self::SetupToolchain(Box::new(node))
    }

    pub fn sync_project(node: SyncProjectNode) -> Self {
        Self::SyncProject(Box::new(node))
    }

    pub fn sync_workspace(source_id: SourceRootId) -> Self {
        Self::SyncWorkspace(Box::new(SyncWorkspaceNode { source_id }))
    }

    pub fn source_id(&self) -> Option<&SourceRootId> {
        match self {
            Self::None => None,
            Self::InstallDependencies(inner) => Some(&inner.source_id),
            Self::RunTask(inner) => Some(inner.key.project_key().source_id()),
            Self::SetupEnvironment(inner) => Some(&inner.source_id),
            Self::SetupProto(inner) => Some(&inner.source_id),
            Self::SetupToolchain(inner) => Some(&inner.source_id),
            Self::SyncProject(inner) => Some(inner.project_key.source_id()),
            Self::SyncWorkspace(inner) => Some(&inner.source_id),
        }
    }

    pub fn get_id(&self) -> u64 {
        match self {
            Self::RunTask(inner) => inner.id.unwrap_or_default(),
            _ => 0,
        }
    }

    pub fn get_spec(&self) -> Option<&ToolchainSpec> {
        match self {
            Self::SetupToolchain(inner) => Some(&inner.toolchain),
            _ => None,
        }
    }

    pub fn get_priority(&self) -> u8 {
        match self {
            Self::RunTask(inner) => inner.priority,
            _ => 0,
        }
    }

    pub fn is_interactive(&self) -> bool {
        match self {
            Self::RunTask(inner) => inner.interactive,
            _ => false,
        }
    }

    pub fn is_persistent(&self) -> bool {
        match self {
            Self::RunTask(inner) => inner.persistent,
            _ => false,
        }
    }

    pub fn is_standard(&self) -> bool {
        match self {
            Self::RunTask(inner) => !inner.interactive && !inner.persistent,
            _ => true,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::InstallDependencies(inner) => {
                if inner.root.as_str().is_empty() {
                    format!("InstallDependencies({})", inner.toolchain_id)
                } else {
                    format!(
                        "InstallDependencies({}, {})",
                        inner.toolchain_id, inner.root
                    )
                }
            }
            Self::RunTask(inner) => {
                format!(
                    "Run{}Task({})",
                    if inner.persistent {
                        "Persistent"
                    } else if inner.interactive {
                        "Interactive"
                    } else {
                        ""
                    },
                    inner.target
                )
            }
            Self::SetupEnvironment(inner) => {
                if inner.root.as_str().is_empty() {
                    format!("SetupEnvironment({})", inner.toolchain_id)
                } else {
                    format!("SetupEnvironment({}, {})", inner.toolchain_id, inner.root)
                }
            }
            Self::SetupProto(inner) => {
                format!(
                    "SetupProto({})",
                    if is_test_env() {
                        "1.2.3".to_string()
                    } else {
                        inner.version.to_string()
                    }
                )
            }
            Self::SetupToolchain(inner) => {
                format!("SetupToolchain({})", inner.toolchain.target())
            }
            Self::SyncProject(inner) => {
                format!("SyncProject({})", inner.project_key.project_id())
            }
            Self::SyncWorkspace(_) => "SyncWorkspace".into(),
            Self::None => "None".into(),
        }
    }
}

impl fmt::Display for ActionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

impl Hash for ActionNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.label().as_bytes());

        match self {
            Self::InstallDependencies(inner) => {
                hash_source(&inner.source_id, state);
                inner.members.hash(state);
                inner
                    .project_key
                    .as_ref()
                    .map(ProjectKey::project_id)
                    .hash(state);
                inner.root.hash(state);
                inner.toolchain_id.hash(state);
            }
            Self::SetupEnvironment(inner) => {
                hash_source(&inner.source_id, state);
                inner
                    .project_key
                    .as_ref()
                    .map(ProjectKey::project_id)
                    .hash(state);
                inner.root.hash(state);
                inner.toolchain_id.hash(state);
            }
            Self::SetupProto(inner) => hash_source(&inner.source_id, state),
            Self::SetupToolchain(inner) => {
                hash_source(&inner.source_id, state);
                inner.toolchain.hash(state);
            }
            Self::SyncProject(inner) => {
                hash_source(inner.project_key.source_id(), state);
                inner.project_key.project_id().hash(state);
            }
            Self::SyncWorkspace(inner) => hash_source(&inner.source_id, state),

            // For tasks with passthrough arguments and environment variables,
            // we need to ensure the hash is more unique in the graph
            Self::RunTask(inner) => {
                inner.invocation_key().hash(state);
            }
            _ => {}
        };
    }
}

fn hash_source<H: Hasher>(source_id: &SourceRootId, state: &mut H) {
    if source_id != &Default::default() {
        source_id.hash(state);
    }
}
