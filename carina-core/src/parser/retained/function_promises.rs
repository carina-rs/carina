//! Function policy over the existing authored judgements and parser projections.
//! Bodies are never instantiated here: their parameter nodes already carry the
//! declared types, and each declaration's return and nested-call verdicts are cached once.

use super::*;
use crate::binding_index::{
    BindingIndex, BindingTarget, RefType, ResolvedBindings, ResolvedRefType,
};
use crate::resource::{AccessPath, DeferredValue, ReferencePathRef};
use crate::static_type::{
    ConsumerExportStaticType, InferenceError, InferenceIssue, NotInferableReason, StaticType,
};
use crate::upstream_exports::UpstreamExportType;
use crate::validation::admission::static_judgement::{
    StaticAssignmentError, StaticAssignmentSource, StaticAssignmentVerdict,
    judge_static_assignment, type_needs_schema,
};
use crate::validation::typecheck::{
    FunctionTypePosition, FunctionTypeSource, WritableType, format_function_type_promise_error,
};
use std::collections::BTreeSet;

/// A non-stopping function finding. Only this policy pass can construct one.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionTypePromise {
    reason: String,
}

impl std::fmt::Display for FunctionTypePromise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&crate::provider_text::DiagnosticText::new(&self.reason), f)
    }
}

// Only ready evidence can be submitted to the shared admission relation.
// Missing schemas, substitutions, and incomplete references have no judgement.
enum Source<'a> {
    Deferred,
    Ready(ReadySource<'a>),
}

enum ReadySource<'a> {
    Reference(ResolvedRefType),
    Element(ResolvedRefType),
    Expression(&'a TypeExpr),
    Authored(&'a JudgedExpr),
    UntypedExport(String),
    UntypedParameter {
        function: &'a str,
        parameter: &'a str,
    },
    Unknown,
}

struct Finding {
    node: (ExpressionInstance, NodeId),
    diagnostic: Located<FunctionTypePromise>,
}

impl ReadySource<'_> {
    fn assignment(&self) -> StaticAssignmentSource<'_> {
        match self {
            Self::Reference(reference) => StaticAssignmentSource::Reference(reference),
            Self::Element(reference) => StaticAssignmentSource::Element(reference),
            Self::Expression(ty) => StaticAssignmentSource::Expression(ty),
            Self::Authored(expression) => StaticAssignmentSource::Judged(expression),
            Self::UntypedExport(_) | Self::UntypedParameter { .. } | Self::Unknown => {
                StaticAssignmentSource::Unknown
            }
        }
    }

    fn diagnostic_source(&self, declared: &TypeExpr) -> FunctionTypeSource<'_> {
        match self {
            Self::UntypedExport(export) => FunctionTypeSource::UntypedExport(export),
            Self::UntypedParameter {
                function,
                parameter,
            } => FunctionTypeSource::UntypedParameter {
                function,
                parameter,
            },
            Self::Reference(reference) => FunctionTypeSource::Typed {
                display: reference.display_type_name(),
                writable: WritableType::reference(reference),
            },
            Self::Element(reference) => match reference.iterable_constraints().next() {
                Some(ty) => FunctionTypeSource::Typed {
                    display: ty.resolved_type_name(),
                    writable: WritableType::schema(ty),
                },
                None => FunctionTypeSource::Unknown,
            },
            Self::Expression(ty) => FunctionTypeSource::Typed {
                display: ty.to_string(),
                writable: Some(WritableType::expression(ty)),
            },
            Self::Authored(expression) => match expression.static_type() {
                Ok(ty) => FunctionTypeSource::Typed {
                    display: ty.to_string(),
                    // A map-shaped literal can be evidence for a struct. Its
                    // inferred homogeneous type is not a useful repair.
                    writable: (!(matches!(declared, TypeExpr::Struct { .. })
                        && matches!(ty, TypeExpr::Map(_))))
                    .then(|| WritableType::expression(ty)),
                },
                Err(_) => FunctionTypeSource::Unknown,
            },
            Self::Unknown => FunctionTypeSource::Unknown,
        }
    }
}

#[derive(Clone, Default)]
struct Relevance {
    parameters: BTreeSet<usize>,
    independent: bool,
}

#[derive(Clone)]
struct BodyFinding {
    error: FunctionTypePromise,
    relevance: Relevance,
}

#[derive(Default)]
struct BodyVerdict {
    result: Option<BodyFinding>,
    nested: Vec<BodyFinding>,
    result_relevance: Relevance,
}

type DeclarationKey = (ExpressionInstance, String);

struct Evidence<'a, 'index, B: BindingAddress> {
    expressions: &'a Expressions<B>,
    report: &'a ExpressionReport<B>,
    bindings: &'a BindingIndex<'index, B>,
    values: &'a ResolvedBindings<ParserPlaceholder, B>,
    projections: HashMap<(ExpressionInstance, NodeId), &'a Value<ParserPlaceholder, B>>,
    definitions: HashSet<&'a str>,
    untyped_parameters: HashMap<(ExpressionInstance, NodeId), (&'a str, &'a str)>,
    syntax: HashMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
}

impl<'a, 'index, B: BindingAddress> Evidence<'a, 'index, B> {
    fn new(
        expressions: &'a Expressions<B>,
        report: &'a ExpressionReport<B>,
        bindings: &'a BindingIndex<'index, B>,
        values: &'a ResolvedBindings<ParserPlaceholder, B>,
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
        let mut untyped_parameters = HashMap::new();
        for node in report.syntax_nodes.values() {
            if let ExprKind::Function { name, signature } = &node.kind {
                for ((parameter, ty), id) in
                    signature.parameters.iter().zip(&signature.parameter_nodes)
                {
                    if ty.is_none() {
                        untyped_parameters.insert(
                            (node.location.instance.clone(), *id),
                            (name.as_str(), parameter.as_str()),
                        );
                    }
                }
            }
        }
        let mut syntax = HashMap::new();
        let mut visited = HashSet::new();
        let mut pending: Vec<_> = expressions
            .roots
            .iter()
            .chain(expressions.definitions.values())
            .collect();
        while let Some(expression) = pending.pop() {
            let node = &expression.authored;
            if visited.insert((node.location.instance.clone(), node.id)) {
                syntax.entry(node.location.clone()).or_insert(expression);
                pending.extend(&node.children);
            }
        }
        Self {
            syntax,
            expressions,
            report,
            bindings,
            values,
            projections,
            untyped_parameters,
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

    fn known_value(
        &self,
        expression: &ExprValue<ParserPlaceholder, B>,
    ) -> Option<Value<ParserPlaceholder, RelativeBindingName>> {
        let Some(value) = self.projection(expression) else {
            let node = &expression.authored;
            let child = match node.kind {
                ExprKind::Alias | ExprKind::Parens | ExprKind::Definition | ExprKind::Iterable => {
                    node.children.first()
                }
                ExprKind::Sink { .. } => node.sink_value(),
                ExprKind::Sequence => node
                    .result_index()
                    .and_then(|index| node.children.get(index)),
                _ => None,
            }?;
            return self.known_value(child);
        };
        // Use the resolver's validation view, just like ordinary value checks.
        // A configured literal remains known when reached through another resource.
        let value = match crate::resolver::project_ref_value_status(
            value,
            self.values,
            self.bindings.provider_context(),
        ) {
            Ok(
                crate::resolver::ValueResolution::Present(value)
                | crate::resolver::ValueResolution::Unknown(value),
            ) => value,
            _ => return None,
        };
        let value = value.map_bindings(&mut |binding| RelativeBindingName::new(binding.as_str()));
        let Ok(knowledge) =
            value.knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        else {
            return None;
        };
        let knowledge = knowledge.into_classification();
        if !knowledge.permits_static_evaluation() {
            return None;
        }
        super::super::evaluate_static_value_for_validation(
            &knowledge,
            self.bindings.provider_context(),
        )
        .filter(|value| {
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .is_ok_and(|proof| proof.into_classification().into_known().is_ok())
        })
    }

    fn known(&self, expression: &ExprValue<ParserPlaceholder, B>) -> bool {
        self.known_value(expression).is_some()
    }

    fn known_judgement(&self, expression: &JudgedExpr) -> bool {
        expression.concrete()
            || expression
                .origin()
                .and_then(|origin| self.syntax.get(origin))
                .is_some_and(|expression| self.known(expression))
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
    ) -> Option<FunctionTypePromise> {
        let judged = self
            .report
            .node(&expression.authored.location.instance, expression.node_id())?;
        if judged.failed() {
            return None;
        }
        let Source::Ready(source) = self.evidence(expression) else {
            return None;
        };
        match judge_static_assignment(source.assignment(), ty, self.bindings, &|expression| {
            self.known_judgement(expression)
        }) {
            Ok(StaticAssignmentVerdict::NeedsLocal) => Some(FunctionTypePromise {
                reason: format_function_type_promise_error(
                    name,
                    position,
                    ty,
                    source.diagnostic_source(ty),
                ),
            }),
            // Existing judgement owns definite mismatches and annotation errors.
            Ok(StaticAssignmentVerdict::Proven | StaticAssignmentVerdict::NeedsSchema)
            | Err(StaticAssignmentError::Mismatch | StaticAssignmentError::Resolution) => None,
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
            if matches!(node.kind, ExprKind::For { .. }) {
                pending.extend(node.children.first());
                let empty = node
                    .children
                    .first()
                    .and_then(|iterable| self.known_value(iterable))
                    .is_some_and(|value| match value {
                        Value::Concrete(crate::resource::ConcreteValue::List(values)) => {
                            values.is_empty()
                        }
                        Value::Concrete(crate::resource::ConcreteValue::Map(values)) => {
                            values.is_empty()
                        }
                        _ => false,
                    })
                    || node
                        .children
                        .first()
                        .and_then(|iterable| {
                            self.report
                                .node(&iterable.authored.location.instance, iterable.node_id())
                        })
                        .is_some_and(|judged| judged.known_empty_collection());
                if !empty {
                    pending.extend(node.children.iter().skip(1).rev());
                }
            } else if matches!(node.kind, ExprKind::If) {
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
    ) -> Vec<(&'e ExprValue<ParserPlaceholder, B>, FunctionTypePromise)> {
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
                    ExprKind::Reference { binding, .. }
                    | ExprKind::Binding {
                        binding,
                        authored_binding: _,
                    } => self.bindings.upstream_is_unavailable(binding.as_str()),
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
            if let ExprKind::Binding {
                binding,
                authored_binding: _,
            }
            | ExprKind::Reference { binding, .. } = &node.kind
            {
                if self.expressions.arguments.contains_key(binding) {
                    return true;
                }
                pending.extend(self.expressions.definitions.get(binding));
            }
            pending.extend(&node.children);
        }
        false
    }

    fn evidence(&self, expression: &ExprValue<ParserPlaceholder, B>) -> Source<'_> {
        let node = &expression.authored;
        let selected = match node.kind {
            ExprKind::Sequence => node
                .result_index()
                .and_then(|index| node.children.get(index)),
            ExprKind::Alias | ExprKind::Parens | ExprKind::Definition | ExprKind::Iterable => {
                node.children.first()
            }
            _ => None,
        };
        if let Some(selected) = selected {
            return self.evidence(selected);
        }
        let bindings = self.bindings;
        let judged = self.report.node(&node.location.instance, node.id);
        let ty = judged.and_then(|judged| judged.static_type().ok());
        if self.schema_unavailable(expression)
            || ty.is_some_and(|ty| type_needs_schema(ty, bindings.provider_context()))
            || self
                .projection(expression)
                .is_some_and(super::super::functions::contains_module_argument)
        {
            return Source::Deferred;
        }
        if matches!(node.kind, ExprKind::Element)
            && ty.is_none()
            && let Some(collection) = node.children.first()
        {
            return match self.evidence(collection) {
                Source::Ready(ReadySource::Reference(reference)) => {
                    Source::Ready(ReadySource::Element(reference))
                }
                Source::Ready(ReadySource::Expression(
                    TypeExpr::List(inner) | TypeExpr::Map(inner),
                )) => Source::Ready(ReadySource::Expression(inner)),
                Source::Ready(ReadySource::Authored(collection)) => {
                    match collection.static_type() {
                        Ok(TypeExpr::List(inner) | TypeExpr::Map(inner)) => {
                            Source::Ready(ReadySource::Expression(inner))
                        }
                        _ => Source::Ready(ReadySource::Authored(collection)),
                    }
                }
                other => other,
            };
        }
        let authored = || {
            if let Some(&(function, parameter)) = self
                .untyped_parameters
                .get(&(node.location.instance.clone(), node.id))
            {
                ReadySource::UntypedParameter {
                    function,
                    parameter,
                }
            } else {
                judged.map_or(ReadySource::Unknown, ReadySource::Authored)
            }
        };
        // The child's declared argument type is authoritative. Its projection
        // contributes known values, never the caller's type or export name.
        if self.uses_module_argument(expression) {
            let mut unavailable = false;
            if let Some(value) = self.projection(expression) {
                value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
                    unavailable |= reference
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
            return if unavailable {
                Source::Deferred
            } else {
                Source::Ready(authored())
            };
        }
        let authored_path = match &node.kind {
            ExprKind::Reference {
                binding,
                authored_binding: _,
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
            if pending_argument(self.expressions, bindings, path.binding())
                || bindings.upstream_is_unavailable(path.binding())
            {
                return Source::Deferred;
            }
            if let Some(entry) = bindings.get(path.binding())
                && let BindingTarget::UpstreamState(exports) = entry.target
                && let Some(export) = exports.get(path.attribute())
            {
                match &export.static_type {
                    UpstreamExportType::Static(ConsumerExportStaticType::Inferred(_)) => {
                        return Source::Ready(ReadySource::UntypedExport(format!(
                            "{}.{}",
                            path.binding(),
                            path.attribute()
                        )));
                    }
                    UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                        StaticType::Unchecked(_),
                    )) => return Source::Deferred,
                    _ => {}
                }
            }
            return match bindings.ref_type(ReferencePathRef::Access(path)) {
                RefType::Typed(reference) => Source::Ready(ReadySource::Reference(reference)),
                RefType::UnknownAttribute(_) => Source::Deferred,
                RefType::Unchecked | RefType::UnknownBinding { .. }
                    if !self.definitions.contains(path.binding()) =>
                {
                    Source::Deferred
                }
                _ => Source::Ready(authored()),
            };
        }
        if let Some(Value::Deferred(DeferredValue::BindingRef { binding })) =
            self.projection(expression)
        {
            if pending_argument(self.expressions, bindings, binding.as_str())
                || bindings.upstream_is_unavailable(binding.as_str())
            {
                return Source::Deferred;
            }
            return match bindings.ref_type(ReferencePathRef::Binding(binding)) {
                RefType::Typed(reference) => Source::Ready(ReadySource::Reference(reference)),
                RefType::UnknownAttribute(_) => Source::Deferred,
                RefType::Unchecked | RefType::UnknownBinding { .. }
                    if !self.expressions.definitions.contains_key(binding) =>
                {
                    Source::Deferred
                }
                _ => Source::Ready(authored()),
            };
        }
        Source::Ready(authored())
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
                result.independent |= verdict.result_relevance.independent;
                pending.extend(
                    verdict
                        .result_relevance
                        .parameters
                        .iter()
                        .filter_map(|index| node.children.get(*index)),
                );
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
        verdict.result = Some(BodyFinding {
            error: failure,
            relevance: Relevance::default(),
        });
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
        body_verdict(callee, evidence, declarations, verdicts, visiting);
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
        for (argument, failure) in evidence.parameter_failures(target, call, true) {
            let relevance = evidence.relevance(argument, &signature.parameter_nodes, verdicts);
            if relevance.independent || !relevance.parameters.is_empty() {
                verdict.nested.push(BodyFinding {
                    error: failure,
                    relevance,
                });
            }
        }
        let Some(target_verdict) = verdicts.get(&callee) else {
            continue;
        };
        let mut nested = target_verdict.nested.clone();
        if let Some(failure) = &target_verdict.result {
            nested.push(failure.clone());
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
    let relevance = evidence.relevance(body, &signature.parameter_nodes, verdicts);
    verdict.result_relevance = relevance.clone();
    if let Some(result) = &mut verdict.result {
        result.relevance = relevance;
    }
    visiting.remove(key);
    let verdict = Arc::new(verdict);
    verdicts.insert(key.clone(), verdict.clone());
    verdict
}

pub(super) fn check<B: BindingAddress>(
    expressions: &Expressions<B>,
    report: &ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    values: &ResolvedBindings<ParserPlaceholder, B>,
) -> Vec<Located<FunctionTypePromise>> {
    let evidence = Evidence::new(expressions, report, bindings, values);
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
        if report
            .node(&node.location.instance, node.id)
            .is_some_and(|judged| judged.failed())
        {
            continue;
        }
        if node
            .children
            .iter()
            .any(|argument| matches!(evidence.evidence(argument), Source::Deferred))
        {
            continue;
        }
        for (_, failure) in evidence.parameter_failures(declaration, node, false) {
            push_finding(&mut findings, node, failure, report);
        }
        let verdict = &verdicts[&key];
        for finding in verdict.nested.iter().chain(verdict.result.iter()) {
            if finding.relevance.independent
                || finding.relevance.parameters.iter().any(|index| {
                    argument(declaration, node, *index).is_some_and(|argument| {
                        !evidence.known(argument)
                            && matches!(evidence.evidence(argument), Source::Ready(_))
                    })
                })
            {
                push_finding(&mut findings, node, finding.error.clone(), report);
            }
        }
    }
    findings.sort_by(|a, b| a.diagnostic.location().cmp(b.diagnostic.location()));
    findings
        .into_iter()
        .map(|finding| finding.diagnostic)
        .collect()
}

fn push_finding<B: BindingAddress>(
    findings: &mut Vec<Finding>,
    node: &AuthoredExpr<ParserPlaceholder, B>,
    mut error: FunctionTypePromise,
    report: &ExpressionReport<B>,
) {
    if report
        .node(&node.location.instance, node.id)
        .is_some_and(|judged| judged.diagnostic_is_secret())
    {
        error.reason = crate::resource::INSIDE_SECRET_MESSAGE.into();
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
