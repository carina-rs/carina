//! Function policy over the existing authored judgements and parser projections.
//! Bodies are never instantiated here: their parameter nodes already carry the
//! declared types, and each declaration's return verdict is cached once.

use super::*;
use crate::binding_index::{BindingIndex, BindingTarget, RefType, ResolvedRefType};
use crate::resource::{AccessPath, DeferredValue, ReferencePathRef};
use crate::static_type::{
    ConsumerExportStaticType, InferenceError, InferenceIssue, NotInferableReason, StaticType,
};
use crate::upstream_exports::UpstreamExportType;
use crate::validation::admission::static_judgement::{
    StaticAssignmentError, StaticAssignmentSource, StaticAssignmentVerdict,
    judge_static_assignment, type_needs_schema,
};
use crate::validation::typecheck::{FunctionTypePosition, format_function_type_promise_error};

struct Source {
    reference: Option<ResolvedRefType>,
    ty: Option<TypeExpr>,
    untyped_export: Option<String>,
    unavailable: bool,
    pending_substitution: bool,
    incomplete_context: bool,
    resolution_error: Option<crate::static_type::StaticTypeResolutionError>,
}

pub(super) struct Finding {
    pub node: (ExpressionInstance, NodeId),
    pub diagnostic: Located<InferenceError>,
}

impl Source {
    fn evidence(&self) -> StaticAssignmentSource<'_> {
        if self.unavailable {
            StaticAssignmentSource::SchemaUnavailable
        } else if let Some(reference) = &self.reference {
            StaticAssignmentSource::Reference(reference)
        } else if let Some(ty) = &self.ty {
            StaticAssignmentSource::Expression(ty)
        } else {
            StaticAssignmentSource::Unknown
        }
    }

    fn label(&self) -> String {
        self.reference
            .as_ref()
            .map(ResolvedRefType::display_type_name)
            .or_else(|| self.ty.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| "unknown".into())
    }
}

pub(super) fn check<B: BindingAddress>(
    expressions: &Expressions<B>,
    report: &ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    complete_sources: bool,
) -> Vec<Finding> {
    let mut declarations = HashMap::new();
    for node in report.syntax_nodes.values() {
        if let ExprKind::Function { name, .. } = &node.kind {
            declarations
                .entry((node.location.instance.clone(), name.clone()))
                .or_insert(node);
        }
    }
    let mut returns = HashMap::new();
    let mut findings = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = expressions
        .roots
        .iter()
        .chain(expressions.definitions.values())
        .collect::<Vec<_>>();
    while let Some(expression) = pending.pop() {
        let node = &expression.authored;
        if !visited.insert((node.location.instance.clone(), node.id))
            || matches!(node.kind, ExprKind::Function { .. })
        {
            continue;
        }
        if matches!(node.kind, ExprKind::If) {
            pending.extend(node.children.first());
            if let Some(selected) = report.secrecy_condition_value(expression) {
                pending.extend(node.children.get(if selected { 1 } else { 2 }));
            }
        } else {
            pending.extend(node.children.iter().rev());
        }
        let ExprKind::Call(name) = &node.kind else {
            continue;
        };
        let key = (node.location.instance.clone(), name.clone());
        let Some(declaration) = declarations.get(&key) else {
            continue;
        };
        let ExprKind::Function { signature, .. } = &declaration.kind else {
            continue;
        };
        // No evaluation means main has not established an unknown call value
        // (for example, a call in the body of an empty static loop).
        if projection(expression, expressions).is_none() {
            continue;
        }
        // Template placeholders have not reached main's call-site known/unknown
        // decision yet. Let its existing substitution/evaluation finish first.
        if projection(expression, expressions)
            .is_some_and(super::super::functions::contains_module_argument)
            || node.children.iter().any(|argument| {
                let source = source(argument, expressions, report, bindings);
                source.pending_substitution
                    || (!complete_sources && (source.unavailable || source.incomplete_context))
            })
        {
            continue;
        }
        for (index, (parameter, ty)) in signature.parameters.iter().enumerate() {
            let Some(ty) = ty else { continue };
            let argument = node.children.get(index).or_else(|| {
                let id = signature.default_nodes.get(index).copied().flatten()?;
                declaration
                    .children
                    .iter()
                    .find(|child| child.node_id() == id)
            });
            let Some(argument) = argument else { continue };
            if known(argument, expressions, bindings) {
                continue;
            }
            if let Some(error) = promise_error(
                argument,
                ty,
                name,
                FunctionTypePosition::Parameter(parameter),
                expressions,
                report,
                bindings,
                complete_sources,
            ) {
                push_finding(&mut findings, node, error, report);
            }
        }
        let Some(ty) = &signature.return_type else {
            continue;
        };
        if node
            .children
            .iter()
            .any(|argument| source(argument, expressions, report, bindings).unavailable)
        {
            continue;
        }
        if known(expression, expressions, bindings) {
            continue;
        }
        let error = returns.entry(key).or_insert_with(|| {
            let AuthoredChildRoles::Function { body } = declaration.roles else {
                return None;
            };
            let body = declaration
                .children
                .iter()
                .find(|child| child.node_id() == body)?;
            if known(body, expressions, bindings) {
                return None;
            }
            promise_error(
                body,
                ty,
                name,
                FunctionTypePosition::Return,
                expressions,
                report,
                bindings,
                complete_sources,
            )
        });
        if let Some(error) = error {
            push_finding(&mut findings, node, error.clone(), report);
        }
    }
    findings
}

fn push_finding<B: BindingAddress>(
    findings: &mut Vec<Finding>,
    node: &AuthoredExpr<ParserPlaceholder, B>,
    mut error: InferenceError,
    report: &ExpressionReport<B>,
) {
    if report
        .node(&node.location.instance, node.id)
        .is_some_and(|judged| judged.diagnostic_is_secret())
    {
        error = error.inside_secret();
    }
    let finding = Located::new(node.location.authored.clone(), error);
    let key = (node.location.instance.clone(), node.id);
    if !findings
        .iter()
        .any(|existing| existing.node == key && existing.diagnostic == finding)
    {
        findings.push(Finding {
            node: key,
            diagnostic: finding,
        });
    }
}

fn projection<'a, B: BindingAddress>(
    expression: &'a ExprValue<ParserPlaceholder, B>,
    expressions: &'a Expressions<B>,
) -> Option<&'a Value<ParserPlaceholder, B>> {
    expressions
        .projections
        .get(&(
            expression.authored.location.instance.clone(),
            expression.node_id(),
        ))
        .or_else(|| expression.projection())
        .or_else(|| {
            // Let RHS evaluation attaches its value to the definition's outer
            // aliases, rather than capturing the nested call separately.
            expressions.definitions.values().find_map(|definition| {
                let value = definition.projection()?;
                let mut current = definition;
                loop {
                    if current.node_id() == expression.node_id()
                        && current.authored.location.instance
                            == expression.authored.location.instance
                    {
                        return Some(value);
                    }
                    current = match (&current.authored.kind, current.authored.children.as_slice()) {
                        (ExprKind::Alias | ExprKind::Parens | ExprKind::Definition, [child]) => {
                            child
                        }
                        _ => return None,
                    };
                }
            })
        })
}

fn known<B: BindingAddress>(
    expression: &ExprValue<ParserPlaceholder, B>,
    expressions: &Expressions<B>,
    bindings: &BindingIndex<'_, B>,
) -> bool {
    let Some(value) = projection(expression, expressions) else {
        return false;
    };
    // Use main's static-evaluation permission. In particular, do not evaluate
    // bodies with unknown arguments just to discover a stronger return type.
    let value = value
        .clone()
        .map_bindings(&mut |binding| RelativeBindingName::new(binding.as_str()));
    let Ok(knowledge) =
        value.knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
    else {
        return false;
    };
    let knowledge = knowledge.into_classification();
    if !knowledge.permits_static_evaluation() {
        return false;
    }
    super::super::evaluate_static_value_for_validation(&knowledge, bindings.provider_context())
        .is_some_and(|value| {
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .is_ok_and(|proof| proof.into_classification().into_known().is_ok())
        })
}

#[allow(clippy::too_many_arguments)]
fn promise_error<B: BindingAddress>(
    expression: &ExprValue<ParserPlaceholder, B>,
    ty: &TypeExpr,
    name: &str,
    position: FunctionTypePosition<'_>,
    expressions: &Expressions<B>,
    report: &ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    complete_sources: bool,
) -> Option<InferenceError> {
    let judged = report.node(&expression.authored.location.instance, expression.node_id())?;
    if matches!(judged.static_type(), Err(InferenceIssue::Failed(_))) {
        return None;
    }
    let source = source(expression, expressions, report, bindings);
    if source.pending_substitution {
        return None;
    }
    if let Some(error) = source.resolution_error {
        return Some(InferenceError::TypeResolution { error });
    }
    if !complete_sources && source.incomplete_context {
        return None;
    }
    match judge_static_assignment(source.evidence(), ty, bindings) {
        Ok(StaticAssignmentVerdict::Proven | StaticAssignmentVerdict::NeedsSchema) => None,
        Err(StaticAssignmentError::Resolution(error)) => {
            Some(InferenceError::TypeResolution { error })
        }
        Ok(StaticAssignmentVerdict::NeedsLocal) | Err(StaticAssignmentError::Mismatch) => {
            Some(InferenceError::UnknownType {
                reason: format_function_type_promise_error(
                    name,
                    position,
                    ty,
                    &source.label(),
                    source.untyped_export.as_deref(),
                ),
            })
        }
    }
}

fn source<B: BindingAddress>(
    expression: &ExprValue<ParserPlaceholder, B>,
    expressions: &Expressions<B>,
    report: &ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
) -> Source {
    let node = &expression.authored;
    // Sequences and aliases select an existing judgement, never a new body.
    let selected = match node.kind {
        ExprKind::Sequence => node
            .result_index()
            .and_then(|index| node.children.get(index)),
        ExprKind::Alias | ExprKind::Parens | ExprKind::Definition => node.children.first(),
        _ => None,
    };
    if let Some(selected) = selected {
        return source(selected, expressions, report, bindings);
    }
    let judged = report.node(&node.location.instance, node.id);
    let mut source = Source {
        reference: None,
        ty: judged.and_then(|judged| judged.static_type().ok()).cloned(),
        untyped_export: None,
        unavailable: schema_unavailable(expression, report),
        pending_substitution: projection(expression, expressions)
            .is_some_and(super::super::functions::contains_module_argument),
        incomplete_context: matches!(
            judged.map(|judged| judged.static_type()),
            Some(Err(InferenceIssue::NotInferable(
                NotInferableReason::NonInferableBinding { .. }
            )))
        ),
        resolution_error: None,
    };
    source.unavailable |= source
        .ty
        .as_ref()
        .is_some_and(|ty| type_needs_schema(ty, bindings.provider_context()));
    let authored_path = match &node.kind {
        ExprKind::Reference {
            binding,
            attribute,
            segments,
        } => Some(AccessPath::from_binding(
            binding.clone(),
            attribute,
            segments.clone(),
        )),
        _ => None,
    };
    let path = match projection(expression, expressions) {
        Some(Value::Deferred(DeferredValue::ResourceRef { path })) => Some(path),
        _ => authored_path.as_ref(),
    };
    if let Some(path) = path {
        source.pending_substitution |= pending_argument(expressions, bindings, path.binding());
        if let Some(entry) = bindings.get(path.binding())
            && let BindingTarget::UpstreamState(exports) = entry.target
            && let Some(export) = exports.get(path.attribute())
        {
            match &export.static_type {
                UpstreamExportType::Static(ConsumerExportStaticType::Inferred(_)) => {
                    source.ty = None;
                    source.incomplete_context = false;
                    source.untyped_export =
                        Some(format!("{}.{}", path.binding(), path.attribute()));
                }
                UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                    StaticType::Unchecked(reason),
                )) => {
                    source.ty = None;
                    source.unavailable |=
                        type_needs_schema(reason.original(), bindings.provider_context());
                    if !source.unavailable {
                        source.resolution_error = Some(reason.resolution_error().clone());
                    }
                }
                _ => {}
            }
        }
        if source.untyped_export.is_none() {
            match bindings.ref_type(ReferencePathRef::Access(path)) {
                RefType::Typed(reference) => {
                    source.reference = Some(reference);
                    source.incomplete_context = false;
                }
                RefType::Unchecked | RefType::UnknownBinding { .. } => {
                    // An instance's declaration cannot strengthen a caller's
                    // still-unindexed reference. The completed index owns it.
                    source.ty = None;
                    source.incomplete_context = true;
                    source.unavailable |= bindings.references_resource_attribute(path.into());
                }
                RefType::UnknownAttribute(_) => {}
            }
        }
    } else if let Some(Value::Deferred(DeferredValue::BindingRef { binding })) =
        projection(expression, expressions)
    {
        source.pending_substitution |= pending_argument(expressions, bindings, binding.as_str());
        match bindings.ref_type(ReferencePathRef::Binding(binding)) {
            RefType::Typed(reference) => {
                source.reference = Some(reference);
                source.incomplete_context = false;
            }
            RefType::Unchecked | RefType::UnknownBinding { .. } => {
                if !expressions.definitions.contains_key(binding) {
                    source.ty = None;
                    source.incomplete_context = true;
                }
            }
            RefType::UnknownAttribute(_) => {}
        }
    }
    source
}

fn pending_argument<B: BindingAddress>(
    expressions: &Expressions<B>,
    bindings: &BindingIndex<'_, B>,
    name: &str,
) -> bool {
    matches!(expressions.instance, ExpressionInstance::Root)
        && matches!(
            bindings.get(name).map(|entry| entry.target),
            Some(BindingTarget::Argument(_))
        )
}

fn schema_unavailable<B: BindingAddress>(
    expression: &ExprValue<ParserPlaceholder, B>,
    report: &ExpressionReport<B>,
) -> bool {
    let mut pending = vec![expression];
    let mut visited = HashSet::new();
    while let Some(expression) = pending.pop() {
        let node = &expression.authored;
        if !visited.insert(node.id) {
            continue;
        }
        if matches!(
            report
                .node(&node.location.instance, node.id)
                .map(|judged| judged.static_type()),
            Some(Err(InferenceIssue::NotInferable(
                NotInferableReason::SchemaUnavailable { .. }
                    | NotInferableReason::CustomTypeSchemaUnavailable { .. }
            )))
        ) {
            return true;
        }
        pending.extend(&node.children);
    }
    false
}
