//! Analysis-pass diagnostics for resource and wait `depends_on` lists.
//!
//! Shared by `carina validate` and the LSP.
//!
//! Both resource and wait owners check dependency scope and kind, reporting
//! unknown or disallowed targets as errors and duplicate elements as warnings.
//! Data sources are valid wait dependencies.
//!
//! Resources additionally report self-dependencies and cycles as errors, and
//! redundant edges already implied by value references as warnings.
//!
//! Element-type errors (`["foo"]` instead of `[foo]`) are caught
//! upstream by `parser::blocks::resource::check_directives_depends_on_elements`
//! before this pass runs.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

use crate::deps::{collect_dependencies, sort_resources_by_dependencies};
use crate::parser::{File, ResourceRef, UnexpandedDependencyOperand};

use super::{DependencyArgumentResolution, ResolvedArgumentTypes, ResolvedDependencyArgumentType};
use crate::resource::{BindingAddress, ModuleSource, ParserPlaceholder};

type ParsedResourceRef<'a, D, B> =
    ResourceRef<'a, ModuleSource, ParserPlaceholder, ParserPlaceholder, D, B>;

/// Severity of a depends_on diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// The declaration whose dependency list owns an element diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependsOnOwner {
    Resource,
    Wait,
}

impl DependsOnOwner {
    fn context(self) -> &'static str {
        match self {
            Self::Resource => "directives.depends_on",
            Self::Wait => "wait.depends_on",
        }
    }
}

/// The owner binding and optional element of a depends_on diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependsOnLocation {
    pub owner: DependsOnOwner,
    pub binding: String,
    pub dep: Option<String>,
}

/// A single depends_on diagnostic.
///
/// `location` lets the LSP resolve a binding or element span without parsing
/// the message text. Cycle errors and other whole-graph diagnostics leave it
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependsOnDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub location: Option<DependsOnLocation>,
}

impl DependsOnDiagnostic {
    fn error(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: msg.into(),
            location: None,
        }
    }

    fn warning(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            message: msg.into(),
            location: None,
        }
    }

    fn at_element(mut self, owner: DependsOnOwner, binding: &str, dep: &str) -> Self {
        self.location = Some(DependsOnLocation {
            owner,
            binding: binding.to_string(),
            dep: Some(dep.to_string()),
        });
        self
    }
}

enum DependencyTarget<'a> {
    Managed,
    DataSource,
    Upstream,
    ModuleParam(&'a DependencyArgumentResolution),
    LoopGroup,
    Handle,
    Unknown,
}

/// Resolve retained operands against declarations, never a union of lexical names.
fn classify_operand<'a, E, B: BindingAddress, D>(
    parsed: &File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    bindings_by_name: &HashMap<&str, ParsedResourceRef<'_, D, B>>,
    argument_types: &'a ResolvedArgumentTypes,
    managed_group_names: &OnceCell<HashSet<String>>,
    operand: &UnexpandedDependencyOperand<B>,
    dep_name: &str,
) -> DependencyTarget<'a>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    if parsed
        .upstream_states
        .iter()
        .any(|upstream| upstream.binding.as_str() == dep_name)
    {
        return DependencyTarget::Upstream;
    }
    if let Some(target) = bindings_by_name.get(dep_name) {
        return if matches!(target, ResourceRef::DataSource(_)) {
            DependencyTarget::DataSource
        } else {
            DependencyTarget::Managed
        };
    }
    if let UnexpandedDependencyOperand::BareBinding(binding) = operand
        && parsed
            .parser_binding_metadata
            .structural_handles()
            .get(binding)
            .is_some_and(|handle| handle.is_for_binding())
    {
        return DependencyTarget::LoopGroup;
    }
    if matches!(operand, UnexpandedDependencyOperand::Handle(_)) {
        return DependencyTarget::Handle;
    }
    if matches!(operand, UnexpandedDependencyOperand::BareBinding(_))
        && managed_group_names
            .get_or_init(|| {
                parsed
                    .iter_all_resources()
                    .filter(|resource| {
                        matches!(
                            resource,
                            ResourceRef::Resource(_) | ResourceRef::Deferred { .. }
                        )
                    })
                    .filter_map(|resource| resource.binding_address())
                    .flat_map(crate::resolved_graph::loop_group_bindings)
                    .map(|group| group.to_string())
                    .collect()
            })
            .contains(dep_name)
    {
        return DependencyTarget::LoopGroup;
    }
    // Declaration resolution supplies the same value-vs-handle result for
    // module arguments in both dependency-list contexts.
    if let Some(parameter_type) = argument_types.types.get(dep_name) {
        return DependencyTarget::ModuleParam(parameter_type);
    }
    DependencyTarget::Unknown
}

/// The `directives.depends_on` list of a resource, or an empty slice.
///
/// `LiveComposition` carries no explicit dependencies and is treated as an
/// empty `depends_on` list.
fn depends_on_of<'a, B: BindingAddress, D>(
    rref: ParsedResourceRef<'a, D, B>,
) -> Option<&'a crate::parser::UnexpandedDependencies<B>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    rref.explicit_dependencies()
}

/// Run all depends_on checks against a parsed file. Returns the full
/// list of diagnostics (errors + warnings); callers decide how to surface
/// them.
///
/// Raw parses cannot be checked without the argument-type resolution witness:
///
/// ```compile_fail,E0061
/// use carina_core::{parser::ParsedFile, validation::depends_on::validate_depends_on};
/// let unresolved = ParsedFile::default();
/// validate_depends_on(&unresolved);
/// ```
pub fn validate_depends_on<E, B: BindingAddress, D>(
    parsed: &File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    argument_types: &ResolvedArgumentTypes,
) -> Vec<DependsOnDiagnostic>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    // Common-case fast path: most resources and waits have no `depends_on`
    // set, and the checks below allocate per-resource.
    // Walk top-level + for-body template resources so loops are covered
    // (per the carina-core "directory-scoped, never single-file" rule).
    let resources_have_no_depends_on = parsed
        .iter_all_resources()
        .all(|rref| depends_on_of(rref).is_none_or(|dependencies| dependencies.is_empty()));
    let waits_have_no_depends_on = parsed
        .wait_bindings
        .iter()
        .all(|wait| wait.depends_on.is_empty());
    if resources_have_no_depends_on && waits_have_no_depends_on {
        return Vec::new();
    }

    let mut diags = Vec::new();

    let bindings_by_name: HashMap<&str, ParsedResourceRef<'_, D, B>> = parsed
        .iter_all_resources()
        .filter_map(|rref| rref.binding().map(|n| (n, rref)))
        .collect();
    // Most operands resolve directly or carry parser-approved handles. Rebuild
    // expanded loop groups only when a retained bare operand needs that lookup.
    let managed_group_names = OnceCell::new();

    let mut check_dependencies =
        |owner_kind: DependsOnOwner,
         binding: Option<&str>,
         depends_on: &crate::parser::UnexpandedDependencies<B>,
         value_ref_deps: Option<&HashSet<B>>| {
            let self_name = binding.unwrap_or("(anonymous)");
            let context = owner_kind.context();
            let mut seen = HashSet::new();
            let mut duplicate_warned = HashSet::new();
            for operand in depends_on {
                let dep_name = operand.diagnostic_reference();
                if !seen.insert(dep_name.clone()) {
                    if duplicate_warned.insert(dep_name.clone()) {
                        diags.push(
                            DependsOnDiagnostic::warning(format!(
                                "{context} on '{self_name}': binding '{dep_name}' is listed multiple times"
                            ))
                            .at_element(owner_kind, self_name, &dep_name),
                        );
                    }
                    continue;
                }

                if owner_kind == DependsOnOwner::Resource && dep_name == self_name {
                    diags.push(
                        DependsOnDiagnostic::error(format!(
                            "{context} on '{self_name}': self-reference is not allowed \
                             (binding '{dep_name}' depends on itself)"
                        ))
                        .at_element(owner_kind, self_name, &dep_name),
                    );
                    continue;
                }

                let diagnostic = match classify_operand(
                    parsed,
                    &bindings_by_name,
                    argument_types,
                    &managed_group_names,
                    operand,
                    &dep_name,
                ) {
                    DependencyTarget::Upstream => Some(DependsOnDiagnostic::error(format!(
                        "{context} on '{self_name}': upstream_state binding '{dep_name}' \
                         is not a valid depends_on target"
                    ))),
                    DependencyTarget::Unknown => Some(DependsOnDiagnostic::error(format!(
                        "{context} on '{self_name}': binding '{dep_name}' is not declared in this scope"
                    ))),
                    DependencyTarget::ModuleParam(DependencyArgumentResolution::Resolved(
                        ResolvedDependencyArgumentType::Value(parameter_type),
                    )) => Some(DependsOnDiagnostic::error(format!(
                        "{context}: expected a managed resource handle; got {parameter_type}"
                    ))),
                    DependencyTarget::DataSource if owner_kind == DependsOnOwner::Resource => {
                        Some(DependsOnDiagnostic::error(format!(
                            "{context} on '{self_name}': data sources cannot be \
                             depends_on targets (binding '{dep_name}')"
                        )))
                    }
                    DependencyTarget::Managed | DependencyTarget::DataSource => value_ref_deps
                        .filter(|dependencies| dependencies.contains(&operand.binding_reference()))
                        .map(|_| {
                            DependsOnDiagnostic::warning(format!(
                                "{context} on '{self_name}': edge to '{dep_name}' is redundant; \
                                 '{dep_name}' is already referenced by value"
                            ))
                        }),
                    DependencyTarget::ModuleParam(
                        DependencyArgumentResolution::Unresolved
                        | DependencyArgumentResolution::Resolved(
                            ResolvedDependencyArgumentType::ManagedHandle,
                        ),
                    )
                    | DependencyTarget::LoopGroup
                    | DependencyTarget::Handle => None,
                };
                if let Some(diagnostic) = diagnostic {
                    diags.push(diagnostic.at_element(owner_kind, self_name, &dep_name));
                }
            }
        };

    for resource in parsed.iter_all_resources() {
        let Some(depends_on) =
            depends_on_of(resource).filter(|dependencies| !dependencies.is_empty())
        else {
            continue;
        };
        let mut value_ref_deps = HashSet::new();
        for value in resource.attributes().values() {
            collect_dependencies(value, &mut value_ref_deps);
        }
        value_ref_deps.extend(resource.dependency_bindings().iter().cloned());
        check_dependencies(
            DependsOnOwner::Resource,
            resource.binding(),
            depends_on,
            Some(&value_ref_deps),
        );
    }

    // The parser rejects wait/self handles and non-handle operand kinds.
    // Data sources are valid wait targets; redundant-edge linting is resource-only.
    for wait in &parsed.wait_bindings {
        check_dependencies(
            DependsOnOwner::Wait,
            Some(wait.binding.as_str()),
            &wait.depends_on,
            None,
        );
    }

    // Check the combined graph of value references and explicit dependencies
    // only when at least one resource declares depends_on. A reported cycle
    // may consist entirely of value references, so do not attribute it to
    // depends_on.
    //
    // carina#3181: only managed resources can participate in a cycle —
    // data sources are rejected as `depends_on` targets above (so they
    // can be sources but never the depended-on end of an edge), and
    // composition resources are post-apply synthetic nodes with no
    // directives. A cycle therefore lives entirely within the managed
    // slice.
    if !resources_have_no_depends_on
        && let Err(msg) = sort_resources_by_dependencies(&parsed.resources)
    {
        diags.push(DependsOnDiagnostic::error(msg));
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse, parse_and_resolve};

    fn check<
        E: crate::parser::ExportParamLike + Clone,
        D: crate::parser::DeferredForPhaseTraversal + Clone,
    >(
        parsed: &File<E, crate::resource::ParserPlaceholder, crate::resource::ParserPlaceholder, D>,
    ) -> Vec<DependsOnDiagnostic> {
        let mut parsed = parsed.clone();
        let report = crate::validation::deferred_types::with_session_for_test(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| super::super::resolve_file_type_exprs_report(&mut parsed, session, None),
        );
        assert!(
            report.diagnostics().is_empty(),
            "{:?}",
            report.diagnostics()
        );
        validate_depends_on(&parsed, report.resolved_argument_types())
    }

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
        let diags = check(&parsed);
        let errors = diag_messages(&diags, Severity::Error);
        assert!(
            errors
                .iter()
                .any(|m| m.contains("non_existent") && m.contains("not declared")),
            "expected 'not declared' error mentioning non_existent, got {:?}",
            errors
        );
    }

    fn check_wait_depends_on(dependency: &str) -> Vec<DependsOnDiagnostic> {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("resources.crn"),
            r#"let cert = aws.acm.Certificate { domain_name = "example.com" }
let dependency = aws.acm.Certificate { domain_name = "dependency.example.com" }
let lookup = read aws.acm.Certificate { domain_name = "lookup.example.com" }
"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("wait.crn"),
            format!(
                "let ready = wait cert {{\n    until = cert.status == \"ISSUED\"\n    depends_on = [{dependency}]\n}}\n"
            ),
        )
        .unwrap();
        let parsed = crate::config_loader::parse_directory(
            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("multi-file wait fixture must parse");
        check(parsed.parsed())
    }

    #[test]
    fn wait_unknown_depends_on_is_diagnosed_as_error() {
        let diagnostics = check_wait_depends_on("nope");
        assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:?}");
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.severity, Severity::Error);
        assert!(
            diagnostic
                .message
                .contains("binding 'nope' is not declared in this scope")
        );
        assert_eq!(
            diagnostic.location,
            Some(DependsOnLocation {
                owner: DependsOnOwner::Wait,
                binding: "ready".to_string(),
                dep: Some("nope".to_string()),
            }),
        );
    }

    #[test]
    fn wait_known_depends_on_in_sibling_file_is_accepted() {
        for dependency in ["dependency", "lookup"] {
            let diagnostics = check_wait_depends_on(dependency);
            assert!(diagnostics.is_empty(), "{dependency}: {diagnostics:?}");
        }
    }

    fn forward_wait_dependency(dependency: &str, declaration: &str) -> crate::parser::ParsedFile {
        // A single-file bootstrap parse retains forward names as bare operands.
        // The analysis pass must classify them from the completed declarations.
        parse(
            &format!(
                "let cert = aws.acm.Certificate {{ domain_name = \"example.com\" }}\n\
                 let ready = wait cert {{ until = cert.status == \"ISSUED\" depends_on = [{dependency}] }}\n\
                 {declaration}\n"
            ),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("forward dependency fixture must parse")
    }

    #[test]
    fn wait_depends_on_rejects_forward_upstream_binding() {
        let mut parsed = forward_wait_dependency(
            "upstream",
            r#"let upstream = upstream_state { source = "../upstream" }"#,
        );
        let wait_errors = diag_messages(&check(&parsed), Severity::Error);
        // Compare the same retained operand on the resource path.
        parsed.resources[0].dependencies.depends_on =
            std::mem::take(&mut parsed.wait_bindings[0].depends_on);
        let resource_errors = diag_messages(&check(&parsed), Severity::Error);
        assert_eq!(resource_errors.len(), 1, "{resource_errors:?}");
        assert_eq!(
            wait_errors,
            vec![
                resource_errors[0]
                    .replace("directives.depends_on", "wait.depends_on")
                    .replace("on 'cert'", "on 'ready'")
            ],
        );
    }

    #[test]
    fn wait_depends_on_rejects_non_dependency_declarations() {
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        for (dependency, declaration) in [
            ("helper", "fn helper() { 1 }"),
            ("secondary", "let secondary = provider aws {}"),
            ("module", "let module = use { source = './module' }"),
            (
                "other",
                "let other = wait cert { until = cert.status == 'ISSUED' }",
            ),
        ] {
            let parsed = forward_wait_dependency(dependency, declaration);
            let errors = diag_messages(&check(&parsed), Severity::Error);
            actual.push((dependency, errors));
            expected.push((dependency, vec![format!(
                "wait.depends_on on 'ready': binding '{dependency}' is not declared in this scope"
            )]));
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn wait_depends_on_rejects_resolved_value_argument_like_resources() {
        let mut parsed = parse(
            "arguments { rec: aws.iam.Policy.Arn }\n\
             let cert = aws.acm.Certificate { directives { depends_on = [rec] } }\n\
             let ready = wait cert { until = cert.status == 'ISSUED' depends_on = [rec] }\n",
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let context = ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                    "aws",
                    crate::schema::TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
                    crate::schema::RefinedScalarKind::String,
                )]),
                Box::new(|_target, _value| Ok(())),
            ),
            ..ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };
        let report =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                super::super::resolve_file_type_exprs_report(&mut parsed, session, None)
            });
        assert!(
            report.diagnostics().is_empty(),
            "{:?}",
            report.diagnostics()
        );
        let errors = diag_messages(
            &validate_depends_on(&parsed, report.resolved_argument_types()),
            Severity::Error,
        );
        assert_eq!(
            errors,
            vec![
                "directives.depends_on: expected a managed resource handle; got aws.iam.Policy.Arn",
                "wait.depends_on: expected a managed resource handle; got aws.iam.Policy.Arn",
            ],
        );
    }

    #[test]
    fn wait_depends_on_warns_once_for_duplicates_without_redundant_edges() {
        let parsed = forward_wait_dependency("cert, cert, cert", "");
        let diagnostics = check(&parsed);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].severity, Severity::Warning);
        assert_eq!(
            diagnostics[0].message,
            "wait.depends_on on 'ready': binding 'cert' is listed multiple times",
        );
    }

    #[test]
    fn wait_depends_on_still_rejects_upstream_state_and_wait_handles_in_parser() {
        for (dependency, kind) in [
            ("upstream", "an upstream state"),
            ("other", "a wait binding"),
            ("ready", "a wait binding"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("resources.crn"),
                r#"let cert = aws.acm.Certificate { domain_name = "example.com" }
let upstream = upstream_state { source = "../upstream" }
let other = wait cert { until = cert.status == "ISSUED" }
"#,
            )
            .unwrap();
            std::fs::write(
                directory.path().join("wait.crn"),
                format!(
                    "let ready = wait cert {{ until = cert.status == \"ISSUED\" depends_on = [{dependency}] }}\n"
                ),
            )
            .unwrap();
            let error = crate::config_loader::parse_directory(
                &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .expect_err("upstream_state and wait handles must remain parser errors");
            assert!(
                error.contains(&format!(
                    "wait.depends_on: expected a managed resource handle; got {kind}"
                )),
                "{dependency}: {error}",
            );
        }
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("fixture must parse");
        assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);

        let errors = diag_messages(&check(&parsed), Severity::Error);
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
        let diags = check(&parsed);
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
        let diags = check(&parsed);
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
        let diags = check(&parsed);
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
    fn managed_for_group_is_a_valid_explicit_dependency() {
        let src = r#"
            let records = for name in ["one", "two"] {
                aws.s3.Bucket { bucket_name = name }
            }
            let consumer = aws.s3.Bucket {
                bucket_name = "consumer"
                directives { depends_on = [records] }
            }
        "#;
        let parsed = parse_and_resolve(src).unwrap();
        let errors = diag_messages(&check(&parsed), Severity::Error);
        assert!(
            errors.is_empty(),
            "expanded managed groups are valid dependency targets: {errors:?}",
        );
    }

    #[test]
    fn expanded_loop_group_takes_precedence_over_value_argument_in_depends_on() {
        let mut parsed = parse_and_resolve(
            r#"
                arguments { records: String }
                let member = aws.s3.Bucket { bucket_name = "one" }
                let consumer = aws.s3.Bucket { bucket_name = "consumer" }
            "#,
        )
        .unwrap();
        // Model an expanded group without a retained parser loop handle.
        parsed.resources[0].binding = Some("records[0]".into());
        parsed.resources[1].dependencies.depends_on = vec!["records".into()].into();

        let diagnostics = check(&parsed);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
        let diags = check(&parsed);
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
        let diags = check(&parsed);
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
        let diags = check(&parsed);
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
        let diags = check(&parsed);
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
        let parsed = parse(
            src,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let diags = check(&parsed);
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
        let parsed = crate::parser::parse(
            src,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let diags = check(&parsed);
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
        let diags = check(&parsed);
        assert!(
            diags.is_empty(),
            "expected zero diagnostics for valid depends_on, got {:?}",
            diags
        );
    }
}
