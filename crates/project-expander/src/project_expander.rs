use crate::expander_context::ProjectExpanderContext;
use moon_common::color;
use moon_config::ProjectDependencyConfig;
use moon_project::Project;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::mem;
use tracing::{debug, instrument};

pub struct ProjectExpander<'graph> {
    context: ProjectExpanderContext<'graph>,
}

impl<'graph> ProjectExpander<'graph> {
    pub fn new(context: ProjectExpanderContext<'graph>) -> Self {
        Self { context }
    }

    #[instrument(name = "expand_project", skip_all)]
    pub fn expand(mut self, project: &Project) -> miette::Result<Project> {
        let mut project = project.to_owned();

        debug!(
            project_id = project.id.as_str(),
            "Expanding project {}",
            color::id(&project.id)
        );

        self.expand_deps(&mut project)?;

        Ok(project)
    }

    #[instrument(skip_all)]
    fn expand_deps(&mut self, project: &mut Project) -> miette::Result<()> {
        let mut local_dependencies = FxHashMap::default();
        let mut cross_source_dependencies = BTreeMap::new();

        for dep_config in mem::take(&mut project.dependencies) {
            let new_dep_id = if dep_config.is_cross_source() {
                dep_config.id.clone()
            } else {
                self.context
                    .aliases
                    .get(dep_config.id.as_str())
                    .filter(|key| key.source_id() == self.context.source_id)
                    .map(|key| key.project_id().to_owned())
                    .unwrap_or_else(|| dep_config.id.clone())
            };

            let dependency = ProjectDependencyConfig {
                id: new_dep_id.clone(),
                ..dep_config
            };

            if let Some(source_root) = &dependency.source_root {
                cross_source_dependencies.insert((source_root.clone(), new_dep_id), dependency);
            } else {
                // Preserve existing source-local ordering and alias flattening.
                local_dependencies.insert(new_dep_id, dependency);
            }
        }

        project.dependencies = local_dependencies.into_values().collect();
        project
            .dependencies
            .extend(cross_source_dependencies.into_values());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moon_common::{Id, SourceRootId};
    use moon_target::ProjectKey;
    use std::path::Path;

    #[test]
    fn expands_local_aliases_without_flattening_cross_source_identities() {
        let source_id = SourceRootId::new("acme/platform").unwrap();
        let alias_key = ProjectKey::new(source_id.clone(), Id::raw("canonical")).unwrap();
        let mut project = Project {
            id: Id::raw("app"),
            source_id: source_id.clone(),
            dependencies: vec![
                ProjectDependencyConfig::new(Id::raw("alias")),
                ProjectDependencyConfig {
                    id: Id::raw("alias"),
                    source_root: Some(SourceRootId::new("acme/web").unwrap()),
                    ..ProjectDependencyConfig::default()
                },
                ProjectDependencyConfig {
                    id: Id::raw("alias"),
                    source_root: Some(SourceRootId::new("frontend").unwrap()),
                    ..ProjectDependencyConfig::default()
                },
            ],
            ..Project::default()
        };
        let aliases = FxHashMap::from_iter([("alias", &alias_key)]);

        project = ProjectExpander::new(ProjectExpanderContext {
            aliases,
            source_id: &source_id,
            workspace_root: Path::new("."),
        })
        .expand(&project)
        .unwrap();

        assert_eq!(project.dependencies.len(), 3);
        assert!(
            project
                .dependencies
                .iter()
                .any(|dep| { dep.id == "canonical" && dep.source_root.is_none() })
        );
        assert!(project.dependencies.iter().any(|dep| {
            dep.id == "alias" && dep.source_root == Some(SourceRootId::new("acme/web").unwrap())
        }));
        assert!(project.dependencies.iter().any(|dep| {
            dep.id == "alias" && dep.source_root == Some(SourceRootId::new("frontend").unwrap())
        }));
    }
}
