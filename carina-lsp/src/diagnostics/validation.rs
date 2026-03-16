//! Struct and block syntax validation for resource attributes.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use crate::position;
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;

use super::DiagnosticEngine;

impl DiagnosticEngine {
    pub(super) fn validate_struct_value(
        &self,
        doc: &Document,
        attr_name: &str,
        value: &Value,
        fields: &[carina_core::schema::StructField],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        let maps: Vec<&HashMap<String, Value>> = match value {
            Value::Map(map) => vec![map],
            Value::List(items) => items
                .iter()
                .filter_map(|item| {
                    if let Value::Map(map) = item {
                        Some(map)
                    } else {
                        None
                    }
                })
                .collect(),
            _ => return diagnostics,
        };

        let field_names: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        // Also include block_names as valid field names
        let block_name_to_canonical: HashMap<&str, &str> = fields
            .iter()
            .filter_map(|f| f.block_name.as_deref().map(|bn| (bn, f.name.as_str())))
            .collect();
        let field_map: HashMap<&str, &carina_core::schema::StructField> =
            fields.iter().map(|f| (f.name.as_str(), f)).collect();

        for (map_index, map) in maps.iter().enumerate() {
            for (key, val) in *map {
                let all_positions = self.find_all_nested_field_positions(doc, attr_name, key);
                if let Some(Some((line, col))) = all_positions.get(map_index) {
                    let (line, col) = (*line, *col);
                    // Resolve block_name to canonical name
                    let canonical_key = block_name_to_canonical
                        .get(key.as_str())
                        .copied()
                        .unwrap_or(key.as_str());

                    // Check for unknown fields
                    if !field_names.contains(canonical_key) {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line,
                                    character: col,
                                },
                                end: Position {
                                    line,
                                    character: col + key.len() as u32,
                                },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("carina".to_string()),
                            message: format!("Unknown field '{}' in '{}'", key, attr_name),
                            ..Default::default()
                        });
                        continue;
                    }

                    // Type validation for known fields
                    if let Some(field) = field_map.get(canonical_key) {
                        // Skip validation for ResourceRef values (resolved at runtime)
                        let type_error = if matches!(val, Value::ResourceRef { .. }) {
                            None
                        } else {
                            field.field_type.validate(val).err().map(|e| e.to_string())
                        };

                        if let Some(message) = type_error {
                            diagnostics.push(Diagnostic {
                                range: Range {
                                    start: Position {
                                        line,
                                        character: col,
                                    },
                                    end: Position {
                                        line,
                                        character: col + key.len() as u32,
                                    },
                                },
                                severity: Some(DiagnosticSeverity::WARNING),
                                source: Some("carina".to_string()),
                                message,
                                ..Default::default()
                            });
                        }

                        // Recurse into nested Struct / List<Struct> fields
                        let nested_fields = match &field.field_type {
                            carina_core::schema::AttributeType::Struct { fields, .. } => {
                                Some(fields)
                            }
                            carina_core::schema::AttributeType::List(inner) => {
                                match inner.as_ref() {
                                    carina_core::schema::AttributeType::Struct {
                                        fields, ..
                                    } => Some(fields),
                                    _ => None,
                                }
                            }
                            _ => None,
                        };
                        if let Some(nested) = nested_fields {
                            diagnostics.extend(self.validate_struct_value(doc, key, val, nested));
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Find the start positions of ALL blocks with the given name.
    /// Returns `(line, col)` for each occurrence of `block_name {`.
    pub(super) fn find_all_block_positions(
        &self,
        doc: &Document,
        block_name: &str,
    ) -> Vec<(u32, u32)> {
        let text = doc.text();
        let mut positions = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Look for "block_name {" (without "=")
            if trimmed.starts_with(block_name) && !trimmed.contains('=') {
                let after = trimmed[block_name.len()..].trim();
                if after.starts_with('{') {
                    positions.push((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }

        positions
    }

    /// Find position of list literal syntax (`attr_name = [`) in source text.
    /// Returns `(line, col)` if found.
    pub(super) fn find_list_literal_position(
        &self,
        doc: &Document,
        attr_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after = &trimmed[attr_name.len()..];
            // Must be followed by whitespace or '=' (not part of a longer identifier)
            if !after.starts_with(' ') && !after.starts_with('=') {
                continue;
            }
            // Check for `= [` pattern (list literal)
            let after_trimmed = after.trim_start();
            if let Some(rest) = after_trimmed.strip_prefix('=') {
                let rest_trimmed = rest.trim_start();
                if rest_trimmed.starts_with('[') {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Check List<Struct> attributes for list literal syntax and suggest block syntax.
    pub(super) fn check_list_struct_syntax(
        &self,
        doc: &Document,
        resource_attrs: &HashMap<String, Value>,
        schema: &ResourceSchema,
    ) -> Vec<Diagnostic> {
        use carina_core::schema::AttributeType;

        let mut diagnostics = Vec::new();

        for (attr_name, attr_schema) in &schema.attributes {
            // Only check List<Struct> attributes
            let is_list_struct = matches!(
                &attr_schema.attr_type,
                AttributeType::List(inner) if matches!(inner.as_ref(), AttributeType::Struct { .. })
            );
            if !is_list_struct {
                continue;
            }

            // Only check attributes that actually exist in the resource
            if !resource_attrs.contains_key(attr_name) {
                continue;
            }

            if let Some((line, col)) = self.find_list_literal_position(doc, attr_name) {
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
                    severity: Some(DiagnosticSeverity::HINT),
                    source: Some("carina".to_string()),
                    message: format!(
                        "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                        attr_name, attr_name, attr_name
                    ),
                    ..Default::default()
                });
            }
        }

        diagnostics
    }

    /// Find the positions of a field inside ALL matching nested blocks
    /// Returns a Vec with one entry per block occurrence. Each entry is `Some((line, col))`
    /// if the field was found in that block, or `None` if not.
    pub(super) fn find_all_nested_field_positions(
        &self,
        doc: &Document,
        block_name: &str,
        field_name: &str,
    ) -> Vec<Option<(u32, u32)>> {
        let text = doc.text();
        let mut positions = Vec::new();
        let mut in_block = false;
        let mut brace_depth = 0;
        let mut found_in_current_block: Option<(u32, u32)> = None;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Look for "block_name {" (without "=") or "block_name = {" (with "=")
            if !in_block && trimmed.starts_with(block_name) {
                let after_name = trimmed[block_name.len()..].trim();
                let has_opening_brace = if after_name.starts_with('{') {
                    true
                } else if let Some(after_eq) = after_name.strip_prefix('=') {
                    after_eq.trim().starts_with('{')
                } else {
                    false
                };
                if has_opening_brace {
                    in_block = true;
                    brace_depth = 1;
                    found_in_current_block = None;
                    continue;
                }
            }

            if in_block {
                // Check for field name BEFORE counting braces so that lines like
                // "field_name {" are detected at the current depth (before '{' increments it)
                if brace_depth == 1
                    && let Some(after) = trimmed.strip_prefix(field_name)
                    && (after.starts_with(' ') || after.starts_with('=') || after.starts_with('{'))
                {
                    found_in_current_block =
                        Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_block = false;
                            positions.push(found_in_current_block);
                            found_in_current_block = None;
                            break;
                        }
                    }
                }
            }
        }

        // Handle case where file ends while still in a block
        if in_block {
            positions.push(found_in_current_block);
        }

        positions
    }
}
