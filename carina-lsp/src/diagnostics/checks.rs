//! Semantic checks: provider region, module calls, unused bindings, undefined references.

use carina_core::parser::ExportParamLike;
use carina_core::validation::admission::configuration::backend::{
    self as backend_admission, BackendAttributeError, BackendConfigError,
};
use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::DiagnosticDocument as Document;
use carina_core::binding_index::{
    BindingIndex, RefTargetKind, RefTypeError, ResolvedBindings as GenericResolvedBindings,
};
use carina_core::config_loader::{DirectoryParseResult, DuplicateDeclaration};
use carina_core::parser::{
    AttributeParameter, ParsedExportParam, ParsedFile, ParserPlaceholderBoundary,
};
use carina_core::resource::{
    BindingAddress, ConcreteValue, DeferredValue, ParserPlaceholder, RelativeBindingName,
    Value as GenericValue,
};
use carina_core::schema::suggest_similar_name;
use carina_core::upstream_exports::UpstreamRefDiagnostic;
use carina_core::validation::typecheck;

use super::{
    DiagnosticRun, ProviderValidationDeduplicator, render_analysis_invariant, render_merged,
    render_parse,
};

type Value = GenericValue<ParserPlaceholder, RelativeBindingName>;
type ResolvedBindings = GenericResolvedBindings<ParserPlaceholder, RelativeBindingName>;

/// Directory parse plus the typed module-resolution outcome.
pub(super) struct MergedParseResult {
    pub(super) analysis_errors: Vec<carina_core::parser::AnalysisTreeError>,
    pub(super) authored_module_findings: Vec<carina_core::validation::ModuleCallFinding>,
    pub(super) directory: std::sync::Arc<DirectoryParseResult>,
    pub(super) module_resolution: MergedModuleResolution,
}

pub(super) enum MergedModuleResolution {
    Unanalyzed,
    Expanded {
        graph: Box<carina_core::parser::ModuleResolvedParsedFile>,
    },
    ConstraintFailed {
        findings: Vec<carina_core::validation::ModuleCallFinding>,
        errors: Vec<carina_core::module_resolver::ResidualModuleResolutionError>,
    },
    Unowned {
        errors: Vec<carina_core::module_resolver::ResidualModuleResolutionError>,
    },
}

#[derive(Clone, Copy)]
pub(super) enum ExpansionBoundaryScope {
    All,
    Authored,
    Unlocated,
}

impl ExpansionBoundaryScope {
    fn includes(self, placeholder: &ParserPlaceholder) -> bool {
        match self {
            Self::All => true,
            Self::Authored => placeholder.authored_location().is_some(),
            Self::Unlocated => placeholder.authored_location().is_none(),
        }
    }
}

fn value_depends_on_module_output(
    value: &Value,
    module_bindings: &HashSet<RelativeBindingName>,
    variables: &indexmap::IndexMap<String, Value>,
    visiting_variables: &mut HashSet<RelativeBindingName>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(|value| {
                value_depends_on_module_output(
                    value,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            if module_bindings.contains(path.binding()) {
                return true;
            }
            let binding = path.binding_address();
            visiting_variables.insert(binding.clone())
                && variables.get(binding.as_str()).is_some_and(|value| {
                    value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
                })
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            if module_bindings.contains(binding) {
                return true;
            }
            visiting_variables.insert(binding.clone())
                && variables.get(binding.as_str()).is_some_and(|value| {
                    value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
                })
        }
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Concrete(ConcreteValue::Map(entries)) => entries.values().any(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                carina_core::resource::InterpolationPart::Expr(value)
                    if value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
            )
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().any(|argument| {
            argument.value().is_some_and(|value| {
                value_depends_on_module_output(
                    value,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
            })
        }),
        Value::Deferred(DeferredValue::Access(access)) => value_depends_on_module_output(
            access.base(),
            module_bindings,
            variables,
            visiting_variables,
        ),
        Value::Deferred(DeferredValue::Secret(inner)) => inner.visit(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
                || value_depends_on_module_output(
                    default,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            value_depends_on_module_output(
                expression.lhs(),
                module_bindings,
                variables,
                visiting_variables,
            ) || value_depends_on_module_output(
                expression.rhs(),
                module_bindings,
                variables,
                visiting_variables,
            )
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(DeferredValue::Unknown(_) | DeferredValue::Placeholder(_)) => false,
    }
}

/// Semantic values available to source-local diagnostics.
///
/// A standalone buffer owns its parsed values. Directory-backed analysis must
/// use the merged parse because only it classifies sibling bindings correctly;
/// if that parse or the current file identity is unavailable, only values
/// proven independent of sibling names may fall back to the buffer parse.
pub(super) enum SemanticView<'a> {
    Standalone(&'a ParsedFile),
    DirectoryParsed { merged: &'a ParsedFile },
    DirectoryUnavailable,
}

impl<'a> SemanticView<'a> {
    pub(super) fn for_analysis(
        authored: &'a ParsedFile,
        merged: Option<&'a ParsedFile>,
        directory_backed: bool,
    ) -> Self {
        if !directory_backed {
            return Self::Standalone(authored);
        }
        match merged {
            Some(merged) => Self::DirectoryParsed { merged },
            None => Self::DirectoryUnavailable,
        }
    }

    fn lookup<'view, T>(&'view self, authored: &'view T) -> Option<&'view T>
    where
        T: SemanticLookup,
    {
        match self {
            Self::Standalone(_) => Some(authored),
            Self::DirectoryParsed { merged } => authored.lookup_in(merged),
            Self::DirectoryUnavailable => None,
        }
    }

    /// Select the only value that may be fed to the shared type checker.
    /// Directory-backed analysis prefers the merged semantic value. If the
    /// directory is unavailable, the authored value is admitted only through
    /// `ContextFreeValue`, which proves it contains no name that a sibling file
    /// could reclassify.
    fn value_for_typecheck<'view, T>(&'view self, authored: &'view T) -> Option<&'view Value>
    where
        T: SemanticLookup,
    {
        match self {
            Self::Standalone(_parsed) => authored.value(),
            Self::DirectoryParsed { merged } => {
                authored.lookup_in(merged).and_then(SemanticLookup::value)
            }
            Self::DirectoryUnavailable => authored
                .value()
                .and_then(ContextFreeValue::new)
                .map(ContextFreeValue::get),
        }
    }
}

/// Proof that a buffer-only value cannot acquire different semantics from a
/// sibling declaration. Construction is private and recursive, so callers
/// cannot bypass the fallback policy with an unchecked `&Value`.
struct ContextFreeValue<'a>(&'a Value);

impl<'a> ContextFreeValue<'a> {
    fn new(value: &'a Value) -> Option<Self> {
        fn is_context_free(value: &Value) -> bool {
            match value {
                Value::Concrete(ConcreteValue::String(_))
                | Value::Concrete(ConcreteValue::CanonicalEnum(_))
                | Value::Concrete(ConcreteValue::Int(_))
                | Value::Concrete(ConcreteValue::Float(_))
                | Value::Concrete(ConcreteValue::Bool(_))
                | Value::Concrete(ConcreteValue::Duration(_))
                | Value::Concrete(ConcreteValue::StringList(_)) => true,
                // EnumIdentifier is the parser's fallback for an unresolved
                // bare/dotted name, so even a dotted spelling needs directory
                // authority before it is safe to type-check.
                Value::Concrete(ConcreteValue::EnumIdentifier(_))
                | Value::Deferred(DeferredValue::ModuleIf(_))
                | Value::Deferred(DeferredValue::ResourceRef { .. })
                | Value::Deferred(DeferredValue::BindingRef { .. })
                | Value::Deferred(DeferredValue::FunctionCall { .. })
                | Value::Deferred(DeferredValue::Unavailable { .. })
                | Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
                | Value::Deferred(DeferredValue::Access(_))
                | Value::Deferred(DeferredValue::Unknown(_))
                | Value::Deferred(DeferredValue::Placeholder(_))
                 => false,
                Value::Concrete(ConcreteValue::List(items)) => {
                    items.iter().all(is_context_free)
                }
                Value::Concrete(ConcreteValue::Map(entries)) => {
                    entries.values().all(is_context_free)
                }
                Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().all(|part| {
                    matches!(part, carina_core::resource::InterpolationPart::Literal(_))
                        || matches!(part, carina_core::resource::InterpolationPart::Expr(value) if is_context_free(value))
                }),
                Value::Deferred(DeferredValue::Secret(inner)) => inner.visit(is_context_free),
                Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                    is_context_free(value) && is_context_free(default)
                }
                Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
                    is_context_free(expression.lhs()) && is_context_free(expression.rhs())
                }
            }
        }

        is_context_free(value).then_some(Self(value))
    }

    fn get(self) -> &'a Value {
        self.0
    }
}

trait SemanticLookup: Sized {
    fn lookup_in<'a, D>(
        &self,
        merged: &'a carina_core::parser::File<
            ParsedExportParam,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
        >,
    ) -> Option<&'a Self>
    where
        D: carina_core::parser::DeferredForPhaseTraversal;

    fn value(&self) -> Option<&Value>;
}

impl SemanticLookup for AttributeParameter {
    fn lookup_in<'a, D>(
        &self,
        merged: &'a carina_core::parser::File<
            ParsedExportParam,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
        >,
    ) -> Option<&'a Self>
    where
        D: carina_core::parser::DeferredForPhaseTraversal,
    {
        let authored_span = self.source_span();
        let authored_file = self.source_file();
        merged.attribute_params.iter().find(|candidate| {
            candidate.source_file() == authored_file && candidate.source_span() == authored_span
        })
    }

    fn value(&self) -> Option<&Value> {
        self.value
            .as_ref()
            .and_then(carina_core::parser::BoundaryExpression::value)
    }
}

impl SemanticLookup for ParsedExportParam {
    fn lookup_in<'a, D>(
        &self,
        merged: &'a carina_core::parser::File<
            ParsedExportParam,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
        >,
    ) -> Option<&'a Self>
    where
        D: carina_core::parser::DeferredForPhaseTraversal,
    {
        let authored_span = self.source_span();
        let authored_file = self.source_file();
        merged.export_params.iter().find(|candidate| {
            candidate.source_file() == authored_file && candidate.source_span() == authored_span
        })
    }

    fn value(&self) -> Option<&Value> {
        self.value
            .as_ref()
            .and_then(carina_core::parser::BoundaryExpression::value)
    }
}

/// Locate the `source = '<expected>'` or `source = "<expected>"` line inside
/// an `upstream_state { ... }` block whose value equals `expected`. Returns
/// `(line, start_col, end_col)` in character columns, positioned over the
/// inner value (quotes excluded).
///
/// Restricting to `upstream_state` blocks avoids false matches against
/// `provider` / `module` blocks that also take a `source` attribute.
fn contains_matching_function_call(value: &Value, matches_name: &impl Fn(&str) -> bool) -> bool {
    match value {
        Value::Deferred(DeferredValue::FunctionCall { name, args, .. }) => {
            matches_name(name)
                || args.iter().any(|arg| {
                    arg.value()
                        .is_some_and(|value| contains_matching_function_call(value, matches_name))
                })
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
            matches_name(&function.name)
                || args.iter().any(|arg| {
                    arg.value()
                        .is_some_and(|value| contains_matching_function_call(value, matches_name))
                })
        }
        Value::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .any(|item| contains_matching_function_call(item, matches_name)),
        Value::Concrete(ConcreteValue::Map(entries)) => entries
            .values()
            .any(|entry| contains_matching_function_call(entry, matches_name)),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                carina_core::resource::InterpolationPart::Expr(value)
                    if contains_matching_function_call(value, matches_name)
            )
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            inner.visit(|value| contains_matching_function_call(value, matches_name))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_matching_function_call(value, matches_name)
                || contains_matching_function_call(default, matches_name)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_matching_function_call(expression.lhs(), matches_name)
                || contains_matching_function_call(expression.rhs(), matches_name)
        }
        _ => false,
    }
}

/// Whether `deferred` was parsed from the editor's current document.
/// `DeferredForExpression.file` is stamped with the full source path, so we
/// compare by basename to the LSP-supplied `current_file_name`.
fn collect_enum_provider_namespaces(
    attr_type: &carina_core::schema::CompiledAttributeType,
    namespaces: &mut HashSet<String>,
) {
    use carina_core::schema::RawShape;

    match attr_type.raw_shape() {
        RawShape::UnloadedSchemaType(_) => {}
        RawShape::Enum { identity, .. } => {
            if let Some(provider) = &identity.provider {
                namespaces.insert(provider.clone());
            }
        }
        RawShape::List { element_type, .. } => {
            collect_enum_provider_namespaces(element_type, namespaces);
        }
        RawShape::Map {
            key,
            value,
            constraints: _,
        } => {
            collect_enum_provider_namespaces(key, namespaces);
            collect_enum_provider_namespaces(value, namespaces);
        }
        RawShape::Struct { fields, .. } => {
            for field in fields {
                collect_enum_provider_namespaces(field.field_type(), namespaces);
            }
        }
        RawShape::Union(members) => {
            for member in members {
                collect_enum_provider_namespaces(member, namespaces);
            }
        }
        RawShape::Reference { .. }
        | RawShape::String { .. }
        | RawShape::Int { .. }
        | RawShape::Float { .. }
        | RawShape::Bool
        | RawShape::Duration
        | RawShape::Ref(_) => {}
    }
}

type AnonymousIdentityGraph = (
    Vec<carina_core::resource::Resource<carina_core::resource::ModuleSource, ParserPlaceholder>>,
    Vec<
        carina_core::parser::ProviderConfig<
            ParserPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
        >,
    >,
);

fn anonymous_identity_graph<E: Clone, D>(
    parsed: &carina_core::parser::File<
        E,
        ParserPlaceholder,
        ParserPlaceholder,
        D,
        carina_core::resource::AbsoluteBindingAddress,
    >,
    schemas: &carina_core::schema::SchemaRegistry,
) -> Option<AnonymousIdentityGraph>
where
    D: Clone
        + carina_core::parser::DeferredForPhaseTraversal<
            ParserPlaceholder,
            ParserPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
        >,
{
    let mut identity_graph = parsed.clone();
    carina_core::schema::resolve_file_block_names_with_schemas(&mut identity_graph, schemas)
        .ok()?;
    Some((identity_graph.resources, identity_graph.providers))
}

impl DiagnosticRun<'_> {
    /// Report anonymous resources that would share an identity scheme derived
    /// from mutable attributes. The core check owns classification and
    /// grouping; this adapter only maps the directory-wide result back onto
    /// declarations in the current document.
    pub(super) fn attribute_derived_anonymous_resource_diagnostics(
        &self,
        doc: &Document,
        directory: &ParsedFile,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        let conflicts =
            carina_core::identifier::check_attribute_derived_anonymous_resource_conflicts(
                &directory.resources,
                &self.schemas,
            );
        if conflicts.is_empty() {
            return Vec::new();
        }

        carina_core::identifier::attribute_derived_conflict_diagnostics(
            tree,
            &conflicts,
            &self.schemas,
        )
        .iter()
        .filter_map(|finding| {
            render_merged(
                doc,
                finding,
                DiagnosticSeverity::ERROR,
                finding.detail().to_string(),
            )
        })
        .collect()
    }

    /// Run the same typed anonymous-identity checks as validate/plan and anchor
    /// messages on the authored resource, loop, or state-block destination.
    pub(super) fn loop_anonymous_identity_diagnostics(
        &self,
        doc: &Document,
        current: (
            &carina_core::parser::RootBindingResolvedParsedFile,
            &super::RootAnalysis<'_>,
        ),
        directory: (
            &carina_core::parser::RootBindingResolvedParsedFile,
            &super::RootAnalysis<'_>,
        ),
        expanded: Option<(
            &carina_core::parser::ModuleResolvedParsedFile,
            &super::ExpandedAnalysis<'_>,
        )>,
        configuration_root: Option<&std::path::Path>,
    ) -> Vec<Diagnostic> {
        let (current_file, current_tree) = current;
        let (directory, directory_tree) = directory;
        let expanded_tree = expanded.map(|(_, tree)| tree);
        let expanded = expanded.map(|(file, _)| file);
        let graph = if let Some(expanded) = expanded {
            anonymous_identity_graph(expanded, &self.schemas)
        } else {
            anonymous_identity_graph(directory, &self.schemas)
        };
        let Some((mut resources, providers)) = graph else {
            return Vec::new();
        };
        let canonical_resources = match carina_core::value::canonicalize_resources_with_schemas(
            &mut resources,
            &self.schemas,
        ) {
            Ok(resources) => resources,
            Err(_) => return Vec::new(),
        };
        let identity_result = carina_core::identifier::compute_anonymous_identifiers_with_factories(
            canonical_resources,
            &providers,
            &self.factories,
            &self.schemas,
            carina_core::identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        );
        let root = configuration_root
            .unwrap_or_else(|| doc.source_file().parent().expect("document directory"));
        let mut diagnostics = Vec::new();
        if let Err(carina_core::identifier::AnonymousIdentityComputationError::Identity(error)) =
            identity_result
        {
            let findings = if let Some(tree) = expanded_tree {
                carina_core::identifier::anonymous_identity_diagnostics(tree, error, root)
            } else {
                carina_core::identifier::anonymous_identity_diagnostics(directory_tree, error, root)
            };
            diagnostics.extend(findings.iter().filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().clone(),
                )
            }));
        }
        let errors = carina_core::identifier::check_legacy_loop_move_targets(
            &current_file.state_blocks,
            &resources,
        );
        diagnostics.extend(
            carina_core::identifier::legacy_loop_move_target_diagnostics(current_tree, errors)
                .iter()
                .filter_map(|finding| {
                    render_merged(
                        doc,
                        finding,
                        DiagnosticSeverity::ERROR,
                        finding.detail().to_string(),
                    )
                }),
        );

        diagnostics
    }

    pub(super) fn check_configuration_placement(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::configuration_placement_diagnostics(tree)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect()
    }

    /// Validate the current file's backend block through the same built-in
    /// schema used by CLI validation and runtime backend construction.
    pub(super) fn check_backend_config(
        &self,
        doc: &Document,
        merged: Option<&MergedParseResult>,
        tree: Option<&carina_core::parser::AnalysisTree<'_>>,
    ) -> Vec<Diagnostic> {
        let (Some(merged), Some(tree)) = (merged, tree) else {
            return Vec::new();
        };
        // CLI validation reports only the structural backend-in-module error
        // for a module-definition directory. Keep editor diagnostics at the
        // same layer instead of adding backend-schema errors.
        if carina_core::validation::is_module_definition(&merged.directory.parsed) {
            return Vec::new();
        }
        // A duplicate backend has no single semantic owner. The parser
        // inventory still supplies every declaration for placement checks.
        if merged.directory.sole_backend_file.is_none() {
            return Vec::new();
        }
        let Some(backend) = merged.directory.parsed.backend.as_ref() else {
            return Vec::new();
        };
        let Err(error) = backend_admission::admit(backend) else {
            return Vec::new();
        };

        let attribute_diagnostic = |error: &BackendAttributeError| {
            // Directory resolution deliberately retains user-function calls
            // that failed evaluation so other editor checks can continue.
            // Suppress only those calls, including nested ones. A deferred
            // builtin with unresolved arguments is still a genuine value-type
            // mismatch and must remain visible, matching CLI validation.
            if backend
                .attributes
                .get(error.attribute())
                .is_some_and(|value| {
                    contains_matching_function_call(value, &|name| {
                        merged.directory.parsed.user_functions.contains_key(name)
                    })
                })
            {
                return None;
            }

            let finding = carina_core::parser::Located::new(
                tree.backend_attribute_location(error.attribute()).clone(),
                error,
            );
            render_merged(doc, &finding, DiagnosticSeverity::ERROR, error.to_string())
        };

        match error {
            BackendConfigError::Invalid(errors) => errors
                .errors()
                .iter()
                .filter_map(attribute_diagnostic)
                .collect(),
            unsupported @ BackendConfigError::UnsupportedBackend(_) => {
                let finding = carina_core::parser::Located::new(
                    tree.backend_location()
                        .expect("backend declaration provenance")
                        .clone(),
                    unsupported,
                );
                render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
                .into_iter()
                .collect()
            }
        }
    }

    /// Check expanded values while retaining the authored provider locations.
    pub(super) fn check_provider_default_tags<B: BindingAddress>(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        providers: &[carina_core::parser::ProviderConfig<ParserPlaceholder, B>],
        bindings: &GenericResolvedBindings<ParserPlaceholder, B>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let upstream = tree
            .upstream_states()
            .iter()
            .map(|upstream| upstream.detail().binding.as_str().to_owned())
            .collect();
        for provider in providers {
            let Some(index) = tree.providers().iter().position(|authored| {
                authored.name == provider.name && authored.binding == provider.binding
            }) else {
                continue;
            };
            if let Err(error) =
                carina_core::parser::ResolvedProviderConfigs::validate_default_tag_knownness(
                    std::slice::from_ref(provider),
                    &upstream,
                    bindings,
                    self.provider_context,
                )
            {
                for error in error.into_findings() {
                    let finding = carina_core::parser::Located::new(
                        tree.provider_attribute_location(
                            index,
                            carina_core::parser::DEFAULT_TAGS_ATTRIBUTE,
                        )
                        .clone(),
                        error,
                    );
                    diagnostics.extend(render_merged(
                        doc,
                        &finding,
                        DiagnosticSeverity::ERROR,
                        finding.detail().to_string(),
                    ));
                }
            }
        }
        diagnostics
    }

    /// Validate provider configuration using its parser-owned header and keys.
    pub(super) fn check_provider_region(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        deduplicator: &ProviderValidationDeduplicator,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (index, provider) in tree.providers().iter().enumerate() {
            let Some(factory) = self.factories.iter().find(|f| f.name() == provider.name) else {
                continue;
            };
            let validation = carina_core::executor::provider_ready::validate_parser_provider_config(
                factory.as_ref(),
                &provider.name,
                &provider.attributes,
            );
            if let Err(error) = validation {
                if error.provider_error().is_some_and(|provider_error| {
                    !deduplicator.should_emit(&provider.name, provider_error)
                }) {
                    continue;
                }
                let location = error
                    .attribute()
                    .map(|key| tree.provider_attribute_location(index, key))
                    .unwrap_or_else(|| tree.provider_attribute_location(index, "region"));
                let finding = carina_core::parser::Located::new(location.clone(), error);
                diagnostics.extend(render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::WARNING,
                    finding.detail().to_string(),
                ));
            }
        }
        diagnostics
    }

    pub(super) fn check_unloaded_providers(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        tree.providers()
            .iter()
            .enumerate()
            .filter_map(|(index, provider)| {
                let reason = self.provider_errors.get(&provider.name)?;
                let finding = carina_core::parser::Located::new(
                    tree.provider_location(index).clone(),
                    format!("Provider '{}' is not loaded: {}", provider.name, reason),
                );
                render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::INFORMATION,
                    finding.detail().clone(),
                )
            })
            .collect()
    }

    /// Run a directory-scoped parse with the current editor buffer substituted
    /// for its on-disk copy, so diagnostics that need cross-file context
    /// (upstream-state exports, for-iterable bindings) update on keystrokes.
    ///
    /// Module analysis follows after sealing and upstream type resolution.
    pub(super) fn parse_merged_with_buffer(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &carina_core::paths::ResolvedPath,
    ) -> Result<MergedParseResult, carina_core::config_loader::DirectoryParseFailure> {
        let mut overrides: HashMap<String, String> = HashMap::new();
        if let Some(name) = current_file_name {
            overrides.insert(name.to_string(), doc.text());
        }
        let result = carina_core::config_loader::parse_directory_with_overrides_and_diagnostics_with_schemas(
            base_path,
            self.provider_context.catalog(),
            &overrides,
            &self.schemas,
        )?;
        Ok(MergedParseResult {
            directory: std::sync::Arc::new(result),
            module_resolution: MergedModuleResolution::Unanalyzed,
            analysis_errors: Vec::new(),
            authored_module_findings: Vec::new(),
        })
    }

    /// Compute the canonical authored findings once, then reuse them to claim
    /// resolver backstops and to publish source-owned diagnostics.
    pub(super) fn resolve_merged_modules(
        &self,
        result: &mut MergedParseResult,
        tree: &carina_core::parser::AnalysisTree<'_>,
        base_path: &carina_core::paths::ResolvedPath,
        upstream_exports: &carina_core::upstream_exports::UpstreamExports,
        source_overrides: &carina_core::config_loader::SourceOverrides,
    ) {
        let covered_findings = match carina_core::validation::check_module_calls_from_analysis(
            tree,
            base_path,
            self.provider_context,
            &self.schemas,
            upstream_exports,
            source_overrides,
        ) {
            Ok(findings) => findings,
            Err(error) => {
                result.analysis_errors.push(error);
                return;
            }
        };
        // Module expansion is a no-op for configs without `module_call`.
        // Failures retain the authored parse for source-level checks; only the
        // successful outcome can replace it with the expanded graph.
        let module_resolution =
            carina_core::module_resolver::resolve_modules_with_diagnostics_and_overrides(
                &result.directory.parsed,
                base_path,
                self.provider_context,
                &self.schemas,
                &covered_findings,
                source_overrides,
            );
        let module_resolution = match module_resolution {
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::Expanded {
                mut graph,
            } => {
                let _ = carina_core::parser::resolve_expanded_provider_unresolved_attributes(
                    &mut graph,
                    self.provider_context.catalog(),
                );
                let _ = carina_core::parser::finalize_provider_configs(&mut graph);
                MergedModuleResolution::Expanded { graph }
            }
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::ConstraintFailed {
                findings,
                errors,
            } => MergedModuleResolution::ConstraintFailed { findings, errors },
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::Unowned { errors } => {
                MergedModuleResolution::Unowned { errors }
            }
        };
        result.module_resolution = module_resolution;
        result.authored_module_findings = covered_findings;
    }

    #[cfg(test)]
    pub(super) fn parse_and_resolve_merged_with_buffer(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &carina_core::paths::ResolvedPath,
    ) -> Result<MergedParseResult, carina_core::config_loader::DirectoryParseFailure> {
        let mut result = self.parse_merged_with_buffer(doc, current_file_name, base_path)?;
        let directory = std::sync::Arc::clone(&result.directory);
        let overrides = carina_core::config_loader::SourceOverrides::from([(
            doc.source_file().to_path_buf(),
            doc.text(),
        )]);
        match carina_core::parser::AnalysisTree::try_from_file(directory.parsed.parsed()) {
            Ok(tree) => {
                match self.resolve_diagnostic_upstream_exports(base_path, &tree, &overrides) {
                    Ok((exports, _)) => self.resolve_merged_modules(
                        &mut result,
                        &tree,
                        base_path,
                        &exports,
                        &overrides,
                    ),
                    Err(error) => result.analysis_errors.push(error),
                }
            }
            Err(error) => result.analysis_errors.push(error),
        }
        Ok(result)
    }

    pub(super) fn module_resolver_error_diagnostics(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        residual: &carina_core::module_resolver::ResidualModuleResolutionError,
        base: &carina_core::paths::ResolvedPath,
    ) -> Vec<Diagnostic> {
        if let Some(error) = residual.analysis_invariant() {
            return render_analysis_invariant(doc, error).into_iter().collect();
        }
        let message = residual.display_relative_to(&base.source_label());
        let element = residual.element_diagnostic();
        let mut diagnostics = element
            .and_then(|finding| {
                render_merged(doc, finding, DiagnosticSeverity::ERROR, message.clone())
            })
            .into_iter()
            .collect::<Vec<_>>();
        if matches!(
            residual.call_source(),
            carina_core::module_resolver::ModuleErrorCallSource::Unavailable
        ) && element.is_none()
            && let Some(finding) = residual.file_diagnostic(base)
        {
            return render_parse(doc, &finding, DiagnosticSeverity::ERROR, message)
                .into_iter()
                .collect();
        }
        diagnostics.extend(
            residual
                .call_diagnostics(tree)
                .iter()
                // Keep caller-owned failures, but prefer the precise semantic
                // element when a call and its error share the current file.
                .filter(|finding| {
                    element.is_none_or(|element| {
                        finding.location().file() != element.location().file()
                    })
                })
                .filter_map(|finding| {
                    render_merged(doc, finding, DiagnosticSeverity::ERROR, message.clone())
                }),
        );
        diagnostics
    }

    /// Check tag-key casing against the directory-wide CLI population while
    /// anchoring diagnostics only in the current editor buffer.
    pub(super) fn check_mixed_tag_key_styles(
        &self,
        doc: &Document,
        base_path: &carina_core::paths::ResolvedPath,
        merged_result: &DirectoryParseResult,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        // Reuse the exact root sources that fed the merged parse. The shared
        // core assembler also adds the CLI-equivalent module population.
        let tag_keys = carina_core::lint::collect_all_tag_keys(
            &merged_result.source_files,
            &merged_result.parsed,
            base_path,
        );

        carina_core::lint::located_mixed_tag_key_styles(tree, &tag_keys)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::WARNING,
                    carina_core::lint::mixed_tag_key_style_message(finding.detail()),
                )
            })
            .collect()
    }

    pub(super) fn duplicate_declaration_diagnostics(
        &self,
        doc: &Document,
        duplicates: &[DuplicateDeclaration],
    ) -> Vec<Diagnostic> {
        duplicates
            .iter()
            .flat_map(DuplicateDeclaration::diagnostics)
            .filter_map(|finding| {
                render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect()
    }

    /// Run finalize-against-a-buffer in isolation so the engine can surface
    /// `default_tags must resolve to a map` etc. as diagnostics. The merged
    /// parse + module-expansion + resolve pair is rebuilt here intentionally:
    /// `parse_merged_with_buffer` swallows the finalize error to keep
    /// identifier-scope diagnostics running, but that error must surface
    /// somewhere (#2717 / #2753).
    pub(super) fn finalize_provider_diagnostic(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &carina_core::paths::ResolvedPath,
    ) -> Option<carina_core::parser::AnalysisResolutionError> {
        let mut overrides: HashMap<String, String> = HashMap::new();
        if let Some(name) = current_file_name {
            overrides.insert(name.to_string(), doc.text());
        }
        let merged = carina_core::config_loader::parse_directory_for_metadata(
            base_path,
            self.provider_context.catalog(),
            &overrides,
            &self.schemas,
        )
        .ok()?;
        let module_resolution =
            carina_core::module_resolver::resolve_modules_with_diagnostics_and_overrides(
                &merged,
                base_path,
                self.provider_context,
                &self.schemas,
                &[],
                &carina_core::config_loader::SourceOverrides::from([(
                    doc.source_file().to_path_buf(),
                    doc.text(),
                )]),
            );
        if let carina_core::module_resolver::ModuleResolutionDiagnosticResult::Expanded { graph } =
            module_resolution
        {
            let mut resolved = *graph;
            if let Err(error) =
                carina_core::parser::resolve_expanded_provider_unresolved_attributes_for_analysis(
                    &mut resolved,
                    self.provider_context.catalog(),
                )
            {
                return Some(error);
            }
            return carina_core::parser::finalize_provider_configs_for_analysis(&mut resolved)
                .err();
        }
        let mut merged = merged.into_parsed();
        {
            // The primary diagnostics pass owns the module error. A provider
            // value that reads a failed module's output cannot be finalized
            // soundly, but unrelated provider values still can (and must) be
            // diagnosed. Remove only the dependent values before finalizing
            // the surviving, context-free subset.
            let module_bindings = merged
                .module_calls
                .iter()
                .filter_map(|call| call.binding_name.clone())
                .collect::<HashSet<_>>();
            let variables = merged.variables.clone();
            for provider in &mut merged.providers {
                provider.unresolved_attributes.retain(|_, value| {
                    !value_depends_on_module_output(
                        value,
                        &module_bindings,
                        &variables,
                        &mut HashSet::new(),
                    )
                });
            }
        }
        if let Err(e) = carina_core::parser::resolve_provider_unresolved_attributes_for_analysis(
            &mut merged,
            self.provider_context.catalog(),
        ) {
            return Some(e);
        }
        carina_core::parser::finalize_provider_configs_for_analysis(&mut merged).err()
    }

    /// Collect every binding name declared anywhere in `base_path` by
    /// parsing each sibling `.crn` independently. Used as a fallback
    /// when the full directory parse fails (`parse_merged_with_buffer`
    /// returns `None`). A per-file failure is non-fatal — that file's
    /// declarations are skipped. The current document's text replaces
    /// its on-disk copy so unsaved edits are honored.
    ///
    /// Mirrors the merge-success path's
    /// `BindingNameSet::from_parsed`, so the same eight binding kinds
    /// (resources, module-calls, upstream-states, arguments, uses,
    /// user-functions, structural, variables) stay covered when the
    /// merge fails. Without this, an unrelated parse error in one
    /// sibling redlines every cross-file binding as Unknown (#2445).
    pub(super) fn collect_sibling_binding_names(
        &self,
        buffer_text: &str,
        current_file_name: Option<&str>,
        base_path: &carina_core::paths::ResolvedPath,
    ) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(base_path) else {
            return out;
        };
        for file in files {
            let file_name = file.display().file_name().and_then(|n| n.to_str());
            let content = match (file_name, current_file_name) {
                (Some(name), Some(current)) if name == current => buffer_text.to_string(),
                _ => match file.read_to_string() {
                    Ok(text) => text,
                    Err(_) => continue,
                },
            };
            let Ok(parsed) = carina_core::parser::parse_file(
                &content,
                &file.source_label(),
                self.provider_context.catalog(),
            ) else {
                continue;
            };
            out.extend(
                carina_core::binding_index::BindingNameSet::from_parsed(&parsed)
                    .iter_names()
                    .map(String::from),
            );
        }
        out
    }

    /// Reject references like `orgs.account` whose field isn't declared by
    /// the upstream's `exports { }` block.
    ///
    /// Runs even when the single-file parse fails — common when a `for`
    /// iterates over a binding declared in a sibling file.
    pub(super) fn check_upstream_state_field_references(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        exports: &carina_core::upstream_exports::UpstreamExports,
        resolve_errors: &[carina_core::parser::Located<
            carina_core::upstream_exports::UpstreamResolveError,
        >],
    ) -> Vec<Diagnostic> {
        let mut diagnostics: Vec<_> = resolve_errors
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect();
        macro_rules! render_references {
            ($check:ident) => {
                diagnostics.extend(
                    carina_core::upstream_exports::$check(tree, exports)
                        .iter()
                        .filter_map(|finding| {
                            render_merged(
                                doc,
                                finding,
                                DiagnosticSeverity::ERROR,
                                finding.detail().diagnostic_message(),
                            )
                        }),
                );
            };
        }
        render_references!(check_upstream_state_untyped_field_references);
        render_references!(check_upstream_state_for_iterable_shapes);
        render_references!(check_upstream_state_attribute_access_shape_fallbacks);
        render_references!(check_upstream_state_subscript_shapes);
        diagnostics
    }

    pub(super) fn check_for_iterable_bindings(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics: Vec<_> = carina_core::validation::validate_for_iterable_bindings(tree)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect();
        diagnostics.extend(
            carina_core::validation::validate_deferred_for_iterable_types(
                tree,
                &self.schemas,
                self.provider_context.catalog(),
            )
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            }),
        );
        diagnostics
    }

    pub(super) fn check_upstream_state_sources(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        base: &carina_core::paths::ResolvedPath,
    ) -> Vec<Diagnostic> {
        carina_core::upstream_exports::check_upstream_state_sources(base, tree)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect()
    }

    pub(super) fn check_user_function_ref_types(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        binding_index: &BindingIndex<'_>,
        report: &carina_core::parser::retained::ExpressionReport,
    ) -> Vec<Diagnostic> {
        carina_core::validation::user_function_reference_diagnostics(tree, binding_index)
            .iter()
            .filter(|finding| match report.expression(finding.location()) {
                Ok(expression) => {
                    expression.check_owner()
                        != carina_core::parser::retained::ExpressionCheckOwner::Reported
                }
                Err(_) => true,
            })
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::WARNING,
                    finding.detail().clone(),
                )
            })
            .collect()
    }

    pub(super) fn unused_binding_diagnostics(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        names: impl IntoIterator<Item = String>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::locate_unused_bindings(tree, names)
            .iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    finding,
                    DiagnosticSeverity::WARNING,
                    carina_core::validation::unused_binding_message(finding.detail()),
                )
            })
            .collect()
    }

    /// Extract resource binding names from `src` (variables defined with
    /// `let binding_name = aws...` or `let binding_name = read aws...`).
    /// See [`DslSource`] for the explicit buffer-vs-directory choice.
    pub(super) fn extract_resource_bindings(
        &self,
        src: crate::completion::DslSource<'_>,
    ) -> HashSet<String> {
        let mut bindings = HashSet::new();
        let text = src.merged_text();
        for line in text.lines() {
            if let Some((name, _)) = crate::let_parse::parse_let_header(line) {
                bindings.insert(name.to_string());
            }
        }
        bindings
    }

    /// Extract provider names declared with `provider NAME {` in the current
    /// document text. Used to treat an unresolved `NAME.` prefix as a provider
    /// namespace rather than an undefined `let` binding when the provider is
    /// declared but not yet downloaded (issue #2019).
    pub(super) fn extract_declared_provider_names(&self, text: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("provider") else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(|c: char| c.is_ascii_whitespace()) else {
                continue;
            };
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if name_end == 0 {
                continue;
            }
            let name = &rest[..name_end];
            let after_name = rest[name_end..].trim_start();
            if after_name.starts_with('{') {
                names.insert(name.to_string());
            }
        }
        names
    }

    /// Check attributes blocks for type mismatches and undefined binding references.
    /// Buffer parameters are iterated while directory-wide context provides binding authority.
    pub(super) fn check_attributes_blocks(
        &self,
        doc: &Document,
        authored: (&ParsedFile, &carina_core::parser::AnalysisTree<'_>),
        semantic_view: &SemanticView<'_>,
        binding_index: &BindingIndex<'_>,
        known_bindings: &HashSet<String>,
        _inference_bindings: &carina_core::validation::inference::InferenceBindings<
            ParserPlaceholder,
        >,
    ) -> Vec<Diagnostic> {
        let (parsed, tree) = authored;
        let mut diagnostics = Vec::new();
        let record_for = |parameter: &AttributeParameter| {
            tree.syntax()
                .declarations()
                .iter()
                .find(|record| {
                    record.kind() == carina_core::config_loader::DeclarationKind::ModuleAttribute
                        && record.name() == parameter.name
                        && record.location().file().path() == parameter.source_file()
                        && parameter.source_span().start_byte()
                            <= record.location().span().start_byte()
                        && record.location().span().end_byte() <= parameter.source_span().end_byte()
                })
                .expect("module attribute name validated at analysis boundary")
        };
        let semantic_params = parsed
            .attribute_params
            .iter()
            .map(|parameter| semantic_view.lookup(parameter))
            .collect::<Vec<_>>();

        for attr_param in &parsed.attribute_params {
            if let Some(value) = attr_param
                .value
                .as_ref()
                .and_then(carina_core::parser::BoundaryExpression::value)
            {
                // Check for undefined binding references in parser-native shapes
                // (`Value::Deferred(DeferredValue::ResourceRef)` for `name.attr`, `Value::Deferred(DeferredValue::BindingRef)`
                // for bare `name`, since #2847).
                let undefined_binding = match semantic_view
                    .value_for_typecheck(attr_param)
                    .unwrap_or(value)
                {
                    Value::Deferred(DeferredValue::ResourceRef { path })
                        if !known_bindings.contains(path.binding()) =>
                    {
                        Some(path.binding().to_string())
                    }
                    Value::Deferred(DeferredValue::BindingRef { binding })
                        if !known_bindings.contains(binding.as_str()) =>
                    {
                        Some(binding.to_string())
                    }
                    // A single-file parse cannot classify an undeclared bare
                    // identifier as a binding, so untyped attribute values use
                    // the parser's enum-identifier fallback until directory
                    // binding authority is available here.
                    Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                        if attr_param.static_type.is_none()
                            && !identifier.as_str().contains('.')
                            && !known_bindings.contains(identifier.as_str()) =>
                    {
                        Some(identifier.to_string())
                    }
                    _ => None,
                };
                if let Some(undefined) = undefined_binding {
                    let finding = carina_core::parser::Located::new(
                        record_for(attr_param)
                            .value_location()
                            .expect("module attribute value provenance")
                            .clone(),
                        format!(
                            "Undefined resource '{}' in attributes '{}'. Define it with 'let {} = ...'",
                            undefined, attr_param.name, undefined
                        ),
                    );
                    diagnostics.extend(render_merged(
                        doc,
                        &finding,
                        DiagnosticSeverity::ERROR,
                        finding.detail().clone(),
                    ));
                }

                if let Some(type_expr) = &attr_param.static_type
                    && let Some(semantic_value) = semantic_view.value_for_typecheck(attr_param)
                {
                    // Declaration resolution owns an unnameable annotation. Value
                    // checking starts only after that producer can classify it.
                    let Ok(resolved_type) = carina_core::validation::resolve_type_expr(
                        type_expr,
                        self.provider_context.catalog(),
                    ) else {
                        continue;
                    };
                    let type_expr = &resolved_type;
                    let expression = semantic_view
                        .lookup(attr_param)
                        .unwrap_or(attr_param)
                        .judgement();
                    let expression = match expression {
                        Ok(expression) => expression,
                        Err(error) => {
                            let finding = carina_core::parser::Located::new(
                                attr_param.type_check_location().clone(),
                                error,
                            );
                            diagnostics.extend(render_merged(
                                doc,
                                &finding,
                                DiagnosticSeverity::ERROR,
                                error.to_string(),
                            ));
                            continue;
                        }
                    };
                    let type_error = match self.provider_context.loaded_context() {
                        Some(provider_context) => typecheck::check_type_expr_value(
                            type_expr,
                            semantic_value,
                            provider_context,
                            expression,
                        )
                        .require_provider_schemas()
                        .err(),
                        None => typecheck::check_type_expr_value_providerless(
                            type_expr,
                            semantic_value,
                            self.provider_context.catalog(),
                            expression,
                        )
                        .record_at(
                            self.provider_context,
                            carina_core::validation::deferred_types::ProviderTypeCheckSite::new(
                                attr_param.type_check_location().clone(),
                                format!("attribute '{}'", attr_param.name),
                            ),
                        )
                        .err(),
                    };
                    if let Some(type_error) = type_error
                        && expression.check_owner()
                            == carina_core::parser::retained::ExpressionCheckOwner::Expression
                    {
                        let finding = carina_core::parser::Located::new(
                            record_for(attr_param).location().clone(),
                            type_error,
                        );
                        diagnostics.extend(render_merged(
                            doc,
                            &finding,
                            DiagnosticSeverity::WARNING,
                            finding.detail().to_string(),
                        ));
                    }
                }
            }
        }

        // Buffer parses use the bootstrap provider context, so dotted
        // boundary annotations remain `DottedUnresolved` even when schemas
        // are loaded. Resolve a local copy before the BindingIndex-backed
        // directional check. Expanded compositions handle imported-module
        // boundaries; this source-local walk covers the open module's own
        // `attributes {}` declaration.
        let attribute_params = semantic_params
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        let resolved_attribute_params =
            carina_core::validation::resolve_attribute_static_types_for_consumer(
                &attribute_params,
                self.provider_context.catalog(),
            );
        if let Err(ref_errors) =
            carina_core::validation::validate_consumer_attribute_param_ref_types_with_bindings(
                &resolved_attribute_params,
                binding_index,
            )
        {
            // Ownership comes from passing only this buffer's parameters to core;
            // the supplied BindingIndex adds context without adding sibling params.
            for error_msg in ref_errors.lines() {
                let Some(param_name) = error_msg
                    .strip_prefix("attribute '")
                    .and_then(|rest| rest.split('\'').next())
                else {
                    continue;
                };
                let parameter = parsed
                    .attribute_params
                    .iter()
                    .find(|parameter| parameter.name == param_name)
                    .expect("attribute reference finding belongs to an authored parameter");
                let finding = carina_core::parser::Located::new(
                    record_for(parameter).location().clone(),
                    error_msg,
                );
                diagnostics.extend(render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::WARNING,
                    error_msg.to_string(),
                ));
            }
        }

        diagnostics
    }

    /// Validate export parameter values against their type annotations.
    ///
    /// The expanded graph, when available, feeds inference and secrecy checks so
    /// module-call / `upstream_state` bindings declared in sibling
    /// `.crn` files are visible — without it the export inference would
    /// raise spurious `unknown binding` for an `exports.crn` that
    /// references a `let` bound elsewhere in the directory (#2493).
    pub(super) fn check_exports_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        tree: &carina_core::parser::AnalysisTree<'_>,
        semantic_view: &SemanticView<'_>,
        value_bindings: &ResolvedBindings,
        expanded: Option<&carina_core::parser::ModuleResolvedParsedFile>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let name_location = |param: &ParsedExportParam| {
            tree.syntax()
                .declarations()
                .iter()
                .find(|declaration| {
                    declaration.kind() == carina_core::config_loader::DeclarationKind::Export
                        && declaration.name() == param.name
                        && declaration.location().file().path() == param.source_file()
                        && (param.source_span().start_byte()
                            <= declaration.location().span().start_byte()
                            && declaration.location().span().end_byte()
                                <= param.source_span().end_byte())
                })
                .expect("parser-owned export name")
                .location()
                .clone()
        };
        let expanded_values = expanded.map(|graph| {
            (
                graph,
                GenericResolvedBindings::for_expanded_validation(graph),
            )
        });

        for param in &parsed.export_params {
            let judged_param = semantic_view.lookup(param).unwrap_or(param);
            let expression = match judged_param.judgement() {
                Ok(expression) => expression,
                Err(error) => {
                    let finding = carina_core::parser::Located::new(
                        param.type_check_location().clone(),
                        error,
                    );
                    diagnostics.extend(render_merged(
                        doc,
                        &finding,
                        DiagnosticSeverity::ERROR,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            if param.static_type.as_ref().is_some_and(|ty| {
                carina_core::validation::resolve_type_expr(ty, self.provider_context.catalog())
                    .is_err()
            }) {
                continue;
            }

            let message = match expanded_values.as_ref() {
                Some((graph, bindings)) => graph
                    .export_params
                    .iter()
                    .find(|candidate| {
                        candidate.source_file() == param.source_file()
                            && candidate.source_span() == param.source_span()
                    })
                    .and_then(|semantic_param| self.export_value_error(semantic_param, bindings)),
                None => {
                    let mut fallback;
                    let semantic_param = match semantic_view.lookup(param) {
                        Some(semantic_param) => semantic_param,
                        None => {
                            let Some(value) = semantic_view.value_for_typecheck(param) else {
                                continue;
                            };
                            fallback = param.clone();
                            fallback.value = Some(carina_core::parser::BoundaryExpression::Value(
                                value.clone(),
                            ));
                            &fallback
                        }
                    };
                    self.export_value_error(semantic_param, value_bindings)
                }
            };
            if let Some(message) = message
                && expression.check_owner()
                    != carina_core::parser::retained::ExpressionCheckOwner::Reported
            {
                let finding = carina_core::parser::Located::new(name_location(param), message);
                diagnostics.extend(render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::WARNING,
                    finding.detail().clone(),
                ));
            }
        }

        diagnostics
    }

    fn export_value_error<B: BindingAddress>(
        &self,
        param: &ParsedExportParam<ParserPlaceholder, B>,
        value_bindings: &GenericResolvedBindings<ParserPlaceholder, B>,
    ) -> Option<String> {
        let error = carina_core::validation::check_export_values_pre_apply(
            std::slice::from_ref(param),
            value_bindings,
            self.provider_context,
        )
        .err();
        if param.judgement().is_ok_and(|expression| {
            expression.check_owner()
                == carina_core::parser::retained::ExpressionCheckOwner::BindingIndex
        }) {
            // BindingIndex owns reference type diagnostics. Secrecy is an
            // independent export obligation, including removed attributes.
            let value = param.value()?;
            let mut removed_secret = false;
            value.visit_resource_refs(&mut |path, usage| {
                if usage == carina_core::resource::ResourceRefUsage::Required
                    && path.removed_attribute_was_sensitive(value_bindings)
                {
                    removed_secret = true;
                }
            });
            let secret = match carina_core::resolver::resolve_ref_value_with_config(
                value,
                value_bindings,
                self.provider_context,
            ) {
                Ok(resolved) => removed_secret || carina_core::value::contains_secret(&resolved),
                Err(_) => removed_secret, // Reference validation owns resolution failures.
            };
            return secret.then(|| format!("export '{}' cannot hold a secret value", param.name));
        }
        error
    }

    pub(super) fn check_inferred_export_types<D, B: BindingAddress>(
        &self,
        doc: &Document,
        configuration: &carina_core::parser::retained::JudgedFile<
            '_,
            ParsedExportParam<ParserPlaceholder, B>,
            D,
            B,
        >,
        tree: &carina_core::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    ) -> Vec<Diagnostic>
    where
        D: Clone
            + carina_core::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let inference_input = configuration.file();
        let mut diagnostics = Vec::new();
        let inferred_export_params = if let Some(loaded_providers) = &self.loaded_providers {
            let provider_inferred = carina_core::validation::inference::
                infer_export_params_with_loaded_provider_schemas(
                    tree,
                    loaded_providers.schemas(),
                    self.provider_context.catalog(),
                );
            // Surface inference failures as "type annotation required"
            // diagnostics, anchored at the export name. Errors for
            // exports declared in sibling `.crn` files (visible only via
            // `merged`) won't anchor in this buffer and are skipped here —
            // the buffer that owns the export will surface them on its
            // own pass.
            for inference_error in carina_core::validation::inference::attached_inference_failures(
                provider_inferred.checked_exports(),
            ) {
                if let Some(diagnostic) = render_merged(
                    doc,
                    &inference_error,
                    DiagnosticSeverity::WARNING,
                    carina_core::validation::inference::format_inference_error(
                        inference_error.name,
                        inference_error.error,
                    ),
                ) {
                    diagnostics.push(diagnostic);
                }
            }
            provider_inferred.into_inferred_export_params()
        } else {
            carina_core::validation::inference::infer_export_params(configuration)
        };
        let signatures = carina_core::module_resolver::ResolvedModuleSignatures::default();
        let export_bindings = carina_core::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
            inference_input, &self.schemas, None, &signatures, self.provider_context,
        );
        let mut boundary_export_params = inferred_export_params.parameters().to_vec();
        for parameter in &mut boundary_export_params {
            let Some(type_expr) = parameter.type_expr_opt_mut() else {
                continue;
            };
            if let Ok(resolved) = carina_core::validation::resolve_type_expr(
                type_expr,
                self.provider_context.catalog(),
            ) {
                *type_expr = resolved;
            }
        }
        if let Err(ref_errors) =
            carina_core::validation::validate_export_param_ref_types_with_bindings(
                &boundary_export_params,
                &export_bindings,
            )
        {
            for error in ref_errors {
                let finding = error.diagnostic(tree);
                diagnostics.extend(render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    error.to_string(),
                ));
            }
        }

        diagnostics
    }

    /// Check authored references against directory-wide binding authority.
    pub(super) fn check_undefined_references(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        defined_bindings: &HashSet<String>,
        declared_providers: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut enum_namespaces = HashSet::new();
        for factory in self
            .factories
            .iter()
            .filter(|factory| declared_providers.contains(factory.name()))
        {
            for attr_type in factory.provider_config_attribute_types().parts().values() {
                collect_enum_provider_namespaces(attr_type, &mut enum_namespaces);
            }
        }
        let mut emitted = HashSet::new();
        tree.value_references()
            .iter()
            .filter_map(|value| {
                let reference = value.reference();
                if reference.binding_matches(|binding| {
                    binding.is_in_names(defined_bindings)
                        || binding.is_in_names(declared_providers)
                        || self
                            .provider_names
                            .iter()
                            .any(|name| binding.matches_name(name))
                        || binding.is_in_names(&enum_namespaces)
                }) || !emitted.insert(reference.location().clone())
                {
                    return None;
                }
                let finding = carina_core::parser::Located::new(
                    reference.prefix_location().clone(),
                    reference.render(|path| {
                        format!(
                            "Undefined resource: '{}'. Define it with 'let {} = aws...'",
                            path.binding(),
                            path.binding()
                        )
                    }),
                );
                render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                )
            })
            .collect()
    }

    /// Check for unknown attributes on resource references (typo detection).
    ///
    /// When a ResourceRef like `igw.internet_gateway_idd` references an attribute
    /// that doesn't exist in the referenced resource's schema, emit a warning
    /// with a "did you mean" suggestion if a similar attribute exists.
    /// Attribute-parameter and module-call refs are excluded because their
    /// source-owned and expanded checks map shared core validation errors.
    pub(super) fn check_resource_ref_attributes(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        binding_index: &BindingIndex<'_>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for resource in tree.resources() {
            for value in resource.attributes().values() {
                value.visit_resource_refs_for_diagnostics(&mut |scoped, usage| {
                    let reference = tree.reference_for_scoped(scoped);
                    let mut location = reference.own_attribute_location();
                    let Some(error) = reference.attribute_error(binding_index, usage) else {
                        return;
                    };
                    let message = match error {
                        carina_core::resource::SecretDiagnosticError::Public(
                            RefTypeError::UnknownAttribute {
                                binding,
                                attribute,
                                known_attributes,
                                target: RefTargetKind::Schema { resource_type },
                            },
                        ) => {
                            let diagnostic_binding = binding;
                            let diagnostic_attribute = attribute;
                            location = reference
                                .attribute_location(&diagnostic_binding, &diagnostic_attribute);
                            let known_attrs: Vec<&str> =
                                known_attributes.iter().map(String::as_str).collect();
                            let suggestion =
                                suggest_similar_name(&diagnostic_attribute, &known_attrs)
                                    .map(|suggestion| format!(" Did you mean '{}'?", suggestion))
                                    .unwrap_or_default();
                            format!(
                                "Unknown attribute '{}' on '{}' (type '{}'){}",
                                diagnostic_attribute, diagnostic_binding, resource_type, suggestion,
                            )
                        }
                        carina_core::resource::SecretDiagnosticError::Public(
                            RefTypeError::UnknownAttribute {
                                binding,
                                attribute,
                                known_attributes,
                                target: RefTargetKind::LiveComposition,
                            },
                        ) => {
                            let diagnostic_binding = binding;
                            let diagnostic_attribute = attribute;
                            location = reference
                                .attribute_location(&diagnostic_binding, &diagnostic_attribute);
                            let known_attrs: Vec<&str> =
                                known_attributes.iter().map(String::as_str).collect();
                            let suffix = suggest_similar_name(&diagnostic_attribute, &known_attrs)
                                .map(|suggestion| format!(". Did you mean '{}'?", suggestion))
                                .unwrap_or_else(|| ".".to_string());
                            format!(
                                "Unknown attribute '{}' on '{}'{}",
                                diagnostic_attribute, diagnostic_binding, suffix,
                            )
                        }
                        error => error.to_string(),
                    };

                    let finding = carina_core::parser::Located::new(location.clone(), message);
                    if let Some(diagnostic) = render_merged(
                        doc,
                        &finding,
                        DiagnosticSeverity::WARNING,
                        finding.detail().clone(),
                    ) {
                        diagnostics.push(diagnostic);
                    }
                });
            }
        }
        diagnostics
    }

    /// Run the same typed parser-placeholder boundary as CLI validation.
    pub(super) fn check_expansion_boundary(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        boundary: ParserPlaceholderBoundary,
        scope: ExpansionBoundaryScope,
    ) -> Vec<Diagnostic> {
        let Err(leftovers) = parsed.validate_expansion_boundary(boundary) else {
            return Vec::new();
        };
        leftovers
            .into_errors()
            .into_iter()
            .filter_map(|error| {
                if error
                    .placeholder()
                    .is_some_and(|placeholder| !scope.includes(placeholder))
                {
                    return None;
                }
                let finding = error
                    .diagnostic()
                    .expect("expansion boundary provenance invariant");
                render_merged(doc, &finding, DiagnosticSeverity::ERROR, error.to_string())
            })
            .collect()
    }

    /// Mirror dependency findings using their sole parser-owned rendering anchor.
    pub(super) fn check_depends_on(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
        argument_types: &carina_core::validation::ResolvedArgumentTypes,
    ) -> Vec<Diagnostic> {
        use carina_core::validation::depends_on::{
            DependsOnDiagnosticKind, Severity, validate_depends_on,
        };
        validate_depends_on(tree, argument_types)
            .into_iter()
            .flat_map(|finding| {
                let severity = match finding.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                };
                match &finding.kind {
                    DependsOnDiagnosticKind::Cycle { cycle } => cycle
                        .members
                        .iter()
                        .find(|member| member.location.file().path() == doc.source_file())
                        .and_then(|member| {
                            let member = carina_core::parser::Located::new(
                                member.location.clone(),
                                &finding.message,
                            );
                            render_merged(doc, &member, severity, finding.message.clone())
                        })
                        .into_iter()
                        .collect::<Vec<_>>(),
                    DependsOnDiagnosticKind::Element { .. } => {
                        render_merged(doc, &finding, severity, finding.message.clone())
                            .into_iter()
                            .collect()
                    }
                }
            })
            .collect()
    }

    pub(super) fn check_wait_bindings(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::wait::validate_wait_bindings(tree, &self.schemas)
            .into_iter()
            .filter_map(|finding| {
                render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.message.clone(),
                )
            })
            .collect()
    }

    pub(super) fn check_deferred_populate_refs(
        &self,
        doc: &Document,
        tree: &carina_core::parser::AnalysisTree<'_>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::deferred_populate::validate_deferred_populate_refs(
            tree,
            &self.schemas,
        )
        .into_iter()
        .filter_map(|finding| {
            render_merged(
                doc,
                &finding,
                DiagnosticSeverity::ERROR,
                finding.message.clone(),
            )
        })
        .collect()
    }
}
