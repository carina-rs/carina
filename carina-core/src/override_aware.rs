//! Resolved resources with permanent name overrides applied.

use std::collections::{HashMap, HashSet};

use crate::binding_index::{PreApplyInputs, ResolvedBindings};
use crate::name_override::{ApplyDecision, NameOverride, should_apply_override};
use crate::resource::{ConcreteValue, Resource, ResourceId, Value};

/// Minimal state-like surface needed by [`OverrideAwareResources`].
pub trait NameOverrideSource {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>>;
}

impl NameOverrideSource for HashMap<ResourceId, HashMap<String, NameOverride>> {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>> {
        self.get(resource_id)
    }
}

pub struct OverrideAwareResources {
    resources: Vec<Resource>,
    unresolved_resources: Vec<Resource>,
    bindings: ResolvedBindings,
    /// Resources whose override was applied from a v7 state (no
    /// recorded original_value). Sub-PR C surfaces this through a warning.
    legacy_overrides_applied: Vec<ResourceId>,
    /// Resources where the DSL value diverged from recorded original_value.
    /// Sub-PR C / plan summary can surface this through the getter.
    skipped_overrides: Vec<ResourceId>,
}

enum ResolutionMode<'a> {
    Strict,
    Plan {
        unresolved_upstream_bindings: &'a HashSet<&'a str>,
    },
}

impl OverrideAwareResources {
    pub fn build<S>(
        unresolved: Vec<Resource>,
        state_file: Option<&S>,
        inputs: PreApplyInputs<'_>,
    ) -> Result<Self, String>
    where
        S: NameOverrideSource + ?Sized,
    {
        Self::build_inner(unresolved, state_file, inputs, ResolutionMode::Strict)
    }

    pub fn build_for_plan<S>(
        unresolved: Vec<Resource>,
        state_file: Option<&S>,
        inputs: PreApplyInputs<'_>,
        unresolved_upstream_bindings: &HashSet<&str>,
    ) -> Result<Self, String>
    where
        S: NameOverrideSource + ?Sized,
    {
        Self::build_inner(
            unresolved,
            state_file,
            inputs,
            ResolutionMode::Plan {
                unresolved_upstream_bindings,
            },
        )
    }

    fn build_inner<S>(
        unresolved: Vec<Resource>,
        state_file: Option<&S>,
        inputs: PreApplyInputs<'_>,
        mode: ResolutionMode<'_>,
    ) -> Result<Self, String>
    where
        S: NameOverrideSource + ?Sized,
    {
        let PreApplyInputs {
            managed: _,
            compositions,
            data_sources,
            current_states,
            remote_bindings,
            wait_aliases,
        } = inputs;

        let mut resources = unresolved.clone();
        let mut bindings = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions,
            data_sources,
            current_states,
            remote_bindings,
            wait_aliases,
        });
        resolve_resources(&mut resources, &bindings, &mode)?;

        let snapshot = resources.clone();
        let mut unresolved_resources = unresolved;
        let mut legacy_overrides_applied = Vec::new();
        let mut skipped_overrides = Vec::new();

        if let Some(state_file) = state_file {
            for ((resource, snapshot_resource), unresolved_resource) in resources
                .iter_mut()
                .zip(snapshot.iter())
                .zip(unresolved_resources.iter_mut())
            {
                let Some(name_overrides) = state_file.name_overrides_for(&resource.id) else {
                    continue;
                };
                for (attribute, override_) in name_overrides {
                    let snapshot_value = snapshot_resource
                        .attributes
                        .get(attribute)
                        .and_then(concrete_string);
                    match should_apply_override(snapshot_value, override_) {
                        ApplyDecision::Apply | ApplyDecision::ApplyWithUnknownDsl => {
                            apply_override(resource, unresolved_resource, attribute, override_);
                        }
                        ApplyDecision::ApplyLegacy => {
                            apply_override(resource, unresolved_resource, attribute, override_);
                            push_unique(&mut legacy_overrides_applied, resource.id.clone());
                        }
                        ApplyDecision::Skip => {
                            push_unique(&mut skipped_overrides, resource.id.clone());
                        }
                    }
                }
            }
        }

        bindings = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions,
            data_sources,
            current_states,
            remote_bindings,
            wait_aliases,
        });
        resolve_resources(&mut resources, &bindings, &mode)?;

        Ok(Self {
            resources,
            unresolved_resources,
            bindings,
            legacy_overrides_applied,
            skipped_overrides,
        })
    }

    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    pub fn resources_mut(&mut self) -> &mut [Resource] {
        &mut self.resources
    }

    pub fn unresolved_resources(&self) -> &[Resource] {
        &self.unresolved_resources
    }

    pub fn bindings(&self) -> &ResolvedBindings {
        &self.bindings
    }

    pub fn legacy_overrides_applied(&self) -> &[ResourceId] {
        &self.legacy_overrides_applied
    }

    pub fn skipped_overrides(&self) -> &[ResourceId] {
        &self.skipped_overrides
    }

    #[cfg(test)]
    pub(crate) fn from_parts_for_tests(
        resources: Vec<Resource>,
        unresolved_resources: Vec<Resource>,
    ) -> Self {
        let compositions = [];
        let data_sources = [];
        let current_states = HashMap::new();
        let remote_bindings = HashMap::new();
        let wait_aliases = [];
        let bindings = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &compositions,
            data_sources: &data_sources,
            current_states: &current_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        });
        Self {
            resources,
            unresolved_resources,
            bindings,
            legacy_overrides_applied: Vec::new(),
            skipped_overrides: Vec::new(),
        }
    }
}

fn resolve_resources(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    mode: &ResolutionMode<'_>,
) -> Result<(), String> {
    match mode {
        ResolutionMode::Strict => {
            crate::resolver::resolve_refs_with_state_and_remote(resources, bindings)
        }
        ResolutionMode::Plan {
            unresolved_upstream_bindings,
        } => crate::resolver::resolve_refs_for_plan(
            resources,
            bindings,
            unresolved_upstream_bindings,
        ),
    }
}

fn concrete_string(value: &Value) -> Option<&str> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn apply_override(
    resource: &mut Resource,
    unresolved_resource: &mut Resource,
    attribute: &str,
    override_: &NameOverride,
) {
    let value = Value::Concrete(ConcreteValue::String(override_.temp_value.clone()));
    resource
        .attributes
        .insert(attribute.to_string(), value.clone());
    unresolved_resource
        .attributes
        .insert(attribute.to_string(), value);
}

fn push_unique(resources: &mut Vec<ResourceId>, id: ResourceId) {
    if !resources.contains(&id) {
        resources.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding_index::WaitAliasSpec;
    use crate::resource::{AccessPath, DeferredValue, PlanInputState};

    fn string(value: impl Into<String>) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    fn resource(binding: &str, name: Value) -> Resource {
        let mut resource = Resource::new("mock.thing", binding);
        resource.binding = Some(binding.to_string());
        resource.attributes.insert("name".to_string(), name);
        resource
    }

    fn resource_ref(binding: &str, attribute: &str) -> Value {
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new(binding, attribute),
        })
    }

    fn overrides(
        entries: Vec<(&ResourceId, &str, NameOverride)>,
    ) -> HashMap<ResourceId, HashMap<String, NameOverride>> {
        let mut overrides = HashMap::new();
        for (id, attribute, override_) in entries {
            overrides
                .entry(id.clone())
                .or_insert_with(HashMap::new)
                .insert(attribute.to_string(), override_);
        }
        overrides
    }

    fn build(
        resources: Vec<Resource>,
        overrides: &HashMap<ResourceId, HashMap<String, NameOverride>>,
    ) -> OverrideAwareResources {
        let compositions = [];
        let data_sources = [];
        let current_states: HashMap<ResourceId, PlanInputState> = HashMap::new();
        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let wait_aliases: Vec<WaitAliasSpec> = Vec::new();

        OverrideAwareResources::build(
            resources,
            Some(overrides),
            PreApplyInputs {
                managed: &[],
                compositions: &compositions,
                data_sources: &data_sources,
                current_states: &current_states,
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        )
        .expect("override-aware resources should build")
    }

    fn resolved_name(resources: &OverrideAwareResources, binding: &str) -> Option<String> {
        resources
            .resources()
            .iter()
            .find(|resource| resource.binding.as_deref() == Some(binding))
            .and_then(|resource| resource.attributes.get("name"))
            .and_then(|value| concrete_string(value).map(ToOwned::to_owned))
    }

    #[test]
    fn chained_cbd_consumer_reading_b_dot_name_resolves_to_b_override() {
        let upstream = resource("a", string("shared-name"));
        let consumer = resource("b", resource_ref("a", "name"));
        let overrides = overrides(vec![
            (
                &upstream.id,
                "name",
                NameOverride {
                    temp_value: "a-cbd".to_string(),
                    original_value: Some("shared-name".to_string()),
                },
            ),
            (
                &consumer.id,
                "name",
                NameOverride {
                    temp_value: "b-cbd".to_string(),
                    original_value: Some("shared-name".to_string()),
                },
            ),
        ]);

        let resources = build(vec![upstream, consumer], &overrides);

        assert_eq!(resolved_name(&resources, "a").as_deref(), Some("a-cbd"));
        assert_eq!(resolved_name(&resources, "b").as_deref(), Some("b-cbd"));
        assert!(resources.skipped_overrides().is_empty());
    }

    #[test]
    fn apply_name_overrides_applies_for_var_substituted_dsl_name() {
        let source = resource("var_foo", string("from-var"));
        let target = resource("target", resource_ref("var_foo", "name"));
        let overrides = overrides(vec![(
            &target.id,
            "name",
            NameOverride {
                temp_value: "target-cbd".to_string(),
                original_value: Some("from-var".to_string()),
            },
        )]);

        let resources = build(vec![source, target], &overrides);

        assert_eq!(
            resolved_name(&resources, "target").as_deref(),
            Some("target-cbd")
        );
        assert!(resources.skipped_overrides().is_empty());
    }

    #[test]
    fn apply_name_overrides_skips_for_ref_substituted_dsl_name_rename() {
        let source = resource("source", string("renamed"));
        let target = resource("target", resource_ref("source", "name"));
        let overrides = overrides(vec![(
            &target.id,
            "name",
            NameOverride {
                temp_value: "target-cbd".to_string(),
                original_value: Some("old-name".to_string()),
            },
        )]);

        let resources = build(vec![source, target.clone()], &overrides);

        assert_eq!(
            resolved_name(&resources, "target").as_deref(),
            Some("renamed")
        );
        assert_eq!(resources.skipped_overrides(), &[target.id]);
    }

    #[test]
    fn override_aware_resources_records_legacy_and_skipped() {
        let legacy = resource("legacy", string("legacy-current"));
        let skipped = resource("skipped", string("skipped-current"));
        let overrides = overrides(vec![
            (
                &legacy.id,
                "name",
                NameOverride {
                    temp_value: "legacy-cbd".to_string(),
                    original_value: None,
                },
            ),
            (
                &skipped.id,
                "name",
                NameOverride {
                    temp_value: "skipped-cbd".to_string(),
                    original_value: Some("recorded-original".to_string()),
                },
            ),
        ]);

        let resources = build(vec![legacy.clone(), skipped.clone()], &overrides);

        assert_eq!(resources.legacy_overrides_applied(), &[legacy.id]);
        assert_eq!(resources.skipped_overrides(), &[skipped.id]);
        assert_eq!(
            resolved_name(&resources, "legacy").as_deref(),
            Some("legacy-cbd")
        );
        assert_eq!(
            resolved_name(&resources, "skipped").as_deref(),
            Some("skipped-current")
        );
    }
}
