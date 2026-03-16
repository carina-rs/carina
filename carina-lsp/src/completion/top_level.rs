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
                label: "input".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("input {\n    ${1:param}: ${2:type}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module input parameters".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "output".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("output {\n    ${1:name}: ${2:type} = ${3:value}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module output values".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "import".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("import \"${1:./modules/name/main.crn}\" as ${2:module_name}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Import a module".to_string()),
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
            CompletionItem {
                label: "ref".to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                insert_text: Some("ref(${1:aws.ec2.vpc})".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Typed resource reference".to_string()),
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

    pub(super) fn extract_input_parameters(&self, text: &str) -> Vec<(String, String)> {
        let mut params = Vec::new();
        let mut in_input_block = false;
        let mut brace_depth = 0;

        for line in text.lines() {
            let trimmed = line.trim();

            // Check for "input {" block start
            if trimmed.starts_with("input ") && trimmed.contains('{') {
                in_input_block = true;
                brace_depth = 1;
                continue;
            }

            if in_input_block {
                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_input_block = false;
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

    /// Extract resource binding names from text (variables defined with `let binding_name = aws...`)
    pub(super) fn extract_resource_bindings(&self, text: &str) -> Vec<String> {
        let mut bindings = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            // Parse: let binding_name = ...
            if let Some(rest) = trimmed.strip_prefix("let ")
                && let Some(eq_pos) = rest.find('=')
            {
                let binding_name = rest[..eq_pos].trim();
                if !binding_name.is_empty()
                    && binding_name
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_')
                {
                    bindings.push(binding_name.to_string());
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
                // Extract input parameters from the module
                for input in &parsed.inputs {
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

    /// Provide completions for input parameters in the current file (after "input.")
    pub(super) fn input_parameter_completions(&self, text: &str) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Extract input parameters from text (works even with incomplete code)
        let input_params = self.extract_input_parameters(text);
        for (name, type_hint) in input_params {
            let required_marker = if type_hint.contains('=') {
                ""
            } else {
                " (required)"
            };
            completions.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(format!("{}{}", type_hint, required_marker)),
                insert_text: Some(name),
                ..Default::default()
            });
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
                format!(
                    "ref({}.{})",
                    resource_path.provider, resource_path.resource_type
                )
            }
        }
    }

    /// Find the import path for a given module name from the import statements
    pub(super) fn find_module_import_path(&self, module_name: &str, text: &str) -> Option<String> {
        for line in text.lines() {
            let trimmed = line.trim();
            // Parse: import "path" as name
            if let Some(rest) = trimmed.strip_prefix("import ")
                && let Some(quote_start) = rest.find('"')
                && let Some(quote_end) = rest[quote_start + 1..].find('"')
            {
                let path = &rest[quote_start + 1..quote_start + 1 + quote_end];
                // Look for "as module_name"
                let after_path = &rest[quote_start + 1 + quote_end + 1..];
                if let Some(as_pos) = after_path.find(" as ") {
                    let alias = after_path[as_pos + 4..].trim();
                    if alias == module_name {
                        return Some(path.to_string());
                    }
                }
            }
        }
        None
    }
}
