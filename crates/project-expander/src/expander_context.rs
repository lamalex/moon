use moon_common::SourceRootId;
use moon_target::ProjectKey;
use rustc_hash::FxHashMap;
use std::path::Path;

pub struct ProjectExpanderContext<'graph> {
    /// Source-local mapping of aliases to canonical project keys.
    pub aliases: FxHashMap<&'graph str, &'graph ProjectKey>,

    /// Source containing the project being expanded.
    pub source_id: &'graph SourceRootId,

    /// Workspace root, of course.
    pub workspace_root: &'graph Path,
}
