//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

use pest::Parser;
use pest_derive::Parser;
use std::collections::HashMap;
use std::env;

use crate::resource::{LifecycleConfig, Resource, ResourceId, Value};

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

    #[error("Environment variable not set: {0}")]
    EnvVarNotSet(String),

    #[error("Invalid resource type: {0}")]
    InvalidResourceType(String),

    #[error("Duplicate module definition: {0}")]
    DuplicateModule(String),

    #[error("Module not found: {0}")]
    ModuleNotFound(String),
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

/// Type expression for input/output parameters
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
    /// Reference to a resource type (e.g., ref(aws.vpc))
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
            TypeExpr::Ref(path) => write!(f, "ref({})", path),
        }
    }
}

/// Input parameter definition
#[derive(Debug, Clone)]
pub struct InputParameter {
    pub name: String,
    pub type_expr: TypeExpr,
    pub default: Option<Value>,
}

/// Output parameter definition
#[derive(Debug, Clone)]
pub struct OutputParameter {
    pub name: String,
    pub type_expr: TypeExpr,
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
    /// Top-level input parameters (directory-based module style)
    pub inputs: Vec<InputParameter>,
    /// Top-level output parameters (directory-based module style)
    pub outputs: Vec<OutputParameter>,
    /// Backend configuration for state storage
    pub backend: Option<BackendConfig>,
}

/// Parse context (variable scope)
struct ParseContext {
    variables: HashMap<String, Value>,
    /// Resource bindings (binding_name -> Resource)
    resource_bindings: HashMap<String, Resource>,
    /// Imported modules (alias -> path)
    imported_modules: HashMap<String, String>,
    /// Whether we're inside a module (for input.* references)
    in_module: bool,
    /// Input parameter names when inside a module
    input_params: HashMap<String, TypeExpr>,
}

impl ParseContext {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
            in_module: false,
            input_params: HashMap::new(),
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

/// Parse a .crn file
pub fn parse(input: &str) -> Result<ParsedFile, ParseError> {
    let pairs = CarinaParser::parse(Rule::file, input)?;

    let mut ctx = ParseContext::new();
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut imports = Vec::new();
    let mut module_calls = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut backend = None;

    for pair in pairs {
        if pair.as_rule() == Rule::file {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::statement {
                    for stmt in inner.into_inner() {
                        match stmt.as_rule() {
                            Rule::import_stmt => {
                                let import = parse_import_stmt(stmt)?;
                                ctx.imported_modules
                                    .insert(import.alias.clone(), import.path.clone());
                                imports.push(import);
                            }
                            Rule::backend_block => {
                                backend = Some(parse_backend_block(stmt, &ctx)?);
                            }
                            Rule::provider_block => {
                                let provider = parse_provider_block(stmt, &ctx)?;
                                providers.push(provider);
                            }
                            Rule::input_block => {
                                let parsed_inputs = parse_input_block(stmt)?;
                                for input in &parsed_inputs {
                                    ctx.input_params
                                        .insert(input.name.clone(), input.type_expr.clone());
                                }
                                ctx.in_module = true;
                                inputs.extend(parsed_inputs);
                            }
                            Rule::output_block => {
                                let parsed_outputs = parse_output_block(stmt, &ctx)?;
                                outputs.extend(parsed_outputs);
                            }
                            Rule::let_binding => {
                                let (name, value, maybe_resource, maybe_module_call) =
                                    parse_let_binding_extended(stmt, &ctx)?;
                                ctx.set_variable(name.clone(), value);
                                if let Some(resource) = maybe_resource {
                                    ctx.set_resource_binding(name.clone(), resource.clone());
                                    resources.push(resource);
                                }
                                if let Some(mut call) = maybe_module_call {
                                    call.binding_name = Some(name);
                                    module_calls.push(call);
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

    Ok(ParsedFile {
        providers,
        resources,
        variables: ctx.variables,
        imports,
        module_calls,
        inputs,
        outputs,
        backend,
    })
}

/// Parse input block
fn parse_input_block(pair: pest::iterators::Pair<Rule>) -> Result<Vec<InputParameter>, ParseError> {
    let mut inputs = Vec::new();
    let ctx = ParseContext::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::input_param {
            let mut param_inner = param.into_inner();
            let name = param_inner.next().unwrap().as_str().to_string();
            let type_expr = parse_type_expr(param_inner.next().unwrap())?;
            let default = if let Some(expr) = param_inner.next() {
                Some(parse_expression(expr, &ctx)?)
            } else {
                None
            };
            inputs.push(InputParameter {
                name,
                type_expr,
                default,
            });
        }
    }

    Ok(inputs)
}

/// Parse output block
fn parse_output_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Vec<OutputParameter>, ParseError> {
    let mut outputs = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::output_param {
            let mut param_inner = param.into_inner();
            let name = param_inner.next().unwrap().as_str().to_string();
            let type_expr = parse_type_expr(param_inner.next().unwrap())?;
            let value = if let Some(expr) = param_inner.next() {
                Some(parse_expression(expr, ctx)?)
            } else {
                None
            };
            outputs.push(OutputParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(outputs)
}

/// Parse type expression
fn parse_type_expr(pair: pest::iterators::Pair<Rule>) -> Result<TypeExpr, ParseError> {
    let inner = pair.into_inner().next().unwrap();
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
            let inner_type = parse_type_expr(generic_inner.next().unwrap())?;

            if is_list {
                Ok(TypeExpr::List(Box::new(inner_type)))
            } else {
                Ok(TypeExpr::Map(Box::new(inner_type)))
            }
        }
        Rule::type_ref => {
            // Parse ref(resource_type_path)
            let mut ref_inner = inner.into_inner();
            let path_str = ref_inner.next().unwrap().as_str();
            let path = ResourceTypePath::parse(path_str).ok_or_else(|| {
                ParseError::InvalidResourceType(format!("Invalid resource type path: {}", path_str))
            })?;
            Ok(TypeExpr::Ref(path))
        }
        _ => Ok(TypeExpr::String),
    }
}

/// Parse import statement
fn parse_import_stmt(pair: pest::iterators::Pair<Rule>) -> Result<ImportStatement, ParseError> {
    let mut inner = pair.into_inner();
    let path = parse_string(inner.next().unwrap());
    let alias = inner.next().unwrap().as_str().to_string();

    Ok(ImportStatement { path, alias })
}

/// Parse module call
fn parse_module_call(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ModuleCall, ParseError> {
    let mut inner = pair.into_inner();
    let module_name = inner.next().unwrap().as_str().to_string();

    let mut arguments = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key = arg_inner.next().unwrap().as_str().to_string();
            let value = parse_expression(arg_inner.next().unwrap(), ctx)?;
            arguments.insert(key, value);
        }
    }

    Ok(ModuleCall {
        module_name,
        binding_name: None,
        arguments,
    })
}

/// Extended parse_let_binding that also handles module calls
fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<(String, Value, Option<Resource>, Option<ModuleCall>), ParseError> {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();
    let expr_pair = inner.next().unwrap();

    // Check if it's a module call or resource expression
    let (value, maybe_resource, maybe_module_call) =
        parse_expression_with_resource_or_module(expr_pair, ctx, &name)?;

    Ok((name, value, maybe_resource, maybe_module_call))
}

/// Parse expression with potential resource or module call
fn parse_expression_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>, Option<ModuleCall>), ParseError> {
    let inner = pair.into_inner().next().unwrap();
    parse_pipe_expr_with_resource_or_module(inner, ctx, binding_name)
}

fn parse_pipe_expr_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>, Option<ModuleCall>), ParseError> {
    let mut inner = pair.into_inner();
    let primary = inner.next().unwrap();
    let (value, maybe_resource, maybe_module_call) =
        parse_primary_with_resource_or_module(primary, ctx, binding_name)?;

    // Pipe operator handling (for future extension)
    for _func_call in inner {
        // TODO: Implement pipe operator
    }

    Ok((value, maybe_resource, maybe_module_call))
}

fn parse_primary_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>, Option<ModuleCall>), ParseError> {
    let inner = pair.into_inner().next().unwrap();

    match inner.as_rule() {
        Rule::read_resource_expr => {
            let resource = parse_read_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, Some(resource), None))
        }
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, Some(resource), None))
        }
        _ => {
            // Check if it could be a module call (identifier followed by braces)
            // This is handled by checking if it's a simple identifier that matches a module
            let value = parse_primary_value(inner, ctx)?;
            Ok((value, None, None))
        }
    }
}

fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = attr_inner.next().unwrap().as_str().to_string();
            let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
            attributes.insert(key, value);
        }
    }

    Ok(ProviderConfig { name, attributes })
}

fn parse_backend_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<BackendConfig, ParseError> {
    let mut inner = pair.into_inner();
    let backend_type = inner.next().unwrap().as_str().to_string();

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = attr_inner.next().unwrap().as_str().to_string();
            let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
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

    let namespaced_type = inner.next().unwrap().as_str().to_string();

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

    // Add provider information to attributes
    let mut attributes = attributes;
    attributes.insert("_provider".to_string(), Value::String(provider.to_string()));
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

/// Parse block contents (attributes and nested blocks)
/// Nested blocks with the same name are collected into a list
fn parse_block_contents(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
) -> Result<HashMap<String, Value>, ParseError> {
    let mut attributes: HashMap<String, Value> = HashMap::new();
    let mut nested_blocks: HashMap<String, Vec<Value>> = HashMap::new();

    for content_pair in pairs {
        match content_pair.as_rule() {
            Rule::block_content => {
                let inner = content_pair.into_inner().next().unwrap();
                match inner.as_rule() {
                    Rule::attribute => {
                        let mut attr_inner = inner.into_inner();
                        let key = attr_inner.next().unwrap().as_str().to_string();
                        let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
                        attributes.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name = block_inner.next().unwrap().as_str().to_string();

                        // Parse nested block attributes into a map
                        let mut block_attrs = HashMap::new();
                        for attr_pair in block_inner {
                            if attr_pair.as_rule() == Rule::attribute {
                                let mut attr_inner = attr_pair.into_inner();
                                let key = attr_inner.next().unwrap().as_str().to_string();
                                let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
                                block_attrs.insert(key, value);
                            }
                        }

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
                let key = attr_inner.next().unwrap().as_str().to_string();
                let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
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
            return LifecycleConfig { force_delete };
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

    let namespaced_type = inner.next().unwrap().as_str().to_string();

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

    // Add provider information to attributes
    attributes.insert("_provider".to_string(), Value::String(provider.to_string()));
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

    let namespaced_type = inner.next().unwrap().as_str().to_string();

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

    // Add provider information to attributes
    attributes.insert("_provider".to_string(), Value::String(provider.to_string()));
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
    let inner = pair.into_inner().next().unwrap();
    parse_pipe_expr(inner, ctx)
}

fn parse_pipe_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut inner = pair.into_inner();
    let primary = inner.next().unwrap();
    let value = parse_primary_value(primary, ctx)?;

    // Pipe operator handling (for future extension)
    for _func_call in inner {
        // TODO: Implement pipe operator
    }

    Ok(value)
}

fn parse_primary_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // For primary, get inner content; otherwise process directly
    let inner = if pair.as_rule() == Rule::primary {
        pair.into_inner().next().unwrap()
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::env_var => {
            let mut env_inner = inner.into_inner();
            let var_name = parse_string(env_inner.next().unwrap());
            match env::var(&var_name) {
                Ok(val) => Ok(Value::String(val)),
                Err(_) => Err(ParseError::EnvVarNotSet(var_name)),
            }
        }
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
            for entry in inner.into_inner() {
                if entry.as_rule() == Rule::map_entry {
                    let mut entry_inner = entry.into_inner();
                    let key = entry_inner.next().unwrap().as_str().to_string();
                    let value = parse_expression(entry_inner.next().unwrap(), ctx)?;
                    map.insert(key, value);
                }
            }
            Ok(Value::Map(map))
        }
        Rule::namespaced_id => {
            // Namespaced identifier (e.g., aws.Region.ap_northeast_1)
            // or resource reference (e.g., bucket.name)
            // or input reference in module context (e.g., input.vpc_id)
            let full_str = inner.as_str();
            let parts: Vec<&str> = full_str.split('.').collect();

            if parts.len() == 2 {
                // Two-part identifier: could be input reference, resource reference or variable access
                if parts[0] == "input" && ctx.in_module {
                    // Input reference in module context (input.vpc_id)
                    // Treat as a special ResourceRef with "input" as the binding name
                    Ok(Value::ResourceRef {
                        binding_name: "input".to_string(),
                        attribute_name: parts[1].to_string(),
                    })
                } else if ctx.get_variable(parts[0]).is_some() && !ctx.is_resource_binding(parts[0])
                {
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
            let f: f64 = inner.as_str().parse().unwrap();
            Ok(Value::Float(f))
        }
        Rule::number => {
            let n: i64 = inner.as_str().parse().unwrap();
            Ok(Value::Int(n))
        }
        Rule::string => {
            let s = parse_string(inner);
            Ok(Value::String(s))
        }
        Rule::variable_ref => {
            // variable_ref can be "identifier" or "identifier.identifier" (member access)
            let mut parts = inner.into_inner();
            let first_ident = parts.next().unwrap().as_str();

            if let Some(second_part) = parts.next() {
                // Member access: resource.attribute or input.param
                let attr_name = second_part.as_str();

                // Handle input reference in module context
                if first_ident == "input" && ctx.in_module {
                    return Ok(Value::ResourceRef {
                        binding_name: "input".to_string(),
                        attribute_name: attr_name.to_string(),
                    });
                }

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

fn parse_string(pair: pest::iterators::Pair<Rule>) -> String {
    let s = pair.as_str();
    // Remove quotes
    let inner = &s[1..s.len() - 1];
    // Handle escape sequences
    inner
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    // Build a map of binding_name -> attributes for quick lookup
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &parsed.resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            binding_map.insert(binding_name.clone(), resource.attributes.clone());
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
    fn parse_env_var() {
        // SAFETY: This test runs in isolation
        unsafe {
            env::set_var("CARINA_TEST_VAR", "test-value");
        }

        let input = r#"
            let my_bucket = aws.s3_bucket {
                name = env("CARINA_TEST_VAR")
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(
            result.resources[0].attributes.get("name"),
            Some(&Value::String("test-value".to_string()))
        );

        // SAFETY: This test runs in isolation
        unsafe {
            env::remove_var("CARINA_TEST_VAR");
        }
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
        assert_eq!(
            result.resources[0].attributes.get("_provider"),
            Some(&Value::String("gcp".to_string()))
        );
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
            input {
                vpc_id: string
                enable_https: bool = true
            }

            output {
                sg_id: string = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = input.vpc_id
            }
        "#;

        let result = parse(input).unwrap();

        // Check inputs
        assert_eq!(result.inputs.len(), 2);
        assert_eq!(result.inputs[0].name, "vpc_id");
        assert_eq!(result.inputs[0].type_expr, TypeExpr::String);
        assert!(result.inputs[0].default.is_none());

        assert_eq!(result.inputs[1].name, "enable_https");
        assert_eq!(result.inputs[1].type_expr, TypeExpr::Bool);
        assert_eq!(result.inputs[1].default, Some(Value::Bool(true)));

        // Check outputs
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].name, "sg_id");
        assert_eq!(result.outputs[0].type_expr, TypeExpr::String);

        // Check resource has input reference
        assert_eq!(result.resources.len(), 1);
        let sg = &result.resources[0];
        assert_eq!(
            sg.attributes.get("vpc_id"),
            Some(&Value::ResourceRef {
                binding_name: "input".to_string(),
                attribute_name: "vpc_id".to_string(),
            })
        );
    }

    #[test]
    fn parse_import_statement() {
        let input = r#"
            import "./modules/web_tier.crn" as web_tier
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].path, "./modules/web_tier.crn");
        assert_eq!(result.imports[0].alias, "web_tier");
    }

    #[test]
    fn parse_generic_type_expressions() {
        let input = r#"
            input {
                ports: list(int)
                tags: map(string)
                cidrs: list(string)
            }

            output {
                result: list(string)
            }
        "#;

        let result = parse(input).unwrap();

        assert_eq!(
            result.inputs[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::Int))
        );
        assert_eq!(
            result.inputs[1].type_expr,
            TypeExpr::Map(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.inputs[2].type_expr,
            TypeExpr::List(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.outputs[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::String))
        );
    }

    #[test]
    fn parse_ref_type_expression() {
        let input = r#"
            input {
                vpc: ref(aws.vpc)
                enable_https: bool = true
            }

            output {
                security_group_id: ref(aws.security_group) = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = input.vpc
            }
        "#;

        let result = parse(input).unwrap();

        // Check ref type input
        assert_eq!(result.inputs[0].name, "vpc");
        assert_eq!(
            result.inputs[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc"))
        );
        assert!(result.inputs[0].default.is_none());

        // Check ref type output
        assert_eq!(result.outputs[0].name, "security_group_id");
        assert_eq!(
            result.outputs[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group"))
        );
    }

    #[test]
    fn parse_ref_type_with_nested_resource_type() {
        let input = r#"
            input {
                sg: ref(aws.security_group)
                rule: ref(aws.security_group.ingress_rule)
            }

            output {
                out: string
            }
        "#;

        let result = parse(input).unwrap();

        // Single-level resource type
        assert_eq!(
            result.inputs[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group"))
        );

        // Nested resource type (security_group.ingress_rule)
        assert_eq!(
            result.inputs[1].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group.ingress_rule"))
        );
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
            "ref(aws.vpc)"
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

            aws.vpc.vpc {
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
}
