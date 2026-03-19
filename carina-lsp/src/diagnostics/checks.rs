//! Semantic checks: provider region, module calls, unused bindings, undefined references.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use crate::position;
use carina_core::parser::{InputParameter, ParsedFile, TypeExpr};
use carina_core::resource::Value;
use carina_core::schema::validate_ipv4_cidr;

use super::DiagnosticEngine;

impl DiagnosticEngine {
    /// Check provider region attribute using factory validate_config
    pub(super) fn check_provider_region(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for provider in &parsed.providers {
            // Find the matching factory for this provider
            if let Some(factory) = self.factories.iter().find(|f| f.name() == provider.name)
                && let Err(e) = factory.validate_config(&provider.attributes)
                && let Some((line, col)) = self.find_provider_region_position(doc, &provider.name)
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
                    message: format!("provider {}: {}", provider.name, e),
                    ..Default::default()
                });
            }
        }
        diagnostics
    }

    /// Find the position of the region attribute in a provider block
    pub(super) fn find_provider_region_position(
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
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_provider = false;
                }
            }
        }
        None
    }

    /// Check module calls against imported module definitions
    pub(super) fn check_module_calls(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        base_path: &std::path::Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Build a map of imported modules: alias -> input parameters
        let mut imported_modules: HashMap<String, Vec<InputParameter>> = HashMap::new();

        for import in &parsed.imports {
            let module_path = base_path.join(&import.path);
            if let Some(module_parsed) = carina_core::module_resolver::load_module(&module_path) {
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
    pub(super) fn validate_module_arg_type(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
    ) -> Option<String> {
        match (type_expr, value) {
            // CIDR type validation
            (TypeExpr::Cidr, Value::String(s)) => validate_ipv4_cidr(s).err(),
            // List of CIDR type validation
            (TypeExpr::List(inner), Value::List(items)) => {
                if let TypeExpr::Cidr = inner.as_ref() {
                    for (i, item) in items.iter().enumerate() {
                        if let Value::String(s) = item {
                            if let Err(e) = validate_ipv4_cidr(s) {
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
            // Float type validation
            (TypeExpr::Float, Value::String(s)) => Some(format!(
                "Type mismatch: expected float, got string \"{}\".",
                s
            )),
            _ => None,
        }
    }

    /// Find the position of a module call in the document
    pub(super) fn find_module_call_position(
        &self,
        doc: &Document,
        module_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(&pattern) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Find the position of an argument in a module call
    pub(super) fn find_module_call_arg_position(
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
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_module_call = false;
                }
            }
        }
        None
    }

    /// Check for unused `let` bindings and emit warnings.
    pub(super) fn check_unused_bindings(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let unused_bindings = carina_core::validation::check_unused_bindings(parsed);
        if unused_bindings.is_empty() {
            return Vec::new();
        }

        let text = doc.text();
        let mut diagnostics = Vec::new();

        for binding_name in &unused_bindings {
            if let Some((line, col)) = self.find_let_binding_position(&text, binding_name) {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line,
                            character: col,
                        },
                        end: Position {
                            line,
                            character: col + binding_name.len() as u32,
                        },
                    },
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("carina".to_string()),
                    message: format!(
                        "Unused let binding '{}'. Consider using an anonymous resource instead.",
                        binding_name
                    ),
                    ..Default::default()
                });
            }
        }

        diagnostics
    }

    /// Find the position of a `let` binding name in the source text.
    pub(super) fn find_let_binding_position(
        &self,
        text: &str,
        binding_name: &str,
    ) -> Option<(u32, u32)> {
        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let name = rest[..eq_pos].trim();
                if name == binding_name {
                    // Find the column of the binding name in the original line
                    let let_byte_pos = line.find("let ").unwrap();
                    let let_char_pos = position::byte_offset_to_char_offset(line, let_byte_pos);
                    let name_col = let_char_pos + 4; // "let " is 4 chars
                    return Some((line_idx as u32, name_col));
                }
            }
        }
        None
    }

    /// Extract resource binding names from text (variables defined with `let binding_name = aws...` or `let binding_name = read aws...`)
    pub(super) fn extract_resource_bindings(&self, text: &str) -> HashSet<String> {
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

    /// Check output blocks for type mismatches and undefined binding references.
    pub(super) fn check_output_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Collect defined binding names from parsed resources
        let mut defined_bindings: HashSet<String> = HashSet::new();
        for resource in &parsed.resources {
            if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
                defined_bindings.insert(binding_name.clone());
            }
        }

        for output in &parsed.outputs {
            if let Some(value) = &output.value {
                // Check for undefined binding references
                if let Value::ResourceRef { binding_name, .. } = value
                    && !defined_bindings.contains(binding_name.as_str())
                    && let Some((line, col)) = self.find_output_value_position(doc, &output.name)
                {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line,
                                character: col,
                            },
                            end: Position {
                                line,
                                character: col + binding_name.len() as u32,
                            },
                        },
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("carina".to_string()),
                        message: format!(
                            "Undefined resource '{}' in output '{}'. Define it with 'let {} = ...'",
                            binding_name, output.name, binding_name
                        ),
                        ..Default::default()
                    });
                }

                // Type validation
                if let Some(type_error) =
                    self.validate_output_type(&output.type_expr, value, &output.name)
                    && let Some((line, col)) = self.find_output_param_position(doc, &output.name)
                {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line,
                                character: col,
                            },
                            end: Position {
                                line,
                                character: col + output.name.len() as u32,
                            },
                        },
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("carina".to_string()),
                        message: type_error,
                        ..Default::default()
                    });
                }
            }
        }

        diagnostics
    }

    /// Validate an output value against its declared type.
    fn validate_output_type(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
        output_name: &str,
    ) -> Option<String> {
        match (type_expr, value) {
            // ResourceRef is always allowed (type is resolved at runtime)
            (_, Value::ResourceRef { .. }) => None,
            // String type checks
            (TypeExpr::String, Value::Bool(b)) => Some(format!(
                "Type mismatch in output '{}': expected string, got bool ({})",
                output_name, b
            )),
            (TypeExpr::String, Value::Int(n)) => Some(format!(
                "Type mismatch in output '{}': expected string, got int ({})",
                output_name, n
            )),
            (TypeExpr::String, Value::Float(f)) => Some(format!(
                "Type mismatch in output '{}': expected string, got float ({})",
                output_name, f
            )),
            // Bool type checks
            (TypeExpr::Bool, Value::String(s)) => Some(format!(
                "Type mismatch in output '{}': expected bool, got string \"{}\". Use true or false.",
                output_name, s
            )),
            (TypeExpr::Bool, Value::Int(n)) => Some(format!(
                "Type mismatch in output '{}': expected bool, got int ({})",
                output_name, n
            )),
            // Int type checks
            (TypeExpr::Int, Value::String(s)) => Some(format!(
                "Type mismatch in output '{}': expected int, got string \"{}\"",
                output_name, s
            )),
            (TypeExpr::Int, Value::Bool(b)) => Some(format!(
                "Type mismatch in output '{}': expected int, got bool ({})",
                output_name, b
            )),
            // Float type checks
            (TypeExpr::Float, Value::String(s)) => Some(format!(
                "Type mismatch in output '{}': expected float, got string \"{}\"",
                output_name, s
            )),
            (TypeExpr::Float, Value::Bool(b)) => Some(format!(
                "Type mismatch in output '{}': expected float, got bool ({})",
                output_name, b
            )),
            _ => None,
        }
    }

    /// Find the position of an output parameter name in the document.
    fn find_output_param_position(&self, doc: &Document, param_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_output_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("output ") && trimmed.contains('{') {
                in_output_block = true;
                continue;
            }

            if in_output_block {
                if trimmed == "}" {
                    in_output_block = false;
                    continue;
                }

                // Look for "param_name:" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Find the position of the value expression in an output parameter line.
    fn find_output_value_position(&self, doc: &Document, param_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_output_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("output ") && trimmed.contains('{') {
                in_output_block = true;
                continue;
            }

            if in_output_block {
                if trimmed == "}" {
                    in_output_block = false;
                    continue;
                }

                // Look for "param_name: type = value" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    // Find the "=" and return position after it
                    if let Some(eq_byte_pos) = line.find('=') {
                        let after_eq = &line[eq_byte_pos + 1..];
                        let trimmed_after = after_eq.trim_start();
                        // Whitespace after '=' is ASCII, so byte diff == char count
                        let ws_after_eq = after_eq.len() - trimmed_after.len();
                        let value_col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                            + 1
                            + ws_after_eq as u32;
                        return Some((line_idx as u32, value_col));
                    }
                }
            }
        }
        None
    }

    /// Check for undefined resource references in attribute values
    pub(super) fn check_undefined_references(
        &self,
        text: &str,
        defined_bindings: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            // Look for patterns like "binding_name.id" or "binding_name.name" after "="
            if let Some(eq_byte_pos) = line.find('=') {
                let after_eq = &line[eq_byte_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                // Whitespace after '=' is ASCII spaces, so byte diff == char count
                let whitespace_chars = after_eq.len() - after_eq_trimmed.len();

                // Skip if it's a string literal
                if after_eq_trimmed.starts_with('"') {
                    continue;
                }

                // Skip if it starts with a provider prefix (enum values like aws.Region.xxx)
                let is_provider_prefix = self
                    .provider_names
                    .iter()
                    .any(|name| after_eq_trimmed.starts_with(&format!("{}.", name)));
                if is_provider_prefix {
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

                    // Check if this looks like a resource reference (e.g., main_vpc.id, bucket.arn)
                    if !identifier.is_empty()
                        && !property.is_empty()
                        && identifier.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && !identifier.starts_with(|c: char| c.is_uppercase())
                    {
                        // Check if the binding is defined
                        if !defined_bindings.contains(identifier) {
                            let col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                                + 1
                                + whitespace_chars as u32;
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
