//! Analysis-pass diagnostics for `wait <target> { ... }` declarations.
//!
//! Shared by `carina validate` and the LSP. Produces errors for:
//!
//! - **target not found**: `wait foo { ... }` where `foo` is not a
//!   known resource binding in the merged directory parse.
//! - **attribute not in target schema**: `until = cert.statu == ...`
//!   where the target's schema has `status` but not `statu`.
//!
//! Operator and shape narrowing (non-`==`, boolean combinators, bare
//! binding LHS) is enforced upstream by `parse_wait_expr`; the parse
//! error surfaces via the regular parser diagnostic path.

use crate::parser::File;
use crate::schema::{SchemaKind, SchemaRegistry};

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

    // Build binding → (provider, resource_type) lookup once.
    let mut by_binding: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for r in &parsed.resources {
        if let Some(b) = &r.binding {
            by_binding.insert(
                b.clone(),
                (r.id.provider.clone(), r.id.resource_type.clone()),
            );
        }
    }

    for wb in &parsed.wait_bindings {
        let Some((provider, resource_type)) = by_binding.get(&wb.target) else {
            out.push(WaitDiagnostic {
                message: format!(
                    "wait `{}`: target binding `{}` is not a known resource",
                    wb.binding, wb.target
                ),
                binding_name: wb.binding.clone(),
                target: wb.target.clone(),
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
        let Some(schema) = schemas.get(provider, resource_type, SchemaKind::Managed) else {
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
                binding_name: wb.binding.clone(),
                target: wb.target.clone(),
                attribute: Some(attr_name.clone()),
            });
        }
    }
    out
}
