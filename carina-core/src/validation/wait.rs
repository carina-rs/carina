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

use crate::parser::{AnalysisItemKind, AnalysisTree, Located};
use crate::resource::BindingAddress;
use crate::schema::{SchemaKind, SchemaRegistry};
use crate::wait::predicate::validate_wait_predicate;

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
/// The detail retains semantic context. The enclosing `Located` owns the only
/// rendering anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitDiagnostic {
    pub message: String,
    pub binding_name: String,
    pub target: String,
}

impl std::fmt::Display for WaitDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Run all wait diagnostics against a parsed file + schema registry.
/// Returns the full list of errors; callers decide how to surface them.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, schema::SchemaRegistry, validation::wait::validate_wait_bindings};
/// fn analyze(file: &ParsedFile) {
///     validate_wait_bindings(file, &SchemaRegistry::new());
/// }
/// ```
pub fn validate_wait_bindings<D, B, E>(
    parsed: &AnalysisTree<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
        E,
    >,
    schemas: &SchemaRegistry,
) -> Vec<Located<WaitDiagnostic>>
where
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    if parsed.waits().next().is_none() {
        return Vec::new();
    }
    let mut out: Vec<Located<WaitDiagnostic>> = Vec::new();

    // Build the binding → wait-target lookup once.
    // Walk the typed top-level slices so unsupported data-source and
    // composition targets remain distinct from a missing binding. Both are
    // rejected explicitly before managed-resource predicate validation.
    let mut by_binding: std::collections::HashMap<B, WaitTarget> = std::collections::HashMap::new();
    for rref in parsed.top_level_resources() {
        if let Some(b) = rref.binding_address() {
            let id = rref.id();
            let target = match rref.kind() {
                AnalysisItemKind::Resource | AnalysisItemKind::DeferredResource => {
                    WaitTarget::Pollable {
                        provider: id.provider.clone(),
                        resource_type: id.resource_type.clone(),
                    }
                }
                AnalysisItemKind::DataSource => WaitTarget::DataSource,
                AnalysisItemKind::Composition => WaitTarget::LiveComposition,
                AnalysisItemKind::Wait => unreachable!("resources do not contain waits"),
            };
            by_binding.insert(b.clone(), target);
        }
    }

    for wb in parsed.waits() {
        let Some(target) = by_binding.get(wb.target()) else {
            out.push(Located::new(
                wb.target_location().clone(),
                WaitDiagnostic {
                    message: format!(
                        "wait `{}`: target binding `{}` is not a known resource",
                        wb.binding(),
                        wb.target()
                    ),
                    binding_name: wb.binding().as_str().to_string(),
                    target: wb.target().as_str().to_string(),
                },
            ));
            continue;
        };
        let pollable = match target {
            WaitTarget::Pollable {
                provider,
                resource_type,
            } => Ok((provider, resource_type)),
            WaitTarget::DataSource => Err(format!(
                "wait `{}`: target binding `{}` is a data source; wait targets must be managed resources",
                wb.binding(),
                wb.target()
            )),
            WaitTarget::LiveComposition => Err(format!(
                "wait `{}`: target binding `{}` is a composition; waiting on module-call outputs is not supported",
                wb.binding(),
                wb.target()
            )),
        };
        let (provider, resource_type) = match pollable {
            Ok(target) => target,
            Err(message) => {
                out.push(Located::new(
                    wb.target_location().clone(),
                    WaitDiagnostic {
                        message,
                        binding_name: wb.binding().as_str().to_string(),
                        target: wb.target().as_str().to_string(),
                    },
                ));
                continue;
            }
        };
        let Some(schema) = schemas.get(provider, resource_type, SchemaKind::Resource) else {
            // No schema for this resource type — skip the attr check.
            // The user already gets a separate "unknown resource type"
            // diagnostic from the upstream identifier-scope pass.
            continue;
        };
        if let Err(error) = validate_wait_predicate(
            &wb.until_predicate().root,
            &wb.until_predicate().attribute_segments,
            &wb.until_predicate().rhs,
            Some(schema.schema()),
        ) {
            let location = if error.rhs_anchor().is_some() {
                wb.rhs_location()
            } else {
                wb.lhs_location()
            };
            out.push(Located::new(
                location.clone(),
                WaitDiagnostic {
                    message: format!("wait `{}`: {error}", wb.binding()),
                    binding_name: wb.binding().as_str().to_string(),
                    target: wb.target().as_str().to_string(),
                },
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;
    use crate::parser::{ProviderContext, parse};
    use crate::schema::TypeIdentity;

    fn wait_schemas() -> SchemaRegistry {
        let status = PatternAttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Status"),
            Some(vec!["ISSUED".to_string()]),
            vec![("ISSUED".to_string(), "issued".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (PatternResourceSchema::new("acm.Certificate")
                .attribute(
                    PatternAttributeSchema::optional("state", PatternAttributeType::string())
                        .build(),
                )
                .attribute(PatternAttributeSchema::optional("status", status).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        schemas
    }

    #[test]
    fn unresolved_wait_enum_identifiers_require_string_quotes()
    -> Result<(), crate::parser::AnalysisTreeError> {
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        let diagnostics =
            validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &wait_schemas());
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert!(messages.contains(&"wait `state_ready`: cannot resolve `ready` as an enum value of `cert.state`; quote it to compare as a string"));
        assert!(messages.contains(&"wait `status_ready`: cannot resolve `aws.acm.Certificate.Status.bogus` as an enum value of `cert.status`; quote it to compare as a string"));
        Ok(())
    }

    #[test]
    fn quoted_string_and_known_enum_wait_values_are_accepted()
    -> Result<(), crate::parser::AnalysisTreeError> {
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        assert!(
            validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &wait_schemas())
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn nested_struct_enum_wait_value_is_resolved_at_the_full_lhs_path()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let renewal_status = PatternAttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "RenewalStatus"),
            Some(vec!["SUCCESS".to_string()]),
            vec![("SUCCESS".to_string(), "success".to_string())],
            None,
            None,
        );
        let renewal_summary = PatternAttributeType::struct_(
            "RenewalSummary",
            vec![PatternStructField::optional("renewal_status", renewal_status).build()],
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            (PatternResourceSchema::new("acm.Certificate").attribute(
                PatternAttributeSchema::optional("renewal_summary", renewal_summary).build(),
            ))
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        assert_eq!(
            validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &schemas),
            Vec::new()
        );
        Ok(())
    }

    #[test]
    fn quoted_wait_value_can_traverse_a_string_map() -> Result<(), crate::parser::AnalysisTreeError>
    {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (PatternResourceSchema::new("job.Job").attribute(
                PatternAttributeSchema::optional(
                    "labels",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        assert_eq!(
            validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &schemas),
            Vec::new()
        );
        Ok(())
    }

    #[test]
    fn typed_enum_wait_value_can_traverse_an_enum_map()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let status = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["job", "Job"], "Status"),
            Some(vec!["READY".to_string()]),
            vec![("READY".to_string(), "ready".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (PatternResourceSchema::new("job.Job").attribute(
                PatternAttributeSchema::optional("statuses", PatternAttributeType::map(status))
                    .build(),
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        assert_eq!(
            validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &schemas),
            Vec::new()
        );
        Ok(())
    }

    #[test]
    fn quoted_wait_value_cannot_traverse_a_list() -> Result<(), crate::parser::AnalysisTreeError> {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (PatternResourceSchema::new("job.Job").attribute(
                PatternAttributeSchema::optional(
                    "states",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        let diagnostics = validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &schemas);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "wait `ready`: cannot resolve wait attribute path `job.states.first`: `job.states` has type List<String>; only struct fields and map values can be traversed"
        );
        Ok(())
    }

    #[test]
    fn data_source_wait_is_rejected_while_managed_wait_is_accepted()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let managed_status = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["lookup", "Job"], "Status"),
            Some(vec!["READY".to_string()]),
            vec![("READY".to_string(), "ready".to_string())],
            None,
            None,
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            (PatternResourceSchema::new("lookup.Job")
                .attribute(PatternAttributeSchema::optional("status", managed_status).build()))
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");

        let details = validate_wait_bindings(&AnalysisTree::try_from_file(&parsed)?, &schemas)
            .into_iter()
            .map(|diagnostic| diagnostic.into_parts().1)
            .collect::<Vec<_>>();
        assert_eq!(
            details,
            vec![WaitDiagnostic {
                message: "wait `found`: target binding `lookup` is a data source; wait targets must be managed resources".to_string(),
                binding_name: "found".to_string(),
                target: "lookup".to_string(),
            }]
        );
        Ok(())
    }
}
