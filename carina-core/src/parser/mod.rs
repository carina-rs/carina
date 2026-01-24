//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

use pest::Parser;
use pest_derive::Parser;
use std::collections::HashMap;
use std::env;

use crate::resource::{Resource, ResourceId, Value};

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
}

/// Provider configuration
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub attributes: HashMap<String, Value>,
}

/// Parse result
#[derive(Debug)]
pub struct ParsedFile {
    pub providers: Vec<ProviderConfig>,
    pub resources: Vec<Resource>,
    pub variables: HashMap<String, Value>,
}

/// Parse context (variable scope)
struct ParseContext {
    variables: HashMap<String, Value>,
    /// Resource bindings (binding_name -> Resource)
    resource_bindings: HashMap<String, Resource>,
}

impl ParseContext {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            resource_bindings: HashMap::new(),
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

    for pair in pairs {
        if pair.as_rule() == Rule::file {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::statement {
                    for stmt in inner.into_inner() {
                        match stmt.as_rule() {
                            Rule::provider_block => {
                                let provider = parse_provider_block(stmt, &ctx)?;
                                providers.push(provider);
                            }
                            Rule::let_binding => {
                                let (name, value, maybe_resource) =
                                    parse_let_binding(stmt, &ctx)?;
                                ctx.set_variable(name.clone(), value);
                                if let Some(resource) = maybe_resource {
                                    ctx.set_resource_binding(name.clone(), resource.clone());
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

    Ok(ParsedFile {
        providers,
        resources,
        variables: ctx.variables,
    })
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

fn parse_let_binding(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<(String, Value, Option<Resource>), ParseError> {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();
    let expr_pair = inner.next().unwrap();

    // Check if it's a resource expression
    let (value, maybe_resource) = parse_expression_with_resource(expr_pair, ctx, &name)?;

    Ok((name, value, maybe_resource))
}

fn parse_expression_with_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>), ParseError> {
    let inner = pair.into_inner().next().unwrap();
    parse_pipe_expr_with_resource(inner, ctx, binding_name)
}

fn parse_pipe_expr_with_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>), ParseError> {
    let mut inner = pair.into_inner();
    let primary = inner.next().unwrap();
    let (value, maybe_resource) = parse_primary_with_resource(primary, ctx, binding_name)?;

    // Pipe operator handling (for future extension)
    for _func_call in inner {
        // TODO: Implement pipe operator
    }

    Ok((value, maybe_resource))
}

fn parse_primary_with_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<(Value, Option<Resource>), ParseError> {
    let inner = pair.into_inner().next().unwrap();

    match inner.as_rule() {
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            // Return reference to resource as a value
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((ref_value, Some(resource)))
        }
        _ => {
            let value = parse_primary_value(inner, ctx)?;
            Ok((value, None))
        }
    }
}

fn parse_anonymous_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = inner.next().unwrap().as_str().to_string();

    // Extract resource type from namespace (aws.s3.bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    let provider = parts[0];
    let resource_type = parts[1..].join("_");

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = attr_inner.next().unwrap().as_str().to_string();
            let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
            attributes.insert(key, value);
        }
    }

    // Get resource name from name attribute
    let resource_name = match attributes.get("name") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: "Anonymous resource must have a 'name' attribute".to_string(),
            });
        }
    };

    // Add provider information to attributes
    attributes.insert("_provider".to_string(), Value::String(provider.to_string()));
    attributes.insert(
        "_type".to_string(),
        Value::String(namespaced_type.clone()),
    );

    Ok(Resource {
        id: ResourceId::new(resource_type, resource_name),
        attributes,
    })
}

fn parse_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = inner.next().unwrap().as_str().to_string();

    // Extract resource type from namespace (aws.s3.bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join("_");

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = attr_inner.next().unwrap().as_str().to_string();
            let value = parse_expression(attr_inner.next().unwrap(), ctx)?;
            attributes.insert(key, value);
        }
    }

    // Get resource name from name attribute (same as anonymous resources)
    let resource_name = match attributes.get("name") {
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "Resource bound to '{}' must have a 'name' attribute",
                    binding_name
                ),
            });
        }
    };

    // Add provider information to attributes
    attributes.insert("_provider".to_string(), Value::String(provider.to_string()));
    attributes.insert(
        "_type".to_string(),
        Value::String(namespaced_type.clone()),
    );
    // Save binding name (for reference)
    attributes.insert(
        "_binding".to_string(),
        Value::String(binding_name.to_string()),
    );

    Ok(Resource {
        id: ResourceId::new(resource_type, resource_name),
        attributes,
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
        Rule::namespaced_id => {
            // Namespaced identifier (e.g., aws.Region.ap_northeast_1)
            // or resource reference (e.g., bucket.name)
            let full_str = inner.as_str();
            let parts: Vec<&str> = full_str.split('.').collect();

            if parts.len() == 2 {
                // Two-part identifier: could be resource reference or undefined variable
                if ctx.is_resource_binding(parts[0]) {
                    // This is a resource reference: resource.attribute
                    Ok(Value::ResourceRef(
                        parts[0].to_string(),
                        parts[1].to_string(),
                    ))
                } else if ctx.get_variable(parts[0]).is_some() {
                    // Variable exists but trying to access attribute on non-resource
                    Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "'{}' is not a resource, cannot access attribute '{}'",
                            parts[0], parts[1]
                        ),
                    })
                } else {
                    // Unknown identifier
                    Err(ParseError::UndefinedVariable(full_str.to_string()))
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
                // Member access: resource.attribute
                let attr_name = second_part.as_str();

                // Check if it's a resource binding
                if ctx.is_resource_binding(first_ident) {
                    // Return a ResourceRef that will be resolved later
                    Ok(Value::ResourceRef(
                        first_ident.to_string(),
                        attr_name.to_string(),
                    ))
                } else {
                    // Not a resource binding, treat as undefined
                    Err(ParseError::UndefinedVariable(format!(
                        "{}.{}",
                        first_ident, attr_name
                    )))
                }
            } else {
                // Simple variable reference
                match ctx.get_variable(first_ident) {
                    Some(val) => Ok(val.clone()),
                    None => Err(ParseError::UndefinedVariable(first_ident.to_string())),
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
        Value::ResourceRef(binding_name, attr_name) => {
            match binding_map.get(binding_name) {
                Some(attributes) => {
                    match attributes.get(attr_name) {
                        Some(attr_value) => {
                            // Recursively resolve in case the attribute itself is a reference
                            resolve_value(attr_value, binding_map)
                        }
                        None => {
                            // Attribute not found, keep as reference (might be resolved at runtime)
                            Ok(value.clone())
                        }
                    }
                }
                None => Err(ParseError::UndefinedVariable(format!(
                    "{}.{}",
                    binding_name, attr_name
                ))),
            }
        }
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
            let my_bucket = aws.s3.bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, "my-bucket"); // name attribute value becomes the resource ID
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
            let logs = aws.s3.bucket {
                name = "app-logs"
            }

            let data = aws.s3.bucket {
                name = "app-data"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name, "app-logs"); // name attribute value becomes the resource ID
        assert_eq!(result.resources[1].id.name, "app-data");
    }

    #[test]
    fn parse_variable_and_resource() {
        let input = r#"
            let default_region = aws.Region.ap_northeast_1

            let my_bucket = aws.s3.bucket {
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
            let app_logs = aws.s3.bucket {
                name = "my-app-logs"
                versioning = versioning
                expiration_days = retention_days
            }

            let app_data = aws.s3.bucket {
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
            let my_bucket = aws.s3.bucket {
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
        assert_eq!(result.resources[0].id.resource_type, "storage_bucket");
        assert_eq!(
            result.resources[0].attributes.get("_provider"),
            Some(&Value::String("gcp".to_string()))
        );
    }

    #[test]
    fn parse_anonymous_resource() {
        let input = r#"
            aws.s3.bucket {
                name = "my-anonymous-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name, "my-anonymous-bucket");
    }

    #[test]
    fn parse_mixed_resources() {
        let input = r#"
            # Anonymous resource
            aws.s3.bucket {
                name = "anonymous-bucket"
            }

            # Named resource
            let named = aws.s3.bucket {
                name = "named-bucket"
            }
        "#;

        let result = parse(input).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name, "anonymous-bucket");
        assert_eq!(result.resources[1].id.name, "named-bucket"); // name attribute value becomes the resource ID
    }

    #[test]
    fn parse_anonymous_resource_without_name_fails() {
        let input = r#"
            aws.s3.bucket {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_resource_reference() {
        let input = r#"
            let bucket = aws.s3.bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3.bucket_policy {
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
            Some(&Value::ResourceRef(
                "bucket".to_string(),
                "name".to_string()
            ))
        );
    }

    #[test]
    fn parse_and_resolve_resource_reference() {
        let input = r#"
            let bucket = aws.s3.bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3.bucket_policy {
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
    fn parse_undefined_resource_reference_fails() {
        let input = r#"
            let policy = aws.s3.bucket_policy {
                name = "my-policy"
                bucket = nonexistent.name
            }
        "#;

        let result = parse(input);
        assert!(result.is_err());
        match result {
            Err(ParseError::UndefinedVariable(name)) => {
                assert!(name.contains("nonexistent"));
            }
            _ => panic!("Expected UndefinedVariable error"),
        }
    }

    #[test]
    fn resource_reference_preserves_namespaced_id() {
        // Ensure that aws.Region.ap_northeast_1 is NOT treated as a resource reference
        let input = r#"
            let bucket = aws.s3.bucket {
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
}
