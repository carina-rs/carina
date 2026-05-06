//! Rhs-driven type inference for `exports { }` declarations whose
//! `type_expr` annotation is omitted.
//!
//! [`apply_inference`] is the bridge from `ParsedFile` (where exports
//! carry `Option<TypeExpr>`) to `InferredFile` (where every export
//! carries a bare `TypeExpr`, possibly the [`TypeExpr::Unknown`]
//! sentinel for a failure paired with a `(name, InferenceError)` entry
//! on the side). The loader runs this once per directory load so
//! downstream consumers (CLI validate, LSP diagnostics, LSP
//! completion) see a definitive type per export instead of having to
//! re-derive it. See `docs/specs/2026-05-03-typeexpr-stage2-design.md`.
//!
//! Inference rules:
//!
//! - `Value::String/Int/Bool/Float` → corresponding `TypeExpr` primitive.
//! - `Value::List(items)` → `List(T)` where `T` is the unified element
//!   type. Heterogeneous lists fail inference.
//! - `Value::Map(entries)` → `Map(T)` where `T` unifies value types.
//! - `Value::ResourceRef { binding, attribute, field_path }` → walk
//!   the binding's resource schema, descend `field_path`, convert the
//!   reached `AttributeType` into a `TypeExpr`.
//! - `Value::Interpolation(_)` → `TypeExpr::String` (interpolation
//!   results are always strings; users who need a specific Custom
//!   identity must annotate to narrow).
//! - `Value::Secret(_)` → `TypeExpr::String` (the secret's plaintext
//!   shape; same reasoning as Interpolation).
//! - `Value::FunctionCall { name, .. }` → consult `builtins`'s
//!   `BuiltinReturnType`. `String/Int/List/Map/Secret` map to the
//!   corresponding `TypeExpr` primitive; `Any` (`lookup`, `min`,
//!   `max`, `map`) is "depends on arguments" — counts as inference
//!   failure.

use std::collections::HashMap;

use crate::builtins::{BuiltinReturnType, builtin_functions};
use crate::parser::TypeExpr;
use crate::resource::Value;
use crate::schema::{AttributeType, ResourceSchema, SchemaKind, SchemaRegistry};

/// Why inference failed. Carries the rhs description so downstream
/// callers can render an actionable "type annotation required" error
/// pointing at the offending field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceError {
    /// Value cannot have a static type (function returns `Any`,
    /// unknown builtin, deferred-eval shape with no concrete type).
    UnknownType { reason: String },
    /// Heterogeneous collection — inferred element types disagreed.
    HeterogeneousCollection { reason: String },
    /// `ResourceRef`'s binding doesn't appear in the binding map.
    UnknownBinding { binding: String },
    /// Binding is known but inference can't proceed through it —
    /// today this is only `upstream_state` bindings (the export's
    /// type lives in the upstream's `parsed.export_params`, which the
    /// inferer doesn't recursively resolve today, see #2357). Distinct from
    /// `UnknownBinding` so callers can swallow this case (the
    /// downstream consumer's typecheck will still gate the use)
    /// while a true typo bubbles up as a hard error.
    NonInferableBinding { binding: String },
    /// `ResourceRef`'s attribute (or nested field) isn't on the
    /// resource schema.
    UnknownAttribute { binding: String, attribute: String },
    /// Could not look up the binding's resource schema (provider
    /// not loaded, etc.).
    SchemaUnavailable { binding: String },
}

impl std::fmt::Display for InferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InferenceError::UnknownType { reason } => write!(f, "{}", reason),
            InferenceError::HeterogeneousCollection { reason } => write!(f, "{}", reason),
            InferenceError::UnknownBinding { binding } => {
                write!(f, "unknown binding `{}`", binding)
            }
            InferenceError::NonInferableBinding { binding } => write!(
                f,
                "cannot infer through binding `{}` (upstream_state)",
                binding
            ),
            InferenceError::UnknownAttribute { binding, attribute } => {
                write!(f, "unknown attribute `{}` on `{}`", attribute, binding)
            }
            InferenceError::SchemaUnavailable { binding } => {
                write!(f, "no schema available for binding `{}`", binding)
            }
        }
    }
}

/// One entry in the binding map. Resource bindings (`let <name> = <provider>.<resource> { ... }`)
/// carry the provider/resource type and are inferred through; `upstream_state`
/// bindings are recognized by name but cannot be projected to a `TypeExpr`
/// today (the export's type lives in the upstream's `parsed.export_params`,
/// which the inferer doesn't recursively resolve here). Distinguishing the
/// two lets the inferer turn a true typo (`mian.vpc_id`) into a hard error
/// while still passing through `upstream_state`-derived references.
///
/// `Virtual` carries the attribute map of a module-call's
/// `ResourceKind::Virtual` resource so an `exports` reference of the form
/// `<module_call>.<attr>` can transitively infer through the bound
/// expression on the module's `attributes { ... }` block, instead of
/// failing with `unknown binding` (#2493).
#[derive(Debug, Clone)]
pub enum InferenceBinding {
    Resource {
        provider: String,
        resource_type: String,
    },
    UpstreamState,
    Virtual {
        attributes: indexmap::IndexMap<String, Value>,
    },
    /// Module-call binding registered before module expansion.
    /// Inference treats it like `UpstreamState` (known-but-non-
    /// inferable) so a downstream `${<call>.<attr>}` doesn't fire a
    /// false `unknown binding` at load time. Post-expansion the
    /// matching `Virtual` resource lands in `parsed.resources` and
    /// `bindings_from_parts` overwrites this entry with a `Virtual`
    /// that can actually project to a `TypeExpr`. #2493.
    ModuleCall,
}

pub type InferenceBindings = HashMap<String, InferenceBinding>;

/// Build an [`InferenceBindings`] map from a `ParsedFile`. Convenience
/// over [`bindings_from_parts`] for the common case of having a parsed
/// file in hand.
pub fn bindings_from_parsed(parsed: &crate::parser::ParsedFile) -> InferenceBindings {
    let mut out = bindings_from_parts(&parsed.resources, &parsed.upstream_states);
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
            && !out.contains_key(name)
        {
            out.insert(name.clone(), InferenceBinding::ModuleCall);
        }
    }
    out
}

/// Build an [`InferenceBindings`] map from already-extracted resource
/// and upstream-state slices. Used by callers that already work in
/// terms of the slice arguments (e.g. `validate_export_param_ref_types`)
/// so the binding-collection logic is in one place.
///
/// Both resource bindings (`let <name> = <provider>.<resource> { ... }`)
/// and `upstream_state` bindings (`let <name> = upstream_state { ... }`)
/// are included; the latter are tagged so the inferer can distinguish
/// "known but non-inferable" from a typo.
///
/// Insert order is upstream-states first, resources second, so a
/// resource binding wins on collision with an `upstream_state` of the
/// same name. The duplicate-binding diagnostic fires elsewhere in
/// validation, but during a mid-edit the LSP can transiently see both —
/// preferring the resource means downstream inference still gets the
/// precise type instead of degrading to `NonInferableBinding`. Two
/// resource bindings with the same name (also illegal but possible
/// mid-edit) silently last-write-wins on the standard `HashMap` insert
/// semantics; the duplicate-binding check elsewhere is the gate that
/// reports it to the user.
pub fn bindings_from_parts(
    resources: &[crate::resource::Resource],
    upstream_states: &[crate::parser::UpstreamState],
) -> InferenceBindings {
    let mut out = InferenceBindings::new();
    for us in upstream_states {
        out.insert(us.binding.clone(), InferenceBinding::UpstreamState);
    }
    for resource in resources {
        let Some(name) = &resource.binding else {
            continue;
        };
        // `Virtual` resources synthesised by module-call expansion
        // (`expand_module_call`) carry no provider identity — their
        // attributes are projections from the module's
        // `attributes { ... }` block. Tag them so `infer_resource_ref`
        // recurses into the bound expression instead of trying a schema
        // lookup that would always fail. #2493.
        let entry = if resource.is_virtual() {
            InferenceBinding::Virtual {
                attributes: resource.attributes.clone(),
            }
        } else {
            InferenceBinding::Resource {
                provider: resource.id.provider.clone(),
                resource_type: resource.id.resource_type.clone(),
            }
        };
        out.insert(name.clone(), entry);
    }
    out
}

/// Try to infer `type_expr` for an unannotated declaration. Returns:
///
/// - `Ok(Some(type_expr))` — declaration has either an explicit
///   annotation (kept) or an inferred one (newly added).
/// - `Ok(None)` — no `value` to infer from, **or** the rhs references
///   a binding the inferer doesn't know about (typically an
///   `upstream_state` binding, which lives in `parsed.upstream_states`
///   not `parsed.resources`). The declaration carries no type today
///   and the caller falls back to today's behavior of skipping the
///   downstream predicate. Inferring through `upstream_state` requires
///   recursively resolving the upstream's exports and is tracked as a
///   follow-up to #2361.
/// - `Err(InferenceError)` — annotation is omitted, the rhs is
///   present, and inference failed for a reason other than an
///   unknown binding (heterogeneous list, dynamic builtin return,
///   unknown attribute, etc.). The caller surfaces this as a
///   "type annotation required" diagnostic.
///
/// `infer_type_expr` is a thin convenience over [`infer_type_from_value`]
/// that resolves the explicit-annotation-wins precedence in one place
/// so consumers don't reimplement it.
pub fn infer_type_expr(
    declared: Option<&TypeExpr>,
    value: Option<&Value>,
    bindings: &InferenceBindings,
    schemas: &SchemaRegistry,
) -> Result<Option<TypeExpr>, InferenceError> {
    if let Some(ty) = declared {
        return Ok(Some(ty.clone()));
    }
    let Some(v) = value else {
        return Ok(None);
    };
    match infer_type_from_value(v, bindings, schemas) {
        Ok(t) => Ok(Some(t)),
        // Known but non-inferable (upstream_state) — fall through to
        // today's "no static type" behavior; the downstream consumer's
        // typecheck still gates the use. A true typo (`UnknownBinding`)
        // is *not* swallowed here so it surfaces as a hard error
        // instead of silently degrading the typecheck.
        Err(InferenceError::NonInferableBinding { .. }) => Ok(None),
        // Schema not yet loaded for the binding's resource type
        // (provider lock missing, in-progress edit) — also pass
        // through; treating this as a hard error would block validate
        // against an in-flux configuration.
        Err(InferenceError::SchemaUnavailable { .. }) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Try to synthesize a `TypeExpr` from a `Value`. Returns `Err` if the
/// value's type cannot be statically determined — the caller is
/// expected to either fall back to a user-supplied annotation or
/// surface the error as "type annotation required" at the offending
/// field.
pub fn infer_type_from_value(
    value: &Value,
    bindings: &InferenceBindings,
    schemas: &SchemaRegistry,
) -> Result<TypeExpr, InferenceError> {
    match value {
        Value::String(_) => Ok(TypeExpr::String),
        Value::Int(_) => Ok(TypeExpr::Int),
        Value::Float(_) => Ok(TypeExpr::Float),
        Value::Bool(_) => Ok(TypeExpr::Bool),
        Value::Interpolation(_) => Ok(TypeExpr::String),
        Value::Secret(_) => Ok(TypeExpr::String),
        Value::List(items) => infer_collection(items, bindings, schemas, TypeExpr::List),
        Value::Map(entries) => infer_collection(entries.values(), bindings, schemas, TypeExpr::Map),
        Value::ResourceRef { path } => infer_resource_ref(
            path.binding(),
            path.attribute(),
            path.field_path(),
            path.subscripts(),
            bindings,
            schemas,
        ),
        Value::FunctionCall { name, .. } => infer_function_call(name),
        Value::Unknown(_) => Err(InferenceError::UnknownType {
            reason: "value not yet known at plan time (unresolved upstream)".to_string(),
        }),
    }
}

fn infer_collection<'a, I, F>(
    items: I,
    bindings: &InferenceBindings,
    schemas: &SchemaRegistry,
    wrap: F,
) -> Result<TypeExpr, InferenceError>
where
    I: IntoIterator<Item = &'a Value>,
    F: FnOnce(Box<TypeExpr>) -> TypeExpr,
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
        Some(v) => infer_type_from_value(v, bindings, schemas)?,
        None => {
            return Err(InferenceError::UnknownType {
                reason: "empty collection — element type cannot be inferred".to_string(),
            });
        }
    };
    for next in iter {
        let next_type = infer_type_from_value(next, bindings, schemas)?;
        if next_type != first {
            return Err(InferenceError::HeterogeneousCollection {
                reason: format!(
                    "collection element types disagree: `{}` vs `{}`",
                    first, next_type
                ),
            });
        }
    }
    Ok(wrap(Box::new(first)))
}

fn infer_resource_ref(
    binding: &str,
    attribute: &str,
    field_path: &[String],
    subscripts: &[crate::resource::Subscript],
    bindings: &InferenceBindings,
    schemas: &SchemaRegistry,
) -> Result<TypeExpr, InferenceError> {
    let target = match bindings.get(binding) {
        Some(InferenceBinding::Resource {
            provider,
            resource_type,
        }) => (provider.as_str(), resource_type.as_str()),
        Some(InferenceBinding::UpstreamState) | Some(InferenceBinding::ModuleCall) => {
            return Err(InferenceError::NonInferableBinding {
                binding: binding.to_string(),
            });
        }
        Some(InferenceBinding::Virtual { attributes }) => {
            // The module-call's virtual resource holds the module's
            // `attributes { ... }` projections directly. Recurse on the
            // bound expression so the type flows transitively through
            // the inner resource's schema. #2493.
            let inner =
                attributes
                    .get(attribute)
                    .ok_or_else(|| InferenceError::UnknownAttribute {
                        binding: binding.to_string(),
                        attribute: attribute.to_string(),
                    })?;
            let inner_type = infer_type_from_value(inner, bindings, schemas)?;
            return super::narrow_type_expr(&inner_type, field_path, subscripts).ok_or_else(|| {
                InferenceError::UnknownAttribute {
                    binding: binding.to_string(),
                    attribute: attribute.to_string(),
                }
            });
        }
        None => {
            return Err(InferenceError::UnknownBinding {
                binding: binding.to_string(),
            });
        }
    };
    let Some(schema) = lookup_schema(schemas, target.0, target.1) else {
        return Err(InferenceError::SchemaUnavailable {
            binding: binding.to_string(),
        });
    };
    let Some(attr_schema) = schema.attributes.get(attribute) else {
        return Err(InferenceError::UnknownAttribute {
            binding: binding.to_string(),
            attribute: attribute.to_string(),
        });
    };
    let mut current = &attr_schema.attr_type;
    for segment in field_path {
        current = match descend_struct_field(current, segment) {
            Some(t) => t,
            None => {
                return Err(InferenceError::UnknownAttribute {
                    binding: binding.to_string(),
                    attribute: format!("{}.{}", attribute, segment),
                });
            }
        };
    }
    // Peel one collection layer per trailing subscript: `[0]` against a
    // `List<T>` projects to `T`; `["k"]` against a `Map<_, V>` projects
    // to `V`. The grammar already enforces matching subscript shapes,
    // but inference still has to descend so the resulting `TypeExpr`
    // reflects the post-projection element type rather than the
    // collection itself.
    for sub in subscripts {
        current = match (sub, current) {
            (crate::resource::Subscript::Int { .. }, AttributeType::List { inner, .. }) => inner,
            (crate::resource::Subscript::Str { .. }, AttributeType::Map { value, .. }) => value,
            _ => {
                return Err(InferenceError::UnknownType {
                    reason: format!(
                        "subscript on `{}.{}` does not match the attribute's collection shape",
                        binding, attribute
                    ),
                });
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
    if matches!(current, AttributeType::Union(_)) {
        return Err(InferenceError::UnknownType {
            reason: format!(
                "rhs `{}.{}` resolves to a union type; annotate the export to pick a branch",
                binding, attribute
            ),
        });
    }
    Ok(attribute_type_to_type_expr(current))
}

fn lookup_schema<'a>(
    schemas: &'a SchemaRegistry,
    provider: &str,
    resource_type: &str,
) -> Option<&'a ResourceSchema> {
    schemas
        .get(provider, resource_type, SchemaKind::Managed)
        .or_else(|| schemas.get(provider, resource_type, SchemaKind::DataSource))
}

fn descend_struct_field<'a>(
    attr_type: &'a AttributeType,
    field: &str,
) -> Option<&'a AttributeType> {
    match attr_type {
        AttributeType::Struct { fields, .. } => fields
            .iter()
            .find(|f| f.name == field)
            .map(|f| &f.field_type),
        _ => None,
    }
}

/// Convert an `AttributeType` (schema side) into a `TypeExpr`
/// (declaration side). Used both by `ResourceRef` inference and by
/// recursive descent into `List`/`Map`/`Struct` schema receivers.
///
/// Notable mappings:
/// - `Custom { semantic_name: Some(name), .. }` → `TypeExpr::Simple(snake)`
///   so the predicate's existing `Simple(name)` arm matches by
///   pascal_to_snake equality.
/// - `Custom { semantic_name: None, .. }` → falls through to the base
///   type; an anonymous Custom is structurally a wrapper and carries
///   no identity to project.
/// - `StringEnum` → `TypeExpr::String` (an enum value is still a
///   string when matched against an annotation; specific enum
///   identities are not currently expressible at the `TypeExpr` level).
fn attribute_type_to_type_expr(attr_type: &AttributeType) -> TypeExpr {
    match attr_type {
        AttributeType::String => TypeExpr::String,
        AttributeType::Int => TypeExpr::Int,
        AttributeType::Float => TypeExpr::Float,
        AttributeType::Bool => TypeExpr::Bool,
        AttributeType::Custom {
            semantic_name: Some(name),
            ..
        } => TypeExpr::Simple(crate::parser::pascal_to_snake(name)),
        AttributeType::Custom { base, .. } => attribute_type_to_type_expr(base),
        AttributeType::StringEnum { .. } => TypeExpr::String,
        AttributeType::List { inner, .. } => {
            TypeExpr::List(Box::new(attribute_type_to_type_expr(inner)))
        }
        AttributeType::Map { value, .. } => {
            TypeExpr::Map(Box::new(attribute_type_to_type_expr(value)))
        }
        AttributeType::Struct { fields, .. } => TypeExpr::Struct {
            fields: fields
                .iter()
                .map(|f| (f.name.clone(), attribute_type_to_type_expr(&f.field_type)))
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
        AttributeType::Union(_) => TypeExpr::String,
    }
}

fn infer_function_call(name: &str) -> Result<TypeExpr, InferenceError> {
    let return_type = builtin_functions()
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.return_type)
        .ok_or_else(|| InferenceError::UnknownType {
            reason: format!("unknown built-in function `{}`", name),
        })?;
    match return_type {
        BuiltinReturnType::String => Ok(TypeExpr::String),
        BuiltinReturnType::Int => Ok(TypeExpr::Int),
        BuiltinReturnType::List => Err(InferenceError::UnknownType {
            reason: format!(
                "built-in `{}` returns a list whose element type depends on arguments",
                name
            ),
        }),
        BuiltinReturnType::Map => Err(InferenceError::UnknownType {
            reason: format!(
                "built-in `{}` returns a map whose value type depends on arguments",
                name
            ),
        }),
        BuiltinReturnType::Secret => Ok(TypeExpr::String),
        BuiltinReturnType::Any => Err(InferenceError::UnknownType {
            reason: format!("built-in `{}` return type depends on arguments", name),
        }),
    }
}

/// Phase transition (#2360 stage 2): walk every `ParsedExportParam` in
/// `parsed`, resolve its effective `TypeExpr` (annotation wins;
/// otherwise infer from rhs), and emit an `InferredFile` with bare
/// `TypeExpr` on every export. Failed inferences are *not* dropped —
/// they are kept with `type_expr: TypeExpr::Unknown` and accompanied
/// by an entry in the returned `Vec<InferenceError>`. The
/// sentinel-over-exclude shape prevents cascading "missing export"
/// diagnostics; the inference error is the single point of truth for
/// "this needs an annotation".
pub fn apply_inference(
    parsed: crate::parser::ParsedFile,
    schemas: &SchemaRegistry,
) -> (crate::parser::InferredFile, Vec<(String, InferenceError)>) {
    let (inferred_exports, errors) = infer_export_params(&parsed, schemas);
    (
        crate::parser::InferredFile {
            providers: parsed.providers,
            resources: parsed.resources,
            variables: parsed.variables,
            uses: parsed.uses,
            module_calls: parsed.module_calls,
            arguments: parsed.arguments,
            attribute_params: parsed.attribute_params,
            export_params: inferred_exports,
            backend: parsed.backend,
            state_blocks: parsed.state_blocks,
            user_functions: parsed.user_functions,
            upstream_states: parsed.upstream_states,
            requires: parsed.requires,
            structural_bindings: parsed.structural_bindings,
            warnings: parsed.warnings,
            deferred_for_expressions: parsed.deferred_for_expressions,
        },
        errors,
    )
}

/// Borrowing variant of [`apply_inference`] that returns just the
/// inferred export parameters and any inference errors. The full
/// `InferredFile` reconstruction in `apply_inference` clones every
/// non-export field; consumers that only need the export-side typecheck
/// (LSP diagnostics, in particular) should use this instead to avoid
/// the per-keystroke deep-clone.
pub fn infer_export_params(
    parsed: &crate::parser::ParsedFile,
    schemas: &SchemaRegistry,
) -> (
    Vec<crate::parser::InferredExportParam>,
    Vec<(String, InferenceError)>,
) {
    let bindings = bindings_from_parsed(parsed);
    let mut errors = Vec::new();
    let inferred_exports: Vec<crate::parser::InferredExportParam> = parsed
        .export_params
        .iter()
        .map(|p| {
            let type_expr =
                match infer_type_expr(p.type_expr.as_ref(), p.value.as_ref(), &bindings, schemas) {
                    Ok(Some(ty)) => ty,
                    Ok(None) => TypeExpr::Unknown,
                    Err(e) => {
                        errors.push((p.name.clone(), e));
                        TypeExpr::Unknown
                    }
                };
            crate::parser::InferredExportParam {
                name: p.name.clone(),
                type_expr,
                value: p.value.clone(),
            }
        })
        .collect();
    (inferred_exports, errors)
}

/// Format an inference error pointing at the offending export. The CLI
/// (`carina-cli/src/commands/validate.rs`) and the LSP
/// (`carina-lsp/src/diagnostics/checks.rs`) both surface this same
/// "type annotation required" wording so editor and command line stay
/// in parity (the existing e2e parity tests pin this).
pub fn format_inference_error(name: &str, err: &InferenceError) -> String {
    format!("export '{}': type annotation required: {}", name, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::AccessPath;
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, legacy_validator,
    };

    fn noop(_v: &Value) -> Result<(), String> {
        Ok(())
    }

    fn vpc_id_custom() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("VpcId".to_string()),
            pattern: None,
            length: None,
            base: Box::new(AttributeType::String),
            validate: legacy_validator(noop),
            namespace: None,
            to_dsl: None,
        }
    }

    fn vpc_schema() -> ResourceSchema {
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
            .attribute(AttributeSchema::new("vpc_id", vpc_id_custom()))
    }

    fn schemas_with_vpc() -> SchemaRegistry {
        let mut s = SchemaRegistry::new();
        s.insert("awscc", vpc_schema());
        s
    }

    fn binding_to_vpc(name: &str) -> InferenceBindings {
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
            &Value::String("hello".to_string()),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn literal_int_inferred_as_int() {
        let r = infer_type_from_value(
            &Value::Int(42),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Int));
    }

    #[test]
    fn literal_bool_inferred_as_bool() {
        let r = infer_type_from_value(
            &Value::Bool(true),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Bool));
    }

    #[test]
    fn interpolation_inferred_as_string() {
        // Build an Interpolation. The inner shape doesn't matter — the
        // result is always String. Use one literal part.
        use crate::resource::InterpolationPart;
        let r = infer_type_from_value(
            &Value::Interpolation(vec![InterpolationPart::Literal("hello".to_string())]),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn secret_inferred_as_string() {
        let r = infer_type_from_value(
            &Value::Secret(Box::new(Value::String("hidden".to_string()))),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn list_of_strings_inferred_as_list_string() {
        let r = infer_type_from_value(
            &Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::List(Box::new(TypeExpr::String))));
    }

    #[test]
    fn list_with_mixed_types_fails() {
        let r = infer_type_from_value(
            &Value::List(vec![Value::String("a".to_string()), Value::Int(1)]),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceError::HeterogeneousCollection { .. })
        ));
    }

    #[test]
    fn map_of_strings_inferred_as_map_string() {
        let mut entries = indexmap::IndexMap::new();
        entries.insert("a".to_string(), Value::String("x".to_string()));
        entries.insert("b".to_string(), Value::String("y".to_string()));
        let r = infer_type_from_value(
            &Value::Map(entries),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::Map(Box::new(TypeExpr::String))));
    }

    #[test]
    fn map_with_mixed_value_types_fails() {
        let mut entries = indexmap::IndexMap::new();
        entries.insert("a".to_string(), Value::String("x".to_string()));
        entries.insert("b".to_string(), Value::Int(1));
        let r = infer_type_from_value(
            &Value::Map(entries),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(
            r,
            Err(InferenceError::HeterogeneousCollection { .. })
        ));
    }

    #[test]
    fn resource_ref_descends_struct_field_path() {
        // `vpc.tags.environment` against a `Struct{environment: String}`.
        // Locks the descent loop so a future refactor cannot silently
        // break field-path traversal.
        use crate::schema::StructField;
        let tags_struct = AttributeType::Struct {
            name: "Tags".to_string(),
            fields: vec![StructField::new("environment", AttributeType::String)],
        };
        let schema =
            ResourceSchema::new("ec2.Vpc").attribute(AttributeSchema::new("tags", tags_struct));
        let mut schemas = SchemaRegistry::new();
        schemas.insert("awscc", schema);

        let path = AccessPath::with_fields("vpc", "tags", vec!["environment".to_string()]);
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
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
            &Value::List(vec![
                Value::FunctionCall {
                    name: "lookup".to_string(),
                    args: vec![],
                },
                Value::String("extra".to_string()),
            ]),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownType { .. })));
    }

    #[test]
    fn empty_list_fails_inference() {
        let r = infer_type_from_value(
            &Value::List(vec![]),
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownType { .. })));
    }

    #[test]
    fn resource_ref_to_custom_attr_inferred_as_simple_snake() {
        // `vpc.vpc_id` where vpc is `awscc.ec2.Vpc` and `vpc_id` is
        // `Custom { semantic_name: "VpcId" }` → `Simple("vpc_id")`.
        // The pascal_to_snake normalization mirrors how the parser
        // stores `: VpcId` annotations.
        let path = AccessPath::new("vpc", "vpc_id");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert_eq!(r, Ok(TypeExpr::Simple("vpc_id".to_string())));
    }

    #[test]
    fn resource_ref_to_string_attr_inferred_as_string() {
        let path = AccessPath::new("vpc", "cidr_block");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn resource_ref_unknown_binding_errors() {
        let path = AccessPath::new("missing", "vpc_id");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &InferenceBindings::new(),
            &schemas_with_vpc(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownBinding { .. })));
    }

    #[test]
    fn resource_ref_unknown_attribute_errors() {
        let path = AccessPath::new("vpc", "missing");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &binding_to_vpc("vpc"),
            &schemas_with_vpc(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownAttribute { .. })));
    }

    #[test]
    fn function_call_join_inferred_as_string() {
        // `join` returns String per builtins metadata.
        let r = infer_type_from_value(
            &Value::FunctionCall {
                name: "join".to_string(),
                args: vec![],
            },
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(TypeExpr::String));
    }

    #[test]
    fn function_call_lookup_fails_inference() {
        // `lookup` returns Any — depends on arguments. Annotation
        // required.
        let r = infer_type_from_value(
            &Value::FunctionCall {
                name: "lookup".to_string(),
                args: vec![],
            },
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownType { .. })));
    }

    #[test]
    fn resource_ref_subscript_on_list_peels_one_layer() {
        // `vpc.subnets[0]` against `subnets: List<String>` infers `String`,
        // not `List<String>`. Without subscript handling the inferer
        // would type the value as the collection itself and a
        // downstream `: String` receiver would spuriously reject.
        let list_str = AttributeType::List {
            inner: Box::new(AttributeType::String),
            ordered: true,
        };
        let schema =
            ResourceSchema::new("ec2.Vpc").attribute(AttributeSchema::new("subnets", list_str));
        let mut schemas = SchemaRegistry::new();
        schemas.insert("awscc", schema);

        let path = AccessPath::with_fields_and_subscripts(
            "vpc",
            "subnets",
            vec![],
            vec![crate::resource::Subscript::Int { index: 0 }],
        );
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
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
            &Value::ResourceRef { path },
            &binding_to_vpc("main"),
            &schemas_with_vpc(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownBinding { .. })));
    }

    #[test]
    fn resource_ref_through_upstream_state_binding_returns_non_inferable() {
        // `network.vpc_id` where `network` is a `let network = upstream_state { ... }`
        // — known binding but the inferer cannot project upstream
        // exports recursively (see #2357). Distinct from `UnknownBinding` (typo) so the
        // outer `infer_type_expr` can pass through cleanly while a real
        // typo bubbles up.
        let mut bindings = InferenceBindings::new();
        bindings.insert("network".to_string(), InferenceBinding::UpstreamState);
        let path = AccessPath::new("network", "vpc_id");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &bindings,
            &SchemaRegistry::new(),
        );
        assert!(matches!(r, Err(InferenceError::NonInferableBinding { .. })));
    }

    #[test]
    fn infer_type_expr_swallows_upstream_state_but_surfaces_typo() {
        // Outer convenience function: NonInferableBinding → Ok(None);
        // UnknownBinding → Err.
        let mut bindings = InferenceBindings::new();
        bindings.insert("network".to_string(), InferenceBinding::UpstreamState);

        let upstream_path = AccessPath::new("network", "vpc_id");
        let r = infer_type_expr(
            None,
            Some(&Value::ResourceRef {
                path: upstream_path,
            }),
            &bindings,
            &SchemaRegistry::new(),
        );
        assert_eq!(r, Ok(None), "upstream_state ref must pass through");

        let typo_path = AccessPath::new("nonexistent", "vpc_id");
        let r = infer_type_expr(
            None,
            Some(&Value::ResourceRef { path: typo_path }),
            &bindings,
            &SchemaRegistry::new(),
        );
        assert!(
            matches!(r, Err(InferenceError::UnknownBinding { .. })),
            "typo'd binding must surface as a hard error"
        );
    }

    #[test]
    fn resource_ref_to_union_typed_attr_demands_annotation() {
        // A `Union` receiver carries multiple alternatives; projecting
        // it to one branch silently lets the value satisfy alternatives
        // it doesn't. Demand annotation instead.
        let union_attr = AttributeType::Union(vec![AttributeType::String, AttributeType::Int]);
        let schema = ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("polymorphic", union_attr));
        let mut schemas = SchemaRegistry::new();
        schemas.insert("awscc", schema);

        let path = AccessPath::new("vpc", "polymorphic");
        let r = infer_type_from_value(
            &Value::ResourceRef { path },
            &binding_to_vpc("vpc"),
            &schemas,
        );
        assert!(matches!(r, Err(InferenceError::UnknownType { .. })));
    }

    #[test]
    fn bindings_from_parts_resource_wins_over_upstream_state_collision() {
        // Mid-edit shape: same binding name appears in both
        // `let x = upstream_state {...}` and `let x = aws.ec2.Vpc {...}`.
        // Insert order (upstream first, resources second) means the
        // resource wins, so `x.attr` infers precisely instead of
        // degrading to `NonInferableBinding`.
        use crate::parser::UpstreamState;
        use crate::resource::Resource;
        let resource = Resource::with_provider("awscc", "ec2.Vpc", "main").with_binding("x");
        let upstream = UpstreamState {
            binding: "x".to_string(),
            source: std::path::PathBuf::from("../other"),
        };
        let bindings = bindings_from_parts(&[resource], &[upstream]);
        assert!(matches!(
            bindings.get("x"),
            Some(InferenceBinding::Resource { .. })
        ));
    }

    #[test]
    fn bindings_from_parts_collects_pure_upstream_state() {
        use crate::parser::UpstreamState;
        let upstream = UpstreamState {
            binding: "network".to_string(),
            source: std::path::PathBuf::from("../network"),
        };
        let bindings = bindings_from_parts(&[], &[upstream]);
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
        use crate::schema::StructField;
        let inner_struct = AttributeType::Struct {
            name: "Statement".to_string(),
            fields: vec![StructField::new(
                "principal",
                AttributeType::Union(vec![AttributeType::String, AttributeType::Int]),
            )],
        };
        let outer_struct = AttributeType::Struct {
            name: "PolicyDocument".to_string(),
            fields: vec![StructField::new(
                "statement",
                AttributeType::List {
                    inner: Box::new(inner_struct),
                    ordered: true,
                },
            )],
        };
        let schema = ResourceSchema::new("iam.Policy")
            .attribute(AttributeSchema::new("policy_document", outer_struct));
        let mut schemas = SchemaRegistry::new();
        schemas.insert("awscc", schema);
        let mut bindings = InferenceBindings::new();
        bindings.insert(
            "policy".to_string(),
            InferenceBinding::Resource {
                provider: "awscc".to_string(),
                resource_type: "iam.Policy".to_string(),
            },
        );
        let path = AccessPath::new("policy", "policy_document");
        let r = infer_type_from_value(&Value::ResourceRef { path }, &bindings, &schemas);
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
    fn function_call_unknown_name_fails_inference() {
        let r = infer_type_from_value(
            &Value::FunctionCall {
                name: "nonexistent".to_string(),
                args: vec![],
            },
            &InferenceBindings::new(),
            &SchemaRegistry::new(),
        );
        assert!(matches!(r, Err(InferenceError::UnknownType { .. })));
    }

    #[test]
    fn apply_inference_fills_inferable_export_with_inferred_type() {
        let mut parsed = crate::parser::ParsedFile::default();
        let res = crate::resource::Resource::with_provider("awscc", "ec2.Vpc", "main")
            .with_binding("main");
        parsed.resources.push(res); // allow: direct — fixture test inspection
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "vpc_id".to_string(),
            type_expr: None,
            value: Some(Value::resource_ref(
                "main".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        });

        let (inferred, errors) = apply_inference(parsed, &schemas_with_vpc());
        assert!(errors.is_empty(), "no errors expected, got {:?}", errors);
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].type_expr,
            TypeExpr::Simple("vpc_id".to_string())
        );
    }

    #[test]
    fn apply_inference_substitutes_unknown_for_failed_inference() {
        let mut parsed = crate::parser::ParsedFile::default();
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "zone_id".to_string(),
            type_expr: None,
            // `lookup` returns Any: inference fails, sentinel produced.
            value: Some(Value::FunctionCall {
                name: "lookup".to_string(),
                args: vec![],
            }),
        });

        let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(inferred.export_params[0].type_expr, TypeExpr::Unknown);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "zone_id");
        assert!(matches!(errors[0].1, InferenceError::UnknownType { .. }));
    }

    #[test]
    fn apply_inference_preserves_explicit_annotation() {
        let mut parsed = crate::parser::ParsedFile::default();
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::Simple("vpc_id".to_string())),
            value: Some(Value::String("vpc-abc".to_string())),
        });

        let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
        assert!(errors.is_empty(), "no errors expected, got {:?}", errors);
        assert_eq!(
            inferred.export_params[0].type_expr,
            TypeExpr::Simple("vpc_id".to_string())
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
        // through the module-call's virtual resource (post-expansion)
        // and the inner role's schema attribute.
        //
        // Pre-#2493 the inference walked `parsed.resources` only, missed
        // the `Virtual` binding (which post-expansion exposes the
        // module's attribute set), and surfaced a misleading
        // "unknown binding" diagnostic.
        use crate::resource::ResourceKind;
        let mut parsed = crate::parser::ParsedFile::default();
        // Concrete role resource with prefixed binding (post-expansion shape).
        let role = crate::resource::Resource::with_provider(
            "awscc",
            "ec2.Vpc",
            "github_actions_carina.role",
        )
        .with_binding("github_actions_carina.role");
        parsed.resources.push(role); // allow: direct — fixture test inspection
        // Virtual resource that exposes the module's attributes.
        // `vpc_id` here stands in for the module-exposed attribute that
        // points at the inner role's schema attribute.
        let mut virt = crate::resource::Resource::new("_virtual", "github_actions_carina");
        virt.binding = Some("github_actions_carina".to_string());
        virt.kind = ResourceKind::Virtual {
            module_name: "github_module".to_string(),
            instance: "github_actions_carina".to_string(),
        };
        virt.attributes.insert(
            "role_id".to_string(),
            Value::resource_ref(
                "github_actions_carina.role".to_string(),
                "vpc_id".to_string(),
                vec![],
            ),
        );
        parsed.resources.push(virt); // allow: direct — fixture test inspection
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "role_id".to_string(),
            type_expr: None,
            value: Some(Value::resource_ref(
                "github_actions_carina".to_string(),
                "role_id".to_string(),
                vec![],
            )),
        });

        let (inferred, errors) = apply_inference(parsed, &schemas_with_vpc());
        assert!(
            errors.is_empty(),
            "module-call attribute export must infer cleanly, got: {errs:?}",
            errs = errors
        );
        assert_eq!(inferred.export_params.len(), 1);
        assert_eq!(
            inferred.export_params[0].type_expr,
            TypeExpr::Simple("vpc_id".to_string()),
            "expected the inner attribute's type to flow through the virtual binding"
        );
    }

    #[test]
    fn apply_inference_treats_pre_expansion_module_call_binding_as_non_inferable() {
        // #2493 load-time path: at `load_configuration` the parser has
        // produced `parsed.module_calls` but module expansion hasn't
        // run yet, so the virtual resource isn't in `parsed.resources`
        // and the bindings map can't see the module-call's projection.
        // The inferer must treat the binding as known-but-non-inferable
        // (mirroring `upstream_state`) so the export gets `Unknown`
        // without a noisy `unknown binding` error. Post-expansion the
        // CLI re-runs inference and the type is filled in via the
        // `Virtual` arm exercised by the sibling test above.
        let mut parsed = crate::parser::ParsedFile::default();
        parsed.module_calls.push(crate::parser::ModuleCall {
            module_name: "github_module".to_string(),
            binding_name: Some("github_actions_carina".to_string()),
            arguments: std::collections::HashMap::new(),
        });
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "role_arn".to_string(),
            type_expr: None,
            value: Some(Value::resource_ref(
                "github_actions_carina".to_string(),
                "role_arn".to_string(),
                vec![],
            )),
        });

        let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
        assert!(
            errors.is_empty(),
            "module-call binding without a virtual resource yet must not error, got: {:?}",
            errors
        );
        assert_eq!(inferred.export_params[0].type_expr, TypeExpr::Unknown);
    }

    #[test]
    fn apply_inference_substitutes_unknown_for_export_without_value() {
        // Export with no value and no annotation: nothing to infer from
        // → Unknown sentinel, no error (the value-less shape is the
        // parser-tombstoned state, not an inference failure).
        let mut parsed = crate::parser::ParsedFile::default();
        parsed.export_params.push(crate::parser::ParsedExportParam {
            name: "vpc_id".to_string(),
            type_expr: None,
            value: None,
        });

        let (inferred, errors) = apply_inference(parsed, &SchemaRegistry::new());
        assert!(errors.is_empty());
        assert_eq!(inferred.export_params[0].type_expr, TypeExpr::Unknown);
    }
}
