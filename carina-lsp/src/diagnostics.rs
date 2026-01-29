use std::collections::{HashMap, HashSet};
use std::path::Path;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use carina_core::parser::{InputParameter, ParseError, ParsedFile, TypeExpr};
use carina_core::resource::Value;
use carina_core::schema::validate_cidr;
use carina_provider_aws::schemas::{s3, types as aws_types, vpc};
use carina_provider_awscc::schemas::vpc as awscc_vpc;

pub struct DiagnosticEngine {
    valid_resource_types: HashSet<String>,
}

impl DiagnosticEngine {
    pub fn new() -> Self {
        let mut valid_resource_types = HashSet::new();

        // S3 resources
        valid_resource_types.insert("s3.bucket".to_string());

        // VPC resources
        valid_resource_types.insert("vpc".to_string());
        valid_resource_types.insert("subnet".to_string());
        valid_resource_types.insert("internet_gateway".to_string());
        valid_resource_types.insert("route_table".to_string());
        valid_resource_types.insert("route".to_string());
        valid_resource_types.insert("security_group".to_string());
        valid_resource_types.insert("security_group.ingress_rule".to_string());
        valid_resource_types.insert("security_group.egress_rule".to_string());

        // AWS Cloud Control resources
        valid_resource_types.insert("awscc.vpc".to_string());

        Self {
            valid_resource_types,
        }
    }

    pub fn analyze(&self, doc: &Document, base_path: Option<&Path>) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        // Extract defined resource bindings
        let defined_bindings = self.extract_resource_bindings(&text);

        // Parse errors
        if let Some(error) = doc.parse_error() {
            diagnostics.push(parse_error_to_diagnostic(error));
        }

        // Check for undefined resource references in the raw text
        diagnostics.extend(self.check_undefined_references(&text, &defined_bindings));

        // Semantic analysis on parsed file
        if let Some(parsed) = doc.parsed() {
            // Check provider region
            diagnostics.extend(self.check_provider_region(doc, parsed));

            // Check module calls
            if let Some(base) = base_path {
                diagnostics.extend(self.check_module_calls(doc, parsed, base));
            }
            // Check resource types
            for resource in &parsed.resources {
                // Detect the provider from the DSL (aws. or awscc.)
                let provider = self.detect_resource_provider(
                    doc,
                    &resource.id.resource_type,
                    &resource.id.name,
                );
                let full_resource_type = if provider == "awscc" {
                    format!("awscc.{}", resource.id.resource_type)
                } else {
                    resource.id.resource_type.clone()
                };

                if !self.valid_resource_types.contains(&full_resource_type) {
                    // Find the line where this resource is defined
                    if let Some((line, col)) =
                        self.find_resource_position(doc, &resource.id.resource_type)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col
                                        + resource.id.resource_type.len() as u32
                                        + provider.len() as u32
                                        + 1, // "provider." prefix
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "Unknown resource type: {}.{}",
                                provider,
                                resource.id.resource_type.replace('_', ".")
                            ),
                            ..Default::default()
                        });
                    }
                }

                // Semantic validation using schema
                let schema = self.get_schema_for_type(&full_resource_type);
                if let Some(schema) = schema {
                    for (attr_name, attr_value) in &resource.attributes {
                        if attr_name.starts_with('_') {
                            continue; // Skip internal attributes
                        }

                        // Check for unknown attributes
                        if !schema.attributes.contains_key(attr_name) {
                            if let Some((line, col)) = self.find_attribute_position(doc, attr_name)
                            {
                                // Check if there's a similar attribute (e.g., vpc -> vpc_id)
                                let suggestion =
                                    if schema.attributes.contains_key(&format!("{}_id", attr_name))
                                    {
                                        format!(". Did you mean '{}_id'?", attr_name)
                                    } else {
                                        String::new()
                                    };

                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col + attr_name.len() as u32,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("carina".to_string()),
                                    message: format!(
                                        "Unknown attribute '{}' for resource type '{}'{}",
                                        attr_name, resource.id.resource_type, suggestion
                                    ),
                                    ..Default::default()
                                });
                            }
                            continue;
                        }

                        // Type validation
                        if let Some(attr_schema) = schema.attributes.get(attr_name) {
                            let type_error = match (&attr_schema.attr_type, attr_value) {
                                // Bool type should not receive String
                                (carina_core::schema::AttributeType::Bool, Value::String(s)) => {
                                    Some(format!(
                                        "Type mismatch: expected Bool, got String \"{}\". Use true or false.",
                                        s
                                    ))
                                }
                                // Int type should not receive String
                                (carina_core::schema::AttributeType::Int, Value::String(s)) => {
                                    Some(format!(
                                        "Type mismatch: expected Int, got String \"{}\".",
                                        s
                                    ))
                                }
                                // Custom type validation (VersioningStatus, InstanceTenancy, Cidr, Region, etc.)
                                (
                                    carina_core::schema::AttributeType::Custom {
                                        name,
                                        validate,
                                        ..
                                    },
                                    value,
                                ) => {
                                    if name == "Cidr" {
                                        if let Value::String(s) = value {
                                            validate_cidr(s).err()
                                        } else {
                                            None
                                        }
                                    } else {
                                        // Use schema's validate function for other Custom types
                                        validate(value).err().map(|e| e.to_string())
                                    }
                                }
                                // String type - check for bare resource binding
                                (carina_core::schema::AttributeType::String, Value::String(s)) => {
                                    if let Some(binding) =
                                        s.strip_prefix("${").and_then(|s| s.strip_suffix("}"))
                                    {
                                        let suggested_attr = if attr_name.ends_with("_id") {
                                            "id"
                                        } else {
                                            "name"
                                        };
                                        Some(format!(
                                            "Expected string, got resource reference '{}'. Did you mean '{}.{}'?",
                                            binding, binding, suggested_attr
                                        ))
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            };

                            if let Some(message) = type_error
                                && let Some((line, col)) =
                                    self.find_attribute_position(doc, attr_name)
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col + attr_name.len() as u32,
                                        },
                                    },
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("carina".to_string()),
                                    message,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }

        diagnostics
    }

    fn get_schema_for_type(
        &self,
        resource_type: &str,
    ) -> Option<carina_core::schema::ResourceSchema> {
        match resource_type {
            "s3_bucket" => Some(s3::bucket_schema()),
            "vpc" => Some(vpc::vpc_schema()),
            "subnet" => Some(vpc::subnet_schema()),
            "internet_gateway" => Some(vpc::internet_gateway_schema()),
            "route_table" => Some(vpc::route_table_schema()),
            "route" => Some(vpc::route_schema()),
            "security_group" => Some(vpc::security_group_schema()),
            "security_group.ingress_rule" => Some(vpc::security_group_ingress_rule_schema()),
            "security_group.egress_rule" => Some(vpc::security_group_egress_rule_schema()),
            // AWS Cloud Control resources
            "awscc.vpc" => Some(awscc_vpc::vpc_schema()),
            _ => None,
        }
    }

    fn find_resource_position(&self, doc: &Document, resource_type: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        // Convert resource_type back to DSL format: vpc -> aws.vpc, s3.bucket -> aws.s3.bucket
        let dsl_type = format!("aws.{}", resource_type.replace('_', "."));

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(col) = line.find(&dsl_type) {
                return Some((line_idx as u32, col as u32));
            }
        }
        None
    }

    fn find_attribute_position(&self, doc: &Document, attr_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Must start with attr_name followed by whitespace or '='
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after_attr = &trimmed[attr_name.len()..];
            if !after_attr.starts_with(' ') && !after_attr.starts_with('=') {
                continue;
            }
            // Calculate column position (account for leading whitespace)
            let leading_ws = line.len() - trimmed.len();
            return Some((line_idx as u32, leading_ws as u32));
        }
        None
    }

    /// Check provider region attribute
    fn check_provider_region(&self, doc: &Document, parsed: &ParsedFile) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let aws_region_type = aws_types::aws_region();
        let awscc_region_type = awscc_vpc::aws_region();

        for provider in &parsed.providers {
            if provider.name == "aws"
                && let Some(region_value) = provider.attributes.get("region")
                && let Err(e) = aws_region_type.validate(region_value)
                && let Some((line, col)) = self.find_provider_region_position(doc, "aws")
            {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: col,
                        },
                        end: Position {
                            line,
                            character: col + 6, // "region"
                        },
                    },
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("carina".to_string()),
                    message: format!("provider aws: {}", e),
                    ..Default::default()
                });
            }
            if provider.name == "awscc"
                && let Some(region_value) = provider.attributes.get("region")
                && let Err(e) = awscc_region_type.validate(region_value)
                && let Some((line, col)) = self.find_provider_region_position(doc, "awscc")
            {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: col,
                        },
                        end: Position {
                            line,
                            character: col + 6, // "region"
                        },
                    },
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("carina".to_string()),
                    message: format!("provider awscc: {}", e),
                    ..Default::default()
                });
            }
        }
        diagnostics
    }

    /// Detect the provider (aws or awscc) for a resource by looking at the DSL
    fn detect_resource_provider(
        &self,
        doc: &Document,
        resource_type: &str,
        resource_name: &str,
    ) -> String {
        let text = doc.text();
        // Look for patterns like "awscc.vpc {" or "let x = awscc.vpc {"
        let awscc_pattern = format!("awscc.{}", resource_type);

        for line in text.lines() {
            let trimmed = line.trim();
            // Check if this line defines the resource with awscc provider
            if trimmed.contains(&awscc_pattern) {
                // Verify it's the right resource by checking the name attribute
                // For now, just check if awscc pattern exists
                return "awscc".to_string();
            }
        }

        // Also check for the name attribute to be more precise
        let mut in_awscc_block = false;
        let mut brace_depth = 0;

        for line in text.lines() {
            let trimmed = line.trim();

            if trimmed.contains(&awscc_pattern) && trimmed.contains('{') {
                in_awscc_block = true;
                brace_depth = 1;
                continue;
            }

            if in_awscc_block {
                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                    }
                }

                // Check if this block has the matching name
                if trimmed.starts_with("name") && trimmed.contains(resource_name) {
                    return "awscc".to_string();
                }

                if brace_depth == 0 {
                    in_awscc_block = false;
                }
            }
        }

        "aws".to_string()
    }

    /// Find the position of the region attribute in a provider block
    fn find_provider_region_position(
        &self,
        doc: &Document,
        provider_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_provider = false;
        let provider_pattern = format!("provider {}", provider_name);

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with(&provider_pattern) {
                in_provider = true;
            }

            if in_provider {
                if trimmed.starts_with("region") {
                    let leading_ws = line.len() - trimmed.len();
                    return Some((line_idx as u32, leading_ws as u32));
                }

                if trimmed == "}" {
                    in_provider = false;
                }
            }
        }
        None
    }

    /// Check module calls against imported module definitions
    fn check_module_calls(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        base_path: &Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Build a map of imported modules: alias -> input parameters
        let mut imported_modules: HashMap<String, Vec<InputParameter>> = HashMap::new();

        for import in &parsed.imports {
            let module_path = base_path.join(&import.path);
            if let Some(module_parsed) = self.load_module(&module_path) {
                imported_modules.insert(import.alias.clone(), module_parsed.inputs);
            }
        }

        // Check each module call
        for call in &parsed.module_calls {
            if let Some(module_inputs) = imported_modules.get(&call.module_name) {
                // Check for unknown parameters
                for (arg_name, arg_value) in &call.arguments {
                    let matching_input = module_inputs.iter().find(|input| &input.name == arg_name);

                    if matching_input.is_none() {
                        if let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                        {
                            // Find similar parameter names for suggestion
                            let suggestion = module_inputs
                                .iter()
                                .find(|input| {
                                    input.name.contains(arg_name) || arg_name.contains(&input.name)
                                })
                                .map(|input| format!(". Did you mean '{}'?", input.name))
                                .unwrap_or_default();

                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line,
                                        character: col,
                                    },
                                    end: Position {
                                        line,
                                        character: col + arg_name.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Unknown parameter '{}' for module '{}'{}",
                                    arg_name, call.module_name, suggestion
                                ),
                                ..Default::default()
                            });
                        }
                        continue;
                    }

                    // Type validation for known parameters
                    let input = matching_input.unwrap();
                    if let Some(type_error) =
                        self.validate_module_arg_type(&input.type_expr, arg_value)
                        && let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + arg_name.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("carina".to_string()),
                            message: type_error,
                            ..Default::default()
                        });
                    }
                }

                // Check for missing required parameters
                for input in module_inputs {
                    if input.default.is_none()
                        && !call.arguments.contains_key(&input.name)
                        && let Some((line, col)) =
                            self.find_module_call_position(doc, &call.module_name)
                    {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + call.module_name.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "Missing required parameter '{}' for module '{}'",
                                input.name, call.module_name
                            ),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        diagnostics
    }

    /// Validate a module argument value against its expected type
    fn validate_module_arg_type(&self, type_expr: &TypeExpr, value: &Value) -> Option<String> {
        match (type_expr, value) {
            // CIDR type validation
            (TypeExpr::Cidr, Value::String(s)) => validate_cidr(s).err(),
            // List of CIDR type validation
            (TypeExpr::List(inner), Value::List(items)) => {
                if let TypeExpr::Cidr = inner.as_ref() {
                    for (i, item) in items.iter().enumerate() {
                        if let Value::String(s) = item {
                            if let Err(e) = validate_cidr(s) {
                                return Some(format!("Element {}: {}", i, e));
                            }
                        } else {
                            return Some(format!("Element {}: expected string, got {:?}", i, item));
                        }
                    }
                }
                None
            }
            // Bool type validation
            (TypeExpr::Bool, Value::String(s)) => Some(format!(
                "Type mismatch: expected bool, got string \"{}\". Use true or false.",
                s
            )),
            // Int type validation
            (TypeExpr::Int, Value::String(s)) => Some(format!(
                "Type mismatch: expected int, got string \"{}\".",
                s
            )),
            _ => None,
        }
    }

    /// Find the position of a module call in the document
    fn find_module_call_position(&self, doc: &Document, module_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(col) = line.find(&pattern) {
                return Some((line_idx as u32, col as u32));
            }
        }
        None
    }

    /// Find the position of an argument in a module call
    fn find_module_call_arg_position(
        &self,
        doc: &Document,
        module_name: &str,
        arg_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_module_call = false;
        let module_pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if line.contains(&module_pattern) {
                in_module_call = true;
            }

            if in_module_call {
                let trimmed = line.trim_start();
                if trimmed.starts_with(arg_name)
                    && trimmed[arg_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ' ' || c == '=')
                {
                    let leading_ws = line.len() - trimmed.len();
                    return Some((line_idx as u32, leading_ws as u32));
                }

                if trimmed == "}" {
                    in_module_call = false;
                }
            }
        }
        None
    }

    /// Extract resource binding names from text (variables defined with `let binding_name = aws...`)
    fn extract_resource_bindings(&self, text: &str) -> HashSet<String> {
        let mut bindings = HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let binding_name = rest[..eq_pos].trim();
                if !binding_name.is_empty()
                    && binding_name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_')
                {
                    bindings.insert(binding_name.to_string());
                }
            }
        }
        bindings
    }

    /// Check for undefined resource references in attribute values
    fn check_undefined_references(
        &self,
        text: &str,
        defined_bindings: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            // Look for patterns like "binding_name.id" or "binding_name.name" after "="
            if let Some(eq_pos) = line.find('=') {
                let after_eq = &line[eq_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                let whitespace_len = after_eq.len() - after_eq_trimmed.len();

                // Skip if it's a string literal
                if after_eq_trimmed.starts_with('"') {
                    continue;
                }

                // Skip if it starts with "aws." (enum values like aws.Region.xxx)
                if after_eq_trimmed.starts_with("aws.") {
                    continue;
                }

                // Check if it looks like a resource reference: identifier.property
                if let Some(dot_pos) = after_eq_trimmed.find('.') {
                    let identifier = &after_eq_trimmed[..dot_pos];
                    let after_dot = &after_eq_trimmed[dot_pos + 1..];

                    // Extract property name
                    let prop_end = after_dot
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(after_dot.len());
                    let property = &after_dot[..prop_end];

                    // Check if this looks like a resource reference (e.g., main_vpc.id)
                    if (property == "id" || property == "name")
                        && !identifier.is_empty()
                        && identifier.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && !identifier.starts_with(|c: char| c.is_uppercase())
                    {
                        // Check if the binding is defined
                        if !defined_bindings.contains(identifier) {
                            let col = (eq_pos + 1 + whitespace_len) as u32;
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line: line_idx as u32,
                                        character: col,
                                    },
                                    end: Position {
                                        line: line_idx as u32,
                                        character: col + identifier.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::ERROR),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Undefined resource: '{}'. Define it with 'let {} = aws...'",
                                    identifier, identifier
                                ),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Load a module from a file or directory
    /// Handles both single-file modules and directory-based modules
    fn load_module(&self, path: &Path) -> Option<ParsedFile> {
        if path.is_dir() {
            // Directory-based module: load main.crn or merge all .crn files
            let main_path = path.join("main.crn");
            if main_path.exists() {
                let content = std::fs::read_to_string(&main_path).ok()?;
                carina_core::parser::parse(&content).ok()
            } else {
                // Merge all .crn files in the directory
                self.load_directory_module(path)
            }
        } else {
            // Single file module
            let content = std::fs::read_to_string(path).ok()?;
            carina_core::parser::parse(&content).ok()
        }
    }

    /// Load all .crn files from a directory and merge them
    fn load_directory_module(&self, dir_path: &Path) -> Option<ParsedFile> {
        let entries = std::fs::read_dir(dir_path).ok()?;
        let mut merged = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            inputs: vec![],
            outputs: vec![],
            backend: None,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "crn")
                && let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(parsed) = carina_core::parser::parse(&content)
            {
                merged.providers.extend(parsed.providers);
                merged.resources.extend(parsed.resources);
                merged.variables.extend(parsed.variables);
                merged.imports.extend(parsed.imports);
                merged.module_calls.extend(parsed.module_calls);
                merged.inputs.extend(parsed.inputs);
                merged.outputs.extend(parsed.outputs);
            }
        }

        if merged.inputs.is_empty() && merged.outputs.is_empty() {
            None
        } else {
            Some(merged)
        }
    }
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            Diagnostic {
                range: Range {
                    start: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: (col.saturating_sub(1)) as u32,
                    },
                    end: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: col as u32,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("carina".to_string()),
                message: format!("{}", pest_error),
                ..Default::default()
            }
        }
        ParseError::InvalidExpression { line, message } => Diagnostic {
            range: Range {
                start: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 0,
                },
                end: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 100,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: message.clone(),
            ..Default::default()
        },
        ParseError::UndefinedVariable(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Undefined variable: {}", name),
            ..Default::default()
        },
        ParseError::EnvVarNotSet(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("carina".to_string()),
            message: format!("Environment variable not set: {}", name),
            ..Default::default()
        },
        ParseError::InvalidResourceType(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Invalid resource type: {}", name),
            ..Default::default()
        },
        ParseError::DuplicateModule(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Duplicate module definition: {}", name),
            ..Default::default()
        },
        ParseError::ModuleNotFound(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Module not found: {}", name),
            ..Default::default()
        },
    }
}
