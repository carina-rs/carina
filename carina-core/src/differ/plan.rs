//! Plan generation from diffs and cascading update logic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::deps::get_resource_dependencies;
use crate::effect::{ChangedCreateOnly, Effect, TemporaryName};
use crate::identifier::generate_random_suffix;
use crate::parser::WaitBinding;
use crate::plan::{PendingReplace, Plan, PlanError, ReplacementGroup};
use crate::provider::Provider;
use crate::resource::{
    ConcreteValue, DataSource, Directives, PlanInputState, Resource, ResourceId, State, Value,
};
use crate::schema::{
    ResourceSchema, SchemaKind, SchemaRegistry, WAIT_DEFAULT_INTERVAL, WAIT_DEFAULT_TIMEOUT,
};
use crate::wait::augment::satisfier_augmentation;
use crate::wait::predicate::{AttrPath, WaitPredicate};

use super::{Diff, diff};

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
/// When a resource has a `name_attribute` with a unique constraint and uses
/// `create_before_destroy`, we need a temporary name for the new resource to
/// avoid conflicts with the old resource that still exists.
///
/// Returns `None` if no temporary name is needed (no name_attribute
/// or the resource already uses name_prefix for that attribute).
fn generate_temporary_name(
    resource: &Resource,
    _from: &State,
    schema: &ResourceSchema,
) -> Option<TemporaryName> {
    let name_attr = schema.name_attribute.as_ref()?;

    // Skip if the resource uses name_prefix for this attribute
    if resource.prefixes.contains_key(name_attr) {
        return None;
    }

    // Get the current value of the name attribute
    let original_value = match resource.get_attr(name_attr) {
        Some(Value::Concrete(ConcreteValue::String(s))) => s.clone(),
        _ => return None,
    };

    let temporary_value = format!("{}-{}", original_value, generate_random_suffix());

    Some(TemporaryName {
        attribute: name_attr.clone(),
        original_value,
        temporary_value,
    })
}

fn apply_temporary_name(mut resource: Resource, temporary_name: &TemporaryName) -> Resource {
    resource.set_attr(
        temporary_name.attribute.clone(),
        Value::Concrete(ConcreteValue::String(
            temporary_name.temporary_value.clone(),
        )),
    );
    resource
}

fn refresh_pending_temporary_name(pending: &mut PendingReplace, registry: &SchemaRegistry) {
    if !pending.create_before_destroy || pending.temporary_name.is_some() {
        return;
    }
    let Some(schema) = registry.get(
        &pending.id.provider,
        &pending.id.resource_type,
        SchemaKind::Resource,
    ) else {
        return;
    };
    if let Some(temp) = generate_temporary_name(&pending.to, &pending.from, schema) {
        let to = std::mem::replace(&mut pending.to, Resource::new("", ""));
        pending.to = apply_temporary_name(to, &temp);
        pending.temporary_name = Some(temp);
    }
}

pub(crate) fn decompose_replace_into_effects(
    plan: &mut Plan,
    pending: PendingReplace,
    consumer_updates: Vec<(String, Effect)>,
) {
    let blocked_by_updates: HashSet<String> = consumer_updates
        .iter()
        .map(|(blocker, _)| blocker.clone())
        .collect();
    let binding = pending.to.binding.clone();
    let delete = Effect::Delete {
        id: pending.id.clone(),
        identifier: pending.from.identifier.clone().unwrap_or_default(),
        directives: pending.directives.clone(),
        binding: binding.clone(),
        dependencies: pending.from.dependency_bindings.iter().cloned().collect(),
        explicit_dependencies: pending.directives.depends_on.iter().cloned().collect(),
        blocked_by_updates,
    };

    if pending.create_before_destroy
        && let Some(temp) = &pending.temporary_name
    {
        plan.add_permanent_name_override(
            pending.id.clone(),
            temp.attribute.clone(),
            temp.temporary_value.clone(),
            temp.original_value.clone(),
        );
    }

    plan.add_replacement(ReplacementGroup {
        id: pending.id,
        binding,
        create: pending.to,
        delete,
        create_before_destroy: pending.create_before_destroy,
        changed_create_only: pending.changed_create_only,
        cascade_ref_hints: pending.cascade_ref_hints,
        temporary_name: pending.temporary_name,
        previous_attributes: pending.from.attributes.clone(),
    });
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
    let mut plan = Plan::new();
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
            resource: ds.clone(),
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
            Diff::Create(r) => {
                plan.add(Effect::Create(r));
            }
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
                    let directives = resource.directives.clone();
                    let create_before_destroy = directives.create_before_destroy;
                    let temporary_name = if create_before_destroy {
                        registry
                            .get(
                                &resource.id.provider,
                                &resource.id.resource_type,
                                SchemaKind::Resource,
                            )
                            .and_then(|schema| generate_temporary_name(&to, &from, schema))
                    } else {
                        None
                    };

                    // If a temporary name is generated, modify the `to` resource
                    let to = if let Some(ref temp) = temporary_name {
                        let mut modified = to;
                        modified.set_attr(
                            temp.attribute.clone(),
                            Value::Concrete(ConcreteValue::String(temp.temporary_value.clone())),
                        );
                        modified
                    } else {
                        to
                    };

                    plan.add_pending_replace(PendingReplace {
                        id,
                        from,
                        to,
                        directives,
                        changed_create_only,
                        temporary_name,
                        cascade_ref_hints: vec![],
                        create_before_destroy,
                    });
                } else {
                    plan.add(Effect::Update {
                        id,
                        from,
                        to,
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
                    id,
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
                id: id.clone(),
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
                    .find(|r| r.id.name.as_str() == wb.target)
                    .map(|r| r.id.clone())
            })
            .or_else(|| {
                data_sources
                    .iter()
                    .find(|r| r.id.name.as_str() == wb.target)
                    .map(|r| r.id.clone())
            });
        let Some(target_id_resolved) = resolved else {
            plan.add_error(PlanError {
                resource_id: ResourceId::new("__wait", wb.binding.as_str()),
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
            // Lower BindingName -> String at the AST→Effect seam: the
            // executor IR (Effect) is string-keyed and is the separate
            // type tracked for its own newtype migration (carina#3066).
            binding: wb.binding.as_str().to_string(),
            target_id,
            until,
            until_surface: wb.until_raw.clone(),
            timeout,
            interval,
            explicit_dependencies,
        });
    }

    let pending_replaces = plan.take_pending_replaces();
    for mut pending in pending_replaces {
        refresh_pending_temporary_name(&mut pending, registry);
        decompose_replace_into_effects(&mut plan, pending, Vec::new());
    }

    plan
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

fn cbd_replaced_bindings(pending_replaces: &[PendingReplace]) -> HashSet<String> {
    pending_replaces
        .iter()
        .filter(|pending| pending.create_before_destroy)
        .filter_map(|pending| pending.to.binding.clone())
        .collect()
}

fn promote_pending_replaces_for_dependents(
    pending_replaces: &mut [PendingReplace],
    unresolved_managed: &[Resource],
    registry: &SchemaRegistry,
) -> bool {
    let pending_by_binding: HashMap<String, usize> = pending_replaces
        .iter()
        .enumerate()
        .filter_map(|(idx, pending)| pending.to.binding.clone().map(|binding| (binding, idx)))
        .collect();
    let mut promoted = false;

    for resource in unresolved_managed {
        for dep in get_resource_dependencies(resource) {
            let Some(idx) = pending_by_binding.get(&dep).copied() else {
                continue;
            };
            let Some(pending) = pending_replaces.get_mut(idx) else {
                continue;
            };
            if pending.id == resource.id || pending.create_before_destroy {
                continue;
            }
            pending.create_before_destroy = true;
            pending.directives.create_before_destroy = true;
            refresh_pending_temporary_name(pending, registry);
            promoted = true;
        }
    }

    promoted
}

fn annotate_pending_replaces_from_cbd_dependencies(
    pending_replaces: &mut [PendingReplace],
    replaced_bindings: &HashSet<String>,
    registry: &SchemaRegistry,
) {
    for pending in pending_replaces {
        for dep in get_resource_dependencies(&pending.to) {
            if !replaced_bindings.contains(&dep) {
                continue;
            }
            let ref_attrs = cascade_ref_attrs(&pending.to.attributes, &dep);
            let ref_attr_names: Vec<String> = ref_attrs
                .iter()
                .map(|ref_attr| ref_attr.attribute.clone())
                .collect();
            let create_only_refs = find_changed_create_only(
                &pending.id.provider,
                &pending.id.resource_type,
                &ref_attr_names,
                registry,
            );
            for attr in create_only_refs {
                if !pending.changed_create_only.contains(&attr) {
                    pending.changed_create_only.push(attr);
                }
            }
            for ref_attr in ref_attrs {
                if pending.changed_create_only.contains(&ref_attr.attribute)
                    && !pending
                        .cascade_ref_hints
                        .iter()
                        .any(|(attr, _)| attr == &ref_attr.attribute)
                {
                    pending
                        .cascade_ref_hints
                        .push((ref_attr.attribute, ref_attr.hint));
                }
            }
        }
    }
}

fn push_pending_replace_if_absent(
    pending_replaces: &mut Vec<PendingReplace>,
    pending: PendingReplace,
) -> bool {
    if pending_replaces
        .iter()
        .any(|existing| existing.id == pending.id)
    {
        return false;
    }
    pending_replaces.push(pending);
    true
}

fn push_consumer_update(
    consumer_updates_by_replaced: &mut HashMap<String, Vec<(String, Effect)>>,
    replaced_binding: String,
    update_key: String,
    update: Effect,
) {
    let updates = consumer_updates_by_replaced
        .entry(replaced_binding)
        .or_default();
    if updates
        .iter()
        .any(|(existing_key, _)| existing_key == &update_key)
    {
        return;
    }
    updates.push((update_key, update));
}

/// Finalize pending replacements and add visible cascade updates.
pub fn cascade_dependent_updates(
    plan: &mut Plan,
    unresolved_managed: &[Resource],
    current_states: &HashMap<ResourceId, PlanInputState>,
    registry: &SchemaRegistry,
) {
    let mut pending_replaces = plan.take_pending_replaces();
    if !plan.replace_display().is_empty() {
        let mut remove_indices: HashSet<usize> = HashSet::new();
        let mut decomposed_replaces = Vec::new();
        for metadata in plan.replace_display() {
            let Some(Effect::Create(to)) = plan.effects().get(metadata.create_idx) else {
                continue;
            };
            let directives = match plan.effects().get(metadata.delete_idx) {
                Some(Effect::Delete { directives, .. }) => directives.clone(),
                _ => Directives::default(),
            };
            let from = current_states
                .get(&metadata.id)
                .map(|state| state.as_state().clone())
                .unwrap_or_else(|| State::not_found(metadata.id.clone()));
            decomposed_replaces.push(PendingReplace {
                id: metadata.id.clone(),
                from: Box::new(from),
                to: to.clone(),
                directives,
                changed_create_only: metadata.changed_create_only.clone(),
                cascade_ref_hints: metadata.cascade_ref_hints.clone(),
                create_before_destroy: metadata.create_before_destroy,
                temporary_name: metadata.temporary_name.clone(),
            });
            remove_indices.insert(metadata.create_idx);
            remove_indices.insert(metadata.delete_idx);
        }
        if !remove_indices.is_empty() {
            plan.retain_indexed(|idx, _| !remove_indices.contains(&idx));
            plan.clear_replace_display();
            pending_replaces.extend(decomposed_replaces);
        }
    }
    let mut consumer_updates_by_replaced: HashMap<String, Vec<(String, Effect)>> = HashMap::new();
    let mut update_ids_to_remove: HashSet<ResourceId> = HashSet::new();
    let mut cascade_updates_by_id: HashMap<ResourceId, Effect> = HashMap::new();

    loop {
        let mut changed = promote_pending_replaces_for_dependents(
            &mut pending_replaces,
            unresolved_managed,
            registry,
        );
        let replaced_bindings = cbd_replaced_bindings(&pending_replaces);
        if replaced_bindings.is_empty() {
            break;
        }

        annotate_pending_replaces_from_cbd_dependencies(
            &mut pending_replaces,
            &replaced_bindings,
            registry,
        );

        let pending_ids: HashSet<ResourceId> = pending_replaces
            .iter()
            .map(|pending| pending.id.clone())
            .collect();
        let planned_ids: HashSet<ResourceId> = plan
            .effects()
            .iter()
            .map(|e| e.resource_id().clone())
            .chain(pending_ids.iter().cloned())
            .chain(cascade_updates_by_id.keys().cloned())
            .collect();

        for resource in unresolved_managed {
            if !planned_ids.contains(&resource.id) || pending_ids.contains(&resource.id) {
                continue;
            }
            let Some(existing_update) = plan
                .effects()
                .iter()
                .find(|effect| matches!(effect, Effect::Update { id, .. } if *id == resource.id))
                .cloned()
            else {
                continue;
            };
            for dep in get_resource_dependencies(resource) {
                if !replaced_bindings.contains(&dep) {
                    continue;
                }
                let ref_attrs = cascade_ref_attrs(&resource.attributes, &dep);
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
                    if resource.directives.prevent_destroy {
                        plan.add_error(PlanError {
                            resource_id: resource.id.clone(),
                            message:
                                "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
                                    .to_string(),
                        });
                        update_ids_to_remove.insert(resource.id.clone());
                        continue;
                    }
                    let from = match &existing_update {
                        Effect::Update { from, .. } => from.clone(),
                        _ => continue,
                    };
                    let ref_hints: Vec<(String, String)> = ref_attrs
                        .into_iter()
                        .filter(|ref_attr| changed_create_only.contains(&ref_attr.attribute))
                        .map(|ref_attr| (ref_attr.attribute, ref_attr.hint))
                        .collect();
                    changed |= push_pending_replace_if_absent(
                        &mut pending_replaces,
                        PendingReplace {
                            id: resource.id.clone(),
                            from,
                            to: resource.clone(),
                            directives: resource.directives.clone(),
                            changed_create_only,
                            cascade_ref_hints: ref_hints,
                            create_before_destroy: false,
                            temporary_name: None,
                        },
                    );
                    update_ids_to_remove.insert(resource.id.clone());
                    continue;
                }
                let update_key = resource.binding.clone().unwrap_or_else(|| {
                    format!("{}:{}", resource.id.resource_type, resource.id.name_str())
                });
                push_consumer_update(
                    &mut consumer_updates_by_replaced,
                    dep,
                    update_key,
                    existing_update.clone(),
                );
            }
        }

        for resource in unresolved_managed {
            if planned_ids.contains(&resource.id) {
                continue;
            }
            for replaced_binding in get_resource_dependencies(resource) {
                if !replaced_bindings.contains(&replaced_binding) {
                    continue;
                }
                let from = current_states
                    .get(&resource.id)
                    .map(|state| state.as_state().clone())
                    .unwrap_or_else(|| State::not_found(resource.id.clone()));
                if !from.exists {
                    continue;
                }

                let ref_attrs = cascade_ref_attrs(&resource.attributes, &replaced_binding);
                let mut ref_attr_names: Vec<String> = ref_attrs
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
                    if resource.directives.prevent_destroy {
                        plan.add_error(PlanError {
                            resource_id: resource.id.clone(),
                            message:
                                "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
                                    .to_string(),
                        });
                        continue;
                    }
                    let ref_hints: Vec<(String, String)> = ref_attrs
                        .into_iter()
                        .filter(|ref_attr| changed_create_only.contains(&ref_attr.attribute))
                        .map(|ref_attr| (ref_attr.attribute, ref_attr.hint))
                        .collect();
                    changed |= push_pending_replace_if_absent(
                        &mut pending_replaces,
                        PendingReplace {
                            id: resource.id.clone(),
                            from: Box::new(from),
                            to: resource.clone(),
                            directives: resource.directives.clone(),
                            changed_create_only,
                            cascade_ref_hints: ref_hints,
                            create_before_destroy: false,
                            temporary_name: None,
                        },
                    );
                } else {
                    ref_attr_names.sort();
                    ref_attr_names.dedup();
                    if !ref_attr_names.is_empty() {
                        let update = Effect::Update {
                            id: resource.id.clone(),
                            from: Box::new(from),
                            to: resource.clone(),
                            changed_attributes: ref_attr_names,
                        };
                        let update_key = resource.binding.clone().unwrap_or_else(|| {
                            format!("{}:{}", resource.id.resource_type, resource.id.name_str())
                        });
                        if cascade_updates_by_id
                            .insert(resource.id.clone(), update.clone())
                            .is_none()
                        {
                            changed = true;
                        }
                        push_consumer_update(
                            &mut consumer_updates_by_replaced,
                            replaced_binding,
                            update_key,
                            update,
                        );
                    }
                }
            }
        }

        if !changed {
            break;
        }
    }

    if !update_ids_to_remove.is_empty() {
        plan.retain(|effect| {
            !matches!(effect, Effect::Update { id, .. } if update_ids_to_remove.contains(id))
        });
    }

    for effect in cascade_updates_by_id.into_values() {
        plan.add(effect);
    }

    for mut pending in pending_replaces {
        refresh_pending_temporary_name(&mut pending, registry);
        let consumer_updates = pending
            .to
            .binding
            .as_ref()
            .and_then(|binding| consumer_updates_by_replaced.get(binding))
            .cloned()
            .unwrap_or_default();
        decompose_replace_into_effects(plan, pending, consumer_updates);
    }
}
