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

use crate::deps::sort_resources_by_dependencies;
use crate::parser::File;
use crate::resource::{Resource, ResourceKind};
use crate::validation::{collect_dot_notation_refs, collect_resource_refs};

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

/// Run all depends_on checks against a parsed file. Returns the full
/// list of diagnostics (errors + warnings); callers decide how to surface
/// them.
pub fn validate_depends_on<E>(parsed: &File<E>) -> Vec<DependsOnDiagnostic> {
    // Common-case fast path: the vast majority of resources will have
    // no `depends_on` set, and the checks below allocate per-resource.
    // Walk top-level + for-body template resources so loops are covered
    // (per the carina-core "directory-scoped, never single-file" rule).
    if parsed
        .iter_all_resources()
        .all(|(_, r)| r.directives.depends_on.is_empty())
    {
        return Vec::new();
    }

    let mut diags = Vec::new();

    let bindings_by_name: HashMap<&str, &Resource> = parsed
        .iter_all_resources()
        .filter_map(|(_, r)| r.binding.as_deref().map(|n| (n, r)))
        .collect();
    let upstream_names: HashSet<&str> = parsed
        .upstream_states
        .iter()
        .map(|us| us.binding.as_str())
        .collect();

    for (_, resource) in parsed.iter_all_resources() {
        if resource.directives.depends_on.is_empty() {
            continue;
        }
        // `self_name` is only needed for self-reference detection; an
        // anonymous resource cannot self-reference (no name to point
        // back to), so an unbound resource just gets the other checks.
        let self_name = resource.binding.as_deref().unwrap_or("(anonymous)");

        let mut value_ref_deps: HashSet<String> = HashSet::new();
        for value in resource.attributes.values() {
            collect_resource_refs(value, &mut value_ref_deps);
            collect_dot_notation_refs(value, &mut value_ref_deps);
        }
        for name in &resource.dependency_bindings {
            value_ref_deps.insert(name.clone());
        }

        let mut seen: HashSet<&str> = HashSet::new();
        let mut duplicate_warned: HashSet<&str> = HashSet::new();
        for dep_name in &resource.directives.depends_on {
            if !seen.insert(dep_name.as_str()) {
                if duplicate_warned.insert(dep_name.as_str()) {
                    diags.push(
                        DependsOnDiagnostic::warning(format!(
                            "directives.depends_on on '{}': binding '{}' is listed multiple times",
                            self_name, dep_name
                        ))
                        .at_element(self_name, dep_name),
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
                    .at_element(self_name, dep_name),
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
                    .at_element(self_name, dep_name),
                );
                continue;
            }

            let Some(target) = bindings_by_name.get(dep_name.as_str()) else {
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': binding '{}' is not declared in this scope",
                        self_name, dep_name
                    ))
                    .at_element(self_name, dep_name),
                );
                continue;
            };

            if matches!(target.kind, ResourceKind::DataSource) {
                diags.push(
                    DependsOnDiagnostic::error(format!(
                        "directives.depends_on on '{}': data sources cannot be \
                         depends_on targets (binding '{}')",
                        self_name, dep_name
                    ))
                    .at_element(self_name, dep_name),
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
                    .at_element(self_name, dep_name),
                );
            }
        }
    }

    // Cycle detection over the unioned graph (value-refs ∪ depends_on).
    // We only reach here if at least one resource declares depends_on
    // (early-return at top), so a cycle here is at least *touched* by
    // depends_on — but the cycle itself may go through value refs only.
    // Don't claim attribution; just report the cycle.
    if let Err(msg) = sort_resources_by_dependencies(&parsed.resources) {
        diags.push(DependsOnDiagnostic::error(msg));
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_and_resolve;

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
    fn upstream_state_target_is_diagnosed_as_error() {
        let src = r#"
            let orgs = upstream_state { source = "../organizations" }
            let bucket = aws.s3.Bucket {
                bucket_name = "x"
                directives { depends_on = [orgs] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let diags = validate_depends_on(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("upstream_state") && m.contains("'orgs'")),
            "expected upstream_state error, got {:?}",
            errors
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
    fn redundant_edge_via_dot_notation_string_is_diagnosed_as_warning() {
        // Ensures the redundant-edge check matches what `check_unused_bindings`
        // sees: dot-notation strings inside collections (e.g.
        // `principals = [role.arn]`) survive resolution as
        // `Value::Concrete(ConcreteValue::String("role.arn"))`, not `Value::Deferred(DeferredValue::ResourceRef)`. Without
        // `collect_dot_notation_refs` here, the warning would silently
        // miss this shape.
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
            "expected redundant-edge warning for dot-notation ref, got {:?}",
            warnings
        );
    }

    #[test]
    fn unknown_binding_in_for_body_is_diagnosed() {
        // `for` template resources must be walked too — without
        // `iter_all_resources`, depends_on inside loops is silently
        // skipped at validate time.
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
