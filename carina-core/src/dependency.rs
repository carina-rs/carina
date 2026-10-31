//! Typed explicit dependency targets used after configuration resolution.
//!
//! Parser and saved-plan wire types intentionally retain authored strings.
//! Live planning crosses this module once: every name is matched against the
//! managed bindings in scope and lowered to [`ResolvedDependencyTarget`].
//! The target's field and constructor are private so scheduling code cannot
//! manufacture an edge from an unchecked string.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::parser::{BindingName, WaitBinding};
use crate::resource::{
    DataSource, ResolvedDataSource, ResolvedResource, Resource, ResourceIdentity,
    ResourceIdentityState,
};

/// A managed resource selected by the explicit-dependency resolver.
///
/// Raw names cannot construct this proof outside the resolver:
///
/// ```compile_fail
/// use carina_core::dependency::ResolvedDependencyTarget;
/// use carina_core::resource::ResourceIdentity;
///
/// let _unchecked = ResolvedDependencyTarget {
///     binding: "unchecked".to_string(),
///     identity: ResourceIdentity::new("unchecked"),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedDependencyTarget {
    binding: String,
    identity: ResourceIdentity,
}

impl ResolvedDependencyTarget {
    pub fn identity(&self) -> &ResourceIdentity {
        &self.identity
    }

    pub fn as_str(&self) -> &str {
        &self.binding
    }
}

/// Deduplicated explicit dependency targets.
///
/// Non-empty values can only be created by this module's resolver. Empty is
/// public because effects with no authored ordering edges need no authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedDependencies(HashSet<ResolvedDependencyTarget>);

impl ResolvedDependencies {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResolvedDependencyTarget> {
        self.0.iter()
    }

    pub fn contains(&self, binding: &str) -> bool {
        self.0.iter().any(|target| target.as_str() == binding)
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    pub(crate) fn names(&self) -> HashSet<String> {
        self.iter()
            .map(|target| target.as_str().to_string())
            .collect()
    }
}

impl Serialize for ResolvedDependencies {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.names().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResolvedDependencies {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = HashSet::<String>::deserialize(deserializer)?;
        if raw.is_empty() {
            Ok(Self::empty())
        } else {
            Err(serde::de::Error::custom(
                "explicit dependency names must be resolved against the plan scope",
            ))
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl FromIterator<String> for ResolvedDependencies {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self(
            iter.into_iter()
                .map(|binding| ResolvedDependencyTarget {
                    identity: ResourceIdentity::new(binding.clone()),
                    binding,
                })
                .collect(),
        )
    }
}

/// A wait declaration whose explicit dependencies have crossed the resolver.
#[derive(Debug, Clone)]
pub struct ResolvedWaitBinding {
    inner: WaitBinding,
    explicit_dependencies: ResolvedDependencies,
}

impl ResolvedWaitBinding {
    pub fn as_wait_binding(&self) -> &WaitBinding {
        &self.inner
    }

    pub fn explicit_dependencies(&self) -> &ResolvedDependencies {
        &self.explicit_dependencies
    }
}

impl std::ops::Deref for ResolvedWaitBinding {
    type Target = WaitBinding;

    fn deref(&self) -> &Self::Target {
        self.as_wait_binding()
    }
}

/// Failure to lower an authored or persisted dependency name to a managed
/// resource in the exact planning scope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DependencyResolutionError {
    #[error("{owner}: binding '{dependency}' is not declared in this scope")]
    Unknown { owner: String, dependency: String },
    #[error("{owner}: data sources cannot be depends_on targets (binding '{dependency}')")]
    DataSource { owner: String, dependency: String },
    #[error("{owner}: upstream_state binding '{dependency}' is not a valid depends_on target")]
    UpstreamState { owner: String, dependency: String },
    #[error("{owner}: self-reference is not allowed (binding '{dependency}' depends on itself)")]
    SelfReference { owner: String, dependency: String },
    #[error("managed dependency target '{binding}' still has a pending identity")]
    PendingTargetIdentity { binding: String },
    #[error("resolved dependency metadata does not match authored depends_on on {owner}")]
    MetadataMismatch { owner: String },
}

/// Resolver-owned lookup of the managed targets admitted by depends_on.
#[derive(Debug, Clone)]
pub struct ResolvedDependencyScope {
    managed: HashMap<String, ResolvedDependencyTarget>,
    pending_managed: HashSet<String>,
    data_sources: HashSet<String>,
    upstream_states: HashSet<String>,
}

impl ResolvedDependencyScope {
    pub fn empty() -> Self {
        Self {
            managed: HashMap::new(),
            pending_managed: HashSet::new(),
            data_sources: HashSet::new(),
            upstream_states: HashSet::new(),
        }
    }

    pub fn from_resources<S>(
        managed: &[Resource<S>],
        data_sources: &[DataSource<S>],
        upstream_states: impl IntoIterator<Item = String>,
    ) -> Result<Self, DependencyResolutionError> {
        let mut targets = HashMap::new();
        let mut pending_managed = HashSet::new();
        for resource in managed {
            let Some(binding) = resource.binding.as_ref() else {
                continue;
            };
            let ResourceIdentityState::Resolved(identity) = resource.id.identity_state() else {
                pending_managed.insert(binding.clone());
                continue;
            };
            targets.insert(
                binding.clone(),
                ResolvedDependencyTarget {
                    binding: binding.clone(),
                    identity: identity.clone(),
                },
            );
        }
        Ok(Self {
            managed: targets,
            pending_managed,
            data_sources: data_sources
                .iter()
                .filter_map(|source| source.binding.clone())
                .collect(),
            upstream_states: upstream_states.into_iter().collect(),
        })
    }

    pub fn from_resolved_resources<S>(
        managed: &[ResolvedResource<S>],
        data_sources: &[DataSource<S>],
    ) -> Result<Self, DependencyResolutionError> {
        let mut targets = HashMap::new();
        for resource in managed {
            let Some(binding) = resource.binding.as_ref() else {
                continue;
            };
            targets.insert(
                binding.clone(),
                ResolvedDependencyTarget {
                    binding: binding.clone(),
                    identity: resource.identity().clone(),
                },
            );
        }
        Ok(Self {
            managed: targets,
            pending_managed: HashSet::new(),
            data_sources: data_sources
                .iter()
                .filter_map(|source| source.binding.clone())
                .collect(),
            upstream_states: HashSet::new(),
        })
    }

    pub fn from_typed_resources<S>(
        managed: &[ResolvedResource<S>],
        data_sources: &[ResolvedDataSource<S>],
    ) -> Self {
        Self {
            managed: managed
                .iter()
                .filter_map(|resource| {
                    resource.binding.as_ref().map(|binding| {
                        (
                            binding.clone(),
                            ResolvedDependencyTarget {
                                binding: binding.clone(),
                                identity: resource.identity().clone(),
                            },
                        )
                    })
                })
                .collect(),
            pending_managed: HashSet::new(),
            data_sources: data_sources
                .iter()
                .filter_map(|source| source.binding.clone())
                .collect(),
            upstream_states: HashSet::new(),
        }
    }

    fn resolve(
        &self,
        owner: String,
        self_binding: Option<&str>,
        implicit_target: Option<&str>,
        dependencies: impl IntoIterator<Item = String>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let mut resolved = HashSet::new();
        for dependency in dependencies {
            if self_binding == Some(dependency.as_str()) {
                return Err(DependencyResolutionError::SelfReference { owner, dependency });
            }
            // A wait already has a required edge to its target. Validation
            // reports the redundant authored element; runtime omits it.
            if implicit_target == Some(dependency.as_str()) {
                continue;
            }
            if self.upstream_states.contains(&dependency) {
                return Err(DependencyResolutionError::UpstreamState { owner, dependency });
            }
            if self.data_sources.contains(&dependency) {
                return Err(DependencyResolutionError::DataSource { owner, dependency });
            }
            if self.pending_managed.contains(&dependency) {
                return Err(DependencyResolutionError::PendingTargetIdentity {
                    binding: dependency,
                });
            }
            let Some(target) = self.managed.get(&dependency) else {
                return Err(DependencyResolutionError::Unknown { owner, dependency });
            };
            resolved.insert(target.clone());
        }
        Ok(ResolvedDependencies(resolved))
    }

    pub fn resolve_resource<S>(
        &self,
        resource: &Resource<S>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let name = resource.binding.as_deref().unwrap_or("(anonymous)");
        self.resolve(
            format!("directives.depends_on on '{name}'"),
            resource.binding.as_deref(),
            None,
            resource.directives.depends_on.iter().cloned(),
        )
    }

    pub fn resolve_data_source<S>(
        &self,
        resource: &DataSource<S>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let name = resource.binding.as_deref().unwrap_or("(anonymous)");
        self.resolve(
            format!("directives.depends_on on '{name}'"),
            resource.binding.as_deref(),
            None,
            resource.directives.depends_on.iter().cloned(),
        )
    }

    pub fn resolve_wait(
        &self,
        wait: WaitBinding,
    ) -> Result<ResolvedWaitBinding, DependencyResolutionError> {
        let explicit_dependencies = self.resolve(
            format!("wait '{}' depends_on", wait.binding),
            Some(wait.binding.as_str()),
            Some(wait.target.as_str()),
            wait.depends_on.iter().map(BindingName::to_string),
        )?;
        Ok(ResolvedWaitBinding {
            inner: wait,
            explicit_dependencies,
        })
    }

    pub fn resolve_persisted_wait(
        &self,
        identity: &ResourceIdentity,
        target: &ResourceIdentity,
        dependencies: impl IntoIterator<Item = String>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        self.resolve(
            format!("wait '{}' depends_on", identity.as_str()),
            Some(identity.as_str()),
            Some(target.as_str()),
            dependencies,
        )
    }

    /// Resolve provider-derived names after the provider has already filtered
    /// them against the known managed binding set.
    pub(crate) fn resolve_provider_dependencies(
        &self,
        dependencies: impl IntoIterator<Item = String>,
    ) -> ResolvedDependencies {
        ResolvedDependencies(
            dependencies
                .into_iter()
                .filter_map(|dependency| self.managed.get(&dependency).cloned())
                .collect(),
        )
    }
}

/// Resolve every explicit dependency carried into one live planning run.
pub struct ResolvedPlanningDependencies {
    scope: ResolvedDependencyScope,
    managed: Vec<ResolvedDependencies>,
    data_sources: Vec<ResolvedDependencies>,
    waits: Vec<ResolvedWaitBinding>,
}

impl ResolvedPlanningDependencies {
    pub fn resolve<S: Clone>(
        managed: &[Resource<S>],
        data_sources: &[DataSource<S>],
        waits: &[WaitBinding],
        upstream_states: impl IntoIterator<Item = String>,
    ) -> Result<Self, DependencyResolutionError> {
        let scope =
            ResolvedDependencyScope::from_resources(managed, data_sources, upstream_states)?;
        let managed_dependencies = managed
            .iter()
            .map(|resource| scope.resolve_resource(resource))
            .collect::<Result<_, _>>()?;
        let data_source_dependencies = data_sources
            .iter()
            .map(|resource| scope.resolve_data_source(resource))
            .collect::<Result<_, _>>()?;
        let waits = waits
            .iter()
            .cloned()
            .map(|wait| scope.resolve_wait(wait))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            scope,
            managed: managed_dependencies,
            data_sources: data_source_dependencies,
            waits,
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        ResolvedDependencyScope,
        Vec<ResolvedDependencies>,
        Vec<ResolvedDependencies>,
        Vec<ResolvedWaitBinding>,
    ) {
        (self.scope, self.managed, self.data_sources, self.waits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{BindingName, UntilPredicateAst};
    use crate::resource::{ConcreteValue, DataSource, Resource, Value};

    fn managed(binding: &'static str) -> Resource {
        let mut resource = Resource::new("mock.test.Resource", binding);
        resource.binding = Some(binding.to_string());
        resource
    }

    fn wait(binding: &str, target: &str, dependencies: &[&str]) -> WaitBinding {
        WaitBinding {
            binding: BindingName::new(binding),
            target: BindingName::new(target),
            until_raw: format!("{target}.status == ready"),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec![target.to_string(), "status".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("ready".into())),
            },
            timeout_secs: None,
            depends_on: dependencies.iter().copied().map(BindingName::new).collect(),
            line: 1,
        }
    }

    #[test]
    fn persisted_wait_rejects_an_unknown_dependency() {
        let resources = vec![managed("target")];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &[] as &[DataSource],
            std::iter::empty(),
        )
        .unwrap();

        let error = scope
            .resolve_persisted_wait(
                &ResourceIdentity::new("w"),
                &ResourceIdentity::new("target"),
                ["nope".to_string()],
            )
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "wait 'w' depends_on: binding 'nope' is not declared in this scope"
        );
    }

    #[test]
    fn wait_target_is_implicit_and_not_duplicated_in_resolved_dependencies() {
        let resources = vec![managed("target"), managed("other")];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &[] as &[DataSource],
            std::iter::empty(),
        )
        .unwrap();

        let resolved = scope
            .resolve_wait(wait("w", "target", &["target", "other"]))
            .unwrap();

        assert_eq!(resolved.explicit_dependencies().len(), 1);
        assert!(resolved.explicit_dependencies().contains("other"));
        assert!(!resolved.explicit_dependencies().contains("target"));
    }

    #[test]
    fn wait_dependency_target_rules_match_managed_resource_rules() {
        let resources = vec![managed("target")];
        let mut data_source = DataSource::new("mock.test.Lookup", "lookup");
        data_source.binding = Some("lookup".to_string());
        let data_sources = vec![data_source];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &data_sources,
            ["remote".to_string()],
        )
        .unwrap();

        assert!(matches!(
            scope.resolve_wait(wait("w", "target", &["lookup"])),
            Err(DependencyResolutionError::DataSource { dependency, .. }) if dependency == "lookup"
        ));
        assert!(matches!(
            scope.resolve_wait(wait("w", "target", &["remote"])),
            Err(DependencyResolutionError::UpstreamState { dependency, .. }) if dependency == "remote"
        ));
        assert!(matches!(
            scope.resolve_wait(wait("w", "target", &["w"])),
            Err(DependencyResolutionError::SelfReference { dependency, .. }) if dependency == "w"
        ));
    }

    #[test]
    fn resolved_target_retains_authored_binding_and_runtime_identity() {
        let target = Resource::with_provider("mock", "test.Resource", "provider-id", None)
            .with_binding("gate");
        let mut consumer = managed("consumer");
        consumer.directives.depends_on.push("gate".to_string());
        let resources = vec![target.clone(), consumer.clone()];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &[] as &[DataSource],
            std::iter::empty(),
        )
        .unwrap();

        let dependencies = scope.resolve_resource(&consumer).unwrap();
        let resolved = dependencies.iter().next().unwrap();

        assert_eq!(resolved.as_str(), "gate");
        assert_eq!(Some(resolved.identity()), target.id.identity());
    }
}
