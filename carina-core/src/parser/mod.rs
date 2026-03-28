//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

use crate::resource::{LifecycleConfig, Resource, ResourceId, Value};
use pest::Parser;
use pest_derive::Parser;
use std::collections::HashMap;

#[derive(Parser)]
#[grammar = "parser/carina.pest"]
struct CarinaParser;

/// Parse error
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Syntax error: {0}")]
    Syntax(#[from] pest::error::Error<Rule>),

    #[error("Invalid expression at line {line}: {message}")]
    InvalidExpression { line: usize, message: String },

    #[error("Undefined variable: {0}")]
    UndefinedVariable(String),

    #[error("Invalid resource type: {0}")]
    InvalidResourceType(String),

    #[error("Duplicate module definition: {0}")]
    DuplicateModule(String),

    #[error("Duplicate binding at line {line}: {name}")]
    DuplicateBinding { name: String, line: usize },

    #[error("Module not found: {0}")]
    ModuleNotFound(String),

    #[error("Internal parser error: expected {expected} in {context}")]
    InternalError { expected: String, context: String },

    #[error("Recursive function call detected: {0}")]
    RecursiveFunction(String),

    #[error("User-defined function error: {0}")]
    UserFunctionError(String),
}

/// Resource type path for typed references (e.g., aws.vpc, aws.security_group)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResourceTypePath {
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// Resource type (e.g., "vpc", "security_group")
    pub resource_type: String,
}

impl ResourceTypePath {
    pub fn new(provider: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
        }
    }

    /// Parse from a dot-separated string (e.g., "aws.vpc" or "aws.security_group")
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() >= 2 {
            Some(Self {
                provider: parts[0].to_string(),
                resource_type: parts[1..].join("."),
            })
        } else {
            None
        }
    }
}

impl std::fmt::Display for ResourceTypePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.provider, self.resource_type)
    }
}

/// Type expression for arguments/attributes parameters
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    String,
    Bool,
    Int,
    Float,
    /// Schema type identified by name (e.g., "cidr", "ipv4_address", "arn")
    Simple(std::string::String),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>),
    /// Reference to a resource type (e.g., aws.vpc)
    Ref(ResourceTypePath),
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::String => write!(f, "string"),
            TypeExpr::Bool => write!(f, "bool"),
            TypeExpr::Int => write!(f, "int"),
            TypeExpr::Float => write!(f, "float"),
            TypeExpr::Simple(name) => write!(f, "{}", name),
            TypeExpr::List(inner) => write!(f, "list({})", inner),
            TypeExpr::Map(inner) => write!(f, "map({})", inner),
            TypeExpr::Ref(path) => write!(f, "{}", path),
        }
    }
}

/// Argument parameter definition (in `arguments { ... }` block)
#[derive(Debug, Clone)]
pub struct ArgumentParameter {
    pub name: String,
    pub type_expr: TypeExpr,
    pub default: Option<Value>,
    /// Optional description (from block form)
    pub description: Option<String>,
}

/// Attribute parameter definition (in `attributes { ... }` block)
#[derive(Debug, Clone)]
pub struct AttributeParameter {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// State manipulation block (import, removed, moved)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StateBlock {
    /// Import existing infrastructure into Carina management
    Import {
        /// Target resource address
        to: ResourceId,
        /// Cloud provider identifier (e.g., "vpc-0abc123def456")
        id: String,
    },
    /// Remove a resource from state without destroying it
    Removed {
        /// Resource address to remove from state
        from: ResourceId,
    },
    /// Rename/move a resource in state without destroy/recreate
    Moved {
        /// Old resource address
        from: ResourceId,
        /// New resource address
        to: ResourceId,
    },
}

/// Import statement
#[derive(Debug, Clone)]
pub struct ImportStatement {
    pub path: String,
    pub alias: String,
}

/// Parameter for a user-defined function
#[derive(Debug, Clone)]
pub struct FnParam {
    pub name: String,
    pub param_type: Option<TypeExpr>,
    pub default: Option<Value>,
}

/// The body of a user-defined function: either a value expression or a resource expression
#[derive(Debug, Clone)]
pub enum UserFunctionBody {
    /// The function returns a value (existing behavior)
    Value(Value),
    /// The function returns a resource expression (resource-generating function).
    /// Stores the raw source text for re-parsing with substituted parameters.
    Resource(String),
    /// The function returns a read resource expression (data source).
    /// Stores the raw source text for re-parsing with substituted parameters.
    ReadResource(String),
}

impl UserFunctionBody {
    /// Returns true if this body produces a resource (either regular or read).
    fn is_resource(&self) -> bool {
        matches!(self, Self::Resource(_) | Self::ReadResource(_))
    }
}

/// User-defined pure function
#[derive(Debug, Clone)]
pub struct UserFunction {
    pub name: String,
    pub params: Vec<FnParam>,
    /// Optional return type annotation
    pub return_type: Option<TypeExpr>,
    /// Local let bindings inside the function body (name, expression)
    pub local_lets: Vec<(String, Value)>,
    /// The body of the function
    pub body: UserFunctionBody,
}

/// Module call (instantiation)
#[derive(Debug, Clone)]
pub struct ModuleCall {
    pub module_name: String,
    pub binding_name: Option<String>,
    pub arguments: HashMap<String, Value>,
}

/// Provider configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub attributes: HashMap<String, Value>,
    /// Default tags to apply to all resources that support tags.
    /// Extracted from `default_tags = { ... }` in the provider block.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub default_tags: HashMap<String, Value>,
}

/// Backend configuration for state storage
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendConfig {
    /// Backend type (e.g., "s3", "gcs", "local")
    pub backend_type: String,
    /// Backend-specific attributes
    pub attributes: HashMap<String, Value>,
}

/// Parse result
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub providers: Vec<ProviderConfig>,
    pub resources: Vec<Resource>,
    pub variables: HashMap<String, Value>,
    /// Import statements
    pub imports: Vec<ImportStatement>,
    /// Module calls (instantiations)
    pub module_calls: Vec<ModuleCall>,
    /// Top-level argument parameters (directory-based module style)
    pub arguments: Vec<ArgumentParameter>,
    /// Top-level attribute parameters (directory-based module style)
    pub attribute_params: Vec<AttributeParameter>,
    /// Backend configuration for state storage
    pub backend: Option<BackendConfig>,
    /// State manipulation blocks (import, removed, moved)
    pub state_blocks: Vec<StateBlock>,
    /// User-defined pure functions
    pub user_functions: HashMap<String, UserFunction>,
}

impl ParsedFile {
    /// Find a resource by resource type and name attribute value
    pub fn find_resource_by_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        attr_value: &str,
    ) -> Option<&Resource> {
        self.resources.iter().find(|r| {
            r.id.resource_type == resource_type
                && matches!(r.attributes.get(attr_name), Some(Value::String(n)) if n == attr_value)
        })
    }
}

/// Parse context (variable scope)
#[derive(Clone)]
struct ParseContext {
    variables: HashMap<String, Value>,
    /// Resource bindings (binding_name -> Resource)
    resource_bindings: HashMap<String, Resource>,
    /// Imported modules (alias -> path)
    imported_modules: HashMap<String, String>,
    /// User-defined functions
    user_functions: HashMap<String, UserFunction>,
    /// Functions currently being evaluated (for recursion detection)
    evaluating_functions: Vec<String>,
}

impl ParseContext {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
            user_functions: HashMap::new(),
            evaluating_functions: Vec::new(),
        }
    }

    fn set_variable(&mut self, name: String, value: Value) {
        self.variables.insert(name, value);
    }

    fn get_variable(&self, name: &str) -> Option<&Value> {
        self.variables.get(name)
    }

    fn set_resource_binding(&mut self, name: String, resource: Resource) {
        self.resource_bindings.insert(name, resource);
    }

    fn is_resource_binding(&self, name: &str) -> bool {
        self.resource_bindings.contains_key(name)
    }
}

/// Helper to get the next element from a pest iterator, returning a ParseError on failure
fn next_pair<'a>(
    iter: &mut pest::iterators::Pairs<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    iter.next().ok_or_else(|| ParseError::InternalError {
        expected: expected.to_string(),
        context: context.to_string(),
    })
}

/// Helper to get the first inner pair from a pest pair
fn first_inner<'a>(
    pair: pest::iterators::Pair<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    pair.into_inner()
        .next()
        .ok_or_else(|| ParseError::InternalError {
            expected: expected.to_string(),
            context: context.to_string(),
        })
}

/// Parse a .crn file
pub fn parse(input: &str) -> Result<ParsedFile, ParseError> {
    let pairs = CarinaParser::parse(Rule::file, input)?;

    let mut ctx = ParseContext::new();
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut imports = Vec::new();
    let mut module_calls = Vec::new();
    let mut arguments = Vec::new();
    let mut attribute_params = Vec::new();
    let mut backend = None;
    let mut state_blocks = Vec::new();
    let mut anon_for_counter = 0usize;
    let mut anon_if_counter = 0usize;

    for pair in pairs {
        if pair.as_rule() == Rule::file {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::statement {
                    for stmt in inner.into_inner() {
                        match stmt.as_rule() {
                            Rule::backend_block => {
                                backend = Some(parse_backend_block(stmt, &ctx)?);
                            }
                            Rule::provider_block => {
                                let provider = parse_provider_block(stmt, &ctx)?;
                                providers.push(provider);
                            }
                            Rule::arguments_block => {
                                let parsed_arguments = parse_arguments_block(stmt)?;
                                for arg in &parsed_arguments {
                                    // Register argument names as lexical bindings so that
                                    // `vpc.vpc_id` resolves as ResourceRef and `cidr_block`
                                    // resolves as a variable reference during parsing.
                                    // No `arguments.` prefix needed.
                                    let placeholder_ref = Value::ResourceRef {
                                        binding_name: arg.name.clone(),
                                        attribute_name: String::new(),
                                        field_path: vec![],
                                    };
                                    ctx.set_variable(arg.name.clone(), placeholder_ref);
                                    let placeholder = Resource::new("_argument", &arg.name);
                                    ctx.set_resource_binding(arg.name.clone(), placeholder);
                                }
                                arguments.extend(parsed_arguments);
                            }
                            Rule::attributes_block => {
                                let parsed_attribute_params = parse_attributes_block(stmt, &ctx)?;
                                attribute_params.extend(parsed_attribute_params);
                            }
                            Rule::import_state_block => {
                                state_blocks.push(parse_import_state_block(stmt)?);
                            }
                            Rule::removed_block => {
                                state_blocks.push(parse_removed_block(stmt)?);
                            }
                            Rule::moved_block => {
                                state_blocks.push(parse_moved_block(stmt)?);
                            }
                            Rule::for_expr => {
                                let binding_name = format!("_for{}", anon_for_counter);
                                anon_for_counter += 1;
                                let (expanded_resources, expanded_module_calls) =
                                    parse_for_expr(stmt, &ctx, &binding_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::if_expr => {
                                let binding_name = format!("_if{}", anon_if_counter);
                                anon_if_counter += 1;
                                let (_value, expanded_resources, expanded_module_calls, _import) =
                                    parse_if_expr(stmt, &ctx, &binding_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::fn_def => {
                                let user_fn = parse_fn_def(stmt, &ctx)?;
                                let fn_name = user_fn.name.clone();
                                // Check for shadowing builtins
                                if crate::builtins::evaluate_builtin(&fn_name, &[]).is_ok()
                                    || crate::builtins::builtin_functions()
                                        .iter()
                                        .any(|f| f.name == fn_name)
                                {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "function '{fn_name}' shadows a built-in function"
                                    )));
                                }
                                if ctx.user_functions.contains_key(&fn_name) {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "duplicate function definition: '{fn_name}'"
                                    )));
                                }
                                ctx.user_functions.insert(fn_name, user_fn);
                            }
                            Rule::let_binding => {
                                let (line, _) = stmt.as_span().start_pos().line_col();
                                let (
                                    name,
                                    value,
                                    expanded_resources,
                                    expanded_module_calls,
                                    maybe_import,
                                ) = parse_let_binding_extended(stmt, &ctx)?;
                                if ctx.variables.contains_key(&name)
                                    || ctx.resource_bindings.contains_key(&name)
                                {
                                    return Err(ParseError::DuplicateBinding { name, line });
                                }
                                ctx.set_variable(name.clone(), value);
                                if !expanded_resources.is_empty() {
                                    // Register the binding name as a resource binding
                                    // (use the first resource as placeholder)
                                    ctx.set_resource_binding(
                                        name.clone(),
                                        expanded_resources[0].clone(),
                                    );
                                    resources.extend(expanded_resources);
                                }
                                if !expanded_module_calls.is_empty() {
                                    for mut call in expanded_module_calls {
                                        if call.binding_name.is_none() {
                                            call.binding_name = Some(name.clone());
                                        }
                                        module_calls.push(call);
                                    }
                                    // Register as a resource binding so that
                                    // `name.attr` resolves as ResourceRef
                                    let placeholder = Resource::new("_module_binding", &name);
                                    ctx.set_resource_binding(name.clone(), placeholder);
                                }
                                if let Some(import) = maybe_import {
                                    ctx.imported_modules
                                        .insert(import.alias.clone(), import.path.clone());
                                    imports.push(import);
                                }
                            }
                            Rule::module_call => {
                                let call = parse_module_call(stmt, &ctx)?;
                                module_calls.push(call);
                            }
                            Rule::function_call => {
                                // Top-level function call: if it's a resource-generating fn,
                                // expand it as an anonymous resource
                                let mut fc_inner = stmt.into_inner();
                                let func_name = next_pair(
                                    &mut fc_inner,
                                    "function name",
                                    "top-level function call",
                                )?
                                .as_str()
                                .to_string();
                                if let Some(user_fn) = ctx.user_functions.get(&func_name)
                                    && user_fn.body.is_resource()
                                {
                                    let args: Result<Vec<Value>, ParseError> =
                                        fc_inner.map(|arg| parse_expression(arg, &ctx)).collect();
                                    let args = args?;
                                    let user_fn = user_fn.clone();
                                    // Use empty binding name for anonymous resource
                                    let resource = evaluate_user_function_as_resource(
                                        &user_fn, &args, &ctx, "",
                                    )?;
                                    resources.push(resource);
                                }
                            }
                            Rule::anonymous_resource => {
                                let resource = parse_anonymous_resource(stmt, &ctx)?;
                                resources.push(resource);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Second pass: resolve forward references.
    // During parsing, unknown 2-part identifiers (e.g., vpc.vpc_id where vpc is
    // declared later) become String values like "vpc.vpc_id". Now that we have the
    // full binding set, convert matching ones to ResourceRef.
    resolve_forward_references(
        &ctx.resource_bindings,
        &mut resources,
        &mut attribute_params,
        &mut module_calls,
    );

    Ok(ParsedFile {
        providers,
        resources,
        variables: ctx.variables,
        imports,
        module_calls,
        arguments,
        attribute_params,
        backend,
        state_blocks,
        user_functions: ctx.user_functions,
    })
}

/// Parse arguments block
fn parse_arguments_block(
    pair: pest::iterators::Pair<Rule>,
) -> Result<Vec<ArgumentParameter>, ParseError> {
    let mut arguments = Vec::new();
    let ctx = ParseContext::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::arguments_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "arguments block")?
                .as_str()
                .to_string();
            let type_expr = parse_type_expr(next_pair(
                &mut param_inner,
                "type expression",
                "arguments parameter",
            )?)?;

            // Check if the next element is a block form or simple default
            if let Some(next) = param_inner.next() {
                if next.as_rule() == Rule::arguments_param_block {
                    // Block form: parse description and default from attrs
                    let mut description = None;
                    let mut default = None;
                    for attr in next.into_inner() {
                        if attr.as_rule() == Rule::arguments_param_attr {
                            let mut attr_inner = attr.into_inner();
                            if let Some(first) = attr_inner.next() {
                                match first.as_rule() {
                                    Rule::string => {
                                        // description = "..."
                                        let value = parse_string_value(first, &ctx)?;
                                        if let Value::String(s) = value {
                                            description = Some(s);
                                        }
                                    }
                                    _ => {
                                        // default = expression
                                        default = Some(parse_expression(first, &ctx)?);
                                    }
                                }
                            }
                        }
                    }
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description,
                    });
                } else {
                    // Simple form: the next element is the default expression
                    let default = Some(parse_expression(next, &ctx)?);
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description: None,
                    });
                }
            } else {
                // No default, no block
                arguments.push(ArgumentParameter {
                    name,
                    type_expr,
                    default: None,
                    description: None,
                });
            }
        }
    }

    Ok(arguments)
}

/// Parse attributes block
fn parse_attributes_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Vec<AttributeParameter>, ParseError> {
    let mut attribute_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::attributes_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "attributes block")?
                .as_str()
                .to_string();

            // Check whether the next inner pair is a type_expr or an expression
            let next = next_pair(
                &mut param_inner,
                "type or expression",
                "attributes parameter",
            )?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                // Has explicit type annotation: name: type = expr
                let type_expr = Some(parse_type_expr(next)?);
                let expr = next_pair(&mut param_inner, "value expression", "attributes parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                // No type annotation: name = expr
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            attribute_params.push(AttributeParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(attribute_params)
}

/// Parse type expression
fn parse_type_expr(pair: pest::iterators::Pair<Rule>) -> Result<TypeExpr, ParseError> {
    let inner = first_inner(pair, "type", "type expression")?;
    match inner.as_rule() {
        Rule::type_simple => match inner.as_str() {
            "string" => Ok(TypeExpr::String),
            "bool" => Ok(TypeExpr::Bool),
            "int" => Ok(TypeExpr::Int),
            "float" => Ok(TypeExpr::Float),
            other => Ok(TypeExpr::Simple(other.to_string())),
        },
        Rule::type_generic => {
            // Get the full string representation to determine if it's list or map
            let full_str = inner.as_str();
            let is_list = full_str.starts_with("list");

            // Get the inner type expression
            let mut generic_inner = inner.into_inner();
            let inner_type = parse_type_expr(next_pair(
                &mut generic_inner,
                "inner type",
                "generic type expression",
            )?)?;

            if is_list {
                Ok(TypeExpr::List(Box::new(inner_type)))
            } else {
                Ok(TypeExpr::Map(Box::new(inner_type)))
            }
        }
        Rule::type_ref => {
            // Parse resource_type_path directly (e.g., aws.vpc)
            let mut ref_inner = inner.into_inner();
            let path_str = next_pair(&mut ref_inner, "resource type path", "type ref")?.as_str();
            let path = ResourceTypePath::parse(path_str).ok_or_else(|| {
                ParseError::InvalidResourceType(format!("Invalid resource type path: {}", path_str))
            })?;
            Ok(TypeExpr::Ref(path))
        }
        _ => Ok(TypeExpr::String),
    }
}

/// Parse import expression (RHS of `let name = import "path"`)
fn parse_import_expr(
    pair: pest::iterators::Pair<Rule>,
    binding_name: &str,
) -> Result<ImportStatement, ParseError> {
    let mut inner = pair.into_inner();
    let path = parse_string(next_pair(&mut inner, "import path", "import expression")?);

    Ok(ImportStatement {
        path,
        alias: binding_name.to_string(),
    })
}

/// Parse module call
fn parse_module_call(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ModuleCall, ParseError> {
    let mut inner = pair.into_inner();
    let module_name = next_pair(&mut inner, "module name", "module call")?
        .as_str()
        .to_string();

    let mut arguments = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key = next_pair(&mut arg_inner, "argument name", "module call argument")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut arg_inner, "argument value", "module call argument")?,
                ctx,
            )?;
            arguments.insert(key, value);
        }
    }

    Ok(ModuleCall {
        module_name,
        binding_name: None,
        arguments,
    })
}

/// Result of parsing the RHS of a let binding: (value, resources, module_calls, import)
type LetBindingRhs = (
    Value,
    Vec<Resource>,
    Vec<ModuleCall>,
    Option<ImportStatement>,
);

/// Extended parse_let_binding that also handles module calls, imports, and for expressions
#[allow(clippy::type_complexity)]
fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<
    (
        String,
        Value,
        Vec<Resource>,
        Vec<ModuleCall>,
        Option<ImportStatement>,
    ),
    ParseError,
> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "binding name", "let binding")?
        .as_str()
        .to_string();
    let expr_pair = next_pair(&mut inner, "expression", "let binding")?;

    // Check if it's a module call, resource expression, import, or for expression
    let (value, expanded_resources, module_calls, maybe_import) =
        parse_expression_with_resource_or_module(expr_pair, ctx, &name)?;

    Ok((name, value, expanded_resources, module_calls, maybe_import))
}

/// Parse expression with potential resource, module call, or import
fn parse_expression_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "expression", "expression with resource or module")?;
    parse_pipe_expr_with_resource_or_module(inner, ctx, binding_name)
}

fn parse_pipe_expr_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();
    let primary = next_pair(&mut inner, "primary expression", "pipe expression")?;
    let (mut value, expanded_resources, module_calls, maybe_import) =
        parse_primary_with_resource_or_module(primary, ctx, binding_name)?;

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        let mut args = extra_args;
        args.push(value);

        value = Value::FunctionCall {
            name: func_name,
            args,
        };
    }

    Ok((value, expanded_resources, module_calls, maybe_import))
}

fn parse_primary_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "value", "primary expression")?;

    match inner.as_rule() {
        Rule::read_resource_expr => {
            let resource = parse_read_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, vec![resource], vec![], None))
        }
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, vec![resource], vec![], None))
        }
        Rule::import_expr => {
            let import = parse_import_expr(inner, binding_name)?;
            let value = Value::String(format!("${{import:{}}}", import.path));
            Ok((value, vec![], vec![], Some(import)))
        }
        Rule::for_expr => {
            let (resources, module_calls) = parse_for_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{for:{}}}", binding_name));
            Ok((ref_value, resources, module_calls, None))
        }
        Rule::if_expr => parse_if_expr(inner, ctx, binding_name),
        Rule::module_call => {
            let call = parse_module_call(inner, ctx)?;
            let value = Value::String(format!("${{module:{}}}", call.module_name));
            Ok((value, vec![], vec![call], None))
        }
        Rule::function_call => {
            // Check if this is a resource-generating user function call
            let mut fc_inner = inner.clone().into_inner();
            let func_name = fc_inner
                .next()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            if let Some(user_fn) = ctx.user_functions.get(&func_name)
                && user_fn.body.is_resource()
            {
                // Parse args and evaluate as resource
                let mut fc_inner = inner.into_inner();
                let _name = fc_inner.next(); // skip function name
                let args: Result<Vec<Value>, ParseError> =
                    fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
                let args = args?;
                let user_fn = user_fn.clone();
                let resource =
                    evaluate_user_function_as_resource(&user_fn, &args, ctx, binding_name)?;
                let ref_value = Value::String(format!("${{{}}}", binding_name));
                return Ok((ref_value, vec![resource], vec![], None));
            }
            let value = parse_primary_value(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
        _ => {
            let value = parse_primary_value(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
    }
}

/// Binding pattern for a for expression
enum ForBinding {
    /// Simple: `for x in ...`
    Simple(String),
    /// Indexed: `for (i, x) in ...`
    Indexed(String, String),
    /// Map: `for k, v in ...`
    Map(String, String),
}

/// Result of parsing a for expression body: either a resource or a module call
enum ForBodyResult {
    Resource(Resource),
    ModuleCall(ModuleCall),
}

/// Parse a for expression and expand it into individual resources and/or module calls.
///
/// `for x in list { resource_expr }` expands to resources with addresses like
/// `binding[0]`, `binding[1]`, etc.
///
/// `for k, v in map { resource_expr }` expands to resources with addresses like
/// `binding["key1"]`, `binding["key2"]`, etc.
///
/// When the body is a module call, each iteration produces a module call with
/// a binding name like `binding[0]` or `binding["key"]`.
fn parse_for_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Vec<Resource>, Vec<ModuleCall>), ParseError> {
    let mut inner = pair.into_inner();

    // Parse the binding pattern
    let binding_pair = next_pair(&mut inner, "for binding", "for expression")?;
    let binding = parse_for_binding(binding_pair)?;

    // Parse the iterable expression
    let iterable_pair = next_pair(&mut inner, "iterable", "for expression")?;
    let iterable = parse_for_iterable(iterable_pair, ctx)?;

    // Parse the body (we'll re-parse it for each iteration)
    let body_pair = next_pair(&mut inner, "body", "for expression")?;

    let mut resources = Vec::new();
    let mut module_calls = Vec::new();

    let collect = |result: ForBodyResult,
                   resources: &mut Vec<Resource>,
                   module_calls: &mut Vec<ModuleCall>| {
        match result {
            ForBodyResult::Resource(r) => resources.push(r),
            ForBodyResult::ModuleCall(c) => module_calls.push(c),
        }
    };

    // Expand based on iterable type
    match (&binding, &iterable) {
        (ForBinding::Simple(var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                iter_ctx.set_variable(var.clone(), item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Indexed(idx_var, val_var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                iter_ctx.set_variable(idx_var.clone(), Value::Int(i as i64));
                iter_ctx.set_variable(val_var.clone(), item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Map(key_var, val_var), Value::Map(map)) => {
            // Sort keys for deterministic output
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let val = &map[key];
                let address = format!("{}[\"{}\"]", binding_name, key);
                let mut iter_ctx = ctx.clone();
                iter_ctx.set_variable(key_var.clone(), Value::String(key.clone()));
                iter_ctx.set_variable(val_var.clone(), val.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        _ => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: "for expression: binding pattern does not match iterable type".to_string(),
            });
        }
    }

    Ok((resources, module_calls))
}

/// Parse a for binding pattern
fn parse_for_binding(pair: pest::iterators::Pair<Rule>) -> Result<ForBinding, ParseError> {
    let inner = first_inner(pair, "binding pattern", "for binding")?;
    match inner.as_rule() {
        Rule::for_simple_binding => {
            let name = first_inner(inner, "identifier", "simple binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Simple(name))
        }
        Rule::for_indexed_binding => {
            let mut parts = inner.into_inner();
            let idx = next_pair(&mut parts, "index variable", "indexed binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "indexed binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Indexed(idx, val))
        }
        Rule::for_map_binding => {
            let mut parts = inner.into_inner();
            let key = next_pair(&mut parts, "key variable", "map binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "map binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Map(key, val))
        }
        _ => Err(ParseError::InternalError {
            expected: "for binding pattern".to_string(),
            context: "for expression".to_string(),
        }),
    }
}

/// Parse the iterable part of a for expression
///
/// When the iterable is a function call with all statically-known arguments,
/// the function is eagerly evaluated at parse time. If any argument depends on
/// a runtime value (e.g. ResourceRef), a clear error is returned.
fn parse_for_iterable(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // for_iterable contains function_call | list | variable_ref | "(" expression ")"
    let inner = first_inner(pair, "iterable expression", "for iterable")?;
    let value = parse_primary_value(inner, ctx)?;
    evaluate_static_value(value)
}

/// Check whether a Value is fully static (no runtime dependencies).
fn is_static_value(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => true,
        Value::List(items) => items.iter().all(is_static_value),
        Value::Map(map) => map.values().all(is_static_value),
        Value::FunctionCall { args, .. } => args.iter().all(is_static_value),
        Value::ResourceRef { .. } | Value::Interpolation(_) => false,
        Value::Secret(inner) => is_static_value(inner),
    }
}

/// If `value` is a FunctionCall with all static arguments, eagerly evaluate it.
/// Nested FunctionCalls in arguments are evaluated recursively first.
fn evaluate_static_value(value: Value) -> Result<Value, ParseError> {
    match value {
        Value::FunctionCall { ref name, ref args } => {
            if !is_static_value(&value) {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "for iterable function call '{name}' depends on a runtime value; \
                         all arguments must be statically known at parse time"
                    ),
                });
            }
            // Recursively evaluate any nested FunctionCall arguments
            let evaluated_args: Result<Vec<Value>, ParseError> =
                args.iter().cloned().map(evaluate_static_value).collect();
            let evaluated_args = evaluated_args?;
            crate::builtins::evaluate_builtin(name, &evaluated_args).map_err(|e| {
                ParseError::InvalidExpression {
                    line: 0,
                    message: format!("for iterable function call '{name}' failed: {e}"),
                }
            })
        }
        other => Ok(other),
    }
}

/// Parse the body of a for expression and produce a single resource or module call
fn parse_for_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    address: &str,
) -> Result<ForBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::for_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "for local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "for local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(resource));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(resource));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(address.to_string());
                return Ok(ForBodyResult::ModuleCall(call));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression or module call".to_string(),
        context: "for body".to_string(),
    })
}

/// Result of parsing an if expression body: a resource, a module call, or a value
enum IfBodyResult {
    Resource(Resource),
    ModuleCall(ModuleCall),
    Value(Value),
}

/// Parse an if expression and conditionally include resources/module calls/values.
///
/// `if condition { body }` includes the body when condition is true.
/// `if condition { body } else { body }` selects one branch.
///
/// The condition must evaluate to a static Bool value at parse time.
fn parse_if_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        // Use the if branch
        parse_if_body_to_rhs(if_body_pair, ctx, binding_name)
    } else if let Some(else_pair) = else_body_pair {
        // Use the else branch
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_to_rhs(else_body, ctx, binding_name)
    } else {
        // No else clause and condition is false: produce nothing
        let ref_value = Value::String(format!("${{if:{}}}", binding_name));
        Ok((ref_value, vec![], vec![], None))
    }
}

/// Parse an if/else body and convert the result to a LetBindingRhs
fn parse_if_body_to_rhs(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let result = parse_if_body(pair, ctx, binding_name)?;
    match result {
        IfBodyResult::Resource(r) => {
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, vec![r], vec![], None))
        }
        IfBodyResult::ModuleCall(c) => {
            let value = Value::String(format!("${{module:{}}}", c.module_name));
            Ok((value, vec![], vec![c], None))
        }
        IfBodyResult::Value(v) => Ok((v, vec![], vec![], None)),
    }
}

/// Parse the body of an if expression and produce a resource, module call, or value
fn parse_if_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<IfBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(resource));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(resource));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(binding_name.to_string());
                return Ok(IfBodyResult::ModuleCall(call));
            }
            Rule::expression => {
                let value = parse_expression(inner, &local_ctx)?;
                return Ok(IfBodyResult::Value(value));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression, module call, or value expression".to_string(),
        context: "if body".to_string(),
    })
}

/// Parse an if/else expression in value position (attribute values, not let bindings).
///
/// Unlike `parse_if_expr()` which returns `LetBindingRhs` (resources, module calls, or values),
/// this function only returns `Value`. The condition must be a static Bool.
/// An else clause is required when the condition is false (a value must always be determined).
fn parse_if_value_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if value expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if value expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        parse_if_body_value(if_body_pair, ctx)
    } else if let Some(else_pair) = else_body_pair {
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_value(else_body, ctx)
    } else {
        Err(ParseError::InvalidExpression {
            line: 0,
            message: "if expression in value position requires an else clause \
                      when condition is false"
                .to_string(),
        })
    }
}

/// Parse the body of an if expression in value position and return only the value.
fn parse_if_body_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::expression => {
                return parse_expression(inner, &local_ctx);
            }
            Rule::resource_expr | Rule::read_resource_expr | Rule::module_call => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: "resource expressions and module calls cannot be used \
                              in if value expressions; use a let binding instead"
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "value expression".to_string(),
        context: "if value expression body".to_string(),
    })
}

fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "provider name", "provider block")?
        .as_str()
        .to_string();

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "provider block")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "provider block")?,
                ctx,
            )?;
            attributes.insert(key, value);
        }
    }

    // Extract default_tags from attributes if present
    let default_tags = if let Some(Value::Map(tags)) = attributes.remove("default_tags") {
        tags
    } else {
        HashMap::new()
    };

    Ok(ProviderConfig {
        name,
        attributes,
        default_tags,
    })
}

/// Split a namespaced identifier (e.g., "awscc.ec2.vpc") into (provider, resource_type)
fn split_namespaced_id(namespaced: &str) -> (String, String) {
    let parts: Vec<&str> = namespaced.split('.').collect();
    if parts.len() >= 2 {
        (parts[0].to_string(), parts[1..].join("."))
    } else {
        (String::new(), namespaced.to_string())
    }
}

/// Parse a resource address: `provider.service.type "name"`
fn parse_resource_address(pair: pest::iterators::Pair<Rule>) -> Result<ResourceId, ParseError> {
    let mut inner = pair.into_inner();
    let namespaced = next_pair(&mut inner, "namespaced id", "resource address")?
        .as_str()
        .to_string();
    let name_pair = next_pair(&mut inner, "resource name", "resource address")?;
    // The name is a string literal - extract value from quotes
    let name = parse_string_literal(name_pair)?;

    // Split namespaced id into provider and resource_type
    let (provider, resource_type) = split_namespaced_id(&namespaced);

    Ok(ResourceId::with_provider(provider, resource_type, name))
}

/// Parse a string token into its literal value (without quotes).
/// Only handles plain strings (no interpolation).
fn parse_string_literal(pair: pest::iterators::Pair<Rule>) -> Result<String, ParseError> {
    let mut result = String::new();
    for part in pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            for inner in part.into_inner() {
                if inner.as_rule() == Rule::string_literal {
                    result.push_str(inner.as_str());
                }
            }
        }
    }
    Ok(result)
}

/// Parse an import state block
fn parse_import_state_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut to: Option<ResourceId> = None;
    let mut id: Option<String> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::import_state_attr {
            let inner = first_inner(attr, "import attribute", "import block")?;
            match inner.as_rule() {
                Rule::import_to_attr => {
                    let addr = first_inner(inner, "resource address", "import to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                Rule::import_id_attr => {
                    let str_pair = first_inner(inner, "string", "import id")?;
                    id = Some(parse_string_literal(str_pair)?);
                }
                _ => {}
            }
        }
    }

    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'to' attribute".to_string(),
    })?;
    let id = id.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'id' attribute".to_string(),
    })?;

    Ok(StateBlock::Import { to, id })
}

/// Parse a removed block
fn parse_removed_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::removed_attr {
            let addr = first_inner(attr, "resource address", "removed from")?;
            from = Some(parse_resource_address(addr)?);
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "removed block requires 'from' attribute".to_string(),
    })?;

    Ok(StateBlock::Removed { from })
}

/// Parse a moved block
fn parse_moved_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;
    let mut to: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::moved_attr {
            let inner = first_inner(attr, "moved attribute", "moved block")?;
            match inner.as_rule() {
                Rule::moved_from_attr => {
                    let addr = first_inner(inner, "resource address", "moved from")?;
                    from = Some(parse_resource_address(addr)?);
                }
                Rule::moved_to_attr => {
                    let addr = first_inner(inner, "resource address", "moved to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                _ => {}
            }
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'from' attribute".to_string(),
    })?;
    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'to' attribute".to_string(),
    })?;

    Ok(StateBlock::Moved { from, to })
}

/// Parse a user-defined function definition
fn parse_fn_def(
    pair: pest::iterators::Pair<Rule>,
    _ctx: &ParseContext,
) -> Result<UserFunction, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "function name", "fn_def")?
        .as_str()
        .to_string();

    // Parse parameters (optional)
    let mut params = Vec::new();
    let next = next_pair(&mut inner, "fn_params or fn_body", "fn_def")?;
    let next_token = if next.as_rule() == Rule::fn_params {
        // Parse parameter list
        for param_pair in next.into_inner() {
            if param_pair.as_rule() == Rule::fn_param {
                let mut param_inner = param_pair.into_inner();
                let param_name = next_pair(&mut param_inner, "parameter name", "fn_param")?
                    .as_str()
                    .to_string();
                // Parse optional type annotation (: type_expr)
                let mut param_type = None;
                let mut default = None;
                for remaining in param_inner {
                    match remaining.as_rule() {
                        Rule::type_expr => {
                            param_type = Some(parse_type_expr(remaining)?);
                        }
                        _ => {
                            // This is the default expression
                            let default_ctx = ParseContext::new();
                            default = Some(parse_expression(remaining, &default_ctx)?);
                        }
                    }
                }
                // Validate: required params must come before optional params
                if default.is_none() && params.iter().any(|p: &FnParam| p.default.is_some()) {
                    return Err(ParseError::UserFunctionError(format!(
                        "in function '{name}': required parameter '{param_name}' cannot follow optional parameter"
                    )));
                }
                params.push(FnParam {
                    name: param_name,
                    param_type,
                    default,
                });
            }
        }
        next_pair(&mut inner, "type_expr or fn_body", "fn_def")?
    } else {
        next
    };

    // Parse optional return type annotation (: type_expr)
    let (return_type, body_pair) = if next_token.as_rule() == Rule::type_expr {
        let rt = parse_type_expr(next_token)?;
        let bp = next_pair(&mut inner, "fn_body", "fn_def")?;
        (Some(rt), bp)
    } else {
        (None, next_token)
    };

    // Parse body: fn_local_let* ~ (resource_expr | read_resource_expr | expression)
    let mut local_lets = Vec::new();
    let mut body: Option<UserFunctionBody> = None;

    // Create a context where parameters are registered as variables
    // so that param references in the body are resolved as variable refs
    let mut body_ctx = ParseContext::new();
    for p in &params {
        body_ctx.set_variable(
            p.name.clone(),
            Value::String(format!("__fn_param_{}", p.name)),
        );
    }

    for body_inner in body_pair.into_inner() {
        match body_inner.as_rule() {
            Rule::fn_local_let => {
                let mut let_inner = body_inner.into_inner();
                let let_name = next_pair(&mut let_inner, "let name", "fn_local_let")?
                    .as_str()
                    .to_string();
                let let_expr = parse_expression(
                    next_pair(&mut let_inner, "let expression", "fn_local_let")?,
                    &body_ctx,
                )?;
                body_ctx.set_variable(
                    let_name.clone(),
                    Value::String(format!("__fn_local_{let_name}")),
                );
                local_lets.push((let_name, let_expr));
            }
            Rule::resource_expr => {
                body = Some(UserFunctionBody::Resource(body_inner.as_str().to_string()));
            }
            Rule::read_resource_expr => {
                body = Some(UserFunctionBody::ReadResource(
                    body_inner.as_str().to_string(),
                ));
            }
            _ => {
                // This should be the expression (the body)
                body = Some(UserFunctionBody::Value(parse_expression(
                    body_inner, &body_ctx,
                )?));
            }
        }
    }

    let body = body.ok_or_else(|| ParseError::InternalError {
        expected: "body expression".to_string(),
        context: "fn_def".to_string(),
    })?;

    Ok(UserFunction {
        name,
        params,
        return_type,
        local_lets,
        body,
    })
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
) -> Result<(ParseContext, HashMap<String, Value>), ParseError> {
    let fn_name = &func.name;

    // Check recursion
    if ctx.evaluating_functions.contains(fn_name) {
        return Err(ParseError::RecursiveFunction(fn_name.clone()));
    }

    // Validate argument count
    let required_count = func.params.iter().filter(|p| p.default.is_none()).count();
    let max_count = func.params.len();
    if args.len() < required_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at least {required_count} argument(s), got {}",
            args.len()
        )));
    }
    if args.len() > max_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at most {max_count} argument(s), got {}",
            args.len()
        )));
    }

    // Build substitution map: param_name -> value, and type-check annotated params
    let mut substitutions: HashMap<String, Value> = HashMap::new();
    for (i, param) in func.params.iter().enumerate() {
        let value = if i < args.len() {
            args[i].clone()
        } else {
            param.default.clone().unwrap()
        };
        // Type-check if the parameter has a type annotation
        if let Some(ref type_expr) = param.param_type {
            check_fn_arg_type(fn_name, &param.name, type_expr, &value)?;
        }
        substitutions.insert(param.name.clone(), value);
    }

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.clone();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    for (let_name, let_expr) in &func.local_lets {
        let substituted = substitute_fn_params(let_expr, &substitutions);
        let evaluated = try_evaluate_fn_value(substituted, &child_ctx)?;
        child_ctx.set_variable(let_name.clone(), evaluated.clone());
        substitutions.insert(let_name.clone(), evaluated);
    }

    Ok((child_ctx, substitutions))
}

/// Check that a function argument matches the declared parameter type.
/// Resource type annotations (TypeExpr::Ref) are parsed but not validated at call site.
fn check_fn_arg_type(
    fn_name: &str,
    param_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
) -> Result<(), ParseError> {
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        TypeExpr::Int => matches!(value, Value::Int(_)),
        TypeExpr::Float => matches!(value, Value::Float(_)),
        TypeExpr::Bool => matches!(value, Value::Bool(_)),
        TypeExpr::List(_) => matches!(value, Value::List(_)),
        TypeExpr::Map(_) => matches!(value, Value::Map(_)),
        // Simple types (cidr, ipv4_address, arn, etc.) are string subtypes at runtime
        TypeExpr::Simple(_) => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        // Resource type refs: parsed but not validated (see issue guide)
        TypeExpr::Ref(_) => true,
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': parameter '{param_name}' expects type '{type_expr}', got {actual_type}"
        )));
    }
    Ok(())
}

/// Check that a function's return value matches the declared return type.
fn check_fn_return_type(
    fn_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
) -> Result<(), ParseError> {
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        TypeExpr::Int => matches!(value, Value::Int(_)),
        TypeExpr::Float => matches!(value, Value::Float(_)),
        TypeExpr::Bool => matches!(value, Value::Bool(_)),
        TypeExpr::List(_) => matches!(value, Value::List(_)),
        TypeExpr::Map(_) => matches!(value, Value::Map(_)),
        // Simple types (cidr, ipv4_address, arn, etc.) are string subtypes at runtime
        TypeExpr::Simple(_) => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        // Resource type refs: not applicable for value functions
        TypeExpr::Ref(_) => true,
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': return type '{type_expr}' does not match actual return value of type {actual_type}"
        )));
    }
    Ok(())
}

/// Return a human-readable type name for a Value
fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::ResourceRef { .. } => "resource reference",
        Value::Interpolation(_) => "string",
        Value::FunctionCall { .. } => "function call",
        Value::Secret(_) => "secret",
    }
}

/// Evaluate a user-defined function call by substituting arguments into the body
fn evaluate_user_function(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let (child_ctx, substitutions) = prepare_user_function_call(func, args, ctx)?;

    match &func.body {
        UserFunctionBody::Value(body) => {
            let substituted_body = substitute_fn_params(body, &substitutions);
            let result = try_evaluate_fn_value(substituted_body, &child_ctx)?;
            // Check return type if annotated
            if let Some(ref return_type) = func.return_type {
                check_fn_return_type(&func.name, return_type, &result)?;
            }
            Ok(result)
        }
        UserFunctionBody::Resource(_) | UserFunctionBody::ReadResource(_) => {
            Err(ParseError::UserFunctionError(format!(
                "function '{}' returns a resource, not a value; use it in a let binding",
                func.name
            )))
        }
    }
}

/// Evaluate a resource-generating user-defined function call.
/// Re-parses the resource expression source with substituted parameter values.
fn evaluate_user_function_as_resource(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let (mut child_ctx, substitutions) = prepare_user_function_call(func, args, ctx)?;

    // Register substituted values in the child context so resource body can reference them.
    // For parameters that point to resource bindings (value = "${binding_name}"),
    // also register the param name as a resource binding so that field access works.
    // Track the mapping from param names to original binding names for ResourceRef fixup.
    let mut param_to_binding: HashMap<String, String> = HashMap::new();
    for (param_name, value) in &substitutions {
        child_ctx.set_variable(param_name.clone(), value.clone());
        if let Value::String(s) = value
            && let Some(ref_name) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
            && let Some(resource) = ctx.resource_bindings.get(ref_name)
        {
            child_ctx.set_resource_binding(param_name.clone(), resource.clone());
            param_to_binding.insert(param_name.clone(), ref_name.to_string());
        }
    }

    let (rule, resource_source) = match &func.body {
        UserFunctionBody::Resource(src) => (Rule::resource_expr, src.as_str()),
        UserFunctionBody::ReadResource(src) => (Rule::read_resource_expr, src.as_str()),
        UserFunctionBody::Value(_) => {
            return Err(ParseError::UserFunctionError(format!(
                "function '{}' returns a value, not a resource",
                func.name
            )));
        }
    };

    // Re-parse the resource expression source with the child context
    // that has parameter values as variables
    let mut parsed = CarinaParser::parse(rule, resource_source).map_err(ParseError::Syntax)?;

    let resource_pair = parsed.next().ok_or_else(|| ParseError::InternalError {
        expected: "resource expression".to_string(),
        context: "fn resource evaluation".to_string(),
    })?;

    let parse_fn = if rule == Rule::resource_expr {
        parse_resource_expr
    } else {
        parse_read_resource_expr
    };
    let mut resource = parse_fn(resource_pair, &child_ctx, binding_name)?;

    // Fix up ResourceRef binding names: replace fn parameter names with
    // the actual outer binding names they refer to
    if !param_to_binding.is_empty() {
        for value in resource.attributes.values_mut() {
            remap_resource_refs(value, &param_to_binding);
        }
    }

    // Check return type if annotated
    if let Some(TypeExpr::Ref(ref expected_path)) = func.return_type {
        let actual_path = ResourceTypePath {
            provider: resource.id.provider.clone(),
            resource_type: resource.id.resource_type.clone(),
        };
        if *expected_path != actual_path {
            return Err(ParseError::UserFunctionError(format!(
                "function '{}': return type '{}' does not match actual resource type '{}'",
                func.name, expected_path, actual_path
            )));
        }
    }

    Ok(resource)
}

/// Recursively remap ResourceRef binding names from fn parameter names to
/// the actual outer binding names they refer to.
fn remap_resource_refs(value: &mut Value, param_to_binding: &HashMap<String, String>) {
    match value {
        Value::ResourceRef { binding_name, .. } => {
            if let Some(actual_name) = param_to_binding.get(binding_name) {
                *binding_name = actual_name.clone();
            }
        }
        Value::List(items) => {
            for item in items {
                remap_resource_refs(item, param_to_binding);
            }
        }
        Value::Map(map) => {
            for v in map.values_mut() {
                remap_resource_refs(v, param_to_binding);
            }
        }
        Value::FunctionCall { args, .. } => {
            for arg in args {
                remap_resource_refs(arg, param_to_binding);
            }
        }
        _ => {}
    }
}

/// Recursively substitute function parameter placeholders with actual values
fn substitute_fn_params(value: &Value, substitutions: &HashMap<String, Value>) -> Value {
    match value {
        Value::String(s) => {
            // Check if this is a parameter placeholder
            if let Some(param_name) = s.strip_prefix("__fn_param_")
                && let Some(sub) = substitutions.get(param_name)
            {
                return sub.clone();
            }
            if let Some(local_name) = s.strip_prefix("__fn_local_")
                && let Some(sub) = substitutions.get(local_name)
            {
                return sub.clone();
            }
            Value::String(s.clone())
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| substitute_fn_params(v, substitutions))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_fn_params(v, substitutions)))
                .collect(),
        ),
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_fn_params(a, substitutions))
                .collect(),
        },
        Value::Interpolation(parts) => Value::Interpolation(
            parts
                .iter()
                .map(|p| match p {
                    crate::resource::InterpolationPart::Expr(v) => {
                        crate::resource::InterpolationPart::Expr(substitute_fn_params(
                            v,
                            substitutions,
                        ))
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        Value::Secret(inner) => Value::Secret(Box::new(substitute_fn_params(inner, substitutions))),
        other => other.clone(),
    }
}

/// Try to evaluate a value (resolve function calls including user-defined ones)
fn try_evaluate_fn_value(value: Value, ctx: &ParseContext) -> Result<Value, ParseError> {
    match value {
        Value::FunctionCall { ref name, ref args } => {
            // First, recursively evaluate arguments
            let evaluated_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| try_evaluate_fn_value(a.clone(), ctx))
                .collect();
            let evaluated_args = evaluated_args?;

            // Try built-in first
            match crate::builtins::evaluate_builtin(name, &evaluated_args) {
                Ok(result) => Ok(result),
                Err(_builtin_err) => {
                    // Try user-defined function
                    if let Some(user_fn) = ctx.user_functions.get(name) {
                        evaluate_user_function(user_fn, &evaluated_args, ctx)
                    } else {
                        // Keep as FunctionCall (may contain unresolved refs)
                        if evaluated_args.iter().all(is_static_value) {
                            Err(ParseError::InvalidExpression {
                                line: 0,
                                message: format!("Unknown function: {name}"),
                            })
                        } else {
                            Ok(Value::FunctionCall {
                                name: name.clone(),
                                args: evaluated_args,
                            })
                        }
                    }
                }
            }
        }
        Value::List(items) => {
            let evaluated: Result<Vec<Value>, ParseError> = items
                .into_iter()
                .map(|v| try_evaluate_fn_value(v, ctx))
                .collect();
            Ok(Value::List(evaluated?))
        }
        Value::Map(map) => {
            let evaluated: Result<HashMap<String, Value>, ParseError> = map
                .into_iter()
                .map(|(k, v)| try_evaluate_fn_value(v, ctx).map(|ev| (k, ev)))
                .collect();
            Ok(Value::Map(evaluated?))
        }
        Value::Interpolation(parts) => {
            let evaluated: Result<Vec<crate::resource::InterpolationPart>, ParseError> = parts
                .into_iter()
                .map(|p| match p {
                    crate::resource::InterpolationPart::Expr(v) => {
                        try_evaluate_fn_value(v, ctx).map(crate::resource::InterpolationPart::Expr)
                    }
                    other => Ok(other),
                })
                .collect();
            Ok(Value::Interpolation(evaluated?))
        }
        other => Ok(other),
    }
}

fn parse_backend_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<BackendConfig, ParseError> {
    let mut inner = pair.into_inner();
    let backend_type = next_pair(&mut inner, "backend type", "backend block")?
        .as_str()
        .to_string();

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "backend block")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "backend block")?,
                ctx,
            )?;
            attributes.insert(key, value);
        }
    }

    Ok(BackendConfig {
        backend_type,
        attributes,
    })
}

fn parse_anonymous_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "anonymous resource")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let attributes = parse_block_contents(inner, ctx)?;

    // Anonymous resources get an empty name that will be replaced by a hash-based
    // identifier computed from create-only properties after parsing.
    let resource_name = String::new();

    let mut attributes = attributes;
    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    Ok(Resource {
        id: ResourceId::with_provider(provider, resource_type, resource_name),
        attributes,
        read_only: false,
        lifecycle,
        prefixes: HashMap::new(),
    })
}

/// Parse block contents (attributes, nested blocks, and local let bindings)
/// Nested blocks with the same name are collected into a list.
/// Local let bindings are resolved within the block scope and NOT included in
/// the returned attributes.
fn parse_block_contents(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
) -> Result<HashMap<String, Value>, ParseError> {
    let mut attributes: HashMap<String, Value> = HashMap::new();
    let mut nested_blocks: HashMap<String, Vec<Value>> = HashMap::new();

    // Local scope extends the parent context with block-scoped let bindings
    let mut local_ctx = ctx.clone();

    for content_pair in pairs {
        match content_pair.as_rule() {
            Rule::block_content => {
                let inner = first_inner(content_pair, "block content item", "block content")?;
                match inner.as_rule() {
                    Rule::local_binding => {
                        let mut binding_inner = inner.into_inner();
                        let name =
                            next_pair(&mut binding_inner, "binding name", "local let binding")?
                                .as_str()
                                .to_string();
                        let value = parse_expression(
                            next_pair(&mut binding_inner, "binding value", "local let binding")?,
                            &local_ctx,
                        )?;
                        // Add to local scope only, not to attributes
                        local_ctx.set_variable(name, value);
                    }
                    Rule::attribute => {
                        let mut attr_inner = inner.into_inner();
                        let key = next_pair(&mut attr_inner, "attribute name", "block content")?
                            .as_str()
                            .to_string();
                        let value = parse_expression(
                            next_pair(&mut attr_inner, "attribute value", "block content")?,
                            &local_ctx,
                        )?;
                        attributes.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name = next_pair(&mut block_inner, "block name", "nested block")?
                            .as_str()
                            .to_string();

                        // Recursively parse nested block contents (supports arbitrary depth)
                        let block_attrs = parse_block_contents(block_inner, &local_ctx)?;

                        // Add to the list of blocks with this name
                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            Rule::attribute => {
                let mut attr_inner = content_pair.into_inner();
                let key = next_pair(&mut attr_inner, "attribute name", "block content")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut attr_inner, "attribute value", "block content")?,
                    &local_ctx,
                )?;
                attributes.insert(key, value);
            }
            _ => {}
        }
    }

    // Convert nested blocks to list attributes
    for (name, blocks) in nested_blocks {
        attributes.insert(name, Value::List(blocks));
    }

    Ok(attributes)
}

/// Extract lifecycle configuration from attributes.
/// The parser parses `lifecycle { ... }` as a nested block, which becomes
/// a List of Maps in attributes. We extract it and convert to LifecycleConfig.
fn extract_lifecycle_config(attributes: &mut HashMap<String, Value>) -> LifecycleConfig {
    if let Some(Value::List(blocks)) = attributes.remove("lifecycle") {
        // Take the first lifecycle block (there should only be one)
        if let Some(Value::Map(map)) = blocks.into_iter().next() {
            let force_delete = matches!(map.get("force_delete"), Some(Value::Bool(true)));
            let create_before_destroy =
                matches!(map.get("create_before_destroy"), Some(Value::Bool(true)));
            return LifecycleConfig {
                force_delete,
                create_before_destroy,
            };
        }
    }
    LifecycleConfig::default()
}

fn parse_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "resource expression")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let mut attributes = parse_block_contents(inner, ctx)?;

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));
    // Save binding name (for reference)
    attributes.insert(
        "_binding".to_string(),
        Value::String(binding_name.to_string()),
    );

    Ok(Resource {
        id: ResourceId::with_provider(provider, resource_type, resource_name),
        attributes,
        read_only: false,
        lifecycle,
        prefixes: HashMap::new(),
    })
}

/// Parse a read resource expression (data source): read aws.s3_bucket { ... }
fn parse_read_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "read resource expression")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let mut attributes = parse_block_contents(inner, ctx)?;

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));
    // Save binding name (for reference)
    attributes.insert(
        "_binding".to_string(),
        Value::String(binding_name.to_string()),
    );
    // Mark as data source
    attributes.insert("_data_source".to_string(), Value::Bool(true));

    Ok(Resource {
        id: ResourceId::with_provider(provider, resource_type, resource_name),
        attributes,
        read_only: true,
        lifecycle,
        prefixes: HashMap::new(),
    })
}

fn parse_expression(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let inner = first_inner(pair, "expression body", "expression")?;
    parse_pipe_expr(inner, ctx)
}

fn parse_pipe_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut inner = pair.into_inner();
    let primary = next_pair(&mut inner, "primary expression", "pipe expression")?;
    let mut value = parse_primary_value(primary, ctx)?;

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        // Build args: pipe value is prepended as the last argument
        // For join: `list |> join(sep)` => `join(sep, list)`
        let mut args = extra_args;
        args.push(value);

        // Try to eagerly evaluate user-defined function calls
        if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
            let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
            value = evaluate_user_function(&user_fn, &args, ctx)?;
        } else {
            value = Value::FunctionCall {
                name: func_name,
                args,
            };
        }
    }

    Ok(value)
}

fn parse_primary_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // For primary, get inner content; otherwise process directly
    let inner = if pair.as_rule() == Rule::primary {
        first_inner(pair, "value", "primary expression")?
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::resource_expr => {
            // Resource expressions cannot be used as attribute values (only valid in top-level let bindings)
            Err(ParseError::InvalidExpression {
                line: 0,
                message: "Resource expressions can only be used in let bindings".to_string(),
            })
        }
        Rule::list => {
            let items: Result<Vec<Value>, ParseError> = inner
                .into_inner()
                .map(|item| parse_expression(item, ctx))
                .collect();
            Ok(Value::List(items?))
        }
        Rule::map => {
            let mut map = HashMap::new();
            let mut nested_blocks: HashMap<String, Vec<Value>> = HashMap::new();
            for entry in inner.into_inner() {
                match entry.as_rule() {
                    Rule::map_entry => {
                        let mut entry_inner = entry.into_inner();
                        let key = next_pair(&mut entry_inner, "map key", "map entry")?
                            .as_str()
                            .to_string();
                        let value = parse_expression(
                            next_pair(&mut entry_inner, "map value", "map entry")?,
                            ctx,
                        )?;
                        map.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = entry.into_inner();
                        let block_name =
                            next_pair(&mut block_inner, "block name", "nested block in map")?
                                .as_str()
                                .to_string();
                        let block_attrs = parse_block_contents(block_inner, ctx)?;
                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            for (name, blocks) in nested_blocks {
                map.insert(name, Value::List(blocks));
            }
            Ok(Value::Map(map))
        }
        Rule::namespaced_id => {
            // Namespaced identifier (e.g., aws.Region.ap_northeast_1)
            // or resource reference (e.g., bucket.name)
            // or arguments reference in module context (e.g., arguments.vpc_id)
            let full_str = inner.as_str();
            let parts: Vec<&str> = full_str.split('.').collect();

            if parts.len() == 2 {
                // Two-part identifier: could be resource reference or variable access
                if ctx.get_variable(parts[0]).is_some() && !ctx.is_resource_binding(parts[0]) {
                    // Variable exists but trying to access attribute on non-resource
                    Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "'{}' is not a resource, cannot access attribute '{}'",
                            parts[0], parts[1]
                        ),
                    })
                } else if ctx.is_resource_binding(parts[0]) {
                    // Known resource binding: treat as resource reference
                    Ok(Value::ResourceRef {
                        binding_name: parts[0].to_string(),
                        attribute_name: parts[1].to_string(),
                        field_path: vec![],
                    })
                } else {
                    // Unknown 2-part identifier: could be TypeName.value enum shorthand
                    // Will be resolved during schema validation
                    Ok(Value::String(format!("{}.{}", parts[0], parts[1])))
                }
            } else if ctx.is_resource_binding(parts[0]) {
                // 3+ part identifier where first part is a resource binding:
                // chained field access (e.g., web.network.vpc_id)
                Ok(Value::ResourceRef {
                    binding_name: parts[0].to_string(),
                    attribute_name: parts[1].to_string(),
                    field_path: parts[2..].iter().map(|s| s.to_string()).collect(),
                })
            } else {
                // 3+ part identifier is a namespaced type (aws.Region.ap_northeast_1)
                Ok(Value::String(full_str.to_string()))
            }
        }
        Rule::boolean => {
            let b = inner.as_str() == "true";
            Ok(Value::Bool(b))
        }
        Rule::float => {
            let f: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("invalid float literal: {e}"),
                })?;
            Ok(Value::Float(f))
        }
        Rule::number => {
            let n: i64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("integer literal out of range: {e}"),
                })?;
            Ok(Value::Int(n))
        }
        Rule::string => parse_string_value(inner, ctx),
        Rule::function_call => {
            let mut fc_inner = inner.into_inner();
            let func_name = next_pair(&mut fc_inner, "function name", "function call")?
                .as_str()
                .to_string();
            let args: Result<Vec<Value>, ParseError> =
                fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
            let args = args?;

            // Try to eagerly evaluate user-defined function calls
            if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
                let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
                return evaluate_user_function(&user_fn, &args, ctx);
            }

            Ok(Value::FunctionCall {
                name: func_name,
                args,
            })
        }
        Rule::variable_ref => {
            // variable_ref = { identifier ~ (field_access | index_access)* }
            // field_access = { "." ~ identifier }
            // index_access = { "[" ~ expression ~ "]" }
            let mut parts = inner.into_inner();
            let first_ident = next_pair(&mut parts, "identifier", "variable reference")?.as_str();

            // Collect all access steps (field or index)
            let access_steps: Vec<pest::iterators::Pair<Rule>> = parts.collect();

            if access_steps.is_empty() {
                // Simple variable reference (no access chain)
                match ctx.get_variable(first_ident) {
                    Some(val) => Ok(val.clone()),
                    None => Ok(Value::String(first_ident.to_string())),
                }
            } else {
                // Build binding_name, attribute_name, and field_path from access steps.
                // Index access (e.g., [0] or ["key"]) composes the binding name.
                // Field access after the binding gives attribute_name and field_path.
                let mut binding_name = first_ident.to_string();
                let mut field_names: Vec<String> = Vec::new();
                let mut in_field_phase = false;

                for step in access_steps {
                    match step.as_rule() {
                        Rule::index_access => {
                            if in_field_phase {
                                // Index access after field access is not yet supported
                                // (e.g., a.b[0] — would need runtime list indexing)
                                return Err(ParseError::InvalidExpression {
                                    line: 0,
                                    message: "index access after field access is not supported"
                                        .to_string(),
                                });
                            }
                            // Parse the index expression
                            let index_expr_pair =
                                first_inner(step, "index expression", "index access")?;
                            let index_value = parse_expression(index_expr_pair, ctx)?;
                            // Compose the binding name: name[0] or name["key"]
                            match &index_value {
                                Value::Int(n) => {
                                    binding_name = format!("{}[{}]", binding_name, n);
                                }
                                Value::String(s) => {
                                    binding_name = format!("{}[\"{}\"]", binding_name, s);
                                }
                                other => {
                                    return Err(ParseError::InvalidExpression {
                                        line: 0,
                                        message: format!(
                                            "index access key must be an integer or string, got {:?}",
                                            other
                                        ),
                                    });
                                }
                            }
                        }
                        Rule::field_access => {
                            in_field_phase = true;
                            let field_ident =
                                first_inner(step, "field identifier", "field access")?;
                            field_names.push(field_ident.as_str().to_string());
                        }
                        _ => {}
                    }
                }

                if field_names.is_empty() {
                    // Index access only, no field access (e.g., subnets[0])
                    // Check if the composed binding name is a known variable
                    match ctx.get_variable(&binding_name) {
                        Some(val) => Ok(val.clone()),
                        None => {
                            // Return as ResourceRef with empty attribute_name
                            // (will be resolved later)
                            Ok(Value::ResourceRef {
                                binding_name,
                                attribute_name: String::new(),
                                field_path: vec![],
                            })
                        }
                    }
                } else {
                    let attribute_name = field_names.remove(0);
                    Ok(Value::ResourceRef {
                        binding_name,
                        attribute_name,
                        field_path: field_names,
                    })
                }
            }
        }
        Rule::if_expr => parse_if_value_expr(inner, ctx),
        Rule::expression => parse_expression(inner, ctx),
        _ => Ok(Value::String(inner.as_str().to_string())),
    }
}

fn parse_string_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    use crate::resource::InterpolationPart;

    let mut parts: Vec<InterpolationPart> = Vec::new();
    let mut has_interpolation = false;

    for part in pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            let inner = first_inner(part, "string content", "string_part")?;
            match inner.as_rule() {
                Rule::string_literal => {
                    let s = unescape_string(inner.as_str());
                    parts.push(InterpolationPart::Literal(s));
                }
                Rule::interpolation => {
                    has_interpolation = true;
                    let expr_pair =
                        first_inner(inner, "interpolation expression", "interpolation")?;
                    let value = parse_expression(expr_pair, ctx)?;
                    parts.push(InterpolationPart::Expr(value));
                }
                _ => {}
            }
        }
    }

    if has_interpolation {
        Ok(Value::Interpolation(parts))
    } else {
        // No interpolation — collapse to a plain String
        let s = parts
            .into_iter()
            .map(|p| match p {
                InterpolationPart::Literal(s) => s,
                _ => unreachable!(),
            })
            .collect::<String>();
        Ok(Value::String(s))
    }
}

/// Parse a string rule for use in non-expression contexts (e.g., import paths).
/// This only handles plain strings without interpolation.
fn parse_string(pair: pest::iterators::Pair<Rule>) -> String {
    let mut result = String::new();
    for part in pair.into_inner() {
        if part.as_rule() == Rule::string_part
            && let Some(inner) = part.into_inner().next()
            && inner.as_rule() == Rule::string_literal
        {
            result.push_str(&unescape_string(inner.as_str()));
        }
    }
    result
}

/// Handle escape sequences in string literals
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('$') => result.push('$'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Resolve forward references after the full binding set is known.
///
/// During single-pass parsing, `identifier.member` forms where `identifier` is
/// not yet a known binding are stored as `String("identifier.member")`.
/// This function walks all resource attributes, module call arguments, and attribute
/// parameter values, converting matching strings to `ResourceRef`.
fn resolve_forward_references(
    resource_bindings: &HashMap<String, Resource>,
    resources: &mut [Resource],
    attribute_params: &mut [AttributeParameter],
    module_calls: &mut [ModuleCall],
) {
    for resource in resources.iter_mut() {
        let keys: Vec<String> = resource.attributes.keys().cloned().collect();
        for key in keys {
            if let Some(value) = resource.attributes.remove(&key) {
                let resolved = resolve_forward_ref_in_value(value, resource_bindings);
                resource.attributes.insert(key, resolved);
            }
        }
    }
    for attr_param in attribute_params.iter_mut() {
        if let Some(value) = attr_param.value.take() {
            attr_param.value = Some(resolve_forward_ref_in_value(value, resource_bindings));
        }
    }
    for call in module_calls.iter_mut() {
        let keys: Vec<String> = call.arguments.keys().cloned().collect();
        for key in keys {
            if let Some(value) = call.arguments.remove(&key) {
                let resolved = resolve_forward_ref_in_value(value, resource_bindings);
                call.arguments.insert(key, resolved);
            }
        }
    }
}

/// Recursively resolve forward references in a single Value.
///
/// Strings in `"name.member"` format where `name` is a known resource binding
/// are resolved to `ResourceRef`. This handles forward references that were
/// stored as strings during single-pass parsing.
fn resolve_forward_ref_in_value(
    value: Value,
    resource_bindings: &HashMap<String, Resource>,
) -> Value {
    match value {
        Value::String(ref s) => {
            // A dotted string like "vpc.vpc_id" or "vpc.attr.nested" may be a
            // forward reference that was stored as a string during single-pass
            // parsing. Resolve it to ResourceRef if the first segment is a known
            // resource binding. Parts after the second become field_path.
            let parts: Vec<&str> = s.splitn(3, '.').collect();
            if parts.len() >= 2 && resource_bindings.contains_key(parts[0]) {
                let field_path = parts
                    .get(2)
                    .map(|rest| rest.split('.').map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                return Value::ResourceRef {
                    binding_name: parts[0].to_string(),
                    attribute_name: parts[1].to_string(),
                    field_path,
                };
            }
            value
        }
        Value::List(items) => Value::List(
            items
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, resolve_forward_ref_in_value(v, resource_bindings)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            Value::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => InterpolationPart::Expr(
                            resolve_forward_ref_in_value(v, resource_bindings),
                        ),
                        other => other,
                    })
                    .collect(),
            )
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        },
        other => other,
    }
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    // Save dependency bindings before resolution may change ResourceRef binding names.
    // This preserves direct dependencies that would be lost by recursive resolution
    // (e.g., tgw_attach.transit_gateway_id resolves to tgw.id, losing the tgw_attach dep).
    for resource in &mut parsed.resources {
        let deps = crate::deps::get_resource_dependencies(resource);
        if !deps.is_empty() {
            let dep_list: Vec<Value> = deps.into_iter().map(Value::String).collect();
            resource
                .attributes
                .insert("_dependency_bindings".to_string(), Value::List(dep_list));
        }
    }

    // Build a map of binding_name -> attributes for quick lookup
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &parsed.resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            binding_map.insert(binding_name.clone(), resource.attributes.clone());
        }
    }

    // Register argument parameters so they're recognized as valid bindings
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }

    // Register module call bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved after module expansion.
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }

    // Resolve references in each resource
    for resource in &mut parsed.resources {
        let mut resolved_attrs: HashMap<String, Value> = HashMap::new();

        for (key, value) in &resource.attributes {
            let resolved = resolve_value(value, &binding_map)?;
            resolved_attrs.insert(key.clone(), resolved);
        }

        resource.attributes = resolved_attrs;
    }

    Ok(())
}

fn resolve_value(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Result<Value, ParseError> {
    match value {
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } => match binding_map.get(binding_name) {
            Some(attributes) => match attributes.get(attribute_name) {
                Some(attr_value) => {
                    // Recursively resolve in case the attribute itself is a reference
                    resolve_value(attr_value, binding_map)
                }
                None => {
                    // Attribute not found, keep as reference (might be resolved at runtime)
                    Ok(value.clone())
                }
            },
            None => Err(ParseError::UndefinedVariable(format!(
                "{}.{}",
                binding_name, attribute_name
            ))),
        },
        Value::List(items) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|item| resolve_value(item, binding_map))
                .collect();
            Ok(Value::List(resolved?))
        }
        Value::Map(map) => {
            let mut resolved = HashMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_value(v, binding_map)?);
            }
            Ok(Value::Map(resolved))
        }
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_value(v, binding_map)?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Interpolation(resolved?))
        }
        Value::FunctionCall { name, args } => {
            let resolved_args: Result<Vec<Value>, ParseError> =
                args.iter().map(|a| resolve_value(a, binding_map)).collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(is_static_value);

            match crate::builtins::evaluate_builtin(name, &resolved_args) {
                Ok(result) => Ok(result),
                Err(e) => {
                    if all_args_resolved {
                        // All args are resolved but builtin failed — propagate the error
                        Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("{}(): {}", name, e),
                        })
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::FunctionCall {
                            name: name.clone(),
                            args: resolved_args,
                        })
                    }
                }
            }
        }
        _ => Ok(value.clone()),
    }
}

/// Parse a .crn file and resolve resource references
pub fn parse_and_resolve(input: &str) -> Result<ParsedFile, ParseError> {
    let mut parsed = parse(input)?;
    resolve_resource_refs(&mut parsed)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::InterpolationPart;

    #[test]
    fn parse_provider_block() {
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_resource_with_namespaced_type() {
        let input = r#"
            let my_bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, "my_bucket"); // binding name becomes the resource ID
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
        assert_eq!(
            resource.attributes.get("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_multiple_resources() {
        let input = r#"
            let logs = aws.s3_bucket {
                name = "app-logs"
            }

            let data = aws.s3_bucket {
                name = "app-data"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name, "logs"); // binding name becomes the resource ID
        assert_eq!(result.resources[1].id.name, "data");
    }

    #[test]
    fn parse_variable_and_resource() {
        let input = r#"
            let default_region = aws.Region.ap_northeast_1

            let my_bucket = aws.s3_bucket {
                name = "my-bucket"
                region = default_region
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_full_example() {
        let input = r#"
            # Provider configuration
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            # Variables
            let versioning = true
            let retention_days = 90

            # Resources
            let app_logs = aws.s3_bucket {
                name = "my-app-logs"
                versioning = versioning
                expiration_days = retention_days
            }

            let app_data = aws.s3_bucket {
                name = "my-app-data"
                versioning = versioning
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.resources.len(), 2);
        assert_eq!(
            result.resources[0].attributes.get("versioning"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            result.resources[0].attributes.get("expiration_days"),
            Some(&Value::Int(90))
        );
    }

    #[test]
    fn function_call_is_parsed() {
        let input = r#"
            let my_bucket = aws.s3_bucket {
                name = env("SOME_VAR")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::FunctionCall {
                name: "env".to_string(),
                args: vec![Value::String("SOME_VAR".to_string())],
            })
        );
    }

    #[test]
    fn parse_gcp_resource() {
        let input = r#"
            let my_bucket = gcp.storage.bucket {
                name = "my-gcp-bucket"
                location = gcp.Location.asia_northeast1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id.resource_type, "storage.bucket");
        assert_eq!(result.resources[0].id.provider, "gcp");
        // _provider attribute should NOT be set (provider identity is in ResourceId)
        assert!(!result.resources[0].attributes.contains_key("_provider"));
    }

    #[test]
    fn parse_anonymous_resource() {
        let input = r#"
            aws.s3_bucket {
                name = "my-anonymous-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, ""); // anonymous resources get empty name (computed later)
    }

    #[test]
    fn parse_mixed_resources() {
        let input = r#"
            # Anonymous resource
            aws.s3_bucket {
                name = "anonymous-bucket"
            }

            # Named resource
            let named = aws.s3_bucket {
                name = "named-bucket"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name, ""); // anonymous gets empty name
        assert_eq!(result.resources[1].id.name, "named"); // binding name becomes the resource ID
    }

    #[test]
    fn parse_anonymous_resource_without_name_succeeds() {
        let input = r#"
            aws.s3_bucket {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.resources[0].id.name, ""); // empty name, computed later
    }

    #[test]
    fn parse_resource_reference() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = bucket.name
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Before resolution, the attribute should be a ResourceRef
        let policy = &result.resources[1];
        assert_eq!(
            policy.attributes.get("bucket"),
            Some(&Value::ResourceRef {
                binding_name: "bucket".to_string(),
                attribute_name: "name".to_string(),
                field_path: vec![],
            })
        );
    }

    #[test]
    fn parse_and_resolve_resource_reference() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = bucket.name
                bucket_region = bucket.region
            }
        "#;

        let result = parse_and_resolve(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // After resolution, the attribute should be the actual value
        let policy = &result.resources[1];
        assert_eq!(
            policy.attributes.get("bucket"),
            Some(&Value::String("my-bucket".to_string()))
        );
        assert_eq!(
            policy.attributes.get("bucket_region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_undefined_two_part_identifier_becomes_string() {
        // When a 2-part identifier references an unknown binding,
        // it becomes a String (e.g., "nonexistent.name") for later schema validation
        let input = r#"
            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = nonexistent.name
            }
        "#;

        // Parsing succeeds - unknown identifiers become String
        let result = parse_and_resolve(input);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(
            parsed.resources[0].attributes.get("bucket"),
            Some(&Value::String("nonexistent.name".to_string()))
        );
    }

    #[test]
    fn parse_bare_identifier_becomes_string() {
        // When a bare identifier is not a known variable or binding,
        // it becomes a String for later schema validation (enum resolution)
        let input = r#"
            let vpc = awscc.ec2.vpc {
                instance_tenancy = dedicated
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("instance_tenancy"),
            Some(&Value::String("dedicated".to_string()))
        );
    }

    #[test]
    fn resource_reference_preserves_namespaced_id() {
        // Ensure that aws.Region.ap_northeast_1 is NOT treated as a resource reference
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn namespaced_id_with_digit_segment() {
        // Enum values containing dots (e.g., "ipsec.1") should be parsed
        // as part of a namespaced_id when written as an identifier
        let input = r#"
            let gw = awscc.ec2.vpn_gateway {
                type = awscc.ec2.vpn_gateway.Type.ipsec.1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("type"),
            Some(&Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }

    #[test]
    fn parse_nested_blocks_terraform_style() {
        let input = r#"
            let web_sg = aws.security_group {
                name        = "web-sg"
                region      = aws.Region.ap_northeast_1
                vpc         = "my-vpc"
                description = "Web server security group"

                ingress {
                    protocol  = "tcp"
                    from_port = 80
                    to_port   = 80
                    cidr      = "0.0.0.0/0"
                }

                ingress {
                    protocol  = "tcp"
                    from_port = 443
                    to_port   = 443
                    cidr      = "0.0.0.0/0"
                }

                egress {
                    protocol  = "-1"
                    from_port = 0
                    to_port   = 0
                    cidr      = "0.0.0.0/0"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let sg = &result.resources[0];
        assert_eq!(sg.id.resource_type, "security_group");

        // Check ingress is a list with 2 items
        let ingress = sg.attributes.get("ingress").unwrap();
        if let Value::List(items) = ingress {
            assert_eq!(items.len(), 2);

            // Check first ingress rule
            if let Value::Map(rule) = &items[0] {
                assert_eq!(
                    rule.get("protocol"),
                    Some(&Value::String("tcp".to_string()))
                );
                assert_eq!(rule.get("from_port"), Some(&Value::Int(80)));
            } else {
                panic!("Expected map for ingress rule");
            }
        } else {
            panic!("Expected list for ingress");
        }

        // Check egress is a list with 1 item
        let egress = sg.attributes.get("egress").unwrap();
        if let Value::List(items) = egress {
            assert_eq!(items.len(), 1);
        } else {
            panic!("Expected list for egress");
        }
    }

    #[test]
    fn parse_list_syntax() {
        let input = r#"
            let rt = aws.route_table {
                name   = "public-rt"
                region = aws.Region.ap_northeast_1
                vpc    = "my-vpc"
                routes = [
                    { destination = "0.0.0.0/0", gateway = "my-igw" },
                    { destination = "10.0.0.0/8", gateway = "local" }
                ]
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let rt = &result.resources[0];
        let routes = rt.attributes.get("routes").unwrap();
        if let Value::List(items) = routes {
            assert_eq!(items.len(), 2);

            if let Value::Map(route) = &items[0] {
                assert_eq!(
                    route.get("destination"),
                    Some(&Value::String("0.0.0.0/0".to_string()))
                );
                assert_eq!(
                    route.get("gateway"),
                    Some(&Value::String("my-igw".to_string()))
                );
            } else {
                panic!("Expected map for route");
            }
        } else {
            panic!("Expected list for routes");
        }
    }

    #[test]
    fn parse_directory_module() {
        let input = r#"
            arguments {
                vpc_id: string
                enable_https: bool = true
            }

            attributes {
                sg_id: string = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc_id
            }
        "#;

        let result = parse(input).unwrap();

        // Check arguments
        assert_eq!(result.arguments.len(), 2);
        assert_eq!(result.arguments[0].name, "vpc_id");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert!(result.arguments[0].default.is_none());

        assert_eq!(result.arguments[1].name, "enable_https");
        assert_eq!(result.arguments[1].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[1].default, Some(Value::Bool(true)));

        // Check attribute params
        assert_eq!(result.attribute_params.len(), 1);
        assert_eq!(result.attribute_params[0].name, "sg_id");
        assert_eq!(result.attribute_params[0].type_expr, Some(TypeExpr::String));

        // Check resource has argument reference (lexically scoped)
        assert_eq!(result.resources.len(), 1);
        let sg = &result.resources[0];
        assert_eq!(
            sg.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "vpc_id".to_string(),
                attribute_name: String::new(),
                field_path: vec![],
            })
        );
    }

    #[test]
    fn parse_import_expression() {
        let input = r#"
            let web_tier = import "./modules/web_tier.crn"
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].path, "./modules/web_tier.crn");
        assert_eq!(result.imports[0].alias, "web_tier");
    }

    #[test]
    fn parse_generic_type_expressions() {
        let input = r#"
            arguments {
                ports: list(int)
                tags: map(string)
                cidrs: list(string)
            }

            attributes {
                result: list(string) = items.ids
            }

            let items = aws.item {
                name = "test"
            }
        "#;

        let result = parse(input).unwrap();

        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::Int))
        );
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Map(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.arguments[2].type_expr,
            TypeExpr::List(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::List(Box::new(TypeExpr::String)))
        );
        assert!(result.attribute_params[0].value.is_some());
    }

    #[test]
    fn parse_ref_type_expression() {
        let input = r#"
            arguments {
                vpc: aws.vpc
                enable_https: bool = true
            }

            attributes {
                security_group_id: aws.security_group = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc
            }
        "#;

        let result = parse(input).unwrap();

        // Check ref type argument
        assert_eq!(result.arguments[0].name, "vpc");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc"))
        );
        assert!(result.arguments[0].default.is_none());

        // Check ref type attribute param
        assert_eq!(result.attribute_params[0].name, "security_group_id");
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::Ref(ResourceTypePath::new(
                "aws",
                "security_group"
            )))
        );
    }

    #[test]
    fn parse_ref_type_with_nested_resource_type() {
        let input = r#"
            arguments {
                sg: aws.security_group
                rule: aws.security_group.ingress_rule
            }

            attributes {
                out: string = sg.name
            }
        "#;

        let result = parse(input).unwrap();

        // Single-level resource type
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group"))
        );

        // Nested resource type (security_group.ingress_rule)
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group.ingress_rule"))
        );
    }

    #[test]
    fn parse_attributes_without_type_annotation() {
        let input = r#"
            attributes {
                security_group = sg.id
            }

            let sg = aws.security_group {
                name = "web-sg"
            }
        "#;

        let result = parse(input).unwrap();

        assert_eq!(result.attribute_params.len(), 1);
        assert_eq!(result.attribute_params[0].name, "security_group");
        assert_eq!(result.attribute_params[0].type_expr, None);
        assert!(result.attribute_params[0].value.is_some());
    }

    #[test]
    fn parse_attributes_mixed_typed_and_untyped() {
        let input = r#"
            attributes {
                vpc_id: awscc.ec2.VpcId = vpc.vpc_id
                security_group = sg.id
                subnet_ids: list(string) = subnets.ids
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }

            let sg = aws.security_group {
                name = "web-sg"
            }

            let subnets = aws.subnet {
                vpc_id = vpc.vpc_id
            }
        "#;

        let result = parse(input).unwrap();

        assert_eq!(result.attribute_params.len(), 3);

        // Explicit type
        assert_eq!(result.attribute_params[0].name, "vpc_id");
        assert!(result.attribute_params[0].type_expr.is_some());
        assert!(result.attribute_params[0].value.is_some());

        // No type annotation
        assert_eq!(result.attribute_params[1].name, "security_group");
        assert_eq!(result.attribute_params[1].type_expr, None);
        assert!(result.attribute_params[1].value.is_some());

        // Explicit type
        assert_eq!(result.attribute_params[2].name, "subnet_ids");
        assert_eq!(
            result.attribute_params[2].type_expr,
            Some(TypeExpr::List(Box::new(TypeExpr::String)))
        );
        assert!(result.attribute_params[2].value.is_some());
    }

    #[test]
    fn resource_type_path_parse() {
        // Simple resource type
        let path = ResourceTypePath::parse("aws.vpc").unwrap();
        assert_eq!(path.provider, "aws");
        assert_eq!(path.resource_type, "vpc");

        // Nested resource type
        let path2 = ResourceTypePath::parse("aws.security_group.ingress_rule").unwrap();
        assert_eq!(path2.provider, "aws");
        assert_eq!(path2.resource_type, "security_group.ingress_rule");

        // Invalid (single component)
        assert!(ResourceTypePath::parse("vpc").is_none());
    }

    #[test]
    fn resource_type_path_display() {
        let path = ResourceTypePath::new("aws", "vpc");
        assert_eq!(path.to_string(), "aws.vpc");

        let path2 = ResourceTypePath::new("aws", "security_group.ingress_rule");
        assert_eq!(path2.to_string(), "aws.security_group.ingress_rule");
    }

    #[test]
    fn type_expr_display_with_ref() {
        assert_eq!(TypeExpr::String.to_string(), "string");
        assert_eq!(TypeExpr::Bool.to_string(), "bool");
        assert_eq!(TypeExpr::Int.to_string(), "int");
        assert_eq!(
            TypeExpr::List(Box::new(TypeExpr::String)).to_string(),
            "list(string)"
        );
        assert_eq!(
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")).to_string(),
            "aws.vpc"
        );
    }

    #[test]
    fn parse_float_literal() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test"
                weight = 2.5
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("weight"),
            Some(&Value::Float(2.5))
        );
    }

    #[test]
    fn parse_negative_float_literal() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test"
                offset = -0.5
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("offset"),
            Some(&Value::Float(-0.5))
        );
    }

    #[test]
    fn type_expr_display_float() {
        assert_eq!(TypeExpr::Float.to_string(), "float");
    }

    #[test]
    fn parse_backend_block() {
        let input = r#"
            backend s3 {
                bucket      = "my-carina-state"
                key         = "infra/prod/carina.crnstate"
                region      = aws.Region.ap_northeast_1
                encrypt     = true
                auto_create = true
            }

            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();

        // Check backend
        assert!(result.backend.is_some());
        let backend = result.backend.unwrap();
        assert_eq!(backend.backend_type, "s3");
        assert_eq!(
            backend.attributes.get("bucket"),
            Some(&Value::String("my-carina-state".to_string()))
        );
        assert_eq!(
            backend.attributes.get("key"),
            Some(&Value::String("infra/prod/carina.crnstate".to_string()))
        );
        assert_eq!(
            backend.attributes.get("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
        assert_eq!(backend.attributes.get("encrypt"), Some(&Value::Bool(true)));
        assert_eq!(
            backend.attributes.get("auto_create"),
            Some(&Value::Bool(true))
        );

        // Check provider
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_backend_block_with_resources() {
        let input = r#"
            backend s3 {
                bucket = "my-state"
                key    = "prod/carina.state"
                region = aws.Region.ap_northeast_1
            }

            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3_bucket {
                name       = "my-state"
                versioning = "Enabled"
            }

            aws.ec2.vpc {
                name       = "main-vpc"
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();

        assert!(result.backend.is_some());
        let backend = result.backend.unwrap();
        assert_eq!(backend.backend_type, "s3");
        assert_eq!(
            backend.attributes.get("bucket"),
            Some(&Value::String("my-state".to_string()))
        );

        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.resources.len(), 2);
    }

    #[test]
    fn parse_read_resource_expr() {
        let input = r#"
            let existing = read aws.s3_bucket {
                name = "my-existing-bucket"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, "existing"); // binding name becomes the resource ID
        assert!(resource.read_only);
        assert!(resource.is_data_source());
        assert_eq!(
            resource.attributes.get("_data_source"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn parse_read_resource_without_name_uses_binding() {
        let input = r#"
            let existing = read aws.s3_bucket {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.resources[0].id.name, "existing"); // binding name
    }

    #[test]
    fn parse_read_with_regular_resources() {
        let input = r#"
            # Read existing bucket (data source)
            let existing_bucket = read aws.s3_bucket {
                name = "existing-bucket"
            }

            # Create new bucket that depends on reading the existing one
            let new_bucket = aws.s3_bucket {
                name = "new-bucket"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // First resource is read-only (data source)
        assert!(result.resources[0].read_only);
        assert_eq!(result.resources[0].id.name, "existing_bucket"); // binding name

        // Second resource is a regular resource
        assert!(!result.resources[1].read_only);
        assert_eq!(result.resources[1].id.name, "new_bucket"); // binding name
    }

    #[test]
    fn parse_lifecycle_force_delete() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert!(resource.lifecycle.force_delete);
        // lifecycle should NOT appear in attributes
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_lifecycle_default_when_absent() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(!result.resources[0].lifecycle.force_delete);
    }

    #[test]
    fn parse_lifecycle_anonymous_resource() {
        let input = r#"
            awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(result.resources[0].lifecycle.force_delete);
        assert!(!result.resources[0].attributes.contains_key("lifecycle"));
    }

    /// Regression test for issue #146: anonymous AWSCC resources should not have
    /// a spurious "name" attribute injected into the attributes map.
    #[test]
    fn anonymous_resource_no_spurious_name_attribute() {
        let input = r#"
            awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.name, ""); // anonymous → empty name
        // "name" must NOT appear in attributes unless the user explicitly wrote it
        assert!(
            !resource.attributes.contains_key("name"),
            "Anonymous AWSCC resource should not have 'name' in attributes, but found: {:?}",
            resource.attributes.get("name")
        );
    }

    /// Regression test for issue #146: let-bound AWSCC resources should not have
    /// a spurious "name" attribute injected by the parser.
    #[test]
    fn let_bound_resource_no_spurious_name_attribute() {
        let input = r#"
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.name, "vpc"); // binding name → resource name
        // "name" must NOT appear in attributes (it's only the id.name, not an attribute)
        assert!(
            !resource.attributes.contains_key("name"),
            "Let-bound AWSCC resource should not have 'name' in attributes, but found: {:?}",
            resource.attributes.get("name")
        );
    }

    #[test]
    fn parse_lifecycle_create_before_destroy() {
        let input = r#"
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
                lifecycle {
                    create_before_destroy = true
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert!(resource.lifecycle.create_before_destroy);
        assert!(!resource.lifecycle.force_delete);
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_lifecycle_both_force_delete_and_create_before_destroy() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                    create_before_destroy = true
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert!(resource.lifecycle.force_delete);
        assert!(resource.lifecycle.create_before_destroy);
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_block_syntax_inside_map() {
        let input = r#"
            let role = awscc.iam.role {
                assume_role_policy_document = {
                    version = "2012-10-17"
                    statement {
                        effect    = "Allow"
                        principal = { service = "lambda.amazonaws.com" }
                        action    = "sts:AssumeRole"
                    }
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let role = &result.resources[0];
        let doc = role.attributes.get("assume_role_policy_document").unwrap();
        if let Value::Map(map) = doc {
            assert_eq!(
                map.get("version"),
                Some(&Value::String("2012-10-17".to_string()))
            );
            // statement block becomes a list with one element
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 1);
                if let Value::Map(stmt) = &stmts[0] {
                    assert_eq!(
                        stmt.get("effect"),
                        Some(&Value::String("Allow".to_string()))
                    );
                    assert_eq!(
                        stmt.get("action"),
                        Some(&Value::String("sts:AssumeRole".to_string()))
                    );
                } else {
                    panic!("Expected map for statement");
                }
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for assume_role_policy_document");
        }
    }

    #[test]
    fn parse_multiple_blocks_inside_map() {
        let input = r#"
            let role = awscc.iam.role {
                policy_document = {
                    version = "2012-10-17"
                    statement {
                        effect = "Allow"
                        action = "s3:GetObject"
                    }
                    statement {
                        effect = "Deny"
                        action = "s3:DeleteObject"
                    }
                }
            }
        "#;

        let result = parse(input).unwrap();
        let role = &result.resources[0];
        let doc = role.attributes.get("policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 2);
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for policy_document");
        }
    }

    #[test]
    fn parse_list_syntax_inside_map_still_works() {
        // Backward compatibility: list literal syntax still works
        let input = r#"
            let role = awscc.iam.role {
                assume_role_policy_document = {
                    version = "2012-10-17"
                    statement = [
                        {
                            effect    = "Allow"
                            principal = { service = "lambda.amazonaws.com" }
                            action    = "sts:AssumeRole"
                        }
                    ]
                }
            }
        "#;

        let result = parse(input).unwrap();
        let role = &result.resources[0];
        let doc = role.attributes.get("assume_role_policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 1);
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for assume_role_policy_document");
        }
    }

    #[test]
    fn parse_deeply_nested_blocks() {
        // Test nested blocks at depth 2: resource { outer { inner { ... } } }
        let input = r#"
            let r = aws.test.resource {
                outer {
                    inner {
                        leaf = "value"
                    }
                }
            }
        "#;

        let result = parse(input).unwrap();
        let r = &result.resources[0];

        let outer = r.attributes.get("outer").unwrap();
        if let Value::List(outer_items) = outer {
            assert_eq!(outer_items.len(), 1);
            if let Value::Map(outer_map) = &outer_items[0] {
                let inner = outer_map.get("inner").unwrap();
                if let Value::List(inner_items) = inner {
                    assert_eq!(inner_items.len(), 1);
                    if let Value::Map(inner_map) = &inner_items[0] {
                        assert_eq!(
                            inner_map.get("leaf"),
                            Some(&Value::String("value".to_string()))
                        );
                    } else {
                        panic!("Expected map for inner block");
                    }
                } else {
                    panic!("Expected list for inner");
                }
            } else {
                panic!("Expected map for outer block");
            }
        } else {
            panic!("Expected list for outer");
        }
    }

    #[test]
    fn parse_nested_block_in_map() {
        // Test nested block inside map value: attr = { block { ... } }
        let input = r#"
            let role = aws.iam.role {
                policy_document = {
                    statement {
                        effect = "Allow"
                        action = "s3:GetObject"
                    }
                }
            }
        "#;

        let result = parse(input).unwrap();
        let role = &result.resources[0];

        let doc = role.attributes.get("policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(items) = statement {
                assert_eq!(items.len(), 1);
                if let Value::Map(s) = &items[0] {
                    assert_eq!(s.get("effect"), Some(&Value::String("Allow".to_string())));
                } else {
                    panic!("Expected map for statement");
                }
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for policy_document");
        }
    }

    #[test]
    fn test_find_resource_by_attr() {
        let input = r#"
            aws.s3.bucket {
                bucket = "my-bucket"
            }
            aws.s3.bucket {
                bucket = "other-bucket"
            }
        "#;
        let parsed = parse(input).unwrap();

        assert!(
            parsed
                .find_resource_by_attr("s3.bucket", "bucket", "my-bucket")
                .is_some()
        );
        assert!(
            parsed
                .find_resource_by_attr("s3.bucket", "bucket", "other-bucket")
                .is_some()
        );
        assert!(
            parsed
                .find_resource_by_attr("s3.bucket", "bucket", "no-such")
                .is_none()
        );
        assert!(
            parsed
                .find_resource_by_attr("ec2.vpc", "bucket", "my-bucket")
                .is_none()
        );
    }

    #[test]
    fn parse_integer_overflow_returns_error() {
        // i64::MAX is 9223372036854775807; one more should fail
        let input = r#"
provider aws {
    region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
    name = "test"
    count = 99999999999999999999
}
"#;
        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("integer literal out of range"),
            "expected 'integer literal out of range' error, got: {err}"
        );
    }

    #[test]
    fn pipe_operator_desugars_to_function_call() {
        let input = r#"
            let x = "hello" |> upper()
        "#;
        let result = parse(input).unwrap();
        // "hello" |> upper() desugars to upper("hello")
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::String("hello".to_string())],
            })
        );
    }

    #[test]
    fn pipe_operator_in_attribute_desugars() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test" |> lower()
            }
        "#;
        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::FunctionCall {
                name: "lower".to_string(),
                args: vec![Value::String("test".to_string())],
            })
        );
    }

    #[test]
    fn join_function_call_parsed() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = join("-", ["a", "b", "c"])
            }
        "#;
        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        // At parse time, function calls remain as FunctionCall values
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                        Value::String("c".to_string()),
                    ]),
                ],
            })
        );
    }

    #[test]
    fn pipe_with_join_parsed() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["a", "b", "c"] |> join("-")
            }
        "#;
        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        // ["a", "b", "c"] |> join("-") desugars to join("-", ["a", "b", "c"])
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                        Value::String("c".to_string()),
                    ]),
                ],
            })
        );
    }

    #[test]
    fn join_with_multiple_pipes() {
        // Chain: value |> f1(args) |> f2(args)
        let input = r#"
            let x = ["a", "b"] |> join("-") |> upper()
        "#;
        let result = parse(input).unwrap();
        // Pipe chaining: ["a", "b"] |> join("-") |> upper()
        // => upper(join("-", ["a", "b"]))
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::FunctionCall {
                    name: "join".to_string(),
                    args: vec![
                        Value::String("-".to_string()),
                        Value::List(vec![
                            Value::String("a".to_string()),
                            Value::String("b".to_string()),
                        ]),
                    ],
                }],
            })
        );
    }

    #[test]
    fn function_call_with_no_args() {
        let input = r#"
            let x = foo()
        "#;
        let result = parse(input).unwrap();
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "foo".to_string(),
                args: vec![],
            })
        );
    }

    #[test]
    fn join_resolved_during_resource_ref_resolution() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = join("-", ["my", "bucket", "name"])
            }
        "#;
        let mut result = parse(input).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("my-bucket-name".to_string()))
        );
    }

    #[test]
    fn pipe_join_resolved_during_resource_ref_resolution() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["my", "bucket"] |> join("-")
            }
        "#;
        let mut result = parse(input).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn forward_reference_parsed_as_resource_ref() {
        // Issue #866: Forward references should be resolved as ResourceRef,
        // not silently left as a plain string.
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        let subnet = &result.resources[0];
        // Forward reference vpc.vpc_id should be a ResourceRef, not a plain String
        assert_eq!(
            subnet.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
                field_path: vec![],
            }),
            "Forward reference should be parsed as ResourceRef, got: {:?}",
            subnet.attributes.get("vpc_id")
        );
    }

    #[test]
    fn forward_reference_resolve_works() {
        // Issue #866: parse_and_resolve should work with forward references
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        // parse_and_resolve should not error on forward references
        let result = parse_and_resolve(input);
        assert!(
            result.is_ok(),
            "parse_and_resolve should succeed with forward references, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn forward_reference_unused_binding_detection() {
        // Forward-referenced bindings should be detected as used
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let parsed = parse(input).unwrap();
        let unused = crate::validation::check_unused_bindings(&parsed);
        // vpc is referenced by subnet, so should NOT be unused
        assert!(
            !unused.contains(&"vpc".to_string()),
            "vpc should not be unused, but check_unused_bindings returned: {:?}",
            unused
        );
    }

    #[test]
    fn forward_reference_in_nested_value() {
        // Forward references inside list/map values should also be resolved
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
                tags = [{ vpc_ref = vpc.vpc_id }]
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];
        // Check nested reference in list > map
        if let Some(Value::List(items)) = subnet.attributes.get("tags") {
            if let Some(Value::Map(map)) = items.first() {
                assert_eq!(
                    map.get("vpc_ref"),
                    Some(&Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                        field_path: vec![],
                    }),
                    "Nested forward reference should be resolved"
                );
            } else {
                panic!("Expected map in tags list");
            }
        } else {
            panic!("Expected tags to be a list");
        }
    }

    #[test]
    fn forward_reference_chained_three_parts() {
        // Issue #1259: Chained forward references like "later.attr.nested" should
        // be resolved to ResourceRef with field_path, not left as a plain string.
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.encryption_specification.status
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];
        assert_eq!(
            subnet.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "encryption_specification".to_string(),
                field_path: vec!["status".to_string()],
            }),
            "Chained forward reference should be parsed as ResourceRef with field_path"
        );
    }

    #[test]
    fn forward_reference_chained_four_parts() {
        // Issue #1259: Deep chained forward references like "later.attr.deep.nested"
        // should be resolved to ResourceRef with multiple field_path entries.
        let input = r#"
            let subnet = awscc.ec2.subnet {
                vpc_id     = vpc.config.deep.nested
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];
        assert_eq!(
            subnet.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "config".to_string(),
                field_path: vec!["deep".to_string(), "nested".to_string()],
            }),
            "Deep chained forward reference should have multiple field_path entries"
        );
    }

    #[test]
    fn duplicate_let_binding_resource_produces_error() {
        // Issue #915: Duplicate let bindings should produce an error,
        // not silently overwrite the first binding.
        let input = r#"
            let rt = awscc.ec2.route_table {
                vpc_id = "vpc-123"
            }

            let rt = awscc.ec2.route_table {
                vpc_id = "vpc-456"
            }
        "#;

        let result = parse(input);
        assert!(
            result.is_err(),
            "Duplicate let binding 'rt' should produce an error, but parsing succeeded: {:?}",
            result.unwrap()
        );
        let err = result.unwrap_err();
        match &err {
            ParseError::DuplicateBinding { name, line } => {
                assert_eq!(name, "rt");
                assert_eq!(
                    *line, 6,
                    "Duplicate binding should report the line of the second 'let rt', got line {line}"
                );
            }
            _ => panic!("Expected DuplicateBinding error, got: {err}"),
        }
        let err_str = err.to_string();
        assert!(
            err_str.contains("Duplicate") && err_str.contains("rt"),
            "Error should mention duplicate binding 'rt', got: {err_str}"
        );
    }

    #[test]
    fn duplicate_let_binding_variable_produces_error() {
        // Issue #915: Duplicate variable bindings should also produce an error.
        let input = r#"
            let region = aws.Region.ap_northeast_1
            let region = aws.Region.us_east_1
        "#;

        let result = parse(input);
        assert!(
            result.is_err(),
            "Duplicate let binding 'region' should produce an error, but parsing succeeded: {:?}",
            result.unwrap()
        );
        let err = result.unwrap_err();
        match &err {
            ParseError::DuplicateBinding { name, line } => {
                assert_eq!(name, "region");
                assert_eq!(
                    *line, 3,
                    "Duplicate binding should report the line of the second 'let region', got line {line}"
                );
            }
            _ => panic!("Expected DuplicateBinding error, got: {err}"),
        }
        let err_str = err.to_string();
        assert!(
            err_str.contains("Duplicate") && err_str.contains("region"),
            "Error should mention duplicate binding 'region', got: {err_str}"
        );
    }

    #[test]
    fn distinct_let_bindings_are_accepted() {
        // Sanity check: different binding names should work fine
        let input = r#"
            let rt1 = awscc.ec2.route_table {
                vpc_id = "vpc-123"
            }

            let rt2 = awscc.ec2.route_table {
                vpc_id = "vpc-456"
            }
        "#;

        let result = parse(input);
        assert!(
            result.is_ok(),
            "Distinct let bindings should parse successfully, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().resources.len(), 2);
    }

    #[test]
    fn parse_error_has_internal_error_variant() {
        // Verify the InternalError variant exists and formats correctly
        let err = ParseError::InternalError {
            expected: "identifier".to_string(),
            context: "provider block".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("expected identifier in provider block"),
            "InternalError should format with expected and context, got: {msg}"
        );
    }

    #[test]
    fn parse_slash_slash_comment_standalone() {
        let input = r#"
            // This is a C-style comment
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_slash_slash_comment_inline() {
        let input = r#"
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"  // inline comment
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_mixed_comment_styles() {
        let input = r#"
            # shell-style comment
            // C-style comment
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"  // inline C-style
                tags = { Name = "main" }    # inline shell-style
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_block_comment_single_line() {
        let input = r#"
            /* single line block comment */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_multi_line() {
        let input = r#"
            /*
              Multi-line block comment.
              All content is ignored by the parser.
            */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_nested() {
        let input = r#"
            /* outer
              /* inner comment */
              still commented out
            */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_inline() {
        let input = r#"
            let vpc = awscc.ec2.vpc {
                cidr_block = /* inline block comment */ "10.0.0.0/16"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_block_comment_with_all_comment_styles() {
        let input = r#"
            # shell-style comment
            // C-style comment
            /* block comment */
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"  // inline C-style
                tags = { Name = "main" }    # inline shell-style
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
    }

    #[test]
    fn parse_provider_block_with_default_tags() {
        let input = r#"
            provider awscc {
                region = awscc.Region.ap_northeast_1
                default_tags = {
                    Environment = "production"
                    Team        = "platform"
                    ManagedBy   = "carina"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "awscc");
        // default_tags should be extracted from attributes
        assert!(!result.providers[0].attributes.contains_key("default_tags"));
        assert_eq!(result.providers[0].default_tags.len(), 3);
        assert_eq!(
            result.providers[0].default_tags.get("Environment"),
            Some(&Value::String("production".to_string()))
        );
        assert_eq!(
            result.providers[0].default_tags.get("Team"),
            Some(&Value::String("platform".to_string()))
        );
        assert_eq!(
            result.providers[0].default_tags.get("ManagedBy"),
            Some(&Value::String("carina".to_string()))
        );
    }

    #[test]
    fn parse_provider_block_without_default_tags() {
        let input = r#"
            provider awscc {
                region = awscc.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert!(result.providers[0].default_tags.is_empty());
    }

    #[test]
    fn resolve_resource_refs_with_argument_parameters() {
        let input = r#"
            arguments {
                cidr_block: string
                subnet_cidr: string
                az: string
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = cidr_block
            }

            let subnet = awscc.ec2.subnet {
                vpc_id = vpc.vpc_id
                cidr_block = subnet_cidr
                availability_zone = az
            }

            attributes {
                vpc_id: awscc.ec2.vpc = vpc.vpc_id
            }
        "#;

        // parse_and_resolve should succeed without "Undefined variable" errors
        let result = parse_and_resolve(input);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

        let parsed = result.unwrap();
        assert_eq!(parsed.resources.len(), 2);
        assert_eq!(parsed.arguments.len(), 3);
    }

    #[test]
    fn parse_let_binding_module_call() {
        let input = r#"
            let web_tier = import "./modules/web_tier"

            let web = web_tier {
                vpc = "vpc-123"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.module_calls.len(), 1);

        let call = &result.module_calls[0];
        assert_eq!(call.module_name, "web_tier");
        assert_eq!(call.binding_name, Some("web".to_string()));
        assert_eq!(
            call.arguments.get("vpc"),
            Some(&Value::String("vpc-123".to_string()))
        );
    }

    #[test]
    fn parse_module_call_binding_enables_resource_ref() {
        // After `let web = web_tier { ... }`, `web.security_group` should
        // resolve as ResourceRef.
        let input = r#"
            let web_tier = import "./modules/web_tier"

            let web = web_tier {
                vpc = "vpc-123"
            }

            let sg = awscc.ec2.security_group {
                group_description = "test"
                group_name = web.security_group
            }
        "#;

        let result = parse(input).unwrap();
        let sg = &result.resources[0];
        assert_eq!(
            sg.attributes.get("group_name"),
            Some(&Value::ResourceRef {
                binding_name: "web".to_string(),
                attribute_name: "security_group".to_string(),
                field_path: vec![],
            })
        );
    }

    #[test]
    fn parse_string_interpolation_simple() {
        let input = r#"
            let env = "prod"
            let vpc = aws.ec2.vpc {
                name = "vpc-${env}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("vpc-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_multiple_exprs() {
        let input = r#"
            let env = "prod"
            let region = "us-east-1"
            let vpc = aws.ec2.vpc {
                name = "vpc-${env}-${region}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("vpc-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
                InterpolationPart::Literal("-".to_string()),
                InterpolationPart::Expr(Value::String("us-east-1".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_with_resource_ref() {
        let input = r#"
            let vpc = aws.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }
            let subnet = aws.ec2.subnet {
                name = "subnet-${vpc.vpc_id}"
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[1];
        assert_eq!(
            subnet.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("subnet-".to_string()),
                InterpolationPart::Expr(Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                    field_path: vec![],
                }),
            ]))
        );
    }

    #[test]
    fn parse_string_no_interpolation() {
        // Strings without ${} should remain as plain Value::String
        let input = r#"
            let vpc = aws.ec2.vpc {
                name = "my-vpc"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::String("my-vpc".to_string()))
        );
    }

    #[test]
    fn parse_string_dollar_without_brace() {
        // A $ not followed by { should be literal
        let input = r#"
            let vpc = aws.ec2.vpc {
                name = "price$100"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::String("price$100".to_string()))
        );
    }

    #[test]
    fn parse_string_escaped_interpolation() {
        // \${ should be literal ${
        let input = r#"
            let vpc = aws.ec2.vpc {
                name = "literal\${expr}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::String("literal${expr}".to_string()))
        );
    }

    #[test]
    fn parse_string_interpolation_with_bool() {
        let input = r#"
            let vpc = aws.ec2.vpc {
                name = "enabled-${true}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("enabled-".to_string()),
                InterpolationPart::Expr(Value::Bool(true)),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_with_number() {
        let input = r#"
            let vpc = aws.ec2.vpc {
                name = "port-${8080}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("port-".to_string()),
                InterpolationPart::Expr(Value::Int(8080)),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_only_expr() {
        // String with only interpolation, no literal parts
        let input = r#"
            let name = "prod"
            let vpc = aws.ec2.vpc {
                tag = "${name}"
            }
        "#;

        let result = parse(input).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.attributes.get("tag"),
            Some(&Value::Interpolation(vec![InterpolationPart::Expr(
                Value::String("prod".to_string())
            ),]))
        );
    }

    #[test]
    fn parse_local_let_binding_in_resource_block() {
        let input = r#"
            let subnet = awscc.ec2.subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Local let binding should NOT appear in attributes
        assert!(!subnet.attributes.contains_key("name"));

        // The local binding value should be resolved in subsequent attributes
        assert_eq!(
            subnet.attributes.get("tag_name"),
            Some(&Value::String("my-subnet".to_string()))
        );
        assert_eq!(
            subnet.attributes.get("cidr_block"),
            Some(&Value::String("10.0.1.0/24".to_string()))
        );
    }

    #[test]
    fn parse_local_let_binding_with_interpolation() {
        let input = r#"
            let env = "prod"
            let subnet = awscc.ec2.subnet {
                let name = "app-${env}"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Local binding should resolve outer scope variable in interpolation
        assert_eq!(
            subnet.attributes.get("tag_name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("app-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_local_let_binding_chain() {
        let input = r#"
            let subnet = awscc.ec2.subnet {
                let prefix = "app"
                let name = "${prefix}-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Chained local bindings should resolve correctly
        assert_eq!(
            subnet.attributes.get("tag_name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Expr(Value::String("app".to_string())),
                InterpolationPart::Literal("-subnet".to_string()),
            ]))
        );

        // Local bindings should NOT appear in attributes
        assert!(!subnet.attributes.contains_key("prefix"));
        assert!(!subnet.attributes.contains_key("name"));
    }

    #[test]
    fn parse_local_let_binding_with_function_call() {
        let input = r#"
            let subnet = awscc.ec2.subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = upper(name)
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Local binding used inside function call
        assert_eq!(
            subnet.attributes.get("tag_name"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::String("my-subnet".to_string())],
            })
        );
    }

    #[test]
    fn parse_local_let_binding_in_anonymous_resource() {
        let input = r#"
            awscc.ec2.subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Local let binding should work in anonymous resources too
        assert!(!subnet.attributes.contains_key("name"));
        assert_eq!(
            subnet.attributes.get("tag_name"),
            Some(&Value::String("my-subnet".to_string()))
        );
    }

    #[test]
    fn parse_local_let_binding_in_nested_block() {
        let input = r#"
            let subnet = awscc.ec2.subnet {
                let env = "prod"
                cidr_block = "10.0.1.0/24"
                tags {
                    Name = env
                }
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[0];

        // Local binding should be visible in nested blocks
        if let Some(Value::List(tags_list)) = subnet.attributes.get("tags") {
            if let Some(Value::Map(tags)) = tags_list.first() {
                assert_eq!(tags.get("Name"), Some(&Value::String("prod".to_string())));
            } else {
                panic!("Expected Map in tags list");
            }
        } else {
            panic!("Expected tags attribute as List");
        }
    }

    #[test]
    fn parse_for_expression_over_list() {
        let input = r#"
            let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input).unwrap();
        // for expression expands to individual resources at parse time
        assert_eq!(result.resources.len(), 2);

        // Resources should be addressed as subnets[0] and subnets[1]
        assert_eq!(result.resources[0].id.name, "subnets[0]");
        assert_eq!(result.resources[1].id.name, "subnets[1]");

        // Each resource should have the loop variable substituted
        assert_eq!(
            result.resources[0].attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string()))
        );
        assert_eq!(
            result.resources[1].attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1c".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_index() {
        let input = r#"
            let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.subnet {
                    availability_zone = az
                    cidr_block = cidr_subnet("10.0.0.0/16", 8, i)
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        assert_eq!(result.resources[0].id.name, "subnets[0]");
        assert_eq!(result.resources[1].id.name, "subnets[1]");

        // Check index variable is substituted
        if let Some(Value::FunctionCall { args, .. }) =
            result.resources[0].attributes.get("cidr_block")
        {
            assert_eq!(args[2], Value::Int(0));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }

        if let Some(Value::FunctionCall { args, .. }) =
            result.resources[1].attributes.get("cidr_block")
        {
            assert_eq!(args[2], Value::Int(1));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }
    }

    #[test]
    fn parse_for_expression_over_map() {
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for name, cidr in cidrs {
                awscc.ec2.vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Map iteration produces map-keyed addresses
        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert!(names.contains(&r#"networks["prod"]"#));
        assert!(names.contains(&r#"networks["staging"]"#));
    }

    #[test]
    fn parse_for_expression_with_local_binding() {
        let input = r#"
            let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
                let cidr = cidr_subnet("10.0.0.0/16", 8, i)
                awscc.ec2.subnet {
                    cidr_block = cidr
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Local binding should be resolved within each iteration
        if let Some(Value::FunctionCall { name, args }) =
            result.resources[0].attributes.get("cidr_block")
        {
            assert_eq!(name, "cidr_subnet");
            assert_eq!(args[2], Value::Int(0));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }
    }

    #[test]
    fn parse_for_expression_with_module_call() {
        let input = r#"
            let web = import "modules/web"

            let envs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let webs = for name, cidr in envs {
                web { vpc_cidr = cidr }
            }
        "#;

        let result = parse(input).unwrap();

        // for expression with module call should produce module calls, not resources
        assert_eq!(result.module_calls.len(), 2);

        // Module calls should have binding names like webs["prod"] and webs["staging"]
        let binding_names: Vec<&str> = result
            .module_calls
            .iter()
            .map(|c| c.binding_name.as_deref().unwrap())
            .collect();
        assert!(binding_names.contains(&r#"webs["prod"]"#));
        assert!(binding_names.contains(&r#"webs["staging"]"#));

        // Each module call should have the loop variable substituted in arguments
        for call in &result.module_calls {
            assert_eq!(call.module_name, "web");
            assert!(call.arguments.contains_key("vpc_cidr"));
        }

        // Verify the argument values are the substituted loop values
        let prod_call = result
            .module_calls
            .iter()
            .find(|c| c.binding_name.as_deref() == Some(r#"webs["prod"]"#))
            .unwrap();
        assert_eq!(
            prod_call.arguments.get("vpc_cidr"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );

        let staging_call = result
            .module_calls
            .iter()
            .find(|c| c.binding_name.as_deref() == Some(r#"webs["staging"]"#))
            .unwrap();
        assert_eq!(
            staging_call.arguments.get("vpc_cidr"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_module_call_over_list() {
        let input = r#"
            let web = import "modules/web"

            let webs = for cidr in ["10.0.0.0/16", "10.1.0.0/16"] {
                web { vpc_cidr = cidr }
            }
        "#;

        let result = parse(input).unwrap();

        // for expression with module call over list
        assert_eq!(result.module_calls.len(), 2);
        assert_eq!(result.resources.len(), 0);

        assert_eq!(
            result.module_calls[0].binding_name.as_deref(),
            Some("webs[0]")
        );
        assert_eq!(
            result.module_calls[1].binding_name.as_deref(),
            Some("webs[1]")
        );

        assert_eq!(
            result.module_calls[0].arguments.get("vpc_cidr"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            result.module_calls[1].arguments.get("vpc_cidr"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn test_chained_field_access_two_levels() {
        // a.b.c should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c"]
        let input = r#"
            let vpc = awscc.ec2.vpc {
                name = "test-vpc"
            }

            awscc.ec2.subnet {
                name = "test-subnet"
                vpc_id = vpc.network.vpc_id
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[1];
        let vpc_id = subnet.attributes.get("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                field_path,
            } => {
                assert_eq!(binding_name, "vpc");
                assert_eq!(attribute_name, "network");
                assert_eq!(field_path, &vec!["vpc_id".to_string()]);
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn test_chained_field_access_three_levels() {
        // a.b.c.d should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c", "d"]
        let input = r#"
            let web = awscc.ec2.vpc {
                name = "test"
            }

            awscc.ec2.subnet {
                name = "test-subnet"
                vpc_id = web.output.network.vpc_id
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = &result.resources[1];
        let vpc_id = subnet.attributes.get("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                field_path,
            } => {
                assert_eq!(binding_name, "web");
                assert_eq!(attribute_name, "output");
                assert_eq!(
                    field_path,
                    &vec!["network".to_string(), "vpc_id".to_string()]
                );
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_integer() {
        // subnets[0].subnet_id should parse as ResourceRef with binding_name="subnets[0]"
        let input = r#"
            let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.subnet {
                    availability_zone = az
                }
            }

            awscc.ec2.route_table {
                name = "test"
                subnet_id = subnets[0].subnet_id
            }
        "#;

        let result = parse(input).unwrap();
        let rt = result.resources.last().expect("route_table resource");
        let subnet_id = rt.attributes.get("subnet_id").expect("subnet_id attribute");
        match subnet_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                field_path,
            } => {
                assert_eq!(binding_name, "subnets[0]");
                assert_eq!(attribute_name, "subnet_id");
                assert!(field_path.is_empty());
            }
            other => panic!("Expected ResourceRef, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_string_key() {
        // networks["prod"].vpc_id should parse as ResourceRef with binding_name=r#networks["prod"]#
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for name, cidr in cidrs {
                awscc.ec2.vpc {
                    cidr_block = cidr
                }
            }

            awscc.ec2.subnet {
                name = "test"
                vpc_id = networks["prod"].vpc_id
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = result.resources.last().expect("subnet resource");
        let vpc_id = subnet.attributes.get("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                field_path,
            } => {
                assert_eq!(binding_name, r#"networks["prod"]"#);
                assert_eq!(attribute_name, "vpc_id");
                assert!(field_path.is_empty());
            }
            other => panic!("Expected ResourceRef, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_chained_fields() {
        // webs["prod"].security_group.id should parse with field_path
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let webs = for name, cidr in cidrs {
                awscc.ec2.vpc {
                    cidr_block = cidr
                }
            }

            awscc.ec2.subnet {
                name = "test"
                sg_id = webs["prod"].security_group.id
            }
        "#;

        let result = parse(input).unwrap();
        let subnet = result.resources.last().expect("subnet resource");
        let sg_id = subnet.attributes.get("sg_id").expect("sg_id attribute");
        match sg_id {
            Value::ResourceRef {
                binding_name,
                attribute_name,
                field_path,
            } => {
                assert_eq!(binding_name, r#"webs["prod"]"#);
                assert_eq!(attribute_name, "security_group");
                assert_eq!(field_path, &vec!["id".to_string()]);
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_block() {
        let input = r#"
            import {
                to = awscc.ec2.vpc "main-vpc"
                id = "vpc-0abc123def456"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Import { to, id } => {
                assert_eq!(to.provider, "awscc");
                assert_eq!(to.resource_type, "ec2.vpc");
                assert_eq!(to.name, "main-vpc");
                assert_eq!(id, "vpc-0abc123def456");
            }
            other => panic!("Expected Import, got {:?}", other),
        }
    }

    #[test]
    fn parse_removed_block() {
        let input = r#"
            removed {
                from = awscc.ec2.vpc "legacy-vpc"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Removed { from } => {
                assert_eq!(from.provider, "awscc");
                assert_eq!(from.resource_type, "ec2.vpc");
                assert_eq!(from.name, "legacy-vpc");
            }
            other => panic!("Expected Removed, got {:?}", other),
        }
    }

    #[test]
    fn parse_moved_block() {
        let input = r#"
            moved {
                from = awscc.ec2.subnet "old-name"
                to   = awscc.ec2.subnet "new-name"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Moved { from, to } => {
                assert_eq!(from.provider, "awscc");
                assert_eq!(from.resource_type, "ec2.subnet");
                assert_eq!(from.name, "old-name");
                assert_eq!(to.provider, "awscc");
                assert_eq!(to.resource_type, "ec2.subnet");
                assert_eq!(to.name, "new-name");
            }
            other => panic!("Expected Moved, got {:?}", other),
        }
    }

    #[test]
    fn parse_for_expression_with_keys_function_call() {
        let input = r#"
            let tags = {
                Name = "web"
                Env  = "prod"
            }

            let resources = for key in keys(tags) {
                awscc.ec2.subnet {
                    name = key
                }
            }
        "#;

        let result = parse(input).unwrap();
        // keys({Name = "web", Env = "prod"}) should evaluate to ["Env", "Name"] (sorted)
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name, "resources[0]");
        assert_eq!(result.resources[1].id.name, "resources[1]");
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("Env".to_string()))
        );
        assert_eq!(
            result.resources[1].attributes.get("name"),
            Some(&Value::String("Name".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_values_function_call() {
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for cidr in values(cidrs) {
                awscc.ec2.vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input).unwrap();
        // values() returns values sorted by key: prod, staging
        assert_eq!(result.resources.len(), 2);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            result.resources[1].attributes.get("cidr_block"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_concat_function_call() {
        let input = r#"
            let networks = for cidr in concat(["10.0.0.0/16"], ["10.1.0.0/16"]) {
                awscc.ec2.vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        // concat(items, base_list) => base_list ++ items
        // So concat(["10.0.0.0/16"], ["10.1.0.0/16"]) => ["10.1.0.0/16", "10.0.0.0/16"]
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
        assert_eq!(
            result.resources[1].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_runtime_function_call_errors() {
        // Function call with runtime-dependent args (ResourceRef) should error
        let input = r#"
            let vpc = awscc.ec2.vpc {
                name = "test"
            }

            let subnets = for key in keys(vpc.tags) {
                awscc.ec2.subnet {
                    name = key
                }
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("runtime"),
            "Expected error about runtime dependency, got: {}",
            err
        );
    }

    // ── if/else expression tests ──

    #[test]
    fn parse_if_true_condition_includes_resource() {
        let input = r#"
            let alarm = if true {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id.name, "alarm");
        assert_eq!(
            result.resources[0].attributes.get("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_if_false_condition_no_resource() {
        let input = r#"
            let alarm = if false {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 0);
    }

    #[test]
    fn parse_if_else_true_uses_if_branch() {
        let input = r#"
            let vpc = if true {
                awscc.ec2.vpc {
                    cidr_block = "10.0.0.0/16"
                }
            } else {
                awscc.ec2.vpc {
                    cidr_block = "172.16.0.0/16"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_false_uses_else_branch() {
        let input = r#"
            let vpc = if false {
                awscc.ec2.vpc {
                    cidr_block = "10.0.0.0/16"
                }
            } else {
                awscc.ec2.vpc {
                    cidr_block = "172.16.0.0/16"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("172.16.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expression() {
        let input = r#"
            let instance_type = if true {
                "m5.xlarge"
            } else {
                "t3.micro"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 0);
        // The binding should be set to the value from the true branch
        // We verify by using the variable in a resource
        let input2 = r#"
            let instance_type = if true {
                "m5.xlarge"
            } else {
                "t3.micro"
            }

            awscc.ec2.instance {
                instance_type = instance_type
            }
        "#;

        let result2 = parse(input2).unwrap();
        assert_eq!(
            result2.resources[0].attributes.get("instance_type"),
            Some(&Value::String("m5.xlarge".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expression_false_branch() {
        let input = r#"
            let instance_type = if false {
                "m5.xlarge"
            } else {
                "t3.micro"
            }

            awscc.ec2.instance {
                instance_type = instance_type
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("instance_type"),
            Some(&Value::String("t3.micro".to_string()))
        );
    }

    #[test]
    fn parse_if_with_variable_condition() {
        let input = r#"
            let enable_monitoring = true

            let alarm = if enable_monitoring {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
    }

    #[test]
    fn parse_if_non_bool_condition_errors() {
        let input = r#"
            let alarm = if "not_a_bool" {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Bool"),
            "Expected error about Bool condition, got: {}",
            err
        );
    }

    #[test]
    fn parse_if_resource_ref_condition_errors() {
        let input = r#"
            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }

            let alarm = if vpc.enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("runtime") || err.contains("statically"),
            "Expected error about runtime dependency, got: {}",
            err
        );
    }

    #[test]
    fn parse_if_with_module_call() {
        let input = r#"
            let web = import "modules/web"

            let monitoring = if true {
                web { vpc_id = "vpc-123" }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.module_calls.len(), 1);
        assert_eq!(result.module_calls[0].module_name, "web");
    }

    #[test]
    fn parse_if_false_with_module_call() {
        let input = r#"
            let web = import "modules/web"

            let monitoring = if false {
                web { vpc_id = "vpc-123" }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.module_calls.len(), 0);
    }

    #[test]
    fn parse_if_with_local_binding() {
        let input = r#"
            let alarm = if true {
                let name = "cpu-high"
                awscc.cloudwatch.alarm {
                    alarm_name = name
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expr_in_attribute_true() {
        let input = r#"
            let is_production = true

            awscc.ec2.vpc {
                cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expr_in_attribute_false() {
        let input = r#"
            let is_production = false

            awscc.ec2.vpc {
                cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("172.16.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_value_expr_no_else_true() {
        // When condition is true and no else, the value is used
        let input = r#"
            awscc.ec2.vpc {
                cidr_block = if true { "10.0.0.0/16" }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_value_expr_no_else_false_errors() {
        // When condition is false and no else, it's an error in value position
        let input = r#"
            awscc.ec2.vpc {
                cidr_block = if false { "10.0.0.0/16" }
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("else"),
            "Expected error about missing else clause, got: {}",
            err
        );
    }

    #[test]
    fn parse_top_level_for_expression() {
        let input = r#"
            for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Each resource should have the loop variable substituted
        assert_eq!(
            result.resources[0].attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string()))
        );
        assert_eq!(
            result.resources[1].attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1c".to_string()))
        );
    }

    #[test]
    fn parse_top_level_if_expression() {
        let input = r#"
            let enabled = true
            if enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_top_level_multiple_for_no_collision() {
        let input = r#"
            for az in ["a", "b"] {
                awscc.ec2.subnet {
                    availability_zone = az
                }
            }
            for name in ["web", "api"] {
                awscc.ec2.security_group {
                    group_name = name
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 4);

        // First for gets _for0, second gets _for1 - no collisions
        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert_eq!(names[0], "_for0[0]");
        assert_eq!(names[1], "_for0[1]");
        assert_eq!(names[2], "_for1[0]");
        assert_eq!(names[3], "_for1[1]");
    }

    #[test]
    fn parse_top_level_if_false_no_resources() {
        let input = r#"
            let enabled = false
            if enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 0);
    }

    #[test]
    fn parse_arguments_block_form_description_only() {
        let input = r#"
            arguments {
                vpc: awscc.ec2.vpc {
                    description = "The VPC to deploy into"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "vpc");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.vpc"))
        );
        assert!(result.arguments[0].default.is_none());
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("The VPC to deploy into")
        );
    }

    #[test]
    fn parse_arguments_block_form_description_and_default() {
        let input = r#"
            arguments {
                port: int {
                    description = "Web server port"
                    default     = 8080
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("Web server port")
        );
    }

    #[test]
    fn parse_arguments_mixed_simple_and_block_form() {
        let input = r#"
            arguments {
                enable_https: bool = true

                vpc: awscc.ec2.vpc {
                    description = "The VPC to deploy into"
                }

                port: int {
                    description = "Web server port"
                    default     = 8080
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 3);

        // Simple form (unchanged)
        assert_eq!(result.arguments[0].name, "enable_https");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[0].default, Some(Value::Bool(true)));
        assert!(result.arguments[0].description.is_none());

        // Block form with description only
        assert_eq!(result.arguments[1].name, "vpc");
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.vpc"))
        );
        assert!(result.arguments[1].default.is_none());
        assert_eq!(
            result.arguments[1].description.as_deref(),
            Some("The VPC to deploy into")
        );

        // Block form with description and default
        assert_eq!(result.arguments[2].name, "port");
        assert_eq!(result.arguments[2].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[2].default, Some(Value::Int(8080)));
        assert_eq!(
            result.arguments[2].description.as_deref(),
            Some("Web server port")
        );
    }

    #[test]
    fn parse_arguments_simple_form_has_no_description() {
        let input = r#"
            arguments {
                vpc_id: string
                port: int = 8080
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 2);
        assert!(result.arguments[0].description.is_none());
        assert!(result.arguments[1].description.is_none());
    }

    #[test]
    fn parse_arguments_block_form_default_only() {
        let input = r#"
            arguments {
                port: int {
                    default = 8080
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
        assert!(result.arguments[0].description.is_none());
    }

    #[test]
    fn parse_arguments_block_form_empty_block() {
        let input = r#"
            arguments {
                port: int {}
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert!(result.arguments[0].default.is_none());
        assert!(result.arguments[0].description.is_none());
    }

    #[test]
    fn parse_arguments_block_form_string_default_not_confused_with_description() {
        let input = r#"
            arguments {
                name: string {
                    description = "Name of the resource"
                    default     = "my-resource"
                }
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "name");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("Name of the resource")
        );
        assert_eq!(
            result.arguments[0].default,
            Some(Value::String("my-resource".to_string()))
        );
    }

    #[test]
    fn env_missing_var_produces_error_at_parse_time() {
        // Use a var name that is extremely unlikely to be set
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3.bucket {
                name = env("CARINA_TEST_NONEXISTENT_VAR_12345")
            }
        "#;

        let result = parse_and_resolve(input);
        assert!(
            result.is_err(),
            "Expected error for missing env var, got: {:?}",
            result
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CARINA_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var name, got: {}",
            err_msg
        );
    }

    #[test]
    fn join_with_resolved_args_still_works() {
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3.bucket {
                name = join("-", ["a", "b", "c"])
            }
        "#;

        let result = parse_and_resolve(input).unwrap();
        let resource = &result.resources[0];
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("a-b-c".to_string())),
        );
    }

    // --- User-defined function tests ---

    #[test]
    fn user_fn_simple_call() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("hello world".to_string())),
        );
    }

    #[test]
    fn user_fn_with_default_param() {
        let input = r#"
            fn tag(env, suffix = "default") {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod")
            }

            let b = aws.s3_bucket {
                name = tag("prod", "web")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("prod-default".to_string())),
        );
        assert_eq!(
            result.resources[1].attributes.get("name"),
            Some(&Value::String("prod-web".to_string())),
        );
    }

    #[test]
    fn user_fn_with_local_let() {
        let input = r#"
            fn subnet_name(env, az) {
                let prefix = join("-", [env, "subnet"])
                join("-", [prefix, az])
            }

            let vpc = aws.s3_bucket {
                name = subnet_name("prod", "a")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("prod-subnet-a".to_string())),
        );
    }

    #[test]
    fn user_fn_calling_builtin() {
        let input = r#"
            fn upper_name(name) {
                upper(name)
            }

            let vpc = aws.s3_bucket {
                name = upper_name("hello")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("HELLO".to_string())),
        );
    }

    #[test]
    fn user_fn_calling_another_fn() {
        let input = r#"
            fn prefix(env) {
                join("-", [env, "app"])
            }

            fn full_name(env, service) {
                join("-", [prefix(env), service])
            }

            let vpc = aws.s3_bucket {
                name = full_name("prod", "web")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("prod-app-web".to_string())),
        );
    }

    #[test]
    fn user_fn_recursive_call_errors() {
        let input = r#"
            fn recurse(x) {
                recurse(x)
            }

            let vpc = aws.s3_bucket {
                name = recurse("hello")
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Recursive function call"),
            "Expected recursive function error, got: {err}"
        );
    }

    #[test]
    fn user_fn_missing_required_arg_errors() {
        let input = r#"
            fn greet(name, title) {
                join(" ", [title, name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expects at least 2"),
            "Expected missing arg error, got: {err}"
        );
    }

    #[test]
    fn user_fn_too_many_args_errors() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world", "extra")
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expects at most 1"),
            "Expected too many args error, got: {err}"
        );
    }

    #[test]
    fn user_fn_shadows_builtin_errors() {
        let input = r#"
            fn join(sep, items) {
                sep
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("shadows a built-in function"),
            "Expected shadow error, got: {err}"
        );
    }

    #[test]
    fn user_fn_duplicate_definition_errors() {
        let input = r#"
            fn greet(name) {
                name
            }

            fn greet(x) {
                x
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate function definition"),
            "Expected duplicate error, got: {err}"
        );
    }

    #[test]
    fn user_fn_stored_in_parsed_file() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }
        "#;

        let result = parse(input).unwrap();
        assert!(result.user_functions.contains_key("greet"));
        let func = &result.user_functions["greet"];
        assert_eq!(func.name, "greet");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].name, "name");
    }

    #[test]
    fn user_fn_no_params() {
        let input = r#"
            fn hello() {
                "hello"
            }

            let vpc = aws.s3_bucket {
                name = hello()
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("hello".to_string())),
        );
    }

    #[test]
    fn user_fn_indirect_recursion_errors() {
        let input = r#"
            fn foo(x) {
                bar(x)
            }

            fn bar(x) {
                foo(x)
            }

            let vpc = aws.s3_bucket {
                name = foo("hello")
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Recursive function call"),
            "Expected recursive function error, got: {err}"
        );
    }

    #[test]
    fn user_fn_required_param_after_optional_errors() {
        let input = r#"
            fn bad(a = "x", b) {
                join("-", [a, b])
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("required parameter") && err.contains("cannot follow optional"),
            "Expected param ordering error, got: {err}"
        );
    }

    #[test]
    fn user_fn_with_pipe_operator() {
        let input = r#"
            fn wrap(prefix, val) {
                join("-", [prefix, val])
            }

            let vpc = aws.s3_bucket {
                name = "world" |> wrap("hello")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("hello-world".to_string())),
        );
    }

    #[test]
    fn user_fn_with_string_interpolation() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = "${greet("world")}-suffix"
            }
        "#;

        // At parse time, fn is evaluated but interpolation is not fully resolved
        let result = parse(input).unwrap();
        let name = result.resources[0].attributes.get("name").unwrap();
        match name {
            Value::Interpolation(parts) => {
                // The greet() call is evaluated to "hello world"
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0],
                    InterpolationPart::Expr(Value::String("hello world".to_string()))
                );
                assert_eq!(parts[1], InterpolationPart::Literal("-suffix".to_string()));
            }
            _ => panic!("Expected Interpolation, got: {:?}", name),
        }
    }

    #[test]
    fn user_fn_returns_resource() {
        let input = r#"
            fn make_bucket(name) {
                aws.s3_bucket {
                    name = name
                }
            }

            let my_bucket = make_bucket("test-bucket")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, "my_bucket");
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("test-bucket".to_string())),
        );
        assert_eq!(
            resource.attributes.get("_binding"),
            Some(&Value::String("my_bucket".to_string())),
        );
    }

    #[test]
    fn user_fn_returns_resource_with_local_let() {
        let input = r#"
            fn tagged_bucket(env) {
                let full_name = join("-", [env, "bucket"])
                aws.s3_bucket {
                    name = full_name
                }
            }

            let prod_bucket = tagged_bucket("prod")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        let resource = &result.resources[0];
        assert_eq!(resource.id.name, "prod_bucket");
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("prod-bucket".to_string())),
        );
    }

    #[test]
    fn user_fn_returns_resource_with_param_substitution() {
        let input = r#"
            fn subnet(vpc_id, cidr, az) {
                awscc.ec2.subnet {
                    vpc_id            = vpc_id
                    cidr_block        = cidr
                    availability_zone = az
                }
            }

            let subnet_a = subnet("vpc-123", "10.0.1.0/24", "ap-northeast-1a")
            let subnet_b = subnet("vpc-123", "10.0.2.0/24", "ap-northeast-1c")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        let subnet_a = &result.resources[0];
        assert_eq!(subnet_a.id.resource_type, "ec2.subnet");
        assert_eq!(subnet_a.id.name, "subnet_a");
        assert_eq!(
            subnet_a.attributes.get("vpc_id"),
            Some(&Value::String("vpc-123".to_string())),
        );
        assert_eq!(
            subnet_a.attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string())),
        );

        let subnet_b = &result.resources[1];
        assert_eq!(subnet_b.id.name, "subnet_b");
        assert_eq!(
            subnet_b.attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1c".to_string())),
        );
    }

    #[test]
    fn user_fn_resource_nested_fn_call() {
        let input = r#"
            fn make_name(prefix) {
                join("-", [prefix, "bucket"])
            }

            fn make_bucket(prefix) {
                aws.s3_bucket {
                    name = make_name(prefix)
                }
            }

            let my_bucket = make_bucket("test")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        let resource = &result.resources[0];
        assert_eq!(resource.id.name, "my_bucket");
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("test-bucket".to_string())),
        );
    }

    #[test]
    fn user_fn_resource_top_level_call() {
        let input = r#"
            fn make_bucket(name) {
                aws.s3_bucket {
                    name = name
                }
            }

            make_bucket("anon-bucket")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(
            resource.attributes.get("name"),
            Some(&Value::String("anon-bucket".to_string())),
        );
    }

    #[test]
    fn user_fn_resource_with_resource_ref_param() {
        let input = r#"
            fn make_subnet(vpc, cidr) {
                awscc.ec2.subnet {
                    vpc_id     = vpc.vpc_id
                    cidr_block = cidr
                }
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }

            let subnet_a = make_subnet(vpc, "10.0.1.0/24")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        let subnet = &result.resources[1];
        assert_eq!(subnet.id.name, "subnet_a");
        assert_eq!(subnet.id.resource_type, "ec2.subnet");
        // vpc.vpc_id should be a ResourceRef
        match subnet.attributes.get("vpc_id") {
            Some(Value::ResourceRef {
                binding_name,
                attribute_name,
                ..
            }) => {
                assert_eq!(binding_name, "vpc");
                assert_eq!(attribute_name, "vpc_id");
            }
            other => panic!("Expected ResourceRef for vpc_id, got: {:?}", other),
        }
    }

    #[test]
    fn user_fn_resource_with_renamed_resource_ref_param() {
        // When fn param name differs from the outer binding name,
        // the resource ref should use the param name in the fn body
        let input = r#"
            fn make_subnet(v, cidr) {
                awscc.ec2.subnet {
                    vpc_id     = v.vpc_id
                    cidr_block = cidr
                }
            }

            let my_vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }

            let subnet_a = make_subnet(my_vpc, "10.0.1.0/24")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        let subnet = &result.resources[1];
        assert_eq!(subnet.id.name, "subnet_a");
        // v.vpc_id should be resolved to my_vpc.vpc_id via forward reference resolution
        // or kept as a resource ref pointing to my_vpc
        match subnet.attributes.get("vpc_id") {
            Some(Value::ResourceRef {
                binding_name,
                attribute_name,
                ..
            }) => {
                // The reference should point to the actual resource (my_vpc)
                assert_eq!(binding_name, "my_vpc");
                assert_eq!(attribute_name, "vpc_id");
            }
            other => panic!("Expected ResourceRef for vpc_id, got: {:?}", other),
        }
    }

    #[test]
    fn user_fn_typed_param_string() {
        let input = r#"
            fn greet(name: string) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("hello world".to_string())),
        );
    }

    #[test]
    fn user_fn_typed_param_type_mismatch() {
        let input = r#"
            fn greet(name: string) {
                name
            }

            let vpc = aws.s3_bucket {
                name = greet(42)
            }
        "#;

        let err = parse(input).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'string'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_typed_param_int() {
        let input = r#"
            fn double(x: int) {
                x
            }

            let vpc = aws.s3_bucket {
                name = double("not_int")
            }
        "#;

        let err = parse(input).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'int'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_typed_param_with_default() {
        let input = r#"
            fn tag(env: string, suffix: string = "default") {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("prod-default".to_string())),
        );
    }

    #[test]
    fn user_fn_mixed_typed_and_untyped() {
        let input = r#"
            fn tag(env, suffix: string) {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod", "web")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("prod-web".to_string())),
        );
    }

    #[test]
    fn user_fn_resource_type_annotation_parsed() {
        // Resource type annotations are parsed but not validated at call site
        let input = r#"
            fn make_subnet(vpc: awscc.ec2.vpc, cidr: string) {
                awscc.ec2.subnet {
                    vpc_id     = vpc.vpc_id
                    cidr_block = cidr
                }
            }

            let vpc = awscc.ec2.vpc {
                cidr_block = "10.0.0.0/16"
            }

            let subnet = make_subnet(vpc, "10.0.1.0/24")
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[1].id.name, "subnet");
    }

    #[test]
    fn user_fn_typed_param_bool_mismatch() {
        let input = r#"
            fn check(flag: bool) {
                flag
            }

            let vpc = aws.s3_bucket {
                name = check("not_bool")
            }
        "#;

        let err = parse(input).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'bool'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_param_type_stored_in_parsed_file() {
        let input = r#"
            fn greet(name: string, count: int) {
                name
            }
        "#;

        let result = parse(input).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.params[0].param_type, Some(TypeExpr::String));
        assert_eq!(func.params[1].param_type, Some(TypeExpr::Int));
    }

    #[test]
    fn user_fn_untyped_param_type_is_none() {
        let input = r#"
            fn greet(name) {
                name
            }
        "#;

        let result = parse(input).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.params[0].param_type, None);
    }

    #[test]
    fn user_fn_return_type_string() {
        let input = r#"
            fn greet(name: string): string {
                name
            }

            let vpc = aws.s3_bucket {
                name = greet("hello")
            }
        "#;

        let result = parse(input).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.return_type, Some(TypeExpr::String));
    }

    #[test]
    fn user_fn_return_type_none_when_omitted() {
        let input = r#"
            fn greet(name) {
                name
            }
        "#;

        let result = parse(input).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.return_type, None);
    }

    #[test]
    fn user_fn_return_type_mismatch_value() {
        let input = r#"
            fn bad(): string {
                42
            }

            let vpc = aws.s3_bucket {
                name = bad()
            }
        "#;

        let err = parse(input).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type"),
            "Expected return type error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_return_type_resource_ref() {
        let input = r#"
            fn make_bucket(): aws.s3_bucket {
                aws.s3_bucket {
                    name = "test"
                }
            }

            let b = make_bucket()
        "#;

        let result = parse(input).unwrap();
        let func = result.user_functions.get("make_bucket").unwrap();
        assert_eq!(
            func.return_type,
            Some(TypeExpr::Ref(ResourceTypePath::new("aws", "s3_bucket")))
        );
    }

    #[test]
    fn user_fn_return_type_resource_mismatch() {
        let input = r#"
            fn make_bucket(): aws.ec2_instance {
                aws.s3_bucket {
                    name = "test"
                }
            }

            let b = make_bucket()
        "#;

        let err = parse(input).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type"),
            "Expected return type error, got: {msg}"
        );
    }

    #[test]
    fn parse_custom_schema_type_in_fn_param() {
        // Custom schema types like cidr, ipv4_address, arn should be accepted as type annotations
        let input = r#"
            fn subnet(vpc: awscc.ec2.vpc, cidr_block: cidr) {
                awscc.ec2.subnet {
                    name     = "test"
                    vpc_id   = vpc.vpc_id
                    cidr_block = cidr_block
                }
            }
        "#;
        let result = parse(input).unwrap();
        let func = result.user_functions.get("subnet").unwrap();
        assert_eq!(func.params[1].name, "cidr_block");
        assert_eq!(
            func.params[1].param_type,
            Some(TypeExpr::Simple("cidr".to_string()))
        );
    }

    #[test]
    fn parse_ipv4_address_type_in_fn_param() {
        let input = r#"
            fn f(addr: ipv4_address) {
                addr
            }
        "#;
        let result = parse(input).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::Simple("ipv4_address".to_string()))
        );
    }

    #[test]
    fn parse_arn_type_in_fn_param() {
        let input = r#"
            fn f(role: arn) {
                role
            }
        "#;
        let result = parse(input).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::Simple("arn".to_string()))
        );
    }

    #[test]
    fn parse_custom_type_in_list_generic() {
        let input = r#"
            fn f(cidrs: list(cidr)) {
                cidrs
            }
        "#;
        let result = parse(input).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::List(Box::new(TypeExpr::Simple(
                "cidr".to_string()
            ))))
        );
    }

    #[test]
    fn parse_custom_type_in_module_arguments() {
        let input = r#"
            arguments {
                vpc_cidr: cidr
                server_ip: ipv4_address
            }

            awscc.ec2.vpc {
                name       = "test"
                cidr_block = vpc_cidr
            }
        "#;
        let result = parse(input).unwrap();
        assert_eq!(result.arguments[0].name, "vpc_cidr");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Simple("cidr".to_string())
        );
        assert_eq!(result.arguments[1].name, "server_ip");
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Simple("ipv4_address".to_string())
        );
    }

    #[test]
    fn parse_custom_type_in_attributes() {
        let input = r#"
            attributes {
                block: cidr = vpc.cidr_block
            }

            let vpc = awscc.ec2.vpc {
                name       = "test"
                cidr_block = "10.0.0.0/16"
            }
        "#;
        let result = parse(input).unwrap();
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::Simple("cidr".to_string()))
        );
    }

    #[test]
    fn type_expr_display_simple() {
        assert_eq!(TypeExpr::Simple("cidr".to_string()).to_string(), "cidr");
        assert_eq!(
            TypeExpr::Simple("ipv4_address".to_string()).to_string(),
            "ipv4_address"
        );
        assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "arn");
    }
}
