//! Consuming editor judgement and value-only provider projections.

use super::{
    DirectoryAttemptError, DirectoryParseFailureKind, DirectoryParseResult, ParsedRootDirectory,
    ParsedSourceBuffer, SourceOverrides,
};
use crate::parser::{self, AnalysisResolutionError, ParsedFile, ProviderContext};
use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart};
pub use crate::validation::deferred_types::EditorFunctionBodyJudgements;
use crate::validation::deferred_types::{EditorFunctionBodyContext, ProviderTypeCheckContext};
use std::{path::Path, sync::Arc};

mod provider;

type Value = parser::ParsedValue;

/// A directory attempt whose resolution failure cannot be borrowed or cloned.
/// Source failures remain available to source/declaration checks.
pub struct MergedAttempt {
    result: Result<DirectoryParseResult, DirectoryAttemptError>,
}
impl MergedAttempt {
    /// Borrow directory values without acquiring a resolution publication.
    pub fn merged(&self) -> Option<&DirectoryParseResult> {
        self.result.as_ref().ok()
    }
    /// Borrow only a source failure, classified by the operation that failed.
    pub fn source_failure(&self) -> Option<&super::DirectoryParseFailure> {
        match &self.result {
            Err(DirectoryAttemptError::Source(error)) => Some(error),
            _ => None,
        }
    }
    /// A current-file resolution already exists, even though publication is later.
    pub fn has_resolution_for(&self, file: &Path) -> bool {
        matches!(&self.result, Err(DirectoryAttemptError::Resolution(error)) if error.source_file() == Some(file))
    }
    /// Select the current document's judgement exactly once. A sibling failure
    /// leaves the buffer fallback available; a current-file resolution consumes it.
    pub fn into_parts(
        self,
        file: &Path,
        buffer: Option<&ParsedSourceBuffer>,
    ) -> (Option<Arc<DirectoryParseResult>>, EditorResolutionRequest) {
        match self.result {
            Ok(directory) => {
                let directory = Arc::new(directory);
                let request = EditorResolutionRequest {
                    input: ResolutionInput::Merged(Arc::clone(&directory)),
                };
                (Some(directory), request)
            }
            Err(DirectoryAttemptError::Resolution(error)) if error.source_file() == Some(file) => (
                None,
                EditorResolutionRequest {
                    input: ResolutionInput::Known(error),
                },
            ),
            Err(DirectoryAttemptError::Resolution(_) | DirectoryAttemptError::Source(_)) => {
                (None, editor_buffer_resolution(buffer))
            }
        }
    }
}

/// Parse an editor snapshot while retaining producer-assigned failure origin.
pub fn parse_editor_directory_with_overrides_and_schemas(
    dir: &crate::paths::ResolvedPath,
    config: &ProviderContext,
    overrides: &SourceOverrides,
    schemas: &crate::schema::SchemaRegistry,
) -> MergedAttempt {
    MergedAttempt {
        result: super::parse_directory_with_overrides_impl(
            dir,
            config,
            overrides,
            super::diagnostics_resolution_context(),
            schemas,
        ),
    }
}

/// Issue the orphan/current-buffer judgement from a parser-minted snapshot.
pub fn editor_buffer_resolution(buffer: Option<&ParsedSourceBuffer>) -> EditorResolutionRequest {
    EditorResolutionRequest {
        input: buffer.map_or(ResolutionInput::Empty, |buffer| {
            ResolutionInput::Buffer(buffer.clone())
        }),
    }
}

/// One file-level strict judgement. Private constructors and no `Clone` keep
/// value consumers from reusing its publication capability.
pub struct EditorResolutionRequest {
    input: ResolutionInput,
}
enum ResolutionInput {
    Merged(Arc<DirectoryParseResult>),
    Buffer(ParsedSourceBuffer),
    Known(super::DirectoryParseFailure),
    Empty,
}
impl EditorResolutionRequest {
    /// Consume the selected attempt, preserving strict resolution's first error.
    pub fn finish(self, session: &dyn ProviderTypeCheckContext) -> EditorResolution {
        match self.input {
            ResolutionInput::Merged(directory) => finish_file(directory.parsed.parsed(), session),
            ResolutionInput::Buffer(buffer) => finish_file(buffer.parsed(), session),
            ResolutionInput::Known(error) => {
                let error = match error.kind {
                    DirectoryParseFailureKind::Element(finding) => {
                        AnalysisResolutionError::Diagnostic(finding)
                    }
                    DirectoryParseFailureKind::Parse { error, .. } => {
                        AnalysisResolutionError::from_parse_failure(*error)
                    }
                    _ => unreachable!("a current-file resolution has parser-owned provenance"),
                };
                EditorResolution {
                    finding: Some(OwnedResolutionFinding { error }),
                    bodies: EditorFunctionBodyJudgements::empty(),
                }
            }
            ResolutionInput::Empty => EditorResolution {
                finding: None,
                bodies: EditorFunctionBodyJudgements::empty(),
            },
        }
    }
}
fn finish_file(file: &ParsedFile, session: &dyn ProviderTypeCheckContext) -> EditorResolution {
    match parser::AnalysisTree::try_from_file(file) {
        Ok(tree) => {
            let context = EditorFunctionBodyContext::new(session, file.expression_report());
            let error = parser::resolve_resource_refs_from_analysis(&tree, &context).err();
            EditorResolution {
                finding: error.map(|error| OwnedResolutionFinding { error }),
                bodies: context.into_judgements(),
            }
        }
        Err(error) => EditorResolution {
            finding: Some(OwnedResolutionFinding {
                error: error.into(),
            }),
            bodies: EditorFunctionBodyJudgements::empty(),
        },
    }
}

/// At most one finding, plus non-publishable function-body dependency receipts.
pub struct EditorResolution {
    finding: Option<OwnedResolutionFinding>,
    bodies: EditorFunctionBodyJudgements,
}
impl EditorResolution {
    /// Transfer the publication once, independently of value-query receipts.
    pub fn into_parts(self) -> (Option<OwnedResolutionFinding>, EditorFunctionBodyJudgements) {
        (self.finding, self.bodies)
    }
}

/// A consuming publication from the root or independent provider judgement.
pub struct OwnedResolutionFinding {
    error: AnalysisResolutionError,
}
impl OwnedResolutionFinding {
    /// Consume the finding at the frontend's resolution renderer.
    pub fn into_error(self) -> AnalysisResolutionError {
        self.error
    }
}

impl EditorFunctionBodyJudgements {
    /// Project provider inputs before their separate metadata/module query.
    /// A rejected body has no provider value; unrelated fields still finalize.
    fn provider_metadata(
        &self,
        mut directory: ParsedRootDirectory,
        session: &dyn ProviderTypeCheckContext,
    ) -> ParsedRootDirectory {
        if self.is_empty() {
            return directory;
        }
        let file = directory.parsed();
        let keep = file
            .providers
            .iter()
            .map(|provider| {
                let project = |value: &Value| {
                    !uses_rejected_definition(self, value, file, &mut Vec::new())
                        && !parser::editor_provider_value_uses_owned_body(
                            file, value, session, self,
                        )
                };
                (
                    provider
                        .attributes
                        .values()
                        .map(project)
                        .collect::<Vec<_>>(),
                    provider
                        .default_tags
                        .values()
                        .map(project)
                        .collect::<Vec<_>>(),
                    provider
                        .unresolved_attributes
                        .values()
                        .map(project)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        for (provider, (attributes, tags, unresolved)) in
            directory.parsed.providers.iter_mut().zip(keep)
        {
            retain_values(&mut provider.attributes, attributes);
            retain_values(&mut provider.default_tags, tags);
            retain_values(&mut provider.unresolved_attributes, unresolved);
        }
        directory
    }
}
fn retain_values(values: &mut parser::ConfigurationAttributes<Value>, keep: Vec<bool>) {
    let mut keep = keep.into_iter();
    values.retain(|_, _| {
        keep.next()
            .expect("one provider projection decision per value")
    });
}

// Definition receipts remain authoritative when an argument is not available
// until module expansion. Invocation receipts above require actual evaluation;
// a different argument must retain its independent provider judgement.
fn uses_rejected_definition(
    owned: &EditorFunctionBodyJudgements,
    value: &Value,
    file: &ParsedFile,
    visiting: &mut Vec<String>,
) -> bool {
    let visit =
        |value, visiting: &mut Vec<String>| uses_rejected_definition(owned, value, file, visiting);
    match value {
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
            owned.rejects_definition(function)
                || args
                    .iter()
                    .any(|argument| argument.value().is_some_and(|value| visit(value, visiting)))
                || function_uses_rejected_definition(owned, function, file, visiting)
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => args
            .iter()
            .any(|argument| argument.value().is_some_and(|value| visit(value, visiting))),
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            let key = format!("binding:{binding}");
            if visiting.contains(&key) {
                return false;
            }
            visiting.push(key);
            let rejected = file
                .variables
                .get(binding.as_str())
                .is_some_and(|value| visit(value, visiting));
            visiting.pop();
            rejected
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let key = format!("reference:{path}");
            if visiting.contains(&key) {
                return false;
            }
            visiting.push(key);
            let rejected = file
                .variables
                .get(path.binding_address().as_str())
                .is_some_and(|value| visit(value, visiting))
                || file.iter_top_level_resources().any(|resource| {
                    resource.binding() == Some(path.binding_address().as_str())
                        && resource
                            .attributes()
                            .get(path.attribute())
                            .is_some_and(|value| {
                                uses_rejected_definition(owned, value, file, visiting)
                            })
                });
            visiting.pop();
            rejected
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(|value| visit(value, visiting))
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            entries.values().any(|value| visit(value, visiting))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts
            .iter()
            .any(|part| matches!(part, InterpolationPart::Expr(value) if visit(value, visiting))),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            inner.visit(|value| visit(value, visiting))
        }
        Value::Deferred(DeferredValue::Access(access)) => visit(access.base(), visiting),
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            visit(expression.lhs(), visiting) || visit(expression.rhs(), visiting)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            visit(value, visiting) || visit(default, visiting)
        }
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred
            .values()
            .into_iter()
            .any(|value| visit(value, visiting)),
        _ => false,
    }
}
fn function_uses_rejected_definition(
    owned: &EditorFunctionBodyJudgements,
    function: &parser::UserFunction,
    file: &ParsedFile,
    visiting: &mut Vec<String>,
) -> bool {
    let key = format!(
        "function:{}:{}",
        function.body.location.file().path().display(),
        function.name
    );
    if visiting.contains(&key) {
        return false;
    }
    visiting.push(key);
    let rejected = function
        .local_lets
        .iter()
        .any(|(_, value, _)| uses_rejected_definition(owned, value, file, visiting))
        || uses_rejected_definition(owned, &function.body.value, file, visiting);
    visiting.pop();
    rejected
}
