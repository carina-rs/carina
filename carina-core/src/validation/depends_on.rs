//! Analysis-pass diagnostics for `directives { depends_on = [...] }`.
//!
//! Shared by `carina validate` and the LSP. Produces:
//!
//! - **Errors**: unknown binding, self-reference, disallowed kind
//!   (data source / upstream_state target), cycle.
//! - **Warnings**: duplicate element, redundant edge already implied
//!   by a value reference.
//!
//! Element-type errors (`["foo"]` instead of `[foo]`) are caught
//! upstream by `parser::blocks::resource::check_directives_depends_on_elements`
//! before this pass runs.

use std::collections::{HashMap, HashSet};

use crate::deps::{collect_dependencies, sort_resources_by_dependencies};
use crate::parser::{
    File, ResourceRef, TypeExpr, UnexpandedDependencies, UnexpandedDependencyOperand,
};

/// Severity of a depends_on diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A single depends_on diagnostic.
///
/// `binding_name` and `dep_name` carry structured location hints so
/// the LSP can resolve a per-element span without re-parsing the
/// validation message text. Both are populated when the diagnostic
/// pertains to a specific binding (`directives.depends_on on '<name>'`)
/// and / or element (`binding '<name>'`); cycle errors and other
/// whole-graph diagnostics leave them `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependsOnDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub binding_name: Option<String>,
    pub dep_name: Option<String>,
}

impl DependsOnDiagnostic {
    fn error(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: msg.into(),
            binding_name: None,
            dep_name: None,
        }
    }

    fn warning(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            message: msg.into(),
            binding_name: None,
            dep_name: None,
        }
    }

    #[allow(dead_code)]
    fn at_binding(mut self, binding: &str) -> Self {
        self.binding_name = Some(binding.to_string());
        self
    }

    fn at_element(mut self, binding: &str, dep: &str) -> Self {
        self.binding_name = Some(binding.to_string());
        self.dep_name = Some(dep.to_string());
        self
    }
}

/// The `directives.depends_on` list of a resource, or an empty slice.
///
/// `LiveComposition` carries no directives — [`ResourceRef::directives`]
/// is `None` for that arm, treated here as an empty `depends_on` list.
fn depends_on_of<'a, D: crate::parser::DeferredForPhaseTraversal>(
    rref: ResourceRef<'a, crate::resource::ModuleSource, D>,
) -> &'a UnexpandedDependencies {
    static EMPTY: std::sync::LazyLock<UnexpandedDependencies> =
        std::sync::LazyLock::new(UnexpandedDependencies::default);
    rref.directives().map_or(&EMPTY, |d| &d.depends_on)
}

/// Run all depends_on checks against a parsed file. Returns the full
/// list of diagnostics (errors + warnings); callers decide how to surface
/// them.
pub fn validate_depends_on<E, D: crate::parser::DeferredForPhaseTraversal>(
    parsed: &File<E, D>,
) -> Vec<DependsOnDiagnostic> {
    // Common-case fast path: the vast majority of resources will have
    // no `depends_on` set, and the checks below allocate per-resource.
    // Walk top-level + for-body template resources so loops are covered
    // (per the carina-core "directory-scoped, never single-file" rule).
    let resources_have_no_depends_on = parsed
        .iter_all_resources()
        .all(|rref| depends_on_of(rref).is_empty());
    if resources_have_no_depends_on {
        return Vec::new();
    }

    let mut diags = Vec::new();

    let bindings_by_name: HashMap<&str, ResourceRef<'_, crate::resource::ModuleSource, D>> = parsed
        .iter_all_resources()
        .filter_map(|rref| rref.binding().map(|n| (n, rref)))
        .collect();
    let upstream_names: HashSet<&str> = parsed
        .upstream_states
        .iter()
        .map(|us| us.binding.as_str())
        .collect();
    // A module parameter is a typed, parser-approved dependency target whose
    // concrete handle is supplied by the caller during expansion. It is not a
    // local resource binding, so standalone module validation must recognize
    // it from the module signature instead of reconstructing validity from
    // `bindings_by_name`. Expansion performs the corresponding value-vs-handle
    // check and rewrites the dependency to the caller's canonical binding.
    let module_parameters: HashMap<&str, &TypeExpr> = parsed
        .arguments
        .iter()
        .map(|argument| (argument.name.as_str(), &argument.static_type))
        .collect();

    let mut check_resource = |binding: Option<&str>,
                              attributes: &indexmap::IndexMap<String, crate::resource::Value>,
                              dependency_bindings: &std::collections::BTreeSet<String>,
                              depends_on: &UnexpandedDependencies| {
        if depends_on.is_empty() {
            return;
        }
        // `self_name` is only needed for self-reference detection; an
        // anonymous resource cannot self-reference (no name to point
        // back to), so an unbound resource just gets the other checks.
        let self_name = binding.unwrap_or("(anonymous)");

        let mut value_ref_deps: HashSet<String> = HashSet::new();
        for value in attributes.values() {
            collect_dependencies(value, &mut value_ref_deps);
        }
        for name in dependency_bindings {
            value_ref_deps.insert(name.clone());
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut duplicate_warned: HashSet<String> = HashSet::new();
        for operand in depends_on.iter() {
            let dep_name = operand.diagnostic_reference();
            if !seen.insert(dep_name.clone()) {
                if duplicate_warned.insert(dep_name.clone()) {
                    diags.push(
                        DependsOnDiagnostic::warning(format!(
                            "directives.depends_on on '{}': binding '{}' is listed multiple times",
                            self_name, dep_name
                        ))
                        .at_element(self_name, &dep_name),
                    );
                }
                continue;
            }

            if dep_name == self_name {
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': self-reference is not allowed \
                         (binding '{}' depends on itself)",
                        self_name, dep_name
                    ))
                    .at_element(self_name, &dep_name),
                );
                continue;
            }

            if upstream_names.contains(dep_name.as_str()) {
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': upstream_state binding '{}' \
                         is not a valid depends_on target",
                        self_name, dep_name
                    ))
                    .at_element(self_name, &dep_name),
                );
                continue;
            }

            let Some(target) = bindings_by_name.get(dep_name.as_str()) else {
                if matches!(operand, UnexpandedDependencyOperand::Handle(_)) {
                    continue;
                }
                if let Some(parameter_type) = module_parameters.get(dep_name.as_str()) {
                    match parameter_type {
                        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) => continue,
                        TypeExpr::String
                        | TypeExpr::Bool
                        | TypeExpr::Int
                        | TypeExpr::Float
                        | TypeExpr::Duration
                        | TypeExpr::Simple(_)
                        | TypeExpr::List(_)
                        | TypeExpr::Map(_)
                        | TypeExpr::SchemaType { .. }
                        | TypeExpr::Struct { .. }
                        | TypeExpr::StringLiteral(_)
                        | TypeExpr::Union(_) => {
                            diags.push(
                                DependsOnDiagnostic::error(format!(
                                    "directives.depends_on: expected a managed resource handle; got {parameter_type}",
                                ))
                                .at_element(self_name, &dep_name),
                            );
                            continue;
                        }
                    }
                }
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': binding '{}' is not declared in this scope",
                        self_name, dep_name
                    ))
                    .at_element(self_name, &dep_name),
                );
                continue;
            };

            if matches!(target, ResourceRef::DataSource(_)) {
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': data sources cannot be \
                         depends_on targets (binding '{}')",
                        self_name, dep_name
                    ))
                    .at_element(self_name, &dep_name),
                );
                continue;
            }

            if value_ref_deps.contains(dep_name.as_str()) {
                diags.push(
                    DependsOnDiagnostic::warning(format!(
                        "directives.depends_on on '{}': edge to '{}' is redundant; \
                         '{}' is already referenced by value",
                        self_name, dep_name, dep_name
                    ))
                    .at_element(self_name, &dep_name),
                );
            }
        }
    };

    for resource in parsed.iter_all_resources() {
        let attributes = resource.attributes();
        check_resource(
            resource.binding(),
            &attributes,
            resource.dependency_bindings(),
            depends_on_of(resource),
        );
    }

    // Cycle detection over the unioned graph (value-refs ∪ depends_on).
    // We only reach here if at least one resource declares depends_on
    // (early-return at top), so a cycle here is at least *touched* by
    // depends_on — but the cycle itself may go through value refs only.
    // Don't claim attribution; just report the cycle.
    //
    // carina#3181: only managed resources can participate in a cycle —
    // data sources are rejected as `depends_on` targets above (so they
    // can be sources but never the depended-on end of an edge), and
    // composition resources are post-apply synthetic nodes with no
    // directives. A cycle therefore lives entirely within the managed
    // slice.
    if let Err(msg) = sort_resources_by_dependencies(&parsed.resources) {
        diags.push(DependsOnDiagnostic::error(msg));
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse, parse_and_resolve};

    fn diag_messages(diags: &[DependsOnDiagnostic], sev: Severity) -> Vec<String> {
        diags
            .iter()
            .filter(|d| d.severity == sev)
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn unknown_binding_is_diagnosed_as_error() {
        let src = r#"
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [non_existent] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("non_existent") && m.contains("not declared")),
            "expected 'not declared' error mentioning non_existent, got {:?}",
            errors
        );
    }

    #[test]
    fn parser_side_deferred_resource_depends_on_is_diagnosed() {
        let parsed = parse(
            r#"
                let pending = upstream_state { source = "../pending" }
                let buckets = for item in pending.items {
                    aws.s3.Bucket {
                        bucket_name = item
                        directives { depends_on = [non_existent] }
                    }
                }
            "#,
            &ProviderContext::default(),
        )
        .expect("fixture must parse");
        assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);

        let errors = diag_messages(&validate_depends_on(&parsed), Severity::Error);
        assert!(
            errors
                .iter()
                .any(|message| message.contains("non_existent") && message.contains("not declared")),
            "expected deferred body depends_on diagnostic, got {errors:?}",
        );
    }

    #[test]
    fn self_reference_is_diagnosed_as_error() {
        let src = r#"
            let a = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [a] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("self-reference") && m.contains("'a'")),
            "expected self-reference error, got {:?}",
            errors
        );
    }

    #[test]
    fn duplicate_element_is_diagnosed_as_warning() {
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [role, role] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let warnings = diag_messages(&diags, Severity::Warning);
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("listed multiple times") && m.contains("'role'")),
            "expected duplicate warning, got {:?}",
            warnings
        );
    }

    #[test]
    fn duplicate_element_listed_three_times_emits_one_warning() {
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [role, role, role] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let warnings = diag_messages(&diags, Severity::Warning);
        let role_warnings: Vec<_> = warnings
            .iter()
            .filter(|m| m.contains("listed multiple times") && m.contains("'role'"))
            .collect();
        assert_eq!(
            role_warnings.len(),
            1,
            "expected exactly one duplicate warning per binding, got {:?}",
            warnings
        );
    }

    #[test]
    fn redundant_value_ref_edge_is_diagnosed_as_warning() {
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = role.role_name
                directives { depends_on = [role] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let warnings = diag_messages(&diags, Severity::Warning);
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("redundant") && m.contains("'role'")),
            "expected redundant-edge warning, got {:?}",
            warnings
        );
    }

    #[test]
    fn cycle_is_diagnosed_as_error() {
        let src = r#"
            let a = aws.s3.Bucket {
                bucket_name = "a"
                directives { depends_on = [b] }
            }
            let b = aws.s3.Bucket {
                bucket_name = "b"
                directives { depends_on = [a] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("Circular") || m.contains("cycle")),
            "expected cycle error, got {:?}",
            errors
        );
    }

    #[test]
    fn upstream_state_target_is_rejected_by_the_typed_directive_parser() {
        let src = r#"
            let orgs = upstream_state { source = "../organizations" }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [orgs] }
            }
        "#;
        let error = parse_and_resolve(src).expect_err("upstream state is not a dependency target");
        let rendered = error.to_string();
        assert!(
            rendered.contains("line 5")
                && rendered.contains("expected a managed resource handle")
                && rendered.contains("got an upstream state"),
            "expected a typed upstream-state directive error, got {rendered:?}",
        );
    }

    #[test]
    fn data_source_target_is_diagnosed_as_error() {
        let src = r#"
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [user] }
            }
            let user = read aws.iam.User {
                user_name = "alice"
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("data sources") && m.contains("'user'")),
            "expected data-source error, got {:?}",
            errors
        );
    }

    #[test]
    fn redundant_edge_via_value_ref_is_diagnosed_as_warning() {
        // The redundant-edge check walks structural value references,
        // including nested map/list attribute values.
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                tags = { Owner = role.role_name }
                directives { depends_on = [role] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let warnings = diag_messages(&diags, Severity::Warning);
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("redundant") && m.contains("'role'")),
            "expected redundant-edge warning for value ref, got {:?}",
            warnings
        );
    }

    #[test]
    fn redundant_edge_via_interpolation_ref_is_diagnosed_as_warning() {
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = "acct-${role.arn}"
                directives { depends_on = [role] }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).unwrap();
        let diags = validate_depends_on(&parsed);
        let warnings = diag_messages(&diags, Severity::Warning);
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("redundant") && m.contains("'role'")),
            "expected redundant-edge warning for interpolation ref, got {:?}",
            warnings
        );
    }

    #[test]
    fn unknown_binding_in_for_body_is_diagnosed() {
        // Parser-side `for` templates must be walked explicitly; they do not
        // enter the resource-only iterator until module resolution.
        let src = r#"
            provider test {
                source = 'x/y'
                version = '0.1'
                region = 'ap-northeast-1'
            }
            let orgs = upstream_state {
                source = "../organizations"
            }
            for account_id in orgs.accounts {
                test.r.res {
                    name = account_id
                    directives { depends_on = [non_existent] }
                }
            }
        "#;
        let parsed = crate::parser::parse(src, &crate::parser::ProviderContext::default()).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("non_existent") && m.contains("not declared")),
            "expected 'not declared' error from for-body, got {:?}",
            errors
        );
    }

    #[test]
    fn no_diagnostics_for_valid_depends_on() {
        let src = r#"
            let role = aws.iam.Role {
                role_name = "r"
                assume_role_policy_document = "{}"
            }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [role] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        assert!(
            diags.is_empty(),
            "expected zero diagnostics for valid depends_on, got {:?}",
            diags
        );
    }
}
