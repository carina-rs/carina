use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use crate::document::Document;
use carina_core::parser;
use carina_core::schema::{AttributeType, CompletionValue, ResourceSchema, StructField};

pub struct CompletionProvider {
    schemas: Arc<HashMap<String, ResourceSchema>>,
    provider_names: Vec<String>,
    region_completions_data: Vec<CompletionValue>,
    /// Resource type patterns sorted longest-first for matching
    resource_type_patterns: Vec<String>,
}

impl CompletionProvider {
    pub fn new(
        schemas: Arc<HashMap<String, ResourceSchema>>,
        provider_names: Vec<String>,
        region_completions_data: Vec<CompletionValue>,
    ) -> Self {
        // Build sorted resource type patterns from schema keys (longest first)
        let mut resource_type_patterns: Vec<String> = schemas.keys().cloned().collect();
        resource_type_patterns.sort_by_key(|b| std::cmp::Reverse(b.len()));

        Self {
            schemas,
            provider_names,
            region_completions_data,
            resource_type_patterns,
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
            CompletionContext::TopLevel => self.top_level_completions(position, &text),
            CompletionContext::InsideResourceBlock { resource_type } => {
                self.attribute_completions_for_type(&resource_type)
            }
            CompletionContext::InsideModuleCall { module_name } => {
                self.module_parameter_completions(&module_name, &text, base_path)
            }
            CompletionContext::AfterEquals {
                resource_type,
                attr_name,
            } => self.value_completions_for_attr(&resource_type, &attr_name, &text),
            CompletionContext::InsideStructBlock {
                resource_type,
                attr_name,
            } => self.struct_field_completions(&resource_type, &attr_name),
            CompletionContext::AfterEqualsInStruct {
                resource_type,
                attr_name,
                field_name,
            } => self.value_completions_for_struct_field(&resource_type, &attr_name, &field_name),
            CompletionContext::AfterProviderRegion => self.region_completions(),
            CompletionContext::AfterRefType => self.ref_type_completions(),
            CompletionContext::AfterInputDot => self.input_parameter_completions(&text),
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

        // Check if we're typing after "input."
        if prefix.contains("input.") || prefix.ends_with("input") {
            return CompletionContext::AfterInputDot;
        }

        // Check if we're typing after "<provider>.Region." or "<provider>.Region"
        for provider_name in &self.provider_names {
            let dot_pattern = format!("{}.Region.", provider_name);
            let end_pattern = format!("{}.Region", provider_name);
            if prefix.contains(&dot_pattern) || prefix.ends_with(&end_pattern) {
                return CompletionContext::AfterProviderRegion;
            }
        }

        // Check if we're typing after "ref("
        if prefix.ends_with("ref(") || prefix.contains("ref(") && !prefix.contains(')') {
            return CompletionContext::AfterRefType;
        }

        // Check if we're inside a resource block or module call and find the type
        let mut brace_depth = 0;
        let mut resource_type = String::new();
        let mut module_name: Option<String> = None;
        let mut nested_block_name: Option<String> = None;

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
            } else if brace_depth == 0
                && trimmed.ends_with('{')
                && !trimmed.starts_with("let ")
                && !self.starts_with_provider_prefix(trimmed)
                && !trimmed.starts_with("provider ")
                && !trimmed.starts_with("input ")
                && !trimmed.starts_with("output ")
                && !trimmed.starts_with('#')
            {
                // This is a module call: "module_name {"
                let name = trimmed.trim_end_matches('{').trim();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    module_name = Some(name.to_string());
                    resource_type.clear();
                }
            }

            // At brace_depth == 1, detect nested block: "identifier {" (without "=")
            if brace_depth == 1
                && trimmed.ends_with('{')
                && !trimmed.contains('=')
                && !resource_type.is_empty()
            {
                let name = trimmed.trim_end_matches('{').trim();
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    nested_block_name = Some(name.to_string());
                }
            }

            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        resource_type.clear();
                        module_name = None;
                        nested_block_name = None;
                    } else if brace_depth == 1 {
                        nested_block_name = None;
                    }
                }
            }
        }

        // Check if we're inside a nested struct block (brace_depth > 1)
        if let Some(attr_name) = nested_block_name
            && brace_depth > 1
            && !resource_type.is_empty()
        {
            if prefix.contains('=') {
                let after_eq = prefix.split('=').next_back().unwrap_or("").trim();
                if !after_eq.starts_with('"') || after_eq == "\"" {
                    let field_name = self.extract_attr_name(&prefix);
                    return CompletionContext::AfterEqualsInStruct {
                        resource_type: resource_type.clone(),
                        attr_name,
                        field_name,
                    };
                }
            }
            return CompletionContext::InsideStructBlock {
                resource_type: resource_type.clone(),
                attr_name,
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
                };
            }
        }

        // Inside module call block
        if brace_depth > 0 {
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

    fn top_level_completions(&self, position: Position, text: &str) -> Vec<CompletionItem> {
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

    fn attribute_completions_for_type(&self, resource_type: &str) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Command to trigger suggestions after inserting the completion
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        // Get schema for specific resource type, or fall back to all schemas
        if let Some(schema) = self.schemas.get(resource_type) {
            for attr in schema.attributes.values() {
                let detail = attr.description.clone();
                let required_marker = if attr.required { " (required)" } else { "" };

                completions.push(CompletionItem {
                    label: attr.name.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: detail.map(|d| format!("{}{}", d, required_marker)),
                    insert_text: Some(format!("{} = ", attr.name)),
                    command: Some(trigger_suggest.clone()),
                    ..Default::default()
                });
            }
        } else {
            // Fall back to all attributes from all schemas
            let mut seen = std::collections::HashSet::new();
            for schema in self.schemas.values() {
                for attr in schema.attributes.values() {
                    if seen.insert(attr.name.clone()) {
                        let detail = attr.description.clone();
                        let required_marker = if attr.required { " (required)" } else { "" };

                        completions.push(CompletionItem {
                            label: attr.name.clone(),
                            kind: Some(CompletionItemKind::PROPERTY),
                            detail: detail.map(|d| format!("{}{}", d, required_marker)),
                            insert_text: Some(format!("{} = ", attr.name)),
                            command: Some(trigger_suggest.clone()),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        completions
    }

    fn value_completions_for_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        text: &str,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // For attributes ending with _id (like vpc_id, route_table_id), suggest resource bindings
        if attr_name.ends_with("_id") {
            let bindings = self.extract_resource_bindings(text);
            for binding_name in bindings {
                // Add completion with .id suffix (e.g., main_vpc.id)
                completions.push(CompletionItem {
                    label: format!("{}.id", binding_name),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!("Reference to {}'s ID", binding_name)),
                    insert_text: Some(format!("{}.id", binding_name)),
                    ..Default::default()
                });
            }
        }

        // Add input parameter references if this file has inputs defined
        let input_params = self.extract_input_parameters(text);
        if !input_params.is_empty() {
            // Add "input" keyword with trigger for further completion
            let trigger_suggest = Command {
                title: "Trigger Suggest".to_string(),
                command: "editor.action.triggerSuggest".to_string(),
                arguments: None,
            };

            completions.push(CompletionItem {
                label: "input".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some("Reference to module input parameters".to_string()),
                insert_text: Some("input.".to_string()),
                command: Some(trigger_suggest),
                ..Default::default()
            });

            // Also add direct input.xxx completions
            for (name, type_hint) in &input_params {
                completions.push(CompletionItem {
                    label: format!("input.{}", name),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(type_hint.clone()),
                    insert_text: Some(format!("input.{}", name)),
                    ..Default::default()
                });
            }
        }

        // Look up the attribute type from schema
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            // First check if schema defines completions for this attribute
            if let Some(schema_completions) = &attr_schema.completions {
                completions.extend(schema_completions.iter().map(|c| CompletionItem {
                    label: c.value.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(c.description.clone()),
                    insert_text: Some(c.value.clone()),
                    ..Default::default()
                }));
                return completions;
            }
            // Fall back to type-based completions
            completions.extend(self.completions_for_type(&attr_schema.attr_type));
            return completions;
        }

        // Fall back to generic value completions
        completions.extend(self.generic_value_completions());
        completions
    }

    /// Extract input parameters from text without full parsing (for incomplete code)
    fn extract_input_parameters(&self, text: &str) -> Vec<(String, String)> {
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
    fn extract_resource_bindings(&self, text: &str) -> Vec<String> {
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

    fn completions_for_type(&self, attr_type: &AttributeType) -> Vec<CompletionItem> {
        match attr_type {
            AttributeType::Bool => {
                vec![
                    CompletionItem {
                        label: "true".to_string(),
                        kind: Some(CompletionItemKind::VALUE),
                        detail: Some("Boolean true".to_string()),
                        ..Default::default()
                    },
                    CompletionItem {
                        label: "false".to_string(),
                        kind: Some(CompletionItemKind::VALUE),
                        detail: Some("Boolean false".to_string()),
                        ..Default::default()
                    },
                ]
            }
            AttributeType::Enum(variants) => {
                // Check if this is an availability zone enum (ends with digit + zone letter like 1a, 2b)
                if variants.iter().any(|v| {
                    v.len() > 2
                        && v.chars().last().is_some_and(|c| c.is_ascii_lowercase())
                        && v.chars()
                            .nth(v.len() - 2)
                            .is_some_and(|c| c.is_ascii_digit())
                }) {
                    return self.availability_zone_completions(variants);
                }
                // Check if this is a region enum (has region-like names but no zone suffix)
                if variants
                    .iter()
                    .any(|v| v.contains("northeast") || v.contains("east_1"))
                {
                    return self.region_completions();
                }
                // Generic enum completions - wrap in quotes since they're string values
                variants
                    .iter()
                    .map(|v| CompletionItem {
                        label: format!("\"{}\"", v),
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        insert_text: Some(format!("\"{}\"", v)),
                        ..Default::default()
                    })
                    .collect()
            }
            AttributeType::Int => {
                vec![] // No specific completions for integers
            }
            AttributeType::Float => {
                vec![] // No specific completions for floats
            }
            AttributeType::Custom { name, .. } if name == "Cidr" || name == "Ipv4Cidr" => {
                self.cidr_completions()
            }
            AttributeType::Custom { name, .. } if name == "Ipv6Cidr" => {
                self.ipv6_cidr_completions()
            }
            AttributeType::Custom { name, .. } if name == "Arn" => self.arn_completions(),
            AttributeType::Union(_) | AttributeType::String | AttributeType::Custom { .. } => {
                vec![CompletionItem {
                    label: "env".to_string(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    insert_text: Some("env(\"${1:VAR_NAME}\")".to_string()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    detail: Some("Read environment variable".to_string()),
                    ..Default::default()
                }]
            }
            _ => self.generic_value_completions(),
        }
    }

    /// Extract struct fields from an attribute type, unwrapping both Struct and List(Struct)
    fn extract_struct_fields<'a>(
        &self,
        attr_type: &'a AttributeType,
    ) -> Option<&'a Vec<StructField>> {
        match attr_type {
            AttributeType::Struct { fields, .. } => Some(fields),
            AttributeType::List(inner) => match inner.as_ref() {
                AttributeType::Struct { fields, .. } => Some(fields),
                _ => None,
            },
            _ => None,
        }
    }

    /// Provide completions for struct fields inside a nested block
    fn struct_field_completions(
        &self,
        resource_type: &str,
        attr_name: &str,
    ) -> Vec<CompletionItem> {
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
            && let Some(fields) = self.extract_struct_fields(&attr_schema.attr_type)
        {
            fields
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
                .collect()
        } else {
            vec![]
        }
    }

    /// Provide value completions for a specific struct field
    fn value_completions_for_struct_field(
        &self,
        resource_type: &str,
        attr_name: &str,
        field_name: &str,
    ) -> Vec<CompletionItem> {
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
            && let Some(fields) = self.extract_struct_fields(&attr_schema.attr_type)
            && let Some(field) = fields.iter().find(|f| f.name == field_name)
        {
            self.completions_for_type(&field.field_type)
        } else {
            vec![]
        }
    }

    fn generic_value_completions(&self) -> Vec<CompletionItem> {
        let mut completions = vec![
            CompletionItem {
                label: "true".to_string(),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some("Boolean true".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "false".to_string(),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some("Boolean false".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.Region.ap_northeast_1".to_string(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some("Tokyo region".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "env".to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                insert_text: Some("env(\"${1:VAR_NAME}\")".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Read environment variable".to_string()),
                ..Default::default()
            },
        ];

        completions.extend(self.region_completions());
        completions
    }

    fn region_completions(&self) -> Vec<CompletionItem> {
        self.region_completions_data
            .iter()
            .map(|c| CompletionItem {
                label: c.value.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(c.description.clone()),
                insert_text: Some(c.value.clone()),
                ..Default::default()
            })
            .collect()
    }

    fn cidr_completions(&self) -> Vec<CompletionItem> {
        let cidrs = vec![
            ("10.0.0.0/16", "VPC CIDR (65,536 IPs)"),
            ("10.0.0.0/24", "Subnet CIDR (256 IPs)"),
            ("10.0.1.0/24", "Subnet CIDR (256 IPs)"),
            ("10.0.2.0/24", "Subnet CIDR (256 IPs)"),
            ("172.16.0.0/16", "VPC CIDR (65,536 IPs)"),
            ("192.168.0.0/16", "VPC CIDR (65,536 IPs)"),
            ("0.0.0.0/0", "All IPv4 addresses"),
        ];

        cidrs
            .into_iter()
            .map(|(cidr, description)| CompletionItem {
                label: format!("\"{}\"", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("\"{}\"", cidr)),
                ..Default::default()
            })
            .collect()
    }

    fn ipv6_cidr_completions(&self) -> Vec<CompletionItem> {
        let cidrs = vec![
            ("::/0", "All IPv6 addresses"),
            ("2001:db8::/32", "Documentation range"),
            ("fe80::/10", "Link-local addresses"),
            ("fc00::/7", "Unique local addresses"),
            ("::1/128", "Loopback address"),
        ];

        cidrs
            .into_iter()
            .map(|(cidr, description)| CompletionItem {
                label: format!("\"{}\"", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("\"{}\"", cidr)),
                ..Default::default()
            })
            .collect()
    }

    fn arn_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "\"arn:aws:...\"".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            insert_text: Some(
                "\"arn:aws:${1:service}:${2:region}:${3:account}:${4:resource}\"".to_string(),
            ),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("ARN format: arn:partition:service:region:account:resource".to_string()),
            ..Default::default()
        }]
    }

    fn ref_type_completions(&self) -> Vec<CompletionItem> {
        self.schemas
            .keys()
            .map(|resource_type| {
                let description = self
                    .schemas
                    .get(resource_type)
                    .and_then(|s| s.description.as_deref())
                    .unwrap_or("Resource reference");

                CompletionItem {
                    label: resource_type.clone(),
                    kind: Some(CompletionItemKind::TYPE_PARAMETER),
                    detail: Some(format!("{} reference", description)),
                    insert_text: Some(format!("{})", resource_type)),
                    ..Default::default()
                }
            })
            .collect()
    }

    fn module_parameter_completions(
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
            if let Some(parsed) = self.load_module(&module_path) {
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

    /// Load module from path, handling both file and directory-based modules
    fn load_module(&self, path: &Path) -> Option<parser::ParsedFile> {
        if path.is_dir() {
            // Directory-based module: load main.crn
            let main_path = path.join("main.crn");
            if main_path.exists() {
                let content = std::fs::read_to_string(&main_path).ok()?;
                parser::parse(&content).ok()
            } else {
                // Fallback: merge all .crn files in the directory
                self.load_directory_module(path)
            }
        } else {
            // Single file module
            let content = std::fs::read_to_string(path).ok()?;
            parser::parse(&content).ok()
        }
    }

    /// Load all .crn files from a directory and merge them
    fn load_directory_module(&self, dir_path: &Path) -> Option<parser::ParsedFile> {
        let entries = std::fs::read_dir(dir_path).ok()?;
        let mut merged = parser::ParsedFile {
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
                && let Ok(parsed) = parser::parse(&content)
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

        Some(merged)
    }

    /// Provide completions for input parameters in the current file (after "input.")
    fn input_parameter_completions(&self, text: &str) -> Vec<CompletionItem> {
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

    fn format_type_expr(&self, type_expr: &parser::TypeExpr) -> String {
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
    fn find_module_import_path(&self, module_name: &str, text: &str) -> Option<String> {
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

    fn availability_zone_completions(&self, variants: &[String]) -> Vec<CompletionItem> {
        // Group AZs by region for better display
        let region_names: std::collections::HashMap<&str, &str> = [
            ("ap_northeast_1", "Tokyo"),
            ("ap_northeast_2", "Seoul"),
            ("ap_northeast_3", "Osaka"),
            ("ap_southeast_1", "Singapore"),
            ("ap_southeast_2", "Sydney"),
            ("ap_south_1", "Mumbai"),
            ("us_east_1", "N. Virginia"),
            ("us_east_2", "Ohio"),
            ("us_west_1", "N. California"),
            ("us_west_2", "Oregon"),
            ("eu_west_1", "Ireland"),
            ("eu_west_2", "London"),
            ("eu_central_1", "Frankfurt"),
        ]
        .into_iter()
        .collect();

        variants
            .iter()
            .map(|az| {
                // Extract region from AZ (e.g., "ap_northeast_1" from "ap_northeast_1a")
                let region = &az[..az.len() - 1];
                let zone_letter = az.chars().last().unwrap_or('?');
                let region_name = region_names.get(region).unwrap_or(&"");
                let detail = if region_name.is_empty() {
                    format!("Zone {}", zone_letter)
                } else {
                    format!("{} Zone {}", region_name, zone_letter)
                };

                CompletionItem {
                    label: format!("awscc.AvailabilityZone.{}", az),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(detail),
                    insert_text: Some(format!("awscc.AvailabilityZone.{}", az)),
                    ..Default::default()
                }
            })
            .collect()
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum CompletionContext {
    TopLevel,
    InsideResourceBlock {
        resource_type: String,
    },
    InsideModuleCall {
        module_name: String,
    },
    AfterEquals {
        resource_type: String,
        attr_name: String,
    },
    InsideStructBlock {
        resource_type: String,
        attr_name: String,
    },
    AfterEqualsInStruct {
        resource_type: String,
        attr_name: String,
        field_name: String,
    },
    AfterProviderRegion,
    AfterRefType,
    AfterInputDot,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use carina_core::provider::ProviderFactory;

    fn create_document(content: &str) -> Document {
        Document::new(content.to_string())
    }

    fn test_provider() -> CompletionProvider {
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(carina_provider_aws::AwsProviderFactory),
            Box::new(carina_provider_awscc::AwsccProviderFactory),
        ];
        let mut schemas = HashMap::new();
        for factory in &factories {
            for schema in factory.schemas() {
                schemas.insert(schema.resource_type.clone(), schema);
            }
        }
        let schemas = Arc::new(schemas);
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
        let region_completions: Vec<CompletionValue> = factories
            .iter()
            .flat_map(|f| f.region_completions())
            .collect();
        CompletionProvider::new(schemas, provider_names, region_completions)
    }

    #[test]
    fn top_level_completion_replaces_prefix() {
        let provider = test_provider();
        let doc = create_document("aws.s");
        // Cursor at end of "aws.s" (line 0, col 5)
        let position = Position {
            line: 0,
            character: 5,
        };

        let completions = provider.complete(&doc, position, None);

        // Find the aws.s3.bucket completion
        let s3_completion = completions
            .iter()
            .find(|c| c.label == "aws.s3.bucket")
            .expect("Should have aws.s3.bucket completion");

        // Verify it uses text_edit, not insert_text
        assert!(
            s3_completion.text_edit.is_some(),
            "Should use text_edit for resource type completion"
        );

        // Verify the text_edit range starts at column 0 (beginning of "aws.s")
        if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &s3_completion.text_edit
        {
            assert_eq!(
                edit.range.start.character, 0,
                "Should replace from start of prefix"
            );
            assert_eq!(edit.range.end.character, 5, "Should replace up to cursor");
            assert!(
                edit.new_text.starts_with("aws.s3.bucket"),
                "new_text should start with aws.s3.bucket"
            );
        } else {
            panic!("Expected CompletionTextEdit::Edit");
        }
    }

    #[test]
    fn top_level_completion_with_leading_whitespace() {
        let provider = test_provider();
        let doc = create_document("    aws.e");
        // Cursor at end of "    aws.e" (line 0, col 9)
        let position = Position {
            line: 0,
            character: 9,
        };

        let completions = provider.complete(&doc, position, None);

        // Find the aws.ec2.vpc completion
        let vpc_completion = completions
            .iter()
            .find(|c| c.label == "aws.ec2.vpc")
            .expect("Should have aws.ec2.vpc completion");

        if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) =
            &vpc_completion.text_edit
        {
            // Should replace from column 4 (after whitespace) to cursor at 9
            assert_eq!(
                edit.range.start.character, 4,
                "Should replace from after whitespace"
            );
            assert_eq!(edit.range.end.character, 9, "Should replace up to cursor");
        } else {
            panic!("Expected CompletionTextEdit::Edit");
        }
    }

    #[test]
    fn top_level_completion_at_line_start() {
        let provider = test_provider();
        let doc = create_document("a");
        // Cursor at end of "a" (line 0, col 1)
        let position = Position {
            line: 0,
            character: 1,
        };

        let completions = provider.complete(&doc, position, None);

        // Find the aws.ec2.vpc completion (should still be offered)
        let vpc_completion = completions.iter().find(|c| c.label == "aws.ec2.vpc");
        assert!(
            vpc_completion.is_some(),
            "Should offer aws.ec2.vpc completion"
        );

        if let Some(c) = vpc_completion
            && let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &c.text_edit
        {
            assert_eq!(
                edit.range.start.character, 0,
                "Should replace from line start"
            );
            assert_eq!(edit.range.end.character, 1, "Should replace up to cursor");
        }
    }

    #[test]
    fn module_parameter_completion_with_directory_module() {
        use std::fs;
        use tempfile::tempdir;

        let provider = test_provider();

        // Create a temporary directory structure
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let base_path = temp_dir.path();

        // Create module directory
        let module_dir = base_path.join("modules").join("web_tier");
        fs::create_dir_all(&module_dir).expect("Failed to create module dir");

        // Create main.crn with input parameters
        let module_content = r#"
input {
    vpc: ref(aws.ec2.vpc)
    cidr_blocks: list(cidr)
    enable_https: bool = true
}

let web_sg = aws.ec2.security_group {
    name = "web-sg"
}
"#;
        fs::write(module_dir.join("main.crn"), module_content)
            .expect("Failed to write module file");

        // Create main file that imports the module
        let main_content = r#"import "./modules/web_tier" as web_tier

web_tier {

}"#;
        let doc = create_document(main_content);

        // Cursor inside the module call block (line 3, after whitespace)
        let position = Position {
            line: 3,
            character: 4,
        };

        let completions = provider.complete(&doc, position, Some(base_path));

        // Should have module parameter completions
        assert!(!completions.is_empty(), "Should have completions");

        // Check for specific parameters
        let vpc_completion = completions.iter().find(|c| c.label == "vpc");
        assert!(
            vpc_completion.is_some(),
            "Should have vpc parameter completion"
        );
        if let Some(c) = vpc_completion {
            assert!(
                c.detail.as_ref().is_some_and(|d| d.contains("required")),
                "vpc should be marked as required"
            );
        }

        let cidr_completion = completions.iter().find(|c| c.label == "cidr_blocks");
        assert!(
            cidr_completion.is_some(),
            "Should have cidr_blocks parameter completion"
        );

        let https_completion = completions.iter().find(|c| c.label == "enable_https");
        assert!(
            https_completion.is_some(),
            "Should have enable_https parameter completion"
        );
        if let Some(c) = https_completion {
            assert!(
                !c.detail.as_ref().is_some_and(|d| d.contains("required")),
                "enable_https should NOT be marked as required (has default)"
            );
        }
    }

    #[test]
    fn module_parameter_completion_with_single_file_module() {
        use std::fs;
        use tempfile::tempdir;

        let provider = test_provider();

        // Create a temporary directory structure
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let base_path = temp_dir.path();

        // Create module directory
        let module_dir = base_path.join("modules");
        fs::create_dir_all(&module_dir).expect("Failed to create module dir");

        // Create single file module
        let module_content = r#"
input {
    name: string
    count: int = 1
}
"#;
        fs::write(module_dir.join("simple.crn"), module_content)
            .expect("Failed to write module file");

        // Create main file that imports the module
        let main_content = r#"import "./modules/simple.crn" as simple

simple {
    n
}"#;
        let doc = create_document(main_content);

        // Cursor inside the module call block (line 3, after "n")
        let position = Position {
            line: 3,
            character: 5,
        };

        let completions = provider.complete(&doc, position, Some(base_path));

        // Should have module parameter completions
        let name_completion = completions.iter().find(|c| c.label == "name");
        assert!(
            name_completion.is_some(),
            "Should have name parameter completion"
        );

        let count_completion = completions.iter().find(|c| c.label == "count");
        assert!(
            count_completion.is_some(),
            "Should have count parameter completion"
        );
    }

    #[test]
    fn instance_tenancy_completion_for_aws_vpc() {
        let provider = test_provider();
        let doc = create_document(
            r#"aws.ec2.vpc {
    name = "my-vpc"
    instance_tenancy =
}"#,
        );
        // Cursor after "instance_tenancy = " (line 2, col 23)
        let position = Position {
            line: 2,
            character: 23,
        };

        let completions = provider.complete(&doc, position, None);

        // Should have namespaced instance_tenancy completions
        let default_completion = completions
            .iter()
            .find(|c| c.label == "aws.ec2.vpc.InstanceTenancy.default");
        assert!(
            default_completion.is_some(),
            "Should have 'aws.ec2.vpc.InstanceTenancy.default' completion"
        );

        let dedicated_completion = completions
            .iter()
            .find(|c| c.label == "aws.ec2.vpc.InstanceTenancy.dedicated");
        assert!(
            dedicated_completion.is_some(),
            "Should have 'aws.ec2.vpc.InstanceTenancy.dedicated' completion"
        );
    }

    // Note: instance_tenancy_completion_for_awscc_vpc test was removed
    // because generated schemas use AttributeType::String for instance_tenancy
    // instead of the custom InstanceTenancy type that provides completions.

    #[test]
    fn versioning_status_completion_for_s3_bucket() {
        let provider = test_provider();
        let doc = create_document(
            r#"aws.s3.bucket {
    name = "my-bucket"

}"#,
        );
        // Cursor inside s3_bucket block (line 2)
        let position = Position {
            line: 2,
            character: 4,
        };

        let completions = provider.complete(&doc, position, None);

        // Should have versioning_status as attribute completion
        let versioning_completion = completions.iter().find(|c| c.label == "versioning_status");
        assert!(
            versioning_completion.is_some(),
            "Should have 'versioning_status' attribute completion"
        );
    }

    #[test]
    fn struct_field_completion_inside_nested_block() {
        let provider = test_provider();
        let doc = create_document(
            r#"awscc.ec2.security_group {
    group_description = "test"
    security_group_ingress {

    }
}"#,
        );
        // Cursor inside the nested block (line 3)
        let position = Position {
            line: 3,
            character: 8,
        };

        let completions = provider.complete(&doc, position, None);

        // Should have struct field completions
        let ip_protocol = completions.iter().find(|c| c.label == "ip_protocol");
        assert!(
            ip_protocol.is_some(),
            "Should have ip_protocol field completion"
        );

        let from_port = completions.iter().find(|c| c.label == "from_port");
        assert!(
            from_port.is_some(),
            "Should have from_port field completion"
        );

        let to_port = completions.iter().find(|c| c.label == "to_port");
        assert!(to_port.is_some(), "Should have to_port field completion");

        // ip_protocol should be marked as required
        if let Some(c) = ip_protocol {
            assert!(
                c.detail.as_ref().is_some_and(|d| d.contains("required")),
                "ip_protocol should be marked as required"
            );
        }

        // Should NOT have top-level resource attributes like group_description
        let group_desc = completions.iter().find(|c| c.label == "group_description");
        assert!(
            group_desc.is_none(),
            "Should not have resource-level attributes inside struct block"
        );
    }

    #[test]
    fn struct_field_value_completion_for_bool() {
        let provider = test_provider();
        // flow_log's destination_options has Bool fields
        let doc = create_document(
            r#"let flow_log = awscc.ec2.flow_log {
    destination_options {
        hive_compatible_partitions =
    }
}"#,
        );
        // Cursor after "hive_compatible_partitions = " (line 2)
        let position = Position {
            line: 2,
            character: 37,
        };

        let completions = provider.complete(&doc, position, None);

        let true_completion = completions.iter().find(|c| c.label == "true");
        assert!(
            true_completion.is_some(),
            "Should have 'true' completion for Bool struct field"
        );

        let false_completion = completions.iter().find(|c| c.label == "false");
        assert!(
            false_completion.is_some(),
            "Should have 'false' completion for Bool struct field"
        );
    }

    #[test]
    fn struct_field_completion_inside_second_repeated_block() {
        let provider = test_provider();
        let doc = create_document(
            r#"awscc.ec2.security_group {
    group_description = "test"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
        cidr_ip = "0.0.0.0/0"
    }
    security_group_ingress {

    }
}"#,
        );
        // Cursor inside the second nested block (line 9)
        let position = Position {
            line: 9,
            character: 8,
        };

        let completions = provider.complete(&doc, position, None);

        // Should have struct field completions in the second block too
        let ip_protocol = completions.iter().find(|c| c.label == "ip_protocol");
        assert!(
            ip_protocol.is_some(),
            "Should have ip_protocol field completion in second repeated block"
        );

        let from_port = completions.iter().find(|c| c.label == "from_port");
        assert!(
            from_port.is_some(),
            "Should have from_port field completion in second repeated block"
        );
    }

    #[test]
    fn context_detection_returns_struct_context() {
        let provider = test_provider();
        let text = r#"awscc.ec2.security_group {
    group_description = "test"
    security_group_ingress {

    }
}"#;
        // Cursor inside nested block
        let context = provider.get_completion_context(
            text,
            Position {
                line: 3,
                character: 8,
            },
        );
        assert!(
            matches!(
                context,
                CompletionContext::InsideStructBlock {
                    ref resource_type,
                    ref attr_name,
                } if resource_type == "awscc.ec2.security_group" && attr_name == "security_group_ingress"
            ),
            "Should detect InsideStructBlock context, got: {:?}",
            context
        );
    }
}
