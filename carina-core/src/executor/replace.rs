//! Replace effect execution: Create-Before-Destroy (CBD) and Destroy-Before-Create (DBD).

use std::collections::HashMap;
use std::time::Instant;

use crate::effect::Effect;
use crate::provider::Provider;
use crate::resource::{Resource, ResourceId, State, Value};

use super::basic::{
    BasicEffectResult, resolve_resource, resolve_resource_with_source, update_binding_map,
};
use super::{ExecutionEvent, ExecutionObserver, ProgressInfo};

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
}

/// Context for executing a Replace effect in the parallel path.
///
/// Groups the resource data, lifecycle configuration, and execution metadata
/// that are passed to both CBD and DBD replace functions.
pub(super) struct ReplaceContext<'a> {
    pub(super) effect: &'a Effect,
    pub(super) id: &'a ResourceId,
    pub(super) from: &'a State,
    pub(super) to: &'a Resource,
    pub(super) lifecycle: &'a crate::resource::LifecycleConfig,
    pub(super) cascading_updates: &'a [crate::effect::CascadingUpdate],
    pub(super) temporary_name: Option<&'a crate::effect::TemporaryName>,
    pub(super) binding_map: &'a HashMap<String, HashMap<String, Value>>,
    pub(super) unresolved: &'a HashMap<ResourceId, Resource>,
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
    if ctx.lifecycle.create_before_destroy {
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
    let resolved = match resolve_resource(ctx.to, ctx.binding_map) {
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

    match provider.create(&resolved).await {
        Ok(state) => {
            // Build a local binding map update for cascade resolution
            let mut local_binding_map = ctx.binding_map.clone();
            update_binding_map(
                &mut local_binding_map,
                &resolved.resolved_attributes(),
                ctx.to.binding.as_deref(),
                &state,
            );

            // Execute cascading updates
            let mut cascade_failed = false;
            for cascade in ctx.cascading_updates {
                let resolved_to = match resolve_resource(&cascade.to, &local_binding_map) {
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
                match provider
                    .update(&cascade.id, cascade_identifier, &cascade.from, &resolved_to)
                    .await
                {
                    Ok(cascade_state) => {
                        observer
                            .on_event(&ExecutionEvent::CascadeUpdateSucceeded { id: &cascade.id });
                        update_binding_map(
                            &mut local_binding_map,
                            &resolved_to.resolved_attributes(),
                            cascade.to.binding.as_deref(),
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
            match provider.delete(ctx.id, identifier, ctx.lifecycle).await {
                Ok(()) => {
                    // Handle rename
                    let mut permanent_overrides = None;
                    let mut final_state = state.clone();
                    let mut rename_failed = false;

                    if let Some(temp) = ctx.temporary_name
                        && temp.can_rename
                    {
                        let new_identifier = state.identifier.as_deref().unwrap_or("");
                        let mut rename_to = ctx.to.clone();
                        rename_to.set_attr(
                            temp.attribute.clone(),
                            Value::String(temp.original_value.clone()),
                        );
                        match provider
                            .update(ctx.id, new_identifier, &state, &rename_to)
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
                            resolved_attrs: Some(resolved.resolved_attributes()),
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
                            resolved_attrs: Some(resolved.resolved_attributes()),
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

    match provider.delete(ctx.id, identifier, ctx.lifecycle).await {
        Ok(()) => {
            let resolve_source = ctx.unresolved.get(&ctx.to.id).unwrap_or(ctx.to);
            let resolved =
                match resolve_resource_with_source(ctx.to, resolve_source, ctx.binding_map) {
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
            match provider.create(&resolved).await {
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
                        resolved_attrs: Some(resolved.resolved_attributes()),
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
