//! Analysis-pass diagnostics for `wait <target> { ... }` declarations.
//!
//! Shared by `carina validate` and the LSP. Produces errors for:
//!
//! - **target not found**: `wait foo { ... }` where `foo` is not a
//!   known resource binding in the merged directory parse.
//! - **attribute not in target schema**: `until = cert.statu == ...`
//!   where the target's schema has `status` but not `statu`.
//! - **unsupported composition target**: a module-call composition has
//!   exported values but no provider-backed state for the wait executor
//!   to poll.
//!
//! Operator and shape narrowing (non-`==`, boolean combinators, bare
//! binding LHS) is enforced upstream by `parse_wait_expr`; the parse
//! error surfaces via the regular parser diagnostic path.

use crate::parser::{File, ResourceRef};
use crate::schema::{SchemaKind, SchemaRegistry};

enum WaitTarget {
    Pollable {
        provider: String,
        resource_type: String,
        schema_kind: SchemaKind,
    },
    Composition,
}

/// A wait-construct diagnostic.
///
/// `binding_name` and `target` carry structured location hints so the
/// LSP can resolve a per-span anchor without re-parsing the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitDiagnostic {
    pub message: String,
    pub binding_name: String,
    pub target: String,
    pub attribute: Option<String>,
}

/// Run all wait diagnostics against a parsed file + schema registry.
/// Returns the full list of errors; callers decide how to surface them.
pub fn validate_wait_bindings<E>(
    parsed: &File<E>,
    schemas: &SchemaRegistry,
) -> Vec<WaitDiagnostic> {
    if parsed.wait_bindings.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<WaitDiagnostic> = Vec::new();

    // Build the binding → wait-target lookup once.
    // carina#3181: walk the typed top-level slices so a data-source
    // target (`let x = read ...`) is still found, and carry its
    // `SchemaKind` so the schema lookup below uses the matching kind.
    // Compositions are retained as a distinct target so validation can
    // reject them explicitly instead of silently treating their missing
    // provider schema as a reason to skip the predicate check.
    let mut by_binding: std::collections::HashMap<String, WaitTarget> =
        std::collections::HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(b) = rref.binding() {
            let id = rref.id();
            let target = match rref {
                ResourceRef::Resource(_) | ResourceRef::Deferred { .. } => WaitTarget::Pollable {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    schema_kind: SchemaKind::Resource,
                },
                ResourceRef::DataSource(_) => WaitTarget::Pollable {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                    schema_kind: SchemaKind::DataSource,
                },
                ResourceRef::Composition(_) => WaitTarget::Composition,
            };
            by_binding.insert(b.to_string(), target);
        }
    }

    for wb in &parsed.wait_bindings {
        let Some(target) = by_binding.get(wb.target.as_str()) else {
            out.push(WaitDiagnostic {
                message: format!(
                    "wait `{}`: target binding `{}` is not a known resource",
                    wb.binding, wb.target
                ),
                binding_name: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
                attribute: None,
            });
            continue;
        };
        let WaitTarget::Pollable {
            provider,
            resource_type,
            schema_kind,
        } = target
        else {
            out.push(WaitDiagnostic {
                message: format!(
                    "wait `{}`: target binding `{}` is a composition; waiting on module-call outputs is not supported",
                    wb.binding, wb.target
                ),
                binding_name: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
                attribute: None,
            });
            continue;
        };
        // Attribute existence check against the target's schema. MVP
        // supports only top-level attributes; nested struct fields
        // (`renewal_summary.renewal_status`) are deferred to a follow-up.
        let Some(attr_name) = wb.until_predicate.lhs_segments.get(1) else {
            continue;
        };
        let Some(schema) = schemas.get(provider, resource_type, *schema_kind) else {
            // No schema for this resource type — skip the attr check.
            // The user already gets a separate "unknown resource type"
            // diagnostic from the upstream identifier-scope pass.
            continue;
        };
        if !schema.attributes.contains_key(attr_name) {
            out.push(WaitDiagnostic {
                message: format!(
                    "wait `{}`: `until` references unknown attribute `{}.{}` on `{}.{}`",
                    wb.binding, wb.target, attr_name, provider, resource_type
                ),
                binding_name: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
                attribute: Some(attr_name.clone()),
            });
        }
    }
    out
}
