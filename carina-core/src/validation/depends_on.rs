//! Analysis of dependency operand occurrences and typed dependency cycles.
//! All findings have parser-owned locations, including one outgoing edge per
//! cycle member. Directory/whole-file failures do not belong to this family.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{DependencyArgumentResolution, ResolvedArgumentTypes, ResolvedDependencyArgumentType};
use crate::deps::collect_dependencies;
use crate::parser::{
    AnalysisItemKind, AnalysisResourceRef, AnalysisTree, DeclarationLocation, Located,
    UnexpandedDependencyOperand,
};
use crate::resource::{BindingAddress, ParserPlaceholder};

type ResourceRef<'a, D, B> = AnalysisResourceRef<'a, ParserPlaceholder, ParserPlaceholder, D, B>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}
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

/// Which authored outgoing edge participates in a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyEdgeOrigin {
    ExplicitDependency,
    ValueReference,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycleMember {
    pub binding: String,
    pub target: String,
    pub origin: DependencyEdgeOrigin,
    pub location: DeclarationLocation,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycle {
    pub members: Vec<DependencyCycleMember>,
}
impl std::fmt::Display for DependencyCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Circular dependency detected: ")?;
        let ring = self
            .members
            .windows(2)
            .all(|pair| pair[0].target == pair[1].binding)
            && self
                .members
                .first()
                .zip(self.members.last())
                .is_some_and(|(first, last)| last.target == first.binding);
        if ring {
            for member in &self.members {
                write!(f, "{} -> ", member.binding)?;
            }
            if let Some(first) = self.members.first() {
                f.write_str(&first.binding)?;
            }
        } else {
            for (index, member) in self.members.iter().enumerate() {
                if index > 0 {
                    f.write_str("; ")?;
                }
                write!(f, "{} -> {}", member.binding, member.target)?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependsOnDiagnosticKind {
    Element {
        owner: DependsOnOwner,
        binding: String,
        dependency: String,
    },
    Cycle {
        cycle: Arc<DependencyCycle>,
        member: usize,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependsOnDiagnostic {
    pub severity: Severity,
    pub message: String,
    pub kind: DependsOnDiagnosticKind,
}
impl std::fmt::Display for DependsOnDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
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
fn classify_operand<'a, B: BindingAddress, D>(
    tree: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    bindings: &HashMap<&str, &ResourceRef<'_, D, B>>,
    argument_types: &'a ResolvedArgumentTypes,
    operand: &UnexpandedDependencyOperand<B>,
    name: &str,
) -> DependencyTarget<'a>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    if tree.is_upstream(name) {
        return DependencyTarget::Upstream;
    }
    if let Some(target) = bindings.get(name) {
        return if target.kind() == AnalysisItemKind::DataSource {
            DependencyTarget::DataSource
        } else {
            DependencyTarget::Managed
        };
    }
    if matches!(operand, UnexpandedDependencyOperand::BareBinding(_)) && tree.is_loop_group(name) {
        return DependencyTarget::LoopGroup;
    }
    if matches!(operand, UnexpandedDependencyOperand::Handle(_)) {
        return DependencyTarget::Handle;
    }
    if let Some(parameter) = argument_types.types.get(name) {
        return DependencyTarget::ModuleParam(parameter);
    }
    DependencyTarget::Unknown
}

/// A raw file cannot enter analysis or omit the argument-type witness.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, validation::{depends_on::validate_depends_on, ResolvedArgumentTypes}};
/// fn analyze(file: &ParsedFile, types: &ResolvedArgumentTypes) { validate_depends_on(file, types); }
/// ```
pub fn validate_depends_on<B: BindingAddress, D>(
    tree: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    argument_types: &ResolvedArgumentTypes,
) -> Vec<Located<DependsOnDiagnostic>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let resources_have_dependencies = tree
        .resources()
        .any(|resource| !resource.dependencies().is_empty());
    if !resources_have_dependencies && tree.waits().all(|wait| wait.dependencies().is_empty()) {
        return Vec::new();
    }
    let bindings: HashMap<_, _> = tree
        .resources()
        .filter_map(|r| r.binding().map(|binding| (binding, r)))
        .collect();
    let mut out = Vec::new();
    let mut check = |owner: DependsOnOwner,
                     binding: Option<&str>,
                     dependencies: &[crate::parser::AnalysisDependencyRef<'_, B>],
                     value_deps: &HashSet<B>| {
        let name = binding.unwrap_or("(anonymous)");
        let context = owner.context();
        let mut seen = HashSet::new();
        let mut warned = HashSet::new();
        for dependency in dependencies {
            let operand = dependency.operand();
            let dep = operand.diagnostic_reference();
            let mut emit = |severity, message| {
                out.push(Located::new(
                    dependency.location().clone(),
                    DependsOnDiagnostic {
                        severity,
                        message,
                        kind: DependsOnDiagnosticKind::Element {
                            owner,
                            binding: name.to_string(),
                            dependency: dep.clone(),
                        },
                    },
                ))
            };
            if !seen.insert(dep.clone()) {
                if warned.insert(dep.clone()) {
                    emit(
                        Severity::Warning,
                        format!("{context} on '{name}': binding '{dep}' is listed multiple times"),
                    );
                }
                continue;
            }
            if owner == DependsOnOwner::Resource && dep == name {
                emit(
                    Severity::Error,
                    format!(
                        "{context} on '{name}': self-reference is not allowed (binding '{dep}' depends on itself)"
                    ),
                );
                continue;
            }
            let diagnostic = match classify_operand(tree, &bindings, argument_types, operand, &dep)
            {
                DependencyTarget::Upstream => Some((
                    Severity::Error,
                    format!(
                        "{context} on '{name}': upstream_state binding '{dep}' is not a valid depends_on target"
                    ),
                )),
                DependencyTarget::Unknown => Some((
                    Severity::Error,
                    format!("{context} on '{name}': binding '{dep}' is not declared in this scope"),
                )),
                DependencyTarget::ModuleParam(DependencyArgumentResolution::Resolved(
                    ResolvedDependencyArgumentType::Value(ty),
                )) => Some((
                    Severity::Error,
                    format!("{context}: expected a managed resource handle; got {ty}"),
                )),
                DependencyTarget::DataSource if owner == DependsOnOwner::Resource => Some((
                    Severity::Error,
                    format!(
                        "{context} on '{name}': data sources cannot be depends_on targets (binding '{dep}')"
                    ),
                )),
                DependencyTarget::Managed | DependencyTarget::DataSource
                    if owner == DependsOnOwner::Resource
                        && value_deps.contains(&operand.binding_reference()) =>
                {
                    Some((
                        Severity::Warning,
                        format!(
                            "{context} on '{name}': edge to '{dep}' is redundant; '{dep}' is already referenced by value"
                        ),
                    ))
                }
                _ => None,
            };
            if let Some((severity, message)) = diagnostic {
                emit(severity, message);
            }
        }
    };
    for resource in tree.resources() {
        let mut value_deps = HashSet::new();
        for value in resource.attributes().values() {
            collect_dependencies(value, &mut value_deps);
        }
        value_deps.extend(resource.dependency_bindings().iter().cloned());
        check(
            DependsOnOwner::Resource,
            resource.binding(),
            resource.dependencies(),
            &value_deps,
        );
    }
    for wait in tree.waits() {
        check(
            DependsOnOwner::Wait,
            Some(wait.binding().as_str()),
            wait.dependencies(),
            &HashSet::new(),
        );
    }
    if resources_have_dependencies {
        for cycle in dependency_cycles(tree) {
            let cycle = Arc::new(cycle);
            for (member, edge) in cycle.members.iter().enumerate() {
                out.push(Located::new(
                    edge.location.clone(),
                    DependsOnDiagnostic {
                        severity: Severity::Error,
                        message: cycle.to_string(),
                        kind: DependsOnDiagnosticKind::Cycle {
                            cycle: Arc::clone(&cycle),
                            member,
                        },
                    },
                ));
            }
        }
    }
    out
}

/// Find cycles using authored edge origins rather than rendered cycle strings.
/// Edges and traversal follow declaration/occurrence order for deterministic anchors.
fn dependency_cycles<B: BindingAddress, D>(
    tree: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> Vec<DependencyCycle>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let resources: Vec<_> = tree
        .resources()
        .filter(|resource| {
            matches!(
                resource.kind(),
                AnalysisItemKind::Resource | AnalysisItemKind::DeferredResource
            )
        })
        .collect();
    let bindings: HashMap<_, _> = resources
        .iter()
        .enumerate()
        .filter_map(|(index, r)| r.binding_address().map(|binding| (binding.clone(), index)))
        .collect();
    let mut edges = vec![Vec::<(usize, DependencyCycleMember)>::new(); resources.len()];
    for (index, resource) in resources.iter().enumerate() {
        let name = resource
            .binding()
            .map(str::to_string)
            .unwrap_or_else(|| resource.id().to_string());
        for dependency in resource.dependencies() {
            if let Some(&target) = bindings.get(&dependency.operand().binding_reference()) {
                edges[index].push((
                    target,
                    DependencyCycleMember {
                        binding: name.clone(),
                        target: resources[target]
                            .binding()
                            .expect("indexed named resource")
                            .to_string(),
                        origin: DependencyEdgeOrigin::ExplicitDependency,
                        location: dependency.location().clone(),
                    },
                ));
            }
        }
        for reference in resource.references() {
            if let Some(&target) = bindings.get(reference.path().binding_address()) {
                edges[index].push((
                    target,
                    DependencyCycleMember {
                        binding: name.clone(),
                        target: resources[target]
                            .binding()
                            .expect("indexed named resource")
                            .to_string(),
                        origin: DependencyEdgeOrigin::ValueReference,
                        location: reference.location().clone(),
                    },
                ));
            }
        }
    }
    // Strongly connected components retain all participants in overlapping
    // cycles. A back-edge-only DFS can omit members reached after a sibling
    // branch has already finished, or emit the same member more than once.
    fn finish(
        index: usize,
        edges: &[Vec<(usize, DependencyCycleMember)>],
        visited: &mut [bool],
        order: &mut Vec<usize>,
    ) {
        if visited[index] {
            return;
        }
        visited[index] = true;
        for (target, _) in &edges[index] {
            finish(*target, edges, visited, order);
        }
        order.push(index);
    }
    fn collect(
        index: usize,
        reverse: &[Vec<usize>],
        visited: &mut [bool],
        members: &mut Vec<usize>,
    ) {
        if visited[index] {
            return;
        }
        visited[index] = true;
        members.push(index);
        for source in &reverse[index] {
            collect(*source, reverse, visited, members);
        }
    }
    let mut reverse = vec![Vec::new(); resources.len()];
    for (source, outgoing) in edges.iter().enumerate() {
        for (target, _) in outgoing {
            reverse[*target].push(source);
        }
    }
    let mut visited = vec![false; resources.len()];
    let mut order = Vec::new();
    for index in 0..resources.len() {
        finish(index, &edges, &mut visited, &mut order);
    }
    visited.fill(false);
    let mut cycles = Vec::new();
    for index in order.into_iter().rev() {
        if visited[index] {
            continue;
        }
        let mut component = Vec::new();
        collect(index, &reverse, &mut visited, &mut component);
        if component.len() == 1 && !edges[index].iter().any(|(target, _)| *target == index) {
            continue;
        }
        component.sort_unstable();
        let membership: HashSet<_> = component.iter().copied().collect();
        let members = component
            .iter()
            .map(|index| {
                edges[*index]
                    .iter()
                    .find(|(target, _)| membership.contains(target))
                    .expect("cyclic component has an internal outgoing edge")
                    .1
                    .clone()
            })
            .collect();
        cycles.push(DependencyCycle { members });
    }
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse, parse_and_resolve};

    fn check<
        E: crate::parser::ExportParamLike + Clone,
        D: crate::parser::DeferredForPhaseTraversal + Clone,
    >(
        parsed: &crate::parser::File<
            E,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            D,
        >,
    ) -> Vec<Located<DependsOnDiagnostic>> {
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
        validate_depends_on(
            &AnalysisTree::try_from_file(&parsed).expect("parsed analysis provenance"),
            report.resolved_argument_types(),
        )
    }

    fn diag_messages(diags: &[Located<DependsOnDiagnostic>], sev: Severity) -> Vec<String> {
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

    fn check_wait_depends_on(dependency: &str) -> Vec<Located<DependsOnDiagnostic>> {
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
            diagnostic.kind,
            DependsOnDiagnosticKind::Element {
                owner: DependsOnOwner::Wait,
                binding: "ready".to_string(),
                dependency: "nope".to_string(),
            },
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
            &validate_depends_on(
                &AnalysisTree::try_from_file(&parsed).expect("parsed analysis provenance"),
                report.resolved_argument_types(),
            ),
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
                let consumer = aws.s3.Bucket { bucket_name = "consumer" directives { depends_on = [missing] } }
            "#,
        )
        .unwrap();
        // Model an expanded group without a retained parser loop handle.
        parsed.resources[0].binding = Some("records[0]".into());
        *parsed.resources[1]
            .dependencies
            .depends_on
            .iter_mut()
            .next()
            .expect("authored operand") =
            UnexpandedDependencyOperand::BareBinding("records".into());

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
    fn overlapping_cycles_report_every_member_once_with_typed_outgoing_origins() {
        let parsed = parse_and_resolve("let a = mock.test.Resource { value = c.output directives { depends_on = [b] } }\nlet b = mock.test.Resource { directives { depends_on = [a] } }\nlet c = mock.test.Resource { directives { depends_on = [b] } }\n").unwrap();
        let diagnostics = check(&parsed);
        let members: Vec<_> = diagnostics
            .iter()
            .filter_map(|diagnostic| match &diagnostic.kind {
                DependsOnDiagnosticKind::Cycle { cycle, member } => Some(&cycle.members[*member]),
                _ => None,
            })
            .collect();
        assert_eq!(members.len(), 3, "{diagnostics:#?}");
        assert_eq!(
            members
                .iter()
                .map(|member| member.binding.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["a", "b", "c"])
        );
        assert!(
            members
                .iter()
                .all(|member| member.origin == DependencyEdgeOrigin::ExplicitDependency)
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
