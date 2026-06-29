//! Plan generation from diffs and cascading update logic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::deps::get_resource_dependencies;
use serde::{Deserialize, Serialize};

use crate::effect::{ChangedCreateOnly, Effect, TemporaryName};
use crate::identifier::generate_random_suffix;
use crate::parser::WaitBinding;
use crate::plan::{PermanentNameOverride, Plan, PlanError, ReplacementDelete, ReplacementGroup};
use crate::provider::Provider;
use crate::resource::{
    ConcreteValue, DataSource, Directives, PlanInputState, ResolvedDataSource, ResolvedResource,
    ResolvedResourceId, Resource, ResourceId, ResourceIdentity, State, Value,
};
use crate::schema::{
    ResourceSchema, SchemaKind, SchemaRegistry, WAIT_DEFAULT_INTERVAL, WAIT_DEFAULT_TIMEOUT,
};
use crate::wait::augment::satisfier_augmentation;
use crate::wait::predicate::{AttrPath, WaitPredicate};

use super::{Diff, diff};

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

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[error(
    "resource type '{resource_type}' has no unique_name_attribute; create_before_destroy needs one to generate a temporary name"
)]
pub struct MissingNameAttributeError {
    pub resource_type: String,
    /// The binding name (let-bound) or resolver identity (anonymous)
    /// of the resource that triggered the CBD path. For diagnostics only.
    pub resource_identity: String,
}

struct RefAttr {
    attribute: String,
    hint: String,
}

/// Check which changed attributes are create-only according to the schema
fn find_changed_create_only(
    provider: &str,
    resource_type: &str,
    changed_attributes: &[String],
    registry: &SchemaRegistry,
) -> Vec<String> {
    let Some(schema) = registry.get(provider, resource_type, SchemaKind::Resource) else {
        return Vec::new();
    };

    let create_only_attrs = schema.create_only_attributes();
    changed_attributes
        .iter()
        .filter(|attr| create_only_attrs.contains(&attr.as_str()))
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
fn filter_non_removable_removals(
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
/// A schema that lacks `unique_name_attribute` is a plan-time error on every
/// CBD path. Other cases can still legitimately avoid a temporary name: a
/// `name_prefix` already produces a non-conflicting cloud name, and a direct
/// change to the unique-name value is already distinct from the old resource.
pub fn generate_temporary_name(
    resource: &Resource,
    from: &State,
    schema: &ResourceSchema,
) -> Result<Option<TemporaryName>, MissingNameAttributeError> {
    let name_attr =
        schema
            .unique_name_attribute
            .as_ref()
            .ok_or_else(|| MissingNameAttributeError {
                resource_type: resource.id.display_type(),
                resource_identity: resource
                    .binding
                    .clone()
                    .unwrap_or_else(|| resource.id.identity_or_empty().to_string()),
            })?;

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

    let temporary_value = format!("{}-{}", original_value, generate_random_suffix());

    Ok(Some(TemporaryName {
        attribute: name_attr.clone(),
        original_value,
        temporary_value,
        can_rename,
    }))
}

/// Compute Diff for multiple resources and generate a Plan
///
/// The `directives_map` provides Carina-side directives for orphaned
/// resources (resources in state but not in desired). For desired
/// resources, the directives are read directly from the `Resource`
/// struct.
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
/// below pins that — passing a [`Composition`](crate::resource::Composition)
/// slice must fail to type-check, so the post-apply-only composition class
/// (carina#3169) cannot accidentally reach pre-apply differ logic.
///
/// ```compile_fail
/// use std::collections::{BTreeSet, HashMap};
/// use carina_core::differ::create_plan;
/// use carina_core::resource::Composition;
/// use carina_core::schema::SchemaRegistry;
/// let compositions: Vec<Composition> = vec![];
/// let _ = create_plan(
///     &compositions,
///     &[],
///     &carina_core::provider::ProviderRouter::new(),
///     &HashMap::new(),
///     &HashMap::new(),
///     &SchemaRegistry::default(),
///     &HashMap::new(),
///     &HashMap::new(),
///     &HashMap::new(),
///     &[],
/// );
/// ```
///
/// Raw [`State`] maps cannot be passed to planning. Callers must convert
/// through [`State::into_plan_input`](crate::resource::State::into_plan_input)
/// or [`into_plan_input_map`](crate::resource::into_plan_input_map), which
/// materializes partial-create unknowns before diffing.
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::differ::create_plan;
/// use carina_core::provider::ProviderRouter;
/// use carina_core::resource::{Resource, ResourceId, State};
/// use carina_core::schema::SchemaRegistry;
///
/// let resources: Vec<Resource> = vec![];
/// let states: HashMap<ResourceId, State> = HashMap::new();
/// let _ = create_plan(
///     &resources,
///     &[],
///     &ProviderRouter::new(),
///     &states,
///     &HashMap::new(),
///     &SchemaRegistry::default(),
///     &HashMap::new(),
///     &HashMap::new(),
///     &HashMap::new(),
///     &[],
/// );
/// ```
#[allow(clippy::too_many_arguments)]
pub fn create_plan(
    managed: &[Resource],
    data_sources: &[DataSource],
    provider: &dyn Provider,
    current_states: &HashMap<ResourceId, PlanInputState>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphan_dependencies: &HashMap<ResourceId, BTreeSet<String>>,
    wait_bindings: &[WaitBinding],
) -> Plan {
    let mut build = create_plan_parts(
        managed,
        data_sources,
        provider,
        current_states,
        directives_map,
        registry,
        saved_attrs,
        prev_explicit,
        orphan_dependencies,
        wait_bindings,
    );
    decompose_replace_into_effects(&mut build.plan, build.pending_replaces);
    build.plan
}

#[allow(clippy::too_many_arguments)]
pub fn create_plan_with_cascades(
    managed: &[Resource],
    data_sources: &[DataSource],
    unresolved_managed: &[Resource],
    provider: &dyn Provider,
    current_states: &HashMap<ResourceId, PlanInputState>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphan_dependencies: &HashMap<ResourceId, BTreeSet<String>>,
    wait_bindings: &[WaitBinding],
) -> Plan {
    let mut build = create_plan_parts(
        managed,
        data_sources,
        provider,
        current_states,
        directives_map,
        registry,
        saved_attrs,
        prev_explicit,
        orphan_dependencies,
        wait_bindings,
    );
    cascade_dependent_updates(
        &mut build.plan,
        &mut build.pending_replaces,
        unresolved_managed,
        current_states,
        registry,
    );
    decompose_replace_into_effects(&mut build.plan, build.pending_replaces);
    build.plan
}

struct PlanBuild {
    plan: Plan,
    pending_replaces: HashMap<ResourceIdentity, PendingReplace>,
}

#[allow(clippy::too_many_arguments)]
fn create_plan_parts(
    managed: &[Resource],
    data_sources: &[DataSource],
    provider: &dyn Provider,
    current_states: &HashMap<ResourceId, PlanInputState>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphan_dependencies: &HashMap<ResourceId, BTreeSet<String>>,
    wait_bindings: &[WaitBinding],
) -> PlanBuild {
    let mut plan = Plan::new();
    let mut pending_replaces = HashMap::new();
    let known_bindings = known_binding_names(managed, data_sources);

    let desired_ids: std::collections::HashSet<&ResourceId> = managed
        .iter()
        .map(|r| &r.id)
        .chain(data_sources.iter().map(|r| &r.id))
        .collect();

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
            .unwrap_or_else(|| State::not_found(resource.id.clone()).into_plan_input());

        let saved = saved_attrs.get(&resource.id);
        let prev_explicit_for_resource = prev_explicit.get(&resource.id);
        let schema = registry.get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Resource,
        );
        let d = diff(
            resource,
            current.as_state(),
            saved,
            prev_explicit_for_resource,
            schema,
        );

        match d {
            Diff::Create(r) => plan.add(Effect::Create(ResolvedResource::new(r))),
            Diff::Update {
                id,
                from,
                to,
                changed_attributes,
            } => {
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
                    // All changes were spurious non-removable removals
                    continue;
                }

                // Check if any changed attributes are create-only
                let changed_create_only = find_changed_create_only(
                    &resource.id.provider,
                    &resource.id.resource_type,
                    &changed_attributes,
                    registry,
                );

                if let Some(changed_create_only) = ChangedCreateOnly::new(changed_create_only) {
                    // Replace involves destroying the old resource
                    if resource.directives.prevent_destroy {
                        plan.add_error(PlanError {
                            resource_id: id.clone(),
                            message:
                                "resource has prevent_destroy set, but the plan would replace it (which requires destroying the old resource)"
                                    .to_string(),
                        });
                        continue;
                    }
                    match pending_replace_from_parts(
                        from,
                        to,
                        resource.directives.clone(),
                        changed_create_only,
                        Vec::new(),
                        registry,
                    ) {
                        Ok(pending) => {
                            pending_replaces.insert(resource_identity(&pending.create.id), pending);
                        }
                        Err(err) => plan.add_error(PlanError {
                            resource_id: id.clone(),
                            message: err.to_string(),
                        }),
                    }
                } else {
                    plan.add(Effect::Update {
                        from,
                        to: ResolvedResource::new(to),
                        changed_attributes,
                    });
                }
            }
            Diff::NoChange(_) => {}
            Diff::Delete(id) => {
                if resource.directives.prevent_destroy {
                    plan.add_error(PlanError {
                        resource_id: id.clone(),
                        message: "resource has prevent_destroy set, but the plan would delete it"
                            .to_string(),
                    });
                    continue;
                }
                let identifier = current_states
                    .get(&id)
                    .and_then(|s| s.as_state().identifier.clone())
                    .unwrap_or_default();
                let directives = resource.directives.clone();
                let binding = resource.binding.clone();
                let dependencies = get_resource_dependencies(resource);
                let explicit_dependencies =
                    resource.directives.depends_on.iter().cloned().collect();
                plan.add(Effect::Delete {
                    id: ResolvedResourceId::new(id),
                    identifier,
                    directives,
                    binding,
                    dependencies,
                    explicit_dependencies,
                    blocked_by_updates: HashSet::new(),
                });
            }
        }
    }

    // Detect orphaned resources: exist in current_states but not in desired
    for (id, plan_state) in current_states {
        let state = plan_state.as_state();
        if state.exists && !desired_ids.contains(id) {
            let directives = directives_map.get(id).cloned().unwrap_or_default();
            if directives.prevent_destroy {
                plan.add_error(PlanError {
                    resource_id: id.clone(),
                    message:
                        "resource has prevent_destroy set, but the plan would delete it (resource removed from configuration)"
                            .to_string(),
                });
                continue;
            }
            let identifier = state.identifier.clone().unwrap_or_default();
            let binding = state.attributes.get("_binding").and_then(|v| match v {
                Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
                _ => None,
            });
            // Use stored dependency bindings from state file if available,
            // otherwise fall back to extracting from state attributes
            let dependencies = if let Some(dep_bindings) = orphan_dependencies.get(id) {
                dep_bindings.iter().cloned().collect()
            } else {
                let temp_resource = Resource {
                    id: id.clone(),
                    // `state.attributes` is `HashMap` — no source order
                    // survives round-tripping through the provider. The
                    // ordering of this synthetic temp resource doesn't
                    // matter (it only feeds the dependency walker), so
                    // a plain clone-through `wrap_map` is fine.
                    attributes: state.attributes.clone().into_iter().collect(),
                    directives: directives.clone(),
                    prefixes: HashMap::new(),
                    binding: None,
                    dependency_bindings: BTreeSet::new(),
                    module_source: None,
                    quoted_string_attrs: std::collections::HashSet::new(),
                };
                get_resource_dependencies(&temp_resource)
            };
            plan.add(Effect::Delete {
                id: ResolvedResourceId::new(id.clone()),
                identifier,
                directives,
                binding,
                dependencies,
                // Orphan deletes have no source `directives.depends_on`;
                // the resource is gone from desired state. Carrying an
                // empty set here is correct and serde-stable for
                // pre-#2871 state files.
                explicit_dependencies: std::collections::HashSet::new(),
                blocked_by_updates: HashSet::new(),
            });
        }
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
        // Wait targets resolve over managed + data-source bindings.
        // compositions are post-apply attribute containers and don't carry
        // `until`-pollable state, so excluding them (by construction
        // via the typed-slice split) matches the runtime invariant.
        let resolved = managed
            .iter()
            .find(|r| r.binding.as_deref() == Some(wb.target.as_str()))
            .map(|r| r.id.clone())
            .or_else(|| {
                data_sources
                    .iter()
                    .find(|r| r.binding.as_deref() == Some(wb.target.as_str()))
                    .map(|r| r.id.clone())
            })
            .or_else(|| {
                managed
                    .iter()
                    .find(|r| r.id.identity_str() == Some(wb.target.as_str()))
                    .map(|r| r.id.clone())
            })
            .or_else(|| {
                data_sources
                    .iter()
                    .find(|r| r.id.identity_str() == Some(wb.target.as_str()))
                    .map(|r| r.id.clone())
            });
        let Some(target_id_resolved) = resolved else {
            plan.add_error(PlanError {
                resource_id: ResourceId::with_identity("__wait", wb.binding.as_str()),
                message: format!(
                    "wait `{}`: target binding `{}` is not a known resource",
                    wb.binding, wb.target
                ),
            });
            continue;
        };
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
        // `lhs_segments` is guaranteed non-empty and the first segment
        // equals `wb.target` (enforced by `parse_wait_expr`). The
        // predicate attribute path is therefore `lhs_segments[1..]`.
        // This `try_new` branch is defense in depth: the parser rejects
        // bare-target LHS values like `until = cert == ...`, but keep a
        // plan-level error here in case a future parser path relaxes
        // that rule or constructs `WaitBinding` directly.
        let attr = match AttrPath::try_new(wb.until_predicate.lhs_segments[1..].to_vec()) {
            Ok(attr) => attr,
            Err(err) => {
                plan.add_error(PlanError {
                    resource_id: target_id_resolved.clone(),
                    message: format!(
                        "wait `{}`: invalid predicate attribute path: {err}",
                        wb.binding
                    ),
                });
                continue;
            }
        };
        let until = WaitPredicate::Equals {
            attr: attr.clone(),
            value: wb.until_predicate.rhs.clone(),
        };
        let target_id = target_id_resolved;
        let schema = registry.get(
            &target_id.provider,
            &target_id.resource_type,
            SchemaKind::Resource,
        );
        let schema_timeout = schema.and_then(|s| s.default_wait_timeout);
        let schema_interval = schema.and_then(|s| s.default_wait_interval);
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
        let wait_has_work = current_states
            .get(&target_id)
            .map(|plan_state| {
                let state = plan_state.as_state();
                let target_is_mutating = plan
                    .effects()
                    .iter()
                    .any(|e| e.resource_id() == &target_id && e.is_mutating());
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
            &target_id,
            &attr,
            &known_bindings,
        ));

        plan.add(Effect::Wait {
            identity: ResourceIdentity::new(wb.binding.as_str()),
            target_id: ResolvedResourceId::new(target_id),
            until,
            until_surface: wb.until_raw.clone(),
            timeout,
            interval,
            explicit_dependencies,
        });
    }

    PlanBuild {
        plan,
        pending_replaces,
    }
}

fn resource_identity(id: &ResourceId) -> ResourceIdentity {
    id.identity
        .as_ref()
        .expect("differ only receives resources after identity resolution")
        .clone()
}

fn pending_replace_from_parts(
    from: Box<State>,
    to: Resource,
    directives: Directives,
    changed_create_only: ChangedCreateOnly,
    cascade_ref_hints: Vec<(String, String)>,
    registry: &SchemaRegistry,
) -> Result<PendingReplace, MissingNameAttributeError> {
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
        identifier: from.identifier.clone().unwrap_or_default(),
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

fn temporary_name_for_cbd(
    resource: &Resource,
    from: &State,
    registry: &SchemaRegistry,
) -> Result<Option<TemporaryName>, MissingNameAttributeError> {
    let schema = registry
        .get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Resource,
        )
        .ok_or_else(|| MissingNameAttributeError {
            resource_type: resource.id.display_type(),
            resource_identity: resource
                .binding
                .clone()
                .unwrap_or_else(|| resource.id.identity_or_empty().to_string()),
        })?;
    generate_temporary_name(resource, from, schema)
}

fn apply_temporary_name(
    mut resource: Resource,
    temporary_name: Option<&TemporaryName>,
) -> Resource {
    if let Some(temp) = temporary_name {
        resource.set_attr(
            temp.attribute.clone(),
            Value::Concrete(ConcreteValue::String(temp.temporary_value.clone())),
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

fn known_binding_names(managed: &[Resource], data_sources: &[DataSource]) -> HashSet<String> {
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
    unresolved_managed: &[Resource],
    current_states: &HashMap<ResourceId, PlanInputState>,
    registry: &SchemaRegistry,
) {
    loop {
        promote_referenced_replaces_to_cbd(plan, pending_replaces, unresolved_managed, registry);

        if pending_replaces.is_empty() {
            return;
        }

        let promoted = promote_pending_replaces_for_dependents(
            plan,
            pending_replaces,
            unresolved_managed,
            current_states,
            registry,
        );

        if !promoted {
            break;
        }
    }
}

fn promote_referenced_replaces_to_cbd(
    plan: &mut Plan,
    pending_replaces: &mut HashMap<ResourceIdentity, PendingReplace>,
    unresolved_managed: &[Resource],
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
            plan.add_error(PlanError {
                resource_id: pending.create.id.clone(),
                message: err.to_string(),
            });
        }
    }
}

fn mark_pending_create_before_destroy(
    pending: &mut PendingReplace,
    registry: &SchemaRegistry,
) -> Result<(), MissingNameAttributeError> {
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
    unresolved_managed: &[Resource],
    current_states: &HashMap<ResourceId, PlanInputState>,
    registry: &SchemaRegistry,
) -> bool {
    let ref_targets = pending_reference_targets(pending_replaces, true);
    let mut promoted = false;

    for resource in unresolved_managed {
        let consumer_identity = resource_identity(&resource.id);
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
            let create_only_refs = find_changed_create_only(
                &resource.id.provider,
                &resource.id.resource_type,
                &ref_attr_names,
                registry,
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
                } else if resource.directives.prevent_destroy {
                    plan.add_error(PlanError {
                        resource_id: resource.id.clone(),
                        message:
                            "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
                                .to_string(),
                    });
                    continue;
                } else if let Some(pending) = promote_resource_to_pending_replace(
                    plan,
                    resource,
                    changed_create_only,
                    ref_hints,
                    current_states,
                    registry,
                ) {
                    pending_replaces.insert(consumer_identity.clone(), pending);
                    promoted = true;
                } else {
                    continue;
                }

                if let Some(replaced) = pending_replaces.get_mut(&replaced_identity) {
                    replaced.consumer_updates.insert(consumer_identity.clone());
                }
            } else if !pending_replaces.contains_key(&consumer_identity)
                && ensure_consumer_update(plan, resource, &ref_attr_names, current_states)
                && let Some(replaced) = pending_replaces.get_mut(&replaced_identity)
            {
                replaced.consumer_updates.insert(consumer_identity.clone());
            }
        }
    }

    promoted
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
    current_states: &HashMap<ResourceId, PlanInputState>,
    registry: &SchemaRegistry,
) -> Option<PendingReplace> {
    let from = take_update_from_plan(plan, &resource.id).or_else(|| {
        current_states
            .get(&resource.id)
            .map(|state| Box::new(state.as_state().clone()))
    })?;

    if !from.exists {
        return None;
    }

    match pending_replace_from_parts(
        from,
        resource.clone(),
        resource.directives.clone(),
        changed_create_only,
        ref_hints,
        registry,
    ) {
        Ok(pending) => Some(pending),
        Err(err) => {
            plan.add_error(PlanError {
                resource_id: resource.id.clone(),
                message: err.to_string(),
            });
            None
        }
    }
}

fn take_update_from_plan(plan: &mut Plan, resource_id: &ResourceId) -> Option<Box<State>> {
    let index = plan
        .effects()
        .iter()
        .position(|effect| matches!(effect, Effect::Update { to, .. } if to.id == *resource_id))?;
    let effect = plan.effects_mut().remove(index);
    match effect {
        Effect::Update { from, .. } => Some(from),
        _ => None,
    }
}

fn ensure_consumer_update(
    plan: &mut Plan,
    resource: &Resource,
    changed_attributes: &[String],
    current_states: &HashMap<ResourceId, PlanInputState>,
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

    let Some(from) = current_states
        .get(&resource.id)
        .map(|state| state.as_state().clone())
    else {
        return false;
    };

    if !from.exists {
        return false;
    }

    plan.add(Effect::Update {
        from: Box::new(from),
        to: ResolvedResource::new(resource.clone()),
        changed_attributes: changed_attributes.to_vec(),
    });
    true
}
