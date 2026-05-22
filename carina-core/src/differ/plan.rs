//! Plan generation from diffs and cascading update logic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::deps::get_resource_dependencies;
use crate::effect::{CascadingUpdate, Effect, TemporaryName, WaitTarget};
use crate::identifier::generate_random_suffix;
use crate::parser::WaitBinding;
use crate::plan::{Plan, PlanError};
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, Directives, ManagedResource, ResourceId, State, Value,
};
use crate::schema::{
    ResourceSchema, SchemaKind, SchemaRegistry, WAIT_DEFAULT_INTERVAL, WAIT_DEFAULT_TIMEOUT,
};
use crate::wait::predicate::{AttrPath, WaitPredicate};

use super::{Diff, diff};

/// A pending merge operation: cascade-triggered create-only attributes to add to an existing effect.
struct CascadeMerge {
    resource_id: ResourceId,
    create_only_attrs: Vec<String>,
    directives: Directives,
    ref_hints: Vec<(String, String)>,
}

/// Check which changed attributes are create-only according to the schema
fn find_changed_create_only(
    provider: &str,
    resource_type: &str,
    changed_attributes: &[String],
    registry: &SchemaRegistry,
) -> Vec<String> {
    let Some(schema) = registry.get(provider, resource_type, SchemaKind::Managed) else {
        return Vec::new();
    };

    let create_only_attrs = schema.create_only_attributes();
    changed_attributes
        .iter()
        .filter(|attr| create_only_attrs.contains(&attr.as_str()))
        .cloned()
        .collect()
}

/// Filter out non-removable attribute removals from the changed list.
/// A "removal" is an attribute that appears in `changed_attributes` but not in `to`.
/// Only attributes marked as `removable` in the schema should be kept as removals.
/// Non-removal changes (attribute present in `to`) are always kept.
fn filter_non_removable_removals(
    provider: &str,
    resource_type: &str,
    to: &ManagedResource,
    changed_attributes: Vec<String>,
    registry: &SchemaRegistry,
) -> Vec<String> {
    let Some(schema) = registry.get(provider, resource_type, SchemaKind::Managed) else {
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
/// Returns `None` if no temporary name is needed (no name_attribute,
/// the resource already uses name_prefix for that attribute, or
/// the name_attribute value changed between `from` and `to`).
fn generate_temporary_name(
    resource: &ManagedResource,
    from: &State,
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

    // Skip if the name_attribute value changed (new name is already different from old)
    if let Some(Value::Concrete(ConcreteValue::String(from_name))) = from.attributes.get(name_attr)
        && *from_name != original_value
    {
        return None;
    }

    // Check if the name attribute is create-only (cannot be renamed after creation)
    let can_rename = schema
        .attributes
        .get(name_attr)
        .map(|attr| !attr.create_only)
        .unwrap_or(false);

    let temporary_value = format!("{}-{}", original_value, generate_random_suffix());

    Some(TemporaryName {
        attribute: name_attr.clone(),
        original_value,
        temporary_value,
        can_rename,
    })
}

/// Compute Diff for multiple resources and generate a Plan
///
/// The `directives_map` provides Carina-side directives for orphaned
/// resources (resources in state but not in desired). For desired
/// resources, the directives are read directly from the `ManagedResource`
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
/// Virtuals are intentionally not an input. The compile-fail doctest
/// below pins that — passing a [`VirtualResource`](crate::resource::VirtualResource)
/// slice must fail to type-check, so the post-apply-only virtual class
/// (carina#3169) cannot accidentally reach pre-apply differ logic.
///
/// ```compile_fail
/// use std::collections::{BTreeSet, HashMap};
/// use carina_core::differ::create_plan;
/// use carina_core::resource::VirtualResource;
/// use carina_core::schema::SchemaRegistry;
/// let virtuals: Vec<VirtualResource> = vec![];
/// let _ = create_plan(
///     &virtuals,
///     &[],
///     &HashMap::new(),
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
    managed: &[ManagedResource],
    data_sources: &[DataSource],
    current_states: &HashMap<ResourceId, State>,
    directives_map: &HashMap<ResourceId, Directives>,
    registry: &SchemaRegistry,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
    prev_explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
    orphan_dependencies: &HashMap<ResourceId, BTreeSet<String>>,
    wait_bindings: &[WaitBinding],
) -> Plan {
    let mut plan = Plan::new();

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
            .unwrap_or_else(|| State::not_found(resource.id.clone()));

        let saved = saved_attrs.get(&resource.id);
        let prev_explicit_for_resource = prev_explicit.get(&resource.id);
        let schema = registry.get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Managed,
        );
        let d = diff(
            resource,
            &current,
            saved,
            prev_explicit_for_resource,
            schema,
        );

        match d {
            Diff::Create(r) => plan.add(Effect::Create(r)),
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

                // Check if the resource type forces replacement (no update support)
                let schema_force_replace = registry
                    .get(
                        &resource.id.provider,
                        &resource.id.resource_type,
                        SchemaKind::Managed,
                    )
                    .is_some_and(|s| s.force_replace);

                if changed_create_only.is_empty() && !schema_force_replace {
                    plan.add(Effect::Update {
                        id,
                        from,
                        to,
                        changed_attributes,
                    });
                } else {
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
                    let temporary_name = if directives.create_before_destroy {
                        registry
                            .get(
                                &resource.id.provider,
                                &resource.id.resource_type,
                                SchemaKind::Managed,
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

                    plan.add(Effect::Replace {
                        id,
                        from,
                        to,
                        directives,
                        changed_create_only,
                        cascading_updates: vec![],
                        temporary_name,
                        cascade_ref_hints: vec![],
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
                    .and_then(|s| s.identifier.clone())
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
                });
            }
        }
    }

    // Detect orphaned resources: exist in current_states but not in desired
    for (id, state) in current_states {
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
                let temp_resource = ManagedResource {
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
            });
        }
    }

    // Lower each `wait` declaration into an `Effect::Wait`.
    //
    // Target resolution: the parser has already pinned the target name;
    // here we look it up in `desired` (the live resource set after
    // forward-ref resolution) to recover the resolved `ResourceId`. If
    // the target is missing — typo, scoped-out binding, etc. — the
    // wait is skipped with a plan-level error so downstream resources
    // referencing the wait binding still fail loudly.
    for wb in wait_bindings {
        // Wait targets resolve over managed + data-source bindings.
        // Virtuals are post-apply attribute containers and don't carry
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
        let attr = AttrPath {
            segments: wb.until_predicate.lhs_segments[1..].to_vec(),
        };
        let until = WaitPredicate::Equals {
            attr,
            value: wb.until_predicate.rhs.clone(),
        };
        let target_id = target_id_resolved;
        // The target's identifier is only known at plan time if it
        // already exists in state. When the target is created/updated
        // in this same run it has no prior state, so the executor must
        // resolve the real identifier from the just-applied state — see
        // [`WaitTarget`] and carina#3119.
        let target = match current_states
            .get(&target_id)
            .and_then(|s| s.identifier.clone())
        {
            Some(id) => WaitTarget::Known(id),
            None => WaitTarget::ResolvedAtApply,
        };
        let schema = registry.get(
            &target_id.provider,
            &target_id.resource_type,
            SchemaKind::Managed,
        );
        let schema_timeout = schema.and_then(|s| s.default_wait_timeout);
        let schema_interval = schema.and_then(|s| s.default_wait_interval);
        let timeout = wb
            .timeout_secs
            .map(std::time::Duration::from_secs)
            .or(schema_timeout)
            .unwrap_or(WAIT_DEFAULT_TIMEOUT);
        let interval = schema_interval.unwrap_or(WAIT_DEFAULT_INTERVAL);
        plan.add(Effect::Wait {
            // Lower BindingName -> String at the AST→Effect seam: the
            // executor IR (Effect) is string-keyed and is the separate
            // type tracked for its own newtype migration (carina#3066).
            binding: wb.binding.as_str().to_string(),
            target_id,
            target,
            until,
            until_surface: wb.until_raw.clone(),
            timeout,
            interval,
            explicit_dependencies: wb
                .depends_on
                .iter()
                .map(|d| d.as_str().to_string())
                .collect(),
        });
    }

    plan
}

/// Populate cascading updates for Replace effects with create_before_destroy.
///
/// When a resource is replaced with create_before_destroy, dependent resources
/// that reference the replaced resource's computed attributes must be updated
/// between the create (new) and delete (old) steps. This function:
///
/// 1. Finds all Replace effects with create_before_destroy = true
/// 2. Identifies dependent resources that reference the replaced resource's binding
/// 3. If the referencing attribute is create-only on the dependent (per the registry),
///    promotes the dependent to its own Replace effect in the plan
/// 4. Otherwise, adds a CascadingUpdate entry to the parent Replace effect
///
/// `unresolved_resources` should be the resources BEFORE ref resolution (still containing
/// ResourceRef values). `current_states` provides the `from` state for each dependent.
/// `registry` provides attribute metadata to detect create-only attributes.
pub fn cascade_dependent_updates(
    plan: &mut Plan,
    unresolved_managed: &[ManagedResource],
    current_states: &HashMap<ResourceId, State>,
    registry: &SchemaRegistry,
) {
    // Build binding/key -> unresolved resource mapping.
    // Uses the same key logic as the dependent lookup below so anonymous resources
    // (without _binding) are also found.
    let mut binding_to_unresolved: HashMap<String, &ManagedResource> = HashMap::new();
    for resource in unresolved_managed {
        let key = resource
            .binding
            .clone()
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name_str()));
        binding_to_unresolved.insert(key, resource);
    }

    // Auto-detect create_before_destroy: if a Replace effect has dependents
    // among the unresolved resources, promote it to create_before_destroy.
    // This must happen before collecting replaced_bindings so that the
    // promoted effects are picked up by the existing cascade logic.
    {
        // Collect all Replace bindings (regardless of CBD flag) with their resource IDs
        let all_replace_bindings: HashMap<String, ResourceId> = plan
            .effects()
            .iter()
            .filter_map(|e| {
                if let Effect::Replace { id, directives, .. } = e {
                    // Only consider Replace effects that are NOT already CBD
                    if !directives.create_before_destroy {
                        e.binding_name().map(|b| (b, id.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if !all_replace_bindings.is_empty() {
            for resource in unresolved_managed {
                let deps = get_resource_dependencies(resource);
                for dep in &deps {
                    if let Some(resource_id) = all_replace_bindings.get(dep) {
                        plan.promote_to_create_before_destroy(resource_id);
                    }
                }
            }
        }
    }

    // Build reverse dependency map: replaced_binding -> [dependent_bindings]
    let mut dependents_of_replaced: HashMap<String, Vec<String>> = HashMap::new();

    // Collect binding names of resources being replaced (now includes auto-promoted ones)
    let replaced_bindings: HashSet<String> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Replace { .. } = e {
                return e.binding_name();
            }
            None
        })
        .collect();

    if replaced_bindings.is_empty() {
        return;
    }

    // Collect resource IDs that already have effects in the plan
    let planned_ids: HashSet<ResourceId> = plan
        .effects()
        .iter()
        .map(|e| e.resource_id().clone())
        .collect();

    // For each unresolved resource, check if it depends on a replaced binding.
    // Resources already in the plan are handled separately below.
    for resource in unresolved_managed {
        if planned_ids.contains(&resource.id) {
            continue;
        }

        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if replaced_bindings.contains(dep) {
                let binding = resource.binding.clone().unwrap_or_else(|| {
                    format!("{}:{}", resource.id.resource_type, resource.id.name_str())
                });
                dependents_of_replaced
                    .entry(dep.clone())
                    .or_default()
                    .push(binding);
            }
        }
    }

    // For resources already in the plan, check if cascade-triggered create-only
    // attributes need to be merged into their existing effects.
    let mut merge_operations: Vec<CascadeMerge> = Vec::new();

    for resource in unresolved_managed {
        if !planned_ids.contains(&resource.id) {
            continue;
        }

        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if !replaced_bindings.contains(dep) {
                continue;
            }

            // Find which attributes on this resource hold a ResourceRef
            // pointing to the replaced binding, and extract ref hints
            let ref_attrs: Vec<String> = resource
                .attributes
                .iter()
                .filter(|(_, v)| matches!(v, Value::Deferred(DeferredValue::ResourceRef{ path }) if path.binding() == dep))
                .map(|(k, _)| k.clone())
                .collect();

            let ref_hints: Vec<(String, String)> = resource
                .attributes
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Deferred(DeferredValue::ResourceRef { path })
                        if path.binding() == dep =>
                    {
                        Some((
                            k.clone(),
                            format!("{}.{}", path.binding(), path.attribute()),
                        ))
                    }
                    _ => None,
                })
                .collect();

            // Check if any of those attributes are create-only
            let create_only_refs = find_changed_create_only(
                &resource.id.provider,
                &resource.id.resource_type,
                &ref_attrs,
                registry,
            );

            if !create_only_refs.is_empty() {
                // Check if this merge would upgrade an Update to Replace.
                // If the resource has prevent_destroy, block the upgrade.
                let is_update_in_plan = plan
                    .effects()
                    .iter()
                    .any(|e| matches!(e, Effect::Update { id, .. } if *id == resource.id));
                if is_update_in_plan && resource.directives.prevent_destroy {
                    plan.add_error(PlanError {
                        resource_id: resource.id.clone(),
                        message:
                            "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
                                .to_string(),
                    });
                    continue;
                }

                // Only keep hints for attributes that are actually create-only
                let filtered_hints: Vec<(String, String)> = ref_hints
                    .into_iter()
                    .filter(|(attr, _)| create_only_refs.contains(attr))
                    .collect();
                merge_operations.push(CascadeMerge {
                    resource_id: resource.id.clone(),
                    create_only_attrs: create_only_refs,
                    directives: resource.directives.clone(),
                    ref_hints: filtered_hints,
                });
            }
        }
    }

    // Apply merge operations to existing effects
    for merge in merge_operations {
        plan.merge_cascade_create_only(
            &merge.resource_id,
            merge.create_only_attrs,
            merge.directives,
            merge.ref_hints,
        );
    }

    // Build cascading updates for each Replace effect.
    // Dependents whose affected attributes are create-only get promoted to
    // their own Replace effect instead of being added as a CascadingUpdate.
    let mut updates_by_replaced_binding: HashMap<String, Vec<CascadingUpdate>> = HashMap::new();
    let mut promoted_replaces: Vec<Effect> = Vec::new();

    for (replaced_binding, dependent_bindings) in &dependents_of_replaced {
        for dep_binding in dependent_bindings {
            if let Some(unresolved) = binding_to_unresolved.get(dep_binding) {
                let from = current_states
                    .get(&unresolved.id)
                    .cloned()
                    .unwrap_or_else(|| State::not_found(unresolved.id.clone()));

                if !from.exists {
                    continue;
                }

                // Find which attributes on this dependent hold a ResourceRef
                // pointing to the replaced binding
                let ref_attrs: Vec<String> = unresolved
                    .attributes
                    .iter()
                    .filter(|(_, v)| {
                        matches!(v, Value::Deferred(DeferredValue::ResourceRef{ path }) if path.binding() == replaced_binding)
                    })
                    .map(|(k, _)| k.clone())
                    .collect();

                // Check if any of those attributes are create-only
                let create_only_refs = find_changed_create_only(
                    &unresolved.id.provider,
                    &unresolved.id.resource_type,
                    &ref_attrs,
                    registry,
                );

                if create_only_refs.is_empty() {
                    // Normal cascading update
                    updates_by_replaced_binding
                        .entry(replaced_binding.clone())
                        .or_default()
                        .push(CascadingUpdate {
                            id: unresolved.id.clone(),
                            from: Box::new(from),
                            to: (*unresolved).clone(),
                        });
                } else if unresolved.directives.prevent_destroy {
                    // Cascade would promote to Replace (destroy + recreate),
                    // but prevent_destroy blocks this.
                    plan.add_error(PlanError {
                        resource_id: unresolved.id.clone(),
                        message:
                            "resource has prevent_destroy set, but cascade from a replaced dependency would replace it (which requires destroying the old resource)"
                                .to_string(),
                    });
                } else {
                    // Extract ref hints for attributes being promoted
                    let ref_hints: Vec<(String, String)> = unresolved
                        .attributes
                        .iter()
                        .filter_map(|(k, v)| match v {
                            Value::Deferred(DeferredValue::ResourceRef { path })
                                if path.binding() == replaced_binding
                                    && create_only_refs.contains(k) =>
                            {
                                Some((
                                    k.clone(),
                                    format!("{}.{}", path.binding(), path.attribute()),
                                ))
                            }
                            _ => None,
                        })
                        .collect();

                    // Promote to a separate Replace effect
                    promoted_replaces.push(Effect::Replace {
                        id: unresolved.id.clone(),
                        from: Box::new(from),
                        to: (*unresolved).clone(),
                        directives: unresolved.directives.clone(),
                        changed_create_only: create_only_refs,
                        cascading_updates: vec![],
                        temporary_name: None,
                        cascade_ref_hints: ref_hints,
                    });
                }
            }
        }
    }

    // Apply cascading updates to the plan's Replace effects
    plan.set_cascading_updates(&replaced_bindings, &updates_by_replaced_binding);

    // Add promoted Replace effects to the plan
    for effect in promoted_replaces {
        plan.add(effect);
    }
}
