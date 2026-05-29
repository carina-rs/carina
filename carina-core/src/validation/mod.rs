//! Validation utilities for resources and modules

pub mod deferred_populate;
pub mod depends_on;
pub mod wait;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::binding_index::BindingIndex;
use crate::parser::{ModuleCall, ProviderContext, ResourceRef, TypeExpr, validate_custom_type};
use crate::provider::ProviderFactory;
use crate::resource::{ConcreteValue, DeferredValue, Resource, Value};
use crate::schema::{AttributeType, SchemaRegistry, suggest_similar_name};

/// Render the trailing `" Did you mean 'X'?"` segment for an unknown
/// name in a diagnostic, or an empty string when nothing close enough
/// is found. The leading space is part of the convention so callers
/// can concat unconditionally onto an already-punctuated message.
fn did_you_mean(unknown: &str, known: &[&str]) -> String {
    suggest_similar_name(unknown, known)
        .map(|s| format!(" Did you mean '{}'?", s))
        .unwrap_or_default()
}

/// Validate resources against their schemas.
///
/// Two-sided check: a `read` resource requires a `DataSource` registry entry,
/// and a non-`read` resource requires a `Managed` registry entry. If the
/// wrong-kind entry is present (e.g. `read` against a managed-only type),
/// emit a kind-specific error explaining the mismatch.
pub fn validate_resources<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
    provider_context: &ProviderContext,
) -> Result<(), String> {
    let mut all_errors = Vec::new();
    let lookup = crate::parser::provider_context_lookup(provider_context);

    // Classify per kind via the typed `ResourceRef` arms instead of
    // runtime `is_virtual()` / `is_data_source()` calls (carina#3180 /
    // #3181). compositions are post-apply attribute containers and have no
    // schema to validate against, so they are silently filtered. Managed
    // and data sources route to the same schema-lookup body but render
    // different kind-mismatch diagnostics when the registry entry of the
    // *opposite* kind exists.
    enum ValidatableKind {
        Resource,
        DataSource,
    }
    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed —
        // `for` bodies never carry `read` / composition.
        let (kind, schema) = match rref {
            ResourceRef::Composition(_) => continue,
            ResourceRef::DataSource(d) => {
                (ValidatableKind::DataSource, registry.get_for_data_source(d))
            }
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                (ValidatableKind::Resource, registry.get_for(m))
            }
        };
        let id = rref.id();
        let quoted_string_attrs = rref.quoted_string_attrs();

        match schema {
            Some(schema) => {
                let is_string_literal = |attr: &str| quoted_string_attrs.contains(attr);
                if let Err(errors) = schema.validate_with_origins_and_lookup(
                    &rref.resolved_attributes(),
                    &is_string_literal,
                    &lookup,
                ) {
                    for error in errors {
                        all_errors.push(format!("{}: {}", id, error));
                    }
                }
            }
            None => {
                let provider = id.provider.as_str();
                let resource_type = id.resource_type.as_str();

                // No matching-kind entry. Skip if provider is not loaded —
                // schemas are simply not available, not a configuration error.
                if !provider.is_empty() && !known_providers.contains(provider) {
                    continue;
                }
                let has_managed = registry.has_managed(provider, resource_type);
                let has_data_source = registry.has_data_source(provider, resource_type);
                let kind_label = if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{}.{}", provider, resource_type)
                };

                match kind {
                    ValidatableKind::DataSource if has_managed => {
                        // `read` used against a managed-only type
                        all_errors.push(format!(
                            "{} is a managed resource, not a data source. Remove the `read` keyword:\n  let <name> = {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    ValidatableKind::Resource if has_data_source => {
                        // No `read` against a data-source-only type
                        all_errors.push(format!(
                            "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    _ => {
                        all_errors.push(format!("Unknown resource type: {}", kind_label));
                    }
                }
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that resource references have compatible types.
///
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
pub fn validate_resource_ref_types<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    argument_names: &HashSet<String>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    // Single source of truth for `binding_name → (resource, schema)` —
    // shared with the LSP via `BindingIndex` so the two paths cannot drift
    // (#2231).
    let bindings = BindingIndex::from_parsed(parsed, registry);

    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed.
        let schema = match rref {
            ResourceRef::Composition(_) => continue,
            ResourceRef::DataSource(d) => registry.get_for_data_source(d),
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                registry.get_for(m)
            }
        };
        let Some(schema) = schema else {
            continue;
        };
        let resource_id = rref.id();

        let attrs = rref.attributes();
        for (attr_name, attr_value) in attrs.iter() {
            if attr_name.starts_with('_') {
                continue;
            }

            let (ref_binding, ref_attr, ref_path) = match attr_value {
                Value::Deferred(DeferredValue::ResourceRef { path }) => (
                    path.binding().to_string(),
                    path.attribute().to_string(),
                    path,
                ),
                _ => continue,
            };

            // Get the expected type for this attribute
            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                continue;
            };
            let expected_type_name = attr_schema.attr_type.type_name();

            // Skip type checking for argument parameter references (resolved at call site)
            if argument_names.contains(ref_binding.as_str()) {
                continue;
            }

            // Look up the referenced binding's schema. `BindingIndex::get`
            // returns `Some` only when both the binding and its schema
            // resolved; `contains_name` distinguishes "unknown binding"
            // from "known binding, schema absent" so we keep the original
            // diagnostic shape (only the former gets reported here).
            let Some(ref_entry) = bindings.get(ref_binding.as_str()) else {
                if !bindings.is_declared(ref_binding.as_str()) {
                    all_errors.push(format!(
                        "{}: unknown binding '{}' in reference {}.{}",
                        resource_id, ref_binding, ref_binding, ref_attr,
                    ));
                }
                continue;
            };
            let ref_schema = ref_entry.schema;
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
                let known_attrs: Vec<&str> =
                    ref_schema.attributes.keys().map(|s| s.as_str()).collect();
                all_errors.push(format!(
                    "{}: unknown attribute '{}' on '{}' in reference {}.{}{}",
                    resource_id,
                    ref_attr,
                    ref_binding,
                    ref_binding,
                    ref_attr,
                    did_you_mean(&ref_attr, &known_attrs),
                ));
                continue;
            };

            // Narrow through the path's segments — `[idx]` peels one
            // `List<T>` / `Map<_,V>` layer, `.field` descends a
            // `Struct`. carina#3028. Unknown struct fields are a
            // real typo (carina#3041) and get reported here with a
            // suggestion; other shape mismatches stay silent because
            // resolver-time evaluation catches them with full location
            // context.
            let narrowed = match narrow_attribute_type(
                &ref_attr_schema.attr_type,
                ref_path.segments(),
                &ref_schema.defs,
            ) {
                Ok(t) => t,
                Err(NarrowError::UnknownStructField {
                    field,
                    struct_name,
                    known_fields,
                }) => {
                    let known: Vec<&str> = known_fields.iter().map(|s| s.as_str()).collect();
                    all_errors.push(format!(
                        "{}: unknown field '{}' on struct '{}' in reference {}; \
                         known fields: {}.{}",
                        resource_id,
                        field,
                        struct_name,
                        ref_path.to_dot_string(),
                        known.join(", "),
                        did_you_mean(&field, &known),
                    ));
                    continue;
                }
                Err(NarrowError::ShapeMismatch) => continue,
            };
            let ref_type_name = narrowed.type_name();

            // Directional check: source (the referenced attribute, post
            // path narrowing) must be assignable to the sink (the
            // current resource's attribute).
            if narrowed.is_assignable_to(&attr_schema.attr_type) {
                continue;
            }

            all_errors.push(format!(
                "{}: cannot assign {} to '{}': expected {}, got {} (from {}.{})",
                resource_id,
                ref_type_name,
                attr_name,
                expected_type_name,
                ref_type_name,
                ref_binding,
                ref_attr,
            ));
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that attribute parameter ResourceRef values have types compatible
/// with their declared TypeExpr types.
///
/// For example, `attributes { role_arn: iam_role_arn = role.role_name }` should
/// be rejected because `role_name` is `String`, not `IamRoleArn`.
pub fn validate_attribute_param_ref_types(
    attribute_params: &[crate::parser::AttributeParameter],
    resources: &[Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    let mut errors = Vec::new();

    for param in attribute_params {
        let Some(ref type_expr) = param.type_expr else {
            continue;
        };
        let Some(ref value) = param.value else {
            continue;
        };

        // Only check ResourceRef values
        let Value::Deferred(DeferredValue::ResourceRef { path }) = value else {
            continue;
        };
        let ref_binding = path.binding().to_string();
        let ref_attr = path.attribute().to_string();

        // Get expected type name from TypeExpr
        let expected_type = match type_expr {
            crate::parser::TypeExpr::Simple(name) => name.as_str(),
            _ => continue, // String, Bool, etc. are handled by validate_type_expr_value
        };

        // Look up referenced resource's schema
        let Some(ref_resource) = binding_map.get(&ref_binding) else {
            continue;
        };
        let Some(ref_schema) = registry.get_for(ref_resource) else {
            continue;
        };
        let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
            continue;
        };

        let ref_type_name = ref_attr_schema.attr_type.type_name();
        let ref_type_snake = crate::parser::pascal_to_snake(&ref_type_name);

        if ref_type_snake == expected_type {
            continue;
        }

        errors.push(format!(
            "attribute '{}': type mismatch: expected {}, got {} (from {}.{})",
            param.name, expected_type, ref_type_snake, ref_binding, ref_attr
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Validate export parameter values that are ResourceRef against their declared
/// TypeExpr by looking up the referenced attribute's schema type.
///
/// This catches mismatches like `exports { x: list(bool) = [vpc.vpc_id] }` where
/// `vpc_id` is a string attribute but the export declares `bool`.
pub fn validate_export_param_ref_types(
    export_params: &[crate::parser::InferredExportParam],
    resources: &[Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    let mut errors = Vec::new();

    for param in export_params {
        let Some(ref value) = param.value else {
            continue;
        };
        // Skip the `Unknown` sentinel — the loader's `inference_errors`
        // channel already reports the missing annotation; emitting a
        // "type mismatch" here would be a duplicate.
        if matches!(&param.type_expr, crate::parser::TypeExpr::Unknown) {
            continue;
        }

        collect_ref_type_errors(
            &param.type_expr,
            value,
            &param.name,
            &binding_map,
            registry,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Recursively check ResourceRef values in a value tree against their declared TypeExpr.
fn collect_ref_type_errors(
    type_expr: &crate::parser::TypeExpr,
    value: &Value,
    param_name: &str,
    binding_map: &HashMap<String, &Resource>,
    registry: &SchemaRegistry,
    errors: &mut Vec<String>,
) {
    use crate::parser::TypeExpr;

    match (type_expr, value) {
        (_, Value::Deferred(DeferredValue::ResourceRef { path })) => {
            let ref_binding = path.binding();
            let ref_attr = path.attribute();

            let Some(ref_resource) = binding_map.get(ref_binding) else {
                return;
            };
            let Some(ref_schema) = registry.get_for(ref_resource) else {
                return;
            };
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr) else {
                return;
            };

            let ref_type = &ref_attr_schema.attr_type;
            if !is_type_expr_compatible_with_schema(type_expr, ref_type) {
                let ref_type_name = ref_type.type_name();
                errors.push(format!(
                    "export '{}': type mismatch for '{}.{}': expected {}, got {}",
                    param_name, ref_binding, ref_attr, type_expr, ref_type_name,
                ));
            }
        }
        (TypeExpr::List(inner), Value::Concrete(ConcreteValue::List(items))) => {
            for item in items {
                collect_ref_type_errors(inner, item, param_name, binding_map, registry, errors);
            }
        }
        (TypeExpr::Map(inner), Value::Concrete(ConcreteValue::Map(map))) => {
            for value in map.values() {
                collect_ref_type_errors(inner, value, param_name, binding_map, registry, errors);
            }
        }
        (TypeExpr::Struct { fields }, Value::Concrete(ConcreteValue::Map(map))) => {
            for (name, field_ty) in fields {
                if let Some(value) = map.get(name) {
                    collect_ref_type_errors(
                        field_ty,
                        value,
                        param_name,
                        binding_map,
                        registry,
                        errors,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Check if a TypeExpr is compatible with an AttributeType from a schema.
pub fn is_type_expr_compatible_with_schema(
    type_expr: &crate::parser::TypeExpr,
    attr_type: &AttributeType,
) -> bool {
    use crate::parser::TypeExpr;

    match type_expr {
        // A bare `TypeExpr::String` must not satisfy a receiver typed as
        // `Custom { semantic_name: Some(_) }` — the receiver names a
        // specific identity (e.g. `VpcId`) that a generic `String`
        // cannot prove. Descends into `Union` members so a polymorphic
        // receiver like `Union<[String, Custom{VpcId}]>` is rejected
        // too: every alternative the value might end up satisfying
        // must be reachable from `String`. The symmetric *narrowing*
        // case — a specific `: VpcId` export against a less specific
        // receiver — is handled by the `Simple(name)` arm below: it
        // walks the receiver's `Custom` base chain looking for a
        // matching identity, returning `true` only when the receiver
        // is at least as specific as (or more general than) the
        // declared type. Issue #2358.
        TypeExpr::String => {
            if attr_type_demands_specific_custom(attr_type) {
                return false;
            }
            is_string_compatible_type(attr_type)
        }
        TypeExpr::Bool => matches!(attr_type, AttributeType::Bool),
        TypeExpr::Int => matches!(attr_type, AttributeType::Int),
        TypeExpr::Float => matches!(attr_type, AttributeType::Float),
        TypeExpr::Duration => matches!(attr_type, AttributeType::Duration),
        TypeExpr::Simple(name) => {
            // Two compatibility directions both succeed:
            //
            // 1. Receiver more specific than value. Walk the
            //    receiver's `Custom` chain looking for a level whose
            //    `type_name()` matches `name`. `Simple("arn")` is
            //    accepted by `Custom { KmsKeyArn → Arn }` because
            //    the chain contains `Arn`. Sibling Customs
            //    (`kms_key_arn` vs `IamRoleArn`) stay rejected
            //    because no chain level matches. Issue #1874.
            //
            // 2. Value more specific than receiver (subtyping into
            //    plain string). `Simple(name)` represents a
            //    particular kind of string; flowing it into a plain
            //    `String` receiver erases nothing. The reverse
            //    direction (`String` value into a Custom-with-
            //    semantic-name receiver) stays rejected by
            //    `attr_type_demands_specific_custom`.
            let mut current = attr_type;
            loop {
                let type_snake = crate::parser::pascal_to_snake(&current.type_name());
                if type_snake == *name {
                    return true;
                }
                match current {
                    AttributeType::Custom { base, .. } => current = base,
                    _ => break,
                }
            }
            if is_plain_string_or_string_union(attr_type) {
                return true;
            }
            // Issue #2663: a `Simple(name)` value is unambiguously
            // string-shaped at runtime, so it can flow into a `Union`
            // receiver as long as one member is plain `String` and
            // every other member is shape-disjoint from a string —
            // i.e. `List`/`Map`/`Struct`. The runtime dispatch on
            // shape sends the value to the String branch with no
            // ambiguity. Mixing in another scalar member (e.g.
            // `Union<String, Int>`) keeps falling through here, so
            // the existing `Simple → Union<String, Int>` rejection
            // (`type_compat_simple_rejected_by_mixed_string_int_union_receiver`)
            // is preserved.
            if let AttributeType::Union(members) = attr_type {
                let has_plain_string = members.iter().any(|m| matches!(m, AttributeType::String));
                let others_shape_disjoint = members.iter().all(|m| {
                    matches!(
                        m,
                        AttributeType::String
                            | AttributeType::List { .. }
                            | AttributeType::Map { .. }
                            | AttributeType::Struct { .. }
                    )
                });
                if has_plain_string && others_shape_disjoint {
                    return true;
                }
            }
            false
        }
        TypeExpr::List(inner) => match attr_type {
            AttributeType::List {
                inner: schema_inner,
                ..
            } => is_type_expr_compatible_with_schema(inner, schema_inner),
            _ => false,
        },
        TypeExpr::Map(inner) => match attr_type {
            AttributeType::Map {
                value: schema_inner,
                ..
            } => is_type_expr_compatible_with_schema(inner, schema_inner),
            _ => false,
        },
        TypeExpr::Struct {
            fields: expr_fields,
        } => match attr_type {
            AttributeType::Struct {
                fields: schema_fields,
                ..
            } => {
                // Bijection: every schema field must match exactly one expr
                // field. We check schema ⇒ expr membership with equal
                // lengths; the parser's duplicate-name rejection keeps
                // expr_fields unique, which together forces a one-to-one
                // correspondence.
                if expr_fields.len() != schema_fields.len() {
                    return false;
                }
                schema_fields.iter().all(|sf| {
                    expr_fields.iter().any(|(n, t)| {
                        n == &sf.name && is_type_expr_compatible_with_schema(t, &sf.field_type)
                    })
                })
            }
            // A consumer annotated as `map(T)` may receive a `struct { a: T,
            // b: T }` value — the shape coerces as long as every field type
            // satisfies T.
            AttributeType::Map {
                value: schema_inner,
                ..
            } => expr_fields
                .iter()
                .all(|(_, ty)| is_type_expr_compatible_with_schema(ty, schema_inner)),
            _ => false,
        },
        // Sentinel for failed inference (#2360 stage 2). Never matches a
        // concrete receiver — the inference_errors channel reports the
        // underlying "type annotation required" instead.
        TypeExpr::Unknown => false,
        _ => true, // Ref, SchemaType — conservatively accept
    }
}

/// Check if an AttributeType is string-compatible (can accept a string value).
pub fn is_string_compatible_type(attr_type: &AttributeType) -> bool {
    match attr_type {
        AttributeType::String | AttributeType::Custom { .. } | AttributeType::StringEnum { .. } => {
            true
        }
        AttributeType::Union(types) => types.iter().all(is_string_compatible_type),
        // `AttributeType::Ref` (carina#3340): resolved target is
        // typically a `Struct`, not a string-shaped scalar. Returning
        // `false` is the safe, schema-consistent answer. If a future
        // schema uses `Ref` to point at a non-Struct, this helper
        // should thread `&defs` and recurse via `resolve_refs`.
        AttributeType::Ref(_) => false,
        AttributeType::Int
        | AttributeType::Float
        | AttributeType::Bool
        | AttributeType::Duration
        | AttributeType::CustomEnum { .. }
        | AttributeType::List { .. }
        | AttributeType::Map { .. }
        | AttributeType::Struct { .. } => false,
    }
}

/// Returns `true` only for receivers that name no specific identity:
/// plain `String` or a `Union` of plain Strings. The wider sibling
/// [`is_string_compatible_type`] also accepts `Custom` and `StringEnum`
/// receivers, but those carry constraints (specific identity / fixed
/// value set) that would be erased by accepting a `Simple(name)` value.
fn is_plain_string_or_string_union(attr_type: &AttributeType) -> bool {
    match attr_type {
        AttributeType::String => true,
        AttributeType::Union(types) => types.iter().all(is_plain_string_or_string_union),
        // `Ref` resolves to a Struct in every cyclic CFN schema today;
        // returning `false` is conservative and consistent with
        // is_string_compatible_type. (carina#3340.)
        AttributeType::Ref(_) => false,
        AttributeType::Int
        | AttributeType::Float
        | AttributeType::Bool
        | AttributeType::Duration
        | AttributeType::Custom { .. }
        | AttributeType::CustomEnum { .. }
        | AttributeType::StringEnum { .. }
        | AttributeType::List { .. }
        | AttributeType::Map { .. }
        | AttributeType::Struct { .. } => false,
    }
}

/// Recursive check used by the `TypeExpr::String` arm of
/// `is_type_expr_compatible_with_schema`: returns `true` when
/// `attr_type` carries a `Custom { identity: Some(_) }` either at
/// the top level or anywhere inside a `Union`. A schema attribute that
/// names a specific identity (`VpcId`, `Arn`, …) cannot accept a value
/// known only as `String`. Issue #2358.
///
/// Scope:
/// - Looks at the outer `identity` only — does **not** walk
///   `Custom.base` chains. Real provider schemas keep `identity`
///   on the outer wrapper, so an anonymous `Custom` wrapping a
///   specific `Custom` does not occur in practice. If a future schema
///   introduces that shape, this helper would need to walk the base
///   chain too.
/// - Only `String`-shaped specifics are guarded today. Provider
///   schemas currently express every named-identity Custom as a
///   `String`-base wrapper, so `TypeExpr::Int/Bool/Float` arms have
///   no analogous strictness. If a future schema adds e.g. a
///   `Custom { identity: "Port", base: Int }`, those arms will
///   also need to consult this helper (or a sibling).
fn attr_type_demands_specific_custom(attr_type: &AttributeType) -> bool {
    match attr_type {
        AttributeType::Custom {
            identity: Some(_), ..
        } => true,
        AttributeType::Union(types) => types.iter().any(attr_type_demands_specific_custom),
        // `Ref` (carina#3340): does not itself demand a custom identity.
        // The resolved target may carry one, but `_specific_custom`
        // operates at the receiver type's outer shape — Union is the
        // only nesting it traverses, by design (see doc comment).
        AttributeType::Ref(_) => false,
        AttributeType::String
        | AttributeType::Int
        | AttributeType::Float
        | AttributeType::Bool
        | AttributeType::Duration
        | AttributeType::Custom { identity: None, .. }
        | AttributeType::CustomEnum { .. }
        | AttributeType::StringEnum { .. }
        | AttributeType::List { .. }
        | AttributeType::Map { .. }
        | AttributeType::Struct { .. } => false,
    }
}

/// Check that a root configuration does not contain `arguments` blocks.
///
/// `arguments` is a module-input declaration: it belongs on the module side
/// of a module boundary and is paired with `use` on the caller side. In a
/// root configuration there is no caller to pass values, so the block has
/// no meaning — its `default` would silently become a de-facto root
/// variable, which is not a documented feature (issue #2198).
///
/// A directory loaded via the CLI may be either a root config or a module
/// the user is validating in isolation. We only flag the misplaced block
/// when a `backend` or `provider` block is also present, since both are
/// root-only constructs and unambiguously identify a root configuration.
pub fn validate_no_arguments_in_root<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_root = parsed.backend.is_some() || !parsed.providers.is_empty();
    if !parsed.arguments.is_empty() && is_root {
        return Err(
            "arguments blocks are only valid inside module definitions, not in root configurations.".to_string(),
        );
    }
    Ok(())
}

/// Reject module-level type declarations whose type position names an
/// unknown bare custom type (carina#3239).
///
/// Walks every typed parameter `parsed` carries — `arguments`,
/// `attributes`, `exports` (when typed) — and applies the same
/// predicate the parser's `customs_loaded` gate uses.
///
/// The parser already rejects unknown `TypeExpr::Simple` names when
/// it is handed a `ProviderContext` with `customs_loaded = true`. That
/// gate fires for every parse path *after* the provider-registration
/// phase has populated the context — imported modules re-parsed by
/// `module_resolver::resolve_modules_with_config` and every LSP
/// diagnostic pass.
///
/// The root-config parse is the one exception: `load_configuration_with_config`
/// runs with the bootstrap context (`customs_loaded = false`) because
/// schemas have not been collected yet, so a standalone-module
/// validate (`carina validate ./my_module/`) would let an unknown
/// custom-type name in `arguments { foo: TotallyMadeUpType }` slip
/// through. This post-parse walk re-applies the same predicate against
/// the now-enriched context, closing the gap without re-parsing.
///
/// `attributes` and `exports` are covered for the same reason as
/// `arguments`: all three are module-boundary type declarations, all
/// three are reached through the same root-config parse, and an
/// unknown bare custom type in any of them surfaces the identical
/// silent-accept bug.
///
/// The check is restricted to bare PascalCase names that parsed as
/// `TypeExpr::Simple`. Provider-scoped customs (`aws.iam.Role.Arn`)
/// travel through `TypeExpr::SchemaType` and have their own
/// `schema_types`-based validation; they are out of scope here.
pub fn validate_argument_custom_types<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    for arg in &parsed.arguments {
        collect_unknown_simple_types_in(&arg.type_expr, config, "argument", &arg.name, &mut errors);
    }
    for ap in &parsed.attribute_params {
        if let Some(ty) = &ap.type_expr {
            collect_unknown_simple_types_in(ty, config, "attribute", &ap.name, &mut errors);
        }
    }
    for ep in &parsed.export_params {
        if let Some(ty) = ep.type_expr_opt() {
            collect_unknown_simple_types_in(ty, config, "export", ep.name(), &mut errors);
        }
    }
    errors
}

/// Recursively walk a [`TypeExpr`] and push one diagnostic per
/// [`TypeExpr::Simple`] whose name is not a known bare custom type
/// under `config`. Helper for [`validate_argument_custom_types`].
///
/// Each emitted message is a single line (no embedded newlines) so the
/// caller can `split('\n')` to lift findings into individual errors.
///
/// Variants that carry no nested `TypeExpr` are listed explicitly
/// rather than caught by a wildcard: a future variant that *does* nest
/// a `TypeExpr` should be a compile error here, not a silent
/// type-checking gap.
fn collect_unknown_simple_types_in(
    ty: &crate::parser::TypeExpr,
    config: &ProviderContext,
    decl_kind: &str,
    decl_name: &str,
    errors: &mut Vec<String>,
) {
    use crate::parser::TypeExpr;
    match ty {
        TypeExpr::Simple(snake) => {
            if !crate::parser::is_known_bare_custom_type(snake, config) {
                let pascal = crate::parser::snake_to_pascal(snake);
                errors.push(format!(
                    "{decl_kind} '{decl_name}': unknown custom type '{pascal}'"
                ));
            }
        }
        TypeExpr::List(inner) | TypeExpr::Map(inner) => {
            collect_unknown_simple_types_in(inner, config, decl_kind, decl_name, errors);
        }
        TypeExpr::Union(members) => {
            for m in members {
                collect_unknown_simple_types_in(m, config, decl_kind, decl_name, errors);
            }
        }
        TypeExpr::Struct { fields } => {
            for (_, field_ty) in fields {
                collect_unknown_simple_types_in(field_ty, config, decl_kind, decl_name, errors);
            }
        }
        // Leaves with no nested `TypeExpr` to recurse into. Listed
        // explicitly so a future variant that *does* nest one fails to
        // compile here instead of silently bypassing the walk.
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Ref(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Unknown => {}
    }
}

/// Check that a module file does not contain provider blocks.
///
/// Provider configuration should only be defined at the root configuration level,
/// not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_provider_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.providers.is_empty() {
        return Err(
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Returns `true` if `value` contains any deferred sub-value that the
/// WASM provider boundary would reject (ResourceRef, BindingRef,
/// Interpolation, FunctionCall, Unknown). Used by
/// [`validate_provider_config`] to skip the plugin-side `validate_config`
/// call for attributes whose refs cannot be substituted at validate
/// time. `Secret` is transparent — its inner value is unwrapped because
/// the secret wrapper survives WASM serialization but the inner value
/// must still be checked. carina#3182.
pub(crate) fn value_contains_unresolved_ref(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => true,
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_unresolved_ref(inner),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(value_contains_unresolved_ref)
        }
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(value_contains_unresolved_ref),
        Value::Concrete(_) => false,
    }
}

/// Validate provider configuration attributes.
///
/// Runs host-side type-level validation using
/// [`ProviderFactory::provider_config_attribute_types`] first, then
/// delegates to [`ProviderFactory::validate_config`] for any
/// provider-specific semantic checks. Keeping format validation
/// (namespace structure, enum membership) on the host side means fixes
/// in `carina-core` take effect without rebuilding provider binaries.
///
/// Attributes containing unresolved references (e.g.
/// `assume_role = { role_arn = upstream.arn }` at validate time, before
/// plan/apply has fetched upstream state) are passed through host-side
/// type validation — `AttributeType::validate` is deferred-aware and
/// no-ops on `Value::Deferred` — but **excluded from the plugin-side
/// `validate_config` call**, because the WASM serializer rejects
/// deferred values. The same `validate_config` runs again at plan/apply
/// time once the refs have been substituted by
/// [`resolve_provider_attributes_with_remote`], so no plugin-side check
/// is permanently lost. carina#3182.
pub fn validate_provider_config<E>(
    parsed: &crate::parser::File<E>,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), String> {
    for provider in &parsed.providers {
        let Some(factory) = factories.iter().find(|f| f.name() == provider.name) else {
            continue;
        };
        // Host-side type-level validation. Routed through
        // `Schema::validate_attr` with an empty `defs` because provider
        // configs are flat (no cyclic CFN-style Refs today); if a
        // future provider config grows a `Ref`, the empty-defs path
        // returns a clean `ValidationFailed` instead of tripping the
        // standalone validator sentinel (carina#3345).
        let attr_types = factory.provider_config_attribute_types();
        let schema_view = crate::schema::Schema::with_defs(std::collections::BTreeMap::new());
        for (attr_name, value) in &provider.attributes {
            if let Some(attr_type) = attr_types.get(attr_name) {
                schema_view
                    .validate_attr(attr_type, value)
                    .map_err(|e| format!("provider {}: {}: {}", provider.name, attr_name, e))?;
            }
        }
        // Plugin-side validation. Drop attributes containing unresolved
        // refs before crossing the WASM boundary; they will be checked
        // again at plan/apply time post-resolution.
        let serializable: IndexMap<String, Value> = provider
            .attributes
            .iter()
            .filter(|(_, value)| !value_contains_unresolved_ref(value))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        factory
            .validate_config(&serializable)
            .map_err(|e| format!("provider {}: {}", provider.name, e))?;
    }
    Ok(())
}

/// Validate module call arguments against module argument types.
///
/// `imported_modules` maps module alias to its argument parameter definitions.
/// `config` provides custom type validators from providers.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &HashMap<String, Vec<crate::parser::ArgumentParameter>>,
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(module_args) = imported_modules.get(&call.module_name) {
            for (arg_name, arg_value) in &call.arguments {
                if let Some(arg_param) = module_args.iter().find(|a| &a.name == arg_name)
                    && let Some(error) =
                        validate_type_expr_value(&arg_param.type_expr, arg_value, config)
                {
                    errors.push(format!(
                        "module {} argument '{}': {}",
                        call.module_name, arg_name, error
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Validate export parameter values against their declared type annotations.
///
/// For each export with both a `type_expr` and a `value`, validates the value
/// using `validate_type_expr_value`. Accumulates all errors.
///
/// Accepts post-inference [`InferredExportParam`]s (#2360 stage 2):
/// `type_expr` is bare. Sentinel-bearing exports
/// (`TypeExpr::Unknown`) are skipped — the loader's `inference_errors`
/// channel surfaces those, and re-checking would double-report.
pub fn validate_export_params(
    export_params: &[crate::parser::InferredExportParam],
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in export_params {
        if matches!(&param.type_expr, crate::parser::TypeExpr::Unknown) {
            continue;
        }
        if let Some(value) = &param.value
            && let Some(error) = validate_type_expr_value(&param.type_expr, value, config)
        {
            errors.push(format!("export '{}': {}", param.name, error));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Check for unused `let` bindings and return the unused binding names.
///
/// A binding is unused if its name never appears as a `ResourceRef.binding_name`
/// in any resource attribute, module call argument, or attribute parameter value.
///
/// Generic over the export-parameter shape so both `ParsedFile` (parser
/// phase) and `InferredFile` (post-loader phase) can drive it without
/// duplicating the binding walk.
pub fn check_unused_bindings<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
) -> Vec<String> {
    // Collect all defined binding names (skip discard pattern `_`).
    // Walk top-level and for-body resources so bindings declared inside a
    // `for` template are also tracked.
    let mut defined_bindings: Vec<String> = Vec::new();
    for rref in parsed.iter_all_resources() {
        if let Some(binding_name) = rref.binding() {
            if binding_name == "_" {
                continue;
            }
            defined_bindings.push(binding_name.to_string());
        }
    }

    if defined_bindings.is_empty() {
        return Vec::new();
    }

    // Collect all referenced binding names. Walk both top-level resources
    // and for-body template resources so bindings referenced only inside a
    // `for` loop are counted as used.
    //
    // `collect_dot_notation_refs` also runs on resource attributes: when
    // a resource in file A references `binding.attr` where `binding` is
    // declared in sibling file B, per-file parse stores it as
    // `Value::Concrete(ConcreteValue::String("binding.attr"))`. `resolve_resource_refs_with_config`
    // lifts those to `ResourceRef` only when the value sits at the top
    // level of an attribute; inside a list / map / interpolation the
    // string form survives, so a reference nested in
    // `principals = [binding.attr]` would otherwise be missed.
    let mut referenced: HashSet<String> = HashSet::new();
    for rref in parsed.iter_all_resources() {
        let attrs = rref.attributes();
        for (attr_name, value) in attrs.iter() {
            if attr_name.starts_with('_') {
                continue;
            }
            collect_resource_refs(value, &mut referenced);
            collect_dot_notation_refs(value, &mut referenced);
        }
        // `Composition` has no directives — `directives()` is `None`
        // for that arm, so the depends_on walk is simply skipped.
        for dep in rref.directives().into_iter().flat_map(|d| &d.depends_on) {
            referenced.insert(dep.clone());
        }
    }
    for call in &parsed.module_calls {
        for value in call.arguments.values() {
            collect_resource_refs(value, &mut referenced);
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    for attr_param in &parsed.attribute_params {
        if let Some(value) = &attr_param.value {
            collect_resource_refs(value, &mut referenced);
        }
    }
    for export_param in &parsed.export_params {
        if let Some(value) = export_param.value() {
            collect_resource_refs(value, &mut referenced);
            // Cross-file: when exports.crn is parsed without the binding context,
            // "vpc.vpc_id" becomes String("vpc.vpc_id") instead of ResourceRef.
            // Extract the binding name from such dot-notation strings.
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    for attr_param in &parsed.attribute_params {
        if let Some(value) = &attr_param.value {
            collect_dot_notation_refs(value, &mut referenced);
        }
    }
    // Each `wait <target> { ... }` declaration references its target
    // and every binding in `depends_on = [...]`. The until predicate's
    // LHS is rooted at the target (enforced by parser), so the target
    // covers the LHS path too.
    for wb in &parsed.wait_bindings {
        referenced.insert(wb.target.as_str().to_string());
        for dep in &wb.depends_on {
            referenced.insert(dep.as_str().to_string());
        }
    }

    // Return unused binding names, skipping structurally-required bindings
    // (if/for/read expressions) and for-generated indexed bindings (e.g., vpcs[0])
    defined_bindings
        .into_iter()
        .filter(|binding| {
            !referenced.contains(binding)
                && !parsed.structural_bindings.contains(binding)
                && !binding.contains('[')
        })
        .collect()
}

/// Recursively collect all `ResourceRef` binding names from a value tree.
pub(crate) fn collect_resource_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            refs.insert(path.binding().to_string());
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                collect_resource_refs(item, refs);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                collect_resource_refs(v, refs);
            }
        }
        _ => {}
    }
}

/// Extract binding names from dot-notation string values (e.g., "vpc.vpc_id" → "vpc").
///
/// When files are parsed independently, cross-file references like `vpc.vpc_id`
/// become `String("vpc.vpc_id")` instead of `ResourceRef`. This function extracts
/// the first component as a potential binding name.
pub(crate) fn collect_dot_notation_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::Concrete(ConcreteValue::String(s))
            if s.contains('.') && !s.contains(' ') && !s.starts_with('/') =>
        {
            if let Some(binding) = s.split('.').next()
                && !binding.is_empty()
                && binding
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                refs.insert(binding.to_string());
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                collect_dot_notation_refs(item, refs);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                collect_dot_notation_refs(v, refs);
            }
        }
        _ => {}
    }
}

/// Validate a value against a TypeExpr, returning an error message if invalid.
///
/// Shared validation logic used by both CLI module call validation and LSP diagnostics.
/// `config` provides custom type validators from providers (e.g., `iam_policy_arn`).
pub fn validate_type_expr_value(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Option<String> {
    // `Value::Deferred(DeferredValue::Unknown)` resolves at upstream apply — the concrete type
    // is unknowable here. Same skip rule the schema validator and
    // `check_fn_arg_type` follow.
    if matches!(value, Value::Deferred(DeferredValue::Unknown(_))) {
        return None;
    }
    match (type_expr, value) {
        (TypeExpr::Simple(name), _) => {
            let identity = crate::schema::TypeIdentity::bare(crate::parser::snake_to_pascal(name));
            validate_custom_type(&identity, value, config).err()
        }
        (TypeExpr::List(inner), Value::Concrete(ConcreteValue::List(items))) => {
            for (i, item) in items.iter().enumerate() {
                if let Some(e) = validate_type_expr_value(inner, item, config) {
                    return Some(format!("Element {}: {}", i, e));
                }
            }
            None
        }
        (TypeExpr::Struct { fields }, Value::Concrete(ConcreteValue::Map(entries))) => {
            validate_struct_fields(fields, entries, config)
        }
        (TypeExpr::Struct { .. }, _) => Some(format!(
            "expected {}, got {}.",
            type_expr,
            crate::parser::value_type_name(value)
        )),
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::String(s))) => Some(format!(
            "expected {type_expr}, got string \"{s}\". Use true or false."
        )),
        (TypeExpr::Int, Value::Concrete(ConcreteValue::String(s))) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::Float, Value::Concrete(ConcreteValue::String(s))) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Int(n))) => {
            Some(format!("expected {type_expr}, got int ({n})."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::Int(n))) => {
            Some(format!("expected {type_expr}, got int ({n})."))
        }
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Int, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        (TypeExpr::Int, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Float, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        // Intentional one-way widening: an Int may flow into a Float sink.
        // The reverse (Float -> Int) is rejected above. Mirrors the schema
        // validator's `(Float, Int) => Ok` rule in `schema/mod.rs`.
        (TypeExpr::Float, Value::Concrete(ConcreteValue::Int(_))) => None,
        // Schema types are string subtypes — reject non-string values
        (TypeExpr::SchemaType { .. }, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {}, got bool ({}).", type_expr, b))
        }
        (TypeExpr::SchemaType { .. }, Value::Concrete(ConcreteValue::Int(n))) => {
            Some(format!("expected {}, got int ({}).", type_expr, n))
        }
        (TypeExpr::SchemaType { .. }, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {}, got float ({}).", type_expr, f))
        }
        _ => None,
    }
}

/// Check shape-level problems of a `Value::Concrete(ConcreteValue::Map)` against a struct field
/// list: extra keys and missing keys. Returns `None` when the key sets
/// match. Callers then walk each field with their own type-check pass.
pub fn struct_field_shape_errors(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
) -> Option<String> {
    // Sort unknown keys so the diagnostic is stable across HashMap's
    // per-process random hash seed.
    let mut unknown: Vec<&String> = entries
        .keys()
        .filter(|k| !fields.iter().any(|(name, _)| &name == k))
        .collect();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Some(format!("expected struct, unknown field '{key}'."));
    }
    for (name, _) in fields {
        if !entries.contains_key(name) {
            return Some(format!("expected struct, missing field '{}'.", name));
        }
    }
    None
}

fn validate_struct_fields(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
    config: &ProviderContext,
) -> Option<String> {
    if let Some(e) = struct_field_shape_errors(fields, entries) {
        return Some(e);
    }
    for (name, ty) in fields {
        if let Some(v) = entries.get(name)
            && let Some(e) = validate_type_expr_value(ty, v, config)
        {
            return Some(format!("field '{}': {}", name, e));
        }
    }
    None
}

/// Walk `field_path` against `start`. Return `Ok(tail_type)` on a
/// clean walk and `Err((mismatched_type, bad_segment))` for the first
/// segment that can't be resolved. Lists, maps, and scalars never host
/// `.field` access — the parent type they reach is the right anchor
/// for the diagnostic builder's "use iteration / subscript / nothing"
/// suggestion.
///
/// Walk `start` through a chain of `.field` segments (the leading
/// `PathSegment::Field` prefix of an [`AccessPath`]). Stops at the first
/// non-`Field` segment and returns the type at that position together
/// with the index of the segment that stopped descent — callers that
/// need to continue with subscripts use [`narrow_type_expr`], callers
/// that only care about the field-path leg use [`walk_type_expr_fields`].
///
/// Walks by reference so deep struct paths don't pay an O(depth) clone
/// chain — the caller clones once at the return site if it needs an
/// owned copy.
///
/// `Map` segments unwrap to the value type so dot-form key access
/// (`accounts.k → T`) is symmetric with the subscript form
/// (`accounts['k']`). #2447.
pub(crate) fn walk_type_expr_fields<'a, 'b>(
    start: &'a TypeExpr,
    field_path: &'b [String],
) -> Result<&'a TypeExpr, (&'a TypeExpr, &'b str)> {
    let mut current = start;
    for segment in field_path {
        match current {
            TypeExpr::Struct { fields } => match fields.iter().find(|(name, _)| name == segment) {
                Some((_, ty)) => current = ty,
                None => return Err((current, segment.as_str())),
            },
            TypeExpr::Map(inner) => current = inner.as_ref(),
            _ => return Err((current, segment.as_str())),
        }
    }
    Ok(current)
}

/// Narrow `start` through an `AccessPath`'s ordered segments — a free
/// mix of `.field` and `[index]` continuations at any depth
/// (carina#3025). Returns `None` when a step doesn't fit the container
/// kind; those mismatches are reported by the dedicated shape checkers
/// and a duplicate here would be noise.
///
/// Used by both upstream-export type-checking and module-call
/// attribute-export inference.
pub(crate) fn narrow_type_expr(
    start: &TypeExpr,
    segments: &[crate::resource::PathSegment],
) -> Option<TypeExpr> {
    use crate::resource::{PathSegment, Subscript};
    let mut current = start.clone();
    for seg in segments {
        current = match (current, seg) {
            (TypeExpr::Struct { fields }, PathSegment::Field { name }) => {
                let (_, ty) = fields.into_iter().find(|(n, _)| n == name)?;
                ty
            }
            (TypeExpr::Map(inner), PathSegment::Field { .. }) => *inner,
            (
                TypeExpr::List(inner),
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
            ) => *inner,
            (
                TypeExpr::Map(inner),
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
            ) => *inner,
            _ => return None,
        };
    }
    Some(current)
}

/// Narrow `start` (a schema [`AttributeType`]) through an
/// [`AccessPath`](crate::resource::AccessPath)'s ordered segments — a
/// free mix of `.field` (descend into a `Struct`) and `[idx]` (peel one
/// `List<T>` / `Map<_, V>` layer) continuations (carina#3025).
///
/// Borrows so deep paths don't pay an O(depth) clone chain. The error
/// variant ([`NarrowError`]) distinguishes a real field typo
/// (actionable, suggest a sibling) from a structural shape mismatch
/// (caller decides whether resolver-time location context is enough).
pub(crate) fn narrow_attribute_type<'a>(
    start: &'a AttributeType,
    segments: &[crate::resource::PathSegment],
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Result<&'a AttributeType, NarrowError> {
    use crate::resource::{PathSegment, Subscript};
    use crate::schema::Shape;
    let mut current = start;
    for seg in segments {
        // Project onto `Shape` so any `Ref` chain is peeled at the
        // type level (carina#3349). Without this, a `Ref`-typed
        // attribute would fall into the wildcard arm below and
        // every nested narrowing step would mis-report a shape
        // mismatch.
        let shape = current.shape(defs);
        current = match (seg, shape) {
            (PathSegment::Field { name }, Shape::Struct { fields, name: sn }) => {
                let Some(field) = fields.iter().find(|f| f.name == *name) else {
                    return Err(NarrowError::UnknownStructField {
                        field: name.clone(),
                        struct_name: sn.to_string(),
                        known_fields: fields.iter().map(|f| f.name.clone()).collect(),
                    });
                };
                &field.field_type
            }
            // Dot-form key access against a `map(_, V)` projects to
            // `V`, mirroring the resolver's behaviour (carina#2447).
            (PathSegment::Field { .. }, Shape::Map { value, .. }) => value,
            (
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
                Shape::List { inner, .. },
            ) => inner,
            (
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
                Shape::Map { value, .. },
            ) => value,
            _ => return Err(NarrowError::ShapeMismatch),
        };
    }
    Ok(current)
}

/// Reason [`narrow_attribute_type`] rejected a path.
#[derive(Debug)]
pub(crate) enum NarrowError {
    /// A `.field` segment named a field that doesn't exist on the
    /// current `Struct`. Carries the struct's declared name and the
    /// names of its fields so the caller can render a suggestion.
    UnknownStructField {
        field: String,
        struct_name: String,
        known_fields: Vec<String>,
    },
    /// A segment didn't fit the container at its position
    /// (e.g. `.x` against a scalar, `[0]` against a struct).
    ShapeMismatch,
}

pub mod inference;

#[cfg(test)]
mod tests;
