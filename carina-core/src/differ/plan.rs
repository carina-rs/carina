//! Plan generation from diffs and cascading update logic.

use std::collections::{HashMap, HashSet};

use crate::deps::get_resource_dependencies;

use crate::effect::{ChangedCreateOnly, Effect, OutcomeUnknownImportReplacement, TemporaryName};
use crate::identifier::generate_random_suffix;
use crate::parser::WaitBinding;
use crate::plan::{
    CreateBeforeDestroyError, PermanentNameOverride, Plan, PlanError, PlanErrorKind,
    PreventDestroyAction, ReplacementCannotCoexistError, ReplacementDelete, ReplacementGroup,
    SchemaNotRegisteredError,
};
use crate::provider::{LiftedSavedAttrs, Provider, ProviderResult, ProviderRouter};
use crate::resource::{
    ConcreteValue, Current, DataSource, Directives, PlanInputCurrent, ResolvedDataSource,
    ResolvedResource, ResolvedResourceId, Resource, ResourceId, ResourceIdentity, State, Value,
};
use crate::schema::{
    AttrTypeKind, AttributeType, ResourceSchema, SchemaKind, SchemaRegistry, UniqueNameSpec,
    WAIT_DEFAULT_INTERVAL, WAIT_DEFAULT_TIMEOUT,
};
use crate::wait::augment::satisfier_augmentation;
use crate::wait::predicate::resolve_wait_predicate;
use crate::wait::{WaitTargetLookup, resolve_wait_target};

use super::{
    ComparisonContext, PlanPriorStates, PlannedListKeyRelease, PresentDiff,
    diff_present_with_releases, type_aware_equal,
};

fn add_key_releases(plan: &mut Plan, id: &ResourceId, releases: Vec<PlannedListKeyRelease>) {
    for release in releases {
        plan.add(Effect::ReleaseListKeys {
            id: ResolvedResourceId::new(id.clone()),
            attribute: release.attribute,
            key_field: release.key_field,
            keys: release.keys,
        });
    }
}

pub(crate) struct PendingReplace {
    pub create: ResolvedResource,
    pub delete: ReplacementDelete,
    pub create_before_destroy: bool,
    pub changed_create_only: ChangedCreateOnly,
    pub cascade_ref_hints: Vec<(String, String)>,
    pub temporary_name: Option<TemporaryName>,
    pub consumer_updates: HashSet<ResourceIdentity>,
    pub previous_attributes: HashMap<String, Value>,
}

struct RefAttr {
    attribute: String,
    hint: String,
}

fn contains_authored_create_only_value<'schema>(
    attr_type: &'schema AttributeType,
    value: &Value,
    schema: &'schema ResourceSchema,
    ref_chain: &mut Vec<&'schema str>,
) -> bool {
    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if ref_chain.contains(&name.as_str()) {
                return false;
            }
            let Some(definition) = schema.defs.get(name) else {
                return false;
            };
            ref_chain.push(name);
            let contains =
                contains_authored_create_only_value(definition, value, schema, ref_chain);
            ref_chain.pop();
            contains
        }
        AttrTypeKind::List { element_type, .. } => {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return false;
            };
            items.iter().any(|item| {
                contains_authored_create_only_value(element_type, item, schema, ref_chain)
            })
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return false;
            };
            entries.values().any(|entry| {
                contains_authored_create_only_value(value_type, entry, schema, ref_chain)
            })
        }
        AttrTypeKind::Struct { fields, .. } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return false;
            };
            fields.iter().any(|field| {
                entries.get(&field.name).is_some_and(|field_value| {
                    field.create_only
                        || contains_authored_create_only_value(
                            &field.field_type,
                            field_value,
                            schema,
                            ref_chain,
                        )
                })
            })
        }
        AttrTypeKind::Union(_)
        | AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => false,
    }
}

struct UnorderedCreateOnlyMatcher<'items, 'schema, 'context, 'comparison> {
    desired_items: &'items [&'items Value],
    current_items: &'items [Value],
    element_type: &'schema AttributeType,
    schema: &'schema ResourceSchema,
    context: &'context ComparisonContext<'comparison>,
    current_matches: &'items mut [Option<usize>],
}

impl UnorderedCreateOnlyMatcher<'_, '_, '_, '_> {
    fn matches(&mut self, desired_index: usize, seen_current: &mut [bool]) -> bool {
        for current_index in 0..self.current_items.len() {
            if seen_current[current_index]
                || !create_only_values_compatible(
                    self.element_type,
                    &self.current_items[current_index],
                    self.desired_items[desired_index],
                    self.schema,
                    self.context,
                    &mut Vec::new(),
                )
            {
                continue;
            }
            seen_current[current_index] = true;
            if self.current_matches[current_index]
                .is_none_or(|previous_desired| self.matches(previous_desired, seen_current))
            {
                self.current_matches[current_index] = Some(desired_index);
                return true;
            }
        }
        false
    }
}

fn create_only_values_compatible<'schema>(
    attr_type: &'schema AttributeType,
    current: &Value,
    desired: &Value,
    schema: &'schema ResourceSchema,
    context: &ComparisonContext<'_>,
    ref_chain: &mut Vec<&'schema str>,
) -> bool {
    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if ref_chain.contains(&name.as_str()) {
                return true;
            }
            let Some(definition) = schema.defs.get(name) else {
                return true;
            };
            ref_chain.push(name);
            let compatible = create_only_values_compatible(
                definition, current, desired, schema, context, ref_chain,
            );
            ref_chain.pop();
            compatible
        }
        AttrTypeKind::List {
            element_type,
            ordered,
            ..
        } => {
            let Value::Concrete(ConcreteValue::List(desired_items)) = desired else {
                return true;
            };
            let Value::Concrete(ConcreteValue::List(current_items)) = current else {
                return !desired_items.iter().any(|item| {
                    contains_authored_create_only_value(element_type, item, schema, &mut Vec::new())
                });
            };
            if *ordered {
                desired_items.iter().enumerate().all(|(index, desired)| {
                    !contains_authored_create_only_value(
                        element_type,
                        desired,
                        schema,
                        &mut Vec::new(),
                    ) || current_items.get(index).is_some_and(|current| {
                        create_only_values_compatible(
                            element_type,
                            current,
                            desired,
                            schema,
                            context,
                            &mut Vec::new(),
                        )
                    })
                })
            } else {
                let desired_items = desired_items
                    .iter()
                    .filter(|item| {
                        contains_authored_create_only_value(
                            element_type,
                            item,
                            schema,
                            &mut Vec::new(),
                        )
                    })
                    .collect::<Vec<_>>();
                let mut current_matches = vec![None; current_items.len()];
                let mut matcher = UnorderedCreateOnlyMatcher {
                    desired_items: &desired_items,
                    current_items,
                    element_type,
                    schema,
                    context,
                    current_matches: &mut current_matches,
                };
                (0..desired_items.len()).all(|desired_index| {
                    matcher.matches(desired_index, &mut vec![false; current_items.len()])
                })
            }
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            let Value::Concrete(ConcreteValue::Map(desired_entries)) = desired else {
                return true;
            };
            let Value::Concrete(ConcreteValue::Map(current_entries)) = current else {
                return !desired_entries.values().any(|entry| {
                    contains_authored_create_only_value(value_type, entry, schema, &mut Vec::new())
                });
            };
            desired_entries.iter().all(|(key, desired)| {
                !contains_authored_create_only_value(value_type, desired, schema, &mut Vec::new())
                    || current_entries.get(key).is_some_and(|current| {
                        create_only_values_compatible(
                            value_type,
                            current,
                            desired,
                            schema,
                            context,
                            &mut Vec::new(),
                        )
                    })
            })
        }
        AttrTypeKind::Struct { fields, .. } => {
            let Value::Concrete(ConcreteValue::Map(desired_fields)) = desired else {
                return true;
            };
            let Value::Concrete(ConcreteValue::Map(current_fields)) = current else {
                return !contains_authored_create_only_value(
                    attr_type,
                    desired,
                    schema,
                    &mut Vec::new(),
                );
            };
            fields.iter().all(|field| {
                let Some(desired) = desired_fields.get(&field.name) else {
                    return true;
                };
                if field.create_only {
                    return current_fields.get(&field.name).is_some_and(|current| {
                        type_aware_equal(
                            current,
                            desired,
                            Some(&field.field_type),
                            &schema.defs,
                            context,
                        )
                    });
                }
                !contains_authored_create_only_value(
                    &field.field_type,
                    desired,
                    schema,
                    &mut Vec::new(),
                ) || current_fields.get(&field.name).is_some_and(|current| {
                    create_only_values_compatible(
                        &field.field_type,
                        current,
                        desired,
                        schema,
                        context,
                        &mut Vec::new(),
                    )
                })
            })
        }
        AttrTypeKind::Union(_)
        | AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => true,
    }
}

pub(super) fn find_changed_create_only(
    changed_attributes: &[String],
    from: &State,
    to: &Resource,
    schema: &ResourceSchema,
    context: &ComparisonContext<'_>,
) -> Vec<String> {
    let top_level = schema.create_only_attributes();
    changed_attributes
        .iter()
        .filter(|attribute| {
            if top_level.contains(&attribute.as_str()) {
                return true;
            }

            let Some(desired) = to.attributes.get(attribute.as_str()) else {
                return false;
            };
            let Some(attribute_schema) = schema.attributes().get(attribute.as_str()) else {
                return false;
            };
            let Some(current) = from.attributes.get(attribute.as_str()) else {
                return contains_authored_create_only_value(
                    attribute_schema.attr_type(),
                    desired,
                    schema,
                    &mut Vec::new(),
                );
            };
            !create_only_values_compatible(
                attribute_schema.attr_type(),
                current,
                desired,
                schema,
                context,
                &mut Vec::new(),
            )
        })
        .cloned()
        .collect()
}

fn cascade_ref_attrs(
    attrs: &indexmap::IndexMap<String, Value>,
    target_binding: &str,
) -> Vec<RefAttr> {
    attrs
        .iter()
        .filter_map(|(attribute, value)| {
            cascade_ref_hint(value, target_binding).map(|hint| RefAttr {
                attribute: attribute.clone(),
                hint,
            })
        })
        .collect()
}

fn cascade_ref_hint(value: &Value, target_binding: &str) -> Option<String> {
    let mut hit: Option<String> = None;

    // Multi-ref attributes report the first target-binding ref in list/IndexMap/source order.
    value.visit_resource_refs(&mut |path| {
        if hit.is_none() && path.binding() == target_binding {
            hit = Some(format!("{}.{}", path.binding(), path.attribute()));
        }
    });

    if hit.is_some() {
        return hit;
    }

    value.visit_binding_refs(&mut |binding| {
        if hit.is_none() && binding == target_binding {
            hit = Some(binding.to_string());
        }
    });

    hit
}

/// Filter out non-removable attribute removals from the changed list.
/// A "removal" is an attribute that appears in `changed_attributes` but not in `to`.
/// Only attributes marked as `removable` in the schema should be kept as removals.
/// Non-removal changes (attribute present in `to`) are always kept.
pub(super) fn filter_non_removable_removals(
    provider: &str,
    resource_type: &str,
    to: &Resource,
    changed_attributes: Vec<String>,
    registry: &SchemaRegistry,
) -> Vec<String> {
    let Some(schema) = registry.get(provider, resource_type, SchemaKind::Resource) else {
        // No schema available — keep all changes (conservative)
        return changed_attributes;
    };
    let schema = schema.as_schema();

    let removable_attrs = schema.removable_attributes();

    changed_attributes
        .into_iter()
        .filter(|attr| {
            // Keep if the attribute is still in desired (it's a change, not a removal)
            if to.attributes.contains_key(attr) {
                return true;
            }
            // It's a removal — only keep if the attribute is removable
            removable_attrs.contains(&attr.as_str())
        })
        .collect()
}

/// Generate a temporary name for create-before-destroy replacement.
///
/// `Conflicting` resources are a plan-time error on every CBD path.
/// Other cases can still legitimately avoid a temporary name: a `Coexisting`
/// resource has no server-side create-first conflict, a `name_prefix` already
/// produces a non-conflicting cloud name, and a direct change to the
/// unique-name value is already distinct from the old resource.
pub fn generate_temporary_name(
    resource: &Resource,
    from: &State,
    schema: &ResourceSchema,
) -> Result<Option<TemporaryName>, CreateBeforeDestroyError> {
    let name_attr = match &schema.unique_name {
        UniqueNameSpec::Attribute(name_attr) => name_attr,
        UniqueNameSpec::Coexisting => return Ok(None),
        UniqueNameSpec::Conflicting => {
            return Err(ReplacementCannotCoexistError {
                resource_type: resource.id.display_type(),
                resource_identity: resource
                    .binding
                    .clone()
                    .unwrap_or_else(|| resource.id.identity_display().to_string()),
            }
            .into());
        }
    };

    // Skip if the resource uses name_prefix for this attribute
    if resource.prefixes.contains_key(name_attr) {
        return Ok(None);
    }

    // Get the current value of the unique-name attribute.
    let original_value = match resource.get_attr(name_attr) {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.clone(),
        _ => return Ok(None),
    };

    // Skip if the unique_name_attribute value changed (new name is already different from old)
    if let Some(Value::Concrete(ConcreteValue::String(from_name))) = from.attributes.get(name_attr)
        && *from_name != original_value
    {
        return Ok(None);
    }

    // Check if the unique-name attribute is create-only (cannot be renamed after creation).
    let can_rename = schema
        .attributes
        .get(name_attr)
        .map(|attr| !attr.create_only)
        .unwrap_or(false);

    let temporary_value = format!(
        "{}-{}",
        original_value.expose_plaintext(),
        generate_random_suffix()
    );

    Ok(Some(TemporaryName {
        attribute: name_attr.clone(),
        original_value: original_value.into_plaintext(),
        temporary_value,
        can_rename,
    }))
}

/// Compute Diff for multiple resources and generate a Plan
///
/// The `directives_map` provides prior directives for desired resources so
/// update-before-delete ordering can recover removed `depends_on` edges.
/// Orphans carry their own directives alongside their provider state in the
/// typed `orphans` input.
///
/// The `saved_attrs` map provides the last-known attribute values from the state file.
/// This is used to merge unmanaged nested fields into desired values before comparison,
/// preventing false diffs when AWS returns extra fields not specified in the .crn file.
///
/// The `prev_explicit` map provides the per-resource authoring tree that
/// the user wrote in their `.crn` during the last apply. The differ uses
/// it both to project the actual-state side (hiding server-side defaults
/// the user never wrote, refs awscc#206) and to detect attribute
/// removals: if a top-level key was previously in the user's desired
/// state but is now absent, it means the user intentionally removed it.
///
/// # Typestate invariants
///
/// compositions are intentionally not an input. The compile-fail doctest
/// below pins that — passing a [`LiveComposition`](crate::resource::LiveComposition)
/// slice must fail to type-check, so the post-apply-only composition class
/// (carina#3169) cannot accidentally reach pre-apply differ logic.
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::differ::{Orphans, PlanPriorStates, create_plan};
/// use carina_core::provider::RawSavedAttrs;
/// use carina_core::resource::{LiveComposition, Current, ResourceId, into_plan_input_current_map};
/// use carina_core::schema::SchemaRegistry;
/// let compositions: Vec<LiveComposition> = vec![];
/// let registry = SchemaRegistry::default();
/// let saved_attrs = RawSavedAttrs::default().lift(&registry);
/// let states = HashMap::<ResourceId, Current>::new();
/// let plan_states = into_plan_input_current_map(states, &registry, &[]);
/// let orphans = Orphans::new();
/// let _ = create_plan(
///     &compositions,
///     &[],
///     &carina_core::provider::ProviderRouter::new(),
///     PlanPriorStates::new(&plan_states, &orphans),
///     &HashMap::new(),
///     &registry,
///     &saved_attrs,
///     &HashMap::new(),
///     &[],
/// );
/// ```
///
/// Raw [`State`] maps cannot be passed directly to planning. Callers must pair
/// planning-ready managed current states and typed orphans in [`PlanPriorStates`],
/// which lets the differ own same-id orphan promotion.
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::differ::create_plan;
/// use carina_core::provider::{ProviderRouter, RawSavedAttrs};
/// use carina_core::resource::{ResolvedResource, ResourceId, State};
/// use carina_core::schema::SchemaRegistry;
///
/// let resources: Vec<ResolvedResource> = vec![];
/// let states: HashMap<ResourceId, State> = HashMap::new();
/// let registry = SchemaRegistry::default();
/// let saved_attrs = RawSavedAttrs::default().lift(&registry);
/// let _ = create_plan(
///     &resources,
///     &[],
///     &ProviderRouter::new(),
///     &states,
///     &HashMap::new(),
///     &registry,
///     &saved_attrs,
///     &HashMap::new(),
///     &[],
/// );
/// ```
///
/// Persisted [`RawSavedAttrs`](crate::provider::RawSavedAttrs) cannot reach
/// the differ before schema lifting.
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::differ::{Orphans, PlanPriorStates, create_plan};
/// use carina_core::provider::{ProviderRouter, RawSavedAttrs};
/// use carina_core::resource::{Current, ResolvedResource, ResourceId, into_plan_input_current_map};
/// use carina_core::schema::SchemaRegistry;
///
/// let resources: Vec<ResolvedResource> = vec![];
/// let states = HashMap::<ResourceId, Current>::new();
/// let orphans = Orphans::new();
/// let registry = SchemaRegistry::default();
/// let plan_states = into_plan_input_current_map(states, &registry, &[]);
/// let raw_saved_attrs = RawSavedAttrs::default();
/// let _ = create_plan(
///     &resources,
///     &[],
///     &ProviderRouter::new(),
///     PlanPriorStates::new(&plan_states, &orphans),
///     &HashMap::new(),
///     &registry,
///     &raw_saved_attrs,
///     &HashMap::new(),
///     &[],
/// );
/// ```
#[allow(clippy::too_many_arguments)]
pub fn create_plan(
    managed: &[ResolvedResource],
    data_sources: &[DataSource],
    provider: &ProviderRouter,
    prior_states: PlanPriorStates<'_>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    wait_bindings: &[WaitBinding],
) -> ProviderResult<Plan> {
    create_plan_with_provider(
        managed,
        data_sources,
        provider,
        provider,
        prior_states,
        directives_map,
        registry,
        saved_attrs,
        prev_explicit,
        wait_bindings,
    )
}

/// Test-only entry point for focused provider fixtures that do not model an
/// initialized provider instance. Comparisons use an unavailable scope.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn create_plan_for_test(
    managed: &[ResolvedResource],
    data_sources: &[DataSource],
    provider: &dyn crate::provider::Provider,
    prior_states: PlanPriorStates<'_>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    wait_bindings: &[WaitBinding],
) -> ProviderResult<Plan> {
    let (registry, comparison_provider) = crate::provider::comparison_fixture_for_test(
        registry,
        managed.iter().map(|resource| &resource.id),
    );
    create_plan_with_provider(
        managed,
        data_sources,
        provider,
        &comparison_provider,
        prior_states,
        directives_map,
        &registry,
        saved_attrs,
        prev_explicit,
        wait_bindings,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_plan_with_provider(
    managed: &[ResolvedResource],
    data_sources: &[DataSource],
    provider: &dyn Provider,
    comparison_provider: &ProviderRouter,
    prior_states: PlanPriorStates<'_>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    wait_bindings: &[WaitBinding],
) -> ProviderResult<Plan> {
    let managed_current = prior_states.materialize_current_states(managed, registry);
    let mut build = create_plan_parts(
        managed,
        data_sources,
        provider,
        comparison_provider,
        &managed_current,
        registry,
        saved_attrs,
        prev_explicit,
        prior_states.orphans(),
        wait_bindings,
        &HashSet::new(),
    )?;
    decompose_replace_into_effects(&mut build.plan, build.pending_replaces);
    block_deletes_on_prior_consumer_updates(&mut build.plan, directives_map);
    Ok(build.plan)
}

#[allow(clippy::too_many_arguments)]
pub fn create_plan_with_cascades(
    managed: &[ResolvedResource],
    unresolved_managed: &[ResolvedResource],
    data_sources: &[DataSource],
    provider: &ProviderRouter,
    prior_states: PlanPriorStates<'_>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    wait_bindings: &[WaitBinding],
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> ProviderResult<Plan> {
    let managed_current = prior_states.materialize_current_states(managed, registry);
    let mut build = create_plan_parts(
        managed,
        data_sources,
        provider,
        provider,
        &managed_current,
        registry,
        saved_attrs,
        prev_explicit,
        prior_states.orphans(),
        wait_bindings,
        outcome_unknown_import_replacements,
    )?;
    cascade_dependent_updates(
        &mut build.plan,
        &mut build.pending_replaces,
        unresolved_managed,
        &managed_current,
        registry,
        provider,
        outcome_unknown_import_replacements,
    )?;
    decompose_replace_into_effects(&mut build.plan, build.pending_replaces);
    block_deletes_on_prior_consumer_updates(&mut build.plan, directives_map);
    Ok(build.plan)
}

struct PlanBuild {
    plan: Plan,
    pending_replaces: HashMap<ResourceIdentity, PendingReplace>,
}

#[allow(clippy::too_many_arguments)]
fn create_plan_parts(
    managed: &[ResolvedResource],
    data_sources: &[DataSource],
    provider: &dyn Provider,
    comparison_provider: &ProviderRouter,
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved_attrs: &LiftedSavedAttrs,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphans: &super::Orphans,
    wait_bindings: &[WaitBinding],
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> ProviderResult<PlanBuild> {
    let mut plan = Plan::new();
    let mut pending_replaces = HashMap::new();
    let known_bindings = known_binding_names(managed, data_sources);

    // Data sources only generate Read effects; the typestate split
    // routes them through their own slice so the legacy
    // `if resource.is_data_source() { continue; }` runtime guard is no
    // longer reachable from this loop (carina#3179).
    for ds in data_sources {
        plan.add(Effect::Read {
            resource: ResolvedDataSource::new(ds.clone()),
        });
    }

    for resource in managed {
        let current = current_states
            .get(&resource.id)
            .cloned()
            .unwrap_or_else(|| PlanInputCurrent::absent(resource.id.clone()));
        let saved = saved_attrs.get(&resource.id);
        let prev_explicit_for_resource = prev_explicit.get(&resource.id);
        match current.current() {
            Current::Absent(_absent) => plan.add(Effect::Create(resource.clone())),
            Current::CreateInFlight(pending) => {
                if pending.id() == &resource.id {
                    plan.add_resume_create(resource.clone(), pending.started_at().clone());
                } else {
                    plan.add_error(PlanError::new(
                        resource.id.clone(),
                        PlanErrorKind::ResumeCreateRoutingMismatch {
                            persisted: pending.id().clone(),
                        },
                    ));
                }
            }
            Current::CreateOutcomeUnknown(unknown) => {
                add_outcome_unknown_error(
                    &mut plan,
                    unknown.id(),
                    outcome_unknown_import_replacements,
                );
            }
            Current::MissingIdentifier(_unidentified) => {
                plan.add_error(PlanError::new(
                    resource.id.clone(),
                    PlanErrorKind::MissingProviderIdentifier,
                ));
            }
            Current::Present(present) => {
                let resolved_comparison =
                    comparison_provider.comparison_for(registry, &resource.id)?;
                let owner_schema = resolved_comparison.owner_schema();
                let context = resolved_comparison.context();
                let validated_schema = owner_schema.schema();
                let schema = validated_schema.as_schema();
                let result = diff_present_with_releases(
                    resource,
                    present,
                    saved,
                    saved_attrs.nested_write_only(&resource.id),
                    prev_explicit_for_resource,
                    validated_schema,
                    context,
                );
                match result.diff {
                    PresentDiff::Update {
                        id,
                        from,
                        to,
                        changed_attributes,
                        ignored_readable_differences,
                    } => {
                        plan.record_ignored_readable_differences(
                            id.clone(),
                            ignored_readable_differences,
                        );
                        let to = *to;
                        // Filter out non-removable attribute removals.
                        // A "removal" is an attribute in changed_attributes that is not in `to`.
                        // Only attributes marked as `removable` in the schema should trigger removal.
                        let changed_attributes = filter_non_removable_removals(
                            &resource.id.provider,
                            &resource.id.resource_type,
                            &to,
                            changed_attributes,
                            registry,
                        );

                        if changed_attributes.is_empty() {
                            add_key_releases(&mut plan, &resource.id, result.releases);
                            continue;
                        }

                        if !resource.ignore_changes().is_empty()
                            && let Err(error) = crate::provider::plan_update_patch(
                                &changed_attributes,
                                resource,
                                &from,
                                schema,
                            )
                        {
                            plan.add_error(PlanError::new(id, error.into()));
                            continue;
                        }

                        let identifier = present.identifier().clone();
                        let changed_create_only = find_changed_create_only(
                            &changed_attributes,
                            &from,
                            &to,
                            schema,
                            context,
                        );

                        if let Some(changed_create_only) =
                            ChangedCreateOnly::new(changed_create_only)
                        {
                            let prevent_destroy_source = owner_schema
                                .resolve_directives(&resource.directives)
                                .prevent_destroy_source();
                            if let Some(source) = prevent_destroy_source {
                                plan.add_error(PlanError::prevent_destroy(
                                    id.clone(),
                                    PreventDestroyAction::Replace,
                                    source,
                                ));
                                continue;
                            }
                            match pending_replace_from_parts(
                                from,
                                identifier,
                                to,
                                resource.directives.clone(),
                                changed_create_only,
                                Vec::new(),
                                registry,
                            ) {
                                Ok(pending) => {
                                    pending_replaces
                                        .insert(resource_identity(&pending.create), pending);
                                }
                                Err(err) => plan.add_error(PlanError::new(id, err.into())),
                            }
                        } else {
                            plan.add(Effect::Update {
                                from,
                                identifier,
                                to: ResolvedResource::new(to),
                                changed_attributes,
                            });
                            add_key_releases(&mut plan, &resource.id, result.releases);
                        }
                    }
                    PresentDiff::NoChange {
                        ignored_readable_differences,
                    } => {
                        plan.record_ignored_readable_differences(
                            resource.id.clone(),
                            ignored_readable_differences,
                        );
                        add_key_releases(&mut plan, &resource.id, result.releases);
                    }
                }
            }
        }
    }

    // Same-id managed orphans were materialized into the managed
    // `PlanInputCurrent` map by `PlanPriorStates`; skip them here so only
    // still-unmanaged orphans emit Deletes. Binding, directives, and provider
    // state remain paired.
    let managed_ids: HashSet<&ResourceId> = managed.iter().map(|resource| &resource.id).collect();
    for (id, orphan) in orphans {
        if managed_ids.contains(id) {
            continue;
        }
        let (state, identifier) = match orphan.current() {
            Current::Absent(_state) => continue,
            Current::Present(present) => (present.state(), present.identifier().clone()),
            Current::CreateInFlight(_pending) => {
                plan.add_error(PlanError::new(
                    id.clone(),
                    PlanErrorKind::CreateInFlightOrphan,
                ));
                continue;
            }
            Current::CreateOutcomeUnknown(unknown) => {
                add_outcome_unknown_error(
                    &mut plan,
                    unknown.id(),
                    outcome_unknown_import_replacements,
                );
                continue;
            }
            Current::MissingIdentifier(_state) => {
                plan.add_error(PlanError::new(
                    id.clone(),
                    PlanErrorKind::MissingProviderIdentifier,
                ));
                continue;
            }
        };
        let directives = orphan.directives().clone();
        let resolved_directives =
            registry.resolve_directives(id, SchemaKind::Resource, &directives);
        let prevent_destroy_source = resolved_directives.prevent_destroy_source();
        if let Some(source) = prevent_destroy_source {
            plan.add_error(PlanError::prevent_destroy(
                id.clone(),
                PreventDestroyAction::DeleteRemovedFromConfiguration,
                source,
            ));
            continue;
        }
        let binding = orphan.binding().map(str::to_owned);
        let dependencies = dependencies_from_prior_state(state, directives.clone());
        let explicit_dependencies = directives.depends_on.iter().cloned().collect();
        plan.add(Effect::Delete {
            id: ResolvedResourceId::new(id.clone()),
            identifier,
            generation: crate::effect::EffectGeneration::Current,
            directives,
            binding,
            dependencies,
            explicit_dependencies,
            blocked_by_updates: HashSet::new(),
        });
    }

    // Lower each `wait` declaration into an `Effect::Wait`.
    //
    // Target resolution: the parser has already pinned the target name;
    // here we look it up in `desired` (the live resource set after
    // `resolve_resource_refs`) to recover the resolved `ResourceId`. If
    // the target is missing — typo, scoped-out binding, etc. — the
    // wait is skipped with a plan-level error so downstream resources
    // referencing the wait binding still fail loudly.
    for wb in wait_bindings {
        let target = match resolve_wait_target(wb.target.as_str(), managed, data_sources) {
            WaitTargetLookup::Managed(target) => target,
            WaitTargetLookup::DataSource => {
                plan.add_error(PlanError::new(
                    ResourceId::with_identity(
                        "__wait",
                        ResourceIdentity::new(wb.binding.to_string()),
                    ),
                    PlanErrorKind::WaitTargetIsDataSource {
                        wait_binding: wb.binding.to_string(),
                        target: wb.target.to_string(),
                    },
                ));
                continue;
            }
            WaitTargetLookup::Missing => {
                plan.add_error(PlanError::new(
                    ResourceId::with_identity(
                        "__wait",
                        ResourceIdentity::new(wb.binding.to_string()),
                    ),
                    PlanErrorKind::WaitTargetMissing {
                        wait_binding: wb.binding.to_string(),
                        target: wb.target.to_string(),
                    },
                ));
                continue;
            }
        };
        let target_id = target.id();
        let diagnostic_id = target_id.as_inner().clone();

        // Once the managed target is known, resolve the full nested attribute
        // path. The same function drives validation (and therefore LSP
        // diagnostics), so resolution and wording cannot drift between those
        // entry points.
        let schema = registry.get(
            target_id.provider(),
            target_id.resource_type(),
            SchemaKind::Resource,
        );
        let until = match resolve_wait_predicate(
            &wb.until_predicate.lhs_segments,
            wb.until_predicate.rhs.clone(),
            schema.as_deref(),
        ) {
            Ok(until) => until,
            Err(error) => {
                plan.add_error(PlanError::new(
                    diagnostic_id,
                    PlanErrorKind::WaitPredicateInvalid {
                        wait_binding: wb.binding.to_string(),
                        reason: error.to_string(),
                    },
                ));
                continue;
            }
        };
        let attr = until
            .watched_attrs()
            .into_iter()
            .next()
            .expect("Equals wait predicate must expose its attribute")
            .clone();
        // A `wait`'s only purpose is to gate downstream resources that
        // reference `<wait-binding>.<attr>` until the `until` predicate
        // holds (design value semantics: consumers that don't need the
        // wait reference the target directly and skip it). So emit the
        // `Effect::Wait` only when at least one resource that depends
        // on this wait binding has a pending infrastructure change in
        // this plan. If every consumer is unchanged (or there is no
        // consumer at all), the wait gates nothing — emitting it would
        // render a lone `> <binding> (until …)` header on a no-change
        // plan and poll the target on `apply` for no reason
        // (carina#3101). A genuinely pending consumer change still
        // produces the wait + its dependency edge (carina#3085 /
        // carina#3061 behavior preserved).
        let gates_a_pending_change = managed.iter().any(|m| {
            get_resource_dependencies(m).contains(wb.binding.as_str())
                && plan
                    .effects()
                    .iter()
                    .any(|e| e.resource_id() == &m.id && e.is_mutating())
        }) || data_sources.iter().any(|d| {
            crate::deps::get_data_source_dependencies(d).contains(wb.binding.as_str())
                && plan
                    .effects()
                    .iter()
                    .any(|e| e.resource_id() == &d.id && e.is_mutating())
        });
        if !gates_a_pending_change {
            continue;
        }
        let schema_timeout = schema.and_then(|s| s.as_schema().default_wait_timeout);
        let schema_interval = schema.and_then(|s| s.as_schema().default_wait_interval);
        let timeout = wb
            .timeout_secs
            .map(std::time::Duration::from_secs)
            .or(schema_timeout)
            .unwrap_or(WAIT_DEFAULT_TIMEOUT);
        let interval = schema_interval.unwrap_or(WAIT_DEFAULT_INTERVAL);
        // A gated wait is still elided when it has no work to do.
        // carina#3101 covers "no consumer is changing ⇒ the wait gates
        // nothing ⇒ elide". carina#3358 extends that: even when a
        // consumer *is* changing (so `gates_a_pending_change` is true),
        // an unchanged target whose cached state already satisfies the
        // `until` predicate gives the wait nothing to do — on `apply` it
        // would poll-and-return immediately. Dragging such a wait into
        // the plan is pure noise (a `> <binding> (until …)` node with no
        // pending work), so suppress it. The wait *does* have work when:
        //   1. the target has no state entry (typically created this
        //      run) — its post-apply attributes are unknown, must poll;
        //   2. the target has a mutating effect in this plan — cached
        //      attributes are stale, must re-poll; or
        //   3. the target is unchanged but its cached state does not yet
        //      satisfy the predicate (missing state ⇒ treat as work).
        let cached_state =
            current_states
                .get(target_id.as_inner())
                .and_then(|current| match current.current() {
                    Current::Absent(_absent) => None,
                    Current::Present(present) => Some(present.state()),
                    Current::CreateInFlight(_pending) => None,
                    Current::CreateOutcomeUnknown(_unknown) => None,
                    Current::MissingIdentifier(unidentified) => Some(unidentified.state()),
                });
        let wait_has_work = cached_state
            .map(|state| {
                let target_is_mutating = plan
                    .effects()
                    .iter()
                    .any(|e| e.resource_id() == target_id.as_inner() && e.is_mutating());
                let target_needs_wait = !until.evaluate(&state.attributes);

                target_is_mutating || target_needs_wait
            })
            .unwrap_or(true);
        if !wait_has_work {
            continue;
        }
        let mut explicit_dependencies: HashSet<String> = wb
            .depends_on
            .iter()
            .map(|d| d.as_str().to_string())
            .collect();
        // Provider hints supplement user-authored ordering. They are additive
        // because `depends_on` is the user's source of truth and must never be
        // weakened by provider-derived knowledge.
        explicit_dependencies.extend(satisfier_augmentation(
            provider,
            target_id.as_inner(),
            &attr,
            &known_bindings,
        )?);

        plan.add(Effect::Wait {
            identity: ResourceIdentity::new(wb.binding.as_str()),
            target_id: target,
            until,
            until_surface: wb.until_raw.clone(),
            timeout,
            interval,
            explicit_dependencies,
        });
    }

    Ok(PlanBuild {
        plan,
        pending_replaces,
    })
}

/// Add apply-time ordering from consumer updates to deletes using each consumer's prior
/// dependencies.
///
/// The reference is absent from the new desired resource when a user removes it, so the desired
/// dependency graph cannot recover this edge. `Effect::Update::from` preserves prior value-ref
/// bindings, while `prior_directives` restores prior `depends_on`-only bindings. Matching their
/// union against each delete's old binding gives the inverse edge needed by
/// `Effect::Delete::blocked_by_updates`.
///
/// A new inverse edge is skipped when the desired consumer still depends on the delete binding.
/// The guard is required because:
///
/// 1. a delete-before-create replacement with a still-referencing consumer would otherwise form
///    `Update(consumer) -> Create(producer) -> Delete(producer) -> Update(consumer)`;
/// 2. a binding reused by a declaration that produces no effect can make the scheduler's
///    create-or-delete lookup choose the delete and close the same cycle; and
/// 3. still-referencing consumers of create-before-destroy replacements are already ordered by
///    the replacement machinery's `consumer_updates`.
///
/// Existing `blocked_by_updates` entries are preserved; this pass only adds missing inverse edges.
pub fn block_deletes_on_prior_consumer_updates(
    plan: &mut Plan,
    prior_directives: &HashMap<ResourceId, Directives>,
) {
    let consumers: Vec<(ResourceIdentity, HashSet<String>, HashSet<String>)> = plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Update { from, to, .. } => Some((
                resource_identity(to),
                dependencies_from_prior_state(
                    from,
                    prior_directives.get(&from.id).cloned().unwrap_or_default(),
                ),
                get_resource_dependencies(to.as_resource()),
            )),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => None,
        })
        .collect();

    for effect in plan.effects_mut() {
        let Effect::Delete {
            id,
            binding,
            blocked_by_updates,
            ..
        } = effect
        else {
            continue;
        };
        let delete_identity = id.identity();
        for (consumer_identity, prior_dependencies, desired_dependencies) in &consumers {
            if consumer_identity == delete_identity {
                continue;
            }
            let should_add_inverse_edge = binding.as_ref().is_some_and(|binding| {
                prior_dependencies.contains(binding) && !desired_dependencies.contains(binding)
            });
            if should_add_inverse_edge {
                blocked_by_updates.insert(consumer_identity.clone());
            }
        }
    }
}

fn dependencies_from_prior_state(state: &State, directives: Directives) -> HashSet<String> {
    state
        .dependency_bindings
        .iter()
        .chain(&directives.depends_on)
        .cloned()
        .collect()
}

fn resource_identity(resource: &ResolvedResource) -> ResourceIdentity {
    resource.identity().clone()
}

fn pending_replace_from_parts(
    from: Box<State>,
    identifier: crate::provider::AppliedIdentifier,
    to: Resource,
    directives: Directives,
    changed_create_only: ChangedCreateOnly,
    cascade_ref_hints: Vec<(String, String)>,
    registry: &SchemaRegistry,
) -> Result<PendingReplace, CreateBeforeDestroyError> {
    let create_before_destroy = directives.create_before_destroy;
    let previous_attributes = from.attributes.clone();
    let temporary_name = if create_before_destroy {
        temporary_name_for_cbd(&to, &from, registry)?
    } else {
        None
    };
    let create = apply_temporary_name(to, temporary_name.as_ref());
    let dependencies = from.dependency_bindings.iter().cloned().collect();
    let explicit_dependencies = directives.depends_on.iter().cloned().collect();
    let delete = ReplacementDelete {
        id: ResolvedResourceId::new(from.id.clone()),
        identifier,
        directives,
        binding: create.binding.clone(),
        dependencies,
        explicit_dependencies,
    };

    Ok(PendingReplace {
        create: ResolvedResource::new(create),
        delete,
        create_before_destroy,
        changed_create_only,
        cascade_ref_hints,
        temporary_name,
        consumer_updates: HashSet::new(),
        previous_attributes,
    })
}

pub(super) fn temporary_name_for_cbd(
    resource: &Resource,
    from: &State,
    registry: &SchemaRegistry,
) -> Result<Option<TemporaryName>, CreateBeforeDestroyError> {
    let schema = registry
        .get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Resource,
        )
        .ok_or_else(|| SchemaNotRegisteredError {
            resource_type: resource.id.display_type(),
            resource_identity: resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.identity_display().to_string()),
        })?;
    generate_temporary_name(resource, from, schema.as_schema())
}

fn apply_temporary_name(
    mut resource: Resource,
    temporary_name: Option<&TemporaryName>,
) -> Resource {
    if let Some(temp) = temporary_name {
        resource.set_attr(
            temp.attribute.clone(),
            Value::Concrete(ConcreteValue::String(temp.temporary_value.clone().into())),
        );
    }
    resource
}

fn decompose_replace_into_effects(
    plan: &mut Plan,
    pending_replaces: HashMap<ResourceIdentity, PendingReplace>,
) {
    let mut pending_replaces: Vec<_> = pending_replaces.into_values().collect();
    pending_replaces.sort_by_key(|pending| pending.create.id.to_string());

    for pending in pending_replaces {
        let permanent_name_override =
            pending
                .temporary_name
                .as_ref()
                .map(|temporary_name| PermanentNameOverride {
                    resource_id: ResolvedResourceId::new(pending.create.id.clone()),
                    attribute: temporary_name.attribute.clone(),
                    temp_value: temporary_name.temporary_value.clone(),
                    original_value: Some(temporary_name.original_value.clone()),
                });

        plan.add_replacement(ReplacementGroup {
            create: pending.create,
            delete: pending.delete,
            create_before_destroy: pending.create_before_destroy,
            changed_create_only: pending.changed_create_only,
            cascade_ref_hints: pending.cascade_ref_hints,
            temporary_name: pending.temporary_name,
            permanent_name_override,
            consumer_updates: pending.consumer_updates,
            previous_attributes: pending.previous_attributes,
        });
    }
}

fn known_binding_names(
    managed: &[ResolvedResource],
    data_sources: &[DataSource],
) -> HashSet<String> {
    managed
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .chain(
            data_sources
                .iter()
                .filter_map(|resource| resource.binding.clone()),
        )
        .collect()
}

fn cascade_dependent_updates(
    plan: &mut Plan,
    pending_replaces: &mut HashMap<ResourceIdentity, PendingReplace>,
    unresolved_managed: &[ResolvedResource],
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> ProviderResult<()> {
    loop {
        promote_referenced_replaces_to_cbd(plan, pending_replaces, unresolved_managed, registry);

        if pending_replaces.is_empty() {
            return Ok(());
        }

        let promoted = promote_pending_replaces_for_dependents(
            plan,
            pending_replaces,
            unresolved_managed,
            current_states,
            registry,
            comparison_provider,
            outcome_unknown_import_replacements,
        )?;

        if !promoted {
            break;
        }
    }
    Ok(())
}

fn promote_referenced_replaces_to_cbd(
    plan: &mut Plan,
    pending_replaces: &mut HashMap<ResourceIdentity, PendingReplace>,
    unresolved_managed: &[ResolvedResource],
    registry: &SchemaRegistry,
) {
    let ref_targets = pending_reference_targets(pending_replaces, false);
    let mut promote = Vec::new();

    for resource in unresolved_managed {
        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            let Some(replaced_identity) = ref_targets.get(dep) else {
                continue;
            };
            let Some(pending) = pending_replaces.get(replaced_identity) else {
                continue;
            };
            if pending.create_before_destroy || pending.create.id == resource.id {
                continue;
            }
            promote.push(replaced_identity.clone());
        }
    }

    promote.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    promote.dedup();

    for identity in promote {
        let result = pending_replaces
            .get_mut(&identity)
            .map(|pending| mark_pending_create_before_destroy(pending, registry));
        if let Some(Err(err)) = result
            && let Some(pending) = pending_replaces.remove(&identity)
        {
            plan.add_error(PlanError::new(pending.create.id.clone(), err.into()));
        }
    }
}

fn mark_pending_create_before_destroy(
    pending: &mut PendingReplace,
    registry: &SchemaRegistry,
) -> Result<(), CreateBeforeDestroyError> {
    if pending.create_before_destroy {
        return Ok(());
    }

    let mut create = pending.create.clone().into_inner();
    create.directives.create_before_destroy = true;
    let from = State::existing(
        pending.delete.id.as_inner().clone(),
        pending.previous_attributes.clone(),
    );
    let temporary_name = temporary_name_for_cbd(&create, &from, registry)?;
    let create = apply_temporary_name(create, temporary_name.as_ref());

    pending.create = ResolvedResource::new(create);
    pending.create_before_destroy = true;
    pending.delete.directives.create_before_destroy = true;
    pending.temporary_name = temporary_name;
    Ok(())
}

fn promote_pending_replaces_for_dependents(
    plan: &mut Plan,
    pending_replaces: &mut HashMap<ResourceIdentity, PendingReplace>,
    unresolved_managed: &[ResolvedResource],
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> ProviderResult<bool> {
    let ref_targets = pending_reference_targets(pending_replaces, true);
    let mut promoted = false;

    for resource in unresolved_managed {
        let consumer_identity = resource_identity(resource);
        let deps = get_resource_dependencies(resource);

        for dep in &deps {
            let Some(replaced_identity) = ref_targets.get(dep).cloned() else {
                continue;
            };
            if consumer_identity == replaced_identity {
                continue;
            }

            let ref_attrs = cascade_ref_attrs(&resource.attributes, dep);
            if ref_attrs.is_empty() {
                continue;
            }

            let ref_attr_names: Vec<String> = ref_attrs
                .iter()
                .map(|ref_attr| ref_attr.attribute.clone())
                .collect();
            let Some(Current::Present(present)) = current_states
                .get(&resource.id)
                .map(PlanInputCurrent::current)
            else {
                continue;
            };
            let resolved_comparison = comparison_provider.comparison_for(registry, &resource.id)?;
            let owner_schema = resolved_comparison.owner_schema();
            let create_only_refs = find_changed_create_only(
                &ref_attr_names,
                present.state(),
                resource,
                owner_schema.schema().as_schema(),
                resolved_comparison.context(),
            );

            if let Some(changed_create_only) = ChangedCreateOnly::new(create_only_refs) {
                let ref_hints: Vec<(String, String)> = ref_attrs
                    .into_iter()
                    .filter(|ref_attr| changed_create_only.contains(&ref_attr.attribute))
                    .map(|ref_attr| (ref_attr.attribute, ref_attr.hint))
                    .collect();

                if pending_replaces.contains_key(&consumer_identity) {
                    merge_pending_create_only(
                        pending_replaces.get_mut(&consumer_identity).unwrap(),
                        changed_create_only,
                        ref_hints,
                    );
                } else {
                    let prevent_destroy_source = owner_schema
                        .resolve_directives(&resource.directives)
                        .prevent_destroy_source();
                    if let Some(source) = prevent_destroy_source {
                        plan.add_error(PlanError::prevent_destroy(
                            resource.id.clone(),
                            PreventDestroyAction::CascadeReplace,
                            source,
                        ));
                        continue;
                    }
                    if let Some(pending) = promote_resource_to_pending_replace(
                        plan,
                        resource,
                        changed_create_only,
                        ref_hints,
                        current_states,
                        registry,
                        outcome_unknown_import_replacements,
                    ) {
                        pending_replaces.insert(consumer_identity.clone(), pending);
                        promoted = true;
                    } else {
                        continue;
                    }
                }

                if let Some(replaced) = pending_replaces.get_mut(&replaced_identity) {
                    replaced.consumer_updates.insert(consumer_identity.clone());
                }
            } else if !pending_replaces.contains_key(&consumer_identity)
                && ensure_consumer_update(
                    plan,
                    resource,
                    &ref_attr_names,
                    current_states,
                    outcome_unknown_import_replacements,
                )
                && let Some(replaced) = pending_replaces.get_mut(&replaced_identity)
            {
                replaced.consumer_updates.insert(consumer_identity.clone());
            }
        }
    }

    Ok(promoted)
}

fn pending_reference_targets(
    pending_replaces: &HashMap<ResourceIdentity, PendingReplace>,
    create_before_destroy_only: bool,
) -> HashMap<String, ResourceIdentity> {
    let mut targets = HashMap::new();
    for (identity, pending) in pending_replaces {
        if create_before_destroy_only && !pending.create_before_destroy {
            continue;
        }
        targets.insert(identity.as_str().to_string(), identity.clone());
        if let Some(binding) = &pending.create.binding {
            targets.insert(binding.clone(), identity.clone());
        }
    }
    targets
}

fn merge_pending_create_only(
    pending: &mut PendingReplace,
    attrs: ChangedCreateOnly,
    ref_hints: Vec<(String, String)>,
) {
    for attr in attrs.iter() {
        if !pending.changed_create_only.contains(attr) {
            pending.changed_create_only.push(attr.to_string());
        }
    }
    for hint in ref_hints {
        if !pending.cascade_ref_hints.contains(&hint) {
            pending.cascade_ref_hints.push(hint);
        }
    }
}

fn promote_resource_to_pending_replace(
    plan: &mut Plan,
    resource: &Resource,
    changed_create_only: ChangedCreateOnly,
    ref_hints: Vec<(String, String)>,
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> Option<PendingReplace> {
    let (from, identifier) = if let Some(update) = take_update_from_plan(plan, &resource.id) {
        update
    } else {
        let (from, identifier) = match current_states.get(&resource.id)?.current() {
            Current::Absent(_state) => return None,
            Current::Present(present) => (
                Box::new(present.state().clone()),
                present.identifier().clone(),
            ),
            Current::CreateInFlight(_pending) => return None,
            Current::CreateOutcomeUnknown(unknown) => {
                add_outcome_unknown_error(plan, unknown.id(), outcome_unknown_import_replacements);
                return None;
            }
            Current::MissingIdentifier(_state) => {
                plan.add_error(PlanError::new(
                    resource.id.clone(),
                    PlanErrorKind::MissingProviderIdentifier,
                ));
                return None;
            }
        };
        (from, identifier)
    };

    match pending_replace_from_parts(
        from,
        identifier,
        resource.clone(),
        resource.directives.clone(),
        changed_create_only,
        ref_hints,
        registry,
    ) {
        Ok(pending) => Some(pending),
        Err(err) => {
            plan.add_error(PlanError::new(resource.id.clone(), err.into()));
            None
        }
    }
}

fn take_update_from_plan(
    plan: &mut Plan,
    resource_id: &ResourceId,
) -> Option<(Box<State>, crate::provider::AppliedIdentifier)> {
    let index = plan
        .effects()
        .iter()
        .position(|effect| matches!(effect, Effect::Update { to, .. } if to.id == *resource_id))?;
    let effect = plan.effects_mut().remove(index);
    let Effect::Update {
        from, identifier, ..
    } = effect
    else {
        unreachable!("index selected by the Update variant")
    };
    Some((from, identifier))
}

fn ensure_consumer_update(
    plan: &mut Plan,
    resource: &Resource,
    changed_attributes: &[String],
    current_states: &HashMap<ResourceId, PlanInputCurrent>,
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) -> bool {
    if let Some(effect) = plan
        .effects_mut()
        .iter_mut()
        .find(|effect| matches!(effect, Effect::Update { to, .. } if to.id == resource.id))
    {
        if let Effect::Update {
            to,
            changed_attributes: existing,
            ..
        } = effect
        {
            *to = ResolvedResource::new(resource.clone());
            for attr in changed_attributes {
                if !existing.contains(attr) {
                    existing.push(attr.clone());
                }
            }
        }
        return true;
    }

    if plan
        .effects()
        .iter()
        .any(|effect| effect.resource_id() == &resource.id)
    {
        return false;
    }

    let Some(current) = current_states.get(&resource.id) else {
        return false;
    };
    let (from, identifier) = match current.current() {
        Current::Absent(_state) => return false,
        Current::Present(present) => (present.state().clone(), present.identifier().clone()),
        Current::CreateInFlight(_pending) => return false,
        Current::CreateOutcomeUnknown(unknown) => {
            add_outcome_unknown_error(plan, unknown.id(), outcome_unknown_import_replacements);
            return false;
        }
        Current::MissingIdentifier(_state) => {
            plan.add_error(PlanError::new(
                resource.id.clone(),
                PlanErrorKind::MissingProviderIdentifier,
            ));
            return false;
        }
    };

    plan.add(Effect::Update {
        from: Box::new(from),
        identifier,
        to: ResolvedResource::new(resource.clone()),
        changed_attributes: changed_attributes.to_vec(),
    });
    true
}

fn add_outcome_unknown_error(
    plan: &mut Plan,
    resource: &ResourceId,
    outcome_unknown_import_replacements: &HashSet<OutcomeUnknownImportReplacement>,
) {
    if !outcome_unknown_import_replacements
        .iter()
        .any(|replacement| replacement.resource_id() == resource)
    {
        plan.add_error(PlanError::create_outcome_unknown(resource.clone()));
    }
}

#[cfg(test)]
mod typed_current_identifier_tests {
    use super::*;

    fn current_with_authoritative_identifier(
        resource: &Resource,
    ) -> HashMap<ResourceId, PlanInputCurrent> {
        let state = State::existing(resource.id.clone(), HashMap::new());
        let identifier = crate::provider::fixture_applied_identifier("typed-provider-id");
        crate::resource::into_plan_input_current_map(
            HashMap::from([(
                resource.id.clone(),
                crate::resource::fixture_present_current(state, identifier),
            )]),
            &SchemaRegistry::new(),
            std::slice::from_ref(resource),
        )
    }

    #[test]
    fn ensure_consumer_update_uses_identifier_carried_by_present_current() {
        let resource = Resource::with_provider("mock", "test.Resource", "consumer", None);
        let current = current_with_authoritative_identifier(&resource);
        let mut plan = Plan::new();

        assert!(ensure_consumer_update(
            &mut plan,
            &resource,
            &["input".to_string()],
            &current,
            &HashSet::new(),
        ));
        let [
            Effect::Update {
                from,
                identifier,
                to,
                changed_attributes,
            },
        ] = plan.effects()
        else {
            panic!("expected one Update effect, got: {:?}", plan.effects());
        };
        assert_eq!(from.id, resource.id);
        assert_eq!(identifier.as_str(), "typed-provider-id");
        assert_eq!(to.id, resource.id);
        assert_eq!(changed_attributes, &["input".to_string()]);
    }

    #[test]
    fn pending_replace_promotion_uses_identifier_carried_by_present_current() {
        let resource = Resource::with_provider("mock", "test.Resource", "consumer", None);
        let current = current_with_authoritative_identifier(&resource);
        let mut plan = Plan::new();

        let pending = promote_resource_to_pending_replace(
            &mut plan,
            &resource,
            ChangedCreateOnly::new(vec!["input".to_string()]).expect("non-empty change set"),
            Vec::new(),
            &current,
            &SchemaRegistry::new(),
            &HashSet::new(),
        )
        .expect("present current state can be promoted");

        assert_eq!(pending.delete.identifier.as_str(), "typed-provider-id");
    }
}
