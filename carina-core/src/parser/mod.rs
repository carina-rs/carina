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
    /// CIDR block (e.g., "10.0.0.0/16")
    Cidr,
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
            TypeExpr::Cidr => write!(f, "cidr"),
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
}

/// Attribute parameter definition (in `attributes { ... }` block)
#[derive(Debug, Clone)]
pub struct AttributeParameter {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// Import statement
#[derive(Debug, Clone)]
pub struct ImportStatement {
    pub path: String,
    pub alias: String,
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
}

impl ParseContext {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
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
                            Rule::let_binding => {
                                let (line, _) = stmt.as_span().start_pos().line_col();
                                let (name, value, maybe_resource, maybe_module_call, maybe_import) =
                                    parse_let_binding_extended(stmt, &ctx)?;
                                if ctx.variables.contains_key(&name)
                                    || ctx.resource_bindings.contains_key(&name)
                                {
                                    return Err(ParseError::DuplicateBinding { name, line });
                                }
                                ctx.set_variable(name.clone(), value);
                                if let Some(resource) = maybe_resource {
                                    ctx.set_resource_binding(name.clone(), resource.clone());
                                    resources.push(resource);
                                }
                                if let Some(mut call) = maybe_module_call {
                                    call.binding_name = Some(name.clone());
                                    module_calls.push(call);
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
    // declared later) become UnresolvedIdent. Now that we have the full binding set,
    // convert them to ResourceRef.
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
            let default = if let Some(expr) = param_inner.next() {
                Some(parse_expression(expr, &ctx)?)
            } else {
                None
            };
            arguments.push(ArgumentParameter {
                name,
                type_expr,
                default,
            });
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
            "cidr" => Ok(TypeExpr::Cidr),
            _ => Ok(TypeExpr::String), // Default fallback
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

/// Result of parsing the RHS of a let binding: (value, resource, module_call, import)
type LetBindingRhs = (
    Value,
    Option<Resource>,
    Option<ModuleCall>,
    Option<ImportStatement>,
);

/// Extended parse_let_binding that also handles module calls and imports
#[allow(clippy::type_complexity)]
fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<
    (
        String,
        Value,
        Option<Resource>,
        Option<ModuleCall>,
        Option<ImportStatement>,
    ),
    ParseError,
> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "binding name", "let binding")?
        .as_str()
        .to_string();
    let expr_pair = next_pair(&mut inner, "expression", "let binding")?;

    // Check if it's a module call, resource expression, or import expression
    let (value, maybe_resource, maybe_module_call, maybe_import) =
        parse_expression_with_resource_or_module(expr_pair, ctx, &name)?;

    Ok((name, value, maybe_resource, maybe_module_call, maybe_import))
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
    let (mut value, maybe_resource, maybe_module_call, maybe_import) =
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

    Ok((value, maybe_resource, maybe_module_call, maybe_import))
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
            Ok((ref_value, Some(resource), None, None))
        }
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, Some(resource), None, None))
        }
        Rule::import_expr => {
            let import = parse_import_expr(inner, binding_name)?;
            let value = Value::String(format!("${{import:{}}}", import.path));
            Ok((value, None, None, Some(import)))
        }
        Rule::module_call => {
            let call = parse_module_call(inner, ctx)?;
            let value = Value::String(format!("${{module:{}}}", call.module_name));
            Ok((value, None, Some(call), None))
        }
        _ => {
            let value = parse_primary_value(inner, ctx)?;
            Ok((value, None, None, None))
        }
    }
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

        value = Value::FunctionCall {
            name: func_name,
            args,
        };
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
                    })
                } else {
                    // Unknown 2-part identifier: could be TypeName.value enum shorthand
                    // Will be resolved during schema validation
                    Ok(Value::UnresolvedIdent(
                        parts[0].to_string(),
                        Some(parts[1].to_string()),
                    ))
                }
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
            Ok(Value::FunctionCall {
                name: func_name,
                args: args?,
            })
        }
        Rule::variable_ref => {
            // variable_ref can be "identifier" or "identifier.identifier" (member access)
            let mut parts = inner.into_inner();
            let first_ident = next_pair(&mut parts, "identifier", "variable reference")?.as_str();

            if let Some(second_part) = parts.next() {
                // Member access: resource.attribute (argument params are also resource bindings)
                let attr_name = second_part.as_str();

                // Return a ResourceRef that will be resolved/validated later
                Ok(Value::ResourceRef {
                    binding_name: first_ident.to_string(),
                    attribute_name: attr_name.to_string(),
                })
            } else {
                // Simple variable reference
                match ctx.get_variable(first_ident) {
                    Some(val) => Ok(val.clone()),
                    None => {
                        // Unknown identifier: could be a shorthand enum value
                        // Will be resolved during schema validation
                        Ok(Value::UnresolvedIdent(first_ident.to_string(), None))
                    }
                }
            }
        }
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
/// not yet a known binding are stored as `UnresolvedIdent(identifier, Some(member))`.
/// This function walks all resource attributes, module call arguments, and attribute
/// parameter values, converting matching `UnresolvedIdent` to `ResourceRef`.
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
fn resolve_forward_ref_in_value(
    value: Value,
    resource_bindings: &HashMap<String, Resource>,
) -> Value {
    match value {
        Value::UnresolvedIdent(ref name, Some(ref member))
            if resource_bindings.contains_key(name) =>
        {
            Value::ResourceRef {
                binding_name: name.clone(),
                attribute_name: member.clone(),
            }
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
        // UnresolvedIdent is kept as-is for later resolution during schema validation
        Value::UnresolvedIdent(_, _) => Ok(value.clone()),
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

            // Try to evaluate the function if all args are resolved
            match crate::builtins::evaluate_builtin(name, &resolved_args) {
                Ok(result) => Ok(result),
                Err(_) => Ok(Value::FunctionCall {
                    name: name.clone(),
                    args: resolved_args,
                }),
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
    fn parse_undefined_resource_reference_becomes_unresolved() {
        // When a 2-part identifier references an unknown binding,
        // it becomes an UnresolvedIdent to be resolved during schema validation
        let input = r#"
            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = nonexistent.name
            }
        "#;

        // Parsing succeeds - unknown identifiers become UnresolvedIdent
        let result = parse_and_resolve(input);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(
            parsed.resources[0].attributes.get("bucket"),
            Some(&Value::UnresolvedIdent(
                "nonexistent".to_string(),
                Some("name".to_string())
            ))
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
        // not silently downgraded to UnresolvedIdent.
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
        // Forward reference vpc.vpc_id should be a ResourceRef, not UnresolvedIdent
        assert_eq!(
            subnet.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
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
}
