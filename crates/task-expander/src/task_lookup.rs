use moon_target::TaskKey;
use moon_task::Task;
use std::sync::Arc;

pub trait TaskLookup {
    fn get_task(&self, key: &TaskKey) -> miette::Result<Arc<Task>>;
}
