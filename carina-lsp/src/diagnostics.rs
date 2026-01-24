use std::collections::HashSet;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use carina_core::parser::ParseError;
use carina_core::resource::Value;
use carina_provider_aws::schemas::{s3, vpc};

pub struct DiagnosticEngine {
    valid_resource_types: HashSet<String>,
}

impl DiagnosticEngine {
    pub fn new() -> Self {
        let mut valid_resource_types = HashSet::new();

        // S3 resources
        valid_resource_types.insert("s3_bucket".to_string());

        // VPC resources
        valid_resource_types.insert("vpc".to_string());
        valid_resource_types.insert("subnet".to_string());
        valid_resource_types.insert("internet_gateway".to_string());
        valid_resource_types.insert("route_table".to_string());
        valid_resource_types.insert("route".to_string());
        valid_resource_types.insert("security_group".to_string());
        valid_resource_types.insert("security_group.ingress_rule".to_string());
        valid_resource_types.insert("security_group.egress_rule".to_string());

        Self {
            valid_resource_types,
        }
    }

    pub fn analyze(&self, doc: &Document) -> Vec<Diagnostic> {
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
            // Check resource types
            for resource in &parsed.resources {
                if !self
                    .valid_resource_types
                    .contains(&resource.id.resource_type)
                {
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
                                    character: col + resource.id.resource_type.len() as u32 + 4, // "aws." prefix
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("carina".to_string()),
                            message: format!(
                                "Unknown resource type: aws.{}",
                                resource.id.resource_type.replace('_', ".")
                            ),
                            ..Default::default()
                        });
                    }
                }

                // Semantic validation using schema
                let schema = self.get_schema_for_type(&resource.id.resource_type);
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
            _ => None,
        }
    }

    fn find_resource_position(&self, doc: &Document, resource_type: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        // Convert resource_type back to DSL format: vpc -> aws.vpc, s3_bucket -> aws.s3.bucket
        let dsl_type = if resource_type == "s3_bucket" {
            "aws.s3.bucket".to_string()
        } else {
            format!("aws.{}", resource_type.replace('_', "."))
        };

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
    }
}
