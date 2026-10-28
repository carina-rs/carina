//! Analysis-pass diagnostics for `wait <target> { ... }` declarations.
//!
//! Shared by `carina validate` and the LSP. Produces errors for:
//!
//! - **target not found**: `wait foo { ... }` where `foo` is not a
//!   known resource binding in the merged directory parse.
//! - **unsupported data-source target**: a `read` binding has no managed
//!   provider identity for the wait executor to poll.
//! - **attribute not in target schema**: `until = cert.statu == ...`
//!   where the target's schema has `status` but not `statu`.
//! - **unresolved enum identifier**: an unquoted RHS is not a member of the
//!   target attribute's enum type (or the attribute is not an enum).
//! - **unsupported composition target**: a module-call composition has
//!   exported values but no provider-backed state for the wait executor
//!   to poll.
//!
//! Operator and shape narrowing (non-`==`, boolean combinators, bare
//! binding LHS) is enforced upstream by `parse_wait_expr`; the parse
//! error surfaces via the regular parser diagnostic path.

use crate::parser::{File, ResourceRef};
use crate::plan::PlanErrorKind;
use crate::schema::{SchemaKind, SchemaRegistry};
use crate::wait::predicate::resolve_wait_predicate;

enum WaitTarget {
    Pollable {
        provider: String,
        resource_type: String,
    },
    DataSource,
    LiveComposition,
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
    pub rhs: Option<String>,
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
    // Walk the typed top-level slices so unsupported data-source and
    // composition targets remain distinct from a missing binding. Both are
    // rejected explicitly before managed-resource predicate validation.
    let mut by_binding: std::collections::HashMap<String, WaitTarget> =
        std::collections::HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(b) = rref.binding() {
            let id = rref.id();
            let target = match rref {
                ResourceRef::Resource(_) | ResourceRef::Deferred { .. } => WaitTarget::Pollable {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                },
                ResourceRef::DataSource(_) => WaitTarget::DataSource,
                ResourceRef::LiveComposition(_) => WaitTarget::LiveComposition,
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
                rhs: None,
            });
            continue;
        };
        let pollable = match target {
            WaitTarget::Pollable {
                provider,
                resource_type,
            } => Ok((provider, resource_type)),
            WaitTarget::DataSource => Err(PlanErrorKind::WaitTargetIsDataSource {
                wait_binding: wb.binding.to_string(),
                target: wb.target.to_string(),
            }
            .to_string()),
            WaitTarget::LiveComposition => Err(format!(
                "wait `{}`: target binding `{}` is a composition; waiting on module-call outputs is not supported",
                wb.binding, wb.target
            )),
        };
        let (provider, resource_type) = match pollable {
            Ok(target) => target,
            Err(message) => {
                out.push(WaitDiagnostic {
                    message,
                    binding_name: wb.binding.as_str().to_string(),
                    target: wb.target.as_str().to_string(),
                    attribute: None,
                    rhs: None,
                });
                continue;
            }
        };
        let Some(schema) = schemas.get(provider, resource_type, SchemaKind::Resource) else {
            // No schema for this resource type — skip the attr check.
            // The user already gets a separate "unknown resource type"
            // diagnostic from the upstream identifier-scope pass.
            continue;
        };
        if let Err(error) = resolve_wait_predicate(
            &wb.until_predicate.lhs_segments,
            wb.until_predicate.rhs.clone(),
            Some(schema),
        ) {
            let attribute = wb
                .until_predicate
                .lhs_segments
                .get(1..)
                .filter(|segments| !segments.is_empty())
                .map(|segments| segments.join("."));
            out.push(WaitDiagnostic {
                message: format!("wait `{}`: {error}", wb.binding),
                binding_name: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
                attribute,
                rhs: error.rhs_anchor().map(str::to_string),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse};
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    };

    fn wait_schemas() -> SchemaRegistry {
        let status = AttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Status"),
            Some(vec!["ISSUED".to_string()]),
            vec![("ISSUED".to_string(), "issued".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (ResourceSchema::new("acm.Certificate")
                .attribute(AttributeSchema::optional("state", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("status", status).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        schemas
    }

    #[test]
    fn unresolved_wait_enum_identifiers_require_string_quotes() {
        let parsed = parse(
            r#"
let cert = aws.acm.Certificate {}
let state_ready = wait cert {
    until = cert.state == ready
}
let status_ready = wait cert {
    until = cert.status == aws.acm.Certificate.Status.bogus
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        let diagnostics = validate_wait_bindings(&parsed, &wait_schemas());
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert!(messages.contains(&"wait `state_ready`: cannot resolve `ready` as an enum value of `cert.state`; quote it to compare as a string"));
        assert!(messages.contains(&"wait `status_ready`: cannot resolve `aws.acm.Certificate.Status.bogus` as an enum value of `cert.status`; quote it to compare as a string"));
    }

    #[test]
    fn quoted_string_and_known_enum_wait_values_are_accepted() {
        let parsed = parse(
            r#"
let cert = aws.acm.Certificate {}
let state_ready = wait cert {
    until = cert.state == 'ready'
}
let status_ready = wait cert {
    until = cert.status == aws.acm.Certificate.Status.issued
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        assert!(validate_wait_bindings(&parsed, &wait_schemas()).is_empty());
    }

    #[test]
    fn nested_struct_enum_wait_value_is_resolved_at_the_full_lhs_path() {
        let renewal_status = AttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "RenewalStatus"),
            Some(vec!["SUCCESS".to_string()]),
            vec![("SUCCESS".to_string(), "success".to_string())],
            None,
            None,
        );
        let renewal_summary = AttributeType::struct_(
            "RenewalSummary",
            vec![StructField::optional("renewal_status", renewal_status).build()],
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (ResourceSchema::new("acm.Certificate")
                .attribute(AttributeSchema::optional("renewal_summary", renewal_summary).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let parsed = parse(
            r#"
let cert = aws.acm.Certificate {}
let renewed = wait cert {
    until = cert.renewal_summary.renewal_status == aws.acm.Certificate.RenewalStatus.success
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        assert_eq!(validate_wait_bindings(&parsed, &schemas), Vec::new());
    }

    #[test]
    fn quoted_wait_value_can_traverse_a_string_map() {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (ResourceSchema::new("job.Job").attribute(
                AttributeSchema::optional("labels", AttributeType::map(AttributeType::string()))
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let parsed = parse(
            r#"
let job = test.job.Job {}
let ready = wait job {
    until = job.labels.state == 'ready'
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        assert_eq!(validate_wait_bindings(&parsed, &schemas), Vec::new());
    }

    #[test]
    fn typed_enum_wait_value_can_traverse_an_enum_map() {
        let status = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["job", "Job"], "Status"),
            Some(vec!["READY".to_string()]),
            vec![("READY".to_string(), "ready".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (ResourceSchema::new("job.Job").attribute(
                AttributeSchema::optional("statuses", AttributeType::map(status)).build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let parsed = parse(
            r#"
let job = test.job.Job {}
let ready = wait job {
    until = job.statuses.primary == test.job.Job.Status.ready
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        assert_eq!(validate_wait_bindings(&parsed, &schemas), Vec::new());
    }

    #[test]
    fn quoted_wait_value_cannot_traverse_a_list() {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (ResourceSchema::new("job.Job").attribute(
                AttributeSchema::optional("states", AttributeType::list(AttributeType::string()))
                    .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let parsed = parse(
            r#"
let job = test.job.Job {}
let ready = wait job {
    until = job.states.first == 'ready'
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        let diagnostics = validate_wait_bindings(&parsed, &schemas);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "wait `ready`: cannot resolve wait attribute path `job.states.first`: `job.states` has type List<String>; only struct fields and map values can be traversed"
        );
    }

    #[test]
    fn data_source_wait_is_rejected_while_managed_wait_is_accepted() {
        let managed_status = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["lookup", "Job"], "Status"),
            Some(vec!["READY".to_string()]),
            vec![("READY".to_string(), "ready".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (ResourceSchema::new("lookup.Job")
                .attribute(AttributeSchema::optional("status", managed_status).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let parsed = parse(
            r#"
let managed = test.lookup.Job {}
let ready = wait managed {
    until = managed.status == test.lookup.Job.Status.ready
}
let lookup = read test.lookup.Job {}
let found = wait lookup {
    until = lookup.status == test.lookup.Job.Status.found
}
"#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");

        assert_eq!(
            validate_wait_bindings(&parsed, &schemas),
            vec![WaitDiagnostic {
                message: "wait `found`: target binding `lookup` is a data source; wait targets must be managed resources".to_string(),
                binding_name: "found".to_string(),
                target: "lookup".to_string(),
                attribute: None,
                rhs: None,
            }]
        );
    }
}
