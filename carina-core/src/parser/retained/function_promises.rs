//! Function policy over the existing authored judgements and parser projections.
//! Bodies are never instantiated here: their parameter nodes already carry the
//! declared types, and each declaration's return and nested-call verdicts are cached once.

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
use std::collections::BTreeSet;

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

#[derive(Clone, Default)]
struct Relevance {
    parameters: BTreeSet<usize>,
    independent: bool,
}

#[derive(Clone)]
struct BodyFinding {
    error: InferenceError,
    relevance: Relevance,
}

#[derive(Default)]
struct BodyVerdict {
    result: Option<InferenceError>,
    nested: Vec<BodyFinding>,
    unknown_capture: bool,
}

type DeclarationKey = (ExpressionInstance, String);

struct Evidence<'a, 'index, B: BindingAddress> {
    expressions: &'a Expressions<B>,
    report: &'a ExpressionReport<B>,
    bindings: &'a BindingIndex<'index, B>,
    complete_sources: bool,
    projections: HashMap<(ExpressionInstance, NodeId), &'a Value<ParserPlaceholder, B>>,
    definitions: HashSet<&'a str>,
}

impl<'a, 'index, B: BindingAddress> Evidence<'a, 'index, B> {
    fn new(
        expressions: &'a Expressions<B>,
        report: &'a ExpressionReport<B>,
        bindings: &'a BindingIndex<'index, B>,
        complete_sources: bool,
    ) -> Self {
        let mut projections = HashMap::new();
        for definition in expressions.definitions.values() {
            let Some(value) = definition.projection() else {
                continue;
            };
            let mut current = definition;
            loop {
                projections
                    .entry((
                        current.authored.location.instance.clone(),
                        current.node_id(),
                    ))
                    .or_insert(value);
                current = match (&current.authored.kind, current.authored.children.as_slice()) {
                    (ExprKind::Alias | ExprKind::Parens | ExprKind::Definition, [child]) => child,
                    _ => break,
                };
            }
        }
        Self {
            expressions,
            report,
            bindings,
            complete_sources,
            projections,
            definitions: expressions
                .definitions
                .keys()
                .map(|name| name.as_str())
                .collect(),
        }
    }

    fn projection<'e>(
        &'e self,
        expression: &'e ExprValue<ParserPlaceholder, B>,
    ) -> Option<&'e Value<ParserPlaceholder, B>> {
        let key = (
            expression.authored.location.instance.clone(),
            expression.node_id(),
        );
        self.expressions
            .projections
            .get(&key)
            .or_else(|| expression.projection())
            .or_else(|| self.projections.get(&key).copied())
    }

    fn known(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        let Some(value) = self.projection(expression) else {
            return false;
        };
        let value = value
            .clone()
            .map_bindings(&mut |binding| RelativeBindingName::new(binding.as_str()));
        let Ok(knowledge) =
            value.knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        else {
            return false;
        };
        let knowledge = knowledge.into_classification();
        knowledge.permits_static_evaluation()
            && super::super::evaluate_static_value_for_validation(
                &knowledge,
                self.bindings.provider_context(),
            )
            .is_some_and(|value| {
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .is_ok_and(|proof| proof.into_classification().into_known().is_ok())
            })
    }

    fn known_in_body(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        self.known(expression)
            || self
                .report
                .node(&expression.authored.location.instance, expression.node_id())
                .is_some_and(|judged| judged.concrete())
    }

    fn promise_error(
        &self,
        expression: &ExprValue<ParserPlaceholder, B>,
        ty: &TypeExpr,
        name: &str,
        position: FunctionTypePosition<'_>,
    ) -> Option<InferenceError> {
        let judged = self
            .report
            .node(&expression.authored.location.instance, expression.node_id())?;
        if judged.failed() {
            return None;
        }
        let source = self.source(expression);
        if source.pending_substitution || (!self.complete_sources && source.incomplete_context) {
            return None;
        }
        if let Some(error) = source.resolution_error {
            return Some(InferenceError::TypeResolution { error });
        }
        match judge_static_assignment(source.evidence(), ty, self.bindings) {
            Ok(StaticAssignmentVerdict::Proven | StaticAssignmentVerdict::NeedsSchema) => None,
            Err(StaticAssignmentError::Resolution(error)) => {
                Some(InferenceError::TypeResolution { error })
            }
            Err(StaticAssignmentError::Mismatch(types)) => {
                // Structural arguments and bare handle returns are owned by
                // the existing handle/value boundary, not a value promise.
                let structural_argument = source
                    .reference
                    .as_ref()
                    .is_some_and(|reference| reference.resource_handle_type().is_some());
                let structural_type = source.ty.as_ref().is_some_and(|ty| {
                    crate::validation::resolve_parameter_type(ty, self.bindings.provider_context())
                        .is_ok_and(|ty| ty.is_handle())
                });
                if structural_argument || structural_type {
                    None
                } else {
                    Some(InferenceError::UnknownType {
                        reason: format!("function '{name}': {position}: {types}"),
                    })
                }
            }
            Ok(StaticAssignmentVerdict::NeedsLocal) => Some(InferenceError::FunctionTypePromise {
                reason: format_function_type_promise_error(
                    name,
                    position,
                    ty,
                    &source.label(),
                    source.untyped_export.as_deref(),
                ),
            }),
        }
    }

    fn calls<'e>(
        &self,
        roots: impl IntoIterator<Item = &'e ExprValue<ParserPlaceholder, B>>,
        in_body: bool,
    ) -> Vec<&'e ExprValue<ParserPlaceholder, B>> {
        let mut pending: Vec<_> = roots.into_iter().collect();
        let mut visited = HashSet::new();
        let mut calls = Vec::new();
        while let Some(expression) = pending.pop() {
            let node = &expression.authored;
            if !visited.insert((node.location.instance.clone(), node.id))
                || matches!(node.kind, ExprKind::Function { .. })
            {
                continue;
            }
            if matches!(node.kind, ExprKind::If) {
                pending.extend(node.children.first());
                match self.report.secrecy_condition_value(expression) {
                    Some(selected) => {
                        pending.extend(node.children.get(if selected { 1 } else { 2 }))
                    }
                    None if in_body => pending.extend(node.children.iter().skip(1).rev()),
                    None => {}
                }
            } else {
                pending.extend(node.children.iter().rev());
            }
            if matches!(node.kind, ExprKind::Call(_)) {
                calls.push(expression);
            }
        }
        calls
    }

    fn parameter_failures<'e>(
        &self,
        declaration: &'e AuthoredExpr<ParserPlaceholder, B>,
        call: &'e AuthoredExpr<ParserPlaceholder, B>,
        in_body: bool,
    ) -> Vec<(&'e ExprValue<ParserPlaceholder, B>, InferenceError)> {
        let ExprKind::Function { signature, name } = &declaration.kind else {
            return Vec::new();
        };
        signature
            .parameters
            .iter()
            .enumerate()
            .filter_map(|(index, (parameter, ty))| {
                let ty = ty.as_ref()?;
                let argument = argument(declaration, call, index)?;
                if if in_body {
                    self.known_in_body(argument)
                } else {
                    self.known(argument)
                } {
                    return None;
                }
                self.promise_error(
                    argument,
                    ty,
                    name,
                    FunctionTypePosition::Parameter(parameter),
                )
                .map(|failure| (argument, failure))
            })
            .collect()
    }

    fn schema_unavailable(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        let mut pending = vec![expression];
        let mut visited = HashSet::new();
        while let Some(expression) = pending.pop() {
            let node = &expression.authored;
            if !visited.insert(node.id) {
                continue;
            }
            if matches!(
                self.report
                    .node(&node.location.instance, node.id)
                    .map(|judged| judged.static_type()),
                Some(Err(InferenceIssue::NotInferable(
                    NotInferableReason::SchemaUnavailable { .. }
                        | NotInferableReason::CustomTypeSchemaUnavailable { .. }
                )))
            ) {
                return true;
            }
            let unavailable = match self.projection(expression) {
                Some(Value::Deferred(DeferredValue::ResourceRef { path })) => {
                    self.bindings.upstream_is_unavailable(path.binding())
                }
                Some(Value::Deferred(DeferredValue::BindingRef { binding })) => {
                    self.bindings.upstream_is_unavailable(binding.as_str())
                }
                _ => match &node.kind {
                    ExprKind::Reference { binding, .. } | ExprKind::Binding(binding) => {
                        self.bindings.upstream_is_unavailable(binding.as_str())
                    }
                    _ => false,
                },
            };
            if unavailable {
                return true;
            }
            pending.extend(&node.children);
        }
        false
    }

    fn uses_module_argument(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        if matches!(
            expression.authored.location.instance,
            ExpressionInstance::Root
        ) || self.expressions.arguments.is_empty()
        {
            return false;
        }
        let mut pending = vec![expression];
        let mut visited = HashSet::new();
        while let Some(expression) = pending.pop() {
            let node = &expression.authored;
            if !visited.insert((node.location.instance.clone(), node.id)) {
                continue;
            }
            if let ExprKind::Binding(binding) | ExprKind::Reference { binding, .. } = &node.kind {
                if self.expressions.arguments.contains_key(binding) {
                    return true;
                }
                pending.extend(self.expressions.definitions.get(binding));
            }
            pending.extend(&node.children);
        }
        false
    }

    fn source(&self, expression: &ExprValue<ParserPlaceholder, B>) -> Source {
        let expressions = self.expressions;
        let report = self.report;
        let bindings = self.bindings;
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
            return self.source(selected);
        }
        let judged = report.node(&node.location.instance, node.id);
        let mut source = Source {
            reference: None,
            ty: judged.and_then(|judged| judged.static_type().ok()).cloned(),
            untyped_export: None,
            unavailable: self.schema_unavailable(expression),
            pending_substitution: self
                .projection(expression)
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
        // The authored judge already uses the child's declared argument type,
        // including through aliases and accesses. A substituted projection is
        // value evidence, not the caller's type or upstream-export provenance.
        if self.uses_module_argument(expression) {
            if let Some(value) = self.projection(expression) {
                value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
                    source.unavailable |= reference
                        .check(|path| {
                            Ok::<_, InferenceError>(
                                bindings.upstream_is_unavailable(path.binding())
                                    || (bindings.references_resource_attribute(path.into())
                                        && matches!(
                                            bindings.ref_type(ReferencePathRef::Access(path)),
                                            RefType::Unchecked | RefType::UnknownBinding { .. }
                                        )),
                            )
                        })
                        .unwrap_or(false);
                });
            }
            return source;
        }
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
        let path = match self.projection(expression) {
            Some(Value::Deferred(DeferredValue::ResourceRef { path })) => Some(path),
            _ => authored_path.as_ref(),
        };
        if let Some(path) = path {
            source.pending_substitution |= pending_argument(expressions, bindings, path.binding());
            source.unavailable |= bindings.upstream_is_unavailable(path.binding());
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
                        if !self.definitions.contains(path.binding()) {
                            source.ty = None;
                            source.incomplete_context = true;
                            source.unavailable |=
                                bindings.references_resource_attribute(path.into());
                        }
                    }
                    RefType::UnknownAttribute(_) => {}
                }
            }
        } else if let Some(Value::Deferred(DeferredValue::BindingRef { binding })) =
            self.projection(expression)
        {
            source.pending_substitution |=
                pending_argument(expressions, bindings, binding.as_str());
            source.unavailable |= bindings.upstream_is_unavailable(binding.as_str());
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

    fn relevance(
        &self,
        expression: &ExprValue<ParserPlaceholder, B>,
        parameters: &[NodeId],
        verdicts: &HashMap<DeclarationKey, Arc<BodyVerdict>>,
    ) -> Relevance {
        let mut result = Relevance::default();
        let mut pending = vec![expression];
        let mut visited = HashSet::new();
        while let Some(expression) = pending.pop() {
            let node = &expression.authored;
            if !visited.insert(node.id) {
                continue;
            }
            if let Some(index) = parameters.iter().position(|id| *id == node.id) {
                result.parameters.insert(index);
                continue;
            }
            if self.known_in_body(expression) {
                continue;
            }
            if matches!(node.kind, ExprKind::Sequence) {
                if let Some(selected) = node
                    .result_index()
                    .and_then(|index| node.children.get(index))
                {
                    pending.push(selected);
                }
                continue;
            }
            if let ExprKind::Call(name) = &node.kind
                && let Some(verdict) = verdicts.get(&(node.location.instance.clone(), name.clone()))
            {
                result.independent |= verdict.unknown_capture;
                pending.extend(&node.children);
                continue;
            }
            result.independent |= node.children.is_empty();
            pending.extend(&node.children);
        }
        result
    }
}

fn argument<'a, B: BindingAddress>(
    declaration: &'a AuthoredExpr<ParserPlaceholder, B>,
    call: &'a AuthoredExpr<ParserPlaceholder, B>,
    index: usize,
) -> Option<&'a ExprValue<ParserPlaceholder, B>> {
    let ExprKind::Function { signature, .. } = &declaration.kind else {
        return None;
    };
    call.children.get(index).or_else(|| {
        let id = signature.default_nodes.get(index).copied().flatten()?;
        declaration
            .children
            .iter()
            .find(|child| child.node_id() == id)
    })
}

fn body_verdict<B: BindingAddress>(
    key: &DeclarationKey,
    evidence: &Evidence<'_, '_, B>,
    declarations: &IndexMap<DeclarationKey, &AuthoredExpr<ParserPlaceholder, B>>,
    verdicts: &mut HashMap<DeclarationKey, Arc<BodyVerdict>>,
    visiting: &mut HashSet<DeclarationKey>,
    findings: &mut Vec<Finding>,
) -> Arc<BodyVerdict> {
    if let Some(verdict) = verdicts.get(key) {
        return verdict.clone();
    }
    let declaration = declarations[key];
    if evidence
        .report
        .node(&declaration.location.instance, declaration.id)
        .is_some_and(|judged| judged.failed())
    {
        // The authored judge owns definite declaration failures and their text.
        let verdict = Arc::new(BodyVerdict::default());
        verdicts.insert(key.clone(), verdict.clone());
        return verdict;
    }
    if !visiting.insert(key.clone()) {
        return Arc::new(BodyVerdict::default());
    }
    let ExprKind::Function { signature, name } = &declaration.kind else {
        unreachable!()
    };
    let AuthoredChildRoles::Function { body } = declaration.roles else {
        unreachable!()
    };
    let body = declaration
        .children
        .iter()
        .find(|child| child.node_id() == body)
        .expect("authored function body");
    let mut verdict = BodyVerdict::default();
    if let Some(ty) = &signature.return_type
        && !evidence.known_in_body(body)
        && let Some(failure) = evidence.promise_error(body, ty, name, FunctionTypePosition::Return)
    {
        if failure.is_function_type_promise() {
            verdict.result = Some(failure);
        } else {
            push_finding(findings, &body.authored, failure, evidence.report);
        }
    }
    let calls: Vec<_> = evidence
        .calls([body], true)
        .into_iter()
        .filter_map(|expression| {
            let node = expression.authored.as_ref();
            let ExprKind::Call(name) = &node.kind else {
                return None;
            };
            let callee = (node.location.instance.clone(), name.clone());
            declarations.contains_key(&callee).then_some((node, callee))
        })
        .collect();
    // Dependency summaries, like the body types, are cached per declaration.
    for (_, callee) in &calls {
        body_verdict(callee, evidence, declarations, verdicts, visiting, findings);
    }
    for (call, callee) in calls {
        if evidence
            .report
            .node(&call.location.instance, call.id)
            .is_some_and(|judged| judged.failed())
        {
            continue;
        }
        let target = declarations[&callee];
        let ExprKind::Function {
            signature: target_signature,
            ..
        } = &target.kind
        else {
            unreachable!()
        };
        for (argument, failure) in evidence.parameter_failures(target, call, true) {
            if failure.is_function_type_promise() {
                let relevance = evidence.relevance(argument, &signature.parameter_nodes, verdicts);
                if relevance.independent || !relevance.parameters.is_empty() {
                    verdict.nested.push(BodyFinding {
                        error: failure,
                        relevance,
                    });
                }
            } else {
                push_finding(findings, call, failure, evidence.report);
            }
        }
        let Some(target_verdict) = verdicts.get(&callee) else {
            continue;
        };
        let mut nested = target_verdict.nested.clone();
        if let Some(failure) = &target_verdict.result {
            nested.push(BodyFinding {
                error: failure.clone(),
                relevance: Relevance {
                    parameters: (0..target_signature.parameters.len()).collect(),
                    independent: target_verdict.unknown_capture,
                },
            });
        }
        for finding in nested {
            let mut relevance = Relevance {
                independent: finding.relevance.independent,
                ..Default::default()
            };
            for index in finding.relevance.parameters {
                if let Some(argument) = argument(target, call, index) {
                    let next = evidence.relevance(argument, &signature.parameter_nodes, verdicts);
                    relevance.independent |= next.independent;
                    relevance.parameters.extend(next.parameters);
                }
            }
            if relevance.independent || !relevance.parameters.is_empty() {
                verdict.nested.push(BodyFinding {
                    error: finding.error,
                    relevance,
                });
            }
        }
    }
    verdict.unknown_capture = evidence
        .relevance(body, &signature.parameter_nodes, verdicts)
        .independent;
    visiting.remove(key);
    let verdict = Arc::new(verdict);
    verdicts.insert(key.clone(), verdict.clone());
    verdict
}

pub(super) fn check<B: BindingAddress>(
    expressions: &Expressions<B>,
    report: &ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    complete_sources: bool,
) -> Vec<Finding> {
    let evidence = Evidence::new(expressions, report, bindings, complete_sources);
    let mut declarations = IndexMap::new();
    for node in report.syntax_nodes.values() {
        if let ExprKind::Function { name, .. } = &node.kind {
            declarations
                .entry((node.location.instance.clone(), name.clone()))
                .or_insert(node.as_ref());
        }
    }
    let mut verdicts = HashMap::new();
    let mut findings = Vec::new();
    for key in declarations.keys() {
        body_verdict(
            key,
            &evidence,
            &declarations,
            &mut verdicts,
            &mut HashSet::new(),
            &mut findings,
        );
    }
    for expression in evidence.calls(
        expressions
            .roots
            .iter()
            .chain(expressions.definitions.values()),
        false,
    ) {
        let node = &expression.authored;
        let ExprKind::Call(name) = &node.kind else {
            continue;
        };
        let key = (node.location.instance.clone(), name.clone());
        let Some(declaration) = declarations.get(&key).copied() else {
            continue;
        };
        // No projection means main has not evaluated this occurrence (empty loops).
        if evidence.projection(expression).is_none()
            || report
                .node(&node.location.instance, node.id)
                .is_some_and(|judged| judged.failed())
        {
            continue;
        }
        if evidence
            .projection(expression)
            .is_some_and(super::super::functions::contains_module_argument)
            || node.children.iter().any(|argument| {
                let source = evidence.source(argument);
                source.pending_substitution
                    || (!complete_sources && (source.unavailable || source.incomplete_context))
            })
        {
            continue;
        }
        for (_, failure) in evidence.parameter_failures(declaration, node, false) {
            push_finding(&mut findings, node, failure, report);
        }
        let verdict = &verdicts[&key];
        for finding in &verdict.nested {
            if finding.relevance.independent
                || finding.relevance.parameters.iter().any(|index| {
                    argument(declaration, node, *index).is_some_and(|argument| {
                        !evidence.known(argument) && !evidence.source(argument).unavailable
                    })
                })
            {
                push_finding(&mut findings, node, finding.error.clone(), report);
            }
        }
        if !node
            .children
            .iter()
            .any(|argument| evidence.source(argument).unavailable)
            && !evidence.known(expression)
            && let Some(failure) = &verdict.result
        {
            push_finding(&mut findings, node, failure.clone(), report);
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
