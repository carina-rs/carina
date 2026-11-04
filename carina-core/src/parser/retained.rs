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
    LoopSlot(TypeExpr),
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
            ExprKind::LoopSlot(ty) => ExprKind::LoopSlot(ty),
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
    arguments: HashMap<B, TypeExpr>,
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
        arguments: impl IntoIterator<Item = (B, TypeExpr)>,
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

/// A schema-judged configuration owns its file and report and borrows the
/// caller's schema registry. Parsing alone cannot construct this type.
/// Consumers borrow this bundle so the file and schemas stay paired.
#[derive(Debug, Clone)]
pub struct JudgedFile<'a, E, D, B: BindingAddress = RelativeBindingName> {
    file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &'a crate::schema::SchemaRegistry,
}

impl<E, D, B: BindingAddress> JudgedFile<'_, E, D, B> {
    pub fn file(&self) -> &super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        &self.file
    }
    pub fn report(&self) -> &ExpressionReport<B> {
        self.file.expression_report()
    }
    pub fn schemas(&self) -> &crate::schema::SchemaRegistry {
        self.schemas
    }
    pub(crate) fn into_file(self) -> super::File<E, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
    }
}

impl<'a, E: super::ExportParamLike<ParserPlaceholder, B>, D, B: BindingAddress>
    JudgedFile<'a, E, D, B>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn new(
        mut file: super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schemas: &'a crate::schema::SchemaRegistry,
        config: &super::ProviderContext,
    ) -> Self {
        judge_configuration(&mut file, schemas, config);
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
                    (Rule::for_indexed_binding, 0) => ExprKind::LoopSlot(TypeExpr::Int),
                    (Rule::for_map_binding, 0) => ExprKind::LoopSlot(TypeExpr::String),
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
pub(crate) fn value_dependent_judgement_for_test() -> JudgedExpr {
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Knowledge {
        Known,
        Deferred,
    }

    impl From<bool> for Knowledge {
        fn from(known: bool) -> Self {
            if known { Self::Known } else { Self::Deferred }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InferredKind {
        Ordinary(Knowledge),
        // Schema-projected references retain the existing binding check and
        // apply-time value validation, rather than acquiring a new nominal
        // declaration obligation at a function boundary.
        Reference(Knowledge),
        Literal,
        EnumIdentifier,
        StringConstruction(Knowledge),
        List(Knowledge),
        Map(Knowledge),
        // Loop slots are inferred from the iterable, never annotated sources.
        LoopSlot,
    }

    impl InferredKind {
        fn knowledge(self) -> Knowledge {
            match self {
                Self::Ordinary(knowledge)
                | Self::Reference(knowledge)
                | Self::StringConstruction(knowledge)
                | Self::List(knowledge)
                | Self::Map(knowledge) => knowledge,
                Self::Literal | Self::EnumIdentifier => Knowledge::Known,
                Self::LoopSlot => Knowledge::Deferred,
            }
        }
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
        Reported,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum ExprJudgement {
        Typed {
            ty: TypeExpr,
            kind: InferredKind,
            owner: FailureOwner,
        },
        Failed {
            error: InferenceError,
            owner: FailureOwner,
        },
        // A valid source type rejected at this authored sink. Keeping the source
        // lets the diagnostic owner describe it without re-inferring a value.
        Rejected {
            source: Box<ExprJudgement>,
            error: InferenceError,
            owner: FailureOwner,
        },
        ValueDependent {
            reason: NotInferableReason,
            shape: Option<(CollectionKind, Knowledge)>,
            owner: FailureOwner,
        },
        AuthorityUnavailable(ProviderAuthorityUnavailable),
        Declaration,
    }

    impl ExprJudgement {
        fn kind(&self) -> Option<InferredKind> {
            match self {
                ExprJudgement::Typed { kind, .. } => Some(*kind),
                ExprJudgement::Rejected { source, .. } => source.kind(),
                ExprJudgement::ValueDependent {
                    shape: Some((CollectionKind::List, knowledge)),
                    ..
                } => Some(InferredKind::List(*knowledge)),
                ExprJudgement::ValueDependent {
                    shape: Some((CollectionKind::Map, knowledge)),
                    ..
                } => Some(InferredKind::Map(*knowledge)),
                _ => None,
            }
        }
        fn static_type(&self) -> Result<&TypeExpr, InferenceIssue> {
            match self {
                ExprJudgement::Typed { ty, .. } => Ok(ty),
                ExprJudgement::Failed { error, .. } => Err(error.clone().into()),
                ExprJudgement::Rejected { source, .. } => source.static_type(),
                ExprJudgement::ValueDependent { reason, .. } => {
                    Err(InferenceIssue::NotInferable(reason.clone()))
                }
                ExprJudgement::AuthorityUnavailable(authority) => {
                    Err(InferenceIssue::NotInferable(match &authority.0 {
                        UnavailableSchema::Resource { binding } => {
                            NotInferableReason::SchemaUnavailable {
                                binding: binding.clone(),
                            }
                        }
                        UnavailableSchema::Named { identity } => {
                            NotInferableReason::CustomTypeSchemaUnavailable {
                                identity: identity.clone(),
                            }
                        }
                    }))
                }
                ExprJudgement::Declaration => Err(InferenceError::UnknownType {
                    reason: "declaration is not a value".into(),
                }
                .into()),
            }
        }
    }

    /// Admission evidence, constructible only by the recursive judge.
    ///
    /// ```compile_fail,E0616
    /// use carina_core::parser::retained::JudgedExpr;
    /// fn overwrite_judgement(expression: &mut JudgedExpr) { expression.judgement = unreachable!(); }
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct JudgedExpr(Arc<JudgedExprData>);

    #[doc(hidden)]
    #[derive(Debug, Clone)]
    pub struct JudgedExprData {
        origin: Option<ExpressionLocation>,
        boundary_owner: Option<Arc<DeclarationLocation>>,
        judgement: ExprJudgement,
        authority: Option<Arc<super::super::ProviderContext>>,
        coalesce: Option<Arc<[JudgedExpr; 2]>>,
        alternatives: Arc<Vec<JudgedExpr>>,
        member: Option<Arc<JudgedExpr>>,
        elements: Arc<Vec<JudgedExpr>>,
        fields: Arc<IndexMap<String, JudgedExpr>>,
    }

    impl PartialEq for JudgedExprData {
        fn eq(&self, other: &Self) -> bool {
            self.origin == other.origin
                && self.boundary_owner == other.boundary_owner
                && self.judgement == other.judgement
                && match (&self.authority, &other.authority) {
                    (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                    (None, None) => true,
                    (Some(_), None) | (None, Some(_)) => false,
                }
                && self.coalesce == other.coalesce
                && self.alternatives == other.alternatives
                && self.member == other.member
                && self.elements == other.elements
                && self.fields == other.fields
        }
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
        fn report_at(&mut self, owner: FailureOwner) {
            match &mut self.data_mut().judgement {
                ExprJudgement::Typed { owner: current, .. }
                | ExprJudgement::Failed { owner: current, .. }
                | ExprJudgement::Rejected { owner: current, .. }
                | ExprJudgement::ValueDependent { owner: current, .. } => *current = owner,
                ExprJudgement::AuthorityUnavailable(_) | ExprJudgement::Declaration => {}
            }
        }
        fn kind(&self) -> Option<InferredKind> {
            self.judgement.kind()
        }
        fn with_kind(mut self, kind: impl FnOnce(Knowledge) -> InferredKind) -> Self {
            self.set_kind(kind);
            self
        }
        fn set_kind(&mut self, kind: impl FnOnce(Knowledge) -> InferredKind) {
            if let ExprJudgement::Typed { kind: current, .. } = &mut self.data_mut().judgement {
                *current = kind(current.knowledge());
            }
        }
        fn materialize_output(&mut self) {
            if let ExprJudgement::Typed {
                kind: kind @ InferredKind::StringConstruction(_),
                ..
            } = &mut self.data_mut().judgement
            {
                *kind = InferredKind::StringConstruction(Knowledge::Known);
            }
        }
        fn concrete(&self) -> bool {
            matches!(self.judgement, ExprJudgement::Typed { kind, .. }
                if kind.knowledge() == Knowledge::Known)
                || matches!(
                    self.judgement,
                    ExprJudgement::ValueDependent {
                        shape: Some((_, Knowledge::Known)),
                        ..
                    }
                )
        }
        fn failed(&self) -> bool {
            self.failure().is_some()
        }
        fn failure(&self) -> Option<(&InferenceError, FailureOwner)> {
            match &self.judgement {
                ExprJudgement::Failed { error, owner }
                | ExprJudgement::Rejected { error, owner, .. } => Some((error, *owner)),
                _ => None,
            }
        }
        fn reject(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Rejected {
                source: Box::new(self.judgement.clone()),
                error,
                owner,
            };
        }
        fn sink_owner(&self, sink: &TypeExpr) -> FailureOwner {
            if self.requires_declared_assignment()
                && self.static_type() == Ok(&TypeExpr::String)
                && sink.is_string_shaped()
                && !matches!(sink, TypeExpr::String)
            {
                return FailureOwner::Expression;
            }
            if let Some((_, owner)) = self.failure() {
                return owner;
            }
            if let Some(branch) = self
                .alternatives
                .iter()
                .find(|branch| admit_expression(branch, sink).is_err())
            {
                return branch.sink_owner(sink);
            }
            match self.check_owner() {
                ExpressionCheckOwner::BindingIndex => FailureOwner::BindingIndex,
                ExpressionCheckOwner::BindingScope => FailureOwner::BindingScope,
                ExpressionCheckOwner::Reported => FailureOwner::Reported,
                ExpressionCheckOwner::Expression if self.concrete() => FailureOwner::Boundary,
                ExpressionCheckOwner::Expression => FailureOwner::Expression,
            }
        }
        fn mark_failed(&mut self, error: InferenceError, owner: FailureOwner) {
            self.data_mut().judgement = ExprJudgement::Failed { error, owner };
        }
        pub fn static_type(&self) -> Result<&TypeExpr, InferenceIssue> {
            self.judgement.static_type()
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
                self.kind(),
                Some(InferredKind::Ordinary(_) | InferredKind::Reference(_))
            ) && self.alternatives.is_empty()
        }
        pub(crate) fn assignment_source_type(&self) -> Option<&TypeExpr> {
            match &self.judgement {
                ExprJudgement::Typed { ty, .. } => Some(ty),
                ExprJudgement::Rejected { source, .. } => match source.as_ref() {
                    ExprJudgement::Typed { ty, .. } => Some(ty),
                    _ => None,
                },
                _ => None,
            }
        }
        pub(crate) fn is_string_construction(&self) -> bool {
            matches!(self.kind(), Some(InferredKind::StringConstruction(_)))
                && self.alternatives.is_empty()
        }
        pub(crate) fn is_value_dependent(&self) -> bool {
            matches!(
                self.judgement,
                ExprJudgement::ValueDependent { shape: None, .. }
            )
        }
        pub fn check_owner(&self) -> ExpressionCheckOwner {
            match &self.judgement {
                ExprJudgement::Failed { owner, .. }
                | ExprJudgement::Rejected { owner, .. }
                | ExprJudgement::Typed { owner, .. }
                | ExprJudgement::ValueDependent { owner, .. } => match owner {
                    FailureOwner::Expression | FailureOwner::Boundary => {
                        ExpressionCheckOwner::Expression
                    }
                    FailureOwner::BindingIndex => ExpressionCheckOwner::BindingIndex,
                    FailureOwner::BindingScope => ExpressionCheckOwner::BindingScope,
                    FailureOwner::Reported => ExpressionCheckOwner::Reported,
                },
                ExprJudgement::AuthorityUnavailable(_) | ExprJudgement::Declaration => {
                    ExpressionCheckOwner::Expression
                }
            }
        }
        pub fn boundary_owner(&self) -> Option<&DeclarationLocation> {
            self.boundary_owner.as_deref()
        }
        fn owned_by_bindings(mut self) -> Self {
            self.report_at(FailureOwner::BindingIndex);
            self
        }
        pub fn coalesce_operand(&self, index: usize) -> Result<&JudgedExpr, MissingEvidence> {
            self.coalesce
                .as_ref()
                .and_then(|operands| operands.get(index))
                .ok_or_else(|| MissingEvidence::new(format!("coalesce operand {index}")))
        }
        pub fn collection_kind(&self) -> Option<CollectionKind> {
            match self.kind() {
                Some(InferredKind::List(_)) => Some(CollectionKind::List),
                Some(InferredKind::Map(_)) => Some(CollectionKind::Map),
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
            match self.kind() {
                Some(InferredKind::List(_)) => Some(ValueShape::List),
                Some(InferredKind::Map(_)) => Some(ValueShape::Map),
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
        use ExpressionAdmission::{DeferredInterpolation, ProviderPending, ValueCheck};
        match (left, right) {
            (
                pending @ ProviderPending(_),
                ValueCheck | DeferredInterpolation | ProviderPending(_),
            )
            | (ValueCheck | DeferredInterpolation, pending @ ProviderPending(_)) => pending,
            (DeferredInterpolation, ValueCheck | DeferredInterpolation)
            | (ValueCheck, DeferredInterpolation) => DeferredInterpolation,
            (ValueCheck, ValueCheck) => ValueCheck,
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
        match &expression.authority {
            Some(authority) => {
                admit_with_context(expression, sink, AdmissionContext::Assignment, authority)
            }
            None => Err(MissingEvidence::new("expression judgement authority").into()),
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AdmissionContext {
        Assignment,
        Function,
        // Struct literal leaves retain their concrete value diagnostics.
        StructValue,
        Resource,
    }

    pub(crate) fn admit_with_context(
        expression: &JudgedExpr,
        sink: &TypeExpr,
        context: AdmissionContext,
        config: &super::super::ProviderContext,
    ) -> Result<ExpressionAdmission, InferenceError> {
        use ExpressionAdmission::{DeferredInterpolation, ProviderPending, ValueCheck};
        let (ty, kind, concrete) = match &expression.judgement {
            ExprJudgement::Failed { error, .. } | ExprJudgement::Rejected { error, .. } => {
                return Err(error.clone());
            }
            ExprJudgement::AuthorityUnavailable(authority) => {
                return Ok(ProviderPending(authority.clone()));
            }
            ExprJudgement::Declaration => return Err(assignment_error(expression, sink)),
            ExprJudgement::Typed { ty, kind, .. } => {
                (Some(ty), Some(kind), kind.knowledge() == Knowledge::Known)
            }
            ExprJudgement::ValueDependent { .. } => (None, None, false),
        };
        if !expression.alternatives.is_empty() {
            return expression
                .alternatives
                .iter()
                .try_fold(ValueCheck, |admission, branch| {
                    Ok(combine_admission(
                        admission,
                        admit_with_context(branch, sink, context, config)?,
                    ))
                });
        }
        if expression.is_value_dependent() {
            return Ok(ValueCheck);
        }
        let sink = crate::validation::resolve_type_expr(sink, config).map_err(|error| {
            InferenceError::UnknownType {
                reason: error.to_string(),
            }
        })?;
        let sink = &sink;
        let resolved_actual = ty
            .map(|ty| crate::validation::resolve_type_expr(ty, config))
            .transpose()
            .map_err(|error| InferenceError::UnknownType {
                reason: error.to_string(),
            })?;
        let ty = resolved_actual.as_ref();
        if context == AdmissionContext::Function
            && (matches!(ty, Some(TypeExpr::Ref(_)))
                || (matches!(kind, Some(InferredKind::Reference(_)))
                    && ty == Some(&TypeExpr::String)
                    && sink.is_string_shaped()))
        {
            // FunctionArgument owns handle identity; schema strings keep concrete refinement checks.
            return Ok(ValueCheck);
        }
        if let TypeExpr::Union(members) = sink {
            if expression.requires_declared_assignment()
                && ty == Some(&TypeExpr::String)
                && !members.is_empty()
                && members
                    .iter()
                    .all(|member| matches!(member, TypeExpr::StringLiteral(_)))
            {
                return Ok(ValueCheck);
            }
            return members
                .iter()
                .find_map(|member| admit_with_context(expression, member, context, config).ok())
                .ok_or_else(|| assignment_error(expression, sink));
        }
        if let Some(shape) = expression.collection_kind() {
            let child_context = if matches!(
                context,
                AdmissionContext::Assignment | AdmissionContext::StructValue
            ) && matches!(sink, TypeExpr::Struct { .. })
            {
                AdmissionContext::StructValue
            } else {
                context
            };
            let pairs: Vec<_> = match (shape, sink) {
                (CollectionKind::List, TypeExpr::List(inner)) => expression
                    .elements()
                    .map(|child| (child, inner.as_ref()))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Map(inner)) => expression
                    .fields()
                    .map(|(_, child)| child)
                    .chain(expression.member.iter().map(Arc::as_ref))
                    .map(|child| (child, inner.as_ref()))
                    .collect(),
                (CollectionKind::Map, TypeExpr::Struct { fields }) => fields
                    .iter()
                    .filter(|(name, _)| {
                        context != AdmissionContext::Resource || expression.field(name).is_ok()
                    })
                    .map(|(name, sink)| {
                        expression
                            .field(name)
                            .map(|child| (child, sink))
                            .map_err(|_| InferenceError::UnknownType {
                                reason: format!("expected struct, missing field '{name}'"),
                            })
                    })
                    .collect::<Result<_, _>>()?,
                (CollectionKind::List | CollectionKind::Map, _) => {
                    return Err(assignment_error(expression, sink));
                }
            };
            return pairs
                .into_iter()
                .try_fold(ValueCheck, |admission, (child, sink)| {
                    Ok(combine_admission(
                        admission,
                        admit_with_context(child, sink, child_context, config)?,
                    ))
                });
        }
        let actual = ty.ok_or_else(|| MissingEvidence::new("collection judgement"))?;
        if let (
            TypeExpr::Ref(actual),
            TypeExpr::Ref(expected) | TypeExpr::DottedUnresolved(expected),
        ) = (actual, sink)
        {
            return if actual == expected {
                Ok(ValueCheck)
            } else {
                Err(assignment_error(expression, sink))
            };
        }
        if let Some(authority) = unavailable_type(actual) {
            return if matches!(
                ValueShape::of_type(sink),
                Some(ValueShape::String | ValueShape::Int | ValueShape::Float)
            ) || unavailable_type(sink).is_some()
            {
                Ok(ProviderPending(authority))
            } else {
                Err(assignment_error(expression, sink))
            };
        }
        if expression.requires_declared_assignment() {
            if *actual == TypeExpr::String && !matches!(sink, TypeExpr::String) {
                return Err(assignment_error(expression, sink));
            }
            return match crate::validation::typecheck::inferred_type_assignment(
                sink,
                actual,
                &config.custom_types,
            ) {
                crate::schema::TypeAssignability::Assignable => Ok(ValueCheck),
                crate::schema::TypeAssignability::Mismatch => {
                    Err(assignment_error(expression, sink))
                }
                crate::schema::TypeAssignability::Unchecked(evidence) => evidence
                    .into_types()
                    .find_map(|ty| pending_type(&ty))
                    .map(ProviderPending)
                    .ok_or_else(|| assignment_error(expression, sink)),
            };
        }
        if context == AdmissionContext::StructValue && concrete {
            return Ok(ValueCheck);
        }
        if let Some(authority) = unavailable_type(sink) {
            return if matches!(
                ValueShape::of_type(actual),
                Some(ValueShape::String | ValueShape::Int | ValueShape::Float)
            ) {
                Ok(ProviderPending(authority))
            } else {
                Err(assignment_error(expression, sink))
            };
        }
        if kind == Some(&InferredKind::StringConstruction(Knowledge::Deferred)) {
            return if sink.is_string_shaped() || matches!(sink, TypeExpr::Ref(_)) {
                Ok(DeferredInterpolation)
            } else {
                Err(InferenceError::UnknownType {
                    reason: format!(
                        "cannot assign deferred interpolation: expected {sink}, got String"
                    ),
                })
            };
        }
        let shaped = match actual {
            TypeExpr::String | TypeExpr::StringLiteral(_) => {
                sink.is_string_shaped()
                    || matches!(sink, TypeExpr::StringLiteral(_) | TypeExpr::Ref(_))
            }
            TypeExpr::Int => matches!(
                ValueShape::of_type(sink),
                Some(ValueShape::Int | ValueShape::Float)
            ),
            TypeExpr::Float => ValueShape::of_type(sink) == Some(ValueShape::Float),
            TypeExpr::Bool => ValueShape::of_type(sink) == Some(ValueShape::Bool),
            TypeExpr::Duration => ValueShape::of_type(sink) == Some(ValueShape::Duration),
            TypeExpr::Ref(_)
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Struct { .. }
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Simple(_)
            | TypeExpr::Union(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::UnloadedSchemaType(_) => false,
        };
        if shaped {
            Ok(ValueCheck)
        } else {
            Err(assignment_error(expression, sink))
        }
    }

    fn pending_type(ty: &TypeExpr) -> Option<ProviderAuthorityUnavailable> {
        match ty {
            TypeExpr::List(inner) | TypeExpr::Map(inner) => pending_type(inner),
            TypeExpr::Struct { fields } => fields.iter().find_map(|(_, ty)| pending_type(ty)),
            TypeExpr::Union(members) => members.iter().find_map(pending_type),
            _ => unavailable_type(ty),
        }
    }

    fn unavailable_type(ty: &TypeExpr) -> Option<ProviderAuthorityUnavailable> {
        let identity = match ty {
            TypeExpr::UnloadedSchemaType(unloaded) => unloaded.identity().clone(),
            TypeExpr::DottedUnresolved(path) => {
                let mut parts = path.resource_type.split('.').collect::<Vec<_>>();
                let kind = parts.pop()?;
                crate::schema::TypeIdentity::new(Some(path.provider.as_str()), parts, kind)
            }
            _ => return None,
        };
        Some(ProviderAuthorityUnavailable(UnavailableSchema::Named {
            identity,
        }))
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
            let admission = admit_expression(expression, sink);
            if let Err(error) = admission {
                let owner = expression.sink_owner(sink);
                let owner = if owner == FailureOwner::Expression {
                    Arc::make_mut(&mut self.findings)
                        .push(Located::new(location.clone(), error.clone()));
                    FailureOwner::Reported
                } else {
                    owner
                };
                if let Some(expression) = Arc::make_mut(&mut self.expressions)
                    .get_mut(&ExpressionLocation::new(self.instance.clone(), location))
                {
                    expression.reject(error, owner);
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
                        Ok(mut expression) => {
                            let resolved = file
                                .compositions
                                .iter()
                                .filter(|composition| composition.binding.as_ref() == Some(binding))
                                .filter_map(|composition| {
                                    composition
                                        .signature
                                        .attributes
                                        .get(declaration.detail().name())
                                })
                                .filter_map(|attribute| attribute.value())
                                .any(|value| value.is_fully_resolved());
                            if resolved {
                                expression.materialize_output();
                            }
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
            first_error(&self.function_errors)
        }
        pub fn first_error(&self) -> Option<super::super::ParseError> {
            first_error(&self.findings)
        }
    }

    fn first_error(findings: &[Located<InferenceError>]) -> Option<super::super::ParseError> {
        findings.first().map(|finding| {
            super::super::ParseError::InvalidExpression {
                line: finding.location().span().start_line(),
                message: finding.detail().to_string(),
            }
            .at(finding.location())
        })
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
        let authority = Arc::new(config.clone());
        let mut judge = Judge {
            authority: authority.clone(),
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
                let refreshed = judge.refresh_module_output(evidence);
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
            module_attributes: Arc::new(module_attributes),
            resource_reports: Arc::default(),
            parameters: file
                .arguments
                .iter()
                .map(|argument| {
                    (argument.name.clone(), {
                        let mut parameter = declared(Ok(argument.static_type.clone()), false);
                        parameter.data_mut().authority = Some(authority.clone());
                        parameter
                    })
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
        authority: Arc<super::super::ProviderContext>,
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
        fn is_list_literal(mut expression: &ExprValue<ParserPlaceholder, B>) -> bool {
            while matches!(expression.authored.kind, ExprKind::Alias)
                && let [inner] = expression.authored.children.as_slice()
            {
                expression = inner;
            }
            matches!(expression.authored.kind, ExprKind::List)
        }

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
                failure.report_at(FailureOwner::BindingIndex);
                return failure;
            }
            self.visiting.push(id);
            // Always visit every child, even for length(), selected access,
            // interpolation, and a coalesce whose left payload is concrete.
            let mut children = root
                .children
                .iter()
                .map(|child| self.judge(child))
                .collect::<Vec<_>>();
            if matches!(&root.kind, ExprKind::Call(name) if name == "join"
                && !self.functions.contains_key(&(root.location.instance.clone(), name.clone())))
                && root.children.get(1).is_some_and(Self::is_list_literal)
                && let Some(list) = children.get_mut(1)
                && list.failed()
                && !list.elements.is_empty()
                && list.elements.iter().all(|element| {
                    !element.failed()
                        && matches!(
                            element.value_shape(),
                            Some(
                                ValueShape::String
                                    | ValueShape::Int
                                    | ValueShape::Float
                                    | ValueShape::Bool
                                    | ValueShape::Duration
                            )
                        )
                })
            {
                // join converts each scalar independently; this argument does
                // not require a common element type. Only its list-shape
                // failure is discharged: every element retains its evidence,
                // and other uses of this collection retain their rejection.
                let knowledge = list.elements.iter().all(JudgedExpr::concrete).into();
                list.data_mut().judgement = ExprJudgement::ValueDependent {
                    reason: NotInferableReason::ValueDependent {
                        reason: "join accepts independently rendered scalar elements".into(),
                    },
                    shape: Some((CollectionKind::List, knowledge)),
                    owner: FailureOwner::Boundary,
                };
            }
            let mut result = self.infer(expression, &children);
            result.data_mut().origin = Some(root.location.clone());
            result.data_mut().authority = Some(self.authority.clone());
            if matches!(root.kind, ExprKind::Reference { .. }) {
                if matches!(result.kind(), Some(InferredKind::Ordinary(_))) {
                    result.set_kind(InferredKind::Reference);
                }
                // The binding pass owns reference existence and field errors.
                // A containing operation preserves that ownership.
                if result.kind() != Some(InferredKind::EnumIdentifier) {
                    result.report_at(FailureOwner::BindingIndex);
                }
            }
            if let Some(failed) = children.iter().find(|child| child.failed()) {
                let (error, owner) = failed.failure().expect("failed child");
                let owner = match owner {
                    FailureOwner::Reported | FailureOwner::BindingScope => owner,
                    _ => match result.judgement {
                        ExprJudgement::Failed { owner, .. }
                        | ExprJudgement::Rejected { owner, .. }
                        | ExprJudgement::Typed { owner, .. }
                        | ExprJudgement::ValueDependent { owner, .. } => owner,
                        ExprJudgement::Declaration => FailureOwner::Boundary,
                        ExprJudgement::AuthorityUnavailable(_) => FailureOwner::Expression,
                    },
                };
                result.mark_failed(error.clone(), owner);
                result.data_mut().boundary_owner = failed.boundary_owner.clone();
            }
            if let ExprJudgement::Failed {
                error,
                owner: FailureOwner::Expression,
            } = &result.judgement
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
                result.report_at(FailureOwner::Reported);
            }
            self.visiting.pop();
            self.memo.insert(id, result.clone());
            self.by_location
                .insert(root.location.clone(), result.clone());
            result
        }

        fn refresh_module_output(&mut self, evidence: &JudgedExpr) -> JudgedExpr {
            let Some(origin) = evidence.origin.as_ref() else {
                return evidence.clone();
            };
            let mut output = match self.authored.get(origin).copied() {
                Some(authored) => self.judge(authored),
                None => incomplete("authored module output origin"),
            };
            if evidence.concrete() {
                output.materialize_output();
            }
            output
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
                        expression.concrete(),
                    );
                    // Declaration resolution owns this error. It still makes
                    // the function/call judgement invalid.
                    failed.report_at(FailureOwner::Reported);
                    return Some(failed);
                }
                crate::validation::typecheck::FunctionExpressionError::Expression(error) => error,
            };
            if expression.failed() {
                return Some(expression.clone());
            }
            let finding = Located::new(
                location.clone(),
                InferenceError::UnknownType {
                    reason: format!("{context}: {error}"),
                },
            );
            if expression.kind() == Some(InferredKind::StringConstruction(Knowledge::Deferred))
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
            owned.report_at(FailureOwner::Reported);
            let owner = Arc::new(owner.clone());
            owned.data_mut().boundary_owner = Some(owner.clone());
            let mut failed = declared(Err(finding.detail().clone().into()), expression.concrete());
            failed.report_at(FailureOwner::Reported);
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

        fn parameter_type(&self, binding: &B) -> Option<&TypeExpr> {
            self.expressions.arguments.get(binding).or_else(|| {
                match self.bindings.get(binding.as_str()) {
                    Some(InferenceBinding::Argument { type_expr }) => Some(type_expr),
                    _ => None,
                }
            })
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

        // Every reference and postfix form selects from judgement evidence.
        // Reconstructing from static_type() alone loses dynamic results and
        // the authored children of heterogeneous records.
        fn access(
            &self,
            base: &JudgedExpr,
            step: Option<&PathSegment>,
            binding: &str,
        ) -> JudgedExpr {
            if base.is_value_dependent()
                || base.failed()
                || matches!(base.judgement, ExprJudgement::AuthorityUnavailable(_))
                || base.kind() == Some(InferredKind::EnumIdentifier)
            {
                return base.clone();
            }
            let Some(step) = step else {
                if let Some(member) = &base.member {
                    return (**member).clone();
                }
                let candidates: Vec<_> = base
                    .elements
                    .iter()
                    .chain(base.fields.values())
                    .cloned()
                    .collect();
                let Some(first) = candidates.first() else {
                    return incomplete("dynamic access element");
                };
                let ty = first.static_type();
                if ty.is_err()
                    || candidates
                        .iter()
                        .any(|candidate| candidate.static_type() != ty)
                {
                    return value_dependent("dynamic access depends on the selected element");
                }
                return alternatives(ty.cloned(), &candidates);
            };
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
            let ty = base
                .static_type()
                .cloned()
                .and_then(|ty| self.narrow_declared(&ty, std::slice::from_ref(step), binding));
            let mut result = declared(ty, base.concrete());
            result.report_at(
                if base.check_owner() == ExpressionCheckOwner::BindingIndex {
                    FailureOwner::BindingIndex
                } else {
                    FailureOwner::Boundary
                },
            );
            if matches!(
                base.static_type(),
                Ok(TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_))
            ) {
                result.set_kind(InferredKind::Reference);
            }
            result
        }

        fn access_path(
            &self,
            base: JudgedExpr,
            steps: &[PathSegment],
            binding: &str,
        ) -> JudgedExpr {
            steps
                .iter()
                .fold(base, |base, step| self.access(&base, Some(step), binding))
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
                    || incomplete("operand").static_type().cloned(),
                    |child| child.static_type().cloned(),
                )
            };
            match &root.kind {
                ExprKind::Scalar(ty) => {
                    ordinary(Ok(ty.clone()), true).with_kind(|_| InferredKind::Literal)
                }
                ExprKind::Parameter(ty) => ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                ExprKind::LoopSlot(ty) => ordinary(Ok(ty.clone()), false)
                    .with_kind(|_| InferredKind::LoopSlot)
                    .owned_by_bindings(),
                ExprKind::UntypedParameter => value_dependent("unannotated function parameter"),
                ExprKind::Binding(name) => {
                    if let Some(ty) = self.parameter_type(name) {
                        ordinary(Ok(ty.clone()), false).owned_by_bindings()
                    } else if let Some(definition) = self.expressions.definitions.get(name) {
                        self.judge(definition)
                    } else {
                        ordinary(Ok(TypeExpr::String), true).with_kind(|_| InferredKind::Literal)
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
                    if let Some(ty) = self.parameter_type(binding) {
                        return self.access_path(
                            ordinary(Ok(ty.clone()), false).owned_by_bindings(),
                            &steps,
                            binding.as_str(),
                        );
                    }
                    if let Some(attribute) = self.module_attributes.and_then(|attributes| {
                        attributes.get(&(binding.clone(), attribute.clone()))
                    }) {
                        let attribute = self.refresh_module_output(attribute);
                        return self.access_path(attribute, segments, binding.as_str());
                    }
                    if matches!(
                        self.bindings.get(binding.as_str()),
                        Some(InferenceBinding::UpstreamState | InferenceBinding::ModuleCall)
                    ) {
                        let mut result = value_dependent(
                            "external value is checked at its declared or resolved boundary",
                        );
                        result.data_mut().judgement = ExprJudgement::ValueDependent {
                            reason: NotInferableReason::NonInferableBinding {
                                binding: binding.to_string(),
                            },
                            shape: None,
                            owner: FailureOwner::Expression,
                        };
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
                        self.access_path(base, &steps, binding.as_str())
                    } else {
                        ordinary(Ok(TypeExpr::String), true)
                            .with_kind(|_| InferredKind::EnumIdentifier)
                    }
                }
                ExprKind::EmptyInterpolation => ordinary(Ok(TypeExpr::String), false),
                ExprKind::Interpolation => ordinary(
                    Ok(TypeExpr::String),
                    children.iter().all(JudgedExpr::concrete),
                )
                .with_kind(InferredKind::StringConstruction),
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
                    let ty = match (&left, &right) {
                        (Ok(_), Ok(_)) => coalesce_type(left, right, empty),
                        _ if matches!((children[0].value_shape(), children[1].value_shape()), (Some(left), Some(right)) if left != right) => {
                            Err(InferenceError::UnknownType {
                                reason: format!(
                                    "coalesce default type `{}` does not match left type `{}`",
                                    children[1].type_description(),
                                    children[0].type_description()
                                ),
                            }
                            .into())
                        }
                        _ => coalesce_type(left, right, empty),
                    };
                    let mut result = alternatives(ty, children);
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
                            Some(first) => first.static_type().cloned().and_then(|first| {
                                for child in &children[1..] {
                                    let next = child.static_type()?;
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
                    let shape = if matches!(root.kind, ExprKind::List) {
                        CollectionKind::List
                    } else {
                        CollectionKind::Map
                    };
                    let mut result = match ty {
                        Err(error @ InferenceIssue::Failed(_))
                            if incompatible.is_some()
                                || (shape == CollectionKind::List
                                    && matches!(
                                        error,
                                        InferenceIssue::Failed(
                                            InferenceError::HeterogeneousCollection { .. }
                                        )
                                    )) =>
                        {
                            let mut failed = declared(Err(error), false);
                            failed.report_at(FailureOwner::Boundary);
                            failed
                        }
                        ty => collection(ty, shape, children.iter().all(JudgedExpr::concrete)),
                    };
                    result.data_mut().member = None;
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
                    let body = authored_child(children.last());
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
                ExprKind::Sequence => authored_child(children.last()),
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
                    use crate::builtins::BuiltinReturnType;
                    let concrete = children.iter().all(JudgedExpr::concrete);
                    match builtin.map(|builtin| builtin.return_type) {
                        Some(BuiltinReturnType::Argument(index)) => children
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| incomplete("builtin argument")),
                        Some(BuiltinReturnType::String | BuiltinReturnType::Secret) => {
                            ordinary(Ok(TypeExpr::String), concrete)
                        }
                        Some(BuiltinReturnType::Int) => ordinary(Ok(TypeExpr::Int), concrete),
                        Some(BuiltinReturnType::StringList) => {
                            ordinary(Ok(TypeExpr::List(Box::new(TypeExpr::String))), concrete)
                        }
                        Some(return_type @ (BuiltinReturnType::List | BuiltinReturnType::Map)) => {
                            let shape = if matches!(return_type, BuiltinReturnType::List) {
                                CollectionKind::List
                            } else {
                                CollectionKind::Map
                            };
                            let mut result = collection(
                                value_dependent("builtin return type depends on arguments")
                                    .static_type()
                                    .cloned(),
                                shape,
                                concrete,
                            );
                            result.data_mut().member = Some(Arc::new(value_dependent(
                                "builtin elements require resolved constraint checks",
                            )));
                            result
                        }
                        Some(BuiltinReturnType::Any) => {
                            value_dependent("builtin return type depends on arguments")
                        }
                        None if signature.is_some() => {
                            value_dependent("unannotated function result")
                        }
                        None => incomplete("function declaration or builtin signature"),
                    }
                }
                ExprKind::Access(step) => self.access(&children[0], Some(step), "access base"),
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
                        children.iter().all(|child| child.concrete()),
                    )
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
                        failure.report_at(FailureOwner::BindingScope);
                        return failure;
                    }
                    children
                        .first()
                        .cloned()
                        .unwrap_or_else(|| incomplete("iterable operand"))
                }
                ExprKind::Alias | ExprKind::Wait | ExprKind::Sink(_) => {
                    authored_child(children.first())
                }
                ExprKind::Element => {
                    let collection = &children[0];
                    if collection.is_value_dependent() {
                        return value_dependent(
                            "external iterable elements require resolved value checks",
                        );
                    }
                    if let Some(member) = &collection.member {
                        return loop_element((**member).clone());
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
                        return loop_element(element);
                    }
                    match child_type(0) {
                        Ok(TypeExpr::List(inner) | TypeExpr::Map(inner)) => {
                            ordinary(Ok(*inner), false).with_kind(|_| InferredKind::LoopSlot)
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
                            failure.report_at(FailureOwner::BindingScope);
                            failure
                        }
                        Err(error) => ordinary(Err(error), false),
                    }
                }
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
                    let rejection = self
                        .schemas
                        .get(provider, resource_type, kind)
                        .and_then(|schema| self.admit_resource_children(&root.children, schema));
                    let mut result = ordinary(
                        Ok(TypeExpr::Ref(super::super::ResourceTypePath::new(
                            provider,
                            resource_type,
                        ))),
                        false,
                    );
                    result.report_at(FailureOwner::Boundary);
                    if let Some(failure) = rejection {
                        let (error, owner) = failure.failure().expect("rejected resource sink");
                        result.reject(error.clone(), owner);
                    }
                    result
                }
                ExprKind::If => {
                    let branches = &children[1..];
                    let mut ty = branches
                        .first()
                        .map(|branch| branch.static_type().cloned())
                        .unwrap_or_else(|| incomplete("if branch").static_type().cloned());
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
                                    && matches!(
                                        branches[0].kind(),
                                        Some(InferredKind::List(_))
                                    ) =>
                            {
                                ty = branch.static_type().cloned()
                            }
                            _ => {}
                        }
                    }
                    let mut result = alternatives(ty, branches);
                    if !branches.iter().any(JudgedExpr::failed) && result.failed() {
                        result.data_mut().judgement = ExprJudgement::ValueDependent {
                            reason: NotInferableReason::ValueDependent {
                                reason: result.static_type().unwrap_err().to_string(),
                            },
                            shape: None,
                            owner: FailureOwner::Boundary,
                        };
                    }
                    if let ExprJudgement::Typed {
                        kind: InferredKind::Ordinary(knowledge),
                        ..
                    } = &mut result.data_mut().judgement
                    {
                        *knowledge = children.iter().all(JudgedExpr::concrete).into();
                    }
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
                        failure.report_at(FailureOwner::BindingScope);
                        return failure;
                    }
                    let body = children
                        .last()
                        .cloned()
                        .unwrap_or_else(|| incomplete("for body"));
                    let mut result = collection(
                        body.static_type()
                            .cloned()
                            .map(|ty| TypeExpr::List(Box::new(ty))),
                        CollectionKind::List,
                        false,
                    );
                    result.data_mut().member = Some(Arc::new(body));
                    result
                }
                ExprKind::DynamicAccess => self.access(&children[0], None, "dynamic access base"),
                ExprKind::Boolean => {
                    let mut result = ordinary(
                        Ok(TypeExpr::Bool),
                        children.iter().all(|child| child.concrete()),
                    );
                    // Conditions retain their dedicated wait/validation checks.
                    result.report_at(FailureOwner::Boundary);
                    result
                }
                ExprKind::NestedBlock { .. } => children[0].clone(),
                ExprKind::ModuleCall => {
                    let mut result = value_dependent("module call result requires its signature");
                    result.report_at(FailureOwner::Boundary);
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
        ) -> Option<JudgedExpr> {
            let mut failure = None;
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
                                schema.schema().as_schema().defs(),
                            );
                            let value = &child.authored.children[0];
                            let judged = self.judge(value);
                            let admission = admit_with_context(
                                &judged,
                                &expected,
                                AdmissionContext::Resource,
                                self.config,
                            );
                            if let Err(error) = admission {
                                // Direct resource parameters retain the binding
                                // check's resource, argument, and source context.
                                // Folded branches and other boundaries still
                                // need the judge to report their rejection.
                                let owner = if judged.requires_declared_assignment()
                                    && matches!(judged.kind(), Some(InferredKind::Ordinary(_)))
                                    && judged.check_owner() == ExpressionCheckOwner::BindingIndex
                                {
                                    FailureOwner::BindingIndex
                                } else {
                                    judged.sink_owner(&expected)
                                };
                                if owner == FailureOwner::Expression {
                                    self.push_finding(value.location().clone(), error.clone());
                                }
                                let mut rejected = judged;
                                rejected.reject(error, owner);
                                self.by_location
                                    .insert(value.authored.location.clone(), rejected.clone());
                                self.memo.insert(
                                    Arc::as_ptr(&value.authored) as usize,
                                    rejected.clone(),
                                );
                                failure.get_or_insert(rejected);
                            }
                        }
                    }
                    ExprKind::Resource { .. } => {}
                    _ => {
                        if let Some(rejected) =
                            self.admit_resource_children(&child.authored.children, schema)
                        {
                            failure.get_or_insert(rejected);
                        }
                    }
                }
            }
            failure
        }
    }

    fn loop_element(mut expression: JudgedExpr) -> JudgedExpr {
        if matches!(
            expression.kind(),
            Some(InferredKind::Ordinary(_) | InferredKind::Reference(_))
        ) {
            expression.set_kind(|_| InferredKind::LoopSlot);
        }
        expression.data_mut().alternatives = Arc::new(
            expression
                .alternatives
                .iter()
                .cloned()
                .map(loop_element)
                .collect(),
        );
        expression
    }

    /// Join source evidence independently of a selected projection. Exact
    /// containers keep evidence for every index/key any branch can produce.
    fn alternatives(ty: Result<TypeExpr, InferenceIssue>, branches: &[JudgedExpr]) -> JudgedExpr {
        if branches.len() == 1 {
            return branches[0].clone();
        }
        let mut result = declared(ty, branches.iter().all(|branch| branch.concrete()));
        result.data_mut().authority = branches.first().and_then(|branch| branch.authority.clone());
        result.data_mut().alternatives = Arc::new(branches.to_vec());
        let member_sources = branches
            .iter()
            .filter_map(|branch| branch.member.as_deref())
            .cloned()
            .collect::<Vec<_>>();
        result.data_mut().member = member_sources
            .first()
            .map(|first| Arc::new(alternatives(first.static_type().cloned(), &member_sources)));
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
                    alternatives(sources[0].static_type().cloned(), &sources)
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
                    (
                        key,
                        alternatives(sources[0].static_type().cloned(), &sources),
                    )
                })
                .collect(),
        );
        result
    }

    fn collection(
        ty: Result<TypeExpr, InferenceIssue>,
        shape: CollectionKind,
        concrete: bool,
    ) -> JudgedExpr {
        let mut result = match ty {
            Err(reason) => {
                let mut result = value_dependent("collection has no common element type");
                result.data_mut().judgement = ExprJudgement::ValueDependent {
                    reason: match reason {
                        InferenceIssue::NotInferable(reason) => reason,
                        InferenceIssue::Failed(error) => NotInferableReason::ValueDependent {
                            reason: error.to_string(),
                        },
                    },
                    shape: Some((shape, concrete.into())),
                    owner: FailureOwner::Boundary,
                };
                result
            }
            ty => declared(ty, concrete),
        };
        result.set_kind(|knowledge| match shape {
            CollectionKind::List => InferredKind::List(knowledge),
            CollectionKind::Map => InferredKind::Map(knowledge),
        });
        result.report_at(FailureOwner::Boundary);
        result
    }

    fn declared(ty: Result<TypeExpr, InferenceIssue>, concrete: bool) -> JudgedExpr {
        let judgement = match ty {
            Ok(ty) => ExprJudgement::Typed {
                ty,
                kind: InferredKind::Ordinary(concrete.into()),
                owner: FailureOwner::Expression,
            },
            Err(InferenceIssue::Failed(error)) => ExprJudgement::Failed {
                error,
                owner: FailureOwner::Expression,
            },
            Err(InferenceIssue::NotInferable(NotInferableReason::SchemaUnavailable {
                binding,
            })) => ExprJudgement::AuthorityUnavailable(ProviderAuthorityUnavailable(
                UnavailableSchema::Resource { binding },
            )),
            Err(InferenceIssue::NotInferable(
                NotInferableReason::CustomTypeSchemaUnavailable { identity },
            )) => ExprJudgement::AuthorityUnavailable(ProviderAuthorityUnavailable(
                UnavailableSchema::Named { identity },
            )),
            Err(InferenceIssue::NotInferable(
                reason @ (NotInferableReason::ValueDependent { .. }
                | NotInferableReason::NonInferableBinding { .. }
                | NotInferableReason::PlaceholderTypeUnavailable { .. }),
            )) => ExprJudgement::ValueDependent {
                reason,
                shape: None,
                owner: FailureOwner::Boundary,
            },
            Err(InferenceIssue::NotInferable(_)) => ExprJudgement::Failed {
                error: MissingEvidence::new("incomplete authored judgement").into(),
                owner: FailureOwner::Boundary,
            },
        };
        JudgedExpr(Arc::new(JudgedExprData {
            origin: None,
            boundary_owner: None,
            judgement,
            authority: None,
            coalesce: None,
            alternatives: Arc::default(),
            member: None,
            elements: Arc::default(),
            fields: Arc::default(),
        }))
    }
    pub(crate) fn unjudged() -> Result<JudgedExpr, MissingEvidence> {
        Err(MissingEvidence::new("declaration has not been judged"))
    }
    fn authored_child(child: Option<&JudgedExpr>) -> JudgedExpr {
        child
            .cloned()
            .unwrap_or_else(|| incomplete("authored child"))
    }

    fn incomplete(request: &str) -> JudgedExpr {
        declared(
            Err(InferenceError::from(MissingEvidence::new(request)).into()),
            false,
        )
    }
    #[cfg(any(test, feature = "test-utils", feature = "test-support"))]
    pub(super) fn constraint_fixture() -> JudgedExpr {
        let mut result = value_dependent("constraint-check fixture");
        result.data_mut().judgement = ExprJudgement::ValueDependent {
            reason: NotInferableReason::NonInferableBinding {
                binding: "constraint-check fixture".into(),
            },
            shape: None,
            owner: FailureOwner::Expression,
        };
        result.data_mut().authority = Some(Arc::new(super::super::ProviderContext::new(
            crate::EnvironmentSnapshot::empty(),
        )));
        result
    }
    #[test]
    fn value_dependent_admission_requires_authority() {
        let mut expression = constraint_fixture();
        expression.data_mut().authority = None;
        assert!(
            admit_expression(&expression, &TypeExpr::String)
                .unwrap_err()
                .to_string()
                .contains("expression judgement authority")
        );
    }

    pub(super) fn value_dependent(reason: &str) -> JudgedExpr {
        let mut result = declared(Ok(TypeExpr::String), false);
        result.data_mut().judgement = ExprJudgement::ValueDependent {
            reason: NotInferableReason::ValueDependent {
                reason: reason.into(),
            },
            shape: None,
            owner: FailureOwner::Expression,
        };
        result
    }
    fn declaration() -> JudgedExpr {
        let mut result = value_dependent("declaration is not a value");
        result.data_mut().judgement = ExprJudgement::Declaration;
        result
    }
}
