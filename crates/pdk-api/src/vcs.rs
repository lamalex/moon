//! Serializable interface for user-scoped VCS overlay plugins.

use crate::{Id, MoonContext};
use warpgate_api::{api_enum, api_struct, api_unit_enum};

pub const VCS_PLUGIN_PROTOCOL_VERSION: u16 = 1;

api_struct!(
    pub struct RegisterVcsInput {
        pub id: Id,
        pub host_protocol_version: u16,
    }
);

api_struct!(
    #[serde(default)]
    pub struct VcsPluginMetadata {
        pub name: String,
        pub description: Option<String>,
        pub plugin_version: String,
        pub protocol_version: u16,
    }
);

api_struct!(
    pub struct DetectVcsInput {
        pub context: MoonContext,
    }
);

api_struct!(
    #[serde(default)]
    pub struct DetectVcsOutput {
        pub active: bool,
        pub reason: String,
    }
);

api_struct!(
    pub struct PrepareVcsInput {
        pub context: MoonContext,
        pub consistency: VcsConsistency,
    }
);

api_unit_enum!(
    pub enum VcsConsistency {
        #[default]
        ExistingSnapshot,
        FreshSnapshot,
    }
);

api_struct!(
    pub struct PreparedVcs {
        /// Opaque adapter-defined token that pins all subsequent queries.
        pub snapshot_id: String,
    }
);

api_struct!(
    pub struct GetVcsStateInput {
        pub context: MoonContext,
        pub default_branch: String,
        pub snapshot_id: String,
    }
);

api_struct!(
    #[serde(default)]
    pub struct VcsStatePatch {
        pub adapter: Option<String>,
        pub current_label: Option<String>,
        pub current_revision: Option<String>,
        pub is_default: Option<bool>,
        pub repository_root: Option<String>,
        pub working_root: Option<String>,
    }
);

api_enum!(
    #[derive(Default)]
    #[serde(tag = "type", content = "value", rename_all = "kebab-case")]
    pub enum VcsRevision {
        #[default]
        Current,
        Default,
        Named(String),
    }
);

api_enum!(
    #[derive(Default)]
    #[serde(tag = "type", content = "value", rename_all = "kebab-case")]
    pub enum VcsChangeQuery {
        #[default]
        WorkingCopy,
        Previous {
            revision: VcsRevision,
        },
        Between {
            base: VcsRevision,
            head: VcsRevision,
        },
    }
);

api_struct!(
    pub struct GetVcsChangedFilesInput {
        pub context: MoonContext,
        pub default_branch: String,
        pub query: VcsChangeQuery,
        pub snapshot_id: String,
    }
);

api_unit_enum!(
    pub enum VcsChangedStatus {
        Added,
        Deleted,
        #[default]
        Modified,
    }
);

api_struct!(
    pub struct VcsChangedFile {
        pub path: String,
        pub status: VcsChangedStatus,
    }
);

api_struct!(
    #[serde(default)]
    pub struct GetVcsChangedFilesOutput {
        pub files: Vec<VcsChangedFile>,
    }
);
