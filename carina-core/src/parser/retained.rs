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
/// ```compile_fail,E0599
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
    location: ExpressionLocation,
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
    Function {
        name: String,
        return_type: Option<TypeExpr>,
    },
    Sequence,
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
            ExprKind::List => ExprKind::List,
            ExprKind::Map(keys) => ExprKind::Map(keys),
            ExprKind::Interpolation => ExprKind::Interpolation,
            ExprKind::Coalesce => ExprKind::Coalesce,
            ExprKind::Call(name) => ExprKind::Call(name),
            ExprKind::Function { name, return_type } => ExprKind::Function { name, return_type },
            ExprKind::Sequence => ExprKind::Sequence,
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
    arguments: HashMap<B, JudgedExpr>,
    instance: ExpressionInstance,
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
        arguments: impl IntoIterator<Item = (B, JudgedExpr)>,
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
        self.arguments.extend(arguments);
        self.report = Arc::new(ExpressionReport::empty());
    }
}

/// A schema-judged configuration owns its file, report, and schema registry.
/// Parsing alone cannot construct this type. Consumers borrow this bundle so
/// neither the file nor the schemas can be replaced independently.
#[derive(Debug, Clone)]
pub struct JudgedFile<E, D, B: BindingAddress = RelativeBindingName> {
    file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: Arc<crate::schema::SchemaRegistry>,
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
        let schemas = Arc::new(schemas.clone());
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
        Rule::fn_body => node(&pair, ExprKind::Sequence, children(), file, source),
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
                    return_type,
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
pub(in crate::parser) use judgement::pending as missing_judgement;
pub use judgement::{
    CollectionKind, ExpressionAdmission, ExpressionReport, JudgedExpr, admit_expression,
};
pub(crate) use judgement::{EmptyCoalesceDefault, coalesce_type};
#[cfg(test)]
pub(crate) fn missing_judgement_for_test() -> JudgedExpr {
    judgement::pending()
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
        // A missing inferable type (dynamic calls, heterogeneous records) is
        // distinct from an invalid authored operation such as a bad fallback.
        invalid: bool,
    }

    /// Admission evidence, constructible only by the recursive judge.
    ///
    /// ```compile_fail,E0451
    /// use carina_core::parser::retained::JudgedExpr;
    /// let forged = JudgedExpr { origin: None, judgement: todo!(), concrete: false, coalesce: None, elements: Default::default(), fields: Default::default() };
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExpr {
        origin: Option<ExpressionLocation>,
        pub(crate) concrete: bool,
        judgement: Arc<ExprJudgement>,
        coalesce: Option<Arc<[JudgedExpr; 2]>>,
        elements: Arc<Vec<JudgedExpr>>,
        fields: Arc<IndexMap<String, JudgedExpr>>,
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
    /// ```compile_fail,E0308
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
                // The sink can check a dynamic result or each field of a
                // record even when no homogeneous type can be inferred.
                InferenceIssue::Failed(_) if !expression.judgement.invalid => {
                    Ok(ExpressionAdmission::ValueCheck)
                }
                InferenceIssue::Failed(error) => Err(error.clone()),
                InferenceIssue::NotInferable(_) => Ok(ExpressionAdmission::Pending),
            };
        }
        if expression.judgement.kind == InferredKind::StringConstruction && !expression.concrete {
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

    /// Deferred interpolation may fill String and named string sinks. Once
    /// it resolves, the resulting value must satisfy the sink's constraints.
    fn admit_deferred_interpolation(expression: &JudgedExpr, sink: &TypeExpr) -> bool {
        expression.static_type().is_ok()
            && expression.judgement.kind == InferredKind::StringConstruction
            && !expression.concrete
            && (matches!(sink, TypeExpr::String)
                || sink.is_string_shaped()
                || matches!(sink, TypeExpr::Ref(_))
                || matches!(sink, TypeExpr::Union(members) if members.iter().any(|member| admit_deferred_interpolation(expression, member))))
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct ExpressionReport<B: BindingAddress = RelativeBindingName> {
        bindings: Arc<HashMap<B, JudgedExpr>>,
        expressions: Arc<HashMap<ExpressionLocation, JudgedExpr>>,
        findings: Arc<Vec<Located<InferenceError>>>,
        function_errors: Arc<Vec<Located<InferenceError>>>,
        operands: Arc<HashMap<ExpressionLocation, [JudgedExpr; 2]>>,
        unknown: JudgedExpr,
        module_attributes: Arc<HashMap<(B, String), JudgedExpr>>,
        resource_reports: Arc<HashMap<crate::resource::ResourceId, Arc<ExpressionReport<B>>>>,
        parameters: Arc<HashMap<String, JudgedExpr>>,
        instance: ExpressionInstance,
        authored: bool,
    }

    impl<B: BindingAddress> ExpressionReport<B> {
        pub(super) fn empty() -> Self {
            Self {
                bindings: Arc::default(),
                expressions: Arc::default(),
                findings: Arc::default(),
                function_errors: Arc::default(),
                operands: Arc::default(),
                unknown: pending(),
                module_attributes: Arc::default(),
                resource_reports: Arc::default(),
                parameters: Arc::default(),
                instance: ExpressionInstance::Root,
                authored: false,
            }
        }
        pub(super) fn extend(&mut self, other: Self) {
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
            Arc::make_mut(&mut self.parameters).extend(Arc::unwrap_or_clone(other.parameters));
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
                    .collect::<HashMap<_, _>>()
                    .into(),
                resource_reports,
                parameters: self.parameters.clone(),
            }
        }
        pub(super) fn admit_sink(&mut self, location: &DeclarationLocation, sink: &TypeExpr) {
            if let Some(expression) = self.expression(location)
                && expression.static_type().is_ok()
                && let Err(error) = admit_expression(expression, sink)
                && !self
                    .findings
                    .iter()
                    .any(|finding| finding.location() == location && finding.detail() == &error)
            {
                Arc::make_mut(&mut self.findings).push(Located::new(location.clone(), error));
            }
        }
        pub(crate) fn has_authored_judgement(&self) -> bool {
            self.authored
        }
        pub fn binding(&self, name: &B) -> Option<&JudgedExpr> {
            self.bindings.get(name)
        }
        #[cfg(test)]
        pub(crate) fn types_at_for_test(&self, location: &DeclarationLocation) -> Vec<TypeExpr> {
            self.expressions
                .iter()
                .filter(|(key, _)| &key.authored == location)
                .filter_map(|(_, expr)| expr.static_type().ok().cloned())
                .collect()
        }
        pub fn expression(&self, location: &DeclarationLocation) -> Option<&JudgedExpr> {
            self.expression_in(&self.instance, location)
        }
        pub(crate) fn expression_in(
            &self,
            instance: &ExpressionInstance,
            location: &DeclarationLocation,
        ) -> Option<&JudgedExpr> {
            self.expressions
                .get(&ExpressionLocation::new(instance.clone(), location))
        }
        pub(crate) fn require_expression_in(
            &self,
            instance: &ExpressionInstance,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, InferenceError> {
            self.expression_in(instance, location)
                .ok_or_else(|| InferenceError::UnknownType {
                    reason: "authored expression has no judgement".to_string(),
                })
        }
        pub(crate) fn require_expression(
            &self,
            location: &DeclarationLocation,
        ) -> Result<&JudgedExpr, InferenceError> {
            self.require_expression_in(&self.instance, location)
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
            self.expressions
                .iter()
                .filter(|(key, _)| key.instance == self.instance)
                .map(|(key, value)| (key.authored.clone(), value.clone()))
                .collect()
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
                            .cloned()
                    {
                        Arc::make_mut(&mut self.module_attributes).insert(
                            (binding.clone(), declaration.detail().name().to_string()),
                            expression.clone(),
                        );
                    }
                }
            }
            let mut snapshot = self.clone();
            snapshot.resource_reports = Arc::default();
            let snapshot = Arc::new(snapshot);
            for id in file
                .resources
                .iter()
                .map(|resource| &resource.id)
                .chain(file.data_sources.iter().map(|source| &source.id))
            {
                Arc::make_mut(&mut self.resource_reports).insert(id.clone(), snapshot.clone());
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
                .get(&ExpressionLocation::new(self.instance.clone(), location))
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
        // Direct resource references already have an existence/access
        // diagnostic owned by the resource check. Retain their failed type
        // evidence without also issuing an expression-level diagnostic.
        fn reference_chain<B: BindingAddress>(
            expression: &ExprValue<ParserPlaceholder, B>,
            owned: &mut std::collections::HashSet<ExpressionLocation>,
        ) -> bool {
            let is_reference = match &expression.authored.kind {
                ExprKind::Reference { .. } => true,
                ExprKind::Alias | ExprKind::Access(_) => expression
                    .authored
                    .children
                    .first()
                    .is_some_and(|base| reference_chain(base, owned)),
                _ => false,
            };
            if is_reference {
                owned.insert(expression.authored.location.clone());
            }
            is_reference
        }
        let mut resource_references = std::collections::HashSet::new();
        for expression in authored.values() {
            if let ExprKind::Sink(_) = &expression.authored.kind
                && let Some(value) = expression.authored.children.first()
            {
                reference_chain(value, &mut resource_references);
            }
        }
        let mut judge = Judge {
            authored,
            expressions,
            bindings: &bindings,
            functions: &file.user_functions,
            schemas,
            config,
            memo: HashMap::new(),
            by_location: HashMap::new(),
            visiting: Vec::new(),
            findings: Vec::new(),
            function_errors: Vec::new(),
            operands: HashMap::new(),
            module_attributes: Some(&expressions.report.module_attributes),
            resource_references,
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
                let refreshed = evidence
                    .origin
                    .as_ref()
                    .and_then(|origin| judge.by_location.get(origin))
                    .unwrap_or(evidence)
                    .clone();
                (key.clone(), refreshed)
            })
            .collect();
        let mut report = ExpressionReport {
            instance: expressions.instance.clone(),
            authored: true,
            bindings: Arc::new(judged_bindings),
            expressions: Arc::new(judge.by_location),
            findings: Arc::new(judge.findings),
            function_errors: Arc::new(judge.function_errors),
            operands: Arc::new(judge.operands),
            unknown: pending(),
            module_attributes: Arc::new(module_attributes),
            resource_reports: Arc::default(),
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (
                        argument.name.clone(),
                        JudgedExpr {
                            origin: None,
                            judgement: Arc::new(ExprJudgement {
                                ty: Ok(argument.static_type.clone()),
                                kind: InferredKind::Ordinary,
                                invalid: false,
                            }),
                            concrete: false,
                            coalesce: None,
                            elements: Arc::default(),
                            fields: Arc::default(),
                        },
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
        report
    }

    struct Judge<'a, B: BindingAddress> {
        authored: HashMap<ExpressionLocation, &'a ExprValue<ParserPlaceholder, B>>,
        expressions: &'a Expressions<B>,
        bindings: &'a InferenceBindings<ParserPlaceholder, B>,
        functions: &'a HashMap<String, super::super::UserFunction<B>>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &'a super::super::ProviderContext,
        memo: HashMap<usize, JudgedExpr>,
        by_location: HashMap<ExpressionLocation, JudgedExpr>,
        visiting: Vec<usize>,
        findings: Vec<Located<InferenceError>>,
        function_errors: Vec<Located<InferenceError>>,
        operands: HashMap<ExpressionLocation, [JudgedExpr; 2]>,
        module_attributes: Option<&'a HashMap<(B, String), JudgedExpr>>,
        resource_references: std::collections::HashSet<ExpressionLocation>,
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
            result.origin = Some(root.location.clone());
            if let Projection::Value(value) = &expression.payload {
                result.concrete = value.is_fully_resolved();
            }
            if let Some(failed) = children.iter().find(|child| child.judgement.invalid) {
                Arc::make_mut(&mut result.judgement).ty = failed.judgement.ty.clone();
                Arc::make_mut(&mut result.judgement).invalid = true;
            } else if let Err(InferenceIssue::Failed(error)) = result.static_type() {
                Arc::make_mut(&mut result.judgement).invalid = match error {
                    InferenceError::UnknownType { .. } => false,
                    InferenceError::HeterogeneousCollection { .. } => {
                        matches!(root.kind, ExprKind::Coalesce)
                    }
                    _ => true,
                };
            }
            if result.judgement.invalid
                && let Err(InferenceIssue::Failed(error)) = result.static_type()
                && !self.resource_references.contains(&root.location)
                && !children.iter().any(|child| {
                    child.judgement.invalid
                        && child.static_type() == Err(&InferenceIssue::Failed(error.clone()))
                })
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
            }
            self.visiting.pop();
            self.memo.insert(id, result.clone());
            self.by_location
                .insert(root.location.clone(), result.clone());
            result
        }

        fn refresh_argument(&mut self, evidence: &JudgedExpr) -> JudgedExpr {
            let Some(authored) = evidence
                .origin
                .as_ref()
                .and_then(|origin| self.authored.get(origin))
                .copied()
            else {
                return evidence.clone();
            };
            let mut result = self.judge(authored);
            result.concrete = evidence.concrete;
            result
        }
        fn check_function_sink(
            &mut self,
            expression: &JudgedExpr,
            sink: &TypeExpr,
            location: &DeclarationLocation,
            context: &str,
        ) {
            if expression.static_type().is_ok()
                && let Err(error) = crate::validation::typecheck::admit_function_expression(
                    expression,
                    sink,
                    self.config,
                )
            {
                let finding = Located::new(
                    location.clone(),
                    InferenceError::UnknownType {
                        reason: format!("{context}: {error}"),
                    },
                );
                // Bootstrap parsing rejects the interpolation policy breach.
                // Declared-type mismatches belong to configuration validation,
                // after declarations and sibling files have been resolved.
                if expression.judgement.kind == InferredKind::StringConstruction
                    && !expression.concrete
                    && !self.function_errors.contains(&finding)
                {
                    self.function_errors.push(finding.clone());
                }
                self.push_finding(finding.location().clone(), finding.detail().clone());
            }
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

        fn infer(
            &mut self,
            expression: &ExprValue<ParserPlaceholder, B>,
            children: &[JudgedExpr],
        ) -> JudgedExpr {
            let root = &expression.authored;
            let ordinary = |ty, concrete| JudgedExpr {
                origin: None,
                concrete,
                coalesce: None,
                elements: Arc::default(),
                fields: Arc::default(),
                judgement: Arc::new(ExprJudgement {
                    ty,
                    kind: InferredKind::Ordinary,
                    invalid: false,
                }),
            };
            let child_type = |index: usize| {
                children.get(index).map_or_else(
                    || pending().judgement.ty.clone(),
                    |child| child.judgement.ty.clone(),
                )
            };
            match &root.kind {
                ExprKind::Scalar(ty) => ordinary(Ok(ty.clone()), true),
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false),
                ExprKind::Binding(name) => {
                    if let Some(argument) = self.expressions.arguments.get(name) {
                        self.refresh_argument(argument)
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
                        let argument = self.refresh_argument(argument);
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
                            argument.concrete,
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
                            base.judgement.ty.clone().and_then(|ty| {
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
                            base.concrete,
                        )
                    } else {
                        ordinary(Ok(TypeExpr::String), true)
                    }
                }
                ExprKind::Interpolation => JudgedExpr {
                    origin: None,
                    concrete: children.iter().all(|child| child.concrete),
                    coalesce: None,
                    elements: Arc::default(),
                    fields: Arc::default(),
                    judgement: Arc::new(ExprJudgement {
                        ty: Ok(TypeExpr::String),
                        kind: InferredKind::StringConstruction,
                        invalid: false,
                    }),
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
                    let ty = coalesce_type(left, right, empty);
                    let mut result = children.first().cloned().unwrap_or_else(pending);
                    Arc::make_mut(&mut result.judgement).ty = ty;
                    result.coalesce = Some(Arc::new([children[0].clone(), children[1].clone()]));
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
                        None => Err(InferenceIssue::NotInferable(NotInferableReason::MissingValue)),
                    };
                    let mut result = ordinary(ty, children.iter().all(|child| child.concrete));
                    Arc::make_mut(&mut result.judgement).kind =
                        if matches!(root.kind, ExprKind::List) {
                            InferredKind::List
                        } else {
                            InferredKind::Map
                        };
                    match &root.kind {
                        ExprKind::List => result.elements = Arc::new(children.to_vec()),
                        ExprKind::Map(keys) => {
                            result.fields = Arc::new(
                                keys.iter().cloned().zip(children.iter().cloned()).collect(),
                            )
                        }
                        _ => unreachable!("collection judgement"),
                    }
                    result
                }
                ExprKind::Call(name) if name == "secret" => {
                    children.first().cloned().unwrap_or_else(pending)
                }
                ExprKind::Function { name, return_type } => {
                    let body = children.last().cloned().unwrap_or_else(pending);
                    if let Some(sink) = return_type {
                        self.check_function_sink(
                            &body,
                            sink,
                            root.children.last().unwrap().location(),
                            &format!("function '{name}': return value"),
                        );
                    }
                    pending()
                }
                ExprKind::Sequence => children.last().cloned().unwrap_or_else(pending),
                ExprKind::Call(name) => {
                    if let Some(function) = self.functions.get(name) {
                        for ((parameter, child), authored) in
                            function.params.iter().zip(children).zip(&root.children)
                        {
                            if let Some(sink) = &parameter.param_type {
                                self.check_function_sink(
                                    child,
                                    sink,
                                    authored.location(),
                                    &format!("function '{name}': parameter '{}'", parameter.name),
                                );
                            }
                        }
                    }
                    if let Some(InferenceBinding::UserFunction {
                        return_type: Some(ty),
                    }) = self.bindings.get(name)
                    {
                        return ordinary(Ok(ty.clone()), false);
                    }
                    let builtin = crate::builtins::builtin_functions()
                        .iter()
                        .find(|builtin| builtin.name == name);
                    let ty = match builtin.map(|builtin| builtin.return_type) {
                        Some(crate::builtins::BuiltinReturnType::String) => Ok(TypeExpr::String),
                        Some(crate::builtins::BuiltinReturnType::Int) => Ok(TypeExpr::Int),
                        Some(crate::builtins::BuiltinReturnType::StringList) => {
                            Ok(TypeExpr::List(Box::new(TypeExpr::String)))
                        }
                        Some(
                            crate::builtins::BuiltinReturnType::Secret
                            | crate::builtins::BuiltinReturnType::List
                            | crate::builtins::BuiltinReturnType::Map
                            | crate::builtins::BuiltinReturnType::Any,
                        ) => Err(InferenceError::UnknownType {
                            reason: format!("built-in `{name}` return type depends on arguments"),
                        }
                        .into()),
                        None => pending().judgement.ty.clone(),
                    };
                    ordinary(ty, children.iter().all(|child| child.concrete))
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
                    children.first().is_some_and(|child| child.concrete),
                ),
                ExprKind::Integer { op, location } => {
                    let operands = [children[0].clone(), children[1].clone()];
                    self.operands.insert(
                        ExpressionLocation::new(root.location.instance.clone(), location),
                        operands.clone(),
                    );
                    ordinary(
                        inference::judge_integer_operands(
                            *op,
                            location,
                            &operands,
                            self.schemas,
                            self.config,
                        ),
                        children.iter().all(|child| child.concrete),
                    )
                }
                ExprKind::Alias => children.first().cloned().unwrap_or_else(pending),
                ExprKind::Element => ordinary(
                    child_type(0).and_then(|ty| match ty {
                        TypeExpr::List(inner) | TypeExpr::Map(inner) => Ok(*inner),
                        _ => pending().judgement.ty.clone(),
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
                                        && !judged.concrete
                                        && judged.static_type().is_ok() =>
                                {
                                    self.push_finding(value.location().clone(), error);
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

    pub(crate) fn pending() -> JudgedExpr {
        JudgedExpr {
            origin: None,
            concrete: false,
            coalesce: None,
            elements: Arc::default(),
            fields: Arc::default(),
            judgement: Arc::new(ExprJudgement {
                ty: Err(InferenceIssue::NotInferable(
                    NotInferableReason::MissingValue,
                )),
                kind: InferredKind::Ordinary,
                invalid: false,
            }),
        }
    }
}
