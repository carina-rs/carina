//! Typed explicit dependency targets used after configuration resolution.
//!
//! Parser types retain authored strings. Live planning crosses this module
//! once: every name is matched against the managed bindings in scope and
//! lowered to [`ResolvedDependencyTarget`]. Format-37 saved plans persist that
//! resolver-produced proof and decode it here without re-resolving names.
//! The target's fields are private so scheduling code cannot manufacture an
//! edge from an unchecked runtime string.

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
/// Non-empty values can only be created by this module's resolver or decoded
/// from its persisted format-37 proof. Empty is public because effects with no
/// authored ordering edges need no authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedDependencies(HashMap<String, ResolvedDependencyTarget>);

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
        self.0.values()
    }

    pub fn contains(&self, binding: &str) -> bool {
        self.0.contains_key(binding)
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    pub fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub(crate) fn names_snapshot(&self) -> HashSet<String> {
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
        #[derive(Serialize)]
        struct PersistedDependencyTarget<'a> {
            binding: &'a str,
            identity: &'a ResourceIdentity,
        }

        let mut targets = self
            .iter()
            .map(|target| PersistedDependencyTarget {
                binding: target.as_str(),
                identity: target.identity(),
            })
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| left.binding.cmp(right.binding));
        targets.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResolvedDependencies {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PersistedDependencyTarget {
            binding: String,
            identity: ResourceIdentity,
        }

        let raw = Vec::<PersistedDependencyTarget>::deserialize(deserializer)?;
        let mut targets = HashMap::with_capacity(raw.len());
        for target in raw {
            let binding = target.binding;
            let resolved = ResolvedDependencyTarget {
                binding: binding.clone(),
                identity: target.identity,
            };
            if targets.insert(binding.clone(), resolved).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "duplicate persisted dependency target '{binding}'"
                )));
            }
        }
        Ok(Self(targets))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl FromIterator<String> for ResolvedDependencies {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self(
            iter.into_iter()
                .map(|binding| ResolvedDependencyTarget {
                    identity: ResourceIdentity::new(binding.clone()),
                    binding: binding.clone(),
                })
                .map(|target| (target.binding.clone(), target))
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
    #[error(
        "provider satisfier hint named binding '{dependency}', which is not declared in this scope"
    )]
    UnknownProviderHint { dependency: String },
}

pub(crate) fn validate_dependency_metadata(
    owner: String,
    authored: &[String],
    resolved: &ResolvedDependencies,
) -> Result<(), DependencyResolutionError> {
    let authored = authored.iter().map(String::as_str).collect::<HashSet<_>>();
    if authored.len() == resolved.len() && authored.iter().all(|binding| resolved.contains(binding))
    {
        Ok(())
    } else {
        Err(DependencyResolutionError::MetadataMismatch { owner })
    }
}

mod scope_entry {
    pub trait Sealed {}
}

/// A resource-shaped configuration node that can contribute a binding and
/// identity to the dependency resolver's scope.
pub trait DependencyScopeEntry: scope_entry::Sealed {
    fn dependency_id(&self) -> &crate::resource::ResourceId;
    fn dependency_binding(&self) -> Option<&str>;
}

impl<S, P: crate::resource::PlaceholderKind> scope_entry::Sealed for Resource<S, P> {}
impl<S, P: crate::resource::PlaceholderKind> DependencyScopeEntry for Resource<S, P> {
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        &self.id
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl<S, P: crate::resource::PlaceholderKind> scope_entry::Sealed for DataSource<S, P> {}
impl<S, P: crate::resource::PlaceholderKind> DependencyScopeEntry for DataSource<S, P> {
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        &self.id
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl<S> scope_entry::Sealed for ResolvedResource<S> {}
impl<S> DependencyScopeEntry for ResolvedResource<S> {
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        &self.id
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl<S> scope_entry::Sealed for ResolvedDataSource<S> {}
impl<S> DependencyScopeEntry for ResolvedDataSource<S> {
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        &self.id
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl<P: crate::resource::PlaceholderKind> scope_entry::Sealed
    for crate::resource::LiveComposition<P>
{
}
impl<P: crate::resource::PlaceholderKind> DependencyScopeEntry
    for crate::resource::LiveComposition<P>
{
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        self.id.as_inner()
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl scope_entry::Sealed for crate::resource::SavedComposition {}
impl DependencyScopeEntry for crate::resource::SavedComposition {
    fn dependency_id(&self) -> &crate::resource::ResourceId {
        self.id.as_inner()
    }

    fn dependency_binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

/// Resolver-owned lookup of the managed targets admitted by depends_on.
#[derive(Debug, Clone)]
pub struct ResolvedDependencyScope {
    managed: HashMap<String, ResolvedDependencyTarget>,
    pending_managed: HashSet<String>,
    data_sources: HashMap<String, ResolvedDependencyTarget>,
    pending_data_sources: HashSet<String>,
    upstream_states: HashSet<String>,
}

impl ResolvedDependencyScope {
    pub fn empty() -> Self {
        Self {
            managed: HashMap::new(),
            pending_managed: HashSet::new(),
            data_sources: HashMap::new(),
            pending_data_sources: HashSet::new(),
            upstream_states: HashSet::new(),
        }
    }

    pub fn from_resources<M: DependencyScopeEntry, D: DependencyScopeEntry>(
        managed: &[M],
        data_sources: &[D],
        upstream_states: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut targets = HashMap::new();
        let mut pending_managed = HashSet::new();
        for resource in managed {
            let Some(binding) = resource.dependency_binding() else {
                continue;
            };
            let ResourceIdentityState::Resolved(identity) =
                resource.dependency_id().identity_state()
            else {
                pending_managed.insert(binding.to_string());
                continue;
            };
            targets.insert(
                binding.to_string(),
                ResolvedDependencyTarget {
                    binding: binding.to_string(),
                    identity: identity.clone(),
                },
            );
        }
        let mut resolved_data_sources = HashMap::new();
        let mut pending_data_sources = HashSet::new();
        for source in data_sources {
            let Some(binding) = source.dependency_binding() else {
                continue;
            };
            let ResourceIdentityState::Resolved(identity) = source.dependency_id().identity_state()
            else {
                pending_data_sources.insert(binding.to_string());
                continue;
            };
            resolved_data_sources.insert(
                binding.to_string(),
                ResolvedDependencyTarget {
                    binding: binding.to_string(),
                    identity: identity.clone(),
                },
            );
        }
        Self {
            managed: targets,
            pending_managed,
            data_sources: resolved_data_sources,
            pending_data_sources,
            upstream_states: upstream_states.into_iter().collect(),
        }
    }

    pub fn with_declared_targets<T: DependencyScopeEntry>(mut self, targets: &[T]) -> Self {
        for target in targets {
            let Some(binding) = target.dependency_binding() else {
                continue;
            };
            match target.dependency_id().identity_state() {
                ResourceIdentityState::Resolved(identity) => {
                    self.managed.insert(
                        binding.to_string(),
                        ResolvedDependencyTarget {
                            binding: binding.to_string(),
                            identity: identity.clone(),
                        },
                    );
                }
                ResourceIdentityState::Pending(_) => {
                    self.pending_managed.insert(binding.to_string());
                }
            }
        }
        self
    }

    fn resolve(
        &self,
        owner: String,
        self_binding: Option<&str>,
        implicit_target: Option<&str>,
        dependencies: impl IntoIterator<Item = String>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let mut resolved = HashMap::new();
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
            if self.data_sources.contains_key(&dependency)
                || self.pending_data_sources.contains(&dependency)
            {
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
            resolved.insert(dependency, target.clone());
        }
        Ok(ResolvedDependencies(resolved))
    }

    pub fn resolve_resource<S, P: crate::resource::PlaceholderKind>(
        &self,
        resource: &Resource<S, P>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let name = resource.binding.as_deref().unwrap_or("(anonymous)");
        self.resolve(
            format!("directives.depends_on on '{name}'"),
            resource.binding.as_deref(),
            None,
            resource.directives.depends_on.iter().cloned(),
        )
    }

    pub fn resolve_data_source<S, P: crate::resource::PlaceholderKind>(
        &self,
        resource: &DataSource<S, P>,
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

    pub(crate) fn resolve_provider_dependencies(
        &self,
        dependencies: impl IntoIterator<Item = String>,
    ) -> Result<ResolvedDependencies, DependencyResolutionError> {
        let mut resolved = HashMap::new();
        for dependency in dependencies {
            let target = self
                .managed
                .get(&dependency)
                .or_else(|| self.data_sources.get(&dependency))
                .ok_or_else(|| DependencyResolutionError::UnknownProviderHint {
                    dependency: dependency.clone(),
                })?;
            resolved.insert(dependency, target.clone());
        }
        Ok(ResolvedDependencies(resolved))
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
    fn wait_rejects_an_unknown_dependency() {
        let resources = vec![managed("target")];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &[] as &[DataSource],
            std::iter::empty(),
        );

        let error = scope
            .resolve_wait(wait("w", "target", &["nope"]))
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
        );

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
        let mut data_source: DataSource = DataSource::new("mock.test.Lookup", "lookup");
        data_source.binding = Some("lookup".to_string());
        let data_sources = vec![data_source];
        let scope = ResolvedDependencyScope::from_resources(
            &resources,
            &data_sources,
            ["remote".to_string()],
        );

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
        );

        let dependencies = scope.resolve_resource(&consumer).unwrap();
        let resolved = dependencies.iter().next().unwrap();

        assert_eq!(resolved.as_str(), "gate");
        assert_eq!(Some(resolved.identity()), target.id.identity());
    }
}
