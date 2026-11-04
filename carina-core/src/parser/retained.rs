//! Authored expressions and their provisional projections live on separate axes.
//!
//! The syntax tree is captured before evaluation, including operands that an
//! enclosing operation does not evaluate. Only the judge can mint admission
//! evidence. A projection is useful for identities and expansion, but cannot
//! be turned back into an authored expression.

use super::{DeclarationLocation, Located, PreprocessedFile, Rule, TypeExpr};
use crate::resource::{
    BindingAddress, ParserPlaceholder, PathSegment, RelativeBindingName, Value, ValueKind,
};
use indexmap::IndexMap;
use pest::iterators::{Pair, Pairs};
use std::{collections::HashMap, path::Path, sync::Arc};

#[cfg(test)]
mod tests;

/// The expansion scope is part of expression identity, even when two module
/// instances originate at the same authored source location.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionInstance {
    #[default]
    Root,
    Module(Arc<str>),
}

/// A failed evidence lookup is an invariant error, never deferred authority.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("missing expression evidence: {request}")]
pub struct MissingEvidence {
    request: String,
}
impl MissingEvidence {
    pub(crate) fn new(request: impl Into<String>) -> Self {
        Self {
            request: request.into(),
        }
    }
    pub fn for_resource(id: &crate::resource::ResourceId) -> Self {
        Self::new(format!("resource {id}"))
    }
}

impl ExpressionInstance {
    pub(crate) fn for_scope(scope: &crate::resource::ModuleInstanceScope) -> Self {
        Self::Module(Arc::from(scope.key()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExpressionLocation {
    instance: ExpressionInstance,
    authored: DeclarationLocation,
}

impl ExpressionLocation {
    fn new(instance: ExpressionInstance, authored: &DeclarationLocation) -> Self {
        Self {
            instance,
            authored: authored.clone(),
        }
    }
}

/// An expression retains its complete authored root and provisional payload.
/// Neither field nor a constructor from a reduced `Value` is public.
///
/// ```compile_fail,E0277
/// use carina_core::parser::retained::ExprValue;
/// use carina_core::resource::{AuthoredValue, Value, ConcreteValue};
/// let reduced: AuthoredValue = Value::Concrete(ConcreteValue::String("selected".into()));
/// let expression: ExprValue = reduced.into();
/// ```
///
/// ```compile_fail,E0616
/// use carina_core::parser::retained::ExprValue;
/// fn discard_authorship(expression: ExprValue) { let _ = expression.payload; }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ExprValue<P: ValueKind = ParserPlaceholder, B: BindingAddress = RelativeBindingName> {
    authored: Arc<AuthoredExpr<P, B>>,
    payload: Projection<P, B>,
}

#[derive(Debug, Clone, PartialEq)]
enum Projection<P: ValueKind, B: BindingAddress> {
    Pending,
    Value(Value<P, B>),
}

#[derive(Debug, Clone, PartialEq)]
struct AuthoredExpr<P: ValueKind, B: BindingAddress> {
    location: ExpressionLocation,
    kind: ExprKind<B>,
    children: Vec<ExprValue<P, B>>,
}

#[derive(Debug, Clone, PartialEq)]
enum ExprKind<B: BindingAddress> {
    Scalar(TypeExpr),
    Parameter(TypeExpr),
    UntypedParameter,
    Binding(B),
    Reference {
        binding: B,
        attribute: String,
        segments: Vec<PathSegment>,
    },
    List,
    Map(Vec<String>),
    NestedBlock {
        name: String,
        header: DeclarationLocation,
    },
    Interpolation,
    EmptyInterpolation,
    Coalesce,
    Call(String),
    Function {
        name: String,
        signature: Arc<FunctionSignature>,
    },
    Sequence,
    Access(PathSegment),
    Integer {
        op: crate::integer_arithmetic::IntegerOp,
        location: DeclarationLocation,
    },
    Alias,
    Element,
    Iterable,
    Resource {
        provider: String,
        resource_type: String,
        read: bool,
    },
    Sink(String),
    If,
    For,
    Wait,
    Compose,
    DynamicAccess,
    ModuleCall,
    Boolean,
    Declaration,
    ProviderDeclaration,
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
struct FunctionSignature {
    parameters: Vec<(String, Option<TypeExpr>)>,
    return_type: Option<TypeExpr>,
}

impl<P: ValueKind, B: BindingAddress> ExprValue<P, B> {
    pub fn projection(&self) -> Option<&Value<P, B>> {
        match &self.payload {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn location(&self) -> &DeclarationLocation {
        &self.authored.location.authored
    }

    fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
        memo: &mut HashMap<usize, Arc<AuthoredExpr<P, C>>>,
    ) -> ExprValue<P, C> {
        let id = Arc::as_ptr(&self.authored) as usize;
        if let Some(authored) = memo.get(&id) {
            return ExprValue {
                authored: authored.clone(),
                payload: Projection::Pending,
            };
        }
        let authored = Arc::unwrap_or_clone(self.authored);
        let kind = match authored.kind {
            ExprKind::Binding(binding) => ExprKind::Binding(map(binding)),
            ExprKind::Reference {
                binding,
                attribute,
                segments,
            } => ExprKind::Reference {
                binding: map(binding),
                attribute,
                segments,
            },
            ExprKind::Scalar(ty) => ExprKind::Scalar(ty),
            ExprKind::Parameter(ty) => ExprKind::Parameter(ty),
            ExprKind::UntypedParameter => ExprKind::UntypedParameter,
            ExprKind::List => ExprKind::List,
            ExprKind::Map(keys) => ExprKind::Map(keys),
            ExprKind::NestedBlock { name, header } => ExprKind::NestedBlock { name, header },
            ExprKind::Interpolation => ExprKind::Interpolation,
            ExprKind::EmptyInterpolation => ExprKind::EmptyInterpolation,
            ExprKind::Coalesce => ExprKind::Coalesce,
            ExprKind::Call(name) => ExprKind::Call(name),
            ExprKind::Function { name, signature } => ExprKind::Function { name, signature },
            ExprKind::Sequence => ExprKind::Sequence,
            ExprKind::Access(step) => ExprKind::Access(step),
            ExprKind::Integer { op, location } => ExprKind::Integer { op, location },
            ExprKind::Alias => ExprKind::Alias,
            ExprKind::Element => ExprKind::Element,
            ExprKind::Iterable => ExprKind::Iterable,
            ExprKind::Resource {
                provider,
                resource_type,
                read,
            } => ExprKind::Resource {
                provider,
                resource_type,
                read,
            },
            ExprKind::Sink(name) => ExprKind::Sink(name),
            ExprKind::If => ExprKind::If,
            ExprKind::For => ExprKind::For,
            ExprKind::Wait => ExprKind::Wait,
            ExprKind::Compose => ExprKind::Compose,
            ExprKind::DynamicAccess => ExprKind::DynamicAccess,
            ExprKind::ModuleCall => ExprKind::ModuleCall,
            ExprKind::Boolean => ExprKind::Boolean,
            ExprKind::Declaration => ExprKind::Declaration,
            ExprKind::ProviderDeclaration => ExprKind::ProviderDeclaration,
            ExprKind::Unsupported(rule) => ExprKind::Unsupported(rule),
        };
        let authored = Arc::new(AuthoredExpr {
            location: authored.location,
            kind,
            children: authored
                .children
                .into_iter()
                .map(|child| child.map_bindings(map, memo))
                .collect(),
        });
        memo.insert(id, authored.clone());
        ExprValue {
            authored,
            // A parser projection is never binding-phase authority. The next
            // phase attaches its own projection without changing the root.
            payload: Projection::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SinkExpression {
    expression: ExprValue,
    payload: crate::eval_value::ParsedSinkValue,
}

impl SinkExpression {
    pub(in crate::parser) fn projection(&self) -> &crate::eval_value::ParsedSinkValue {
        &self.payload
    }
}

pub(in crate::parser) fn evaluate(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    reduce: impl FnOnce() -> Result<super::ParsedValue, super::ParseError>,
) -> Result<ExprValue, super::ParseError> {
    let mut expression = compile(pair, file, source, &HashMap::new());
    expression.payload = Projection::Value(reduce()?);
    Ok(expression)
}

pub(in crate::parser) fn evaluate_sink(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    reduce: impl FnOnce() -> Result<crate::eval_value::ParsedSinkValue, super::ParseError>,
) -> Result<SinkExpression, super::ParseError> {
    let mut expression = compile(pair, file, source, &HashMap::new());
    let payload = reduce()?;
    if let crate::eval_value::ParsedSinkValue::Ready(value) = &payload {
        expression.payload = Projection::Value(value.clone());
    }
    Ok(SinkExpression {
        expression,
        payload,
    })
}

/// Parser-owned expression inventory. Merging and binding expansion preserve
/// this value wholesale; no provisional evaluator receives its authored roots.
///
/// Configuration validation requires a loader-owned judgement.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{ParsedFile, ProviderContext}, schema::SchemaRegistry,
///     module_resolver::ResolvedModuleSignatures, validation::ValidationTypeContext};
/// let raw = ParsedFile::default();
/// let config = ProviderContext::new(carina_core::EnvironmentSnapshot::new([]));
/// let schemas = SchemaRegistry::new();
/// let signatures = ResolvedModuleSignatures::default();
/// let unchecked = ValidationTypeContext::new_providerless(&raw, &config, &signatures);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Expressions<B: BindingAddress = RelativeBindingName> {
    roots: Vec<ExprValue<ParserPlaceholder, B>>,
    definitions: IndexMap<B, ExprValue<ParserPlaceholder, B>>,
    report: Arc<ExpressionReport<B>>,
    arguments: HashMap<B, ArgumentJudgement>,
    instance: ExpressionInstance,
}

#[derive(Debug, Clone, PartialEq)]
struct ArgumentJudgement {
    declared_type: TypeExpr,
    value: JudgedExpr,
}

impl<B: BindingAddress> Default for Expressions<B> {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            definitions: IndexMap::new(),
            report: Arc::new(ExpressionReport::empty()),
            arguments: HashMap::new(),
            instance: ExpressionInstance::Root,
        }
    }
}

impl<B: BindingAddress> Expressions<B> {
    pub fn binding(&self, name: &B) -> Option<&ExprValue<ParserPlaceholder, B>> {
        self.definitions.get(name)
    }
    pub fn roots(&self) -> &[ExprValue<ParserPlaceholder, B>] {
        &self.roots
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.roots.extend(other.roots);
        self.definitions.extend(other.definitions);
        self.arguments.extend(other.arguments);
        Arc::make_mut(&mut self.report).extend(Arc::unwrap_or_clone(other.report));
    }

    pub(super) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> Expressions<C> {
        let mut memo = HashMap::new();
        Expressions {
            instance: self.instance,
            arguments: self
                .arguments
                .into_iter()
                .map(|(name, expression)| (map(name), expression))
                .collect(),
            report: Arc::new(self.report.map_bindings(map)),
            roots: self
                .roots
                .into_iter()
                .map(|root| root.map_bindings(map, &mut memo))
                .collect(),
            definitions: self
                .definitions
                .into_iter()
                .map(|(name, expression)| (map(name), expression.map_bindings(map, &mut memo)))
                .collect(),
        }
    }

    pub(super) fn attach_binding_projections(
        &mut self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) {
        for (binding, expression) in &mut self.definitions {
            if let Some(value) = values.get(binding.as_str()) {
                expression.payload = Projection::Value(value.clone());
            }
        }
    }

    pub(crate) fn bind_arguments(
        &mut self,
        scope: &crate::resource::ModuleInstanceScope,
        arguments: impl IntoIterator<Item = (B, TypeExpr, JudgedExpr)>,
    ) {
        self.instance = ExpressionInstance::for_scope(scope);
        fn qualify<P: ValueKind, B: BindingAddress>(
            expression: &mut ExprValue<P, B>,
            instance: &ExpressionInstance,
            memo: &mut HashMap<usize, Arc<AuthoredExpr<P, B>>>,
        ) {
            let id = Arc::as_ptr(&expression.authored) as usize;
            if let Some(authored) = memo.get(&id) {
                expression.authored = authored.clone();
                return;
            }
            let mut authored = (*expression.authored).clone();
            authored.location.instance = instance.clone();
            for child in &mut authored.children {
                qualify(child, instance, memo);
            }
            let authored = Arc::new(authored);
            memo.insert(id, authored.clone());
            expression.authored = authored;
        }
        let mut memo = HashMap::new();
        for root in &mut self.roots {
            qualify(root, &self.instance, &mut memo);
        }
        for root in self.definitions.values_mut() {
            qualify(root, &self.instance, &mut memo);
        }
        self.arguments
            .extend(arguments.into_iter().map(|(name, declared_type, value)| {
                (
                    name,
                    ArgumentJudgement {
                        declared_type,
                        value,
                    },
                )
            }));
        self.report = Arc::new(ExpressionReport::empty());
    }
}

/// A schema-judged configuration owns its file, report, and schema registry.
/// Parsing alone cannot construct this type. Consumers borrow this bundle so
/// neither the file nor the schemas can be replaced independently.
#[derive(Debug, Clone)]
pub struct JudgedFile<E, D, B: BindingAddress = RelativeBindingName> {
    file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: crate::schema::SchemaRegistry,
}

impl<E, D, B: BindingAddress> JudgedFile<E, D, B> {
    pub fn file(&self) -> &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        &self.file
    }
    pub fn report(&self) -> &ExpressionReport<B> {
        self.file.expression_report()
    }
    pub fn schemas(&self) -> &crate::schema::SchemaRegistry {
        &self.schemas
    }
    pub(crate) fn into_file(self) -> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
    }
}

impl<E: super::ExportParamLike<ParserPlaceholder, B>, D, B: BindingAddress> JudgedFile<E, D, B>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn new(
        mut file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &crate::schema::SchemaRegistry,
        config: &super::ProviderContext,
    ) -> Self {
        let schemas = schemas.clone();
        judge_configuration(&mut file, &schemas, config);
        Self { file, schemas }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("configuration has not crossed the expression judgement boundary")]
pub struct UnjudgedConfiguration;

impl<E, D, B: BindingAddress> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
    pub(crate) fn expression_report(&self) -> &ExpressionReport<B> {
        &self.parser_binding_metadata.syntax().expressions().report
    }
}

pub(crate) struct ParserExpressionReport<B: BindingAddress>(ExpressionReport<B>);

pub(crate) fn judge_parser_configuration(
    file: &mut super::ParsedFile,
    config: &super::ProviderContext,
) -> ParserExpressionReport<RelativeBindingName> {
    let report = ParserExpressionReport(judge_expressions(
        file,
        &crate::schema::SchemaRegistry::new(),
        config,
    ));
    for parameter in &mut file.export_params {
        super::ExportParamLike::attach_judgement(parameter, &report.0);
    }
    for parameter in &mut file.attribute_params {
        parameter.attach_judgement(&report.0);
    }
    file.parser_binding_metadata.syntax.expressions_mut().report = Arc::new(report.0.clone());
    report
}
impl<B: BindingAddress> ParserExpressionReport<B> {
    pub(crate) fn first_function_error(&self) -> Option<super::ParseError> {
        self.0.first_function_error()
    }
}

pub(crate) fn judge_configuration<
    E: super::ExportParamLike<ParserPlaceholder, B>,
    D,
    B: BindingAddress,
>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &crate::schema::SchemaRegistry,
    config: &super::ProviderContext,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    if file.expression_report().matches_authority(schemas, config) {
        let mut report = file.expression_report().clone();
        report.record_resources(file);
        file.parser_binding_metadata.syntax.expressions_mut().report = Arc::new(report);
        return;
    }
    let mut report = judge_expressions(file, schemas, config);
    for parameter in &file.export_params {
        if let (Some(sink), Some(location)) =
            (parameter.type_expr_opt(), parameter.value_location())
        {
            report.admit_sink(location, sink);
        }
    }
    for parameter in &file.attribute_params {
        if let (Some(sink), Some(location)) = (&parameter.static_type, parameter.value_location()) {
            report.admit_sink(location, sink);
        }
    }
    for parameter in &mut file.export_params {
        parameter.attach_judgement(&report);
    }
    for parameter in &mut file.attribute_params {
        parameter.attach_judgement(&report);
    }
    file.parser_binding_metadata.syntax.expressions_mut().report = Arc::new(report);
}

pub(crate) fn attach_report<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: ExpressionReport<B>,
) {
    file.parser_binding_metadata.syntax.expressions_mut().report = Arc::new(report);
}

impl Expressions {
    pub(in crate::parser) fn from_pairs(
        pairs: Pairs<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let mut out = Self::default();
        for statement in pairs
            .flat_map(|pair| pair.into_inner())
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let pair = statement.into_inner().next().expect("statement body");
            if pair.as_rule() == Rule::let_binding {
                let mut parts = pair.clone().into_inner();
                let name = parts.next().expect("let binding name").as_str();
                if let Some(rhs) = parts.find(|part| part.as_rule() == Rule::expression) {
                    let expression = compile(rhs, file, source, &HashMap::new());
                    if name != "_" {
                        out.definitions
                            .insert(RelativeBindingName::new(name), expression.clone());
                    }
                    out.roots.push(expression);
                    continue;
                }
            }
            out.roots.push(compile(pair, file, source, &HashMap::new()));
        }
        out
    }
}

fn node(
    pair: &Pair<'_, Rule>,
    kind: ExprKind<RelativeBindingName>,
    children: Vec<ExprValue>,
    file: &PreprocessedFile,
    source: &Path,
) -> ExprValue {
    ExprValue {
        authored: Arc::new(AuthoredExpr {
            location: ExpressionLocation::new(
                ExpressionInstance::Root,
                &DeclarationLocation::expression_node(pair, file, source),
            ),
            kind,
            children,
        }),
        payload: Projection::Pending,
    }
}

fn lower_blocks(children: Vec<ExprValue>) -> Vec<ExprValue> {
    let mut groups: IndexMap<String, Vec<ExprValue>> = IndexMap::new();
    let mut headers = HashMap::new();
    for child in &children {
        if let ExprKind::NestedBlock { name, header } = &child.authored.kind {
            groups
                .entry(name.clone())
                .or_default()
                .push(child.authored.children[0].clone());
            headers
                .entry(name.clone())
                .or_insert_with(|| header.clone());
        }
    }
    let groups: HashMap<_, _> = groups
        .into_iter()
        .map(|(name, records)| {
            let value = ExprValue {
                authored: Arc::new(AuthoredExpr {
                    location: ExpressionLocation::new(ExpressionInstance::Root, &headers[&name]),
                    kind: ExprKind::List,
                    children: records,
                }),
                payload: Projection::Pending,
            };
            (name, value)
        })
        .collect();
    children
        .into_iter()
        .map(|child| {
            if let ExprKind::NestedBlock { name, header } = &child.authored.kind {
                ExprValue {
                    authored: Arc::new(AuthoredExpr {
                        location: ExpressionLocation::new(ExpressionInstance::Root, header),
                        kind: ExprKind::Sink(name.clone()),
                        children: vec![groups[name].clone()],
                    }),
                    payload: Projection::Pending,
                }
            } else {
                child
            }
        })
        .collect()
}

fn compile(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
) -> ExprValue {
    // Keep pass-through grammar levels off the large form compiler's stack.
    // Each still receives its authored location and its own checked evidence.
    if matches!(
        pair.as_rule(),
        Rule::expression
            | Rule::primary
            | Rule::coalesce_expr
            | Rule::pipe_expr
            | Rule::additive_expr
            | Rule::multiplicative_expr
            | Rule::compose_expr
            | Rule::postfix_expr
            | Rule::subscripted_id
            | Rule::validate_expr
            | Rule::validate_primary
            | Rule::validate_or_expr
            | Rule::validate_and_expr
            | Rule::validate_comparison
            | Rule::validate_additive_expr
            | Rule::validate_multiplicative_expr
    ) || (pair.as_rule() == Rule::validate_not_expr
        && !pair.as_str().trim_start().starts_with('!'))
    {
        let mut parts = pair.clone().into_inner();
        if let Some(child) = parts.next()
            && parts.next().is_none()
        {
            let child = compile(child, file, source, scope);
            return node(&pair, ExprKind::Alias, vec![child], file, source);
        }
    }
    compile_form(pair, file, source, scope)
}

fn compile_form(
    pair: Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
    scope: &HashMap<String, ExprValue>,
) -> ExprValue {
    let children = || {
        let mut scope = scope.clone();
        let mut children = Vec::new();
        for child in pair.clone().into_inner().flat_map(|child| {
            if child.as_rule() == Rule::block_content {
                child.into_inner().collect::<Vec<_>>()
            } else {
                vec![child]
            }
        }) {
            if matches!(
                child.as_rule(),
                Rule::local_binding
                    | Rule::fn_local_let
                    | Rule::for_local_binding
                    | Rule::if_local_binding
            ) {
                let mut parts = child.clone().into_inner();
                let name = parts
                    .next()
                    .expect("local binding name")
                    .as_str()
                    .to_string();
                if let Some(rhs) = parts.find(|part| part.as_rule() == Rule::expression) {
                    let expression = compile(rhs, file, source, &scope);
                    scope.insert(name, expression.clone());
                    children.push(expression);
                }
            } else {
                children.push(compile(child, file, source, &scope));
            }
        }
        lower_blocks(children)
    };
    match pair.as_rule() {
        Rule::expression | Rule::primary => {
            let children = children();
            if children.len() == 1 {
                node(&pair, ExprKind::Alias, children, file, source)
            } else {
                node(&pair, ExprKind::Sequence, children, file, source)
            }
        }
        Rule::coalesce_expr => {
            let mut children = children();
            let mut right = children.pop().expect("coalesce operand");
            while let Some(left) = children.pop() {
                right = node(&pair, ExprKind::Coalesce, vec![left, right], file, source);
            }
            right
        }
        Rule::pipe_expr => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(parts.next().expect("pipe operand"), file, source, scope);
            for function in parts {
                let mut call = compile(function, file, source, scope);
                let authored = Arc::make_mut(&mut call.authored);
                authored.children.push(value);
                value = call;
            }
            value
        }
        Rule::additive_expr
        | Rule::multiplicative_expr
        | Rule::validate_additive_expr
        | Rule::validate_multiplicative_expr => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(parts.next().expect("integer operand"), file, source, scope);
            while let Some(operator) = parts.next() {
                use crate::integer_arithmetic::IntegerOp;
                let op = match operator.as_str() {
                    "+" => IntegerOp::Add,
                    "-" => IntegerOp::Subtract,
                    "*" => IntegerOp::Multiply,
                    "/" => IntegerOp::Divide,
                    "%" => IntegerOp::Remainder,
                    _ => unreachable!("integer operator"),
                };
                let location = DeclarationLocation::expression_node(&operator, file, source);
                let right = compile(
                    parts.next().expect("right integer operand"),
                    file,
                    source,
                    scope,
                );
                value = node(
                    &pair,
                    ExprKind::Integer { op, location },
                    vec![value, right],
                    file,
                    source,
                );
            }
            value
        }
        Rule::compose_expr => {
            let parts = pair
                .clone()
                .into_inner()
                .filter(|child| !matches!(child.as_rule(), Rule::add_op | Rule::multiplicative_op))
                .map(|child| compile(child, file, source, scope))
                .collect::<Vec<_>>();
            if parts.len() == 1 {
                parts.into_iter().next().unwrap()
            } else {
                node(&pair, ExprKind::Compose, parts, file, source)
            }
        }
        Rule::postfix_expr | Rule::subscripted_id | Rule::for_iterable => {
            let mut parts = pair.clone().into_inner();
            let mut value = compile(parts.next().expect("access base"), file, source, scope);
            for step in parts {
                let operand = step
                    .clone()
                    .into_inner()
                    .find(|child| child.as_rule() == Rule::expression)
                    .map(|child| compile(child, file, source, scope));
                let kind = access_step(step)
                    .map(ExprKind::Access)
                    .unwrap_or(ExprKind::DynamicAccess);
                let mut operands = vec![value];
                operands.extend(operand);
                value = node(&pair, kind, operands, file, source);
            }
            let kind = if pair.as_rule() == Rule::for_iterable {
                ExprKind::Iterable
            } else {
                ExprKind::Alias
            };
            node(&pair, kind, vec![value], file, source)
        }
        Rule::variable_ref => {
            let mut parts = pair.clone().into_inner().peekable();
            let name = parts.next().expect("variable name");
            if !scope.contains_key(name.as_str())
                && parts
                    .peek()
                    .is_some_and(|step| step.as_rule() == Rule::field_access)
            {
                let attribute = parts
                    .next()
                    .expect("first reference field")
                    .as_str()
                    .trim_start_matches('.')
                    .to_string();
                let mut value = node(
                    &pair,
                    ExprKind::Reference {
                        binding: RelativeBindingName::new(name.as_str()),
                        attribute,
                        segments: Vec::new(),
                    },
                    Vec::new(),
                    file,
                    source,
                );
                for step in parts {
                    let operand = step
                        .clone()
                        .into_inner()
                        .find(|child| child.as_rule() == Rule::expression)
                        .map(|child| compile(child, file, source, scope));
                    let kind = access_step(step)
                        .map(ExprKind::Access)
                        .unwrap_or(ExprKind::DynamicAccess);
                    let mut operands = vec![value];
                    operands.extend(operand);
                    value = node(&pair, kind, operands, file, source);
                }
                return value;
            }
            let mut value = match scope.get(name.as_str()) {
                Some(definition) => node(
                    &name,
                    ExprKind::Alias,
                    vec![definition.clone()],
                    file,
                    source,
                ),
                None => node(
                    &name,
                    ExprKind::Binding(RelativeBindingName::new(name.as_str())),
                    vec![],
                    file,
                    source,
                ),
            };
            for step in parts {
                let operand = step
                    .clone()
                    .into_inner()
                    .find(|child| child.as_rule() == Rule::expression)
                    .map(|child| compile(child, file, source, scope));
                let kind = access_step(step)
                    .map(ExprKind::Access)
                    .unwrap_or(ExprKind::DynamicAccess);
                let mut operands = vec![value];
                operands.extend(operand);
                value = node(&pair, kind, operands, file, source);
            }
            value
        }
        Rule::namespaced_id => {
            let mut parts = pair.as_str().split('.');
            let binding = RelativeBindingName::new(parts.next().expect("reference binding"));
            let attribute = parts.next().expect("reference attribute").to_string();
            let segments: Vec<_> = parts
                .map(|name| PathSegment::Field {
                    name: name.to_string(),
                })
                .collect();
            if let Some(definition) = scope.get(binding.as_str()) {
                let mut value = node(
                    &pair,
                    ExprKind::Alias,
                    vec![definition.clone()],
                    file,
                    source,
                );
                for step in std::iter::once(PathSegment::Field { name: attribute }).chain(segments)
                {
                    value = node(&pair, ExprKind::Access(step), vec![value], file, source);
                }
                return value;
            }
            node(
                &pair,
                ExprKind::Reference {
                    binding,
                    attribute,
                    segments,
                },
                vec![],
                file,
                source,
            )
        }
        Rule::number => node(&pair, ExprKind::Scalar(TypeExpr::Int), vec![], file, source),
        Rule::float => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Float),
            vec![],
            file,
            source,
        ),
        Rule::boolean => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Bool),
            vec![],
            file,
            source,
        ),
        Rule::duration_literal => node(
            &pair,
            ExprKind::Scalar(TypeExpr::Duration),
            vec![],
            file,
            source,
        ),
        Rule::string => {
            let expressions = pair
                .clone()
                .into_inner()
                .flat_map(|quoted| quoted.into_inner())
                .filter(|part| part.as_rule() == Rule::string_part)
                .filter_map(|part| part.into_inner().next())
                .filter(|part| part.as_rule() == Rule::interpolation)
                .map(|part| {
                    let expression = part
                        .clone()
                        .into_inner()
                        .next()
                        .and_then(|wrapper| wrapper.into_inner().next());
                    match expression {
                        Some(expression) => compile(expression, file, source, scope),
                        None => node(&part, ExprKind::EmptyInterpolation, vec![], file, source),
                    }
                })
                .collect::<Vec<_>>();
            node(
                &pair,
                if expressions.is_empty() {
                    ExprKind::Scalar(TypeExpr::String)
                } else {
                    ExprKind::Interpolation
                },
                expressions,
                file,
                source,
            )
        }
        Rule::list => node(&pair, ExprKind::List, children(), file, source),
        Rule::map => {
            let entries = lower_blocks(
                pair.clone()
                    .into_inner()
                    .map(|entry| {
                        if entry.as_rule() == Rule::map_entry {
                            let mut parts = entry.clone().into_inner();
                            let key = parts.next().expect("map key");
                            let name = if key.as_rule() == Rule::string {
                                super::expressions::string_literal::parse_string_literal(
                                    key.clone(),
                                )
                                .unwrap_or_else(|_| key.as_str().to_string())
                            } else {
                                key.as_str().to_string()
                            };
                            let value =
                                compile(parts.next().expect("map value"), file, source, scope);
                            node(&entry, ExprKind::Sink(name), vec![value], file, source)
                        } else {
                            compile(entry, file, source, scope)
                        }
                    })
                    .collect(),
            );
            let keys = entries
                .iter()
                .map(|entry| match &entry.authored.kind {
                    ExprKind::Sink(name) => name.clone(),
                    _ => unreachable!("compiled map entry"),
                })
                .collect();
            node(&pair, ExprKind::Map(keys), entries, file, source)
        }
        Rule::fn_body | Rule::if_body | Rule::else_clause | Rule::for_body => {
            node(&pair, ExprKind::Sequence, children(), file, source)
        }
        Rule::fn_def => {
            let return_type = pair
                .clone()
                .into_inner()
                .find(|part| part.as_rule() == Rule::type_expr)
                .and_then(|annotation| {
                    super::parse_type_expr_str(
                        annotation.as_str(),
                        &super::ProviderContext::new(crate::EnvironmentSnapshot::new([])),
                    )
                });
            let mut scope = scope.clone();
            let mut parameters = Vec::new();
            for parameter in pair
                .clone()
                .into_inner()
                .flatten()
                .filter(|part| part.as_rule() == Rule::fn_param)
            {
                let mut parts = parameter.into_inner();
                let name = parts.next().expect("function parameter name");
                if let Some(annotation) = parts.find(|part| part.as_rule() == Rule::type_expr)
                    && let Some(ty) = super::parse_type_expr_str(
                        annotation.as_str(),
                        &super::ProviderContext::new(crate::EnvironmentSnapshot::new([])),
                    )
                {
                    parameters.push((name.as_str().to_string(), Some(ty.clone())));
                    scope.insert(
                        name.as_str().to_string(),
                        node(&name, ExprKind::Parameter(ty), vec![], file, source),
                    );
                } else {
                    parameters.push((name.as_str().to_string(), None));
                    scope.insert(
                        name.as_str().to_string(),
                        node(&name, ExprKind::UntypedParameter, vec![], file, source),
                    );
                }
            }
            let children = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::fn_body)
                .map(|body| compile(body, file, source, &scope))
                .collect();
            node(
                &pair,
                ExprKind::Function {
                    name: pair
                        .clone()
                        .into_inner()
                        .next()
                        .unwrap()
                        .as_str()
                        .to_string(),
                    signature: Arc::new(FunctionSignature {
                        parameters,
                        return_type,
                    }),
                },
                children,
                file,
                source,
            )
        }
        Rule::for_expr => {
            let mut parts = pair.clone().into_inner();
            let binding = parts.next().expect("loop binding");
            let iterable = compile(parts.next().expect("loop iterable"), file, source, scope);
            let mut scope = scope.clone();
            let pattern = binding.into_inner().next().expect("loop pattern");
            let names = pattern.clone().into_inner().collect::<Vec<_>>();
            for (index, name) in names.iter().enumerate() {
                if name.as_str() == "_" {
                    continue;
                }
                let kind = match (pattern.as_rule(), index) {
                    (Rule::for_indexed_binding, 0) => ExprKind::Parameter(TypeExpr::Int),
                    (Rule::for_map_binding, 0) => ExprKind::Parameter(TypeExpr::String),
                    _ => ExprKind::Element,
                };
                scope.insert(
                    name.as_str().to_string(),
                    node(name, kind, vec![iterable.clone()], file, source),
                );
            }
            let body = compile(parts.next().expect("loop body"), file, source, &scope);
            node(&pair, ExprKind::For, vec![iterable, body], file, source)
        }
        Rule::function_call | Rule::validate_function_call => {
            let mut parts = pair.clone().into_inner();
            let name = parts.next().expect("function name").as_str().to_string();
            node(
                &pair,
                ExprKind::Call(name),
                parts
                    .map(|child| compile(child, file, source, scope))
                    .collect(),
                file,
                source,
            )
        }
        Rule::resource_expr | Rule::anonymous_resource | Rule::read_resource_expr => {
            let full_type = pair
                .clone()
                .into_inner()
                .next()
                .expect("resource type")
                .as_str();
            let (provider, resource_type) =
                full_type.split_once('.').expect("qualified resource type");
            node(
                &pair,
                ExprKind::Resource {
                    provider: provider.to_string(),
                    resource_type: resource_type.to_string(),
                    read: pair.as_rule() == Rule::read_resource_expr,
                },
                // The qualified resource type is a declaration, not a value
                // reference whose provider head can be shadowed by a binding.
                children().into_iter().skip(1).collect(),
                file,
                source,
            )
        }
        Rule::attribute => {
            let mut parts = pair.clone().into_inner();
            let name = parts.next().expect("attribute name");
            let name = if name.as_rule() == Rule::string {
                super::expressions::string_literal::parse_string_literal(name).unwrap_or_default()
            } else {
                name.as_str().to_string()
            };
            let value = compile(parts.next().expect("attribute value"), file, source, scope);
            node(&pair, ExprKind::Sink(name), vec![value], file, source)
        }
        Rule::if_expr => node(&pair, ExprKind::If, children(), file, source),
        Rule::wait_expr => {
            let mut parts = pair.clone().into_inner();
            let target = parts.next().expect("wait target");
            let target = scope.get(target.as_str()).cloned().unwrap_or_else(|| {
                node(
                    &target,
                    ExprKind::Binding(RelativeBindingName::new(target.as_str())),
                    vec![],
                    file,
                    source,
                )
            });
            let mut operands = vec![target];
            operands.extend(parts.map(|part| compile(part, file, source, scope)));
            node(&pair, ExprKind::Wait, operands, file, source)
        }
        Rule::validate_expr | Rule::validate_primary => {
            node(&pair, ExprKind::Alias, children(), file, source)
        }
        Rule::validate_or_expr
        | Rule::validate_and_expr
        | Rule::validate_not_expr
        | Rule::validate_comparison => {
            let values = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() != Rule::compare_op)
                .map(|part| compile(part, file, source, scope))
                .collect::<Vec<_>>();
            let kind = if values.len() == 1 && !pair.as_str().trim_start().starts_with('!') {
                ExprKind::Alias
            } else {
                ExprKind::Boolean
            };
            node(&pair, kind, values, file, source)
        }
        Rule::let_binding
        | Rule::local_binding
        | Rule::for_local_binding
        | Rule::if_local_binding
        | Rule::fn_local_let
        | Rule::attributes_param
        | Rule::exports_param
        | Rule::module_call_arg
        | Rule::arg_default_attr => {
            let values: Vec<_> = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::expression)
                .map(|part| compile(part, file, source, scope))
                .collect();
            let kind = if values.is_empty() {
                ExprKind::Declaration
            } else {
                ExprKind::Sequence
            };
            node(&pair, kind, values, file, source)
        }
        Rule::module_call => node(&pair, ExprKind::ModuleCall, children(), file, source),
        Rule::nested_block => {
            let name_pair = pair.clone().into_inner().next().expect("nested block name");
            let name = name_pair.as_str().to_string();
            let header = DeclarationLocation::expression_node(&name_pair, file, source);
            let members = children().into_iter().skip(1).collect::<Vec<_>>();
            let (fields, mut locals): (Vec<_>, Vec<_>) = members
                .into_iter()
                .partition(|member| matches!(member.authored.kind, ExprKind::Sink(_)));
            let keys = fields
                .iter()
                .map(|field| match &field.authored.kind {
                    ExprKind::Sink(name) => name.clone(),
                    _ => unreachable!(),
                })
                .collect();
            let record = node(&pair, ExprKind::Map(keys), fields, file, source);
            locals.push(record);
            let record = node(&pair, ExprKind::Sequence, locals, file, source);
            node(
                &pair,
                ExprKind::NestedBlock { name, header },
                vec![record],
                file,
                source,
            )
        }
        Rule::provider_block | Rule::provider_expr => node(
            &pair,
            ExprKind::ProviderDeclaration,
            children(),
            file,
            source,
        ),
        Rule::arguments_block
        | Rule::arguments_param
        | Rule::arguments_param_block
        | Rule::arguments_param_attr
        | Rule::arg_description_attr
        | Rule::arg_validation_block
        | Rule::validation_block_attr
        | Rule::validation_condition_attr
        | Rule::validation_error_message_attr
        | Rule::attributes_block
        | Rule::exports_block
        | Rule::backend_block
        | Rule::require_statement
        | Rule::wait_attr
        | Rule::wait_until_attr
        | Rule::wait_timeout_attr
        | Rule::wait_depends_on_attr
        | Rule::upstream_state_expr
        | Rule::use_expr
        | Rule::import_state_block
        | Rule::import_state_attr
        | Rule::import_to_attr
        | Rule::import_id_attr
        | Rule::removed_block
        | Rule::removed_attr
        | Rule::moved_block
        | Rule::moved_attr
        | Rule::moved_from_attr
        | Rule::moved_to_attr
        | Rule::resource_address
        | Rule::type_expr
        | Rule::type_expr_atom
        | Rule::type_simple
        | Rule::type_generic
        | Rule::type_ref
        | Rule::resource_type_path
        | Rule::type_struct
        | Rule::struct_field_list
        | Rule::struct_field
        | Rule::type_string_literal
        | Rule::identifier
        | Rule::discard_pattern
        | Rule::reference_prefix
        | Rule::reference_field
        | Rule::null_literal
        | Rule::EOI => node(&pair, ExprKind::Declaration, children(), file, source),
        rule => node(
            &pair,
            ExprKind::Unsupported(format!("{rule:?}")),
            children(),
            file,
            source,
        ),
    }
}

fn access_step(pair: Pair<'_, Rule>) -> Option<PathSegment> {
    match pair.as_rule() {
        Rule::field_access => Some(PathSegment::Field {
            name: pair.as_str().trim_start_matches('.').to_string(),
        }),
        Rule::index_access => {
            fn literal(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
                if matches!(pair.as_rule(), Rule::number | Rule::string) {
                    return Some(pair);
                }
                let mut children = pair.into_inner();
                let child = children.next()?;
                if children.next().is_some() {
                    return None;
                }
                literal(child)
            }
            let index = literal(pair)?;
            let subscript = match index.as_rule() {
                Rule::number => crate::resource::Subscript::Int {
                    index: index.as_str().parse().ok()?,
                },
                Rule::string => crate::resource::Subscript::Str {
                    key: super::expressions::string_literal::parse_string_literal(index)
                        .ok()?
                        .into(),
                },
                _ => return None,
            };
            Some(PathSegment::Subscript { index: subscript })
        }
        _ => None,
    }
}

pub(crate) use judgement::judge_expressions;
pub(in crate::parser) use judgement::unjudged as missing_judgement;
pub(crate) use judgement::{
    AdmissionContext, EmptyCoalesceDefault, admit_with_context, coalesce_type,
};
pub use judgement::{
    CollectionKind, ExpressionAdmission, ExpressionCheckOwner, ExpressionReport, JudgedExpr,
    ProviderAuthorityUnavailable, admit_expression,
};
#[cfg(any(test, feature = "test-utils", feature = "test-support"))]
pub(crate) fn missing_judgement_for_test() -> JudgedExpr {
    judgement::constraint_fixture()
}

// Constructors for judgement evidence are confined to this module. In
// particular, the syntax compiler and provisional evaluators cannot mint it.
mod judgement {
    use super::*;
    use crate::static_type::{InferenceError, InferenceIssue, NotInferableReason};
    use crate::validation::inference::{self, InferenceBinding, InferenceBindings};

    #[derive(Clone, Copy)]
    pub(crate) enum EmptyCoalesceDefault {
        None,
        List,
        Map,
    }

    pub(crate) fn coalesce_type(
        left: Result<TypeExpr, InferenceIssue>,
        default: Result<TypeExpr, InferenceIssue>,
        empty: EmptyCoalesceDefault,
    ) -> Result<TypeExpr, InferenceIssue> {
        match (left, default) {
            (Ok(left), Ok(right)) if left == right => Ok(left),
            (Ok(left), Ok(right)) => Err(InferenceError::IncompatibleCoalesceTypes {
                types: crate::schema::MismatchTypeNames::for_type_exprs(&left, &right),
            }
            .into()),
            (Ok(left @ TypeExpr::List(_)), Err(_))
                if matches!(empty, EmptyCoalesceDefault::List) =>
            {
                Ok(left)
            }
            (Ok(left @ TypeExpr::Map(_)), Err(_)) if matches!(empty, EmptyCoalesceDefault::Map) => {
                Ok(left)
            }
            (
                _,
                Err(
                    error @ InferenceIssue::Failed(InferenceError::IncompatibleCoalesceTypes {
                        ..
                    }),
                ),
            ) => Err(error),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum InferredKind {
        Ordinary,
        // Schema-projected references retain the existing binding check and
        // apply-time value validation, rather than acquiring a new nominal
        // declaration obligation at a function boundary.
        Reference,
        Literal,
        EnumIdentifier,
        ValueDependent,
        Declaration,
        StringConstruction,
        List,
        Map,
    }

    /// The intrinsic value shape does not depend on nominal identity or on
    /// inferring a common element type. In particular an empty list still has
    /// a list shape, and a heterogeneous record is still a map.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ValueShape {
        String,
        Int,
        Float,
        Bool,
        Duration,
        List,
        Map,
        Handle,
    }

    impl ValueShape {
        fn of_type(ty: &TypeExpr) -> Option<Self> {
            use crate::schema::{RawShape, RefinedScalarKind};
            Some(match ty {
                TypeExpr::String | TypeExpr::StringLiteral(_) => Self::String,
                TypeExpr::Int => Self::Int,
                TypeExpr::Float => Self::Float,
                TypeExpr::Bool => Self::Bool,
                TypeExpr::Duration => Self::Duration,
                TypeExpr::List(_) => Self::List,
                TypeExpr::Map(_) | TypeExpr::Struct { .. } => Self::Map,
                TypeExpr::Ref(_) => Self::Handle,
                TypeExpr::SchemaType { owned } => match owned.scalar_kind() {
                    RefinedScalarKind::String => Self::String,
                    RefinedScalarKind::Int => Self::Int,
                    RefinedScalarKind::Float => Self::Float,
                },
                TypeExpr::Simple(name) => match name.declaration().raw_shape() {
                    RawShape::String { .. } | RawShape::Reference { .. } => Self::String,
                    RawShape::Int { .. } => Self::Int,
                    RawShape::Float { .. } => Self::Float,
                    RawShape::Bool => Self::Bool,
                    RawShape::Duration => Self::Duration,
                    RawShape::List { .. } => Self::List,
                    RawShape::Map { .. } | RawShape::Struct { .. } => Self::Map,
                    _ => return None,
                },
                TypeExpr::Union(members) => {
                    let shape = Self::of_type(members.first()?)?;
                    if !members
                        .iter()
                        .all(|member| Self::of_type(member) == Some(shape))
                    {
                        return None;
                    }
                    shape
                }
                TypeExpr::DottedUnresolved(_) | TypeExpr::UnloadedSchemaType(_) => return None,
            })
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailureOwner {
        Expression,
        Boundary,
        BindingIndex,
        BindingScope,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct ExprJudgement {
        ty: Result<TypeExpr, InferenceIssue>,
        kind: InferredKind,
        // A missing inferable type (dynamic calls, heterogeneous records) is
        // distinct from an invalid authored operation such as a bad fallback.
        invalid: bool,
        // The producer assigns diagnostic ownership when creating a failure.
        // Wrappers preserve it, so reporting never depends on an ExprKind list.
        failure_owner: FailureOwner,
    }

    /// Admission evidence, constructible only by the recursive judge.
    ///
    /// ```compile_fail,E0616
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn overwrite_projection(expression: &mut JudgedExpr) { expression.concrete = true; }
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExpr(Arc<JudgedExprData>);

    #[doc(hidden)]
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExprData {
        origin: Option<ExpressionLocation>,
        concrete: bool,
        reported: bool,
        direct_reference: bool,
        boundary_owner: Option<Arc<DeclarationLocation>>,
        judgement: Arc<ExprJudgement>,
        coalesce: Option<Arc<[JudgedExpr; 2]>>,
        alternatives: Arc<Vec<JudgedExpr>>,
        member: Option<Arc<JudgedExpr>>,
        elements: Arc<Vec<JudgedExpr>>,
        fields: Arc<IndexMap<String, JudgedExpr>>,
    }

    impl std::ops::Deref for JudgedExpr {
        type Target = JudgedExprData;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl JudgedExpr {
        fn data_mut(&mut self) -> &mut JudgedExprData {
            Arc::make_mut(&mut self.0)
        }
        fn failure_owned_by(&mut self, owner: FailureOwner) {
            Arc::make_mut(&mut self.data_mut().judgement).failure_owner = owner;
        }
        pub fn static_type(&self) -> Result<&TypeExpr, &InferenceIssue> {
            self.judgement.ty.as_ref()
        }
        pub(crate) fn alternatives(&self) -> &[JudgedExpr] {
            &self.alternatives
        }
        pub(crate) fn elements(&self) -> impl Iterator<Item = &JudgedExpr> {
            self.elements
                .iter()
                .chain(self.member.iter().map(Arc::as_ref))
        }
        pub(crate) fn fields(&self) -> impl Iterator<Item = (&str, &JudgedExpr)> {
            self.fields
                .iter()
                .map(|(name, field)| (name.as_str(), field))
        }
        pub fn coalesce_operands(&self) -> Option<&[JudgedExpr; 2]> {
            self.coalesce.as_deref()
        }
        pub fn element(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.elements
                .get(index)
                .or(self.member.as_deref())
                .ok_or_else(|| MissingEvidence::new(format!("list element {index}")))
        }
        pub fn field(&self, key: &str) -> Result<&JudgedExpr, MissingEvidence> {
            self.fields
                .get(key)
                .or(self.member.as_deref())
                .ok_or_else(|| MissingEvidence::new(format!("field {key}")))
        }
        pub fn branch(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.alternatives
                .get(index)
                .ok_or_else(|| MissingEvidence::new(format!("branch {index}")))
        }
        pub(crate) fn requires_declared_assignment(&self) -> bool {
            matches!(
                self.judgement.kind,
                InferredKind::Ordinary | InferredKind::Reference
            ) && self.alternatives.is_empty()
        }
        pub(crate) fn is_string_construction(&self) -> bool {
            self.judgement.kind == InferredKind::StringConstruction && self.alternatives.is_empty()
        }
        pub(crate) fn is_value_dependent(&self) -> bool {
            self.judgement.kind == InferredKind::ValueDependent
        }
        pub fn check_owner(&self) -> ExpressionCheckOwner {
            if self.reported {
                ExpressionCheckOwner::Reported
            } else if self.judgement.invalid {
                match self.judgement.failure_owner {
                    FailureOwner::Expression | FailureOwner::Boundary => {
                        ExpressionCheckOwner::Expression
                    }
                    FailureOwner::BindingIndex => ExpressionCheckOwner::BindingIndex,
                    FailureOwner::BindingScope => ExpressionCheckOwner::BindingScope,
                }
            } else if self.direct_reference && self.alternatives.is_empty() {
                ExpressionCheckOwner::BindingIndex
            } else {
                ExpressionCheckOwner::Expression
            }
        }
        pub fn boundary_owner(&self) -> Option<&DeclarationLocation> {
            self.boundary_owner.as_deref()
        }
        fn owned_by_bindings(mut self) -> Self {
            self.data_mut().direct_reference = true;
            self.failure_owned_by(FailureOwner::BindingIndex);
            self
        }
        fn needs_authored_sink_check(&self) -> bool {
            (self.judgement.kind == InferredKind::StringConstruction && !self.concrete)
                || !self.alternatives.is_empty()
                || self.elements.iter().any(Self::needs_authored_sink_check)
                || self.fields.values().any(Self::needs_authored_sink_check)
                || self
                    .member
                    .as_deref()
                    .is_some_and(Self::needs_authored_sink_check)
        }
        fn contains_deferred_interpolation(&self) -> bool {
            (self.judgement.kind == InferredKind::StringConstruction && !self.concrete)
                || self
                    .alternatives
                    .iter()
                    .any(Self::contains_deferred_interpolation)
                || self
                    .elements
                    .iter()
                    .any(Self::contains_deferred_interpolation)
                || self
                    .fields
                    .values()
                    .any(Self::contains_deferred_interpolation)
                || self
                    .member
                    .as_deref()
                    .is_some_and(Self::contains_deferred_interpolation)
        }
        pub fn coalesce_operand(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.coalesce
                .as_ref()
                .and_then(|operands| operands.get(index))
                .ok_or_else(|| MissingEvidence::new(format!("coalesce operand {index}")))
        }
        pub fn collection_kind(&self) -> Option<CollectionKind> {
            match self.judgement.kind {
                InferredKind::List => Some(CollectionKind::List),
                InferredKind::Map => Some(CollectionKind::Map),
                _ => None,
            }
        }
        fn value_shape(&self) -> Option<ValueShape> {
            if let Some(first) = self.alternatives.first() {
                let shape = first.value_shape()?;
                return self
                    .alternatives
                    .iter()
                    .all(|branch| branch.value_shape() == Some(shape))
                    .then_some(shape);
            }
            match self.judgement.kind {
                InferredKind::List => Some(ValueShape::List),
                InferredKind::Map => Some(ValueShape::Map),
                _ => self.static_type().ok().and_then(ValueShape::of_type),
            }
        }
        fn type_description(&self) -> String {
            self.static_type()
                .map(ToString::to_string)
                .unwrap_or_else(|_| {
                    self.value_shape()
                        .map(|shape| format!("{shape:?}"))
                        .unwrap_or_else(|| "value-dependent expression".into())
                })
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CollectionKind {
        List,
        Map,
    }

    /// The validator responsible for a diagnostic. This does not change
    /// admission: failed and missing evidence remain errors at every sink.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExpressionCheckOwner {
        Expression,
        BindingIndex,
        BindingScope,
        Reported,
    }

    /// Opaque proof that a provider schema is unavailable. Missing authored
    /// evidence cannot construct this token.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ProviderAuthorityUnavailable(UnavailableSchema);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum UnavailableSchema {
        Resource {
            binding: String,
        },
        Named {
            identity: crate::schema::TypeIdentity,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ExpressionAdmission {
        ValueCheck,
        DeferredInterpolation,
        ProviderPending(ProviderAuthorityUnavailable),
    }

    fn combine_admission(
        left: ExpressionAdmission,
        right: ExpressionAdmission,
    ) -> ExpressionAdmission {
        match (left, right) {
            (pending @ ExpressionAdmission::ProviderPending(_), _)
            | (_, pending @ ExpressionAdmission::ProviderPending(_)) => pending,
            (ExpressionAdmission::DeferredInterpolation, _)
            | (_, ExpressionAdmission::DeferredInterpolation) => {
                ExpressionAdmission::DeferredInterpolation
            }
            _ => ExpressionAdmission::ValueCheck,
        }
    }

    /// Static admission requires the judge's evidence, never a projection.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::{parser::{TypeExpr, retained::admit_expression}, resource::{Value, ConcreteValue}};
    /// let reduced = Value::<carina_core::resource::ParserPlaceholder, carina_core::resource::RelativeBindingName>::Concrete(ConcreteValue::String("selected".into()));
    /// admit_expression(&reduced, &TypeExpr::String);
    /// ```
    pub fn admit_expression(
        expression: &JudgedExpr,
        sink: &TypeExpr,
    ) -> Result<ExpressionAdmission, InferenceError> {
        admit_with_context(expression, sink, AdmissionContext::Assignment)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AdmissionContext {
        Assignment,
        Function,
    }

    impl JudgedExpr {
        pub(crate) fn retains_function_value_check(&self, sink: &TypeExpr) -> bool {
            self.judgement.kind == InferredKind::Reference
                && self.static_type() == Ok(&TypeExpr::String)
                && sink.is_string_shaped()
        }
    }

    pub(crate) fn admit_with_context(
        expression: &JudgedExpr,
        sink: &TypeExpr,
        context: AdmissionContext,
    ) -> Result<ExpressionAdmission, InferenceError> {
        if expression.judgement.invalid
            && let Err(InferenceIssue::Failed(error)) = expression.static_type()
        {
            return Err(error.clone());
        }
        if expression.judgement.kind == InferredKind::Declaration {
            return Err(InferenceError::UnknownType {
                reason: "declaration is not a value".into(),
            });
        }
        if !expression.alternatives.is_empty() {
            let mut admission = ExpressionAdmission::ValueCheck;
            for branch in expression.alternatives.iter() {
                admission =
                    combine_admission(admission, admit_with_context(branch, sink, context)?);
            }
            return Ok(admission);
        }
        if context == AdmissionContext::Function && expression.retains_function_value_check(sink) {
            return Ok(ExpressionAdmission::ValueCheck);
        }
        // Literal-only unions have the language's ordinary String contract.
        // Named string identities retain their directional assignment rules.
        if matches!(sink, TypeExpr::Union(members) if !members.is_empty() && members.iter().all(|member| matches!(member, TypeExpr::StringLiteral(_))))
            && expression.requires_declared_assignment()
            && expression.static_type() == Ok(&TypeExpr::String)
        {
            return Ok(ExpressionAdmission::ValueCheck);
        }
        if let TypeExpr::Union(members) = sink {
            return members
                .iter()
                .find_map(|sink| admit_with_context(expression, sink, context).ok())
                .ok_or_else(|| assignment_error(expression, sink));
        }
        // A declared String has no nominal identity, independently of whether
        // the provider can currently validate the target's scalar constraints.
        if expression.requires_declared_assignment()
            && expression.static_type() == Ok(&TypeExpr::String)
            && !matches!(sink, TypeExpr::String)
        {
            return Err(assignment_error(expression, sink));
        }
        if let TypeExpr::UnloadedSchemaType(unloaded) = sink {
            if expression.collection_kind().is_some()
                || matches!(
                    expression.static_type(),
                    Ok(TypeExpr::Bool
                        | TypeExpr::Duration
                        | TypeExpr::List(_)
                        | TypeExpr::Map(_)
                        | TypeExpr::Struct { .. }
                        | TypeExpr::Ref(_))
                )
            {
                return Err(assignment_error(expression, sink));
            }
            return Ok(ExpressionAdmission::ProviderPending(
                ProviderAuthorityUnavailable(UnavailableSchema::Named {
                    identity: unloaded.identity().clone(),
                }),
            ));
        }
        match (&expression.judgement.kind, sink) {
            (InferredKind::List, TypeExpr::List(inner)) => {
                let mut admission = ExpressionAdmission::ValueCheck;
                for element in expression
                    .elements
                    .iter()
                    .chain(expression.member.iter().map(Arc::as_ref))
                {
                    admission =
                        combine_admission(admission, admit_with_context(element, inner, context)?);
                }
                return Ok(admission);
            }
            (InferredKind::Map, TypeExpr::Map(inner)) => {
                let mut admission = ExpressionAdmission::ValueCheck;
                for field in expression
                    .fields
                    .values()
                    .chain(expression.member.iter().map(Arc::as_ref))
                {
                    admission =
                        combine_admission(admission, admit_with_context(field, inner, context)?);
                }
                return Ok(admission);
            }
            (InferredKind::Map, TypeExpr::Struct { fields }) => {
                let _ = fields;
                return Ok(ExpressionAdmission::ValueCheck);
            }
            (InferredKind::List | InferredKind::Map, _) => {
                return Err(assignment_error(expression, sink));
            }
            _ => {}
        }
        if expression.judgement.kind == InferredKind::StringConstruction && !expression.concrete {
            return if sink.is_string_shaped() || matches!(sink, TypeExpr::Ref(_)) {
                Ok(ExpressionAdmission::DeferredInterpolation)
            } else {
                Err(InferenceError::UnknownType {
                    reason: format!(
                        "cannot assign deferred interpolation: expected {sink}, got String"
                    ),
                })
            };
        }
        if expression.is_value_dependent() {
            return Ok(ExpressionAdmission::ValueCheck);
        }
        match expression.static_type() {
            Err(InferenceIssue::NotInferable(NotInferableReason::SchemaUnavailable {
                binding,
            })) => Ok(ExpressionAdmission::ProviderPending(
                ProviderAuthorityUnavailable(UnavailableSchema::Resource {
                    binding: binding.clone(),
                }),
            )),
            Err(InferenceIssue::NotInferable(
                NotInferableReason::CustomTypeSchemaUnavailable { identity },
            )) => Ok(ExpressionAdmission::ProviderPending(
                ProviderAuthorityUnavailable(UnavailableSchema::Named {
                    identity: identity.clone(),
                }),
            )),
            Err(InferenceIssue::Failed(error))
                if expression.judgement.kind != InferredKind::ValueDependent
                    && !matches!(
                        expression.judgement.kind,
                        InferredKind::List | InferredKind::Map
                    ) =>
            {
                Err(error.clone())
            }
            Err(InferenceIssue::NotInferable(_)) => {
                Err(MissingEvidence::new("incomplete authored judgement").into())
            }
            Err(_) => Ok(ExpressionAdmission::ValueCheck),
            Ok(actual) => {
                let shaped = match actual {
                    TypeExpr::String => {
                        sink.is_string_shaped()
                            || matches!(
                                sink,
                                TypeExpr::StringLiteral(_)
                                    | TypeExpr::Ref(_)
                                    | TypeExpr::UnloadedSchemaType(_)
                            )
                    }
                    TypeExpr::Int => matches!(
                        sink,
                        TypeExpr::Int
                            | TypeExpr::Float
                            | TypeExpr::Simple(_)
                            | TypeExpr::SchemaType { .. }
                            | TypeExpr::DottedUnresolved(_)
                            | TypeExpr::UnloadedSchemaType(_)
                    ),
                    TypeExpr::Bool => matches!(sink, TypeExpr::Bool),
                    TypeExpr::Float => matches!(
                        sink,
                        TypeExpr::Float
                            | TypeExpr::Simple(_)
                            | TypeExpr::SchemaType { .. }
                            | TypeExpr::DottedUnresolved(_)
                            | TypeExpr::UnloadedSchemaType(_)
                    ),
                    TypeExpr::Duration => matches!(sink, TypeExpr::Duration),
                    TypeExpr::Ref(_) => {
                        matches!(sink, TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_))
                    }
                    _ => true,
                };
                if !shaped {
                    return Err(assignment_error(expression, sink));
                }
                Ok(ExpressionAdmission::ValueCheck)
            }
        }
    }

    fn assignment_error(expression: &JudgedExpr, sink: &TypeExpr) -> InferenceError {
        InferenceError::UnknownType {
            reason: format!(
                "expected {sink}, got {}",
                expression
                    .static_type()
                    .map(ToString::to_string)
                    .unwrap_or_else(|_| "collection".into())
            ),
        }
    }

    #[derive(Debug, Clone)]
    struct JudgementAuthority {
        schemas: crate::schema::SchemaRegistry,
        catalog: super::super::ProviderCustomTypes,
        resource_types: std::collections::HashSet<(String, String)>,
    }
    impl PartialEq for JudgementAuthority {
        fn eq(&self, other: &Self) -> bool {
            self.schemas.same_snapshot(&other.schemas)
                && self.catalog.same_snapshot(&other.catalog)
                && self.resource_types == other.resource_types
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct ExpressionReport<B: BindingAddress = RelativeBindingName> {
        bindings: Arc<HashMap<B, JudgedExpr>>,
        expressions: Arc<HashMap<ExpressionLocation, JudgedExpr>>,
        findings: Arc<Vec<Located<InferenceError>>>,
        function_errors: Arc<Vec<Located<InferenceError>>>,
        operands: Arc<HashMap<ExpressionLocation, [JudgedExpr; 2]>>,
        module_attributes: Arc<HashMap<(B, String), JudgedExpr>>,
        resource_reports: Arc<HashMap<crate::resource::ResourceId, Arc<ExpressionReport<B>>>>,
        parameters: Arc<HashMap<String, JudgedExpr>>,
        instance: ExpressionInstance,
        authored: bool,
        authority: Option<Arc<JudgementAuthority>>,
    }

    impl<B: BindingAddress> ExpressionReport<B> {
        pub(super) fn empty() -> Self {
            Self {
                bindings: Arc::default(),
                expressions: Arc::default(),
                findings: Arc::default(),
                function_errors: Arc::default(),
                operands: Arc::default(),
                module_attributes: Arc::default(),
                resource_reports: Arc::default(),
                parameters: Arc::default(),
                instance: ExpressionInstance::Root,
                authored: false,
                authority: None,
            }
        }
        pub(super) fn extend(&mut self, other: Self) {
            // A merged lexical scope has new binding dependencies even when
            // both files used the same provider and schema snapshots.
            self.authority = None;
            Arc::make_mut(&mut self.bindings).extend(Arc::unwrap_or_clone(other.bindings));
            Arc::make_mut(&mut self.expressions).extend(Arc::unwrap_or_clone(other.expressions));
            Arc::make_mut(&mut self.findings).extend(Arc::unwrap_or_clone(other.findings));
            Arc::make_mut(&mut self.function_errors)
                .extend(Arc::unwrap_or_clone(other.function_errors));
            Arc::make_mut(&mut self.operands).extend(Arc::unwrap_or_clone(other.operands));
            Arc::make_mut(&mut self.module_attributes)
                .extend(Arc::unwrap_or_clone(other.module_attributes));
            Arc::make_mut(&mut self.resource_reports)
                .extend(Arc::unwrap_or_clone(other.resource_reports));
        }
        pub(super) fn map_bindings<C: BindingAddress>(
            &self,
            map: &mut impl FnMut(B) -> C,
        ) -> ExpressionReport<C> {
            let mut mapped = HashMap::new();
            let resource_reports = self
                .resource_reports
                .iter()
                .map(|(id, report)| {
                    let key = Arc::as_ptr(report) as usize;
                    let report = mapped
                        .entry(key)
                        .or_insert_with(|| Arc::new(report.map_bindings(map)))
                        .clone();
                    (id.clone(), report)
                })
                .collect::<HashMap<_, _>>()
                .into();
            ExpressionReport {
                instance: self.instance.clone(),
                authored: self.authored,
                authority: self.authority.clone(),
                bindings: self
                    .bindings
                    .iter()
                    .map(|(name, expression)| (map(name.clone()), expression.clone()))
                    .collect::<HashMap<_, _>>()
                    .into(),
                expressions: self.expressions.clone(),
                findings: self.findings.clone(),
                function_errors: self.function_errors.clone(),
                operands: self.operands.clone(),
                module_attributes: self
                    .module_attributes
                    .iter()
                    .map(|((binding, attribute), expression)| {
                        (
                            (map(binding.clone()), attribute.clone()),
                            expression.clone(),
                        )
                    })
                    .collect::<HashMap<_, _>>()
                    .into(),
                resource_reports,
                parameters: self.parameters.clone(),
            }
        }
        pub(super) fn matches_authority(
            &self,
            schemas: &crate::schema::SchemaRegistry,
            config: &super::super::ProviderContext,
        ) -> bool {
            self.authority.as_ref().is_some_and(|authority| {
                authority.schemas.same_snapshot(schemas)
                    && authority.catalog.same_snapshot(&config.custom_types)
                    && authority.resource_types == config.resource_types
            })
        }
        pub(crate) fn has_authored_judgement(&self) -> bool {
            self.authored
        }
        pub(super) fn admit_sink(&mut self, location: &DeclarationLocation, sink: &TypeExpr) {
            let expression = match self.expression(location) {
                Ok(expression) => expression,
                Err(error) => {
                    Arc::make_mut(&mut self.findings)
                        .push(Located::new(location.clone(), error.into()));
                    return;
                }
            };
            if expression.reported {
                return;
            }
            let requires_nominal_identity = expression.requires_declared_assignment()
                && expression.static_type() == Ok(&TypeExpr::String)
                && sink.is_string_shaped()
                && !matches!(sink, TypeExpr::String);
            if (expression.contains_deferred_interpolation() || requires_nominal_identity)
                && let Err(error) = admit_expression(expression, sink)
            {
                Arc::make_mut(&mut self.findings).push(Located::new(location.clone(), error));
                if let Some(expression) = Arc::make_mut(&mut self.expressions)
                    .get_mut(&ExpressionLocation::new(self.instance.clone(), location))
                {
                    expression.data_mut().reported = true;
                }
            }
        }
        pub fn binding(&self, name: &B) -> Result<&JudgedExpr, MissingEvidence> {
            self.bindings
                .get(name)
                .ok_or_else(|| MissingEvidence::new(format!("binding {name}")))
        }
        #[cfg(test)]
        pub(crate) fn types_at_for_test(&self, location: &DeclarationLocation) -> Vec<TypeExpr> {
            self.expressions
                .iter()
                .filter(|(key, _)| &key.authored == location)
                .filter_map(|(_, expr)| expr.static_type().ok().cloned())
                .collect()
        }
        pub fn expression(
            &self,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.expression_in(&self.instance, location)
        }
        pub(crate) fn instances_at(
            &self,
            location: &DeclarationLocation,
        ) -> Result<Vec<&JudgedExpr>, MissingEvidence> {
            let instances: Vec<_> = self
                .expressions
                .iter()
                .filter(|(key, _)| &key.authored == location)
                .map(|(_, expression)| expression)
                .collect();
            if instances.is_empty() {
                Err(MissingEvidence::new(format!(
                    "all instances at {location:?}"
                )))
            } else {
                Ok(instances)
            }
        }
        pub(crate) fn expression_in(
            &self,
            instance: &ExpressionInstance,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.expressions
                .get(&ExpressionLocation::new(instance.clone(), location))
                .ok_or_else(|| MissingEvidence::new(format!("{instance:?} at {location:?}")))
        }
        pub(crate) fn require_expression_in(
            &self,
            instance: &ExpressionInstance,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, InferenceError> {
            self.expression_in(instance, location).map_err(Into::into)
        }
        pub(crate) fn require_expression(
            &self,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, InferenceError> {
            self.require_expression_in(&self.instance, location)
        }
        pub fn for_resource(
            &self,
            id: &crate::resource::ResourceId,
        ) -> Result<&Self, MissingEvidence> {
            self.resource_reports
                .get(id)
                .map(Arc::as_ref)
                .ok_or_else(|| MissingEvidence::new(format!("resource {id}")))
        }
        pub fn module_attribute(
            &self,
            binding: &B,
            name: &str,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            self.module_attributes
                .get(&(binding.clone(), name.to_string()))
                .ok_or_else(|| MissingEvidence::new(format!("module {binding} attribute {name}")))
        }
        pub(crate) fn locations(&self) -> HashMap<DeclarationLocation, JudgedExpr> {
            self.expressions
                .iter()
                .filter(|(key, _)| key.instance == self.instance)
                .map(|(key, value)| (key.authored.clone(), value.clone()))
                .collect()
        }
        pub(crate) fn parameter(&self, name: &str) -> Result<&JudgedExpr, MissingEvidence> {
            self.parameters
                .get(name)
                .ok_or_else(|| MissingEvidence::new(format!("parameter {name}")))
        }
        pub(crate) fn record_instance<E, D>(
            &mut self,
            file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
            binding: Option<&B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
        {
            // Prototype IDs are local to the unexpanded module. Publish only
            // identities present in this contribution, retaining their scope.
            let ids: std::collections::HashSet<_> = file
                .iter_all_resources()
                .chain(file.deferred_for_phase.analysis_resource_refs())
                .map(|resource| resource.id().clone())
                .collect();
            Arc::make_mut(&mut self.resource_reports).retain(|id, _| ids.contains(id));
            if let Some(binding) = binding {
                for declaration in file.parser_binding_metadata.syntax().declarations() {
                    if declaration.detail().kind()
                        != crate::config_loader::DeclarationKind::ModuleAttribute
                    {
                        continue;
                    }
                    let evidence = declaration
                        .detail()
                        .value_location()
                        .ok_or_else(|| MissingEvidence::new("module attribute value location"))
                        .and_then(|location| self.expression(location))
                        .cloned();
                    match evidence {
                        Ok(expression) => {
                            Arc::make_mut(&mut self.module_attributes).insert(
                                (binding.clone(), declaration.detail().name().to_string()),
                                expression,
                            );
                        }
                        Err(error) => {
                            Arc::make_mut(&mut self.findings)
                                .push(Located::new(declaration.location().clone(), error.into()));
                        }
                    }
                }
            }
            self.record_resources(file);
        }
        pub(super) fn record_resources<E, D>(
            &mut self,
            file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        ) where
            D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
        {
            let mut snapshots = HashMap::new();
            for resource in file
                .iter_all_resources()
                .chain(file.deferred_for_phase.analysis_resource_refs())
            {
                if self.resource_reports.contains_key(resource.id()) {
                    continue;
                }
                let source = match resource {
                    super::super::ResourceRef::Resource(resource) => {
                        resource.module_source.as_ref()
                    }
                    super::super::ResourceRef::DataSource(source) => source.module_source.as_ref(),
                    super::super::ResourceRef::Deferred { resource, .. } => {
                        resource.as_resource().module_source.as_ref()
                    }
                    super::super::ResourceRef::LiveComposition(_) => None,
                };
                let instance = source
                    .and_then(crate::resource::ModuleSource::identity_scope)
                    .map(|scope| ExpressionInstance::for_scope(&scope))
                    .unwrap_or_else(|| self.instance.clone());
                let snapshot = snapshots.entry(instance.clone()).or_insert_with(|| {
                    let mut snapshot = self.clone();
                    snapshot.instance = instance;
                    snapshot.resource_reports = Arc::default();
                    Arc::new(snapshot)
                });
                Arc::make_mut(&mut self.resource_reports)
                    .insert(resource.id().clone(), snapshot.clone());
            }
        }
        pub fn operand(
            &self,
            location: &DeclarationLocation,
            side: crate::integer_arithmetic::IntegerOperandSide,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            let index = match side {
                crate::integer_arithmetic::IntegerOperandSide::Left => 0,
                crate::integer_arithmetic::IntegerOperandSide::Right => 1,
            };
            self.operands
                .get(&ExpressionLocation::new(self.instance.clone(), location))
                .map(|operands| &operands[index])
                .ok_or_else(|| {
                    MissingEvidence::new(format!("integer operand {side:?} at {location:?}"))
                })
        }
        pub fn access<P: ValueKind>(
            &self,
            access: &crate::resource::AccessExpr<P, B>,
        ) -> Result<&JudgedExpr, MissingEvidence> {
            let location = access
                .access_locations()
                .ok_or_else(|| MissingEvidence::new("access location"))?;
            self.expression(location.expression_location())
        }
        pub fn findings(&self) -> &[Located<InferenceError>] {
            &self.findings
        }
        pub(crate) fn first_function_error(&self) -> Option<super::super::ParseError> {
            self.function_errors.first().map(|finding| {
                super::super::ParseError::InvalidExpression {
                    line: finding.location().span().start_line(),
                    message: finding.detail().to_string(),
                }
                .at(finding.location())
            })
        }
        pub fn first_error(&self) -> Option<super::super::ParseError> {
            self.findings.first().map(|finding| {
                super::super::ParseError::InvalidExpression {
                    line: finding.location().span().start_line(),
                    message: finding.detail().to_string(),
                }
                .at(finding.location())
            })
        }
    }

    pub fn judge_expressions<E, D, B: BindingAddress>(
        file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &crate::schema::SchemaRegistry,
        config: &super::super::ProviderContext,
    ) -> ExpressionReport<B>
    where
        D: super::super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let expressions = file.parser_binding_metadata.syntax().expressions();
        let mut bindings = inference::bindings_from_parsed(file);
        for wait in &file.wait_bindings {
            if let Some(target) = bindings.get(wait.target.as_str()).cloned() {
                bindings.insert(wait.binding.to_string(), target);
            }
        }
        fn index<'a, B: BindingAddress>(
            expression: &'a ExprValue<ParserPlaceholder, B>,
            authored: &mut HashMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
            visited: &mut std::collections::HashSet<usize>,
        ) {
            if !visited.insert(Arc::as_ptr(&expression.authored) as usize) {
                return;
            }
            authored
                .entry(expression.authored.location.clone())
                .or_insert(expression);
            for child in &expression.authored.children {
                index(child, authored, visited);
            }
        }
        let mut authored = HashMap::new();
        let mut visited = std::collections::HashSet::new();
        for expression in expressions
            .roots
            .iter()
            .chain(expressions.definitions.values())
        {
            index(expression, &mut authored, &mut visited);
        }
        let functions = authored
            .values()
            .filter_map(|expression| {
                if let ExprKind::Function { name, signature } = &expression.authored.kind {
                    Some((
                        (expression.authored.location.instance.clone(), name.clone()),
                        signature.clone(),
                    ))
                } else {
                    None
                }
            })
            .collect();
        let mut judge = Judge {
            authored,
            expressions,
            bindings: &bindings,
            functions,
            schemas,
            config,
            memo: HashMap::new(),
            by_location: HashMap::new(),
            visiting: Vec::new(),
            findings: Vec::new(),
            function_errors: Vec::new(),
            operands: HashMap::new(),
            module_attributes: Some(&expressions.report.module_attributes),
        };
        let mut judged_bindings = HashMap::new();
        for (name, expression) in &expressions.definitions {
            judged_bindings.insert(name.clone(), judge.judge(expression));
        }
        for expression in &expressions.roots {
            judge.judge(expression);
        }
        let module_attributes = expressions
            .report
            .module_attributes
            .iter()
            .map(|(key, evidence)| {
                let refreshed = judge.refresh_argument(evidence);
                (key.clone(), refreshed)
            })
            .collect();
        let mut report = ExpressionReport {
            instance: expressions.instance.clone(),
            authored: true,
            authority: Some(Arc::new(JudgementAuthority {
                schemas: schemas.clone(),
                catalog: config.custom_types.clone(),
                resource_types: config.resource_types.clone(),
            })),
            bindings: Arc::new(judged_bindings),
            expressions: Arc::new(judge.by_location),
            findings: Arc::new(judge.findings),
            function_errors: Arc::new(judge.function_errors),
            operands: Arc::new(judge.operands),
            module_attributes: Arc::new(module_attributes),
            resource_reports: Arc::default(),
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (
                        argument.name.clone(),
                        declared(Ok(argument.static_type.clone()), false),
                    )
                })
                .collect::<HashMap<_, _>>()
                .into(),
        };
        let mut snapshots = HashMap::new();
        for (id, previous) in expressions.report.resource_reports.iter() {
            let snapshot = snapshots
                .entry(previous.instance.clone())
                .or_insert_with(|| {
                    let mut snapshot = report.clone();
                    snapshot.instance = previous.instance.clone();
                    snapshot.resource_reports = Arc::default();
                    Arc::new(snapshot)
                });
            Arc::make_mut(&mut report.resource_reports).insert(id.clone(), snapshot.clone());
        }
        report.record_instance(file, None);
        report
    }

    struct Judge<'a, B: BindingAddress> {
        authored: HashMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
        expressions: &'a Expressions<B>,
        bindings: &'a InferenceBindings<ParserPlaceholder, B>,
        functions: HashMap<(ExpressionInstance, String), Arc<FunctionSignature>>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &'a super::super::ProviderContext,
        memo: HashMap<usize, JudgedExpr>,
        by_location: HashMap<ExpressionLocation, JudgedExpr>,
        visiting: Vec<usize>,
        findings: Vec<Located<InferenceError>>,
        function_errors: Vec<Located<InferenceError>>,
        operands: HashMap<ExpressionLocation, [JudgedExpr; 2]>,
        module_attributes: Option<&'a HashMap<(B, String), JudgedExpr>>,
    }

    impl<B: BindingAddress> Judge<'_, B> {
        fn judge(&mut self, expression: &ExprValue<ParserPlaceholder, B>) -> JudgedExpr {
            let root = &expression.authored;
            let id = Arc::as_ptr(root) as usize;
            if let Some(judged) = self.memo.get(&id) {
                return judged.clone();
            }
            if self.visiting.contains(&id) {
                let mut failure = declared(
                    Err(InferenceError::CyclicVirtualBinding {
                        cycle: vec!["cyclic authored binding".into()],
                    }
                    .into()),
                    false,
                );
                failure.failure_owned_by(FailureOwner::BindingIndex);
                return failure;
            }
            self.visiting.push(id);
            // Always visit every child, even for length(), selected access,
            // interpolation, and a coalesce whose left payload is concrete.
            let children = root
                .children
                .iter()
                .map(|child| self.judge(child))
                .collect::<Vec<_>>();
            let mut result = self.infer(expression, &children);
            result.data_mut().origin = Some(root.location.clone());
            if matches!(root.kind, ExprKind::Reference { .. }) {
                result.data_mut().direct_reference =
                    result.judgement.kind != InferredKind::EnumIdentifier;
                if result.judgement.kind == InferredKind::Ordinary {
                    Arc::make_mut(&mut result.data_mut().judgement).kind = InferredKind::Reference;
                }
                // The binding pass owns reference existence and field errors.
                // A containing operation preserves that ownership.
                result.failure_owned_by(FailureOwner::BindingIndex);
            }
            if let Some(failed) = children.iter().find(|child| child.judgement.invalid) {
                Arc::make_mut(&mut result.data_mut().judgement).ty = failed.judgement.ty.clone();
                Arc::make_mut(&mut result.data_mut().judgement).invalid = true;
                result.data_mut().reported = failed.reported;
                if failed.judgement.failure_owner == FailureOwner::BindingScope {
                    result.failure_owned_by(FailureOwner::BindingScope);
                }
                result.data_mut().boundary_owner = failed.boundary_owner.clone();
            }
            if result.judgement.invalid
                && !result.reported
                && result.judgement.failure_owner == FailureOwner::Expression
                && let Err(InferenceIssue::Failed(error)) = result.static_type()
            {
                let location = match error {
                    InferenceError::InvalidIntegerArithmeticOperand { operator_span, .. } => {
                        operator_span.location().unwrap_or(&root.location.authored)
                    }
                    _ if matches!(root.kind, ExprKind::Coalesce) => root.children[1].location(),
                    _ => &root.location.authored,
                }
                .clone();
                self.push_finding(location, error.clone());
                result.data_mut().reported = true;
            }
            self.visiting.pop();
            self.memo.insert(id, result.clone());
            self.by_location
                .insert(root.location.clone(), result.clone());
            result
        }

        fn refresh_argument(&mut self, evidence: &JudgedExpr) -> JudgedExpr {
            let Some(origin) = evidence.origin.as_ref() else {
                return evidence.clone();
            };
            match self.authored.get(origin).copied() {
                Some(authored) => self.judge(authored),
                None => incomplete("authored argument origin"),
            }
        }
        fn check_function_sink(
            &mut self,
            expression: &JudgedExpr,
            sink: &TypeExpr,
            location: &DeclarationLocation,
            owner: &DeclarationLocation,
            context: &str,
        ) -> Option<JudgedExpr> {
            let error = crate::validation::typecheck::admit_function_expression(
                expression,
                sink,
                self.config,
            )
            .err()?;
            let error = match error {
                crate::validation::typecheck::FunctionExpressionError::Annotation(error) => {
                    let mut failed = declared(
                        Err(InferenceError::UnknownType {
                            reason: error.to_string(),
                        }
                        .into()),
                        expression.concrete,
                    );
                    // Declaration resolution owns this error. It still makes
                    // the function/call judgement invalid.
                    failed.data_mut().reported = true;
                    return Some(failed);
                }
                crate::validation::typecheck::FunctionExpressionError::Expression(error) => error,
            };
            if expression.judgement.invalid {
                return Some(expression.clone());
            }
            let finding = Located::new(
                location.clone(),
                InferenceError::UnknownType {
                    reason: format!("{context}: {error}"),
                },
            );
            if expression.judgement.kind == InferredKind::StringConstruction
                && !expression.concrete
                && !self.function_errors.contains(&finding)
            {
                self.function_errors.push(finding.clone());
            }
            self.push_finding(finding.location().clone(), finding.detail().clone());
            let Some(origin) = expression.origin.as_ref() else {
                return Some(incomplete("function boundary origin"));
            };
            let Some(owned) = self.by_location.get_mut(origin) else {
                return Some(incomplete("function boundary evidence"));
            };
            owned.data_mut().reported = true;
            let owner = Arc::new(owner.clone());
            owned.data_mut().boundary_owner = Some(owner.clone());
            let mut failed = declared(Err(finding.detail().clone().into()), expression.concrete);
            failed.data_mut().reported = true;
            failed.data_mut().boundary_owner = Some(owner);
            Some(failed)
        }

        fn push_finding(&mut self, location: DeclarationLocation, error: InferenceError) {
            if !self
                .findings
                .iter()
                .any(|finding| finding.location() == &location && finding.detail() == &error)
            {
                self.findings.push(Located::new(location, error));
            }
        }

        fn narrow_declared(
            &self,
            ty: &TypeExpr,
            steps: &[PathSegment],
            binding: &str,
        ) -> Result<TypeExpr, InferenceIssue> {
            let mut current = ty.clone();
            for step in steps {
                if let TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path) = &current
                    && self
                        .schemas
                        .get(
                            &path.provider,
                            &path.resource_type,
                            crate::schema::SchemaKind::Resource,
                        )
                        .or_else(|| {
                            self.schemas.get(
                                &path.provider,
                                &path.resource_type,
                                crate::schema::SchemaKind::DataSource,
                            )
                        })
                        .is_none()
                {
                    return Err(InferenceIssue::NotInferable(
                        NotInferableReason::SchemaUnavailable {
                            binding: binding.to_string(),
                        },
                    ));
                }
                current = crate::validation::narrow_type_expr(
                    &current,
                    std::slice::from_ref(step),
                    Some(self.schemas),
                )
                .ok_or_else(|| InferenceError::InvalidAccess {
                    base_type: current.to_string(),
                    step: step.clone(),
                })?;
            }
            Ok(current)
        }

        fn infer(
            &mut self,
            expression: &ExprValue<ParserPlaceholder, B>,
            children: &[JudgedExpr],
        ) -> JudgedExpr {
            let root = &expression.authored;
            let ordinary = declared;
            let child_type = |index: usize| {
                children.get(index).map_or_else(
                    || incomplete("operand").judgement.ty.clone(),
                    |child| child.judgement.ty.clone(),
                )
            };
            match &root.kind {
                ExprKind::Scalar(ty) => {
                    let mut value = ordinary(Ok(ty.clone()), true);
                    Arc::make_mut(&mut value.data_mut().judgement).kind = InferredKind::Literal;
                    value
                }
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                ExprKind::UntypedParameter => value_dependent("unannotated function parameter"),
                ExprKind::Binding(name) => {
                    if let Some(argument) = self.expressions.arguments.get(name) {
                        return ordinary(
                            Ok(argument.declared_type.clone()),
                            argument.value.concrete,
                        )
                        .owned_by_bindings();
                    }
                    if let Some(InferenceBinding::Argument { type_expr }) =
                        self.bindings.get(name.as_str())
                    {
                        ordinary(
                            Ok(type_expr.clone()),
                            self.expressions
                                .arguments
                                .get(name)
                                .is_some_and(|argument| argument.value.concrete),
                        )
                        .owned_by_bindings()
                    } else if let Some(definition) = self.expressions.definitions.get(name) {
                        self.judge(definition)
                    } else {
                        let mut value = ordinary(Ok(TypeExpr::String), true);
                        Arc::make_mut(&mut value.data_mut().judgement).kind = InferredKind::Literal;
                        value
                    }
                }
                ExprKind::Reference {
                    binding,
                    attribute,
                    segments,
                } => {
                    let steps = std::iter::once(PathSegment::Field {
                        name: attribute.clone(),
                    })
                    .chain(segments.iter().cloned())
                    .collect::<Vec<_>>();
                    if let Some(InferenceBinding::Argument { type_expr }) =
                        self.bindings.get(binding.as_str())
                    {
                        return ordinary(
                            self.narrow_declared(type_expr, &steps, binding.as_str()),
                            self.expressions
                                .arguments
                                .get(binding)
                                .is_some_and(|argument| argument.value.concrete),
                        );
                    }
                    if let Some(argument) = self.expressions.arguments.get(binding) {
                        return ordinary(
                            self.narrow_declared(&argument.declared_type, &steps, binding.as_str()),
                            argument.value.concrete,
                        );
                    }
                    if let Some(attribute) = self.module_attributes.and_then(|attributes| {
                        attributes.get(&(binding.clone(), attribute.clone()))
                    }) {
                        let attribute = self.refresh_argument(attribute);
                        return ordinary(
                            attribute
                                .static_type()
                                .cloned()
                                .map_err(Clone::clone)
                                .and_then(|ty| {
                                    if segments.is_empty() {
                                        Ok(ty)
                                    } else {
                                        crate::validation::narrow_type_expr(
                                            &ty,
                                            segments,
                                            Some(self.schemas),
                                        )
                                        .ok_or(
                                            InferenceIssue::NotInferable(
                                                NotInferableReason::MissingValue,
                                            ),
                                        )
                                    }
                                }),
                            attribute.concrete,
                        );
                    }
                    if matches!(
                        self.bindings.get(binding.as_str()),
                        Some(InferenceBinding::UpstreamState | InferenceBinding::ModuleCall)
                    ) {
                        let mut result = value_dependent(
                            "external value is checked at its declared or resolved boundary",
                        );
                        Arc::make_mut(&mut result.data_mut().judgement).ty = Err(
                            InferenceIssue::NotInferable(NotInferableReason::NonInferableBinding {
                                binding: binding.to_string(),
                            }),
                        );
                        return result;
                    }
                    if let Some(InferenceBinding::Virtual { attributes }) =
                        self.bindings.get(binding.as_str())
                        && !attributes.contains_key(attribute)
                    {
                        return ordinary(
                            Err(InferenceError::UnknownAttribute {
                                binding: binding.to_string(),
                                attribute: attribute.clone(),
                            }
                            .into()),
                            false,
                        );
                    }
                    if self.bindings.contains_key(binding.as_str()) {
                        ordinary(
                            inference::infer_declared_reference_type(
                                binding.as_str(),
                                attribute,
                                segments,
                                self.bindings,
                                self.schemas,
                            ),
                            false,
                        )
                    } else if let Some(definition) = self.expressions.definitions.get(binding) {
                        let base = self.judge(definition);
                        ordinary(
                            base.judgement
                                .ty
                                .clone()
                                .and_then(|ty| self.narrow_declared(&ty, &steps, binding.as_str())),
                            base.concrete,
                        )
                    } else {
                        let mut literal = ordinary(Ok(TypeExpr::String), true);
                        Arc::make_mut(&mut literal.data_mut().judgement).kind =
                            InferredKind::EnumIdentifier;
                        literal
                    }
                }
                ExprKind::EmptyInterpolation => ordinary(Ok(TypeExpr::String), false),
                ExprKind::Interpolation => JudgedExpr(Arc::new(JudgedExprData {
                    origin: None,
                    concrete: children.iter().all(|child| child.concrete),
                    reported: false,
                    direct_reference: false,
                    boundary_owner: None,
                    coalesce: None,
                    alternatives: Arc::default(),
                    member: None,
                    elements: Arc::default(),
                    fields: Arc::default(),
                    judgement: Arc::new(ExprJudgement {
                        ty: Ok(TypeExpr::String),
                        kind: InferredKind::StringConstruction,
                        invalid: false,
                        failure_owner: FailureOwner::Expression,
                    }),
                })),
                ExprKind::Coalesce => {
                    let left = child_type(0);
                    let right = child_type(1);
                    let right_expression = &children[1];
                    let empty = match right_expression.collection_kind() {
                        Some(CollectionKind::List)
                            if right_expression.elements.is_empty()
                                && right_expression.member.is_none() =>
                        {
                            EmptyCoalesceDefault::List
                        }
                        Some(CollectionKind::Map)
                            if right_expression.fields.is_empty()
                                && right_expression.member.is_none() =>
                        {
                            EmptyCoalesceDefault::Map
                        }
                        _ => EmptyCoalesceDefault::None,
                    };
                    let incompatible = match (&left, &right) {
                        (Ok(left), Ok(right)) => left != right,
                        _ => match (children[0].value_shape(), children[1].value_shape()) {
                            (Some(left), Some(right)) => left != right,
                            _ => false,
                        },
                    };
                    let ty = if incompatible {
                        match (&left, &right) {
                            (Ok(left), Ok(right)) => {
                                Err(InferenceError::IncompatibleCoalesceTypes {
                                    types: crate::schema::MismatchTypeNames::for_type_exprs(
                                        left, right,
                                    ),
                                }
                                .into())
                            }
                            _ => Err(InferenceError::UnknownType {
                                reason: format!(
                                    "coalesce default type `{}` does not match left type `{}`",
                                    children[1].type_description(),
                                    children[0].type_description()
                                ),
                            }
                            .into()),
                        }
                    } else {
                        coalesce_type(left, right, empty)
                    };
                    let mut result = alternatives(ty, children);
                    Arc::make_mut(&mut result.data_mut().judgement).invalid |= incompatible;
                    result.data_mut().coalesce =
                        Some(Arc::new([children[0].clone(), children[1].clone()]));
                    result
                }
                ExprKind::List | ExprKind::Map(_) => {
                    let mut shapes = children
                        .iter()
                        .filter_map(|child| child.value_shape().map(|shape| (child, shape)));
                    let incompatible = if matches!(root.kind, ExprKind::List)
                        && let Some((first, shape)) = shapes.next()
                    {
                        shapes.find_map(|(next, next_shape)| {
                            (shape != next_shape).then(|| {
                                match (first.static_type(), next.static_type()) {
                                    (Ok(first), Ok(next)) => {
                                        InferenceError::HeterogeneousCollection {
                                            types: crate::schema::MismatchTypeNames::for_type_exprs(
                                                first, next,
                                            ),
                                        }
                                    }
                                    _ => InferenceError::UnknownType {
                                        reason: format!(
                                            "collection element types disagree: `{}` vs `{}`",
                                            first.type_description(),
                                            next.type_description()
                                        ),
                                    },
                                }
                            })
                        })
                    } else {
                        None
                    };
                    let ty = if let Some(error) = &incompatible {
                        Err(error.clone().into())
                    } else {
                        match children.first() {
                            Some(first) => first.judgement.ty.clone().and_then(|first| {
                                for child in &children[1..] {
                                    let next = child.static_type().map_err(Clone::clone)?;
                                    if *next != first {
                                        return Err(InferenceError::HeterogeneousCollection {
                                            types: crate::schema::MismatchTypeNames::for_type_exprs(
                                                &first, next,
                                            ),
                                        }
                                        .into());
                                    }
                                }
                                Ok(if matches!(root.kind, ExprKind::List) {
                                    TypeExpr::List(Box::new(first))
                                } else {
                                    TypeExpr::Map(Box::new(first))
                                })
                            }),
                            None => Err(InferenceIssue::NotInferable(
                                NotInferableReason::MissingValue,
                            )),
                        }
                    };
                    let invalid = incompatible.is_some()
                        || (matches!(&root.kind, ExprKind::List)
                            && matches!(
                                &ty,
                                Err(InferenceIssue::Failed(
                                    InferenceError::HeterogeneousCollection { .. }
                                ))
                            ));
                    let mut result = ordinary(ty, children.iter().all(|child| child.concrete));
                    result.failure_owned_by(FailureOwner::Boundary);
                    result.data_mut().member = None;
                    Arc::make_mut(&mut result.data_mut().judgement).invalid = invalid;
                    Arc::make_mut(&mut result.data_mut().judgement).kind =
                        if matches!(root.kind, ExprKind::List) {
                            InferredKind::List
                        } else {
                            InferredKind::Map
                        };
                    match &root.kind {
                        ExprKind::List => result.data_mut().elements = Arc::new(children.to_vec()),
                        ExprKind::Map(keys) => {
                            result.data_mut().fields = Arc::new(
                                keys.iter().cloned().zip(children.iter().cloned()).collect(),
                            )
                        }
                        _ => unreachable!("collection judgement"),
                    }
                    result
                }
                ExprKind::Function { name, signature } => {
                    let body = children
                        .last()
                        .cloned()
                        .unwrap_or_else(|| incomplete("authored child"));
                    if let Some(sink) = &signature.return_type
                        && let Some(failure) = self.check_function_sink(
                            &body,
                            sink,
                            root.children.last().unwrap().location(),
                            root.children.last().unwrap().location(),
                            &format!("function '{name}': return value"),
                        )
                    {
                        return failure;
                    }
                    declaration()
                }
                ExprKind::Sequence => children
                    .last()
                    .cloned()
                    .unwrap_or_else(|| incomplete("authored child")),
                ExprKind::Call(name) => {
                    let mut argument_failure = None;
                    let signature = self
                        .functions
                        .get(&(root.location.instance.clone(), name.clone()))
                        .cloned();
                    if let Some(signature) = &signature {
                        for (((parameter, parameter_type), child), authored) in signature
                            .parameters
                            .iter()
                            .zip(children)
                            .zip(&root.children)
                        {
                            if let Some(sink) = parameter_type
                                && let Some(failure) = self.check_function_sink(
                                    child,
                                    sink,
                                    authored.location(),
                                    &root.location.authored,
                                    &format!("function '{name}': parameter '{parameter}'"),
                                )
                            {
                                argument_failure.get_or_insert(failure);
                            }
                        }
                    }
                    if let Some(failure) = argument_failure {
                        return failure;
                    }
                    if let Some(ty) = signature
                        .as_ref()
                        .and_then(|signature| signature.return_type.as_ref())
                    {
                        return ordinary(Ok(ty.clone()), false);
                    }
                    let builtin = crate::builtins::builtin_functions()
                        .iter()
                        .find(|builtin| builtin.name == name);
                    if let Some(crate::builtins::BuiltinReturnType::Argument(index)) =
                        builtin.map(|builtin| builtin.return_type)
                    {
                        return children
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| incomplete("builtin argument"));
                    }
                    let return_type = builtin.map(|builtin| builtin.return_type);
                    let ty = match return_type {
                        Some(
                            crate::builtins::BuiltinReturnType::String
                            | crate::builtins::BuiltinReturnType::Secret,
                        ) => Ok(TypeExpr::String),
                        Some(crate::builtins::BuiltinReturnType::Int) => Ok(TypeExpr::Int),
                        Some(crate::builtins::BuiltinReturnType::StringList) => {
                            Ok(TypeExpr::List(Box::new(TypeExpr::String)))
                        }
                        Some(
                            crate::builtins::BuiltinReturnType::Argument(_)
                            | crate::builtins::BuiltinReturnType::List
                            | crate::builtins::BuiltinReturnType::Map
                            | crate::builtins::BuiltinReturnType::Any,
                        ) => Err(InferenceError::UnknownType {
                            reason: format!("built-in `{name}` return type depends on arguments"),
                        }
                        .into()),
                        None if signature.is_some() => {
                            value_dependent("unannotated function result")
                                .judgement
                                .ty
                                .clone()
                        }
                        None => return incomplete("function declaration or builtin signature"),
                    };
                    let mut result = ordinary(ty, children.iter().all(|child| child.concrete));
                    match return_type {
                        Some(
                            crate::builtins::BuiltinReturnType::List
                            | crate::builtins::BuiltinReturnType::Map,
                        ) => {
                            Arc::make_mut(&mut result.data_mut().judgement).kind = if matches!(
                                return_type,
                                Some(crate::builtins::BuiltinReturnType::List)
                            ) {
                                InferredKind::List
                            } else {
                                InferredKind::Map
                            };
                            Arc::make_mut(&mut result.data_mut().judgement).invalid = false;
                            result.data_mut().member = Some(Arc::new(value_dependent(
                                "builtin elements require resolved constraint checks",
                            )));
                        }
                        Some(crate::builtins::BuiltinReturnType::Any) | None => {
                            Arc::make_mut(&mut result.data_mut().judgement).kind =
                                InferredKind::ValueDependent;
                            Arc::make_mut(&mut result.data_mut().judgement).invalid = false;
                        }
                        _ => {}
                    }
                    result
                }
                ExprKind::Access(step) => {
                    let base = &children[0];
                    if base.judgement.kind == InferredKind::EnumIdentifier {
                        return base.clone();
                    }
                    if base.is_value_dependent() {
                        return base.clone();
                    }
                    let selected = match step {
                        PathSegment::Subscript {
                            index: crate::resource::Subscript::Int { index },
                        } => usize::try_from(*index).ok().and_then(|index| {
                            base.elements.get(index).or_else(|| {
                                base.member
                                    .as_deref()
                                    .filter(|_| base.value_shape() == Some(ValueShape::List))
                            })
                        }),
                        PathSegment::Subscript {
                            index: crate::resource::Subscript::Str { key },
                        } => base.fields.get(key.expose_plaintext()).or_else(|| {
                            base.member
                                .as_deref()
                                .filter(|_| base.value_shape() == Some(ValueShape::Map))
                        }),
                        PathSegment::Field { name } => base.fields.get(name).or_else(|| {
                            base.member
                                .as_deref()
                                .filter(|_| base.value_shape() == Some(ValueShape::Map))
                        }),
                    };
                    if let Some(selected) = selected {
                        return selected.clone();
                    }
                    let ty = child_type(0).and_then(|base| {
                        self.narrow_declared(&base, std::slice::from_ref(step), "access base")
                    });
                    let invalid = ty.is_err();
                    let mut result = ordinary(ty, base.concrete);
                    Arc::make_mut(&mut result.data_mut().judgement).invalid = invalid;
                    result.failure_owned_by(match base.judgement.failure_owner {
                        FailureOwner::BindingIndex => FailureOwner::BindingIndex,
                        _ => FailureOwner::Boundary,
                    });
                    if matches!(
                        base.static_type(),
                        Ok(TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_))
                    ) {
                        Arc::make_mut(&mut result.data_mut().judgement).kind =
                            InferredKind::Reference;
                    }
                    result
                }
                ExprKind::Integer { op, location } => {
                    let operands = [children[0].clone(), children[1].clone()];
                    self.operands.insert(
                        ExpressionLocation::new(root.location.instance.clone(), location),
                        operands.clone(),
                    );
                    let mut result = ordinary(
                        inference::judge_integer_operands(
                            *op,
                            location,
                            &operands,
                            self.schemas,
                            self.config,
                        ),
                        children.iter().all(|child| child.concrete),
                    );
                    Arc::make_mut(&mut result.data_mut().judgement).invalid =
                        result.static_type().is_err();
                    result
                }
                ExprKind::Iterable => {
                    fn root_binding<B: BindingAddress>(
                        expression: &ExprValue<ParserPlaceholder, B>,
                    ) -> Option<&B> {
                        match &expression.authored.kind {
                            ExprKind::Binding(binding) | ExprKind::Reference { binding, .. } => {
                                Some(binding)
                            }
                            ExprKind::Alias | ExprKind::Access(_) | ExprKind::DynamicAccess => {
                                expression.authored.children.first().and_then(root_binding)
                            }
                            _ => None,
                        }
                    }
                    if let Some(binding) = root.children.first().and_then(root_binding)
                        && !self.bindings.contains_key(binding.as_str())
                        && !self.expressions.definitions.contains_key(binding)
                        && !self.expressions.arguments.contains_key(binding)
                    {
                        let mut failure = ordinary(
                            Err(InferenceError::UnknownBinding {
                                binding: binding.to_string(),
                            }
                            .into()),
                            false,
                        );
                        failure.failure_owned_by(FailureOwner::BindingScope);
                        return failure;
                    }
                    children
                        .first()
                        .cloned()
                        .unwrap_or_else(|| incomplete("iterable operand"))
                }
                ExprKind::Alias | ExprKind::Wait => children
                    .first()
                    .cloned()
                    .unwrap_or_else(|| incomplete("authored child")),
                ExprKind::Element => {
                    let collection = &children[0];
                    if collection.is_value_dependent() {
                        return collection.clone();
                    }
                    if let Some(member) = &collection.member {
                        return (**member).clone();
                    }
                    let members = collection
                        .elements
                        .iter()
                        .chain(collection.fields.values())
                        .cloned()
                        .collect::<Vec<_>>();
                    if !members.is_empty() {
                        let mut element = members[0].clone();
                        element.data_mut().alternatives = Arc::new(members);
                        return element;
                    }
                    match child_type(0) {
                        Ok(TypeExpr::List(inner) | TypeExpr::Map(inner)) => {
                            ordinary(Ok(*inner), false)
                        }
                        Ok(actual) => {
                            let mut failure = ordinary(
                                Err(InferenceError::UnknownType {
                                    reason: format!(
                                        "for iterable must be a list or map, got {actual}"
                                    ),
                                }
                                .into()),
                                false,
                            );
                            failure.failure_owned_by(FailureOwner::BindingScope);
                            failure
                        }
                        Err(error) => ordinary(Err(error), false),
                    }
                }
                ExprKind::Sink(_) => children
                    .first()
                    .cloned()
                    .unwrap_or_else(|| incomplete("authored child")),
                ExprKind::Resource {
                    provider,
                    resource_type,
                    read,
                } => {
                    let kind = if *read {
                        crate::schema::SchemaKind::DataSource
                    } else {
                        crate::schema::SchemaKind::Resource
                    };
                    if let Some(schema) = self.schemas.get(provider, resource_type, kind) {
                        self.admit_resource_children(&root.children, schema);
                    }
                    let mut result = ordinary(
                        Ok(TypeExpr::Ref(super::super::ResourceTypePath::new(
                            provider,
                            resource_type,
                        ))),
                        false,
                    );
                    result.failure_owned_by(FailureOwner::Boundary);
                    result
                }
                ExprKind::If => {
                    let branches = &children[1..];
                    let mut ty = branches
                        .first()
                        .map(|branch| branch.judgement.ty.clone())
                        .unwrap_or_else(|| incomplete("if branch").judgement.ty.clone());
                    for branch in branches.iter().skip(1) {
                        match (&ty, branch.static_type()) {
                            (Ok(left), Ok(right)) if left != right => {
                                let mut members = match left {
                                    TypeExpr::Union(members) => members.clone(),
                                    left => vec![left.clone()],
                                };
                                if !members.contains(right) {
                                    members.push(right.clone());
                                }
                                ty = Ok(TypeExpr::Union(members));
                            }
                            (Err(_), Ok(_))
                                if branches[0].elements.is_empty()
                                    && branches[0].judgement.kind == InferredKind::List =>
                            {
                                ty = branch.judgement.ty.clone()
                            }
                            _ => {}
                        }
                    }
                    let mut result = alternatives(ty, branches);
                    result.data_mut().concrete = children.iter().all(|child| child.concrete);
                    result
                }
                ExprKind::For => {
                    if let Some(collection) = children.first()
                        && collection.value_shape().is_some_and(|shape| {
                            !matches!(shape, ValueShape::List | ValueShape::Map)
                        })
                    {
                        let mut failure = ordinary(
                            Err(InferenceError::UnknownType {
                                reason: format!(
                                    "for iterable must be a list or map, got {}",
                                    collection.type_description()
                                ),
                            }
                            .into()),
                            false,
                        );
                        // Keep the operand's own type for the precise iterable
                        // diagnostic; the loop operation is invalid.
                        failure.failure_owned_by(FailureOwner::BindingScope);
                        return failure;
                    }
                    let body = children
                        .last()
                        .cloned()
                        .unwrap_or_else(|| incomplete("for body"));
                    let mut result = ordinary(
                        body.judgement
                            .ty
                            .clone()
                            .map(|ty| TypeExpr::List(Box::new(ty))),
                        false,
                    );
                    Arc::make_mut(&mut result.data_mut().judgement).invalid =
                        body.judgement.invalid;
                    Arc::make_mut(&mut result.data_mut().judgement).kind = InferredKind::List;
                    result.data_mut().member = Some(Arc::new(body));
                    result
                }
                ExprKind::DynamicAccess => {
                    let base = &children[0];
                    if base.is_value_dependent() {
                        return base.clone();
                    }
                    if let Some(member) = &base.member {
                        return (**member).clone();
                    }
                    let candidates: Vec<_> = base
                        .elements
                        .iter()
                        .chain(base.fields.values())
                        .cloned()
                        .collect();
                    if candidates.is_empty() {
                        return incomplete("dynamic access element");
                    }
                    let mut result = candidates[0].clone();
                    result.data_mut().alternatives = Arc::new(candidates);
                    result
                }
                ExprKind::Boolean => {
                    let mut result = ordinary(
                        Ok(TypeExpr::Bool),
                        children.iter().all(|child| child.concrete),
                    );
                    // Conditions retain their dedicated wait/validation checks.
                    result.failure_owned_by(FailureOwner::Boundary);
                    result
                }
                ExprKind::NestedBlock { .. } => children[0].clone(),
                ExprKind::ModuleCall => {
                    let mut result = value_dependent("module call result requires its signature");
                    result.failure_owned_by(FailureOwner::Boundary);
                    result
                }
                ExprKind::Declaration | ExprKind::ProviderDeclaration | ExprKind::Compose => {
                    declaration()
                }
                ExprKind::Unsupported(rule) => {
                    incomplete(&format!("unsupported authored rule {rule}"))
                }
            }
        }

        fn admit_resource_children(
            &mut self,
            children: &[ExprValue<ParserPlaceholder, B>],
            schema: crate::schema::OwnerBoundResourceSchema<'_>,
        ) {
            for child in children {
                match &child.authored.kind {
                    ExprKind::Sink(name) => {
                        let aliases = schema.schema().as_schema().block_name_map();
                        let canonical = aliases.get(name).map(String::as_str).unwrap_or(name);
                        if let Some(attribute) = schema.schema().effective_attribute(canonical) {
                            if attribute.is_read_only() {
                                continue;
                            }
                            let expected = crate::validation::expression_sink_type(
                                attribute.field_type(),
                                schema.owner(),
                            );
                            let value = &child.authored.children[0];
                            let judged = self.judge(value);
                            match admit_expression(&judged, &expected) {
                                Err(error)
                                    if judged.needs_authored_sink_check() && !judged.reported =>
                                {
                                    self.push_finding(value.location().clone(), error);
                                }
                                Ok(
                                    ExpressionAdmission::ValueCheck
                                    | ExpressionAdmission::ProviderPending(_)
                                    | ExpressionAdmission::DeferredInterpolation,
                                )
                                | Err(_) => {}
                            }
                        }
                    }
                    ExprKind::Resource { .. } => {}
                    _ => self.admit_resource_children(&child.authored.children, schema),
                }
            }
        }
    }

    /// Join source evidence independently of a selected projection. Exact
    /// containers keep evidence for every index/key any branch can produce.
    fn alternatives(ty: Result<TypeExpr, InferenceIssue>, branches: &[JudgedExpr]) -> JudgedExpr {
        if branches.len() == 1 {
            return branches[0].clone();
        }
        let mut result = declared(ty, branches.iter().all(|branch| branch.concrete));
        Arc::make_mut(&mut result.data_mut().judgement).invalid =
            branches.iter().any(|branch| branch.judgement.invalid);
        result.data_mut().alternatives = Arc::new(branches.to_vec());
        let member_sources = branches
            .iter()
            .filter_map(|branch| branch.member.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        result.data_mut().member = member_sources
            .first()
            .map(|first| Arc::new(alternatives(first.judgement.ty.clone(), &member_sources)));
        let max_len = branches
            .iter()
            .map(|branch| branch.elements.len())
            .max()
            .unwrap_or(0);
        result.data_mut().elements = Arc::new(
            (0..max_len)
                .map(|index| {
                    let sources = branches
                        .iter()
                        .filter_map(|branch| {
                            branch.elements.get(index).or(branch.member.as_deref())
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    alternatives(sources[0].judgement.ty.clone(), &sources)
                })
                .collect(),
        );
        let mut keys = IndexMap::new();
        for branch in branches {
            for key in branch.fields.keys() {
                keys.insert(key.clone(), ());
            }
        }
        result.data_mut().fields = Arc::new(
            keys.into_keys()
                .map(|key| {
                    let sources = branches
                        .iter()
                        .filter_map(|branch| branch.fields.get(&key).or(branch.member.as_deref()))
                        .cloned()
                        .collect::<Vec<_>>();
                    (key, alternatives(sources[0].judgement.ty.clone(), &sources))
                })
                .collect(),
        );
        result
    }

    fn declared(ty: Result<TypeExpr, InferenceIssue>, concrete: bool) -> JudgedExpr {
        let member = match &ty {
            Ok(TypeExpr::List(inner) | TypeExpr::Map(inner)) => {
                Some(Arc::new(declared(Ok(*inner.clone()), concrete)))
            }
            _ => None,
        };
        let fields = match &ty {
            Ok(TypeExpr::Struct { fields }) => fields
                .iter()
                .map(|(key, ty)| (key.clone(), declared(Ok(ty.clone()), concrete)))
                .collect(),
            _ => IndexMap::new(),
        };
        JudgedExpr(Arc::new(JudgedExprData {
            origin: None,
            concrete,
            reported: false,
            direct_reference: false,
            boundary_owner: None,
            coalesce: None,
            alternatives: Arc::default(),
            member,
            elements: Arc::default(),
            fields: Arc::new(fields),
            judgement: Arc::new(ExprJudgement {
                invalid: matches!(ty, Err(InferenceIssue::Failed(_))),
                ty,
                kind: InferredKind::Ordinary,
                failure_owner: FailureOwner::Expression,
            }),
        }))
    }
    pub(crate) fn unjudged() -> Result<JudgedExpr, MissingEvidence> {
        Err(MissingEvidence::new("declaration has not been judged"))
    }
    fn incomplete(request: &str) -> JudgedExpr {
        let mut result = declared(
            Err(InferenceError::from(MissingEvidence::new(request)).into()),
            false,
        );
        Arc::make_mut(&mut result.data_mut().judgement).invalid = true;
        result
    }
    #[cfg(any(test, feature = "test-utils", feature = "test-support"))]
    pub(super) fn constraint_fixture() -> JudgedExpr {
        let mut result = value_dependent("constraint-check fixture");
        Arc::make_mut(&mut result.data_mut().judgement).ty = Err(InferenceIssue::NotInferable(
            NotInferableReason::NonInferableBinding {
                binding: "constraint-check fixture".into(),
            },
        ));
        result
    }
    pub(super) fn value_dependent(reason: &str) -> JudgedExpr {
        let mut result = declared(
            Err(InferenceError::UnknownType {
                reason: reason.into(),
            }
            .into()),
            false,
        );
        Arc::make_mut(&mut result.data_mut().judgement).kind = InferredKind::ValueDependent;
        Arc::make_mut(&mut result.data_mut().judgement).invalid = false;
        result
    }
    fn declaration() -> JudgedExpr {
        let mut result = value_dependent("declaration is not a value");
        Arc::make_mut(&mut result.data_mut().judgement).kind = InferredKind::Declaration;
        result.failure_owned_by(FailureOwner::Boundary);
        result
    }
}
