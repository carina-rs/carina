use std::collections::HashSet;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use carina_core::parser::ParseError;
use carina_core::providers::{ec2, s3};
use carina_core::resource::Value;

pub struct DiagnosticEngine {
    valid_resource_types: HashSet<String>,
}

impl DiagnosticEngine {
    pub fn new() -> Self {
        let mut valid_resource_types = HashSet::new();

        // S3 resources
        valid_resource_types.insert("s3_bucket".to_string());

        // EC2/VPC resources
        valid_resource_types.insert("vpc".to_string());
        valid_resource_types.insert("subnet".to_string());
        valid_resource_types.insert("internet_gateway".to_string());
        valid_resource_types.insert("route_table".to_string());
        valid_resource_types.insert("security_group".to_string());
        valid_resource_types.insert("security_group.ingress_rule".to_string());
        valid_resource_types.insert("security_group.egress_rule".to_string());

        Self {
            valid_resource_types,
        }
    }

    pub fn analyze(&self, doc: &Document) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Parse errors
        if let Some(error) = doc.parse_error() {
            diagnostics.push(parse_error_to_diagnostic(error));
        }

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

                // Check for ResourceRef or resource binding used where String is expected
                let schema = self.get_schema_for_type(&resource.id.resource_type);
                if let Some(schema) = schema {
                    for (attr_name, attr_value) in &resource.attributes {
                        if attr_name.starts_with('_') {
                            continue; // Skip internal attributes
                        }

                        // Check if attr expects a String type
                        let expects_string = schema
                            .attributes
                            .get(attr_name)
                            .map(|s| {
                                matches!(s.attr_type, carina_core::schema::AttributeType::String)
                            })
                            .unwrap_or(false);

                        if !expects_string {
                            continue;
                        }

                        // Determine the suggested attribute based on the attribute name
                        let suggested_attr = if attr_name.ends_with("_id") {
                            "id"
                        } else {
                            "name"
                        };

                        // Check for ResourceRef (e.g., vpc.id when misused)
                        if let Value::ResourceRef(binding, _) = attr_value
                            && let Some((line, col)) =
                                self.find_attribute_value_position(doc, attr_name, binding)
                        {
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position { line, character: col },
                                    end: Position {
                                        line,
                                        character: col + binding.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Expected string, got resource reference '{}'. Did you mean '{}.{}'?",
                                    binding, binding, suggested_attr
                                ),
                                ..Default::default()
                            });
                        }

                        // Check for resource binding placeholder ${binding}
                        // This happens when you write `vpc_id = vpc` instead of `vpc_id = vpc.id`
                        if let Value::String(s) = attr_value
                            && let Some(binding) =
                                s.strip_prefix("${").and_then(|s| s.strip_suffix("}"))
                            && let Some((line, col)) =
                                self.find_attribute_value_position(doc, attr_name, binding)
                        {
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position { line, character: col },
                                    end: Position {
                                        line,
                                        character: col + binding.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message: format!(
                                    "Expected string, got resource reference '{}'. Did you mean '{}.{}'?",
                                    binding, binding, suggested_attr
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

    fn get_schema_for_type(
        &self,
        resource_type: &str,
    ) -> Option<carina_core::schema::ResourceSchema> {
        match resource_type {
            "s3_bucket" => Some(s3::bucket_schema()),
            "vpc" => Some(ec2::vpc_schema()),
            "subnet" => Some(ec2::subnet_schema()),
            "internet_gateway" => Some(ec2::internet_gateway_schema()),
            "route_table" => Some(ec2::route_table_schema()),
            "security_group" => Some(ec2::security_group_schema()),
            "security_group.ingress_rule" => Some(ec2::security_group_ingress_rule_schema()),
            "security_group.egress_rule" => Some(ec2::security_group_egress_rule_schema()),
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

    fn find_attribute_value_position(
        &self,
        doc: &Document,
        attr_name: &str,
        value: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Must start with attr_name followed by whitespace or '='
            // This ensures we match "vpc = ..." but not "let vpc = ..."
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after_attr = &trimmed[attr_name.len()..];
            // After attr_name, must have whitespace or '=' (not part of larger identifier)
            if !after_attr.starts_with(' ') && !after_attr.starts_with('=') {
                continue;
            }

            if let Some(eq_pos) = line.find('=') {
                let after_eq = &line[eq_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                // Check if value appears at the start (not as substring of larger identifier)
                if after_eq_trimmed.starts_with(value) {
                    let next_char_idx = value.len();
                    let is_end_of_value = after_eq_trimmed.len() == next_char_idx
                        || !after_eq_trimmed[next_char_idx..]
                            .chars()
                            .next()
                            .map(|c| c.is_alphanumeric() || c == '_')
                            .unwrap_or(false);
                    if is_end_of_value {
                        // Calculate column: position where value starts
                        let whitespace_len = after_eq.len() - after_eq_trimmed.len();
                        let col = eq_pos + 1 + whitespace_len;
                        return Some((line_idx as u32, col as u32));
                    }
                }
            }
        }
        None
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
