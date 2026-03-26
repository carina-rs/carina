//! Top-level, module, and input parameter completions.

use std::path::Path;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::parser;

use super::CompletionProvider;

impl CompletionProvider {
    pub(super) fn top_level_completions(
        &self,
        position: Position,
        text: &str,
    ) -> Vec<CompletionItem> {
        // Calculate the range for resource type replacements
        // Find where the current word/prefix starts on this line
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let prefix_start = if line_idx < lines.len() {
            let line = lines[line_idx];
            let before_cursor: String = line.chars().take(col).collect();
            // Find where the identifier starts (going backwards from cursor)
            // Stop at whitespace, but continue through dots
            let mut start = col;
            for (i, c) in before_cursor.chars().rev().enumerate() {
                if c.is_whitespace() {
                    break;
                }
                start = col - i - 1;
            }
            start as u32
        } else {
            position.character
        };

        let replacement_range = Range {
            start: Position {
                line: position.line,
                character: prefix_start,
            },
            end: position,
        };

        let mut completions = vec![
            CompletionItem {
                label: "provider".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("provider ${1:aws} {\n    region = aws.Region.${2:ap_northeast_1}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define a provider block".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "let".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("let ${1:name} = ".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define a named resource or variable".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "read".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("let ${1:name} = read ${2:aws.s3.bucket} {\n    name = \"${3:existing-resource}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Read existing resource (data source)".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "arguments".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("arguments {\n    ${1:param}: ${2:type}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module argument parameters".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "attributes".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("attributes {\n    ${1:name}: ${2:type} = ${3:value}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module attribute values".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "let import".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("let ${1:module_name} = import \"${2:./modules/name}\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Import a module".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "import".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("import {\n    to = ${1:awscc.ec2.vpc} \"${2:name}\"\n    id = \"${3:resource-id}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Import existing resource into state".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "removed".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("removed {\n    from = ${1:awscc.ec2.vpc} \"${2:name}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Remove resource from state without destroying".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "moved".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("moved {\n    from = ${1:awscc.ec2.vpc} \"${2:old-name}\"\n    to   = ${3:awscc.ec2.vpc} \"${4:new-name}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Move/rename resource in state".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "backend".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("backend s3 {\n    bucket = \"${1:my-carina-state}\"\n    key    = \"${2:prod/carina.crnstate}\"\n    region = aws.Region.${3:ap_northeast_1}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Configure state backend (S3)".to_string()),
                ..Default::default()
            },
        ];

        // Generate resource type completions from schemas
        for (resource_type, schema) in self.schemas.iter() {
            let description = schema
                .description
                .as_deref()
                .unwrap_or("Resource")
                .to_string();

            // Build snippet with required attributes
            let mut snippet = format!("{} {{\n", resource_type);
            let mut tab_stop = 1;
            for attr in schema.attributes.values() {
                if attr.required {
                    snippet.push_str(&format!("    {} = ${{{}}}\n", attr.name, tab_stop));
                    tab_stop += 1;
                }
            }
            snippet.push('}');

            completions.push(CompletionItem {
                label: resource_type.clone(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: snippet,
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some(description),
                ..Default::default()
            });
        }

        completions
    }

    pub(super) fn extract_argument_parameters(&self, text: &str) -> Vec<(String, String)> {
        let mut params = Vec::new();
        let mut in_arguments_block = false;
        let mut brace_depth = 0;

        for line in text.lines() {
            let trimmed = line.trim();

            // Check for "arguments {" block start
            if trimmed.starts_with("arguments ") && trimmed.contains('{') {
                in_arguments_block = true;
                brace_depth = 1;
                continue;
            }

            if in_arguments_block {
                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_arguments_block = false;
                            break;
                        }
                    }
                }

                // Parse parameter: "name: type" or "name: type = default"
                if brace_depth > 0 && trimmed.contains(':') && !trimmed.starts_with('#') {
                    let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let name = parts[0].trim().to_string();
                        let rest = parts[1].trim();
                        // Extract type (before '=' if present)
                        let type_hint = if let Some(eq_pos) = rest.find('=') {
                            rest[..eq_pos].trim().to_string()
                        } else {
                            rest.to_string()
                        };
                        if !name.is_empty() {
                            params.push((name, type_hint));
                        }
                    }
                }
            }
        }

        params
    }

    /// Extract resource binding names and their resource types from text
    /// (variables defined with `let binding_name = awscc.ec2.vpc {`)
    /// Returns Vec<(binding_name, resource_type)> where resource_type is the schema key
    /// (e.g., "awscc.ec2.vpc")
    pub(super) fn extract_resource_bindings(&self, text: &str) -> Vec<(String, String)> {
        let mut bindings = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            // Parse: let binding_name = <resource_type> {
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let binding_name = rest[..eq_pos].trim();
                if !binding_name.is_empty()
                    && binding_name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_')
                {
                    // Extract resource type from the part after "="
                    let after_eq = rest[eq_pos + 1..].trim();
                    if let Some(resource_type) = self.extract_resource_type(after_eq) {
                        bindings.push((binding_name.to_string(), resource_type));
                    } else {
                        // Fallback: include binding with empty resource type
                        bindings.push((binding_name.to_string(), String::new()));
                    }
                }
            }
        }
        bindings
    }

    pub(super) fn module_parameter_completions(
        &self,
        module_name: &str,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Find the import statement for this module
        let import_path = self.find_module_import_path(module_name, text);

        if let Some(import_path) = import_path
            && let Some(base) = base_path
        {
            let module_path = base.join(&import_path);
            if let Some(parsed) = carina_core::module_resolver::load_module(&module_path) {
                // Extract argument parameters from the module
                for input in &parsed.arguments {
                    let type_str = self.format_type_expr(&input.type_expr);
                    let required_marker = if input.default.is_some() {
                        ""
                    } else {
                        " (required)"
                    };

                    let trigger_suggest = Command {
                        title: "Trigger Suggest".to_string(),
                        command: "editor.action.triggerSuggest".to_string(),
                        arguments: None,
                    };

                    completions.push(CompletionItem {
                        label: input.name.clone(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: Some(format!("{}{}", type_str, required_marker)),
                        insert_text: Some(format!("{} = ", input.name)),
                        command: Some(trigger_suggest),
                        ..Default::default()
                    });
                }
            }
        }

        completions
    }

    pub(super) fn format_type_expr(&self, type_expr: &parser::TypeExpr) -> String {
        match type_expr {
            parser::TypeExpr::String => "string".to_string(),
            parser::TypeExpr::Bool => "bool".to_string(),
            parser::TypeExpr::Int => "int".to_string(),
            parser::TypeExpr::Float => "float".to_string(),
            parser::TypeExpr::Cidr => "cidr".to_string(),
            parser::TypeExpr::List(inner) => format!("list({})", self.format_type_expr(inner)),
            parser::TypeExpr::Map(inner) => format!("map({})", self.format_type_expr(inner)),
            parser::TypeExpr::Ref(resource_path) => {
                format!("{}.{}", resource_path.provider, resource_path.resource_type)
            }
        }
    }

    /// Find the import path for a given module name from let import bindings
    pub(super) fn find_module_import_path(&self, module_name: &str, text: &str) -> Option<String> {
        for line in text.lines() {
            let trimmed = line.trim();
            // Parse: let name = import "path"
            if let Some(rest) = trimmed.strip_prefix("let ") {
                let rest = rest.trim_start();
                if let Some(eq_pos) = rest.find('=') {
                    let alias = rest[..eq_pos].trim();
                    let after_eq = rest[eq_pos + 1..].trim();
                    if let Some(import_rest) = after_eq.strip_prefix("import ") {
                        let import_rest = import_rest.trim();
                        if let Some(path_start) = import_rest.find('"')
                            && let Some(path_end) = import_rest[path_start + 1..].find('"')
                        {
                            let path = &import_rest[path_start + 1..path_start + 1 + path_end];
                            if alias == module_name {
                                return Some(path.to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }
}
