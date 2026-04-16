//! Completion provider for the Carina LSP.

mod top_level;
mod values;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Command, CompletionItem, CompletionItemKind, Position};

use crate::document::Document;
use carina_core::schema::{AttributeType, CompletionValue, ResourceSchema, StructField};

pub struct CompletionProvider {
    schemas: Arc<HashMap<String, ResourceSchema>>,
    provider_names: Vec<String>,
    region_completions_data: Vec<CompletionValue>,
    /// Resource type patterns sorted longest-first for matching
    resource_type_patterns: Vec<String>,
    /// Custom type names from provider validators (e.g., "arn", "vpc_id")
    custom_type_names: Vec<String>,
}

impl CompletionProvider {
    pub fn new(
        schemas: Arc<HashMap<String, ResourceSchema>>,
        provider_names: Vec<String>,
        region_completions_data: Vec<CompletionValue>,
        custom_type_names: Vec<String>,
    ) -> Self {
        // Build sorted resource type patterns from schema keys (longest first)
        let mut resource_type_patterns: Vec<String> = schemas.keys().cloned().collect();
        resource_type_patterns.sort_by_key(|b| std::cmp::Reverse(b.len()));

        Self {
            schemas,
            provider_names,
            region_completions_data,
            resource_type_patterns,
            custom_type_names,
        }
    }

    pub fn complete(
        &self,
        doc: &Document,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let text = doc.text();
        let context = self.get_completion_context(&text, position);

        match context {
            CompletionContext::TopLevel => self.top_level_completions(position, &text, base_path),
            CompletionContext::InsideResourceBlock { resource_type } => {
                self.attribute_completions_for_type(&resource_type)
            }
            CompletionContext::InsideUpstreamStateBlock => self.upstream_state_block_completions(),
            CompletionContext::InsideModuleCall { module_name } => {
                self.module_parameter_completions(&module_name, &text, base_path)
            }
            CompletionContext::AfterEquals {
                resource_type,
                attr_name,
                current_binding,
            } => self.value_completions_for_attr(
                &resource_type,
                &attr_name,
                &text,
                current_binding.as_deref(),
                position,
            ),
            CompletionContext::InsideStructBlock {
                resource_type,
                attr_path,
            } => self.struct_field_completions(&resource_type, &attr_path),
            CompletionContext::AfterEqualsInStruct {
                resource_type,
                attr_path,
                field_name,
            } => self.value_completions_for_struct_field(&resource_type, &attr_path, &field_name),
            CompletionContext::InsideProviderBlock { .. } => self.provider_block_completions(),
            CompletionContext::AfterProviderRegion { provider_name } => {
                self.region_completions_for_provider(&provider_name)
            }
            CompletionContext::InTypePosition => self.ref_type_completions(position, &text),
            CompletionContext::InsideImportPath { partial_path } => {
                self.import_path_completions(&partial_path, base_path)
            }
            CompletionContext::None => vec![],
        }
    }

    fn get_completion_context(&self, text: &str, position: Position) -> CompletionContext {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;

        if line_idx >= lines.len() {
            return CompletionContext::TopLevel;
        }

        let current_line = lines[line_idx];
        let col = position.character as usize;
        let prefix: String = current_line.chars().take(col).collect();

        // Check if we're typing after "<provider>.Region." or "<provider>.Region"
        for provider_name in &self.provider_names {
            let dot_pattern = format!("{}.Region.", provider_name);
            let end_pattern = format!("{}.Region", provider_name);
            if prefix.contains(&dot_pattern) || prefix.ends_with(&end_pattern) {
                return CompletionContext::AfterProviderRegion {
                    provider_name: provider_name.clone(),
                };
            }
        }

        // Check if cursor is inside an import path string
        // e.g., let x = import './modules/|'
        if let Some(import_pos) = prefix.find("import ") {
            let after_import = &prefix[import_pos + 7..];
            let trimmed = after_import.trim_start();
            if (trimmed.starts_with('\'') || trimmed.starts_with('"'))
                && !trimmed[1..].contains(trimmed.chars().next().unwrap())
            {
                // Inside an unclosed quote after "import"
                let partial_path = &trimmed[1..]; // strip opening quote
                return CompletionContext::InsideImportPath {
                    partial_path: partial_path.to_string(),
                };
            }
        }

        // Check if we're in a type position after ":" in arguments/attributes blocks.
        // e.g., "vpc: aws." or "vpc: " — detect by checking if the line has a colon
        // but no equals sign, and we're inside arguments/attributes blocks.
        // This is checked later after determining block context.

        // Check if we're inside a resource block or module call and find the type
        let mut brace_depth: i32 = 0;
        let mut resource_type = String::new();
        let mut current_binding: Option<String> = None;
        let mut module_name: Option<String> = None;
        let mut provider_block_name: Option<String> = None;
        let mut in_args_or_attrs_block = false;
        let mut in_upstream_state_block = false;
        // Track nested block names at each depth level (index 0 = depth 1, etc.)
        let mut nested_block_names: Vec<String> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if i > line_idx {
                break;
            }
            let trimmed = line.trim();

            // Look for resource type declaration: "aws.ec2.vpc {" or "let x = aws.ec2.vpc {"
            if let Some(rt) = self.extract_resource_type(line)
                && brace_depth == 0
            {
                resource_type = rt;
                module_name = None;
                // Extract binding name from "let binding_name = resource_type {"
                current_binding = trimmed
                    .strip_prefix("let ")
                    .and_then(|rest| rest.find('=').map(|eq| rest[..eq].trim().to_string()))
                    .filter(|name| {
                        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                    });
            } else if brace_depth == 0 && trimmed.starts_with("provider ") && trimmed.ends_with('{')
            {
                // Detect "provider <name> {"
                let name = trimmed
                    .strip_prefix("provider ")
                    .unwrap()
                    .trim_end_matches('{')
                    .trim();
                if !name.is_empty() {
                    provider_block_name = Some(name.to_string());
                    resource_type.clear();
                    module_name = None;
                }
            } else if brace_depth == 0
                && (trimmed.starts_with("arguments")
                    || trimmed.starts_with("attributes")
                    || trimmed.starts_with("exports"))
                && trimmed.ends_with('{')
            {
                in_args_or_attrs_block = true;
                resource_type.clear();
                module_name = None;
            } else if brace_depth == 0 && is_let_upstream_state_line(trimmed) {
                in_upstream_state_block = true;
                resource_type.clear();
                module_name = None;
            } else if brace_depth == 0
                && trimmed.ends_with('{')
                && !trimmed.starts_with("let ")
                && !self.starts_with_provider_prefix(trimmed)
                && !trimmed.starts_with("provider ")
                && !trimmed.starts_with("arguments ")
                && !trimmed.starts_with("attributes ")
                && !trimmed.starts_with("exports ")
                && !trimmed.starts_with('#')
            {
                // This is a module call: "module_name {"
                let name = trimmed.trim_end_matches('{').trim();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    module_name = Some(name.to_string());
                    resource_type.clear();
                }
            }

            // At brace_depth >= 1, detect nested block in two forms:
            //   - block syntax: "identifier {"
            //   - assignment syntax: "identifier = {"
            if brace_depth >= 1 && trimmed.ends_with('{') && !resource_type.is_empty() {
                let before_brace = trimmed.trim_end_matches('{').trim();
                let name = if before_brace.contains('=') {
                    // Assignment syntax: "name = {" → extract name before "="
                    before_brace.split('=').next().unwrap_or("").trim()
                } else {
                    // Block syntax: "name {"
                    before_brace
                };
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let depth_index = (brace_depth - 1) as usize;
                    nested_block_names.truncate(depth_index);
                    nested_block_names.push(name.to_string());
                }
            }

            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        resource_type.clear();
                        current_binding = None;
                        module_name = None;
                        provider_block_name = None;
                        in_args_or_attrs_block = false;
                        in_upstream_state_block = false;
                        nested_block_names.clear();
                    } else {
                        // Truncate to current depth
                        let depth_index = (brace_depth - 1) as usize;
                        if nested_block_names.len() > depth_index {
                            nested_block_names.truncate(depth_index);
                        }
                    }
                }
            }
        }

        // Check if we're in a type position inside arguments/attributes block
        // e.g., "vpc: " or "vpc: aws." — after colon, before any equals sign
        if in_args_or_attrs_block && brace_depth > 0 && prefix.contains(':') {
            // Check we're after the colon part, not after an equals sign
            let after_colon = prefix.rsplit(':').next().unwrap_or("").trim();
            let has_equals_after_colon = after_colon.contains('=');
            if !has_equals_after_colon {
                return CompletionContext::InTypePosition;
            }
        }

        // Check if we're in a type position inside a fn definition (at top level)
        // e.g., "fn greet(name: " or "fn greet(name: string): "
        if brace_depth == 0 {
            let trimmed_prefix = prefix.trim_start();
            if trimmed_prefix.starts_with("fn ") && prefix.contains(':') {
                let after_colon = prefix.rsplit(':').next().unwrap_or("").trim();
                // Not after an equals sign or opening brace
                if !after_colon.contains('=') && !after_colon.contains('{') {
                    return CompletionContext::InTypePosition;
                }
            }
        }

        // Check if we're inside a nested struct block (brace_depth > 1)
        if !nested_block_names.is_empty() && brace_depth > 1 && !resource_type.is_empty() {
            if prefix.contains('=') {
                let after_eq = prefix.split('=').next_back().unwrap_or("").trim();
                if !after_eq.starts_with('"') || after_eq == "\"" {
                    let field_name = self.extract_attr_name(&prefix);
                    return CompletionContext::AfterEqualsInStruct {
                        resource_type: resource_type.clone(),
                        attr_path: nested_block_names,
                        field_name,
                    };
                }
            }
            return CompletionContext::InsideStructBlock {
                resource_type: resource_type.clone(),
                attr_path: nested_block_names,
            };
        }

        // Check if we're inside a provider block
        if brace_depth > 0
            && let Some(ref pname) = provider_block_name
        {
            if prefix.contains('=') {
                // After "region = " inside provider block -> show region completions
                return CompletionContext::AfterProviderRegion {
                    provider_name: pname.clone(),
                };
            }
            return CompletionContext::InsideProviderBlock {
                provider_name: pname.clone(),
            };
        }

        // Check if we're after an equals sign (value position) inside a block
        if brace_depth > 0 && prefix.contains('=') {
            let after_eq = prefix.split('=').next_back().unwrap_or("").trim();
            // Don't show completions if user is typing a string literal (except just starting)
            if !after_eq.starts_with('"') || after_eq == "\"" {
                // Extract attribute name from current line
                let attr_name = self.extract_attr_name(&prefix);
                return CompletionContext::AfterEquals {
                    resource_type: resource_type.clone(),
                    attr_name,
                    current_binding: current_binding.clone(),
                };
            }
        }

        // Inside module call block
        if brace_depth > 0 {
            if in_upstream_state_block {
                return CompletionContext::InsideUpstreamStateBlock;
            }
            if let Some(name) = module_name {
                return CompletionContext::InsideModuleCall { module_name: name };
            }
            return CompletionContext::InsideResourceBlock { resource_type };
        }

        CompletionContext::TopLevel
    }

    /// Check if a line starts with any provider prefix (e.g., "aws.", "awscc.")
    fn starts_with_provider_prefix(&self, line: &str) -> bool {
        self.provider_names
            .iter()
            .any(|name| line.starts_with(&format!("{}.", name)))
    }

    /// Extract resource type from a line like "aws.ec2.vpc {" or "let x = aws.ec2.vpc {"
    /// Returns the resource type (e.g., "aws.ec2.vpc") for schema lookups
    fn extract_resource_type(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();

        // Match against schema keys (sorted longest first for correct matching)
        for pattern in &self.resource_type_patterns {
            if trimmed.contains(pattern.as_str()) {
                return Some(pattern.clone());
            }
        }
        None
    }

    /// Extract attribute name from a line prefix like "    enable_dns_hostnames = "
    fn extract_attr_name(&self, prefix: &str) -> String {
        let before_eq = prefix.split('=').next().unwrap_or("").trim();
        before_eq.to_string()
    }

    fn extract_struct_fields<'a>(
        &self,
        attr_type: &'a AttributeType,
    ) -> Option<&'a Vec<StructField>> {
        match attr_type {
            AttributeType::Struct { fields, .. } => Some(fields),
            AttributeType::List { inner, .. } => match inner.as_ref() {
                AttributeType::Struct { fields, .. } => Some(fields),
                _ => None,
            },
            AttributeType::Union(members) => {
                members.iter().find_map(|m| self.extract_struct_fields(m))
            }
            _ => None,
        }
    }

    /// Look up attribute schema by name, falling back to block_name match.
    fn find_attr_schema<'a>(
        &self,
        schema: &'a ResourceSchema,
        attr_name: &str,
    ) -> Option<&'a carina_core::schema::AttributeSchema> {
        if let Some(attr_schema) = schema.attributes.get(attr_name) {
            return Some(attr_schema);
        }
        // Fallback: check if attr_name matches a block_name
        schema
            .attributes
            .values()
            .find(|a| a.block_name.as_ref().is_some_and(|bn| bn == attr_name))
    }

    /// Provide completions for struct fields inside a nested block
    /// Resolve struct fields by walking down a path of nested block names.
    /// For a single-element path like ["versioning_configuration"], looks up the attribute directly.
    /// For multi-element paths like ["assume_role_policy_document", "statement"],
    /// walks down the struct hierarchy.
    /// Resolve the AttributeType at the given attr_path within a resource schema.
    fn resolve_type_for_path<'a>(
        &self,
        schema: &'a ResourceSchema,
        attr_path: &[String],
    ) -> Option<&'a AttributeType> {
        if attr_path.is_empty() {
            return None;
        }

        let attr_schema = self.find_attr_schema(schema, &attr_path[0])?;
        let mut current_type = &attr_schema.attr_type;

        for name in &attr_path[1..] {
            let fields = self.extract_struct_fields(current_type)?;
            let field = fields
                .iter()
                .find(|f| f.name == *name || f.block_name.as_deref() == Some(name))?;
            current_type = &field.field_type;
        }

        Some(current_type)
    }

    fn resolve_struct_fields_for_path<'a>(
        &self,
        schema: &'a ResourceSchema,
        attr_path: &[String],
    ) -> Option<&'a Vec<StructField>> {
        let attr_type = self.resolve_type_for_path(schema, attr_path)?;
        self.extract_struct_fields(attr_type)
    }

    fn struct_field_completions(
        &self,
        resource_type: &str,
        attr_path: &[String],
    ) -> Vec<CompletionItem> {
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        if let Some(schema) = self.schemas.get(resource_type) {
            // Try struct field completions first
            if let Some(fields) = self.resolve_struct_fields_for_path(schema, attr_path) {
                return fields
                    .iter()
                    .map(|field| {
                        let required_marker = if field.required { " (required)" } else { "" };
                        CompletionItem {
                            label: field.name.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail: field
                                .description
                                .as_ref()
                                .map(|d| format!("{}{}", d, required_marker))
                                .or_else(|| {
                                    if field.required {
                                        Some("(required)".to_string())
                                    } else {
                                        None
                                    }
                                }),
                            insert_text: Some(format!("{} = ", field.name)),
                            command: Some(trigger_suggest.clone()),
                            ..Default::default()
                        }
                    })
                    .collect();
            }

            // Try map key completions: if the attribute type at attr_path is a Map
            // with a StringEnum key, provide key name completions
            if let Some(key_type) = self.resolve_map_key_type(schema, attr_path) {
                return self.map_key_completions_from_type(key_type, &trigger_suggest);
            }
        }

        vec![]
    }

    /// Resolve the Map key type for an attribute path.
    /// Returns the key AttributeType if the attribute at the path is a Map.
    fn resolve_map_key_type<'a>(
        &self,
        schema: &'a ResourceSchema,
        attr_path: &[String],
    ) -> Option<&'a AttributeType> {
        let attr_type = self.resolve_type_for_path(schema, attr_path)?;
        if let AttributeType::Map { key, .. } = attr_type {
            Some(key)
        } else {
            None
        }
    }

    /// Generate completions from a Map key type (e.g., StringEnum values).
    fn map_key_completions_from_type(
        &self,
        key_type: &AttributeType,
        trigger_suggest: &Command,
    ) -> Vec<CompletionItem> {
        match key_type {
            AttributeType::StringEnum { values, .. } => values
                .iter()
                .map(|v| CompletionItem {
                    label: v.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    insert_text: Some(format!("{} = ", v)),
                    command: Some(trigger_suggest.clone()),
                    ..Default::default()
                })
                .collect(),
            _ => vec![],
        }
    }

    /// Provide value completions for a specific struct field
    fn value_completions_for_struct_field(
        &self,
        resource_type: &str,
        attr_path: &[String],
        field_name: &str,
    ) -> Vec<CompletionItem> {
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(fields) = self.resolve_struct_fields_for_path(schema, attr_path)
            && let Some(field) = fields.iter().find(|f| f.name == field_name)
        {
            self.completions_for_type(&field.field_type, Some(resource_type))
        } else {
            vec![]
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum CompletionContext {
    TopLevel,
    InsideResourceBlock {
        resource_type: String,
    },
    InsideUpstreamStateBlock,
    InsideModuleCall {
        module_name: String,
    },
    AfterEquals {
        resource_type: String,
        attr_name: String,
        current_binding: Option<String>,
    },
    InsideStructBlock {
        resource_type: String,
        attr_path: Vec<String>,
    },
    AfterEqualsInStruct {
        resource_type: String,
        attr_path: Vec<String>,
        field_name: String,
    },
    InsideProviderBlock {
        provider_name: String,
    },
    AfterProviderRegion {
        provider_name: String,
    },
    InTypePosition,
    InsideImportPath {
        partial_path: String,
    },
    None,
}

/// Detect a `let <binding> = upstream_state {` opening line, where `<binding>`
/// is a bare identifier. Used to enter the upstream_state block context.
fn is_let_upstream_state_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("let ") else {
        return false;
    };
    let Some(eq_pos) = rest.find('=') else {
        return false;
    };
    let binding = rest[..eq_pos].trim();
    if binding.is_empty() || !binding.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return false;
    }
    let after_eq = rest[eq_pos + 1..].trim_start();
    let Some(rest) = after_eq.strip_prefix("upstream_state") else {
        return false;
    };
    // Must be followed by whitespace or `{` to ensure it's the keyword, not a
    // longer identifier like `upstream_states`.
    let next = rest.trim_start();
    next.starts_with('{') || next.is_empty()
}
