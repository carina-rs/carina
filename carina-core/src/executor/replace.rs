//! Replace effect execution: Create-Before-Destroy (CBD) and Destroy-Before-Create (DBD).

use std::collections::HashMap;
use std::time::Instant;

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::executor::normalized::NormalizedResource;
use crate::provider::{
    CreateRequest, DeleteRequest, PatchOp, PatchOpKind, Provider, UpdatePatch, UpdateRequest,
    build_update_patch,
};
use crate::resource::{ConcreteValue, Resource, ResourceId, State, Value};

use super::basic::{
    BasicEffectResult, RenormalizePipeline, resolve_resource, resolve_resource_with_source,
};
use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};

/// Build a full attribute-diff [`UpdatePatch`] between an existing
/// `from` state and a desired `to` resource, used by the cascade
/// path of replacements (cascade has no precomputed
/// `changed_attributes` list, so the patch is derived from the
/// from/to comparison directly).
pub(super) fn compute_full_diff_patch(from: &State, to: &NormalizedResource) -> UpdatePatch {
    use std::collections::HashSet;

    let to_resource = to.as_resource();
    let mut keys: HashSet<&str> = HashSet::new();
    keys.extend(from.attributes.keys().map(String::as_str));
    keys.extend(to_resource.attributes.keys().map(String::as_str));
    let mut sorted_keys: Vec<&str> = keys.into_iter().collect();
    sorted_keys.sort();

    let changed: Vec<String> = sorted_keys
        .into_iter()
        .filter(|k| from.attributes.get(*k) != to_resource.attributes.get(*k))
        .map(|k| k.to_string())
        .collect();
    build_update_patch(&changed, to, from)
}

/// Build a single-attribute [`UpdatePatch`] for the rename path of
/// CBD replace, where exactly one attribute is being flipped from
/// the temporary value back to the original.
pub(super) fn single_attribute_patch(key: String, value: Value) -> UpdatePatch {
    UpdatePatch {
        ops: vec![PatchOp {
            kind: PatchOpKind::Replace,
            key,
            value: Some(value),
        }],
    }
}

/// Result of executing a single effect.
pub(super) enum SingleEffectResult {
    /// Create/Update/Delete completed (wraps BasicEffectResult)
    Basic(BasicEffectResult),
    Replace {
        success: bool,
        state: Option<State>,
        resource_id: ResourceId,
        resolved_attrs: Option<HashMap<String, Value>>,
        binding: Option<String>,
        refreshes: Vec<(ResourceId, String)>,
        permanent_overrides: Option<(ResourceId, HashMap<String, String>)>,
    },
    ReadNoOp,
    /// `Effect::Wait` execution outcome. On success carries the
    /// captured target state so the parallel scheduler can register it
    /// under the wait binding for downstream resolution. On failure
    /// carries the wait binding so dependents can be marked failed.
    Wait {
        success: bool,
        binding: String,
        target_state: Option<State>,
    },
}

/// Context for executing a Replace effect in the parallel path.
///
/// Groups the resource data, directives, and execution metadata
/// that are passed to both CBD and DBD replace functions.
pub(super) struct ReplaceContext<'a> {
    pub(super) effect: &'a Effect,
    pub(super) id: &'a ResourceId,
    pub(super) from: &'a State,
    pub(super) to: &'a Resource,
    pub(super) directives: &'a crate::resource::Directives,
    pub(super) cascading_updates: &'a [crate::effect::CascadingUpdate],
    pub(super) temporary_name: Option<&'a crate::effect::TemporaryName>,
    pub(super) bindings: &'a ResolvedBindings,
    pub(super) unresolved: &'a HashMap<ResourceId, Resource>,
    pub(super) pipeline: &'a RenormalizePipeline<'a>,
    pub(super) started: Instant,
    pub(super) progress: ProgressInfo,
}

/// Execute a Replace effect, returning a `SingleEffectResult`.
///
/// This handles both CBD and DBD replace within the parallel execution path.
/// It does not mutate shared state directly; instead returns all data needed
/// for the caller to update shared state after the level completes.
pub(super) async fn execute_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    if ctx.directives.create_before_destroy {
        execute_cbd_replace_parallel(provider, ctx, observer).await
    } else {
        execute_dbd_replace_parallel(provider, ctx, observer).await
    }
}

/// CBD Replace for the parallel execution path.
pub(super) async fn execute_cbd_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let resolved = match resolve_resource(ctx.to, ctx.bindings, ctx.pipeline).await {
        Ok(r) => r,
        Err(e) => {
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &e,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            return SingleEffectResult::Basic(BasicEffectResult::Failure {
                binding: ctx.effect.binding_name(),
                refresh: None,
            });
        }
    };
    let mut refreshes = Vec::new();

    match provider
        .create(
            &ctx.to.id,
            CreateRequest {
                resource: resolved.as_resource().clone(),
            },
        )
        .await
    {
        Ok(state) => {
            // Build a local bindings clone for cascade resolution
            let mut local_bindings = ctx.bindings.clone();
            local_bindings.record_applied(
                ctx.to.binding.as_deref(),
                &resolved.as_resource().resolved_attributes(),
                &state,
            );

            // Execute cascading updates
            let mut cascade_failed = false;
            for cascade in ctx.cascading_updates {
                let resolved_to = match resolve_resource(&cascade.to, &local_bindings, ctx.pipeline)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        observer.on_event(&ExecutionEvent::CascadeUpdateFailed {
                            id: &cascade.id,
                            error: &e,
                        });
                        let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                        refreshes.push((cascade.id.clone(), cascade_identifier.to_string()));
                        cascade_failed = true;
                        break;
                    }
                };
                let cascade_identifier = cascade.from.identifier.as_deref().unwrap_or("");
                let cascade_patch = compute_full_diff_patch(&cascade.from, &resolved_to);
                let cascade_request = UpdateRequest {
                    from: (*cascade.from).clone(),
                    patch: cascade_patch,
                };
                match provider
                    .update(&cascade.id, cascade_identifier, cascade_request)
                    .await
                {
                    Ok(cascade_state) => {
                        observer
                            .on_event(&ExecutionEvent::CascadeUpdateSucceeded { id: &cascade.id });
                        local_bindings.record_applied(
                            cascade.to.binding.as_deref(),
                            &resolved_to.as_resource().resolved_attributes(),
                            &cascade_state,
                        );
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        observer.on_event(&ExecutionEvent::CascadeUpdateFailed {
                            id: &cascade.id,
                            error: &error_str,
                        });
                        refreshes.push((cascade.id.clone(), cascade_identifier.to_string()));
                        cascade_failed = true;
                        break;
                    }
                }
            }

            if cascade_failed {
                refreshes.push((
                    ctx.to.id.clone(),
                    state.identifier.clone().unwrap_or_default(),
                ));
                return SingleEffectResult::Replace {
                    success: false,
                    state: None,
                    resource_id: ctx.to.id.clone(),
                    resolved_attrs: None,
                    binding: ctx.effect.binding_name(),
                    refreshes,
                    permanent_overrides: None,
                };
            }

            // Delete the old resource
            let identifier = ctx.from.identifier.as_deref().unwrap_or("");
            match provider
                .delete(
                    ctx.id,
                    identifier,
                    DeleteRequest {
                        directives: ctx.directives.clone(),
                    },
                )
                .await
            {
                Ok(()) => {
                    // Handle rename
                    let mut permanent_overrides = None;
                    let mut final_state = state.clone();
                    let mut rename_failed = false;

                    if let Some(temp) = ctx.temporary_name
                        && temp.can_rename
                    {
                        let new_identifier = state.identifier.as_deref().unwrap_or("");
                        let rename_patch = single_attribute_patch(
                            temp.attribute.clone(),
                            Value::Concrete(ConcreteValue::String(temp.original_value.clone())),
                        );
                        let rename_request = UpdateRequest {
                            from: state.clone(),
                            patch: rename_patch,
                        };
                        match provider
                            .update(ctx.id, new_identifier, rename_request)
                            .await
                        {
                            Ok(renamed_state) => {
                                observer.on_event(&ExecutionEvent::RenameSucceeded {
                                    id: ctx.id,
                                    from: &temp.temporary_value,
                                    to: &temp.original_value,
                                });
                                final_state = renamed_state;
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                observer.on_event(&ExecutionEvent::RenameFailed {
                                    id: ctx.id,
                                    error: &error_str,
                                });
                                rename_failed = true;
                            }
                        }
                    } else if let Some(temp) = ctx.temporary_name
                        && !temp.can_rename
                    {
                        let mut overrides = HashMap::new();
                        overrides.insert(temp.attribute.clone(), temp.temporary_value.clone());
                        permanent_overrides = Some((ctx.to.id.clone(), overrides));
                    }

                    if rename_failed {
                        observer.on_event(&ExecutionEvent::EffectFailed {
                            effect: ctx.effect,
                            error: "rename failed",
                            duration: ctx.started.elapsed(),
                            progress: ctx.progress,
                        });
                        SingleEffectResult::Replace {
                            success: false,
                            state: Some(final_state),
                            resource_id: ctx.to.id.clone(),
                            resolved_attrs: Some(resolved.as_resource().resolved_attributes()),
                            binding: ctx.effect.binding_name(),
                            refreshes,

                            permanent_overrides,
                        }
                    } else {
                        observer.on_event(&ExecutionEvent::EffectSucceeded {
                            effect: ctx.effect,
                            state: None,
                            duration: ctx.started.elapsed(),
                            progress: ctx.progress,
                        });
                        SingleEffectResult::Replace {
                            success: true,
                            state: Some(final_state),
                            resource_id: ctx.to.id.clone(),
                            resolved_attrs: Some(resolved.as_resource().resolved_attributes()),
                            binding: ctx.to.binding.clone(),
                            refreshes,

                            permanent_overrides,
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: ctx.effect,
                        error: &error_str,
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    refreshes.push((
                        ctx.to.id.clone(),
                        state.identifier.clone().unwrap_or_default(),
                    ));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: None,
                        binding: ctx.effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &error_str,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: ctx.to.id.clone(),
                resolved_attrs: None,
                binding: ctx.effect.binding_name(),
                refreshes,

                permanent_overrides: None,
            }
        }
    }
}

/// DBD Replace for the parallel execution path.
pub(super) async fn execute_dbd_replace_parallel(
    provider: &dyn Provider,
    ctx: &ReplaceContext<'_>,
    observer: &dyn ExecutionObserver,
) -> SingleEffectResult {
    let identifier = ctx.from.identifier.as_deref().unwrap_or("");
    let mut refreshes = Vec::new();

    match provider
        .delete(
            ctx.id,
            identifier,
            DeleteRequest {
                directives: ctx.directives.clone(),
            },
        )
        .await
    {
        Ok(()) => {
            let resolve_source = ctx.unresolved.get(&ctx.to.id).unwrap_or(ctx.to);
            let resolved = match resolve_resource_with_source(
                ctx.to,
                resolve_source,
                ctx.bindings,
                ctx.pipeline,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: ctx.effect,
                        error: &e,
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    refreshes.push((ctx.to.id.clone(), identifier.to_string()));
                    return SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: None,
                        binding: ctx.effect.binding_name(),
                        refreshes,
                        permanent_overrides: None,
                    };
                }
            };
            match provider
                .create(
                    &ctx.to.id,
                    CreateRequest {
                        resource: resolved.as_resource().clone(),
                    },
                )
                .await
            {
                Ok(state) => {
                    observer.on_event(&ExecutionEvent::EffectSucceeded {
                        effect: ctx.effect,
                        state: Some(&state),
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    SingleEffectResult::Replace {
                        success: true,
                        state: Some(state),
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: Some(resolved.as_resource().resolved_attributes()),
                        binding: ctx.to.binding.clone(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    observer.on_event(&ExecutionEvent::EffectFailed {
                        effect: ctx.effect,
                        error: &error_str,
                        duration: ctx.started.elapsed(),
                        progress: ctx.progress,
                    });
                    refreshes.push((ctx.to.id.clone(), identifier.to_string()));
                    SingleEffectResult::Replace {
                        success: false,
                        state: None,
                        resource_id: ctx.to.id.clone(),
                        resolved_attrs: None,
                        binding: ctx.effect.binding_name(),
                        refreshes,

                        permanent_overrides: None,
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            observer.on_event(&ExecutionEvent::EffectFailed {
                effect: ctx.effect,
                error: &error_str,
                duration: ctx.started.elapsed(),
                progress: ctx.progress,
            });
            refreshes.push((ctx.id.clone(), identifier.to_string()));
            SingleEffectResult::Replace {
                success: false,
                state: None,
                resource_id: ctx.to.id.clone(),
                resolved_attrs: None,
                binding: ctx.effect.binding_name(),
                refreshes,

                permanent_overrides: None,
            }
        }
    }
}
