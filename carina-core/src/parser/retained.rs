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

/// An expression retains its complete authored root and provisional payload.
/// Neither field nor a constructor from a reduced `Value` is public.
///
/// ```compile_fail
/// use carina_core::parser::retained::ExprValue;
/// use carina_core::resource::{AuthoredValue, Value, ConcreteValue};
/// let reduced: AuthoredValue = Value::Concrete(ConcreteValue::String("selected".into()));
/// let expression: ExprValue = reduced.into();
/// ```
///
/// ```compile_fail
/// use carina_core::{parser::retained::ExprValue, resource::AuthoredValue};
/// fn discard_authorship(expression: ExprValue) -> AuthoredValue { expression.into_value() }
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
    location: DeclarationLocation,
    kind: ExprKind<B>,
    children: Vec<ExprValue<P, B>>,
}

#[derive(Debug, Clone, PartialEq)]
enum ExprKind<B: BindingAddress> {
    Scalar(TypeExpr),
    Parameter(TypeExpr),
    Binding(B),
    Reference {
        binding: B,
        attribute: String,
        segments: Vec<PathSegment>,
    },
    List,
    Map(Vec<String>),
    Interpolation,
    Coalesce,
    Call(String),
    Access(PathSegment),
    Integer {
        op: crate::integer_arithmetic::IntegerOp,
        location: DeclarationLocation,
    },
    Alias,
    Element,
    Resource {
        provider: String,
        resource_type: String,
        read: bool,
    },
    Sink(String),
    Pending,
}

impl<P: ValueKind, B: BindingAddress> ExprValue<P, B> {
    pub fn projection(&self) -> Option<&Value<P, B>> {
        match &self.payload {
            Projection::Value(value) => Some(value),
            Projection::Pending => None,
        }
    }

    pub fn location(&self) -> &DeclarationLocation {
        &self.authored.location
    }

    fn map_bindings<C: BindingAddress>(self, map: &mut impl FnMut(B) -> C) -> ExprValue<P, C> {
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
            ExprKind::List => ExprKind::List,
            ExprKind::Map(keys) => ExprKind::Map(keys),
            ExprKind::Interpolation => ExprKind::Interpolation,
            ExprKind::Coalesce => ExprKind::Coalesce,
            ExprKind::Call(name) => ExprKind::Call(name),
            ExprKind::Access(step) => ExprKind::Access(step),
            ExprKind::Integer { op, location } => ExprKind::Integer { op, location },
            ExprKind::Alias => ExprKind::Alias,
            ExprKind::Element => ExprKind::Element,
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
            ExprKind::Pending => ExprKind::Pending,
        };
        ExprValue {
            authored: Arc::new(AuthoredExpr {
                location: authored.location,
                kind,
                children: authored
                    .children
                    .into_iter()
                    .map(|child| child.map_bindings(map))
                    .collect(),
            }),
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
/// ```compile_fail
/// use carina_core::{parser::{ParsedFile, ProviderContext}, schema::SchemaRegistry,
///     module_resolver::ResolvedModuleSignatures, validation::ValidationTypeContext};
/// let raw = ParsedFile::default();
/// let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
/// let schemas = SchemaRegistry::new();
/// let signatures = ResolvedModuleSignatures::default();
/// let unchecked = ValidationTypeContext::new_providerless(&raw, &schemas, &config, &signatures);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Expressions<B: BindingAddress = RelativeBindingName> {
    roots: Vec<ExprValue<ParserPlaceholder, B>>,
    definitions: IndexMap<B, ExprValue<ParserPlaceholder, B>>,
    report: Option<Arc<ExpressionReport<B>>>,
    arguments: HashMap<B, JudgedExpr>,
}

impl<B: BindingAddress> Default for Expressions<B> {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            definitions: IndexMap::new(),
            report: None,
            arguments: HashMap::new(),
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
        self.report = match (self.report.take(), other.report) {
            (Some(left), Some(right)) => Some(Arc::new(left.merged(&right))),
            _ => None,
        };
    }

    pub(super) fn map_bindings<C: BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> Expressions<C> {
        Expressions {
            arguments: self
                .arguments
                .into_iter()
                .map(|(name, expression)| (map(name), expression))
                .collect(),
            report: self.report.map(|report| Arc::new(report.map_bindings(map))),
            roots: self
                .roots
                .into_iter()
                .map(|root| root.map_bindings(map))
                .collect(),
            definitions: self
                .definitions
                .into_iter()
                .map(|(name, expression)| (map(name), expression.map_bindings(map)))
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

    pub(crate) fn bind_arguments(&mut self, arguments: impl IntoIterator<Item = (B, JudgedExpr)>) {
        self.arguments.extend(arguments);
        self.report = None;
    }
}

/// A configuration together with the report produced at its load/expansion
/// boundary. Raw, caller-built files cannot supply this validation input.
pub struct JudgedFile<'a, E, D, B: BindingAddress = RelativeBindingName> {
    file: &'a super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: &'a ExpressionReport<B>,
}

impl<'a, E, D, B: BindingAddress> JudgedFile<'a, E, D, B> {
    pub fn file(&self) -> &'a super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
    }
    pub fn report(&self) -> &'a ExpressionReport<B> {
        self.report
    }
}

#[derive(Debug, thiserror::Error)]
#[error("configuration has not crossed the expression judgement boundary")]
pub struct UnjudgedConfiguration;

impl<E, D, B: BindingAddress> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
    pub fn judged(&self) -> Result<JudgedFile<'_, E, D, B>, UnjudgedConfiguration> {
        let report = self
            .parser_binding_metadata
            .syntax()
            .expressions()
            .report
            .as_deref()
            .ok_or(UnjudgedConfiguration)?;
        Ok(JudgedFile { file: self, report })
    }
}

/// Only producers attach judgement evidence. Downstream code borrows it using
/// `File::judged`; it never runs the judge as a validation convention.
pub(crate) fn judge_configuration<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &crate::schema::SchemaRegistry,
    config: &super::ProviderContext,
) where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let report = judge_expressions(file, schemas, config);
    file.parser_binding_metadata.syntax.expressions_mut().report = Some(Arc::new(report));
}

pub(crate) fn attach_report<E, D, B: BindingAddress>(
    file: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    report: ExpressionReport<B>,
) {
    file.parser_binding_metadata.syntax.expressions_mut().report = Some(Arc::new(report));
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
            location: DeclarationLocation::expression_node(pair, file, source),
            kind,
            children,
        }),
        payload: Projection::Pending,
    }
}

fn compile(
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
        children
    };
    match pair.as_rule() {
        Rule::expression | Rule::primary => {
            let children = children();
            if children.len() == 1 {
                node(&pair, ExprKind::Alias, children, file, source)
            } else {
                node(&pair, ExprKind::Pending, children, file, source)
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
        Rule::additive_expr | Rule::multiplicative_expr => {
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
                node(&pair, ExprKind::Pending, parts, file, source)
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
                    .unwrap_or(ExprKind::Pending);
                let mut operands = vec![value];
                operands.extend(operand);
                value = node(&pair, kind, operands, file, source);
            }
            node(&pair, ExprKind::Alias, vec![value], file, source)
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
                        .unwrap_or(ExprKind::Pending);
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
                    .unwrap_or(ExprKind::Pending);
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
                        None => node(&part, ExprKind::Pending, vec![], file, source),
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
            let keys = pair
                .clone()
                .into_inner()
                .filter_map(|entry| entry.into_inner().next())
                .map(|key| {
                    if key.as_rule() == Rule::string {
                        super::expressions::string_literal::parse_string_literal(key)
                            .unwrap_or_default()
                    } else {
                        key.as_str().to_string()
                    }
                })
                .collect();
            let children = pair
                .clone()
                .into_inner()
                .map(|entry| {
                    if entry.as_rule() == Rule::map_entry {
                        let value = entry
                            .into_inner()
                            .find(|child| child.as_rule() == Rule::expression)
                            .expect("map value");
                        compile(value, file, source, scope)
                    } else {
                        compile(entry, file, source, scope)
                    }
                })
                .collect();
            node(&pair, ExprKind::Map(keys), children, file, source)
        }
        Rule::fn_def => {
            let mut scope = scope.clone();
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
                    scope.insert(
                        name.as_str().to_string(),
                        node(&name, ExprKind::Parameter(ty), vec![], file, source),
                    );
                }
            }
            let children = pair
                .clone()
                .into_inner()
                .filter(|part| part.as_rule() == Rule::fn_body)
                .map(|body| compile(body, file, source, &scope))
                .collect();
            node(&pair, ExprKind::Pending, children, file, source)
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
                    (Rule::for_indexed_binding, 0) => ExprKind::Scalar(TypeExpr::Int),
                    (Rule::for_map_binding, 0) => ExprKind::Scalar(TypeExpr::String),
                    _ => ExprKind::Element,
                };
                scope.insert(
                    name.as_str().to_string(),
                    node(name, kind, vec![iterable.clone()], file, source),
                );
            }
            let body = compile(parts.next().expect("loop body"), file, source, &scope);
            node(&pair, ExprKind::Pending, vec![iterable, body], file, source)
        }
        Rule::function_call => {
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
                children(),
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
        _ => node(&pair, ExprKind::Pending, children(), file, source),
    }
}

fn access_step(pair: Pair<'_, Rule>) -> Option<PathSegment> {
    match pair.as_rule() {
        Rule::field_access => Some(PathSegment::Field {
            name: pair.as_str().trim_start_matches('.').to_string(),
        }),
        Rule::index_access => {
            let index = pair
                .into_inner()
                .flatten()
                .find(|part| matches!(part.as_rule(), Rule::number | Rule::string))?;
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

#[cfg(not(test))]
use judgement::judge_expressions;
#[cfg(test)]
pub(crate) use judgement::judge_expressions;
pub use judgement::{
    CollectionKind, ExpressionAdmission, ExpressionReport, JudgedExpr, admit_expression,
};
pub(crate) use judgement::{EmptyCoalesceDefault, coalesce_type};

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
                left: left.to_string(),
                default: right.to_string(),
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
        StringConstruction,
        List,
        Map,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct ExprJudgement {
        ty: Result<TypeExpr, InferenceIssue>,
        kind: InferredKind,
        concrete: bool,
    }

    /// Admission evidence, constructible only by the recursive judge.
    ///
    /// ```compile_fail
    /// use carina_core::parser::retained::JudgedExpr;
    /// let forged = JudgedExpr { judgement: todo!() };
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExpr {
        judgement: ExprJudgement,
        coalesce: Option<Box<[JudgedExpr; 2]>>,
        elements: Vec<JudgedExpr>,
        fields: IndexMap<String, JudgedExpr>,
    }

    impl JudgedExpr {
        pub fn static_type(&self) -> Result<&TypeExpr, &InferenceIssue> {
            self.judgement.ty.as_ref()
        }
        pub fn coalesce_operands(&self) -> Option<&[JudgedExpr; 2]> {
            self.coalesce.as_deref()
        }
        pub fn element(&self, index: usize) -> Option<&JudgedExpr> {
            self.elements.get(index)
        }
        pub fn field(&self, key: &str) -> Option<&JudgedExpr> {
            self.fields.get(key)
        }
        pub fn collection_kind(&self) -> Option<CollectionKind> {
            match self.judgement.kind {
                InferredKind::List => Some(CollectionKind::List),
                InferredKind::Map => Some(CollectionKind::Map),
                _ => None,
            }
        }
        pub(crate) fn after_substitution<P: crate::resource::PlaceholderKind, B: BindingAddress>(
            &self,
            value: &Value<P, B>,
        ) -> Self {
            let mut expression = self.clone();
            expression.judgement.concrete = value.is_fully_resolved();
            expression
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CollectionKind {
        List,
        Map,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExpressionAdmission {
        ValueCheck,
        DeferredInterpolation,
        Pending,
    }

    /// Static admission requires the judge's evidence, never a projection.
    ///
    /// ```compile_fail
    /// use carina_core::{parser::{TypeExpr, retained::admit_expression}, resource::{Value, ConcreteValue}};
    /// let reduced = Value::<carina_core::resource::ParserPlaceholder, carina_core::resource::RelativeBindingName>::Concrete(ConcreteValue::String("selected".into()));
    /// admit_expression(&reduced, &TypeExpr::String);
    /// ```
    pub fn admit_expression(
        expression: &JudgedExpr,
        sink: &TypeExpr,
    ) -> Result<ExpressionAdmission, InferenceError> {
        if let Err(error) = expression.static_type() {
            return match error {
                InferenceIssue::Failed(error) => Err(error.clone()),
                InferenceIssue::NotInferable(_) => Ok(ExpressionAdmission::Pending),
            };
        }
        if expression.judgement.kind == InferredKind::StringConstruction
            && !expression.judgement.concrete
        {
            return if admit_deferred_interpolation(expression, sink) {
                Ok(ExpressionAdmission::DeferredInterpolation)
            } else {
                Err(InferenceError::UnknownType {
                    reason: format!(
                        "cannot assign deferred interpolation: expected {sink}, got String"
                    ),
                })
            };
        }
        Ok(ExpressionAdmission::ValueCheck)
    }

    /// The sole policy switch for deferred interpolation. Placeholder (b)
    /// admits string construction; resolved values still undergo constraints.
    fn admit_deferred_interpolation(expression: &JudgedExpr, sink: &TypeExpr) -> bool {
        const ADMIT_NAMED_STRING: bool = true;
        expression.static_type().is_ok()
            && expression.judgement.kind == InferredKind::StringConstruction
            && !expression.judgement.concrete
            && (matches!(sink, TypeExpr::String)
                || (ADMIT_NAMED_STRING
                    && (sink.is_string_shaped() || matches!(sink, TypeExpr::Ref(_))))
                || matches!(sink, TypeExpr::Union(members) if members.iter().any(|member| admit_deferred_interpolation(expression, member))))
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct ExpressionReport<B: BindingAddress = RelativeBindingName> {
        bindings: HashMap<B, JudgedExpr>,
        expressions: HashMap<DeclarationLocation, JudgedExpr>,
        findings: Vec<Located<InferenceError>>,
        operands: HashMap<DeclarationLocation, [JudgedExpr; 2]>,
        unknown: JudgedExpr,
        module_attributes: HashMap<(B, String), JudgedExpr>,
        resource_reports: HashMap<crate::resource::ResourceId, Arc<ExpressionReport<B>>>,
        parameters: HashMap<String, JudgedExpr>,
    }

    impl<B: BindingAddress> ExpressionReport<B> {
        pub(super) fn merged(&self, other: &Self) -> Self {
            let mut report = self.clone();
            report.bindings.extend(other.bindings.clone());
            report.expressions.extend(other.expressions.clone());
            report.findings.extend(other.findings.clone());
            report.operands.extend(other.operands.clone());
            report
                .module_attributes
                .extend(other.module_attributes.clone());
            report
                .resource_reports
                .extend(other.resource_reports.clone());
            report.parameters.extend(other.parameters.clone());
            report
        }
        pub(super) fn map_bindings<C: BindingAddress>(
            &self,
            map: &mut impl FnMut(B) -> C,
        ) -> ExpressionReport<C> {
            ExpressionReport {
                bindings: self
                    .bindings
                    .iter()
                    .map(|(name, expression)| (map(name.clone()), expression.clone()))
                    .collect(),
                expressions: self.expressions.clone(),
                findings: self.findings.clone(),
                operands: self.operands.clone(),
                unknown: self.unknown.clone(),
                module_attributes: self
                    .module_attributes
                    .iter()
                    .map(|((binding, attribute), expression)| {
                        (
                            (map(binding.clone()), attribute.clone()),
                            expression.clone(),
                        )
                    })
                    .collect(),
                resource_reports: self
                    .resource_reports
                    .iter()
                    .map(|(id, report)| (id.clone(), Arc::new(report.map_bindings(map))))
                    .collect(),
                parameters: self.parameters.clone(),
            }
        }
        pub fn binding(&self, name: &B) -> Option<&JudgedExpr> {
            self.bindings.get(name)
        }
        pub fn expression(&self, location: &DeclarationLocation) -> Option<&JudgedExpr> {
            self.expressions.get(location)
        }
        pub fn unknown(&self) -> &JudgedExpr {
            &self.unknown
        }
        pub fn for_resource(&self, id: &crate::resource::ResourceId) -> &Self {
            self.resource_reports
                .get(id)
                .map(Arc::as_ref)
                .unwrap_or(self)
        }
        pub fn module_attribute(&self, binding: &B, name: &str) -> Option<&JudgedExpr> {
            self.module_attributes
                .get(&(binding.clone(), name.to_string()))
        }
        pub(crate) fn locations(&self) -> HashMap<DeclarationLocation, JudgedExpr> {
            self.expressions.clone()
        }
        pub(crate) fn parameter(&self, name: &str) -> Option<&JudgedExpr> {
            self.parameters.get(name)
        }
        pub(crate) fn record_instance<E, D>(
            &mut self,
            file: &super::super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
            binding: Option<&B>,
        ) {
            if let Some(binding) = binding {
                for declaration in file.parser_binding_metadata.syntax().declarations() {
                    if declaration.detail().kind()
                        == crate::config_loader::DeclarationKind::ModuleAttribute
                        && let Some(expression) = declaration
                            .detail()
                            .value_location()
                            .and_then(|location| self.expression(location))
                    {
                        self.module_attributes.insert(
                            (binding.clone(), declaration.detail().name().to_string()),
                            expression.clone(),
                        );
                    }
                }
            }
            let snapshot = Arc::new(self.clone());
            for id in file
                .resources
                .iter()
                .map(|resource| &resource.id)
                .chain(file.data_sources.iter().map(|source| &source.id))
            {
                self.resource_reports.insert(id.clone(), snapshot.clone());
            }
        }
        pub fn operand(
            &self,
            location: &DeclarationLocation,
            side: crate::integer_arithmetic::IntegerOperandSide,
        ) -> &JudgedExpr {
            let index = match side {
                crate::integer_arithmetic::IntegerOperandSide::Left => 0,
                crate::integer_arithmetic::IntegerOperandSide::Right => 1,
            };
            self.operands
                .get(location)
                .map(|operands| &operands[index])
                .unwrap_or(&self.unknown)
        }
        pub fn access<P: ValueKind>(
            &self,
            access: &crate::resource::AccessExpr<P, B>,
        ) -> &JudgedExpr {
            access
                .access_locations()
                .and_then(|location| self.expression(location.expression_location()))
                .unwrap_or(&self.unknown)
        }
        pub fn findings(&self) -> &[Located<InferenceError>] {
            &self.findings
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
        let bindings = inference::bindings_from_parsed(file);
        let mut judge = Judge {
            expressions,
            bindings: &bindings,
            schemas,
            config,
            memo: HashMap::new(),
            by_location: HashMap::new(),
            visiting: Vec::new(),
            findings: Vec::new(),
            operands: HashMap::new(),
            module_attributes: expressions
                .report
                .as_deref()
                .map(|report| &report.module_attributes),
        };
        let mut judged_bindings = HashMap::new();
        for (name, expression) in &expressions.definitions {
            judged_bindings.insert(name.clone(), judge.judge(expression));
        }
        for expression in &expressions.roots {
            judge.judge(expression);
        }
        ExpressionReport {
            bindings: judged_bindings,
            expressions: judge.by_location,
            findings: judge.findings,
            operands: judge.operands,
            unknown: pending(),
            module_attributes: expressions
                .report
                .as_deref()
                .map(|report| report.module_attributes.clone())
                .unwrap_or_default(),
            resource_reports: expressions
                .report
                .as_deref()
                .map(|report| report.resource_reports.clone())
                .unwrap_or_default(),
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (
                        argument.name.clone(),
                        JudgedExpr {
                            judgement: ExprJudgement {
                                ty: Ok(argument.static_type.clone()),
                                kind: InferredKind::Ordinary,
                                concrete: false,
                            },
                            coalesce: None,
                            elements: Vec::new(),
                            fields: IndexMap::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    struct Judge<'a, B: BindingAddress> {
        expressions: &'a Expressions<B>,
        bindings: &'a InferenceBindings<ParserPlaceholder, B>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &'a super::super::ProviderContext,
        memo: HashMap<usize, JudgedExpr>,
        by_location: HashMap<DeclarationLocation, JudgedExpr>,
        visiting: Vec<usize>,
        findings: Vec<Located<InferenceError>>,
        operands: HashMap<DeclarationLocation, [JudgedExpr; 2]>,
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
                return pending();
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
            if let Projection::Value(value) = &expression.payload {
                result.judgement.concrete = value.is_fully_resolved();
            }
            if let Some(failed) = children.iter().find(|child| {
                matches!(
                    child.judgement.ty,
                    Err(InferenceIssue::Failed(
                        InferenceError::IncompatibleCoalesceTypes { .. }
                    ))
                )
            }) {
                result.judgement.ty = failed.judgement.ty.clone();
            }
            self.visiting.pop();
            self.memo.insert(id, result.clone());
            self.by_location
                .insert(root.location.clone(), result.clone());
            result
        }

        fn infer(
            &mut self,
            expression: &ExprValue<ParserPlaceholder, B>,
            children: &[JudgedExpr],
        ) -> JudgedExpr {
            let root = &expression.authored;
            let ordinary = |ty, concrete| JudgedExpr {
                coalesce: None,
                elements: Vec::new(),
                fields: IndexMap::new(),
                judgement: ExprJudgement {
                    ty,
                    concrete,
                    kind: InferredKind::Ordinary,
                },
            };
            let child_type = |index: usize| {
                children.get(index).map_or_else(
                    || pending().judgement.ty,
                    |child| child.judgement.ty.clone(),
                )
            };
            match &root.kind {
                ExprKind::Scalar(ty) => ordinary(Ok(ty.clone()), true),
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false),
                ExprKind::Binding(name) => {
                    if let Some(argument) = self.expressions.arguments.get(name) {
                        argument.clone()
                    } else if let Some(definition) = self.expressions.definitions.get(name) {
                        self.judge(definition)
                    } else if let Some(InferenceBinding::Argument { type_expr }) =
                        self.bindings.get(name.as_str())
                    {
                        ordinary(Ok(type_expr.clone()), false)
                    } else {
                        pending()
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
                    if let Some(argument) = self.expressions.arguments.get(binding) {
                        return ordinary(
                            argument
                                .static_type()
                                .cloned()
                                .map_err(Clone::clone)
                                .and_then(|ty| {
                                    crate::validation::narrow_type_expr(
                                        &ty,
                                        &steps,
                                        Some(self.schemas),
                                    )
                                    .ok_or(
                                        InferenceIssue::NotInferable(
                                            NotInferableReason::MissingValue,
                                        ),
                                    )
                                }),
                            argument.judgement.concrete,
                        );
                    }
                    if let Some(attribute) = self.module_attributes.and_then(|attributes| {
                        attributes.get(&(binding.clone(), attribute.clone()))
                    }) {
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
                            attribute.judgement.concrete,
                        );
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
                    if let Some(InferenceBinding::Argument { type_expr }) =
                        self.bindings.get(binding.as_str())
                    {
                        ordinary(
                            crate::validation::narrow_type_expr(
                                type_expr,
                                &steps,
                                Some(self.schemas),
                            )
                            .ok_or(InferenceIssue::NotInferable(
                                NotInferableReason::MissingValue,
                            )),
                            false,
                        )
                    } else if self.bindings.contains_key(binding.as_str()) {
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
                            base.judgement.ty.and_then(|ty| {
                                let mut current = ty;
                                for step in &steps {
                                    current = crate::validation::narrow_type_expr(
                                        &current,
                                        std::slice::from_ref(step),
                                        Some(self.schemas),
                                    )
                                    .ok_or_else(|| {
                                        InferenceIssue::Failed(InferenceError::InvalidAccess {
                                            base_type: current.to_string(),
                                            step: step.clone(),
                                        })
                                    })?;
                                }
                                Ok(current)
                            }),
                            base.judgement.concrete,
                        )
                    } else {
                        ordinary(Ok(TypeExpr::String), true)
                    }
                }
                ExprKind::Interpolation => JudgedExpr {
                    coalesce: None,
                    elements: Vec::new(),
                    fields: IndexMap::new(),
                    judgement: ExprJudgement {
                        ty: Ok(TypeExpr::String),
                        kind: InferredKind::StringConstruction,
                        concrete: children.iter().all(|child| child.judgement.concrete),
                    },
                },
                ExprKind::Coalesce => {
                    let left = child_type(0);
                    let right = child_type(1);
                    let empty = if root.children[1].authored.children.is_empty() {
                        match root.children[1].authored.kind {
                            ExprKind::List => EmptyCoalesceDefault::List,
                            ExprKind::Map(_) => EmptyCoalesceDefault::Map,
                            _ => EmptyCoalesceDefault::None,
                        }
                    } else {
                        EmptyCoalesceDefault::None
                    };
                    let owns_mismatch =
                        matches!((&left, &right), (Ok(left), Ok(right)) if left != right);
                    let ty = coalesce_type(left, right, empty);
                    if owns_mismatch && let Err(InferenceIssue::Failed(error)) = &ty {
                        let location = root.children[1].location().clone();
                        if !self.findings.iter().any(|finding| {
                            finding.location() == &location && finding.detail() == error
                        }) {
                            self.findings.push(Located::new(location, error.clone()));
                        }
                    }
                    let mut result = children.first().cloned().unwrap_or_else(pending);
                    result.judgement.ty = ty;
                    result.coalesce = Some(Box::new([children[0].clone(), children[1].clone()]));
                    result
                }
                ExprKind::List | ExprKind::Map(_) => {
                    let ty = match children.first() {
                        Some(first) => first.judgement.ty.clone().and_then(|first| {
                            for child in &children[1..] {
                                let next = child.static_type().map_err(Clone::clone)?;
                                if *next != first {
                                    return Err(InferenceError::HeterogeneousCollection {
                                        reason: format!("collection element types disagree: `{first}` vs `{next}`"),
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
                        None => Err(InferenceError::UnknownType { reason: "empty collection has no inferable element type".to_string() }.into()),
                    };
                    let mut result =
                        ordinary(ty, children.iter().all(|child| child.judgement.concrete));
                    result.judgement.kind = if matches!(root.kind, ExprKind::List) {
                        InferredKind::List
                    } else {
                        InferredKind::Map
                    };
                    match &root.kind {
                        ExprKind::List => result.elements = children.to_vec(),
                        ExprKind::Map(keys) => {
                            result.fields =
                                keys.iter().cloned().zip(children.iter().cloned()).collect()
                        }
                        _ => unreachable!("collection judgement"),
                    }
                    result
                }
                ExprKind::Call(name) if name == "secret" => {
                    children.first().cloned().unwrap_or_else(pending)
                }
                ExprKind::Call(name) => {
                    if let Some(InferenceBinding::UserFunction {
                        return_type: Some(ty),
                    }) = self.bindings.get(name)
                    {
                        return ordinary(Ok(ty.clone()), false);
                    }
                    let ty = crate::builtins::builtin_functions()
                        .iter()
                        .find(|builtin| builtin.name == name)
                        .and_then(|builtin| match builtin.return_type {
                            crate::builtins::BuiltinReturnType::String => Some(TypeExpr::String),
                            crate::builtins::BuiltinReturnType::Int => Some(TypeExpr::Int),
                            crate::builtins::BuiltinReturnType::StringList => {
                                Some(TypeExpr::List(Box::new(TypeExpr::String)))
                            }
                            crate::builtins::BuiltinReturnType::Secret
                            | crate::builtins::BuiltinReturnType::List
                            | crate::builtins::BuiltinReturnType::Map
                            | crate::builtins::BuiltinReturnType::Any => None,
                        });
                    ordinary(
                        ty.ok_or_else(|| {
                            InferenceIssue::Failed(InferenceError::UnknownType {
                                reason: format!(
                                    "built-in `{name}` return type depends on arguments"
                                ),
                            })
                        }),
                        children.iter().all(|child| child.judgement.concrete),
                    )
                }
                ExprKind::Access(step) => ordinary(
                    child_type(0).and_then(|base| {
                        crate::validation::narrow_type_expr(
                            &base,
                            std::slice::from_ref(step),
                            Some(self.schemas),
                        )
                        .ok_or_else(|| {
                            InferenceError::InvalidAccess {
                                base_type: base.to_string(),
                                step: step.clone(),
                            }
                            .into()
                        })
                    }),
                    children
                        .first()
                        .is_some_and(|child| child.judgement.concrete),
                ),
                ExprKind::Integer { op, location } => {
                    let operands = [children[0].clone(), children[1].clone()];
                    self.operands.insert(location.clone(), operands.clone());
                    ordinary(
                        inference::judge_integer_operands(
                            *op,
                            location,
                            &operands,
                            self.schemas,
                            self.config,
                        ),
                        children.iter().all(|child| child.judgement.concrete),
                    )
                }
                ExprKind::Alias => children.first().cloned().unwrap_or_else(pending),
                ExprKind::Element => ordinary(
                    child_type(0).and_then(|ty| match ty {
                        TypeExpr::List(inner) | TypeExpr::Map(inner) => Ok(*inner),
                        _ => pending().judgement.ty,
                    }),
                    false,
                ),
                ExprKind::Sink(_) => children.first().cloned().unwrap_or_else(pending),
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
                    pending()
                }
                ExprKind::Pending => pending(),
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
                                    if judged.judgement.kind
                                        == InferredKind::StringConstruction
                                        && !judged.judgement.concrete
                                        && !matches!(
                                            error,
                                            InferenceError::IncompatibleCoalesceTypes { .. }
                                        ) =>
                                {
                                    self.findings
                                        .push(Located::new(value.location().clone(), error));
                                }
                                Ok(
                                    ExpressionAdmission::ValueCheck
                                    | ExpressionAdmission::Pending
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

    fn pending() -> JudgedExpr {
        JudgedExpr {
            coalesce: None,
            elements: Vec::new(),
            fields: IndexMap::new(),
            judgement: ExprJudgement {
                ty: Err(InferenceIssue::NotInferable(
                    NotInferableReason::MissingValue,
                )),
                kind: InferredKind::Ordinary,
                concrete: false,
            },
        }
    }
}
