//! `ModuleResolver` driver: import processing, nested-module resolution,
//! and the top-level `resolve_modules*` entry points.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::parser::{
    DeferredIfBody, File, LoadedProviderContext, ModuleCallDiagnosticSource, ModuleResolvedFile,
    ModuleResolvedParsedFile, ParseError, ParsedFile, ProviderContext, StateBlock, UseStatement,
    ValidateExpr,
};
use crate::resource::{ModuleConstraintId, Value};
use crate::schema::SchemaRegistry;

use super::error::{ModuleError, ModuleErrorCallSource};
use super::expander::instance_prefix_for_call;
use super::loader::sorted_crn_paths_in;
use super::validation::{ConstraintOrigin, ModuleConstraintDiagnostic};

/// Resolver-owned result of evaluating one authored module constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModuleConstraintOutcome {
    pub id: ModuleConstraintId,
    pub status: ResolvedModuleConstraintStatus,
}

/// Value-time state observed by the resolver after its fixed-point argument
/// substitution has completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedModuleConstraintStatus {
    Satisfied,
    Pending,
    Failed {
        diagnostic: Box<ModuleConstraintDiagnostic>,
        origin: ResolvedModuleConstraintOrigin,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedModuleConstraintOrigin {
    Argument(crate::parser::ArgumentOrigin),
    Call,
}

/// Constraint outcomes and authored location for one expanded module call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallConstraintReport {
    pub source: ModuleCallDiagnosticSource,
    pub instance: String,
    pub outcomes: Vec<ResolvedModuleConstraintOutcome>,
}

impl ModuleCallConstraintReport {
    pub(crate) fn new(
        source: ModuleCallDiagnosticSource,
        instance: String,
        outcomes: Vec<ResolvedModuleConstraintOutcome>,
    ) -> Self {
        Self {
            source,
            instance,
            outcomes,
        }
    }
}

pub type ModuleCallConstraintReports = Vec<ModuleCallConstraintReport>;

/// Diagnostic module-resolution outcome.
///
/// The expanded graph is owned exclusively by [`Self::Expanded`]. Constraint
/// and residual failures retain their typed diagnostics but cannot expose the
/// scratch graph that produced them.
pub enum ModuleResolutionDiagnosticResult<E> {
    Expanded {
        graph: Box<ModuleResolvedFile<E>>,
    },
    ConstraintFailed {
        findings: Vec<crate::validation::ModuleCallFinding>,
        errors: Vec<ResidualModuleResolutionError>,
    },
    Unowned {
        errors: Vec<ResidualModuleResolutionError>,
    },
}

/// Resolver failure left after typed source diagnostics have claimed every
/// call-boundary and constraint error they own.
pub struct ResidualModuleResolutionError(Box<ModuleError>);

/// Source-routing shape of a residual resolver error. Core classifies every
/// [`ModuleError`] variant; editor adapters only map the resulting location.
pub enum ResidualModuleResolutionDiagnostic<'a> {
    Constraint {
        origin: &'a ConstraintOrigin,
    },
    ArgumentValueAccess {
        source_path: &'a str,
        line: usize,
    },
    ModuleCall,
    Parse {
        source_path: Option<&'a Path>,
        error: &'a ParseError,
    },
}

impl ResidualModuleResolutionError {
    fn new(
        error: ModuleError,
        findings: &[crate::validation::ModuleCallFinding],
        config: &ProviderContext,
    ) -> Option<Self> {
        let error = without_covered_reference_type_members(error, findings)?;
        let residual = Self(Box::new(error));
        (!residual.is_covered_by(findings, config)).then_some(residual)
    }

    pub fn call_source(&self) -> ModuleErrorCallSource<'_> {
        self.0.call_source()
    }

    pub fn display_relative_to(&self, root: &Path) -> String {
        self.0.display_relative_to(root)
    }

    pub fn is_constraint(&self) -> bool {
        matches!(self.0.root_cause(), ModuleError::Constraint(_diagnostic))
    }

    pub fn diagnostic(&self) -> Option<ResidualModuleResolutionDiagnostic<'_>> {
        match self.0.root_cause() {
            ModuleError::Multiple(_errors) => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::Constraint(diagnostic) => {
                Some(ResidualModuleResolutionDiagnostic::Constraint {
                    origin: &diagnostic.origin,
                })
            }
            ModuleError::ArgumentValueAccess {
                module: _module,
                source_path,
                line,
                message: _message,
            } => Some(ResidualModuleResolutionDiagnostic::ArgumentValueAccess {
                source_path,
                line: *line,
            }),
            ModuleError::DroppedConditionalBinding {
                binding: _binding,
                condition: _condition,
                source_path,
                line,
            } => Some(ResidualModuleResolutionDiagnostic::ArgumentValueAccess {
                source_path: source_path.to_str().unwrap_or("<module>"),
                line: *line,
            }),
            ModuleError::UnexpandedModuleTemplate { context: _context } => None,
            ModuleError::Parse(error) => match error {
                ParseError::Located { path, error } => {
                    Some(ResidualModuleResolutionDiagnostic::Parse {
                        source_path: Some(path),
                        error,
                    })
                }
                error => Some(ResidualModuleResolutionDiagnostic::Parse {
                    source_path: None,
                    error,
                }),
            },
            ModuleError::ParseInFile { path, error } => {
                Some(ResidualModuleResolutionDiagnostic::Parse {
                    source_path: Some(path),
                    error,
                })
            }
            ModuleError::TypeResolution(_diagnostic) => {
                Some(ResidualModuleResolutionDiagnostic::ModuleCall)
            }
            ModuleError::NotFound(_module) => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::CircularImport(_module) => None,
            ModuleError::Io(_error) => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::UnknownModule(_module) => {
                Some(ResidualModuleResolutionDiagnostic::ModuleCall)
            }
            ModuleError::ProviderInModule
            | ModuleError::StateBlockInModule
            | ModuleError::BackendInModule
            | ModuleError::UpstreamStateInModule
            | ModuleError::ExportsInModule => None,
            ModuleError::NotADirectory { path: _path } => {
                Some(ResidualModuleResolutionDiagnostic::ModuleCall)
            }
            ModuleError::OwnedByCalls {
                calls: _calls,
                error: _error,
            } => None,
            ModuleError::AtCall {
                call: _call,
                error: _error,
            } => None,
            ModuleError::MissingArgument {
                module: _module,
                argument: _argument,
            } => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::InvalidArgumentType {
                module: _module,
                argument: _argument,
                mismatch: _mismatch,
            } => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::BareStructuralHandleArgument {
                module: _module,
                argument: _argument,
                source_path: _source_path,
                line: _line,
                message: _message,
            } => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::ReferenceType(errors) => errors
                .iter()
                .any(|error| match error {
                    crate::validation::ExpansionBoundaryReferenceError::ModuleCall(_error) => true,
                    crate::validation::ExpansionBoundaryReferenceError::Attribute(_message)
                    | crate::validation::ExpansionBoundaryReferenceError::Export(_message)
                    | crate::validation::ExpansionBoundaryReferenceError::Function(_message) => {
                        false
                    }
                })
                .then_some(ResidualModuleResolutionDiagnostic::ModuleCall),
            ModuleError::UnknownArgument {
                module: _module,
                argument: _argument,
            } => Some(ResidualModuleResolutionDiagnostic::ModuleCall),
        }
    }

    fn is_covered_by(
        &self,
        findings: &[crate::validation::ModuleCallFinding],
        config: &ProviderContext,
    ) -> bool {
        let root_cause = self.0.root_cause();
        match root_cause {
            ModuleError::Multiple(_errors) => false,
            ModuleError::MissingArgument { module, argument } => findings.iter().any(|finding| {
                self.finding_owns_error(finding)
                    && finding.message().covers_missing_argument(module, argument)
            }),
            ModuleError::InvalidArgumentType {
                module: _,
                argument,
                mismatch,
            } => findings.iter().any(|finding| {
                self.finding_owns_error(finding)
                    && finding
                        .message()
                        .covers_invalid_argument_type(argument, mismatch)
            }),
            ModuleError::BareStructuralHandleArgument {
                module: _,
                argument,
                source_path: _,
                line: _,
                message: _,
            } => findings.iter().any(|finding| {
                self.finding_owns_error(finding)
                    && finding.message().covers_bare_handle_argument(argument)
            }),
            ModuleError::ReferenceType(_errors) => false,
            ModuleError::UnknownArgument { module, argument } => findings.iter().any(|finding| {
                self.finding_owns_error(finding)
                    && finding.message().covers_unknown_argument(module, argument)
            }),
            ModuleError::OwnedByCalls { calls: _, error: _ } => false,
            ModuleError::AtCall { call: _, error: _ } => false,
            ModuleError::NotFound(_) => false,
            ModuleError::CircularImport(_) => false,
            ModuleError::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                message: _,
            } => false,
            ModuleError::DroppedConditionalBinding { .. } => false,
            ModuleError::UnexpandedModuleTemplate { .. } => false,
            ModuleError::Io(_) => false,
            ModuleError::Parse(error) => match error {
                ParseError::Located { path, error } => {
                    crate::validation::type_resolution_parser_error_is_covered(
                        config,
                        findings
                            .iter()
                            .filter_map(|finding| finding.message().type_resolution_diagnostic()),
                        crate::validation::TypeResolutionParserError::new(path, error),
                    )
                }
                ParseError::Syntax(_)
                | ParseError::InvalidExpression {
                    line: _,
                    message: _,
                }
                | ParseError::IntegerArithmetic {
                    line: _,
                    column: _,
                    end_column: _,
                    message: _,
                }
                | ParseError::UnboundLoopRead {
                    kind: _,
                    line: _,
                    span: _,
                }
                | ParseError::DeferredForBodyUnsupported(_)
                | ParseError::DeferredForBodyTemplate {
                    line: _,
                    header: _,
                    span: _,
                    error: _,
                }
                | ParseError::UnresolvedDeclarationType {
                    token: _,
                    line: _,
                    message: _,
                }
                | ParseError::UnknownFunction {
                    name: _,
                    line: _,
                    column: _,
                }
                | ParseError::ReservedResourceAttribute {
                    attribute: _,
                    line: _,
                    column: _,
                    end_line: _,
                    end_column: _,
                }
                | ParseError::SecretDerivedBackendAttribute {
                    backend_type: _,
                    attribute: _,
                    line: _,
                    start_column: _,
                    end_column: _,
                }
                | ParseError::InterpolationNotAllowedInLiteralString { line: _ }
                | ParseError::InvalidResourceIdentity { line: _, error: _ }
                | ParseError::UndefinedVariable(_)
                | ParseError::InvalidResourceType(_)
                | ParseError::DuplicateModule(_)
                | ParseError::DuplicateBinding { name: _, line: _ }
                | ParseError::UndefinedIdentifier {
                    name: _,
                    line: _,
                    span: _,
                    suggestion: _,
                    in_scope: _,
                }
                | ParseError::UndefinedDeferredIterable {
                    name: _,
                    line: _,
                    span: _,
                    suggestion: _,
                    in_scope: _,
                }
                | ParseError::ModuleNotFound(_)
                | ParseError::InternalError {
                    expected: _,
                    context: _,
                }
                | ParseError::RecursiveFunction(_)
                | ParseError::CyclicLetBinding { chain: _ }
                | ParseError::UserFunctionError(_) => false,
            },
            ModuleError::ParseInFile { path, error } => {
                crate::validation::type_resolution_parser_error_is_covered(
                    config,
                    findings
                        .iter()
                        .filter_map(|finding| finding.message().type_resolution_diagnostic()),
                    crate::validation::TypeResolutionParserError::new(path, error),
                )
            }
            ModuleError::TypeResolution(diagnostic) => findings.iter().any(|finding| {
                finding.message().type_resolution_diagnostic() == Some(diagnostic.as_ref())
            }),
            ModuleError::UnknownModule(_) => false,
            ModuleError::ProviderInModule => false,
            ModuleError::StateBlockInModule => false,
            ModuleError::BackendInModule => false,
            ModuleError::UpstreamStateInModule => false,
            ModuleError::ExportsInModule => false,
            ModuleError::Constraint(_) => false,
            ModuleError::NotADirectory { path: _ } => false,
        }
    }

    fn finding_owns_error(&self, finding: &crate::validation::ModuleCallFinding) -> bool {
        if let Some(failing_call) = self.0.failing_call() {
            return finding
                .owners()
                .iter()
                .any(|owner| owner.immediate.module_call() == Some(failing_call));
        }
        let call_source = self.0.call_source();
        finding.owners().iter().any(|owner| match call_source {
            ModuleErrorCallSource::Failing(call) => owner.immediate.module_call() == Some(call),
            ModuleErrorCallSource::Owners(calls) => owner
                .root
                .module_call()
                .is_some_and(|root| calls.contains(root)),
            ModuleErrorCallSource::Unavailable => false,
        })
    }
}

fn without_covered_reference_type_members(
    error: ModuleError,
    findings: &[crate::validation::ModuleCallFinding],
) -> Option<ModuleError> {
    match error {
        ModuleError::Multiple(errors) => ModuleError::from_errors(
            errors
                .into_vec()
                .into_iter()
                .filter_map(|error| without_covered_reference_type_members(error, findings))
                .collect(),
        ),
        ModuleError::OwnedByCalls { calls, error } => {
            without_covered_reference_type_members(*error, findings).map(|error| {
                ModuleError::OwnedByCalls {
                    calls,
                    error: Box::new(error),
                }
            })
        }
        ModuleError::AtCall { call, error } => {
            without_covered_reference_type_members(*error, findings).map(|error| {
                ModuleError::AtCall {
                    call,
                    error: Box::new(error),
                }
            })
        }
        ModuleError::ReferenceType(errors) => errors
            .without_covered_module_calls(findings)
            .map(ModuleError::ReferenceType),
        error @ (ModuleError::NotFound(_)
        | ModuleError::CircularImport(_)
        | ModuleError::MissingArgument {
            module: _,
            argument: _,
        }
        | ModuleError::InvalidArgumentType {
            module: _,
            argument: _,
            mismatch: _,
        }
        | ModuleError::BareStructuralHandleArgument {
            module: _,
            argument: _,
            source_path: _,
            line: _,
            message: _,
        }
        | ModuleError::ArgumentValueAccess {
            module: _,
            source_path: _,
            line: _,
            message: _,
        }
        | ModuleError::DroppedConditionalBinding {
            binding: _,
            condition: _,
            source_path: _,
            line: _,
        }
        | ModuleError::UnexpandedModuleTemplate { context: _ }
        | ModuleError::Io(_)
        | ModuleError::Parse(_)
        | ModuleError::ParseInFile { path: _, error: _ }
        | ModuleError::TypeResolution(_)
        | ModuleError::UnknownArgument {
            module: _,
            argument: _,
        }
        | ModuleError::UnknownModule(_)
        | ModuleError::ProviderInModule
        | ModuleError::StateBlockInModule
        | ModuleError::BackendInModule
        | ModuleError::UpstreamStateInModule
        | ModuleError::ExportsInModule
        | ModuleError::Constraint(_)
        | ModuleError::NotADirectory { path: _ }) => Some(error),
    }
}

#[cfg(test)]
mod diagnostic_status_tests {
    use super::*;

    #[test]
    fn constraint_failure_does_not_publish_the_expanded_directory_graph() {
        let fixture = tempfile::tempdir().expect("constraint typestate fixture");
        let checked = fixture.path().join("checked");
        std::fs::create_dir_all(&checked).expect("create checked module");
        std::fs::write(
            fixture.path().join("imports.crn"),
            "let checked = use { source = './checked' }\n",
        )
        .expect("write root import");
        std::fs::write(
            fixture.path().join("calls.crn"),
            "let instance = checked {\n  name = \"too-long\"\n}\n",
        )
        .expect("write root call");
        std::fs::write(
            checked.join("arguments.crn"),
            r#"arguments {
  name: String {
    validation {
      condition = length(name) <= 3
      error_message = "name is too long"
    }
  }
}
"#,
        )
        .expect("write module arguments");
        std::fs::write(
            checked.join("attributes.crn"),
            "attributes {\n  name = name\n}\n",
        )
        .expect("write module attributes");

        let config = ProviderContext::default();
        let schemas = SchemaRegistry::new();
        let parsed = crate::config_loader::parse_directory(fixture.path(), &config)
            .expect("parse directory fixture");
        let result = resolve_modules_with_providerless_diagnostics(
            &parsed,
            fixture.path(),
            &config,
            &schemas,
            &[],
        );

        match result {
            ModuleResolutionDiagnosticResult::Expanded { graph } => panic!(
                "constraint failure unexpectedly published a graph with {} compositions",
                graph.compositions.len()
            ),
            ModuleResolutionDiagnosticResult::ConstraintFailed { findings, errors } => {
                assert_eq!(findings.len(), 1);
                assert!(
                    findings[0]
                        .message()
                        .to_string()
                        .contains("name is too long"),
                    "unexpected constraint finding: {}",
                    findings[0].message()
                );
                assert!(errors.is_empty());
            }
            ModuleResolutionDiagnosticResult::Unowned { errors } => {
                panic!("unexpected residual errors: {}", errors.len())
            }
        }
    }

    #[test]
    fn source_ownership_requires_a_covering_module_call_finding() {
        let parsed = crate::parser::parse_file(
            "let child = use { source = './child' }\nchild {}\n",
            std::path::Path::new("root.crn"),
            &ProviderContext::default(),
        )
        .expect("fixture parse");
        let signatures = HashMap::from([(
            "child".to_string(),
            crate::module_resolver::ResolvedModuleSignature {
                arguments: vec![crate::parser::ArgumentParameter {
                    name: "required".to_string(),
                    static_type: crate::validation::resolve_consumer_static_type_for_signature(
                        &crate::parser::TypeExpr::String,
                        &ProviderContext::default(),
                    ),
                    default: None,
                    description: None,
                    validations: Vec::new(),
                }],
                attributes: indexmap::IndexMap::new(),
                requires: Vec::new(),
            },
        )]);
        let schemas = SchemaRegistry::new();
        let provider_context = ProviderContext::default();
        let binding_use_report = crate::validation::check_bare_binding_uses(
            &parsed,
            &crate::validation::ValidationTypeContext::new_providerless(
                &parsed,
                &schemas,
                &provider_context,
                &signatures,
            ),
        );
        let bindings = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
            &parsed,
            &schemas,
            &signatures,
        );
        let findings = crate::validation::check_module_calls_providerless(
            crate::validation::ModuleCallCheckInput {
                authored: &parsed,
                semantic: Some(&parsed),
                base_dir: None,
                imported_signatures: &signatures,
                provider_context: &provider_context,
                schemas: &schemas,
                binding_use_report: &binding_use_report,
                bindings: &bindings,
                reference_checks: crate::validation::ModuleCallReferenceChecks::Skipped,
            },
        );
        let error = ModuleError::MissingArgument {
            module: "child".to_string(),
            argument: "required".to_string(),
        }
        .with_failing_call(parsed.module_calls[0].id().clone());
        assert!(
            ResidualModuleResolutionError::new(error, &[], &provider_context).is_some(),
            "a resolver error cannot be hidden before a concrete finding covers it",
        );
        let unrelated = ModuleError::MissingArgument {
            module: "child".to_string(),
            argument: "different".to_string(),
        }
        .with_failing_call(parsed.module_calls[0].id().clone());
        assert!(
            ResidualModuleResolutionError::new(unrelated, &findings, &provider_context).is_some(),
            "a finding for another argument must not claim this resolver error",
        );
        let covered = ModuleError::MissingArgument {
            module: "child".to_string(),
            argument: "required".to_string(),
        }
        .with_failing_call(parsed.module_calls[0].id().clone());
        assert!(
            ResidualModuleResolutionError::new(covered, &findings, &provider_context).is_none(),
            "the exact call-owner finding must cover the resolver backstop",
        );
    }
}

/// Opaque proof that every reachable module definition has passed the shared
/// BindingIndex reference-type validation before expansion.
///
/// Expansion is exposed only through [`Self::expand`] and
/// [`Self::expand_template`]; the resolver and the witness consumed by their
/// internal expansion primitives are private. Consequently a command may skip
/// provider/resource validation, but cannot construct an expansion path that
/// skips reference-type validation.
///
/// ```compile_fail
/// use carina_core::module_resolver::ModuleResolver;
/// // ModuleResolver is private; callers must obtain RefCheckedModuleGraph.
/// let _ = ModuleResolver::new(".");
/// ```
///
/// A checked graph owns exclusive access to the exact parse it checked, so a
/// caller cannot swap in a different parse at expansion time:
///
/// ```compile_fail
/// use std::path::Path;
/// use carina_core::module_resolver::RefCheckedModuleGraph;
/// use carina_core::parser::{ParsedFile, ProviderContext};
/// use carina_core::schema::SchemaRegistry;
///
/// let mut first = ParsedFile::default();
/// let mut second = ParsedFile::default();
/// let config = ProviderContext::default();
/// let schemas = SchemaRegistry::new();
/// let checked = RefCheckedModuleGraph::check(
///     &mut first,
///     Path::new("."),
///     &config,
///     &schemas,
/// ).unwrap();
/// checked.expand(&mut second).unwrap();
/// ```
pub struct RefCheckedModuleGraph<'parsed, 'cfg, E> {
    parsed: &'parsed mut crate::parser::File<E>,
    resolver: ModuleResolver<'cfg, StrictTypeResolution>,
    root_witness: RefCheckWitness,
}

/// Upstream expansion has a deliberately weaker type-name-resolution policy:
/// the consumer need not load every provider used by the upstream project.
/// Keeping that policy in a distinct, private graph prevents ordinary module
/// expansion from opting into it accidentally.
struct UpstreamRefCheckedModuleGraph<'parsed, 'cfg, E> {
    parsed: &'parsed mut crate::parser::File<E>,
    resolver: ModuleResolver<'cfg, DeferredUpstreamTypeResolution>,
    root_witness: RefCheckWitness,
}

/// Private capability produced only by `ModuleResolver::validate_reference_types`.
pub(super) struct RefCheckWitness(pub(super) ());

/// Compile-time policy for resolving type annotations in loaded modules.
pub(crate) trait TypeResolutionPolicy {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError>;

    fn resolve_static_type(
        type_expr: &crate::parser::TypeExpr,
        config: &ProviderContext,
    ) -> crate::static_type::ConsumerStaticType;
}

/// Normal module resolution requires every declared type to resolve.
pub(crate) struct StrictTypeResolution;

impl TypeResolutionPolicy for StrictTypeResolution {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError> {
        let type_errors = crate::validation::resolve_file_type_exprs(parsed, config);
        if let Some(first) = type_errors.into_iter().next() {
            return Err(ModuleError::TypeResolution(Box::new(first)));
        }
        Ok(())
    }

    fn resolve_static_type(
        type_expr: &crate::parser::TypeExpr,
        config: &ProviderContext,
    ) -> crate::static_type::ConsumerStaticType {
        crate::validation::resolve_consumer_static_type_for_signature(type_expr, config)
    }
}

/// Upstream projects are interpreted with the consumer's incomplete provider
/// registry, so unknown provider-owned types carry no usable constraint.
struct DeferredUpstreamTypeResolution;

impl TypeResolutionPolicy for DeferredUpstreamTypeResolution {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError> {
        crate::validation::resolve_file_type_exprs_best_effort(parsed, config);
        Ok(())
    }

    fn resolve_static_type(
        type_expr: &crate::parser::TypeExpr,
        config: &ProviderContext,
    ) -> crate::static_type::ConsumerStaticType {
        crate::validation::resolve_consumer_static_type(type_expr, config)
    }
}

/// Context for module resolution
pub(crate) struct ModuleResolver<'cfg, P = StrictTypeResolution> {
    /// Base directory for resolving relative imports
    pub(super) base_dir: PathBuf,
    /// Root of the configuration being expanded. Unlike `base_dir`, this does
    /// not change while recursively resolving nested module imports and is
    /// therefore the stable base for user-facing source labels.
    configuration_root: PathBuf,
    /// Cache of loaded modules: path -> ParsedFile
    pub(super) module_cache: HashMap<PathBuf, ParsedFile>,
    /// Currently resolving modules (for cycle detection)
    pub(super) resolving: HashSet<PathBuf>,
    /// Imported module definitions by alias
    pub(super) imported_modules: HashMap<String, ParsedFile>,
    /// `use { source = "..." }` paths by alias — the DSL-visible
    /// source location for each imported module. Threaded into the
    /// `ExpansionTrace` so plan rendering can label a composition
    /// group with `module "<binding>" (<source_path>)` (carina#3322).
    pub(super) module_paths: HashMap<String, String>,
    /// Parser configuration (decryptor, custom validators)
    pub(super) provider_context: crate::validation::typecheck::TypecheckProviderContext<'cfg>,
    /// Schema authority used by the mandatory pre-expansion reference check.
    pub(super) schemas: &'cfg SchemaRegistry,
    /// Present only for the editor-facing resolution entry point. Expansion
    /// records into it at the same seam that evaluates fixed-point arguments.
    pub(super) constraint_reports: RefCell<Option<ModuleCallConstraintReports>>,
    /// The type-resolution mode is selected by the resolver's type. This field
    /// is private so callers cannot construct a resolver with a new policy.
    type_resolution: PhantomData<P>,
}

impl<'cfg> ModuleResolver<'cfg, StrictTypeResolution> {
    /// Create a new resolver with the given base directory and default config
    #[cfg(test)]
    pub(crate) fn new(base_dir: impl AsRef<Path>) -> Self {
        static DEFAULT_CONFIG: std::sync::LazyLock<ProviderContext> =
            std::sync::LazyLock::new(ProviderContext::default);
        static EMPTY_SCHEMAS: std::sync::LazyLock<SchemaRegistry> =
            std::sync::LazyLock::new(SchemaRegistry::new);
        Self::with_providerless_config_and_schemas(base_dir, &DEFAULT_CONFIG, &EMPTY_SCHEMAS)
    }

    /// Create a new resolver with the given base directory and parser configuration
    pub(crate) fn with_config_and_schemas(
        base_dir: impl AsRef<Path>,
        config: &'cfg LoadedProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(
            base_dir,
            crate::validation::typecheck::TypecheckProviderContext::loaded(config),
            schemas,
        )
    }

    fn with_providerless_config_and_schemas(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(
            base_dir,
            crate::validation::typecheck::TypecheckProviderContext::providerless(config),
            schemas,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_config(base_dir: impl AsRef<Path>, config: &'cfg ProviderContext) -> Self {
        static EMPTY_SCHEMAS: std::sync::LazyLock<SchemaRegistry> =
            std::sync::LazyLock::new(SchemaRegistry::new);
        Self::with_providerless_config_and_schemas(base_dir, config, &EMPTY_SCHEMAS)
    }

    fn with_constraint_reports(
        base_dir: impl AsRef<Path>,
        config: &'cfg LoadedProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        let resolver = Self::with_config_and_schemas(base_dir, config, schemas);
        *resolver.constraint_reports.borrow_mut() = Some(Vec::new());
        resolver
    }

    fn with_providerless_constraint_reports(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        let resolver = Self::with_providerless_config_and_schemas(base_dir, config, schemas);
        *resolver.constraint_reports.borrow_mut() = Some(Vec::new());
        resolver
    }
}

impl<'cfg> ModuleResolver<'cfg, DeferredUpstreamTypeResolution> {
    fn for_upstream(
        base_dir: impl AsRef<Path>,
        config: &'cfg LoadedProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(
            base_dir,
            crate::validation::typecheck::TypecheckProviderContext::loaded(config),
            schemas,
        )
    }

    fn for_providerless_upstream(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(
            base_dir,
            crate::validation::typecheck::TypecheckProviderContext::providerless(config),
            schemas,
        )
    }
}

impl<'cfg, P: TypeResolutionPolicy> ModuleResolver<'cfg, P> {
    fn with_type_resolution(
        base_dir: impl AsRef<Path>,
        provider_context: crate::validation::typecheck::TypecheckProviderContext<'cfg>,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        let base_dir = base_dir.as_ref().to_path_buf();
        Self {
            configuration_root: base_dir.clone(),
            base_dir,
            module_cache: HashMap::new(),
            resolving: HashSet::new(),
            imported_modules: HashMap::new(),
            module_paths: HashMap::new(),
            provider_context,
            schemas,
            constraint_reports: RefCell::new(None),
            type_resolution: PhantomData,
        }
    }

    pub(super) fn config(&self) -> &'cfg ProviderContext {
        self.provider_context.as_provider_context()
    }

    pub(super) fn record_constraint_report(&self, report: ModuleCallConstraintReport) {
        if let Some(reports) = self.constraint_reports.borrow_mut().as_mut() {
            reports.push(report);
        }
    }

    pub(super) fn records_constraint_reports(&self) -> bool {
        self.constraint_reports.borrow().is_some()
    }

    /// Produce the private witness required by every expansion call.
    ///
    /// This is intentionally the only constructor for `RefCheckWitness`.
    /// Resource/provider validation may be skipped by a command, but the
    /// BindingIndex reference relation at module and function boundaries may
    /// not be skipped.
    fn validate_reference_types<E: crate::parser::ExportParamLike>(
        &self,
        parsed: &mut crate::parser::File<E>,
    ) -> Result<RefCheckWitness, ModuleError> {
        let imported_modules = self.resolved_module_signatures();
        lift_module_attribute_handles(parsed, &imported_modules);
        let bindings = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
            parsed,
            self.schemas,
            &imported_modules,
        );
        crate::validation::validate_expansion_boundary_ref_types_with_bindings(
            parsed,
            &imported_modules,
            &bindings,
        )
        .map(|()| RefCheckWitness(()))
        .map_err(ModuleError::ReferenceType)
    }

    fn resolved_module_signatures(&self) -> super::ResolvedModuleSignatures {
        self.imported_modules
            .iter()
            .map(|(alias, module)| {
                (
                    alias.clone(),
                    super::signature::resolved_signature_with_policy::<P>(module, self.config()),
                )
            })
            .collect()
    }

    /// Load and cache a module from a directory path.
    ///
    /// Modules are directory-scoped: a module is a directory containing one or
    /// more `.crn` files. Single-file modules are not supported — pass the
    /// module's directory, not an individual `.crn` file.
    pub fn load_module(&mut self, path: &str) -> Result<ParsedFile, ModuleError> {
        let full_path = self.resolve_path(path);

        // Distinguish "file exists but isn't a directory" from
        // "path doesn't exist / permission denied": only the former is the
        // single-file-module contract violation. Otherwise let canonicalize()
        // surface the underlying IO error (NotFound, PermissionDenied, ...).
        let metadata = full_path.metadata()?;
        if !metadata.is_dir() {
            return Err(ModuleError::NotADirectory {
                path: path.to_string(),
            });
        }

        let canonical = full_path.canonicalize()?;

        // Check for circular import
        if self.resolving.contains(&canonical) {
            return Err(ModuleError::CircularImport(path.to_string()));
        }

        // Check cache
        if let Some(module) = self.module_cache.get(&canonical) {
            return Ok(module.clone());
        }

        // Mark as resolving
        self.resolving.insert(canonical.clone());

        let mut parsed = match self.load_directory_module(&full_path) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.resolving.remove(&canonical);
                return Err(e);
            }
        };

        // Verify it's a module (has arguments or attributes)
        if parsed.arguments.is_empty() && parsed.attribute_params.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::NotFound(path.to_string()));
        }

        // Expansion-phase backstops for root-owned blocks inside modules. The
        // CLI's validation walks are the user-facing gates: they collect
        // module-path-prefixed diagnostics and stop before expansion, so
        // validate/plan/apply and lint cannot emit both forms. The production
        // paths that bypass those walks but still expand are destroy, state
        // refresh, and apply's backend-bootstrap re-resolve
        // (`skip_resource_validation = true`). `fixture_plan` also depends on
        // these backstops as test support.
        if !parsed.providers.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ProviderInModule);
        }
        if !parsed.state_blocks.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::StateBlockInModule);
        }
        if parsed.backend.is_some() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::BackendInModule);
        }
        if !parsed.upstream_states.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::UpstreamStateInModule);
        }
        if !parsed.export_params.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ExportsInModule);
        }

        // Recursively resolve nested module imports within this module.
        // The module's directory is used for resolving its relative imports.
        if let Err(e) = self.resolve_nested_modules(&mut parsed, &full_path) {
            self.resolving.remove(&canonical);
            return Err(e);
        }

        // Remove from resolving set
        self.resolving.remove(&canonical);

        // Cache the module
        self.module_cache.insert(canonical, parsed.clone());

        Ok(parsed)
    }

    /// Load all .crn files from a directory and merge them into a single ParsedFile.
    ///
    /// Routed through [`crate::config_loader::parse_directory_files`] so
    /// every file's `ParseContext` is seeded with the binding-name union
    /// from sibling `.crn` files. Without this, `arguments {}` declared
    /// in `main.crn` would be invisible to `${env}` interpolations in
    /// sibling `role.crn` (#2815). #2817.
    fn load_directory_module(&self, dir_path: &Path) -> Result<ParsedFile, ModuleError> {
        let paths = sorted_crn_paths_in(dir_path)?;
        let mut files: Vec<(std::path::PathBuf, String)> = Vec::with_capacity(paths.len());
        for path in paths {
            let content = std::fs::read_to_string(&path)?;
            files.push((path, content));
        }
        let parsed_files = crate::config_loader::parse_directory_files_located_from_root(
            &files,
            self.config(),
            &self.configuration_root,
        )
        .map_err(|located| match located.source_file {
            Some(path) => ModuleError::ParseInFile {
                path: crate::utils::normalize_source_path(&path),
                error: located.error,
            },
            None => ModuleError::Parse(*located.error),
        })?;

        let mut merged = ParsedFile::default();
        for (_, parsed) in parsed_files {
            crate::config_loader::merge_parsed_file(&mut merged, parsed.into_inner());
        }

        // Keep the authored value tree beside the semantic tree while this
        // file is a reusable template. Reference resolution below may fold a
        // direct reference to a concrete value, but dependency edges for an
        // argument-dependent `if` must be derived only after expansion has
        // selected a branch.
        merged.capture_template_dependency_values();

        // Resolve references while the module's own binding namespace is still
        // intact. Besides folding module-local values, this records the direct
        // dependency bindings that expansion subsequently instance-prefixes.
        // Argument bindings remain deferred for call-site substitution.
        crate::parser::resolve_resource_refs_with_config(&mut merged, self.config())
            .map_err(ModuleError::Parse)?;

        P::resolve_file_type_exprs(&mut merged, self.config())?;
        Ok(merged)
    }

    /// Resolve a relative path to an absolute path
    fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    /// Process imports and store imported modules
    pub fn process_imports(
        &mut self,
        imports: &[UseStatement],
        module_calls: &[crate::parser::ModuleCall],
    ) -> Result<(), ModuleError> {
        for import in imports {
            let module = self.load_module(&import.path).map_err(|error| {
                let owning_calls = module_calls
                    .iter()
                    .filter(|call| call.module_name == import.alias)
                    .map(|call| call.id().clone())
                    .collect();
                error.with_owning_calls(owning_calls)
            })?;
            self.imported_modules.insert(import.alias.clone(), module);
            self.module_paths
                .insert(import.alias.clone(), import.path.clone());
        }
        Ok(())
    }

    fn process_imports_collecting(
        &mut self,
        imports: &[UseStatement],
        module_calls: &[crate::parser::ModuleCall],
    ) -> Vec<ModuleError> {
        let mut errors = Vec::new();
        for import in imports {
            match self.load_module(&import.path) {
                Ok(module) => {
                    self.imported_modules.insert(import.alias.clone(), module);
                    self.module_paths
                        .insert(import.alias.clone(), import.path.clone());
                }
                Err(error) => {
                    let owning_calls = module_calls
                        .iter()
                        .filter(|call| call.module_name == import.alias)
                        .map(|call| call.id().clone())
                        .collect();
                    errors.push(error.with_owning_calls(owning_calls));
                }
            }
        }
        errors
    }

    /// Resolve nested module imports within a parsed module.
    ///
    /// This processes the module's own imports and module_calls, expanding any
    /// nested modules recursively. Cycle detection is handled by the `resolving`
    /// set in `load_module()`.
    fn resolve_nested_modules(
        &mut self,
        parsed: &mut ParsedFile,
        base_dir: &Path,
    ) -> Result<(), ModuleError> {
        // Save and temporarily replace the base_dir and imported_modules
        let original_base_dir = std::mem::replace(&mut self.base_dir, base_dir.to_path_buf());
        let original_imported = std::mem::take(&mut self.imported_modules);
        let original_paths = std::mem::take(&mut self.module_paths);

        let conditional_calls = parsed
            .deferred_module_ifs
            .iter()
            .flat_map(|deferred| deferred.branches())
            .filter_map(|branch| match branch {
                DeferredIfBody::ModuleCall(call) => Some(call.clone()),
                DeferredIfBody::Resource(_)
                | DeferredIfBody::DataSource(_)
                | DeferredIfBody::ExpandedModuleCall(_) => None,
            })
            .collect::<Vec<_>>();

        // Process the module's own imports. Conditional calls participate in
        // source ownership even though their branch cannot be selected until
        // an outer caller supplies the module argument.
        let imports = parsed.uses.clone();
        let mut owned_calls = parsed.module_calls.clone();
        owned_calls.extend(conditional_calls.iter().cloned());
        let result = self.process_imports(&imports, &owned_calls);

        if let Err(e) = result {
            // Restore state on error
            self.base_dir = original_base_dir;
            self.imported_modules = original_imported;
            self.module_paths = original_paths;
            return Err(e);
        }

        let ref_check = match self.validate_reference_types(parsed) {
            Ok(witness) => witness,
            Err(error) => {
                self.base_dir = original_base_dir;
                self.imported_modules = original_imported;
                self.module_paths = original_paths;
                return Err(error);
            }
        };

        // Expand the module's own module_calls. `module_calls` is cloned
        // because the loop body folds each contribution into the whole file;
        // that mutable borrow conflicts with any live immutable borrow of a
        // `parsed` field.
        let module_calls = parsed.module_calls.clone();
        let signatures = self.resolved_module_signatures();
        let binding_index = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
            parsed,
            self.schemas,
            &signatures,
        );
        let resolved_bindings = crate::binding_index::ResolvedBindings::for_validation(parsed);
        let mut expanded_calls = Vec::with_capacity(module_calls.len());
        let mut expanded_conditional_calls = HashMap::with_capacity(conditional_calls.len());
        for call in &conditional_calls {
            let instance_prefix = instance_prefix_for_call(call);
            match self.expand_module_call_checked(
                call,
                &instance_prefix,
                &ref_check,
                Some(&binding_index),
                Some(&resolved_bindings),
            ) {
                Ok(expanded) => {
                    expanded_conditional_calls.insert(call.id().clone(), expanded);
                }
                Err(e) => {
                    self.base_dir = original_base_dir;
                    self.imported_modules = original_imported;
                    self.module_paths = original_paths;
                    return Err(e);
                }
            }
        }
        for call in &module_calls {
            let instance_prefix = instance_prefix_for_call(call);
            match self.expand_module_call_checked(
                call,
                &instance_prefix,
                &ref_check,
                Some(&binding_index),
                Some(&resolved_bindings),
            ) {
                Ok(expanded) => expanded_calls.push(expanded),
                Err(e) => {
                    self.base_dir = original_base_dir;
                    self.imported_modules = original_imported;
                    self.module_paths = original_paths;
                    return Err(e);
                }
            }
        }
        drop(binding_index);
        for deferred in &mut parsed.deferred_module_ifs {
            for branch in deferred.branches_mut() {
                let DeferredIfBody::ModuleCall(call) = branch else {
                    continue;
                };
                let expanded = expanded_conditional_calls
                    .remove(call.id())
                    .expect("every conditional call was expanded in its module scope");
                *branch = DeferredIfBody::ExpandedModuleCall(std::sync::Arc::new(expanded));
            }
        }
        debug_assert!(expanded_conditional_calls.is_empty());
        for expanded in expanded_calls {
            // The module contribution is a `ParsedFile`; fold it in via the
            // same-phase exhaustive merge. This is deliberately the raw
            // template path: forwarded outer arguments may still be deferred.
            parsed.merge_from(expanded);
        }

        // Restore original state
        self.base_dir = original_base_dir;
        self.imported_modules = original_imported;
        self.module_paths = original_paths;

        Ok(())
    }
}

fn lift_module_attribute_handles<E>(
    parsed: &mut crate::parser::File<E>,
    signatures: &super::ResolvedModuleSignatures,
) {
    let calls = parsed
        .module_calls
        .iter()
        .filter_map(|call| {
            call.binding_name
                .as_ref()
                .map(|binding| (binding.clone(), call.module_name.clone()))
        })
        .collect::<HashMap<_, _>>();
    let local_resources = parsed
        .iter_all_resources()
        .filter_map(|resource| match resource {
            crate::parser::ResourceRef::Resource(resource) => {
                resource.binding.as_ref().map(|binding| {
                    (
                        binding.clone(),
                        crate::parser::ResourceTypePath::new(
                            resource.id.provider.clone(),
                            resource.id.resource_type.clone(),
                        ),
                    )
                })
            }
            crate::parser::ResourceRef::Deferred {
                resource,
                deferred: _deferred,
            } => resource.binding.as_ref().map(|binding| {
                (
                    binding.clone(),
                    crate::parser::ResourceTypePath::new(
                        resource.id.provider.clone(),
                        resource.id.resource_type.clone(),
                    ),
                )
            }),
            crate::parser::ResourceRef::LiveComposition(_composition) => None,
            crate::parser::ResourceRef::DataSource(_data_source) => None,
        })
        .collect::<HashMap<_, _>>();

    let lift = |value: &crate::resource::Value| {
        let crate::resource::Value::Deferred(crate::resource::DeferredValue::ResourceRef { path }) =
            value
        else {
            return None;
        };
        if !path.segments().is_empty() {
            return None;
        }
        let module_name = calls.get(path.binding())?;
        let static_type = signatures
            .get(module_name)?
            .attributes
            .get(path.attribute())?
            .as_ref()?
            .known()?;
        let crate::parser::TypeExpr::Ref(resource_type) = static_type else {
            return None;
        };
        Some(
            crate::eval_value::UnexpandedStructuralHandle::module_attribute(
                path.binding(),
                path.attribute(),
                resource_type.clone(),
            )
            .resource_handle()
            .expect("a typed module attribute constructs a resource handle"),
        )
    };

    for attribute in &mut parsed.attribute_params {
        let handle = match attribute.value.as_ref() {
            Some(crate::parser::BoundaryExpression::Value(value)) => lift(value),
            Some(crate::parser::BoundaryExpression::Handle(_)) | None => None,
        };
        if let Some(handle) = handle {
            attribute.value = Some(crate::parser::BoundaryExpression::Handle(handle));
        }
    }

    for call in &mut parsed.module_calls {
        for argument in call.arguments_mut() {
            let handle = argument.value().and_then(&lift);
            if let Some(handle) = handle {
                argument.replace_with_resource_handle(handle);
            }
        }
    }

    let lift_dependency = |operand: &mut crate::parser::UnexpandedDependencyOperand| {
        let handle = match operand {
            crate::parser::UnexpandedDependencyOperand::Handle(_handle) => None,
            crate::parser::UnexpandedDependencyOperand::BareBinding(binding) => {
                local_resources.get(binding.as_str()).map(|resource_type| {
                    crate::eval_value::UnexpandedResourceHandle::new(
                        binding.as_str(),
                        resource_type.clone(),
                    )
                })
            }
            crate::parser::UnexpandedDependencyOperand::AttributeSelection {
                binding,
                attribute,
            } => calls
                .get(binding.as_str())
                .and_then(|module_name| signatures.get(module_name))
                .and_then(|signature| signature.attributes.get(attribute))
                .and_then(Option::as_ref)
                .and_then(|static_type| static_type.known())
                .and_then(|resolved| match resolved {
                    crate::parser::TypeExpr::Ref(resource_type) => Some(
                        crate::eval_value::UnexpandedStructuralHandle::module_attribute(
                            binding.as_str(),
                            attribute,
                            resource_type.clone(),
                        )
                        .resource_handle()
                        .expect("a typed module output constructs a resource handle"),
                    ),
                    crate::parser::TypeExpr::String
                    | crate::parser::TypeExpr::Bool
                    | crate::parser::TypeExpr::Int
                    | crate::parser::TypeExpr::Float
                    | crate::parser::TypeExpr::Duration
                    | crate::parser::TypeExpr::Simple(_)
                    | crate::parser::TypeExpr::List(_)
                    | crate::parser::TypeExpr::Map(_)
                    | crate::parser::TypeExpr::DottedUnresolved(_)
                    | crate::parser::TypeExpr::SchemaType { .. }
                    | crate::parser::TypeExpr::Struct { .. }
                    | crate::parser::TypeExpr::StringLiteral(_)
                    | crate::parser::TypeExpr::Union(_) => None,
                }),
        };
        if let Some(handle) = handle {
            *operand = crate::parser::UnexpandedDependencyOperand::Handle(handle);
        }
    };

    for resource in parsed.iter_all_resources_mut() {
        match resource {
            crate::parser::ResourceMut::Resource(resource) => {
                for operand in resource.dependencies.depends_on.iter_mut() {
                    lift_dependency(operand);
                }
            }
            crate::parser::ResourceMut::DataSource(data_source) => {
                for operand in data_source.dependencies.depends_on.iter_mut() {
                    lift_dependency(operand);
                }
            }
            crate::parser::ResourceMut::Deferred {
                resource,
                attributes: (),
            } => {
                for operand in resource.dependencies.depends_on.iter_mut() {
                    lift_dependency(operand);
                }
            }
            crate::parser::ResourceMut::LiveComposition(_composition) => {}
        }
    }
    for wait in &mut parsed.wait_bindings {
        for operand in wait.depends_on.iter_mut() {
            lift_dependency(operand);
        }
    }
}

impl<'parsed, 'cfg, E: crate::parser::ExportParamLike> RefCheckedModuleGraph<'parsed, 'cfg, E> {
    /// Load the complete reachable module graph and validate every reference
    /// boundary before making expansion available.
    pub fn check(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg LoadedProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let resolver = ModuleResolver::with_config_and_schemas(base_dir, config, schemas);
        Self::check_with_resolver(parsed, resolver)
    }

    /// Bootstrap/editor variant that retains structural value checks while
    /// skipping provider-owned custom-type leaves.
    pub fn check_providerless(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let resolver =
            ModuleResolver::with_providerless_config_and_schemas(base_dir, config, schemas);
        Self::check_with_resolver(parsed, resolver)
    }

    fn check_with_resolver(
        parsed: &'parsed mut crate::parser::File<E>,
        mut resolver: ModuleResolver<'cfg, StrictTypeResolution>,
    ) -> Result<Self, ModuleError> {
        resolver.process_imports(&parsed.uses, &parsed.module_calls)?;
        let root_witness = resolver.validate_reference_types(parsed)?;
        Ok(Self {
            parsed,
            resolver,
            root_witness,
        })
    }

    /// Consume the checked graph and expand its root configuration.
    pub fn expand(mut self) -> Result<(), ModuleError> {
        resolve_modules_with_resolver(self.parsed, &mut self.resolver, &self.root_witness)
    }

    /// Expand as much as possible while retaining argument-dependent nodes
    /// owned by a reusable module template. Validation, lint, and editors use
    /// this path; it deliberately does not mint a publishable root graph.
    pub fn expand_template(mut self) -> Result<(), ModuleError> {
        let mut errors = resolve_modules_with_resolver_collecting(
            self.parsed,
            &mut self.resolver,
            &self.root_witness,
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.remove(0))
        }
    }
}

impl<'parsed, 'cfg, E: crate::parser::ExportParamLike>
    UpstreamRefCheckedModuleGraph<'parsed, 'cfg, E>
{
    /// The sole constructor for the upstream-only, unknown-type-deferring
    /// resolver. Ordinary callers can construct only `RefCheckedModuleGraph`.
    fn check(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg LoadedProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let mut resolver = ModuleResolver::<DeferredUpstreamTypeResolution>::for_upstream(
            base_dir, config, schemas,
        );
        resolver.process_imports(&parsed.uses, &parsed.module_calls)?;
        let root_witness = resolver.validate_reference_types(parsed)?;
        Ok(Self {
            parsed,
            resolver,
            root_witness,
        })
    }

    fn check_providerless(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let mut resolver =
            ModuleResolver::<DeferredUpstreamTypeResolution>::for_providerless_upstream(
                base_dir, config, schemas,
            );
        resolver.process_imports(&parsed.uses, &parsed.module_calls)?;
        let root_witness = resolver.validate_reference_types(parsed)?;
        Ok(Self {
            parsed,
            resolver,
            root_witness,
        })
    }

    fn expand(mut self) -> Result<(), ModuleError> {
        resolve_modules_with_resolver(self.parsed, &mut self.resolver, &self.root_witness)
    }
}

/// Resolve all modules without a loaded provider catalog.
pub fn resolve_modules_providerless(
    parsed: crate::parser::ParsedFile,
    base_dir: &Path,
) -> Result<ModuleResolvedParsedFile, ModuleError> {
    resolve_modules_with_providerless_config(
        parsed,
        base_dir,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
}

/// Resolve all modules in a parsed file with the given parser configuration.
///
/// Generic over the export-parameter shape so callers can pass either
/// `ParsedFile` (parser phase) or `InferredFile` (post-loader phase) —
/// the resolver only touches `uses`, `module_calls`, and `resources`.
pub fn resolve_modules_with_config<E: crate::parser::ExportParamLike>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &LoadedProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    RefCheckedModuleGraph::check(&mut parsed, base_dir, config, schemas)?.expand()?;
    finalize_deferred_for_bodies(parsed)
}

/// Resolve modules in a bootstrap/editor context. Every value check is
/// classified first, and provider-owned schema types are left unchecked.
pub fn resolve_modules_with_providerless_config<E: crate::parser::ExportParamLike>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    RefCheckedModuleGraph::check_providerless(&mut parsed, base_dir, config, schemas)?.expand()?;
    finalize_deferred_for_bodies(parsed)
}

/// Template-tolerant counterpart of [`resolve_modules_with_config`].
///
/// Imported calls are expanded and checked, but a module argument owned by the
/// root directory may remain deferred for a future caller.
pub fn resolve_module_template_with_config<E: crate::parser::ExportParamLike>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &LoadedProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    RefCheckedModuleGraph::check(&mut parsed, base_dir, config, schemas)?.expand_template()?;
    finalize_deferred_for_bodies(parsed)
}

/// Providerless/editor form of [`resolve_module_template_with_config`].
pub fn resolve_module_template_with_providerless_config<E: crate::parser::ExportParamLike>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    RefCheckedModuleGraph::check_providerless(&mut parsed, base_dir, config, schemas)?
        .expand_template()?;
    finalize_deferred_for_bodies(parsed)
}

/// Expand an upstream project's modules using the consumer's necessarily
/// partial provider context. Unresolvable custom type names remain unchecked;
/// all concrete value and liftable boundary mismatches still use the normal
/// witness and shared value checker.
pub(crate) fn resolve_upstream_modules_with_providerless_config<
    E: crate::parser::ExportParamLike,
>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    UpstreamRefCheckedModuleGraph::check_providerless(&mut parsed, base_dir, config, schemas)?
        .expand()?;
    finalize_deferred_for_bodies(parsed)
}

pub(crate) fn resolve_upstream_modules_with_config<E: crate::parser::ExportParamLike>(
    mut parsed: crate::parser::File<E>,
    base_dir: &Path,
    config: &LoadedProviderContext,
    schemas: &SchemaRegistry,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    UpstreamRefCheckedModuleGraph::check(&mut parsed, base_dir, config, schemas)?.expand()?;
    finalize_deferred_for_bodies(parsed)
}

/// Resolve modules while retaining the expander's own per-call constraint
/// outcomes and source locations for editor diagnostics.
///
/// `covered_findings` are typed source diagnostics the caller will publish.
/// Residuals returned from this seam have those call-owned errors removed,
/// including individual covered members of a mixed reference-type failure.
pub fn resolve_modules_with_diagnostics<E: crate::parser::ExportParamLike + Clone>(
    parsed: &crate::parser::File<E>,
    base_dir: &Path,
    config: &LoadedProviderContext,
    schemas: &SchemaRegistry,
    covered_findings: &[crate::validation::ModuleCallFinding],
) -> ModuleResolutionDiagnosticResult<E> {
    let resolver = ModuleResolver::with_constraint_reports(base_dir, config, schemas);
    resolve_modules_with_diagnostics_and_resolver(
        parsed,
        resolver,
        config.as_provider_context(),
        covered_findings,
    )
}

/// Editor/bootstrap diagnostic expansion with provider-owned value checks
/// unavailable by construction.
pub fn resolve_modules_with_providerless_diagnostics<E: crate::parser::ExportParamLike + Clone>(
    parsed: &crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
    covered_findings: &[crate::validation::ModuleCallFinding],
) -> ModuleResolutionDiagnosticResult<E> {
    let resolver = ModuleResolver::with_providerless_constraint_reports(base_dir, config, schemas);
    resolve_modules_with_diagnostics_and_resolver(parsed, resolver, config, covered_findings)
}

fn resolve_modules_with_diagnostics_and_resolver<E: crate::parser::ExportParamLike + Clone>(
    parsed: &crate::parser::File<E>,
    mut resolver: ModuleResolver<'_, StrictTypeResolution>,
    config: &ProviderContext,
    covered_findings: &[crate::validation::ModuleCallFinding],
) -> ModuleResolutionDiagnosticResult<E> {
    let mut graph = parsed.clone();
    let mut errors = resolver.process_imports_collecting(&graph.uses, &graph.module_calls);
    match resolver.validate_reference_types(&mut graph) {
        Ok(root_witness) => errors.extend(resolve_modules_with_resolver_collecting(
            &mut graph,
            &mut resolver,
            &root_witness,
        )),
        Err(error) => errors.push(error),
    }
    let (graph, deferred_for_errors) = finalize_deferred_for_bodies_collecting(graph);
    errors.extend(deferred_for_errors);
    let constraint_reports = resolver
        .constraint_reports
        .into_inner()
        .expect("diagnostic resolver always records constraint reports");
    let findings = crate::validation::check_expanded_module_calls(
        crate::validation::ExpandedModuleCallCheckInput::Constraints {
            compositions: &graph.compositions,
            constraint_reports: &constraint_reports,
        },
    );
    let resolution_failed = !errors.is_empty();
    let errors = errors
        .into_iter()
        .filter_map(|error| ResidualModuleResolutionError::new(error, covered_findings, config))
        .collect::<Vec<_>>();
    if !findings.is_empty() {
        ModuleResolutionDiagnosticResult::ConstraintFailed { findings, errors }
    } else if !resolution_failed {
        ModuleResolutionDiagnosticResult::Expanded {
            graph: Box::new(graph),
        }
    } else {
        ModuleResolutionDiagnosticResult::Unowned { errors }
    }
}

/// A parser-phase module contribution that has proved no parser-template
/// conditional can escape into the published root graph.
///
/// The inner file is private and this conversion is the only constructor used
/// by root expansion. Recursive module-template expansion intentionally keeps
/// using `ParsedFile` so an outer argument may still select a forwarded node.
#[derive(Debug)]
struct ExpandedModuleContribution(ParsedFile);

impl ExpandedModuleContribution {
    fn try_new(file: ParsedFile) -> Result<Self, ModuleError> {
        audit_expanded_module_contribution(&file).map_err(|context| {
            ModuleError::Parse(ParseError::InternalError {
                expected: "a module contribution with no deferred module conditionals".to_string(),
                context,
            })
        })?;
        Ok(Self(file))
    }

    fn merge_into<E: crate::parser::ExportParamLike>(self, target: &mut File<E>) {
        let contribution = crate::config_loader::relabel_export_phase(self.0);
        crate::config_loader::merge_parsed_file(target, contribution);
    }
}

/// Publication gate for a fully resolved root graph. Validation and editor
/// diagnostics deliberately operate on reusable module templates before this
/// gate; planning/apply and the ordinary resolver must pass it.
pub fn ensure_expanded_root_graph<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal,
>(
    file: &File<E, D>,
) -> Result<(), ModuleError> {
    audit_expanded_module_contribution(file)
        .map_err(|context| ModuleError::UnexpandedModuleTemplate { context })
}

fn audit_value_has_no_module_if(
    value: &Value,
    context: std::fmt::Arguments<'_>,
) -> Result<(), String> {
    if super::deferred_if::value_contains_module_if(value) {
        Err(context.to_string())
    } else {
        Ok(())
    }
}

fn audit_validate_expr_has_no_module_if(
    expression: &ValidateExpr,
    context: std::fmt::Arguments<'_>,
) -> Result<(), String> {
    match expression {
        ValidateExpr::BoundArgument { value, .. } => audit_value_has_no_module_if(value, context),
        ValidateExpr::Compare { lhs, rhs, .. }
        | ValidateExpr::IntegerArithmetic { lhs, rhs, .. }
        | ValidateExpr::And(lhs, rhs)
        | ValidateExpr::Or(lhs, rhs) => {
            audit_validate_expr_has_no_module_if(lhs, context)?;
            audit_validate_expr_has_no_module_if(rhs, context)
        }
        ValidateExpr::Not(inner) => audit_validate_expr_has_no_module_if(inner, context),
        ValidateExpr::FunctionCall { args, .. } => {
            for argument in args {
                audit_validate_expr_has_no_module_if(argument, context)?;
            }
            Ok(())
        }
        ValidateExpr::Bool(_)
        | ValidateExpr::Int(_)
        | ValidateExpr::Float(_)
        | ValidateExpr::Duration(_)
        | ValidateExpr::String(_)
        | ValidateExpr::Var(_)
        | ValidateExpr::Access { .. }
        | ValidateExpr::Null => Ok(()),
    }
}

fn audit_attributes<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    context: std::fmt::Arguments<'_>,
) -> Result<(), String> {
    for (index, value) in values.into_iter().enumerate() {
        audit_value_has_no_module_if(value, format_args!("{context}[{index}]"))?;
    }
    Ok(())
}

/// Exhaustively classify every `File` field, then recursively classify every
/// `Value`/`DeferredValue` shape reachable from fields that carry values. A new
/// file field or deferred-value variant therefore breaks compilation until the
/// publication audit makes an explicit decision about it.
fn audit_expanded_module_contribution<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal,
>(
    file: &File<E, D>,
) -> Result<(), String> {
    file.assert_module_if_audit_classifies_every_field();
    let File {
        providers,
        resources,
        data_sources,
        compositions,
        variables,
        uses: _uses,
        module_calls,
        arguments,
        attribute_params,
        export_params,
        backend,
        state_blocks,
        user_functions,
        upstream_states: _upstream_states,
        wait_bindings,
        requires,
        structural_bindings: _structural_bindings,
        parser_binding_metadata,
        warnings: _warnings,
        deferred_for_phase: _deferred_for_phase,
        deferred_module_ifs,
        expansion_trace: _expansion_trace,
        ..
    } = file;

    if !deferred_module_ifs.is_empty() {
        return Err("expanded contribution structural conditionals".to_string());
    }
    for (index, provider) in providers.iter().enumerate() {
        let crate::parser::ProviderConfig {
            name: _name,
            attributes,
            default_tags,
            source: _source,
            version: _version,
            revision: _revision,
            unresolved_attributes,
            binding: _binding,
        } = provider;
        audit_attributes(
            attributes.values(),
            format_args!("provider[{index}].attributes"),
        )?;
        audit_attributes(
            default_tags.values(),
            format_args!("provider[{index}].default_tags"),
        )?;
        audit_attributes(
            unresolved_attributes.values(),
            format_args!("provider[{index}].unresolved_attributes"),
        )?;
    }
    for (index, resource) in resources.iter().enumerate() {
        let crate::resource::Resource {
            id: _id,
            attributes,
            directives: _directives,
            resolved_ignore_changes: _resolved_ignore_changes,
            prefixes: _prefixes,
            binding: _binding,
            dependencies: _dependencies,
            module_source: _module_source,
            default_tag_keys: _default_tag_keys,
            loop_provenance: _loop_provenance,
            quoted_string_attrs: _quoted_string_attrs,
        } = resource;
        audit_attributes(
            attributes.values(),
            format_args!("resource[{index}].attributes"),
        )?;
    }
    for (index, data_source) in data_sources.iter().enumerate() {
        let crate::resource::DataSource {
            id: _id,
            attributes,
            directives: _directives,
            binding: _binding,
            dependencies: _dependencies,
            module_source: _module_source,
            quoted_string_attrs: _quoted_string_attrs,
        } = data_source;
        audit_attributes(
            attributes.values(),
            format_args!("data_source[{index}].attributes"),
        )?;
    }
    for (index, composition) in compositions.iter().enumerate() {
        let crate::resource::LiveComposition {
            id: _id,
            signature,
            binding: _binding,
            dependency_bindings: _dependency_bindings,
            module_name: _module_name,
            instance: _instance,
            call_path: _call_path,
            quoted_string_attrs: _quoted_string_attrs,
        } = composition;
        let crate::resource::LiveCompositionSignature {
            arguments,
            attributes,
            pending_constraints,
        } = signature;
        for (name, argument) in arguments {
            argument.try_visit_values(&mut |value| {
                audit_value_has_no_module_if(
                    value,
                    format_args!("composition[{index}].argument.{name}"),
                )
            })?;
        }
        for (name, attribute) in attributes {
            match attribute.value() {
                Some(value) => audit_value_has_no_module_if(
                    &value,
                    format_args!("composition[{index}].attribute.{name}"),
                )?,
                None => {
                    let _resolved_handle = attribute
                        .handle()
                        .expect("a composition attribute without a value is a typed handle");
                }
            }
        }
        for constraint in pending_constraints {
            audit_validate_expr_has_no_module_if(
                constraint.expression(),
                format_args!("composition[{index}].constraint"),
            )?;
        }
    }
    audit_attributes(variables.values(), format_args!("variables"))?;
    for (index, call) in module_calls.iter().enumerate() {
        call.try_visit_values_for_module_if_audit(&mut |value| {
            audit_value_has_no_module_if(value, format_args!("module_call[{index}].argument"))
        })?;
    }
    for (index, argument) in arguments.iter().enumerate() {
        let crate::parser::ArgumentParameter {
            name: _name,
            static_type: _static_type,
            default,
            description: _description,
            validations,
        } = argument;
        if let Some(default) = default {
            audit_value_has_no_module_if(default, format_args!("argument[{index}].default"))?;
        }
        for validation in validations {
            audit_validate_expr_has_no_module_if(
                validation.condition_for_module_if_audit(),
                format_args!("argument[{index}].validation"),
            )?;
        }
    }
    for (index, attribute) in attribute_params.iter().enumerate() {
        if let Some(value) = attribute.value_for_module_if_audit() {
            audit_value_has_no_module_if(value, format_args!("attribute[{index}].value"))?;
        }
    }
    for (index, export) in export_params.iter().enumerate() {
        if let Some(value) = export.value() {
            audit_value_has_no_module_if(value, format_args!("export[{index}].value"))?;
        }
    }
    if let Some(backend) = backend {
        let crate::parser::BackendConfig {
            backend_type: _backend_type,
            attributes,
            attribute_spans: _attribute_spans,
        } = backend;
        audit_attributes(attributes.values(), format_args!("backend.attributes"))?;
    }
    for (index, block) in state_blocks.iter().enumerate() {
        match block {
            StateBlock::Import {
                to: _to,
                to_span: _to_span,
                id,
            } => {
                audit_value_has_no_module_if(id, format_args!("state_block[{index}].id"))?;
            }
            StateBlock::Removed { from: _from } => {}
            StateBlock::Moved {
                addresses: _addresses,
            } => {}
        }
    }
    for (name, function) in user_functions {
        let crate::parser::UserFunction {
            name: _function_name,
            params,
            return_type: _return_type,
            local_lets,
            body,
        } = function;
        for (index, parameter) in params.iter().enumerate() {
            let crate::parser::FnParam {
                name: _parameter_name,
                param_type: _param_type,
                default,
            } = parameter;
            if let Some(default) = default {
                audit_value_has_no_module_if(
                    default,
                    format_args!("function.{name}.parameter[{index}]"),
                )?;
            }
        }
        for (local, value, _line) in local_lets {
            audit_value_has_no_module_if(value, format_args!("function.{name}.local.{local}"))?;
        }
        let crate::parser::UserFunctionBody {
            value,
            source_line: _source_line,
            source_file: _source_file,
        } = body;
        audit_value_has_no_module_if(value, format_args!("function.{name}.body"))?;
    }
    for (index, wait) in wait_bindings.iter().enumerate() {
        let crate::parser::WaitBinding {
            binding: _binding,
            target: _target,
            until_raw: _until_raw,
            until_predicate,
            timeout_secs: _timeout_secs,
            depends_on: _depends_on,
            line: _line,
        } = wait;
        let crate::parser::UntilPredicateAst {
            lhs_segments: _lhs_segments,
            rhs,
        } = until_predicate;
        audit_value_has_no_module_if(rhs, format_args!("wait[{index}].predicate"))?;
    }
    for (index, requirement) in requires.iter().enumerate() {
        audit_validate_expr_has_no_module_if(
            requirement.condition_for_module_if_audit(),
            format_args!("require[{index}]"),
        )?;
    }
    file.try_visit_deferred_for_values_for_module_if_audit(&mut |value| {
        audit_value_has_no_module_if(value, format_args!("deferred_for"))
    })?;
    parser_binding_metadata.try_visit_values(&mut |value| {
        audit_value_has_no_module_if(value, format_args!("parser_binding_metadata"))
    })?;
    file.try_visit_bare_binding_values(&mut |value| {
        audit_value_has_no_module_if(value, format_args!("parsed_binding_uses"))
    })?;
    Ok(())
}

/// Convert parser-side deferred-loop bodies into the resource-only typestate
/// consumed by every post-resolution subsystem.
///
/// This runs only after root and module-expanded loops have crossed argument
/// substitution. A read or module-call template therefore reaches exactly one
/// typed rejection seam, with enough information to distinguish a genuinely
/// residual iterable from one that became concrete during module expansion.
fn finalize_deferred_for_bodies<E>(
    parsed: crate::parser::File<E>,
) -> Result<ModuleResolvedFile<E>, ModuleError> {
    let (resolved, errors) = finalize_deferred_for_bodies_collecting(parsed);
    match ModuleError::from_errors(errors) {
        Some(error) => Err(error),
        None => Ok(resolved),
    }
}

fn finalize_deferred_for_bodies_collecting<E>(
    parsed: crate::parser::File<E>,
) -> (ModuleResolvedFile<E>, Vec<ModuleError>) {
    let known_bindings = deferred_for_known_bindings(&parsed);
    parsed.map_deferred_for_phase(|pending| {
        let mut resolved = Vec::with_capacity(pending.len());
        let mut errors = Vec::new();
        for expression in pending {
            match expression.into_resource_expression(&known_bindings) {
                Ok(expression) => resolved.push(expression),
                Err(error) => {
                    errors.push(deferred_for_resource_expression_error(error));
                }
            }
        }
        (resolved, errors)
    })
}

fn deferred_for_resource_expression_error(
    error: crate::parser::DeferredForResourceExpressionError,
) -> ModuleError {
    match error {
        crate::parser::DeferredForResourceExpressionError::UndefinedIterable(error) => {
            parse_error_to_module_error(error)
        }
        crate::parser::DeferredForResourceExpressionError::Unsupported { file, error } => {
            let error = ParseError::DeferredForBodyUnsupported(error);
            match file {
                Some(path) => ModuleError::ParseInFile {
                    path: std::path::PathBuf::from(path),
                    error: Box::new(error),
                },
                None => ModuleError::Parse(error),
            }
        }
    }
}

fn parse_error_to_module_error(error: ParseError) -> ModuleError {
    match error {
        ParseError::Located { path, error } => ModuleError::ParseInFile { path, error },
        error => ModuleError::Parse(error),
    }
}

fn deferred_for_known_bindings<E>(parsed: &crate::parser::File<E>) -> Vec<String> {
    let mut known = parsed
        .structural_binding_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    known.extend(parsed.variables.keys().cloned());
    known.sort_unstable();
    known.dedup();
    known
}

pub(super) fn deferred_for_body_errors<E>(parsed: &crate::parser::File<E>) -> Vec<ModuleError> {
    let known_bindings = deferred_for_known_bindings(parsed);
    parsed
        .parsed_deferred_for_expressions()
        .iter()
        .filter_map(|expression| {
            match expression.clone().into_resource_expression(&known_bindings) {
                Ok(_) => None,
                Err(error) => Some(deferred_for_resource_expression_error(error)),
            }
        })
        .collect()
}

fn resolve_modules_with_resolver_collecting<E, P: TypeResolutionPolicy>(
    parsed: &mut crate::parser::File<E>,
    resolver: &mut ModuleResolver<'_, P>,
    ref_check: &RefCheckWitness,
) -> Vec<ModuleError> {
    let module_calls = parsed.module_calls.clone();
    let signatures = resolver.resolved_module_signatures();
    let binding_index = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
        parsed,
        resolver.schemas,
        &signatures,
    );
    let resolved_bindings = crate::binding_index::ResolvedBindings::for_validation(parsed);
    let mut expanded_calls = Vec::with_capacity(module_calls.len());
    let mut errors = Vec::new();
    for call in &module_calls {
        let import_failed = parsed
            .uses
            .iter()
            .any(|import| import.alias == call.module_name)
            && !resolver.imported_modules.contains_key(&call.module_name);
        if import_failed {
            continue;
        }
        let instance_prefix = instance_prefix_for_call(call);
        match resolver.expand_module_call_checked_collecting(
            call,
            &instance_prefix,
            ref_check,
            Some(&binding_index),
            Some(&resolved_bindings),
        ) {
            // Editor/validation expansion deliberately retains a forwarded
            // parser template. Only the ordinary publication path below may
            // construct `ExpandedModuleContribution` and reject leakage.
            Ok(expanded) => expanded_calls.push(expanded),
            Err(call_errors) => errors.extend(call_errors),
        }
    }
    drop(binding_index);
    for expanded in expanded_calls {
        let contribution = crate::config_loader::relabel_export_phase(expanded);
        crate::config_loader::merge_parsed_file(parsed, contribution);
    }
    errors
}

fn resolve_modules_with_resolver<E: crate::parser::ExportParamLike, P: TypeResolutionPolicy>(
    parsed: &mut crate::parser::File<E>,
    resolver: &mut ModuleResolver<'_, P>,
    ref_check: &RefCheckWitness,
) -> Result<(), ModuleError> {
    // Expand module calls. `module_calls` is cloned because merging each
    // contribution takes a whole-`&mut parsed` borrow, which conflicts with
    // iterating `&parsed.module_calls` in place.
    let module_calls = parsed.module_calls.clone();
    let signatures = resolver.resolved_module_signatures();
    let binding_index = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
        parsed,
        resolver.schemas,
        &signatures,
    );
    let resolved_bindings = crate::binding_index::ResolvedBindings::for_validation(parsed);
    let mut expanded_calls = Vec::with_capacity(module_calls.len());
    for call in &module_calls {
        let instance_prefix = instance_prefix_for_call(call);
        let expanded = resolver.expand_module_call_checked(
            call,
            &instance_prefix,
            ref_check,
            Some(&binding_index),
            Some(&resolved_bindings),
        )?;
        let expanded = ExpandedModuleContribution::try_new(expanded)?;
        // `expand_module_call` returns a parser-phase `ParsedFile`;
        // this caller's `parsed` is a generic `File<E>` (the CLI
        // validate path passes an `InferredFile`). Relabel the
        // export-param-free contribution to the target phase, then
        // fold it in via the one shared merge surface so no field can
        // be silently dropped here (carina#3126 / carina#3061).
        expanded_calls.push(expanded);
    }
    drop(binding_index);
    for expanded in expanded_calls {
        expanded.merge_into(parsed);
    }

    ensure_expanded_root_graph(parsed)
}

#[cfg(test)]
mod type_resolution_policy_tests {
    use super::*;

    fn assert_strict(_: &ModuleResolver<'_, StrictTypeResolution>) {}

    fn assert_upstream(_: &ModuleResolver<'_, DeferredUpstreamTypeResolution>) {}

    #[test]
    fn resolver_constructors_choose_distinct_type_resolution_policies() {
        let config = ProviderContext::default();
        let schemas = SchemaRegistry::new();

        let strict = ModuleResolver::with_providerless_config_and_schemas(".", &config, &schemas);
        let upstream = ModuleResolver::<DeferredUpstreamTypeResolution>::for_providerless_upstream(
            ".", &config, &schemas,
        );

        assert_strict(&strict);
        assert_upstream(&upstream);
    }

    #[test]
    fn non_call_reference_error_is_retained_without_module_call_routing() {
        let error = ResidualModuleResolutionError::new(
            ModuleError::ReferenceType(
                crate::validation::ExpansionBoundaryReferenceErrors::function_for_test(
                    "module call 'is only user-authored diagnostic text'",
                ),
            ),
            &[],
            &ProviderContext::default(),
        )
        .expect("a non-call reference error remains residual");

        assert!(
            error.diagnostic().is_none(),
            "typed routing must not classify user-authored text as a module-call error",
        );
        assert!(
            error
                .display_relative_to(std::path::Path::new("."))
                .contains("module call 'is only user-authored diagnostic text'"),
            "the uncovered non-call reference error must remain renderable",
        );
    }
}
