//! Rhs-driven type inference for `exports { }` declarations whose
//! `type_expr` annotation is omitted.
//!
//! [`apply_inference`] is the bridge from `ParsedFile` to `InferredFile`.
//! Each inferred export retains its declared/inferred origin and any typed
//! unchecked reason directly on the parameter. The loader runs this once per directory load so
//! downstream consumers (CLI validate, LSP diagnostics, LSP
//! completion) see a definitive type per export instead of having to
//! re-derive it. See `notes/specs/2026-05-03-typeexpr-stage2-design.md`.
//!
//! Inference rules:
//!
//! - `Value::Concrete(ConcreteValue::String)/Int/Bool/Float` → corresponding `TypeExpr` primitive.
//! - `Value::Concrete(ConcreteValue::List(items))` → `List(T)` where `T` is the unified element
//!   type. Heterogeneous lists fail inference.
//! - `Value::Concrete(ConcreteValue::Map(entries))` → `Map(T)` where `T` unifies value types.
//! - `Value::Deferred(DeferredValue::ResourceRef{ binding, attribute, field_path })` → walk
//!   the binding's resource schema, descend `field_path`, convert the
//!   reached `AttributeType` into a `TypeExpr`.
//! - `Value::Deferred(DeferredValue::Interpolation(_))` → `TypeExpr::String` (interpolation
//!   results are always strings; users who need a specific Custom
//!   identity must annotate to narrow).
//! - `Value::Deferred(DeferredValue::Secret(_))` → the wrapped value's type; sensitivity is
//!   orthogonal to its list/map/scalar shape.
//! - `Value::Deferred(DeferredValue::FunctionCall{ name, .. })` → consult `builtins`'s
//!   `BuiltinReturnType`. `String/Int/List/Map/Secret` map to the
//!   corresponding `TypeExpr` primitive, while `Secret` preserves its argument type; `Any` (`lookup`, `min`,
//!   `max`, `map`) is "depends on arguments" — counts as inference
//!   failure.

use std::collections::HashMap;

use crate::builtins::{BuiltinReturnType, builtin_functions};
use crate::parser::TypeExpr;
use crate::provider::ProviderLoadedSchemaRegistry;
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, PlaceholderKind,
    RelativeBindingName, Value,
};
use crate::schema::{
    AttrTypeKind, CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema,
    SchemaKind, SchemaRegistry,
};
pub use crate::static_type::InferenceError;
use crate::static_type::{
    ExportStaticType, InferenceIssue, InferredUncheckedReason, NotInferableReason, StaticType,
};

/// One entry in the binding map. Resource bindings (`let <name> = <provider>.<resource> { ... }`)
/// carry the provider/resource type and are inferred through; `upstream_state`
/// bindings are recognized by name but cannot be projected to a `TypeExpr`
/// today (the export's type lives in the upstream's `parsed.export_params`,
/// which the inferer doesn't recursively resolve here). Distinguishing the
/// two lets the inferer turn a true typo (`mian.vpc_id`) into a hard error
/// while still passing through `upstream_state`-derived references.
///
/// `Virtual` carries the attribute map of a module-call's
/// composition resource so an `exports` reference of the form
/// `<module_call>.<attr>` can transitively infer through the bound
/// expression on the module's `attributes { ... }` block, instead of
/// failing with `unknown binding` (#2493).
#[derive(Debug, Clone)]
pub enum InferenceBinding<
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = RelativeBindingName,
> {
    Resource {
        provider: String,
        resource_type: String,
    },
    UpstreamState,
    Virtual {
        attributes: indexmap::IndexMap<String, Value<P, B>>,
    },
    /// Module-call binding registered before module expansion.
    /// Inference treats it like `UpstreamState` (known-but-non-
    /// inferable) so a downstream `${<call>.<attr>}` doesn't fire a
    /// false `unknown binding` at load time. Post-expansion the
    /// matching `Virtual` resource lands in `parsed.resources` and
    /// `bindings_from_parts` overwrites this entry with a `Virtual`
    /// that can actually project to a `TypeExpr`. #2493.
    ModuleCall,
    /// A value parameter declared by an `arguments {}` block. Parser-side
    /// expressions carry a `Placeholder(ModuleArgument)` until the
    /// caller value is substituted; retaining the declaration here lets
    /// postfix access be checked before expansion.
    Argument {
        type_expr: TypeExpr,
    },
    /// A user-defined function whose call may remain deferred until provider
    /// schemas are available. Its annotation is still enough authority to
    /// infer an export without evaluating the call or dropping its pending
    /// argument checks.
    UserFunction {
        return_type: Option<TypeExpr>,
    },
}

pub type InferenceBindings<P = NoPlaceholder, B = RelativeBindingName> =
    HashMap<String, InferenceBinding<P, B>>;

/// Build an [`InferenceBindings`] map from a `ParsedFile`. Convenience
/// over [`bindings_from_parts`] for the common case of having a parsed
/// file in hand.
pub fn bindings_from_parsed<E, P, T, D, B>(
    parsed: &crate::parser::File<E, P, T, D, B>,
) -> InferenceBindings<P, B>
where
    P: PlaceholderKind + PartialEq,
    T: PlaceholderKind,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
{
    // carina#3181: pass the typed top-level slices directly so
    // `bindings_from_parts` classifies managed resources, data sources,
    // and (post-expansion) composition rows from their own kinds.
    let mut out = bindings_from_parts(
        &parsed.resources,
        &parsed.data_sources,
        &parsed.compositions,
        &parsed.upstream_states,
    );
    // Pre-expansion (load_configuration), `parsed.resources` does not
    // yet hold the `Virtual` resources that `expand_module_call`
    // synthesises for each module-call binding. Register the binding
    // names here as `ModuleCall` so a downstream
    // `${<call>.<attr>}` reference is treated as known-but-non-
    // inferable (mirroring `upstream_state`) instead of falsely
    // flagged as `unknown binding`. Post-expansion the `Virtual` arm
    // takes over and resolves the type properly. #2493.
    for call in &parsed.module_calls {
        if let Some(name) = &call.binding_name
            && !out.contains_key(name.as_str())
        {
            out.insert(name.to_string(), InferenceBinding::ModuleCall);
        }
    }
    for argument in &parsed.arguments {
        out.entry(argument.name.clone())
            .or_insert_with(|| InferenceBinding::Argument {
                type_expr: argument.static_type.clone(),
            });
    }
    for function in parsed.user_functions.values() {
        out.entry(function.name.clone())
            .or_insert_with(|| InferenceBinding::UserFunction {
                return_type: function.return_type.clone(),
            });
    }
    out
}

/// Build an [`InferenceBindings`] map from already-extracted resource
/// and upstream-state slices. Used by callers that already work in
/// terms of the slice arguments (e.g. `validate_export_param_ref_types_with_bindings`)
/// so the binding-collection logic is in one place.
///
/// Both resource bindings (`let <name> = <provider>.<resource> { ... }`)
/// and `upstream_state` bindings (`let <name> = upstream_state { ... }`)
/// are included; the latter are tagged so the inferer can distinguish
/// "known but non-inferable" from a typo.
///
/// Insert order is upstream-states first, resources second, so a
/// resource binding wins on collision with an `upstream_state` of the
/// same name. The directory loader's duplicate-declaration channel is
/// the validation gate for these cross-file collisions, but during a
/// mid-edit the LSP deliberately keeps the merged parse alive — preferring
/// the resource means downstream inference still gets the precise type
/// instead of degrading to `NonInferableBinding`. Two resource bindings
/// with the same name likewise remain deterministic here while the loader
/// diagnostic reports the collision to the user.
pub fn bindings_from_parts<S, P: PlaceholderKind + PartialEq, B: BindingAddress>(
    resources: &[crate::resource::Resource<S, P, B>],
    data_sources: &[crate::resource::DataSource<S, P, B>],
    compositions: &[crate::resource::LiveComposition<P, B>],
    upstream_states: &[crate::parser::UpstreamState<B>],
) -> InferenceBindings<P, B> {
    let mut out = InferenceBindings::new();
    for us in upstream_states {
        out.insert(us.binding.to_string(), InferenceBinding::UpstreamState);
    }
    // carina#3181: managed resources and data sources both carry a
    // provider identity, so each maps to `InferenceBinding::Resource`
    // (a `read`-bound `${data_source.attr}` still resolves its type via
    // a schema lookup). Virtual resources synthesised by module-call
    // expansion (`expand_module_call`) carry no provider identity — their
    // attributes are projections from the module's `attributes { ... }`
    // block — so they are tagged `Virtual` and `infer_resource_ref`
    // recurses into the bound expression instead of a schema lookup that
    // would always fail. #2493.
    for resource in resources {
        let Some(name) = &resource.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Resource {
                provider: resource.id.provider.clone(),
                resource_type: resource.id.resource_type.clone(),
            },
        );
    }
    for data_source in data_sources {
        let Some(name) = &data_source.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Resource {
                provider: data_source.id.provider.clone(),
                resource_type: data_source.id.resource_type.clone(),
            },
        );
    }
    for composition in compositions {
        let Some(name) = &composition.binding else {
            continue;
        };
        out.insert(
            name.to_string(),
            InferenceBinding::Virtual {
                attributes: composition
                    .signature
                    .attributes
                    .iter()
                    .map(|(k, attr)| (k.clone(), attr.to_value()))
                    .collect(),
            },
        );
    }
    out
}

/// Try to synthesize a `TypeExpr` from a `Value`. Returns `Err` if the
/// value's type cannot be statically determined — the caller is
/// expected to either fall back to a user-supplied annotation or
/// surface the error as "type annotation required" at the offending
/// field.
pub fn infer_type_from_value<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &InferenceBindings<P, B>,
    schemas: &SchemaRegistry,
) -> Result<TypeExpr, InferenceIssue> {
    infer_type_from_value_with_provider_context(
        value,
        bindings,
        schemas,
        crate::parser::ProviderContext::unloaded(),
    )
}

/// Infer with the same provider catalog that owns the binding/type check.
pub fn infer_type_from_value_with_provider_context<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &InferenceBindings<P, B>,
    schemas: &SchemaRegistry,
    provider_context: &crate::parser::ProviderContext,
) -> Result<TypeExpr, InferenceIssue> {
    let mut visiting = indexmap::IndexSet::new();
    infer_type_from_value_with_visiting(
        value,
        bindings,
        InferenceSchemaContext {
            schemas,
            provider_context,
        },
        &mut visiting,
    )
}

#[derive(Clone, Copy)]
struct InferenceSchemaContext<'a> {
    schemas: &'a SchemaRegistry,
    provider_context: &'a crate::parser::ProviderContext,
}

fn heterogeneous_collection_error(first: &TypeExpr, next: &TypeExpr) -> InferenceError {
    InferenceError::HeterogeneousCollection {
        reason: format!(
            "collection element types disagree: `{}` vs `{}`",
            first, next
        ),
    }
}

/// Infer only from the value's own shape and builtin metadata.
///
/// This explicitly has no binding/schema authority: references therefore
/// remain unknown, while literals and computed values whose return shape is
/// intrinsic (for example `split(...)[0]`) can still be checked. Callers use
/// this instead of manufacturing empty binding/schema registries.
pub(crate) fn infer_type_from_value_without_bindings<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> Result<TypeExpr, InferenceIssue> {
    fn infer_collection_without_bindings<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
        values: impl IntoIterator<Item = &'a Value<P, B>>,
        wrap: impl FnOnce(Box<TypeExpr>) -> TypeExpr,
    ) -> Result<TypeExpr, InferenceIssue> {
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return Err(InferenceError::UnknownType {
                reason: "empty collection has no inferable element type".to_string(),
            }
            .into());
        };
        let element = infer_type_from_value_without_bindings(first)?;
        for value in values {
            let next = infer_type_from_value_without_bindings(value)?;
            if next != element {
                return Err(heterogeneous_collection_error(&element, &next).into());
            }
        }
        Ok(wrap(Box::new(element)))
    }

    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let then_type = infer_type_from_value_without_bindings(deferred.then_value())?;
            let else_type = infer_type_from_value_without_bindings(deferred.else_value())?;
            if then_type == else_type {
                Ok(then_type)
            } else {
                Err(heterogeneous_collection_error(&then_type, &else_type).into())
            }
        }
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Deferred(DeferredValue::Interpolation(_)) => Ok(TypeExpr::String),
        Value::Concrete(ConcreteValue::Int(_)) => Ok(TypeExpr::Int),
        // The result shape is intrinsic even when this deliberately
        // authority-free inferer cannot prove deferred operands. Final
        // directory validation uses `infer_type_from_value` below and rejects
        // every operand that is not proven Int.
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => Ok(TypeExpr::Int),
        Value::Concrete(ConcreteValue::Float(_)) => Ok(TypeExpr::Float),
        Value::Concrete(ConcreteValue::Bool(_)) => Ok(TypeExpr::Bool),
        Value::Concrete(ConcreteValue::Duration(_)) => Ok(TypeExpr::Duration),
        Value::Concrete(ConcreteValue::List(items)) => {
            infer_collection_without_bindings(items, TypeExpr::List)
        }
        Value::Concrete(ConcreteValue::StringList(_)) => {
            Ok(TypeExpr::List(Box::new(TypeExpr::String)))
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            infer_collection_without_bindings(entries.values(), TypeExpr::Map)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            infer_type_from_value_without_bindings(inner)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            let left = infer_type_from_value_without_bindings(value)?;
            match infer_type_from_value_without_bindings(default) {
                Ok(right) if right == left => Ok(left),
                Ok(right) => Err(InferenceError::IncompatibleCoalesceTypes {
                    left: left.to_string(),
                    default: right.to_string(),
                }
                .into()),
                Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
                    if matches!(
                        (default.as_ref(), &left),
                        (
                            Value::Concrete(ConcreteValue::List(items)),
                            TypeExpr::List(_)
                        ) if items.is_empty()
                    ) || matches!(
                        (default.as_ref(), &left),
                        (
                            Value::Concrete(ConcreteValue::Map(entries)),
                            TypeExpr::Map(_)
                        ) if entries.is_empty()
                    ) =>
                {
                    Ok(left)
                }
                Err(error) => Err(error),
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, args, .. }
            | DeferredValue::Unavailable { name, args, .. },
        ) => {
            let return_type = builtin_functions()
                .iter()
                .find(|function| function.name == name.as_str())
                .map(|function| function.return_type)
                .ok_or_else(|| {
                    InferenceIssue::from(InferenceError::UnknownType {
                        reason: format!("unknown built-in function `{name}`"),
                    })
                })?;
            match return_type {
                BuiltinReturnType::String => Ok(TypeExpr::String),
                BuiltinReturnType::Int => Ok(TypeExpr::Int),
                BuiltinReturnType::StringList => Ok(TypeExpr::List(Box::new(TypeExpr::String))),
                BuiltinReturnType::Secret => args.first().map_or_else(
                    || {
                        Err(InferenceError::UnknownType {
                            reason: format!("built-in `{name}` has no argument to wrap as secret"),
                        }
                        .into())
                    },
                    |argument| infer_type_from_value_without_bindings(argument.walker_value()),
                ),
                BuiltinReturnType::List | BuiltinReturnType::Map | BuiltinReturnType::Any => {
                    Err(InferenceError::UnknownType {
                        reason: format!("built-in `{name}` return type depends on arguments"),
                    }
                    .into())
                }
            }
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            function.return_type.clone().ok_or_else(|| {
                InferenceIssue::from(InferenceError::UnknownType {
                    reason: format!(
                        "user function `{}` has no declared return type",
                        function.name
                    ),
                })
            })
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let base_type = infer_type_from_value_without_bindings(access.base())?;
            super::narrow_type_expr(&base_type, std::slice::from_ref(access.step()), None)
                .ok_or_else(|| {
                    InferenceIssue::from(InferenceError::InvalidAccess {
                        base_type: base_type.to_string(),
                        step: access.step().clone(),
                    })
                })
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            match placeholder.inference_type_authority() {
                crate::resource::PlaceholderTypeAuthority::MissingExpression => {
                    Err(InferenceError::UnknownType {
                        reason: placeholder.to_string(),
                    }
                    .into())
                }
                crate::resource::PlaceholderTypeAuthority::Binding(_)
                | crate::resource::PlaceholderTypeAuthority::AwaitingSubstitution => Err(
                    InferenceIssue::NotInferable(NotInferableReason::PlaceholderTypeUnavailable {
                        placeholder: placeholder.to_string(),
                    }),
                ),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => Err(InferenceError::UnknownType {
            reason: "value requires binding/schema authority".to_string(),
        }
        .into()),
    }
}

/// Internal core of [`infer_type_from_value`] threading the
/// currently-being-resolved `Virtual` binding chain through the
/// recursive calls. Cycle detection lives on the `Virtual` arm of
/// [`infer_resource_ref_with_visiting`] (#2497); every other arm just
/// forwards the set unchanged. The set is scope-popped when each
/// `Virtual` recursion returns, so unrelated sibling traversals (two
/// list elements that each resolve through different `Virtual`
/// bindings) do not see each other.
fn infer_type_from_value_with_visiting<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    bindings: &InferenceBindings<P, B>,
    context: InferenceSchemaContext<'_>,
    visiting: &mut indexmap::IndexSet<String>,
) -> Result<TypeExpr, InferenceIssue> {
    let schemas = context.schemas;
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let then_type = infer_type_from_value_with_visiting(
                deferred.then_value(),
                bindings,
                context,
                visiting,
            )?;
            let else_type = infer_type_from_value_with_visiting(
                deferred.else_value(),
                bindings,
                context,
                visiting,
            )?;
            if then_type == else_type {
                Ok(then_type)
            } else {
                Err(heterogeneous_collection_error(&then_type, &else_type).into())
            }
        }
        Value::Concrete(ConcreteValue::String(_)) => Ok(TypeExpr::String),
        // Enum identifiers infer as `String` for the type-expression
        // layer — the value-layer distinction matters only inside the
        // `Enum` validator. See carina#2986.
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => Ok(TypeExpr::String),
        Value::Concrete(ConcreteValue::CanonicalEnum(_)) => Ok(TypeExpr::String),
        Value::Concrete(ConcreteValue::Int(_)) => Ok(TypeExpr::Int),
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = require_proven_integer_operand(
                infer_type_from_value_with_visiting(expression.lhs(), bindings, context, visiting),
                expression.op(),
                crate::integer_arithmetic::IntegerOperandSide::Left,
                expression.operator_span(),
            )?;
            let rhs = require_proven_integer_operand(
                infer_type_from_value_with_visiting(expression.rhs(), bindings, context, visiting),
                expression.op(),
                crate::integer_arithmetic::IntegerOperandSide::Right,
                expression.operator_span(),
            )?;
            require_integer_arithmetic_operands(
                expression.op(),
                expression.operator_span(),
                &lhs,
                &rhs,
                context,
            )
        }
        Value::Concrete(ConcreteValue::Float(_)) => Ok(TypeExpr::Float),
        Value::Concrete(ConcreteValue::Bool(_)) => Ok(TypeExpr::Bool),
        Value::Concrete(ConcreteValue::Duration(_)) => Ok(TypeExpr::Duration),
        Value::Deferred(DeferredValue::Interpolation(_)) => Ok(TypeExpr::String),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            infer_type_from_value_with_visiting(inner, bindings, context, visiting)
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Err(InferenceError::UnknownType {
            reason: "saved-plan secret has not been rehydrated".to_string(),
        }
        .into()),
        Value::Concrete(ConcreteValue::List(items)) => {
            infer_collection(items, bindings, context, visiting, TypeExpr::List)
        }
        Value::Concrete(ConcreteValue::StringList(_)) => {
            Ok(TypeExpr::List(Box::new(TypeExpr::String)))
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            infer_collection(entries.values(), bindings, context, visiting, TypeExpr::Map)
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => infer_resource_ref_with_visiting(
            path.binding(),
            path.attribute(),
            path.segments(),
            bindings,
            context,
            visiting,
        ),
        // `BindingRef` is a bare-binding placeholder with no attribute
        // and therefore no inferable position. Inference treats it as
        // "type unknown until accessed" — same shape as `Unknown(_)`.
        Value::Deferred(DeferredValue::BindingRef { .. }) => Err(InferenceError::UnknownType {
            reason: "bare binding reference has no attribute to infer".to_string(),
        }
        .into()),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            let left = infer_type_from_value_with_visiting(value, bindings, context, visiting)?;
            match infer_type_from_value_with_visiting(default, bindings, context, visiting) {
                Ok(right) if right == left => Ok(left),
                Ok(right) => Err(InferenceError::IncompatibleCoalesceTypes {
                    left: left.to_string(),
                    default: right.to_string(),
                }
                .into()),
                Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
                    if matches!(
                        (default.as_ref(), &left),
                        (
                            Value::Concrete(ConcreteValue::List(items)),
                            TypeExpr::List(_)
                        ) if items.is_empty()
                    ) || matches!(
                        (default.as_ref(), &left),
                        (
                            Value::Concrete(ConcreteValue::Map(entries)),
                            TypeExpr::Map(_)
                        ) if entries.is_empty()
                    ) =>
                {
                    Ok(left)
                }
                Err(error) => Err(error),
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, args, .. }
            | DeferredValue::Unavailable { name, args, .. },
        ) => infer_function_call(name.as_str(), args, bindings, context, visiting),
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            function.return_type.clone().ok_or_else(|| {
                InferenceIssue::from(InferenceError::UnknownType {
                    reason: format!(
                        "user function `{}` has no declared return type",
                        function.name
                    ),
                })
            })
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let base_type =
                infer_type_from_value_with_visiting(access.base(), bindings, context, visiting)?;
            super::narrow_type_expr(
                &base_type,
                std::slice::from_ref(access.step()),
                Some(schemas),
            )
            .ok_or_else(|| {
                InferenceIssue::from(InferenceError::InvalidAccess {
                    base_type: base_type.to_string(),
                    step: access.step().clone(),
                })
            })
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            let binding = match placeholder.inference_type_authority() {
                crate::resource::PlaceholderTypeAuthority::Binding(name) => bindings.get(name),
                crate::resource::PlaceholderTypeAuthority::AwaitingSubstitution => None,
                crate::resource::PlaceholderTypeAuthority::MissingExpression => {
                    return Err(InferenceError::UnknownType {
                        reason: placeholder.to_string(),
                    }
                    .into());
                }
            };
            match binding {
                Some(InferenceBinding::Argument { type_expr }) => Ok(type_expr.clone()),
                // Anonymous loop slots intentionally have no lexical binding
                // name. Named parser slots can also precede the scope that
                // supplies their declaration. Neither case proves bad access;
                // substitution/materialization supplies the value authority.
                Some(InferenceBinding::Resource { .. })
                | Some(InferenceBinding::UpstreamState)
                | Some(InferenceBinding::Virtual { .. })
                | Some(InferenceBinding::ModuleCall)
                | Some(InferenceBinding::UserFunction { .. })
                | None => Err(InferenceIssue::NotInferable(
                    NotInferableReason::PlaceholderTypeUnavailable {
                        placeholder: placeholder.to_string(),
                    },
                )),
            }
        }
        Value::Deferred(DeferredValue::Unknown(_)) => Err(InferenceError::UnknownType {
            reason: "value not yet known at plan time (unresolved upstream)".to_string(),
        }
        .into()),
    }
}

fn require_proven_integer_operand(
    inferred: Result<TypeExpr, InferenceIssue>,
    op: crate::integer_arithmetic::IntegerOp,
    side: crate::integer_arithmetic::IntegerOperandSide,
    operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
) -> Result<TypeExpr, InferenceIssue> {
    match inferred {
        Ok(inferred) => Ok(inferred),
        Err(
            error @ InferenceIssue::Failed(InferenceError::InvalidIntegerArithmeticOperand {
                ..
            }),
        ) => Err(error),
        Err(
            error @ InferenceIssue::NotInferable(NotInferableReason::CustomTypeSchemaUnavailable {
                ..
            }),
        ) => Err(error),
        Err(error) => Err(InferenceError::InvalidIntegerArithmeticOperand {
            op,
            side,
            actual: format!("unproven type ({error})"),
            operator_span,
        }
        .into()),
    }
}

fn require_integer_arithmetic_operands(
    op: crate::integer_arithmetic::IntegerOp,
    operator_span: crate::integer_arithmetic::IntegerOperatorSpan,
    lhs: &TypeExpr,
    rhs: &TypeExpr,
    context: InferenceSchemaContext<'_>,
) -> Result<TypeExpr, InferenceIssue> {
    let schemas = context.schemas;
    for (side, operand) in [
        (crate::integer_arithmetic::IntegerOperandSide::Left, lhs),
        (crate::integer_arithmetic::IntegerOperandSide::Right, rhs),
    ] {
        let operand = crate::parser::resolve_type_expr_from_schemas(
            operand,
            schemas,
            context.provider_context,
        )
        .map_err(|error| {
            InferenceIssue::from(InferenceError::UnknownType {
                reason: error.to_string(),
            })
        })?;
        if inferred_scalar_kind(&operand).map_err(InferenceIssue::NotInferable)?
            != Some(crate::schema::RefinedScalarKind::Int)
        {
            return Err(InferenceError::InvalidIntegerArithmeticOperand {
                op,
                side,
                actual: operand.to_string(),
                operator_span,
            }
            .into());
        }
    }
    Ok(TypeExpr::Int)
}

fn inferred_scalar_kind(
    inferred: &TypeExpr,
) -> Result<Option<crate::schema::RefinedScalarKind>, NotInferableReason> {
    use crate::schema::RefinedScalarKind;

    Ok(match inferred {
        TypeExpr::UnloadedSchemaType(unloaded) => {
            return Err(NotInferableReason::CustomTypeSchemaUnavailable {
                identity: unloaded.identity().clone(),
            });
        }
        TypeExpr::String | TypeExpr::StringLiteral(_) => Some(RefinedScalarKind::String),
        TypeExpr::Int => Some(RefinedScalarKind::Int),
        TypeExpr::Float => Some(RefinedScalarKind::Float),
        TypeExpr::SchemaType { owned } => Some(owned.scalar_kind()),
        TypeExpr::Simple(name) => crate::provider::identified_custom_type_scalar_kind(
            name.declaration(),
        )
        .map_err(|unloaded| NotInferableReason::CustomTypeSchemaUnavailable {
            identity: unloaded.identity().clone(),
        })?,
        TypeExpr::Bool
        | TypeExpr::Duration
        | TypeExpr::List(_)
        | TypeExpr::Map(_)
        | TypeExpr::Ref(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::Struct { .. }
        | TypeExpr::Union(_) => None,
    })
}

fn infer_collection<'a, I, F, P, B>(
    items: I,
    bindings: &InferenceBindings<P, B>,
    context: InferenceSchemaContext<'_>,
    visiting: &mut indexmap::IndexSet<String>,
    wrap: F,
) -> Result<TypeExpr, InferenceIssue>
where
    I: IntoIterator<Item = &'a Value<P, B>>,
    F: FnOnce(Box<TypeExpr>) -> TypeExpr,
    P: PlaceholderKind + 'a,
    B: BindingAddress + 'a,
{
    // All-or-nothing: every element must infer cleanly and to the
    // same type. Tolerating dynamic siblings ("the concrete one
    // carries the type") was tempting but biases toward silent
    // acceptance of type-confused collections — a `[lookup(...),
    // "extra"]` where the user meant `lookup` to return `Int` would
    // lock the export to `String` with no warning. The project
    // prefers loud failures; the user who wants the loose semantics
    // can add an explicit annotation to the export.
    let mut iter = items.into_iter();
    let first = match iter.next() {
        Some(v) => infer_type_from_value_with_visiting(v, bindings, context, visiting)?,
        None => {
            return Err(InferenceError::UnknownType {
                reason: "empty collection — element type cannot be inferred".to_string(),
            }
            .into());
        }
    };
    for next in iter {
        let next_type = infer_type_from_value_with_visiting(next, bindings, context, visiting)?;
        if next_type != first {
            return Err(heterogeneous_collection_error(&first, &next_type).into());
        }
    }
    Ok(wrap(Box::new(first)))
}

fn infer_resource_ref_with_visiting<P: PlaceholderKind, B: BindingAddress>(
    binding: &str,
    attribute: &str,
    segments: &[crate::resource::PathSegment],
    bindings: &InferenceBindings<P, B>,
    context: InferenceSchemaContext<'_>,
    visiting: &mut indexmap::IndexSet<String>,
) -> Result<TypeExpr, InferenceIssue> {
    let schemas = context.schemas;
    let target = match bindings.get(binding) {
        Some(InferenceBinding::Resource {
            provider,
            resource_type,
        }) => (provider.as_str(), resource_type.as_str()),
        Some(InferenceBinding::UpstreamState)
        | Some(InferenceBinding::ModuleCall)
        | Some(InferenceBinding::Argument { .. })
        | Some(InferenceBinding::UserFunction { .. }) => {
            return Err(InferenceIssue::NotInferable(
                NotInferableReason::NonInferableBinding {
                    binding: binding.to_string(),
                },
            ));
        }
        Some(InferenceBinding::Virtual { attributes }) => {
            // The module-call's composition resource holds the module's
            // `attributes { ... }` projections directly. Recurse on the
            // bound expression so the type flows transitively through
            // the inner resource's schema. #2493.
            //
            let inner = attributes.get(attribute).ok_or_else(|| {
                InferenceIssue::from(InferenceError::UnknownAttribute {
                    binding: binding.to_string(),
                    attribute: attribute.to_string(),
                })
            })?;
            // #2497: register the binding in `visiting` before the
            // recursive descend; on a cycle (`a -> b -> a`) the second
            // entry's `insert` returns `false` and we surface the
            // chain as a `CyclicVirtualBinding` diagnostic instead of
            // looping until stack exhaustion. Pop the binding back off
            // once the recursion returns so unrelated sibling subtrees
            // (e.g. two list elements that each resolve through
            // different `Virtual` bindings) do not see each other.
            // Use `shift_remove` (not `swap_remove`) so the cycle
            // diagnostic preserves entry order if the next traversal
            // re-enters after this pop.
            if !visiting.insert(binding.to_string()) {
                let mut cycle: Vec<String> = visiting.iter().cloned().collect();
                cycle.push(binding.to_string());
                return Err(InferenceError::CyclicVirtualBinding { cycle }.into());
            }
            let inner_type_result =
                infer_type_from_value_with_visiting(inner, bindings, context, visiting);
            visiting.shift_remove(binding);
            let inner_type = inner_type_result?;
            return super::narrow_type_expr(&inner_type, segments, Some(schemas)).ok_or_else(
                || {
                    InferenceIssue::from(InferenceError::UnknownAttribute {
                        binding: binding.to_string(),
                        attribute: attribute.to_string(),
                    })
                },
            );
        }
        None => {
            return Err(InferenceError::UnknownBinding {
                binding: binding.to_string(),
            }
            .into());
        }
    };
    let Some(schema) = lookup_schema(schemas, target.0, target.1) else {
        return Err(InferenceIssue::NotInferable(
            NotInferableReason::SchemaUnavailable {
                binding: binding.to_string(),
            },
        ));
    };
    let Some(attr_schema) = schema.attributes.get(attribute) else {
        return Err(InferenceError::UnknownAttribute {
            binding: binding.to_string(),
            attribute: attribute.to_string(),
        }
        .into());
    };
    // Walk the path's ordered segments — a free mix of `.field` and
    // `[idx]` continuations (carina#3025). A `.field` descends into the
    // current struct; an `[idx]` peels one collection layer (`list(T)
    // → T`, `map(_, V) → V`). Any mismatch surfaces a typed error
    // rather than silently dropping the path.
    use crate::resource::{PathSegment, Subscript};
    let mut current = &attr_schema.attr_type;
    for seg in segments {
        // Resolve a `Ref` chain at each step so Subscript arms below
        // see the underlying List / Map / Struct shape instead of the
        // opaque `Ref` (carina#3340). `descend_struct_field` already
        // resolves internally for the Field arm; pre-resolving here
        // also keeps the Subscript arms shape-matched.
        let resolved = current.resolve_refs_with_defs(&schema.defs).as_attr();
        current = match (seg, &resolved.kind) {
            (PathSegment::Field { name }, AttrTypeKind::Struct { .. }) => {
                match descend_struct_field(resolved, name, &schema.defs) {
                    Some(t) => t,
                    None => {
                        return Err(InferenceError::UnknownAttribute {
                            binding: binding.to_string(),
                            attribute: format!("{}.{}", attribute, name),
                        }
                        .into());
                    }
                }
            }
            (PathSegment::Field { .. }, AttrTypeKind::Map { value, .. }) => value,
            (
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
                AttrTypeKind::List {
                    element_type: inner,
                    ..
                },
            ) => inner,
            (
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
                AttrTypeKind::Map { value, .. },
            ) => value,
            _ => {
                return Err(InferenceError::InvalidAccess {
                    base_type: attribute_type_to_type_expr(resolved, target.0).to_string(),
                    step: seg.clone(),
                }
                .into());
            }
        };
    }
    // A `Union` at the *resolved leaf* of the access path carries
    // multiple alternatives — projecting it to a single `TypeExpr`
    // would silently let a value satisfy branches it shouldn't.
    // Demand the user write an annotation so the intended branch is
    // explicit.
    //
    // The check is outermost-only on purpose: real provider schemas
    // bury `Union` inside larger structs (e.g. IAM policy documents:
    // `policy_document` is a `Struct{statement: List<Struct{principal:
    // Union<...>, action: Union<...>, resource: Union<...>, ...}>}`).
    // Flagging every nested union would block legitimate
    // `policy_document = main.policy.policy_document` references that
    // forward the whole struct without naming the inner union branch.
    // The user can only "pick a branch" for a union they're directly
    // accessing, not for one buried inside a transferred value.
    // Resolve any trailing `Ref` so the Union check and the
    // TypeExpr projection see the underlying shape (carina#3340).
    let current = current.resolve_refs_with_defs(&schema.defs).as_attr();
    if matches!(&current.kind, AttrTypeKind::Union(_)) {
        return Err(InferenceError::UnknownType {
            reason: format!(
                "rhs `{}.{}` resolves to a union type; annotate the export to pick a branch",
                binding, attribute
            ),
        }
        .into());
    }
    Ok(attribute_type_to_type_expr(current, target.0))
}

fn lookup_schema<'a>(
    schemas: &'a SchemaRegistry,
    provider: &str,
    resource_type: &str,
) -> Option<&'a ResourceSchema> {
    schemas
        .get(provider, resource_type, SchemaKind::Resource)
        .or_else(|| schemas.get(provider, resource_type, SchemaKind::DataSource))
        .map(|schema| schema.schema().as_schema())
}

fn descend_struct_field<'a>(
    attr_type: &'a AttributeType,
    field: &str,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    // Resolve any leading `Ref` chain so a reference path inside a
    // cyclic CFN struct (`WebACL.Statement -> AndStatement -> ...`)
    // can be followed across the cycle (carina#3340).
    match attr_type.shape_with_defs(defs) {
        crate::schema::Shape::Struct { .. } => {
            crate::schema::struct_fields_with_defs(attr_type, defs)
                .expect("Shape::Struct must expose struct fields internally")
                .iter()
                .find(|f| f.name == field)
                .map(|f| &f.field_type)
        }
        _ => None,
    }
}

/// Convert an `AttributeType` (schema side) into a `TypeExpr`
/// (declaration side). Used both by `ResourceRef` inference and by
/// recursive descent into `List`/`Map`/`Struct` schema receivers.
///
/// Notable mappings:
/// - `Custom { identity: Some(id), .. }` → `TypeExpr::SchemaType`, including
///   bare semantic identities. The schema owner must survive inference so a
///   downstream sink never falls back to identity-only routing.
/// - `Custom { identity: None, .. }` → falls through to the base
///   type; an anonymous Custom is structurally a wrapper and carries
///   no identity to project.
/// - closed `Enum` (`values: Some`) → `TypeExpr::String`
/// - dynamic `Enum` (`values: None` with provider transform or host
///   validator) → `TypeExpr::SchemaType`, preserving the typed
///   identity old CustomEnum-origin schemas carried before enum
///   unification.
pub(super) fn attribute_type_to_type_expr(
    attr_type: &AttributeType,
    schema_owner: &str,
) -> TypeExpr {
    let scalar_identity = match attr_type.kind() {
        AttrTypeKind::UnloadedSchemaType(unloaded) => {
            return TypeExpr::UnloadedSchemaType(unloaded.clone());
        }
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. } => binding.identity(),
        AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. }
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. }
        | AttrTypeKind::Union(_)
        | AttrTypeKind::Ref(_) => None,
    };
    if scalar_identity.is_some() {
        return TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::from_declaration(
                schema_owner,
                crate::parser::IdentifiedCustomTypeDeclaration::new(attr_type)
                    .expect("identified scalar declaration"),
            ),
        };
    }

    match attr_type.kind() {
        AttrTypeKind::UnloadedSchemaType(unloaded) => {
            TypeExpr::UnloadedSchemaType(unloaded.clone())
        }
        AttrTypeKind::String { .. } => TypeExpr::String,
        AttrTypeKind::ResourceHandle {
            provider,
            resource_type,
            ..
        } => TypeExpr::Ref(crate::parser::ResourceTypePath::new(
            provider.clone(),
            resource_type.clone(),
        )),
        AttrTypeKind::StringLiteralSet { values, .. } => match values.as_slice() {
            [value] => TypeExpr::StringLiteral(value.clone()),
            values => TypeExpr::Union(
                values
                    .iter()
                    .cloned()
                    .map(TypeExpr::StringLiteral)
                    .collect(),
            ),
        },
        AttrTypeKind::Int { .. } => TypeExpr::Int,
        AttrTypeKind::Float { .. } => TypeExpr::Float,
        AttrTypeKind::Bool => TypeExpr::Bool,
        AttrTypeKind::Duration => TypeExpr::Duration,
        AttrTypeKind::Enum {
            values,
            validate,
            to_dsl,
            ..
        } if values.is_none() && (validate.is_some() || to_dsl.is_some()) => TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::from_declaration(
                schema_owner,
                crate::parser::IdentifiedCustomTypeDeclaration::new(attr_type)
                    .expect("identified enum declaration"),
            ),
        },
        AttrTypeKind::Enum { .. } => TypeExpr::String,
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => TypeExpr::List(Box::new(attribute_type_to_type_expr(inner, schema_owner))),
        AttrTypeKind::Map { value, .. } => {
            TypeExpr::Map(Box::new(attribute_type_to_type_expr(value, schema_owner)))
        }
        AttrTypeKind::Struct { fields, .. } => TypeExpr::Struct {
            fields: fields
                .iter()
                .map(|f| {
                    (
                        f.name.clone(),
                        attribute_type_to_type_expr(&f.field_type, schema_owner),
                    )
                })
                .collect(),
        },
        // A union receiver has no single TypeExpr that captures it —
        // collapsing to one member would silently let a value satisfy
        // alternatives it actually doesn't. Returning `String` as a
        // sentinel matches today's `attribute_type_to_type_expr`
        // contract (must return a TypeExpr) but consumers that care
        // about precision should detect the union upstream and emit
        // a "type annotation required" error instead of trusting this
        // result.
        AttrTypeKind::Union(_) => TypeExpr::String,
        // `Ref` requires the enclosing `Schema` to resolve — this pure
        // attribute-type-to-type-expr conversion does not have one in
        // scope. Returning `String` as a sentinel matches the
        // `Union(_)` arm: callers that need precision must resolve the
        // `Ref` via `Schema::resolve` upstream and convert the result.
        // The carina#3340 model emits `Ref` only at cycle points in
        // generated schemas, so this fallback path is unreachable for
        // hand-written or codegen-produced attribute types reached
        // through normal validation flows.
        AttrTypeKind::Ref(_) => TypeExpr::String,
    }
}

fn infer_function_call<P: PlaceholderKind, B: BindingAddress>(
    name: &str,
    args: &[crate::resource::FunctionArgument<P, B>],
    bindings: &InferenceBindings<P, B>,
    context: InferenceSchemaContext<'_>,
    visiting: &mut indexmap::IndexSet<String>,
) -> Result<TypeExpr, InferenceIssue> {
    let return_type = builtin_functions()
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.return_type);
    let Some(return_type) = return_type else {
        return match bindings.get(name) {
            Some(InferenceBinding::UserFunction {
                return_type: Some(return_type),
            }) => Ok(return_type.clone()),
            Some(InferenceBinding::UserFunction { return_type: None }) => {
                Err(InferenceError::UnknownType {
                    reason: format!("user-defined function `{name}` has no declared return type"),
                }
                .into())
            }
            _ => Err(InferenceError::UnknownType {
                reason: format!("unknown built-in function `{name}`"),
            }
            .into()),
        };
    };
    match return_type {
        BuiltinReturnType::String => Ok(TypeExpr::String),
        BuiltinReturnType::Int => Ok(TypeExpr::Int),
        BuiltinReturnType::List => Err(InferenceError::UnknownType {
            reason: format!(
                "built-in `{}` returns a list whose element type depends on arguments",
                name
            ),
        }
        .into()),
        BuiltinReturnType::StringList => Ok(TypeExpr::List(Box::new(TypeExpr::String))),
        BuiltinReturnType::Map => Err(InferenceError::UnknownType {
            reason: format!(
                "built-in `{}` returns a map whose value type depends on arguments",
                name
            ),
        }
        .into()),
        BuiltinReturnType::Secret => args.first().map_or_else(
            || {
                Err(InferenceError::UnknownType {
                    reason: format!("built-in `{name}` has no argument to wrap as secret"),
                }
                .into())
            },
            |inner| {
                infer_type_from_value_with_visiting(
                    inner.walker_value(),
                    bindings,
                    context,
                    visiting,
                )
            },
        ),
        BuiltinReturnType::Any => Err(InferenceError::UnknownType {
            reason: format!("built-in `{}` return type depends on arguments", name),
        }
        .into()),
    }
}

/// Phase transition (#2360 stage 2): attach declared/inferred origin and any
/// typed unchecked reason to every export.
pub fn apply_inference<D, B: BindingAddress>(
    mut parsed: crate::parser::File<
        crate::parser::ParsedExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &SchemaRegistry,
) -> crate::parser::File<
    crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>,
    crate::resource::ParserPlaceholder,
    crate::resource::ParserPlaceholder,
    D,
    B,
>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    for export in &parsed.export_params {
        parsed.parser_binding_metadata.insert_export_type_source(
            export.name.clone(),
            export.source_file().to_path_buf(),
            export.source_span(),
        );
    }
    let inferred_exports = infer_export_params(&parsed, schemas).into_parameters();
    // Single phase-axis surface (carina#3126): only `export_params`
    // changes phase; every other field moves through `map_export_params`
    // so a new `File<E>` field cannot be silently dropped on the
    // parser→inferred relabel.
    parsed.map_export_params(|_| inferred_exports)
}

/// Export parameters inferred with a caller-supplied schema registry.
#[derive(Debug, Clone)]
pub struct SchemaCheckedExportParams<B: BindingAddress = RelativeBindingName> {
    parameters: Vec<crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>>,
}

impl<B: BindingAddress> SchemaCheckedExportParams<B> {
    pub fn parameters(
        &self,
    ) -> &[crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>] {
        &self.parameters
    }

    fn into_parameters(
        self,
    ) -> Vec<crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>> {
        self.parameters
    }
}

/// Borrowing variant of [`apply_inference`] that returns just the
/// inferred export parameters. The full
/// `InferredFile` reconstruction in `apply_inference` clones every
/// non-export field; consumers that only need the export-side typecheck
/// (LSP diagnostics, in particular) should use this instead to avoid
/// the per-keystroke deep-clone.
pub fn infer_export_params<D, B: BindingAddress>(
    parsed: &crate::parser::File<
        crate::parser::ParsedExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &SchemaRegistry,
) -> SchemaCheckedExportParams<B>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    let bindings = bindings_from_parsed(parsed);
    let parameters = parsed
        .export_params
        .iter()
        .map(|parameter| {
            let static_type = match &parameter.static_type {
                Some(type_expr) => ExportStaticType::Declared(type_expr.clone()),
                None => match &parameter.value {
                    None => ExportStaticType::Inferred(StaticType::Unchecked(
                        InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue),
                    )),
                    Some(value) => match infer_type_from_value(value, &bindings, schemas) {
                        Ok(type_expr) => ExportStaticType::Inferred(StaticType::Known(type_expr)),
                        Err(issue) => {
                            ExportStaticType::Inferred(StaticType::Unchecked(issue.into()))
                        }
                    },
                },
            };
            crate::parser::InferredExportParam {
                name: parameter.name.clone(),
                static_type,
                value: parameter.value.clone(),
            }
        })
        .collect();
    SchemaCheckedExportParams { parameters }
}

/// Export parameters whose inference used a registry minted by provider loading.
#[derive(Debug, Clone)]
pub struct ExportParamsInferredWithLoadedProviderSchemas<B: BindingAddress = RelativeBindingName> {
    inferred: SchemaCheckedExportParams<B>,
}

impl<B: BindingAddress> ExportParamsInferredWithLoadedProviderSchemas<B> {
    pub fn parameters(
        &self,
    ) -> &[crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>] {
        self.inferred.parameters()
    }

    pub fn checked_exports(&self) -> ExportsInferredWithLoadedProviderSchemas<'_, B> {
        ExportsInferredWithLoadedProviderSchemas {
            parameters: self.parameters(),
        }
    }

    pub fn into_inferred_export_params(self) -> SchemaCheckedExportParams<B> {
        self.inferred
    }
}

/// Infer export parameters using schemas proven to come from provider loading.
pub fn infer_export_params_with_loaded_provider_schemas<D, B: BindingAddress>(
    parsed: &crate::parser::File<
        crate::parser::ParsedExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &ProviderLoadedSchemaRegistry,
) -> ExportParamsInferredWithLoadedProviderSchemas<B>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    ExportParamsInferredWithLoadedProviderSchemas {
        inferred: infer_export_params(parsed, schemas.schemas()),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AttachedInferenceFailure<'a> {
    pub name: &'a str,
    pub error: &'a InferenceError,
}

/// Proof-bearing export view whose inference ran against loaded provider schemas.
#[derive(Debug, Clone, Copy)]
pub struct ExportsInferredWithLoadedProviderSchemas<'a, B: BindingAddress = RelativeBindingName> {
    parameters: &'a [crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>],
}

pub fn attached_inference_failures<'a, B: BindingAddress>(
    checked: ExportsInferredWithLoadedProviderSchemas<'a, B>,
) -> impl Iterator<Item = AttachedInferenceFailure<'a>> + 'a {
    checked
        .parameters
        .iter()
        .filter_map(|parameter| match &parameter.static_type {
            ExportStaticType::Declared(_type_expr) => None,
            ExportStaticType::Inferred(StaticType::Known(_type_expr)) => None,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(_reason),
            )) => None,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(error),
            )) => Some(AttachedInferenceFailure {
                name: &parameter.name,
                error,
            }),
        })
}

/// Retry inferred exports whose original reason was not having enough schema
/// authority. A newly reportable failure replaces that reason; successful or
/// still-deferred probes retain the original provenance.
pub fn retry_unchecked_export_inference<'a, D, B: BindingAddress>(
    parsed: &'a mut crate::parser::File<
        crate::parser::InferredExportParam<crate::resource::ParserPlaceholder, B>,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &ProviderLoadedSchemaRegistry,
) -> ExportsInferredWithLoadedProviderSchemas<'a, B>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    let bindings = bindings_from_parsed(parsed);

    for parameter in &mut parsed.export_params {
        let retry = match &parameter.static_type {
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(
                    NotInferableReason::MissingValue
                    | NotInferableReason::PlaceholderTypeUnavailable { .. },
                ),
            )) => false,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(
                    NotInferableReason::NonInferableBinding { .. }
                    | NotInferableReason::SchemaUnavailable { .. }
                    | NotInferableReason::CustomTypeSchemaUnavailable { .. },
                ),
            )) => true,
            ExportStaticType::Declared(_type_expr) => false,
            ExportStaticType::Inferred(StaticType::Known(_type_expr)) => false,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(_error),
            )) => false,
        };
        if !retry {
            continue;
        }
        let Some(value) = &parameter.value else {
            continue;
        };

        match infer_type_from_value(value, &bindings, schemas.schemas()) {
            Ok(_type_expr) => {}
            Err(InferenceIssue::NotInferable(_reason)) => {}
            Err(InferenceIssue::Failed(error)) => {
                parameter.static_type = ExportStaticType::Inferred(StaticType::Unchecked(
                    InferredUncheckedReason::InferenceFailed(error),
                ));
            }
        }
    }

    ExportsInferredWithLoadedProviderSchemas {
        parameters: &parsed.export_params,
    }
}

/// Format an inference error pointing at the offending export. The shared CLI
/// validation pipeline and the LSP
/// (`carina-lsp/src/diagnostics/checks.rs`) both surface this same
/// "type annotation required" wording so editor and command line stay
/// in parity (the existing e2e parity tests pin this).
pub fn format_inference_error(name: &str, err: &InferenceError) -> String {
    format!("export '{}': type annotation required: {}", name, err)
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;
    use crate::parser::ModuleCall;
    use crate::resource::{
        AuthoredAccessPath as AccessPath, AuthoredConcreteValue as ConcreteValue,
        AuthoredDeferredValue as DeferredValue, AuthoredInterpolationPart as InterpolationPart,
        AuthoredValue as Value, ParserPlaceholder, RelativeBindingName,
    };
    use crate::schema::{
        CompiledAttributeType as AttributeType, SchemaRegistry, TypeIdentity, legacy_validator,
    };

    #[test]
    fn parser_placeholders_without_type_authority_are_not_inferable() {
        use crate::resource::{AccessMissPolicy, LoopSlot, PathSegment, apply_postfix_access};

        fn assert_pending<P: PlaceholderKind>(placeholder: P) {
            let value: crate::resource::Value<P, RelativeBindingName> = apply_postfix_access(
                crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                    placeholder,
                )),
                PathSegment::Field {
                    name: "action".into(),
                },
                1,
                AccessMissPolicy::Defer,
            )
            .unwrap();
            let inferred =
                infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());
            assert!(
                matches!(
                    inferred,
                    Err(InferenceIssue::NotInferable(
                        NotInferableReason::PlaceholderTypeUnavailable { .. }
                    ))
                ),
                "a placeholder without authority cannot prove an invalid type: {inferred:?}"
            );
            assert!(matches!(
                infer_type_from_value_without_bindings(&value),
                Err(InferenceIssue::NotInferable(
                    NotInferableReason::PlaceholderTypeUnavailable { .. }
                ))
            ));
        }

        for placeholder in [
            ParserPlaceholder::Loop(LoopSlot::Value),
            ParserPlaceholder::Loop(LoopSlot::Key),
            ParserPlaceholder::Loop(LoopSlot::Index),
            ParserPlaceholder::FnParam {
                name: "item".into(),
            },
            ParserPlaceholder::FnLocal {
                name: "local".into(),
            },
            ParserPlaceholder::ModuleArgument {
                name: "argument".into(),
                diagnostic: Default::default(),
            },
        ] {
            assert_pending(placeholder);
        }
        for slot in [LoopSlot::Value, LoopSlot::Key, LoopSlot::Index] {
            assert_pending(slot);
        }
    }

    fn noop(_v: &crate::resource::ExpandedValue) -> Result<(), String> {
        Ok(())
    }

    type ParserValue = Value;
    type ParserDeferredValue = DeferredValue;

    fn vpc_id_custom<R: crate::schema::PatternForm>() -> crate::schema::PatternAttributeType<R> {
        PatternAttributeType::refined_string_with_validator(
            TypeIdentity::bare("VpcId"),
            None,
            None,
            legacy_validator(noop),
            None,
        )
    }

    fn vpc_schema<R: crate::schema::PatternForm>() -> crate::schema::PatternResourceSchema<R> {
        PatternResourceSchema::new("ec2.Vpc")
            .attribute(
                PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string())
                    .build(),
            )
            .attribute(PatternAttributeSchema::optional("vpc_id", vpc_id_custom()).build())
    }

    #[test]
    fn unloaded_inferred_scalar_kind_defers_without_panicking() {
        let annotation =
            crate::parser::parse_type_expr_str("Count", &crate::parser::ProviderContext::default())
                .unwrap();
        assert!(
            matches!(inferred_scalar_kind(&annotation), Err(NotInferableReason::CustomTypeSchemaUnavailable { identity }) if identity == crate::schema::TypeIdentity::bare("Count"))
        );
    }

    #[test]
    fn integer_arithmetic_requires_proven_operands_but_accepts_typed_deferred_ints() {
        use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values};

        let source = format!("\n{}+", " ".repeat(19));
        let preprocessed = crate::parser::preprocess_file(&source).expect("test source");
        let operator_span = IntegerOperatorSpan::from_original_position(
            preprocessed.original_position(source.len() - 1),
        );

        let bare = evaluate_integer_values(
            Value::Deferred(DeferredValue::BindingRef {
                binding: RelativeBindingName::new("source"),
            }),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            operator_span,
        )
        .expect("parser-stage bare references remain pending until validation");
        assert!(matches!(
            infer_type_from_value(&bare, &InferenceBindings::new(), &SchemaRegistry::new()),
            Err(InferenceIssue::Failed(
                InferenceError::InvalidIntegerArithmeticOperand {
                    side: crate::integer_arithmetic::IntegerOperandSide::Left,
                    actual,
                    operator_span,
                    ..
                }
            )) if actual.contains("unproven type")
                && operator_span.line() == Some(2)
                && operator_span.column() == Some(20)
        ));

        let schema = PatternResourceSchema::new("clock.Source").attribute(
            PatternAttributeSchema::provider_populated("seconds", PatternAttributeType::int())
                .build(),
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("clock schema must be valid"),
        );
        let bindings = InferenceBindings::from([(
            "source".to_string(),
            InferenceBinding::Resource {
                provider: "test".to_string(),
                resource_type: "clock.Source".to_string(),
            },
        )]);
        let deferred = evaluate_integer_values(
            Value::resource_ref("source", "seconds", Vec::new()),
            IntegerOp::Divide,
            Value::Concrete(ConcreteValue::Int(60)),
            IntegerOperatorSpan::synthetic(),
        )
        .expect("resource arithmetic is structurally valid");
        assert_eq!(
            infer_type_from_value(&deferred, &bindings, &schemas),
            Ok(TypeExpr::Int),
        );
    }

    #[test]
    fn refined_module_arguments_use_their_declared_scalar_base_for_arithmetic() {
        use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values};

        let port_identity = TypeIdentity::bare("Port");
        let label_identity = TypeIdentity::bare("Label");
        let schema = PatternResourceSchema::new("clock.Source")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "port",
                    PatternAttributeType::refined_int(Some(port_identity.clone()), None),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "label",
                    PatternAttributeType::refined_string(
                        Some(label_identity.clone()),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("clock schema must be valid"),
        );

        let infer_argument_arithmetic = |name: &str, identity: TypeIdentity, kind| {
            let bindings = InferenceBindings::from([(
                name.to_string(),
                InferenceBinding::Argument {
                    type_expr: TypeExpr::SchemaType {
                        owned: crate::parser::OwnedTypeIdentity::provider("test", identity, kind),
                    },
                },
            )]);
            let expression = evaluate_integer_values(
                Value::Deferred(DeferredValue::Placeholder(
                    crate::resource::ParserPlaceholder::ModuleArgument {
                        name: name.to_string(),
                        diagnostic: Default::default(),
                    },
                )),
                IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                IntegerOperatorSpan::synthetic(),
            )
            .expect("a typed module argument is structurally valid before inference");
            infer_type_from_value(&expression, &bindings, &schemas)
        };

        assert_eq!(
            infer_argument_arithmetic("port", port_identity, crate::schema::RefinedScalarKind::Int),
            Ok(TypeExpr::Int),
            "an identity-bound refined Int argument remains an Int operand",
        );
        assert!(matches!(
            infer_argument_arithmetic(
                "label",
                label_identity,
                crate::schema::RefinedScalarKind::String
            ),
            Err(InferenceIssue::Failed(
                InferenceError::InvalidIntegerArithmeticOperand {
                    side: crate::integer_arithmetic::IntegerOperandSide::Left,
                    ..
                }
            )),
        ));
    }

    fn schemas_with_vpc() -> SchemaRegistry {
        let mut s = SchemaRegistry::new();
        s.insert(
            "awscc",
            vpc_schema()
                .validate()
                .expect("VPC inference schema must be valid"),
        );
        s
    }

    fn owned_vpc_id_type() -> TypeExpr {
        TypeExpr::SchemaType {
            owned: crate::parser::OwnedTypeIdentity::provider(
                "awscc",
                TypeIdentity::bare("VpcId"),
                crate::schema::RefinedScalarKind::String,
            ),
        }
    }

    fn binding_to_vpc(name: &str) -> InferenceBindings<crate::resource::ParserPlaceholder> {
        let mut b = InferenceBindings::new();
        b.insert(
            name.to_string(),
            InferenceBinding::Resource {
                provider: "awscc".to_string(),
                resource_type: "ec2.Vpc".to_string(),
            },
        );
        b
    }

    #[test]
    fn literal_string_inferred_as_string() {
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::String("hello".into())),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn literal_int_inferred_as_int() {
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::Int(42)),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Int));
    }

    #[test]
    fn literal_bool_inferred_as_bool() {
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::Bool(true)),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Bool));
    }

    #[test]
    fn interpolation_inferred_as_string() {
        // Build an Interpolation. The inner shape doesn't matter — the
        // result is always String. Use one literal part.
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("hello".into()),
            ])),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn secret_inferred_as_string() {
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hidden".into()),
            )))),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn list_of_strings_inferred_as_list_string() {
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::List(Box::new(TypeExpr::String))));
    }

    #[test]
    fn list_with_mixed_types_fails() {
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::Int(1)),
        ]));
        let r = infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::HeterogeneousCollection { .. }
            ))
        ));
    }

    #[test]
    fn mixed_collection_without_bindings_matches_main_inference_error() {
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::Int(1)),
        ]));
        let without_bindings = infer_type_from_value_without_bindings(&value);
        let with_bindings =
            infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());

        assert_eq!(without_bindings, with_bindings);
        assert_eq!(
            without_bindings,
            Err(InferenceIssue::Failed(
                InferenceError::HeterogeneousCollection {
                    reason: "collection element types disagree: `String` vs `Int`".to_string(),
                }
            ))
        );
    }

    #[test]
    fn map_of_strings_inferred_as_map_string() {
        let mut entries = indexmap::IndexMap::new();
        entries.insert(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("x".into())),
        );
        entries.insert(
            "b".to_string(),
            Value::Concrete(ConcreteValue::String("y".into())),
        );
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::Map(entries)),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Map(Box::new(TypeExpr::String))));
    }

    #[test]
    fn map_with_mixed_value_types_fails() {
        let mut entries = indexmap::IndexMap::new();
        entries.insert(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("x".into())),
        );
        entries.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::Map(entries)),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::HeterogeneousCollection { .. }
            ))
        ));
    }

    #[test]
    fn resource_ref_descends_struct_field_path() {
        // `vpc.tags.environment` against a `Struct{environment: String}`.
        // Locks the descent loop so a future refactor cannot silently
        // break field-path traversal.

        let tags_struct = PatternAttributeType::struct_(
            "Tags",
            vec![
                PatternStructField::optional("environment", PatternAttributeType::string()).build(),
            ],
        );
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("tags", tags_struct).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let path = AccessPath::with_fields("vpc", "tags", vec!["environment".to_string()]);
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas,
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn list_with_any_dynamic_element_demands_annotation() {
        // `[lookup(...), "extra"]` fails inference even though one
        // sibling is concrete — biasing toward concrete would let a
        // user typo-confused `[lookup(...), "extra"]` (where the user
        // meant `lookup` to return `Int`) silently lock the export to
        // String. All-or-nothing is the safer default.
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::List(vec![
                Value::Deferred(DeferredValue::FunctionCall {
                    name: crate::builtins::BuiltinFunction::lookup,
                    args: vec![],
                    source_line: (0).into(),
                }),
                Value::Concrete(ConcreteValue::String("extra".into())),
            ])),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn empty_list_fails_inference() {
        let r = infer_type_from_value(
            &Value::Concrete(ConcreteValue::List(vec![])),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn coalesce_uses_left_list_type_for_empty_default() {
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "up".to_string(),
            composition_binding(&[(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".into()),
                )])),
            )]),
        );
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("up", "accounts", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
        });

        assert_eq!(
            infer_type_from_value(&value, &bindings, &SchemaRegistry::new()),
            Ok(TypeExpr::List(Box::new(TypeExpr::String))),
        );
    }

    #[test]
    fn coalesce_rejects_default_that_conflicts_with_left_type() {
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "up".to_string(),
            composition_binding(&[(
                "accounts",
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("a".into()),
                )])),
            )]),
        );
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("up", "accounts", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::String("not-a-list".into()))),
        });

        let error = infer_type_from_value(&value, &bindings, &SchemaRegistry::new())
            .expect_err("coalesce default must match the left type");
        assert!(matches!(
            &error,
            InferenceIssue::Failed(InferenceError::IncompatibleCoalesceTypes { .. })
        ));
        assert!(error.to_string().contains("does not match left type"));
    }

    #[test]
    fn resource_ref_to_custom_attr_keeps_its_schema_owner() {
        // `vpc.vpc_id` where vpc is `awscc.ec2.Vpc` and `vpc_id` is
        // identified as bare `VpcId` still carries the `awscc` schema owner.
        let path = AccessPath::new("vpc", "vpc_id");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert_eq!(r, Ok(owned_vpc_id_type()));
    }

    #[test]
    fn resource_ref_to_string_attr_inferred_as_string() {
        let path = AccessPath::new("vpc", "cidr_block");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn dynamic_enum_attr_type_projects_to_schema_type() {
        let region = AttributeType::enum_(
            TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
            None,
            vec![],
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );

        assert_eq!(
            attribute_type_to_type_expr(&region, "aws"),
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                    crate::schema::RefinedScalarKind::String
                ),
            }
        );
    }

    #[test]
    fn closed_enum_attr_type_still_projects_to_string() {
        let effect = AttributeType::enum_(
            crate::schema::enum_identity("Effect", Some("aws.iam.PolicyDocument")),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );

        assert_eq!(
            attribute_type_to_type_expr(&effect, "aws"),
            TypeExpr::String
        );
    }

    #[test]
    fn resource_ref_unknown_binding_errors() {
        let path = AccessPath::new("missing", "vpc_id");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &InferenceBindings::new(),
            &schemas_with_vpc(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::UnknownBinding { .. }
            ))
        ));
    }

    #[test]
    fn resource_ref_unknown_attribute_errors() {
        let path = AccessPath::new("vpc", "missing");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::UnknownAttribute { .. }
            ))
        ));
    }

    #[test]
    fn function_call_join_inferred_as_string() {
        // `join` returns String per builtins metadata.
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::join,
                args: vec![],
                source_line: (0).into(),
            }),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn postfix_access_on_list_result_infers_element_type() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let list = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("item".into()),
        )]));
        let value = apply_postfix_access(
            list,
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("valid concrete list access");
        let inferred =
            infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());

        assert_eq!(inferred, Ok(TypeExpr::String));
    }

    #[test]
    fn postfix_access_on_split_result_infers_string_element_type() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let value = apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("/".into())).into(),
                    Value::resource_ref("role", "arn", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("deferred split result remains accessible");

        assert_eq!(
            infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new()),
            Ok(TypeExpr::String)
        );
    }

    #[test]
    fn postfix_access_on_map_result_infers_value_type() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let mut map = indexmap::IndexMap::new();
        map.insert("x".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        let value = apply_postfix_access(
            Value::Concrete(ConcreteValue::Map(map)),
            PathSegment::Subscript {
                index: Subscript::Str { key: "x".into() },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("valid concrete map access");
        let inferred =
            infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());

        assert_eq!(inferred, Ok(TypeExpr::Int));
    }

    #[test]
    fn postfix_access_on_unknown_base_keeps_type_unknown() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let value = apply_postfix_access(
            ParserValue::Deferred(ParserDeferredValue::Placeholder(
                crate::resource::ParserPlaceholder::EmptyInterpolation {
                    source: crate::resource::DiagnosticSourceSpan::authored(
                        "test.crn".into(),
                        1,
                        1,
                        1,
                        4,
                    ),
                },
            )),
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("unknown bases remain deferred");
        let inferred =
            infer_type_from_value(&value, &InferenceBindings::new(), &SchemaRegistry::new());

        assert!(matches!(
            inferred,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn function_call_lookup_fails_inference() {
        // `lookup` returns Any — depends on arguments. Annotation
        // required.
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::lookup,
                args: vec![],
                source_line: (0).into(),
            }),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn resource_ref_subscript_on_list_peels_one_layer() {
        // `vpc.subnets[0]` against `subnets: List<String>` infers `String`,
        // not `List<String>`. Without subscript handling the inferer
        // would type the value as the collection itself and a
        // downstream `: String` receiver would spuriously reject.
        let list_str = PatternAttributeType::list(PatternAttributeType::string());
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("subnets", list_str).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let path = AccessPath::with_fields_and_subscripts(
            "vpc",
            "subnets",
            vec![],
            vec![crate::resource::Subscript::Int { index: 0 }],
        );
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas,
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn resource_ref_typo_binding_errors_loudly() {
        // A typo'd binding (`mian` instead of `main`) must surface as
        // `UnknownBinding` rather than silently degrading to `None`.
        let path = AccessPath::new("mian", "vpc_id");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("main"),
            &schemas_with_vpc(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(
                InferenceError::UnknownBinding { .. }
            ))
        ));
    }

    #[test]
    fn resource_ref_through_upstream_state_binding_returns_non_inferable() {
        // `network.vpc_id` where `network` is a `let network = upstream_state { ... }`
        // — known binding but the inferer cannot project upstream
        // exports recursively (see #2357). Distinct from `UnknownBinding` (typo) so the
        // export inference can pass through cleanly while a real
        // typo bubbles up.
        let mut bindings = InferenceBindings::new();
        bindings.insert("network".to_string(), InferenceBinding::UpstreamState);
        let path = AccessPath::new("network", "vpc_id");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &bindings,
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::NotInferable(
                NotInferableReason::NonInferableBinding { .. }
            ))
        ));
    }

    #[test]
    fn resource_ref_to_union_typed_attr_demands_annotation() {
        // A `Union` receiver carries multiple alternatives; projecting
        // it to one branch silently lets the value satisfy alternatives
        // it doesn't. Demand annotation instead.
        let union_attr = PatternAttributeType::union(vec![
            PatternAttributeType::string(),
            PatternAttributeType::int(),
        ]);
        let schema = PatternResourceSchema::new("ec2.Vpc")
            .attribute(PatternAttributeSchema::optional("polymorphic", union_attr).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );

        let path = AccessPath::new("vpc", "polymorphic");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &binding_to_vpc("vpc"),
            &schemas,
        );
        assert!(matches!(
            r,
            Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
        ));
    }

    #[test]
    fn bindings_from_parts_resource_wins_over_upstream_state_collision() {
        // Mid-edit shape: same binding name appears in both
        // `let x = upstream_state {...}` and `let x = aws.ec2.Vpc {...}`.
        // Insert order (upstream first, resources second) means the
        // resource wins, so `x.attr` infers precisely instead of
        // degrading to `NonInferableBinding`.
        use crate::parser::UpstreamState;
        use crate::resource::AuthoredResource as Resource;
        let resource: Resource =
            Resource::with_provider("awscc", "ec2.Vpc", "main", None).with_binding("x");
        let upstream: UpstreamState = UpstreamState {
            binding: "x".into(),
            source: std::path::PathBuf::from("../other"),
        };
        let bindings = bindings_from_parts(&[resource], &[], &[], &[upstream]);
        assert!(matches!(
            bindings.get("x"),
            Some(InferenceBinding::Resource { .. })
        ));
    }

    #[test]
    fn bindings_from_parts_collects_pure_upstream_state() {
        use crate::parser::UpstreamState;
        let upstream: UpstreamState = UpstreamState {
            binding: "network".into(),
            source: std::path::PathBuf::from("../network"),
        };
        let bindings = bindings_from_parts::<
            crate::resource::ModuleSource,
            ParserPlaceholder,
            RelativeBindingName,
        >(&[], &[], &[], &[upstream]);
        assert!(matches!(
            bindings.get("network"),
            Some(InferenceBinding::UpstreamState)
        ));
    }

    #[test]
    fn list_inside_struct_field_with_union_does_not_demand_annotation() {
        // Real-world IAM-policy shape: outer Struct, inner field is a
        // List<Struct{principal: Union<...>}>. The user's
        // `policy_document = main.policy.policy_document` reference is
        // forwarding the whole struct, not picking a union branch, and
        // must not be blocked. Inference accepts this and projects the
        // outer Struct into a TypeExpr::Struct.

        let inner_struct = PatternAttributeType::struct_(
            "Statement",
            vec![
                PatternStructField::optional(
                    "principal",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::string(),
                        PatternAttributeType::int(),
                    ]),
                )
                .build(),
            ],
        );
        let outer_struct = PatternAttributeType::struct_(
            "PolicyDocument",
            vec![
                PatternStructField::optional("statement", PatternAttributeType::list(inner_struct))
                    .build(),
            ],
        );
        let schema = PatternResourceSchema::new("iam.Policy")
            .attribute(PatternAttributeSchema::optional("policy_document", outer_struct).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "awscc",
            schema.validate().expect("schema fixture must be valid"),
        );
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "policy".to_string(),
            InferenceBinding::Resource {
                provider: "awscc".to_string(),
                resource_type: "iam.Policy".to_string(),
            },
        );
        let path = AccessPath::new("policy", "policy_document");
        let r = infer_type_from_value(
            &Value::Deferred(DeferredValue::ResourceRef { path }),
            &bindings,
            &schemas,
        );
        // Pin the inferred shape, not just `is_ok`: the outer Struct
        // is preserved with its `statement` field, and the buried
        // Union inside the inner Statement struct collapses to the
        // sentinel `String` at the leaf (per `attribute_type_to_type_expr`'s
        // documented behavior). A future refactor that changes how
        // nested unions are rendered will trip this assertion and
        // force a deliberate decision rather than a silent drift.
        let inferred = r.expect("inference should accept policy_document");
        let TypeExpr::Struct { fields } = inferred else {
            panic!("expected Struct, got {:?}", inferred);
        };
        assert_eq!(fields.len(), 1, "outer struct must have one field");
        assert_eq!(fields[0].0, "statement");
        let TypeExpr::List(elem) = &fields[0].1 else {
            panic!("statement must project to a List, got {:?}", fields[0].1);
        };
        let TypeExpr::Struct {
            fields: inner_fields,
        } = elem.as_ref()
        else {
            panic!("List element must be a Struct, got {:?}", elem);
        };
        assert_eq!(inner_fields.len(), 1);
        assert_eq!(inner_fields[0].0, "principal");
        // The buried Union projects to the sentinel `String`. This is
        // the documented contract — see `attribute_type_to_type_expr`'s
        // doc-comment on Union. Locking the assertion here prevents
        // silent drift to e.g. `Int` if the first-member fallback ever
        // gets reintroduced.
        assert_eq!(inner_fields[0].1, TypeExpr::String);
    }

    #[test]
    fn deferred_builtin_identity_rejects_unknown_names() {
        assert!(crate::builtins::BuiltinFunction::from_name("nonexistent").is_none());
    }

    #[test]
    fn apply_inference_fills_inferable_export_with_inferred_type() {
        let mut parsed = crate::parser::ParsedFile::default();
        let res = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main");
        parsed.resources.push(res); // allow: direct — fixture test inspection
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "vpc_id".to_string(),
                None,
                Some(Value::resource_ref(
                    "main".to_string(),
                    "vpc_id".to_string(),
                    vec![],
                )),
            ));

        let inferred = apply_inference(parsed, &schemas_with_vpc());
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(owned_vpc_id_type()))
        );
    }

    #[test]
    fn apply_inference_attaches_failure_to_inferred_export() {
        use crate::static_type::{ExportStaticType, InferredUncheckedReason, StaticType};

        let mut parsed = crate::parser::ParsedFile::default();
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "zone_id".to_string(),
                None,
                Some(Value::Deferred(DeferredValue::FunctionCall {
                    name: crate::builtins::BuiltinFunction::lookup,
                    args: vec![],
                    source_line: (0).into(),
                })),
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());

        assert!(matches!(
            &inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::InferenceFailed(InferenceError::UnknownType { .. })
            ))
        ));
    }

    #[test]
    fn apply_inference_preserves_declared_and_inferred_origins() {
        use crate::static_type::{ExportStaticType, StaticType};

        let mut parsed = crate::parser::ParsedFile::default();
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "declared".to_string(),
                Some(TypeExpr::String),
                Some(Value::Concrete(ConcreteValue::String("declared".into()))),
            ));
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "inferred".to_string(),
                None,
                Some(Value::Concrete(ConcreteValue::String("inferred".into()))),
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Declared(TypeExpr::String)
        );
        assert_eq!(
            inferred.export_params[1].static_type,
            ExportStaticType::Inferred(StaticType::Known(TypeExpr::String))
        );
    }

    #[test]
    fn apply_inference_keeps_not_inferable_reasons_distinct() {
        use crate::parser::UpstreamState;
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let mut parsed = crate::parser::ParsedFile::default();
        parsed.upstream_states.push(UpstreamState {
            binding: "upstream".into(),
            source: std::path::PathBuf::from("../upstream"),
        });
        let resource = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main");
        parsed.resources.push(resource); // allow: direct — fixture test inspection
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "missing".to_string(),
                None,
                None,
            ));
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "upstream".to_string(),
                None,
                Some(Value::resource_ref(
                    "upstream".to_string(),
                    "value".to_string(),
                    vec![],
                )),
            ));
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "schema".to_string(),
                None,
                Some(Value::resource_ref(
                    "main".to_string(),
                    "vpc_id".to_string(),
                    vec![],
                )),
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue)
            ))
        );
        assert_eq!(
            inferred.export_params[1].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "upstream".to_string()
                })
            ))
        );
        assert_eq!(
            inferred.export_params[2].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            ))
        );
    }

    #[test]
    fn retry_unchecked_export_inference_attaches_reportable_failure() {
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let mut parsed = crate::parser::ParsedFile::default();
        let resource = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main");
        parsed.resources.push(resource); // allow: direct — fixture test inspection
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "vpc_id".to_string(),
                None,
                Some(Value::resource_ref(
                    "main".to_string(),
                    "vpc_i".to_string(),
                    vec![],
                )),
            ));

        let mut inferred = apply_inference(parsed, &SchemaRegistry::new());
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            ))
        );

        let loaded_schemas = ProviderLoadedSchemaRegistry::for_test(schemas_with_vpc());
        let schema_checked = retry_unchecked_export_inference(&mut inferred, &loaded_schemas);
        let failures = attached_inference_failures(schema_checked)
            .map(|failure| (failure.name.to_string(), failure.error.clone()))
            .collect::<Vec<_>>();

        assert!(matches!(
            failures.as_slice(),
            [(name, InferenceError::UnknownAttribute { binding, attribute })]
                if name == "vpc_id" && binding == "main" && attribute == "vpc_i"
        ));
    }

    #[test]
    fn retry_unchecked_export_inference_preserves_reason_after_success() {
        use crate::static_type::{
            ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
        };

        let mut parsed = crate::parser::ParsedFile::default();
        let resource = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main", None)
            .with_binding("main");
        parsed.resources.push(resource); // allow: direct — fixture test inspection
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "vpc_id".to_string(),
                None,
                Some(Value::resource_ref(
                    "main".to_string(),
                    "vpc_id".to_string(),
                    vec![],
                )),
            ));

        let mut inferred = apply_inference(parsed, &SchemaRegistry::new());
        let loaded_schemas = ProviderLoadedSchemaRegistry::for_test(schemas_with_vpc());
        let schema_checked = retry_unchecked_export_inference(&mut inferred, &loaded_schemas);
        assert_eq!(attached_inference_failures(schema_checked).count(), 0);

        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::SchemaUnavailable {
                    binding: "main".to_string()
                })
            )),
            "retry probes for reportable errors without replacing unchecked provenance",
        );
    }

    #[test]
    fn apply_inference_preserves_explicit_annotation() {
        let mut parsed = crate::parser::ParsedFile::default();
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "vpc_id".to_string(),
                Some(crate::parser::bare_type_for_test("vpc_id".to_string())),
                Some(Value::Concrete(ConcreteValue::String("vpc-abc".into()))),
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Declared(crate::parser::bare_type_for_test("vpc_id".to_string()))
        );
    }

    #[test]
    fn apply_inference_resolves_module_call_attribute_via_virtual_binding() {
        // #2493: an `exports` value referencing `<module_call>.<attr>`
        // (e.g. `role_arn = github_actions_carina.role_arn` where
        // `let github_actions_carina = github_module {...}`) must
        // resolve to the same `TypeExpr` as if the user had annotated
        // the export explicitly. The module's `attributes { role_arn =
        // role.arn }` block lets the type be inferred transitively
        // through the module-call's composition resource (post-expansion)
        // and the inner role's schema attribute.
        //
        // Pre-#2493 the inference walked `parsed.resources` only, missed
        // the `Virtual` binding (which post-expansion exposes the
        // module's attribute set), and surfaced a misleading
        // "unknown binding" diagnostic.
        let mut parsed = crate::parser::ParsedFile::default();
        // Concrete role resource with prefixed binding (post-expansion shape).
        let role = crate::resource::Resource::with_provider(
            "awscc",
            "ec2.Vpc",
            "github_actions_carina.role",
            None,
        )
        .with_binding("github_actions_carina.role");
        parsed.resources.push(role); // allow: direct — fixture test inspection
        // Virtual resource that exposes the module's attributes.
        // `vpc_id` here stands in for the module-exposed attribute that
        // points at the inner role's schema attribute. carina#3181:
        // compositions are a distinct typestate in `compositions`.
        let mut virt_attrs: indexmap::IndexMap<
            String,
            crate::resource::AuthoredLiveCompositionAttribute,
        > = indexmap::IndexMap::new();
        virt_attrs.insert(
            "role_id".to_string(),
            crate::resource::AuthoredLiveCompositionAttribute::from_value(
                Value::resource_ref(
                    "github_actions_carina.role".to_string(),
                    "vpc_id".to_string(),
                    vec![],
                ),
                None,
            ),
        );
        let virt: crate::resource::AuthoredLiveComposition = crate::resource::LiveComposition {
            id: crate::resource::ResolvedResourceId::new(
                crate::resource::ResourceId::with_identity("_virtual", "github_actions_carina"),
            ),
            signature: crate::resource::LiveCompositionSignature {
                arguments: indexmap::IndexMap::new(),
                attributes: virt_attrs,
                pending_constraints: Vec::new(),
            },
            binding: Some("github_actions_carina".into()),
            dependency_bindings: std::collections::BTreeSet::new(),
            module_name: "github_module".to_string(),
            instance: "github_actions_carina".to_string(),
            call_path: crate::resource::AuthoredCompositionCallPath::direct(
                crate::resource::AuthoredCompositionCall {
                    id: crate::parser::ModuleCallId::test_dummy_in("inference.crn"),
                    module_name: "github_module".to_string(),
                    binding: Some("github_actions_carina".into()),
                    instance: "github_actions_carina".to_string(),
                    module_source: None,
                    module_directory: None,
                    loop_origin: None,
                },
            ),
            quoted_string_attrs: std::collections::HashSet::new(),
        };
        parsed.compositions.push(virt); // allow: direct — fixture test inspection
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "role_id".to_string(),
                None,
                Some(Value::resource_ref(
                    "github_actions_carina".to_string(),
                    "role_id".to_string(),
                    vec![],
                )),
            ));

        let inferred = apply_inference(parsed, &schemas_with_vpc());
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Known(owned_vpc_id_type())),
            "expected the inner attribute's type to flow through the composition binding"
        );
    }

    #[test]
    fn apply_inference_treats_pre_expansion_module_call_binding_as_non_inferable() {
        // #2493 load-time path: at `load_configuration` the parser has
        // produced `parsed.module_calls` but module expansion hasn't
        // run yet, so the composition resource isn't in `parsed.resources`
        // and the bindings map can't see the module-call's projection.
        // The inferer must treat the binding as known-but-non-inferable
        // (mirroring `upstream_state`) so the export remains unchecked
        // without a noisy `unknown binding` error. A post-expansion consumer
        // such as the LSP can run inference against the `Virtual` arm exercised
        // by the sibling test above; the CLI retry remains report-only.
        let mut parsed = crate::parser::ParsedFile::default();
        parsed.module_calls.push(ModuleCall::test(
            "github_module".to_string(),
            Some("github_actions_carina".to_string()),
            std::collections::HashMap::new(),
        ));
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "role_arn".to_string(),
                None,
                Some(Value::resource_ref(
                    "github_actions_carina".to_string(),
                    "role_arn".to_string(),
                    vec![],
                )),
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "github_actions_carina".to_string()
                })
            ))
        );
    }

    #[test]
    fn apply_inference_attaches_missing_value_reason() {
        // Export with no value and no annotation: nothing to infer from
        // → a typed non-failure reason (the value-less shape is the
        // parser-tombstoned state, not an inference failure).
        let mut parsed = crate::parser::ParsedFile::default();
        parsed
            .export_params
            .push(crate::parser::ParsedExportParam::dummy(
                "vpc_id".to_string(),
                None,
                None,
            ));

        let inferred = apply_inference(parsed, &SchemaRegistry::new());
        assert_eq!(
            inferred.export_params[0].static_type,
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue)
            ))
        );
    }

    /// Convenience for the `Virtual`-binding cycle tests below.
    fn composition_binding(
        attrs: &[(&str, Value)],
    ) -> InferenceBinding<crate::resource::ParserPlaceholder> {
        let mut map = indexmap::IndexMap::new();
        for (k, v) in attrs {
            map.insert((*k).to_string(), v.clone());
        }
        InferenceBinding::Virtual { attributes: map }
    }

    #[test]
    fn composition_binding_two_node_cycle_reports_cycle_path() {
        // #2497: a synthetic 2-binding `Virtual` cycle (`a.x -> b.y ->
        // a.x`) must be detected as a cycle and surface the chain in
        // the diagnostic, not loop until stack exhaustion. Parser and
        // module resolver reject module-of-module cycles in practice,
        // so this shape is reachable today only via hand-constructed
        // bindings (or a future refactor that breaks the no-cycle
        // invariant); the inferer's defense in depth is what this test
        // pins.
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "a".to_string(),
            composition_binding(&[(
                "x",
                Value::resource_ref("b".to_string(), "y".to_string(), vec![]),
            )]),
        );
        bindings.insert(
            "b".to_string(),
            composition_binding(&[(
                "y",
                Value::resource_ref("a".to_string(), "x".to_string(), vec![]),
            )]),
        );

        let entry = Value::resource_ref("a".to_string(), "x".to_string(), vec![]);
        let r = infer_type_from_value(&entry, &bindings, &SchemaRegistry::new());
        match r {
            Err(InferenceIssue::Failed(InferenceError::CyclicVirtualBinding { cycle })) => {
                assert_eq!(
                    cycle,
                    vec!["a".to_string(), "b".to_string(), "a".to_string()],
                    "cycle must record entry order with the closing binding repeated"
                );
                assert_eq!(
                    cycle.first(),
                    cycle.last(),
                    "first and last entries must match for the cycle to be visually obvious"
                );
            }
            other => panic!(
                "expected CyclicVirtualBinding error for 2-binding Virtual cycle, got: {other:?}"
            ),
        }
    }

    #[test]
    fn virtual_binding_self_cycle_reports_cycle_path() {
        // #2497 edge case: a 1-binding self-referential `Virtual`
        // (`a.x -> a.x`) must also surface as a cycle, not loop. The
        // visited-set must register the binding at entry, before the
        // recursive descend.
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "a".to_string(),
            composition_binding(&[(
                "x",
                Value::resource_ref("a".to_string(), "x".to_string(), vec![]),
            )]),
        );

        let entry = Value::resource_ref("a".to_string(), "x".to_string(), vec![]);
        let r = infer_type_from_value(&entry, &bindings, &SchemaRegistry::new());
        match r {
            Err(InferenceIssue::Failed(InferenceError::CyclicVirtualBinding { cycle })) => {
                assert_eq!(
                    cycle,
                    vec!["a".to_string(), "a".to_string()],
                    "self-cycle must record the binding twice (entry + closure)"
                );
            }
            other => panic!("expected CyclicVirtualBinding error for self-cycle, got: {other:?}"),
        }
    }

    #[test]
    fn virtual_binding_two_unrelated_visits_do_not_false_positive() {
        // Defensive: visiting `a` and then (after returning) visiting
        // `b` from a sibling collection element must NOT trip the cycle
        // guard. Pins that the visited-set is scope-popped between
        // sibling subtrees rather than accumulating across unrelated
        // traversals — a naive `&mut HashSet` that only ever grows
        // would surface a false-positive cycle here.
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "a".to_string(),
            composition_binding(&[("x", Value::Concrete(ConcreteValue::String("hi".into())))]),
        );
        bindings.insert(
            "b".to_string(),
            composition_binding(&[("y", Value::Concrete(ConcreteValue::String("there".into())))]),
        );

        // List of two refs to *different* Virtual bindings. After the
        // first ref returns, the visited-set must no longer contain `a`
        // when the second ref descends into `b`.
        let entries = Value::Concrete(ConcreteValue::List(vec![
            Value::resource_ref("a".to_string(), "x".to_string(), vec![]),
            Value::resource_ref("b".to_string(), "y".to_string(), vec![]),
        ]));
        let r = infer_type_from_value(&entries, &bindings, &SchemaRegistry::new());
        assert_eq!(
            r,
            Ok(TypeExpr::List(Box::new(TypeExpr::String))),
            "sibling Virtual visits must not trigger a false cycle"
        );
    }
}
