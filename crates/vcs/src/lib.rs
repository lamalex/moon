pub mod git;

mod changed_files;
mod vcs;
mod workspace_files;

pub use changed_files::*;
pub use vcs::*;
pub use workspace_files::*;

pub type BoxedVcs = Box<dyn Vcs + Send + Sync + 'static>;
