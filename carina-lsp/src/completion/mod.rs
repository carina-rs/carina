//! Completion provider for the Carina LSP.

pub(crate) mod dsl_source;
mod top_level;
mod values;

#[cfg(test)]
mod tests;

pub(crate) use dsl_source::DslSource;

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

        // `<binding>.<key>.<partial>` (depth-2) is checked first because
        // its prefix shape also matches the depth-1 walker's tail (which
        // would treat `<key>` as the binding and decline). #2041.
        if let Some(m) = detect_upstream_state_depth2_dot(&text, position, base_path) {
            return self.upstream_state_depth2_dot_completions(
                &m.binding, &m.key, &m.partial, &m.source, position, base_path,
            );
        }

        // `<binding>.<partial>` where `<binding>` is an `upstream_state` is
        // matched ahead of the block-context walk: the same prefix would
        // otherwise be interpreted as `AfterEquals` (dot-after-binding inside
        // a resource block) or fall through to `TopLevel`, neither of which
        // knows how to surface upstream exports.
        if let Some((binding, partial, source)) =
            detect_upstream_state_dot(&text, position, base_path)
        {
            return self
                .upstream_state_dot_completions(&binding, &partial, &source, position, base_path);
        }

        let context = self.get_completion_context(&text, position);

        match context {
            CompletionContext::TopLevel => self.top_level_completions(position, &text, base_path),
            CompletionContext::InsideResourceBlock { resource_type } => {
                self.attribute_completions_for_type(&resource_type)
            }
            CompletionContext::InsideUpstreamStateBlock => self.upstream_state_block_completions(),
            CompletionContext::InsideUseBlock => self.use_block_completions(),
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
                base_path,
            ),
            CompletionContext::AfterEqualsInExports {
                type_expr_text,
                in_nested,
            } => self.exports_value_completions(&type_expr_text, in_nested, &text, base_path),
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
            CompletionContext::InsideUpstreamStateSource { partial_path } => {
                self.upstream_state_source_completions(&partial_path, position, base_path)
            }
            CompletionContext::ForIterable { partial } => {
                self.for_iterable_completions(&text, position, &partial, base_path)
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

        // `for <pat> in <HERE>`: offer in-scope bindings. Detected before the
        // block-context walk because a `for` header sits at brace_depth 0 and
        // would otherwise fall through to `TopLevel`.
        if let Some(partial) = extract_for_iterable_partial(&prefix) {
            return CompletionContext::ForIterable { partial };
        }

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
        let mut in_exports_block = false;
        let mut in_upstream_state_block = false;
        let mut in_use_block = false;
        // Type annotation of the most recent `<name>: <type> = ...` entry
        // inside an `exports { ... }` block. Recorded at brace_depth 1 and
        // consulted when the cursor lands inside that entry's value
        // position (depth 1 for top-level, depth 2+ for nested map/list).
        // Cleared when the entry closes or a new one starts.
        let mut exports_entry_type: Option<String> = None;
        // Track nested block names at each depth level (index 0 = depth 1, etc.)
        let mut nested_block_names: Vec<String> = Vec::new();
        // Depth-of-for-bodies currently open. A `for ... { <body> }` stacks a
        // resource_block-free brace level that should be transparent when we
        // look for enclosing resource types, so a resource declaration at
        // `brace_depth == for_body_depth` is still a top-level declaration
        // relative to any enclosing resource.
        let mut for_body_depth: i32 = 0;

        for (i, line) in lines.iter().enumerate() {
            if i > line_idx {
                break;
            }
            let trimmed = line.trim();

            // A `for ... in ... {` header opens a pure control-flow brace level,
            // not a resource block. Track it so resource-type detection below
            // can see through it.
            let is_for_header = trimmed.starts_with("for ") && trimmed.ends_with('{');

            // Look for resource type declaration: "aws.ec2.Vpc {" or "let x = aws.ec2.Vpc {"
            // Accept at depth == 0 (top level) or at depth == for_body_depth
            // (directly inside a `for` body, which is semantically top-level
            // for the purpose of resolving the enclosing resource type).
            if let Some(rt) = self.extract_resource_type(line)
                && brace_depth == for_body_depth
            {
                resource_type = rt;
                module_name = None;
                // Extract binding name from "let binding_name = resource_type {"
                current_binding =
                    crate::let_parse::parse_let_header(trimmed).map(|(name, _)| name.to_string());
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
                if trimmed.starts_with("exports") {
                    in_exports_block = true;
                }
                resource_type.clear();
                module_name = None;
            } else if brace_depth == 0 && is_let_upstream_state_line(trimmed) {
                in_upstream_state_block = true;
                resource_type.clear();
                module_name = None;
            } else if brace_depth == 0 && is_let_use_line(trimmed) {
                in_use_block = true;
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

            // Inside an `exports` block, capture the type annotation of
            // the current entry: `<name>: <type> = ...`. Recorded at
            // depth 1 (the exports block body itself). Used by value-
            // position completion to filter candidates by the declared
            // type.
            if in_exports_block
                && brace_depth == 1
                && let Some(ty) = extract_exports_entry_type(trimmed)
            {
                exports_entry_type = Some(ty);
            }

            // Track for-body opens so the opening `{` increments both
            // brace_depth and for_body_depth. The matching `}` drops
            // for_body_depth alongside brace_depth.
            let mut for_brace_pending = is_for_header;

            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                    if for_brace_pending {
                        for_body_depth += 1;
                        for_brace_pending = false;
                    }
                } else if c == '}' {
                    if brace_depth == for_body_depth && for_body_depth > 0 {
                        for_body_depth -= 1;
                    }
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        resource_type.clear();
                        current_binding = None;
                        module_name = None;
                        provider_block_name = None;
                        in_args_or_attrs_block = false;
                        in_exports_block = false;
                        in_upstream_state_block = false;
                        in_use_block = false;
                        exports_entry_type = None;
                        nested_block_names.clear();
                    } else if brace_depth == 1 && in_exports_block {
                        // Closing a nested `{ ... }` inside an exports
                        // map/list value — the next entry starts fresh.
                        exports_entry_type = None;
                    } else if brace_depth == for_body_depth {
                        // Closed the resource/module block that was inside the
                        // current for body — forget its type so the next
                        // resource at this level is detected fresh.
                        resource_type.clear();
                        current_binding = None;
                        module_name = None;
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
        // e.g., "fn greet(name: " or "fn greet(name: String): "
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

        // Check if cursor is inside `source = '...'` within an upstream_state block
        if in_upstream_state_block
            && brace_depth > 0
            && let Some(partial) = extract_upstream_source_partial(&prefix)
        {
            return CompletionContext::InsideUpstreamStateSource {
                partial_path: partial,
            };
        }

        // Check if cursor is inside `source = '...'` within a `use` block.
        // Unlike `upstream_state` (where `source = '...'` almost always sits
        // on its own line), a `use` block is frequently written in-line as
        // `let x = use { source = '<cursor>` — so we have to search the full
        // prefix, not just its trimmed start, for the attribute.
        if in_use_block
            && brace_depth > 0
            && let Some(partial) = extract_source_partial_anywhere(&prefix)
        {
            return CompletionContext::InsideImportPath {
                partial_path: partial,
            };
        }

        // Attribute-name position inside a `use { ... }` block. The sole
        // valid attribute is `source`. Handled before the generic
        // `contains('=')` fallback so the in-line shape
        // `let x = use { <cursor>` — whose prefix still contains the
        // binding's own `=` — doesn't get routed to value-position
        // builtins. The `source = '...'` value position is already caught
        // by the `InsideImportPath` branch above.
        if in_use_block && brace_depth > 0 {
            return CompletionContext::InsideUseBlock;
        }

        // Check if we're after an equals sign (value position) inside a block
        if brace_depth > 0 && prefix.contains('=') {
            let after_eq = prefix.split('=').next_back().unwrap_or("").trim();
            // Don't show completions if user is typing a string literal (except just starting)
            if !after_eq.starts_with('"') || after_eq == "\"" {
                // Inside an `exports` block, filter value-position
                // candidates by the entry's declared type. If the
                // annotation can't be resolved the original empty
                // fallback from #1993 still applies — it is preferable
                // to silence than to dump every built-in.
                if in_exports_block {
                    if let Some(entry_type) = exports_entry_type.as_deref() {
                        // `brace_depth == 1` means the value sits
                        // directly after `= ` at the top of the exports
                        // entry; `>= 2` means it's inside the entry's
                        // `{ ... }` map or list body, so the relevant
                        // type is unwrapped by one level.
                        let in_nested = brace_depth >= 2;
                        return CompletionContext::AfterEqualsInExports {
                            type_expr_text: entry_type.to_string(),
                            in_nested,
                        };
                    }
                    return CompletionContext::None;
                }
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

    /// Extract resource type from a line like "aws.ec2.Vpc {" or "let x = aws.ec2.Vpc {"
    /// Returns the resource type (e.g., "aws.ec2.Vpc") for schema lookups
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
    InsideUseBlock,
    InsideModuleCall {
        module_name: String,
    },
    AfterEquals {
        resource_type: String,
        attr_name: String,
        current_binding: Option<String>,
    },
    /// Cursor is at a value position inside an `exports { ... }` block
    /// for an entry that carries a type annotation. `type_expr_text` is
    /// the raw annotation text (`string`, `map(aws_account_id)`, etc.);
    /// `in_nested` is true when the cursor is inside that entry's
    /// `{ ... }` map or list body, meaning the effective type is the
    /// annotation unwrapped by one level.
    AfterEqualsInExports {
        type_expr_text: String,
        in_nested: bool,
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
    InsideUpstreamStateSource {
        partial_path: String,
    },
    /// Cursor is at the iterable position of a `for ... in <HERE>` header
    /// (after the `in` keyword, before any `.` field access). The partial is
    /// whatever the user has typed so far, used by the handler to compute
    /// the replace range.
    ForIterable {
        partial: String,
    },
    None,
}

/// Detect a `let <binding> = upstream_state {` opening line, where `<binding>`
/// is a bare identifier. Used to enter the upstream_state block context.
/// Extract the type annotation from a line that opens an `exports`
/// entry, e.g. `accounts: map(aws_account_id) = { ...` returns
/// `Some("map(AwsAccountId)")`. Returns `None` when the line doesn't
/// match the `<name>: <type> =` shape — either because there's no
/// colon, no equals, or the type portion is empty.
fn extract_exports_entry_type(line: &str) -> Option<String> {
    let (before_eq, _) = line.split_once('=')?;
    let (_name, after_colon) = before_eq.split_once(':')?;
    let type_text = after_colon.trim();
    if type_text.is_empty() {
        return None;
    }
    Some(type_text.to_string())
}

fn is_let_upstream_state_line(trimmed: &str) -> bool {
    let Some((_, rhs)) = crate::let_parse::parse_let_header(trimmed) else {
        return false;
    };
    let Some(rest) = rhs.strip_prefix("upstream_state") else {
        return false;
    };
    // Must be followed by whitespace or `{` to ensure it's the keyword, not a
    // longer identifier like `upstream_states`.
    let next = rest.trim_start();
    next.starts_with('{') || next.is_empty()
}

fn is_let_use_line(trimmed: &str) -> bool {
    let Some((_, rhs)) = crate::let_parse::parse_let_header(trimmed) else {
        return false;
    };
    let Some(rest) = rhs.strip_prefix("use") else {
        return false;
    };
    // Must be followed by whitespace or `{` to ensure it's the keyword, not a
    // longer identifier starting with `use` (e.g. `user_data`). For such
    // identifiers the continuation after stripping `use` starts with an
    // identifier character (`r` for `user_data`), so `trim_start` preserves
    // it and neither `starts_with('{')` nor `is_empty()` matches.
    let next = rest.trim_start();
    next.starts_with('{') || next.is_empty()
}

/// If the cursor sits at the iterable position of a `for <pat> in <partial>`
/// header — i.e. after the `in` keyword and inside (possibly empty) identifier
/// characters — return the partial identifier typed so far. A `.` in the
/// partial means the user has moved past the root binding into field access,
/// which is a separate completion context (see #1996).
fn extract_for_iterable_partial(prefix: &str) -> Option<String> {
    let rest = prefix.trim_start().strip_prefix("for ")?;
    // A real `in` token has whitespace on the left (so `information` can't
    // masquerade) and either whitespace or end-of-prefix on the right (so
    // the moment the cursor lands just past `in` — with no trailing space
    // yet — still fires).
    let after_in = rest.match_indices(" in").find_map(|(idx, _)| {
        let after = &rest[idx + 3..];
        let right_ok = after.is_empty() || after.starts_with(|c: char| c.is_whitespace());
        right_ok.then_some(after.trim_start())
    })?;
    // Anything non-identifier (`.`, `[`, whitespace, `{`, etc.) means the
    // user has moved past the root-binding position — into field access,
    // indexing, or the loop body — all of which are other contexts.
    if !after_in
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(after_in.to_string())
}

/// If the prefix up to `position` ends with `<binding>.<partial>` where
/// `<binding>` matches a `let <binding> = upstream_state { ... }` declared
/// anywhere in `base_path`, return `(binding, partial, source_path)`. The
/// source is returned alongside so the handler doesn't have to re-scan
/// sibling files. Bare `<binding>` (no dot yet) is the ForIterable / value
/// completion surface, not this one.
fn detect_upstream_state_dot(
    text: &str,
    position: Position,
    base_path: Option<&std::path::Path>,
) -> Option<(String, String, String)> {
    let line_idx = position.line as usize;
    let current_line = text.lines().nth(line_idx)?;
    let col = position.character as usize;
    let prefix: String = current_line.chars().take(col).collect();
    let segs = parse_trailing_dotted_segments(&prefix, 2)?;
    let [binding, partial] = segs.as_slice() else {
        unreachable!("parse_trailing_dotted_segments returns exactly the requested count");
    };
    let source = resolve_upstream_state_source(text, base_path, binding)?;
    Some((binding.to_string(), partial.to_string(), source))
}

/// Match on `<binding>.<key>.<partial>` (depth-2), where `<binding>` is
/// an `upstream_state` and `<key>` is one of its declared exports. The
/// caller uses the export's `TypeExpr` to descend (struct fields,
/// future map-key recursion, etc.). See #2041.
///
/// Returns `None` when the prefix isn't depth-2 shape or when
/// `<binding>` isn't a known upstream-state — in which case the
/// dispatcher falls through to the depth-1 detector or further down.
fn detect_upstream_state_depth2_dot(
    text: &str,
    position: Position,
    base_path: Option<&std::path::Path>,
) -> Option<UpstreamDepth2Match> {
    let line_idx = position.line as usize;
    let current_line = text.lines().nth(line_idx)?;
    let col = position.character as usize;
    let prefix: String = current_line.chars().take(col).collect();
    let segs = parse_trailing_dotted_segments(&prefix, 3)?;
    let [binding, key, partial] = segs.as_slice() else {
        unreachable!("parse_trailing_dotted_segments returns exactly the requested count");
    };
    let source = resolve_upstream_state_source(text, base_path, binding)?;
    Some(UpstreamDepth2Match {
        binding: binding.to_string(),
        key: key.to_string(),
        partial: partial.to_string(),
        source,
    })
}

/// Result of [`detect_upstream_state_depth2_dot`]. Named fields keep
/// the four positional `String`s from being reordered at call sites.
struct UpstreamDepth2Match {
    binding: String,
    key: String,
    partial: String,
    source: String,
}

/// Walk back from the end of `prefix` collecting the trailing run of
/// `<id>(.<id>)*<.partial>` (where `partial` may be empty after a
/// trailing `.`). Returns exactly `n` segments — earliest first — or
/// `None` if the run is too short, contains non-identifier characters,
/// or any non-trailing segment is empty / starts with a digit. The
/// trailing segment (`partial`) is allowed to be empty so completion
/// fires the moment the user types the dot.
fn parse_trailing_dotted_segments(prefix: &str, n: usize) -> Option<Vec<&str>> {
    debug_assert!(n >= 2, "trailing-dot parse needs at least one dot");
    // Walk back across `n - 1` dots, validating identifier chars between
    // them. The trailing segment is allowed to be empty.
    let last_dot = prefix.rfind('.')?;
    let trailing = &prefix[last_dot + 1..];
    if !trailing
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let mut segments_rev: Vec<&str> = vec![trailing];
    let mut cursor = &prefix[..last_dot];
    while segments_rev.len() < n - 1 {
        let dot = cursor.rfind('.')?;
        let seg = &cursor[dot + 1..];
        if !is_identifier(seg) {
            return None;
        }
        segments_rev.push(seg);
        cursor = &cursor[..dot];
    }
    // The first segment is whatever non-identifier-bounded run sits at
    // the tail of `cursor` — the binding name. Its leading char must
    // start an identifier; any non-identifier before it is fine.
    let first_start = cursor
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let first = &cursor[first_start..];
    if !is_identifier(first) {
        return None;
    }
    segments_rev.push(first);
    segments_rev.reverse();
    Some(segments_rev)
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Look up the `source = '...'` path for `binding` declared in any
/// sibling `.crn` file under `base_path`. Returns `None` when the
/// binding isn't declared as an `upstream_state` anywhere visible.
fn resolve_upstream_state_source(
    text: &str,
    base_path: Option<&std::path::Path>,
    binding: &str,
) -> Option<String> {
    let mut src_buf = String::new();
    let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
    collect_upstream_state_bindings(src).get(binding).cloned()
}

/// Return every `let <name> = upstream_state { source = '...' }` declared
/// in `src` as a map from binding name to source path (relative, as written).
///
/// Intentionally does a text scan rather than going through `parse_directory`:
/// completion runs on partial, often syntactically invalid buffers. We only
/// need binding → source to feed `resolve_upstream_exports`, which then
/// parses the *upstream* directory (separate from the downstream buffer).
fn collect_upstream_state_bindings(
    src: DslSource<'_>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    scan_upstream_state_let(src.merged_text(), &mut out);
    out
}

/// Line-by-line state machine that captures `let <name> = upstream_state { ... }`
/// and the first `source = '...'` inside its body. Handles both the common
/// multi-line form and the single-line
/// `let x = upstream_state { source = '...' }`.
///
/// A bare `find("let ")` walk would match `let ` embedded in comments or
/// string literals; requiring `let` at the start of a trimmed line dodges
/// those false positives.
fn scan_upstream_state_let(text: &str, out: &mut std::collections::HashMap<String, String>) {
    let mut pending: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some((name, rhs)) = crate::let_parse::parse_let_header(line) {
            // Any new `let` ends a previous `upstream_state` search window,
            // preventing a sibling block's `source` from being misattributed.
            pending = None;
            let rhs_after = rhs.strip_prefix("upstream_state").map(str::trim_start);
            if let Some(after_keyword) = rhs_after {
                if let Some(body) = after_keyword.strip_prefix('{')
                    && let Some(src) = find_source_in_line(body)
                {
                    out.insert(name.to_string(), src);
                    continue;
                }
                pending = Some(name.to_string());
            }
            continue;
        }
        if let Some(binding) = &pending {
            if let Some(src) = find_source_in_line(trimmed) {
                out.insert(binding.clone(), src);
                pending = None;
                continue;
            }
            if trimmed.starts_with('}') {
                pending = None;
            }
        }
    }
}

/// If `segment` (a single line or the tail of one) contains `source = '...'`
/// or `source = "..."`, return the inner string.
fn find_source_in_line(segment: &str) -> Option<String> {
    let trimmed = segment.trim_start();
    let rest = trimmed.strip_prefix("source")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let inner = &rest[quote.len_utf8()..];
    let end = inner.find(quote)?;
    Some(inner[..end].to_string())
}

/// If `prefix` ends with `source = '<partial>` or `source = "<partial>`
/// (unclosed quote of either kind), return `<partial>`. Otherwise return
/// `None`.
fn extract_upstream_source_partial(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim_start();
    parse_source_partial_from(trimmed)
}

/// Same as `extract_upstream_source_partial`, but searches for the last
/// occurrence of `source = '…` / `source = "…` anywhere within `prefix`
/// (not just at its trimmed start). Needed for single-line shapes like
/// `let x = use { source = '<cursor>` where the prefix carries the full
/// let header, not just the attribute.
fn extract_source_partial_anywhere(prefix: &str) -> Option<String> {
    let mut search_from = prefix.len();
    while let Some(idx) = prefix[..search_from].rfind("source") {
        // Treat only the source keyword as a standalone identifier — reject
        // matches where it's a suffix of a longer identifier like `my_source`.
        let boundary_ok = idx == 0
            || !prefix[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary_ok && let Some(partial) = parse_source_partial_from(&prefix[idx..]) {
            return Some(partial);
        }
        if idx == 0 {
            break;
        }
        search_from = idx;
    }
    None
}

/// Parse `source = '<partial>` (or with `"`) starting exactly at `s`.
/// Used by both [`extract_upstream_source_partial`] and
/// [`extract_source_partial_anywhere`].
fn parse_source_partial_from(s: &str) -> Option<String> {
    let rest = s.strip_prefix("source")?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (quote, rest) = if let Some(r) = rest.strip_prefix('\'') {
        ('\'', r)
    } else {
        ('"', rest.strip_prefix('"')?)
    };
    if rest.contains(quote) {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
mod helper_tests {
    use super::{
        extract_source_partial_anywhere, extract_upstream_source_partial, is_let_use_line,
    };

    #[test]
    fn single_quote_unclosed_returns_partial() {
        assert_eq!(
            extract_upstream_source_partial("source = '"),
            Some(String::new())
        );
        assert_eq!(
            extract_upstream_source_partial("source = '../ot"),
            Some("../ot".to_string())
        );
    }

    #[test]
    fn double_quote_unclosed_returns_partial() {
        assert_eq!(
            extract_upstream_source_partial("source = \""),
            Some(String::new())
        );
        assert_eq!(
            extract_upstream_source_partial("source = \"../ot"),
            Some("../ot".to_string())
        );
    }

    #[test]
    fn closed_quotes_return_none() {
        assert_eq!(extract_upstream_source_partial("source = '../x'"), None);
        assert_eq!(extract_upstream_source_partial("source = \"../x\""), None);
    }

    #[test]
    fn missing_pieces_return_none() {
        assert_eq!(extract_upstream_source_partial(""), None);
        assert_eq!(extract_upstream_source_partial("source"), None);
        assert_eq!(extract_upstream_source_partial("source ="), None);
        assert_eq!(extract_upstream_source_partial("source = ../x"), None);
    }

    #[test]
    fn anywhere_finds_partial_after_let_header() {
        // Single-line `let x = use { source = '../x` shape: the attribute
        // sits after the `let` / `use` / `{` tokens on the same line.
        assert_eq!(
            extract_source_partial_anywhere("let x = use { source = './modules/"),
            Some("./modules/".to_string())
        );
        assert_eq!(
            extract_source_partial_anywhere("let x = use { source = \"./modules/"),
            Some("./modules/".to_string())
        );
    }

    #[test]
    fn anywhere_respects_identifier_boundary() {
        // `my_source` ends in `source` but is a different identifier — must
        // not be matched.
        assert_eq!(
            extract_source_partial_anywhere("let x = use { my_source = './m/"),
            None
        );
    }

    #[test]
    fn anywhere_returns_none_when_quote_is_closed() {
        assert_eq!(
            extract_source_partial_anywhere("let x = use { source = './m' }"),
            None
        );
    }

    #[test]
    fn is_let_use_accepts_common_shapes() {
        assert!(is_let_use_line("let x = use { source = './m' }"));
        assert!(is_let_use_line("let x = use {"));
        assert!(is_let_use_line("let x = use{"));
    }

    #[test]
    fn is_let_use_rejects_near_misses() {
        // A longer identifier starting with `use` must not be mistaken for
        // the keyword.
        assert!(!is_let_use_line("let x = user_data"));
        // `read`, `upstream_state`, etc. live on other let RHS shapes.
        assert!(!is_let_use_line("let x = read awscc.ec2.Vpc { }"));
        assert!(!is_let_use_line("let x = upstream_state { source = '..' }"));
    }
}
