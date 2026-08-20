use crate::{QueryScope, WorkspaceGraph};
use moon_common::{Id, SourceRootId, color};
use moon_project_graph::Project;
use moon_query::*;
use moon_target::{ProjectKey, TaskKey};
use moon_task_graph::Task;
use std::{fmt::Debug, sync::Arc};
use tracing::{debug, instrument};

impl WorkspaceGraph {
    /// Return all expanded tasks that match the query criteria.
    #[instrument(skip(self))]
    pub fn query_tasks<'input, Q: AsRef<Criteria<'input>> + Debug>(
        &self,
        query: Q,
    ) -> miette::Result<Vec<Arc<Task>>> {
        let mut tasks = vec![];

        for key in self
            .internal_query_tasks(query, QueryScope::Primary)?
            .iter()
        {
            tasks.push(self.get_task_by_key(key)?);
        }

        Ok(tasks)
    }

    /// Return expanded tasks matching the query with canonical identities.
    #[instrument(skip(self))]
    pub fn query_tasks_with_keys<'input, Q: AsRef<Criteria<'input>> + Debug>(
        &self,
        query: Q,
    ) -> miette::Result<Vec<(TaskKey, Arc<Task>)>> {
        let mut tasks = vec![];

        for key in self.internal_query_tasks(query, QueryScope::All)?.iter() {
            tasks.push((key.clone(), self.get_task_by_key(key)?));
        }

        Ok(tasks)
    }

    fn internal_query_tasks<'input, Q: AsRef<Criteria<'input>>>(
        &self,
        query: Q,
        scope: QueryScope,
    ) -> miette::Result<Arc<Vec<TaskKey>>> {
        let query = query.as_ref();
        let query_input = query
            .input
            .as_ref()
            .expect("Querying the task graph requires a query input string.");
        let cache_key = scope.cache_key(query_input);

        if let Some(cache) = self
            .task_query_cache
            .read_sync(&cache_key, |_, value| value.clone())
        {
            return Ok(cache);
        }

        debug!(?scope, "Querying tasks with {}", color::shell(query_input));

        let mut keys = vec![];

        // Don't use `get_all` as it recursively calls `query`,
        // which runs into a deadlock! This should be faster also...
        for task in self.tasks.get_all_unexpanded()? {
            let source_id = &task.source_id;

            if matches!(scope, QueryScope::Primary) && source_id != self.sources.primary_id() {
                continue;
            }

            if (matches!(scope, QueryScope::Primary) || !task.is_internal())
                && self.does_task_match_criteria(task, source_id, query)?
            {
                keys.push(task.key());
            }
        }

        keys.sort();

        let keys = Arc::new(keys);
        let _ = self
            .task_query_cache
            .insert_sync(cache_key, Arc::clone(&keys));

        Ok(keys)
    }

    // Use the unexpanded project, as expanding may recursively call
    // `query`, which runs into a deadlock!
    fn get_task_parent_project(
        &self,
        task: &Task,
        source_id: &SourceRootId,
    ) -> miette::Result<Option<&Project>> {
        Ok(match task.target.get_project_id() {
            Ok(project_id) => Some(self.projects.get_unexpanded_by_key(&ProjectKey::new(
                source_id.clone(),
                Id::raw(project_id),
            )?)?),
            Err(_) => None,
        })
    }

    fn does_task_match_criteria(
        &self,
        task: &Task,
        source_id: &SourceRootId,
        query: &Criteria,
    ) -> miette::Result<bool> {
        let match_all = matches!(query.op, LogicalOperator::And);
        let mut matched_any = false;

        for condition in &query.conditions {
            let matches = match condition {
                Condition::Field { field, .. } => {
                    let result = match field {
                        Field::Project(ids) => {
                            if let Ok(project_id) = task.target.get_project_id() {
                                if condition.matches(ids, project_id)? {
                                    Ok(true)
                                } else if let Some(project) =
                                    self.get_task_parent_project(task, source_id)?
                                    && !project.aliases.is_empty()
                                {
                                    condition.matches_list(ids, &project.aliases)
                                } else {
                                    Ok(false)
                                }
                            } else {
                                Ok(false)
                            }
                        }
                        Field::Task(ids) => condition.matches(ids, &task.id),
                        Field::TaskTag(tags) => condition.matches_list(tags, &task.tags),
                        Field::TaskToolchain(ids) => condition.matches_list(ids, &task.toolchains),
                        Field::TaskType(types) => condition.matches_enum(types, &task.type_of),
                        // These fields match against the task's parent project
                        _ => Ok(false),
                    };

                    result?
                }
                Condition::Criteria { criteria } => {
                    self.does_task_match_criteria(task, source_id, criteria)?
                }
            };

            if matches {
                matched_any = true;

                if match_all {
                    continue;
                } else {
                    break;
                }
            } else if match_all {
                return Ok(false);
            }
        }

        // No matches using the OR condition
        if !matched_any {
            return Ok(false);
        }

        Ok(true)
    }
}
