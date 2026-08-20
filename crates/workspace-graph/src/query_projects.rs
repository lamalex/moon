use crate::{QueryScope, WorkspaceGraph};
use moon_common::{Id, IdExt, color};
use moon_project_graph::{GraphConnections, Project};
use moon_query::*;
use moon_target::ProjectKey;
use std::{fmt::Debug, sync::Arc};
use tracing::{debug, instrument};

impl WorkspaceGraph {
    /// Return all expanded projects that match the query criteria.
    #[instrument(skip(self))]
    pub fn query_projects<'input, Q: AsRef<Criteria<'input>> + Debug>(
        &self,
        query: Q,
    ) -> miette::Result<Vec<Arc<Project>>> {
        let mut projects = vec![];

        for key in self
            .internal_query_projects(query, QueryScope::Primary)?
            .iter()
        {
            projects.push(self.get_project_by_key(key)?);
        }

        Ok(projects)
    }

    /// Return expanded projects matching the query with canonical identities.
    #[instrument(skip(self))]
    pub fn query_projects_with_keys<'input, Q: AsRef<Criteria<'input>> + Debug>(
        &self,
        query: Q,
    ) -> miette::Result<Vec<(ProjectKey, Arc<Project>)>> {
        let mut projects = vec![];

        for key in self.internal_query_projects(query, QueryScope::All)?.iter() {
            projects.push((key.clone(), self.get_project_by_key(key)?));
        }

        Ok(projects)
    }

    fn internal_query_projects<'input, Q: AsRef<Criteria<'input>>>(
        &self,
        query: Q,
        scope: QueryScope,
    ) -> miette::Result<Arc<Vec<ProjectKey>>> {
        let query = query.as_ref();
        let query_input = query
            .input
            .as_ref()
            .expect("Querying the project graph requires a query input string.");
        let cache_key = scope.cache_key(query_input);

        if let Some(cache) = self
            .project_query_cache
            .read_sync(&cache_key, |_, value| value.clone())
        {
            return Ok(cache);
        }

        debug!(
            ?scope,
            "Querying projects with {}",
            color::shell(query_input)
        );

        let mut project_keys = self.projects.get_node_keys();
        project_keys.sort();
        let mut keys = vec![];

        for key in project_keys {
            if matches!(scope, QueryScope::Primary) && key.source_id() != self.sources.primary_id()
            {
                continue;
            }

            let project = self.projects.get_unexpanded_by_key(&key)?;

            if self.does_project_match_criteria(project, query)? {
                keys.push(key);
            }
        }

        let keys = Arc::new(keys);
        let _ = self
            .project_query_cache
            .insert_sync(cache_key, Arc::clone(&keys));

        Ok(keys)
    }

    fn does_project_match_criteria(
        &self,
        project: &Project,
        query: &Criteria,
    ) -> miette::Result<bool> {
        let match_all = matches!(query.op, LogicalOperator::And);
        let mut matched_any = false;

        for condition in &query.conditions {
            let matches = match condition {
                Condition::Field { field, .. } => {
                    let result = match field {
                        Field::Language(langs) => condition.matches_enum(langs, &project.language),
                        Field::Project(ids) => {
                            if condition.matches(ids, &project.id)? {
                                Ok(true)
                            } else if !project.aliases.is_empty() {
                                condition.matches_list(ids, &project.aliases)
                            } else {
                                Ok(false)
                            }
                        }
                        Field::ProjectAlias(aliases) => {
                            if !project.aliases.is_empty() {
                                condition.matches_list(aliases, &project.aliases)
                            } else {
                                Ok(false)
                            }
                        }
                        Field::ProjectLayer(types) => condition.matches_enum(types, &project.layer),
                        Field::ProjectId(ids) => condition.matches(ids, &project.id),
                        Field::ProjectSource(sources) => {
                            condition.matches(sources, &project.source)
                        }
                        Field::ProjectStack(types) => condition.matches_enum(types, &project.stack),
                        Field::ProjectTag(tags) => {
                            condition.matches_list(tags, &project.config.tags)
                        }
                        Field::Task(ids) => Ok(project.task_targets.iter().any(|target| {
                            target
                                .get_task_id()
                                .and_then(|task_id| condition.matches(ids, task_id))
                                .unwrap_or_default()
                        })),
                        Field::TaskTag(tags) => Ok(self
                            .tasks
                            .get_many_by_key(&self.task_keys(project)?)?
                            .iter()
                            .any(|task| {
                                condition.matches_list(tags, &task.tags).unwrap_or_default()
                            })),
                        Field::TaskToolchain(ids) => Ok(self
                            .tasks
                            .get_many_by_key(&self.task_keys(project)?)?
                            .iter()
                            .any(|task| {
                                let mut toolchains = vec![];

                                // Support stable and unstable IDs
                                for id in &task.toolchains {
                                    let (stable_id, unstable_id) = Id::stable_and_unstable(id);
                                    toolchains.push(stable_id);
                                    toolchains.push(unstable_id);
                                }

                                condition.matches_list(ids, &toolchains).unwrap_or_default()
                            })),
                        Field::TaskType(types) => Ok(self
                            .tasks
                            .get_many_by_key(&self.task_keys(project)?)?
                            .iter()
                            .any(|task| {
                                condition
                                    .matches_enum(types, &task.type_of)
                                    .unwrap_or_default()
                            })),
                    };

                    result?
                }
                Condition::Criteria { criteria } => {
                    self.does_project_match_criteria(project, criteria)?
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

    fn task_keys(&self, project: &Project) -> miette::Result<Vec<moon_target::TaskKey>> {
        project
            .task_targets
            .iter()
            .map(|target| moon_target::TaskKey::from_target(project.source_id.clone(), target))
            .collect()
    }
}
