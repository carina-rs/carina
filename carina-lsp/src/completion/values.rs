//! Attribute, value, and type-specific completions.

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::builtins;
use carina_core::schema::AttributeType;

use super::CompletionProvider;

/// Context when the user has typed `binding_name.` after `=`.
struct BindingDotContext {
    binding_name: String,
    resource_type: String,
}

impl CompletionProvider {
    pub(super) fn attribute_completions_for_type(
        &self,
        resource_type: &str,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Command to trigger suggestions after inserting the completion
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        // Get schema for specific resource type
        if let Some(schema) = self.schemas.get(resource_type) {
            for attr in schema.attributes.values().filter(|a| !a.read_only) {
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

                // For List(Struct) attributes with block_name, offer block syntax completion
                if let Some(bn) = &attr.block_name
                    && matches!(
                        &attr.attr_type,
                        AttributeType::List { inner, .. } if matches!(inner.as_ref(), AttributeType::Struct { .. })
                    )
                {
                    completions.push(CompletionItem {
                        label: bn.clone(),
                        kind: Some(CompletionItemKind::SNIPPET),
                        detail: Some(format!("Block syntax for '{}'", attr.name)),
                        insert_text: Some(format!("{} {{\n  $0\n}}", bn)),
                        insert_text_format: Some(InsertTextFormat::SNIPPET),
                        ..Default::default()
                    });
                }
            }
        }
        // When resource type is unknown, return no attribute completions
        // rather than showing irrelevant attributes from all schemas.

        completions
    }

    pub(super) fn value_completions_for_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        text: &str,
        current_binding: Option<&str>,
        position: Position,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Compute the text_edit range for resource reference completions.
        // When the user has typed "igw." after "=", we need the range to cover
        // from the start of "igw" to the cursor, so accepting a completion like
        // "igw.internet_gateway_id" replaces "igw." instead of being appended.
        let ref_edit_range = self.compute_value_prefix_range(text, position);

        // Check if the user has typed "binding." after "=" — if so, show only
        // that binding's resource attributes, not built-in functions or generic completions.
        if let Some(dot_binding) = self.detect_binding_dot_context(text, position, current_binding)
        {
            return self.binding_attribute_completions(
                &dot_binding.binding_name,
                &dot_binding.resource_type,
                ref_edit_range,
            );
        }

        // Type-based resource reference completions:
        // Look up the attribute's type from the schema. If it's a Custom type,
        // find bindings whose resource schema has an attribute with the same Custom type name.
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            let target_type_name = Self::extract_custom_type_name(&attr_schema.attr_type);
            if let Some(target_name) = target_type_name {
                let bindings = self.extract_resource_bindings(text);
                for (binding_name, binding_resource_type) in &bindings {
                    if binding_resource_type.is_empty() {
                        continue;
                    }
                    // Skip self-references: don't suggest the current resource's own binding
                    if current_binding.is_some_and(|cb| cb == binding_name) {
                        continue;
                    }
                    // Look up the binding's resource schema and find attributes
                    // with matching Custom type name
                    if let Some(binding_schema) = self.schemas.get(binding_resource_type) {
                        for binding_attr in binding_schema.attributes.values() {
                            if let Some(binding_type_name) =
                                Self::extract_custom_type_name(&binding_attr.attr_type)
                                && binding_type_name == target_name
                            {
                                let full_ref = format!("{}.{}", binding_name, binding_attr.name);
                                completions.push(CompletionItem {
                                    label: full_ref.clone(),
                                    kind: Some(CompletionItemKind::REFERENCE),
                                    detail: Some(format!(
                                        "Reference to {}'s {} ({})",
                                        binding_name, binding_attr.name, target_name
                                    )),
                                    text_edit: Some(
                                        tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                                            range: ref_edit_range,
                                            new_text: full_ref,
                                        }),
                                    ),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }

        // Add argument parameter references (lexically scoped — direct name access)
        let argument_params = self.extract_argument_parameters(text);
        for (name, type_hint) in &argument_params {
            completions.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format!("argument: {}", type_hint)),
                insert_text: Some(name.clone()),
                ..Default::default()
            });
        }

        // Always include built-in function completions in value position
        completions.extend(self.builtin_function_completions());

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

    /// Detect if the user has typed `binding_name.` after `=` on the current line.
    /// Returns the binding name and its resource type if detected.
    fn detect_binding_dot_context(
        &self,
        text: &str,
        position: Position,
        current_binding: Option<&str>,
    ) -> Option<BindingDotContext> {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }

        let col = position.character as usize;
        let prefix: String = lines[line_idx].chars().take(col).collect();

        // Extract the value part after "="
        let after_eq = prefix.rsplit('=').next()?.trim();

        // Check if it looks like "identifier." (ends with dot or has dot followed by partial text)
        let dot_pos = after_eq.find('.')?;
        let candidate_binding = &after_eq[..dot_pos];

        // Validate binding name: alphanumeric + underscore
        if candidate_binding.is_empty()
            || !candidate_binding
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_')
        {
            return None;
        }

        // Look up this binding in the file's bindings
        let bindings = self.extract_resource_bindings(text);
        for (binding_name, binding_resource_type) in &bindings {
            if binding_name == candidate_binding && !binding_resource_type.is_empty() {
                // Skip self-references
                if current_binding.is_some_and(|cb| cb == binding_name) {
                    return None;
                }
                return Some(BindingDotContext {
                    binding_name: binding_name.clone(),
                    resource_type: binding_resource_type.clone(),
                });
            }
        }

        None
    }

    /// Provide completions for a binding's resource attributes.
    /// Shows all attributes of the binding's resource type as `binding.attribute` completions.
    fn binding_attribute_completions(
        &self,
        binding_name: &str,
        binding_resource_type: &str,
        edit_range: Range,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();
        if let Some(schema) = self.schemas.get(binding_resource_type) {
            for attr in schema.attributes.values() {
                let full_ref = format!("{}.{}", binding_name, attr.name);
                completions.push(CompletionItem {
                    label: full_ref.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: attr.description.clone(),
                    text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                        range: edit_range,
                        new_text: full_ref,
                    })),
                    ..Default::default()
                });
            }
        }
        completions
    }

    /// Compute a text edit range covering the value prefix the user has already typed
    /// after the `=` sign. This allows resource reference completions like `igw.internet_gateway_id`
    /// to replace the already-typed prefix (e.g., `igw.`) instead of being appended after it.
    fn compute_value_prefix_range(&self, text: &str, position: Position) -> Range {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let start_col = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Find the position after "= " (the value start)
            if let Some(eq_pos) = prefix.rfind('=') {
                let after_eq = &prefix[eq_pos + 1..];
                let whitespace_len = after_eq.len() - after_eq.trim_start().len();
                (eq_pos + 1 + whitespace_len) as u32
            } else {
                position.character
            }
        } else {
            position.character
        };

        Range {
            start: Position {
                line: position.line,
                character: start_col,
            },
            end: position,
        }
    }

    /// Extract the Custom type name from an AttributeType, if it is a Custom type.
    fn extract_custom_type_name(attr_type: &AttributeType) -> Option<&str> {
        match attr_type {
            AttributeType::Custom { name, .. } => Some(name),
            _ => None,
        }
    }

    pub(super) fn completions_for_type(&self, attr_type: &AttributeType) -> Vec<CompletionItem> {
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
            AttributeType::StringEnum {
                name,
                values,
                namespace,
                to_dsl,
            } => self.string_enum_completions(name, values, namespace.as_deref(), *to_dsl),
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
            AttributeType::Custom {
                name, namespace, ..
            } if name == "AvailabilityZone" => {
                self.availability_zone_completions(namespace.as_deref().unwrap_or(""), name)
            }
            // List(non-Struct): delegate to inner type completions
            AttributeType::List { inner, .. } => self.completions_for_type(inner),
            // Map: delegate to inner value type completions
            AttributeType::Map(inner) => self.completions_for_type(inner),
            // Union: collect completions from all member types
            AttributeType::Union(members) => {
                let mut completions = Vec::new();
                let mut seen_labels = std::collections::HashSet::new();
                for member in members {
                    for item in self.completions_for_type(member) {
                        if seen_labels.insert(item.label.clone()) {
                            completions.push(item);
                        }
                    }
                }
                completions
            }
            _ => vec![],
        }
    }

    pub(super) fn provider_block_completions(&self) -> Vec<CompletionItem> {
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        vec![CompletionItem {
            label: "region".to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some("Provider region".to_string()),
            insert_text: Some("region = ".to_string()),
            command: Some(trigger_suggest),
            ..Default::default()
        }]
    }

    pub(super) fn generic_value_completions(&self) -> Vec<CompletionItem> {
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
        ];

        completions.extend(self.region_completions());
        completions
    }

    /// Provide completions for built-in function names.
    pub(super) fn builtin_function_completions(&self) -> Vec<CompletionItem> {
        builtins::builtin_functions()
            .iter()
            .map(|func| CompletionItem {
                label: func.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(func.signature.to_string()),
                insert_text: Some(format!("{}($0)", func.name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn region_completions(&self) -> Vec<CompletionItem> {
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

    pub(super) fn region_completions_for_provider(
        &self,
        provider_name: &str,
    ) -> Vec<CompletionItem> {
        let prefix = format!("{}.Region.", provider_name);
        self.region_completions_data
            .iter()
            .filter(|c| c.value.starts_with(&prefix))
            .map(|c| CompletionItem {
                label: c.value.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(c.description.clone()),
                insert_text: Some(c.value.clone()),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn cidr_completions(&self) -> Vec<CompletionItem> {
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

    pub(super) fn ipv6_cidr_completions(&self) -> Vec<CompletionItem> {
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

    pub(super) fn arn_completions(&self) -> Vec<CompletionItem> {
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

    pub(super) fn ref_type_completions(
        &self,
        position: Position,
        text: &str,
    ) -> Vec<CompletionItem> {
        // Calculate the replacement range: from right after ":" to the cursor position.
        // This ensures dotted identifiers like "aws.ec2.vpc" are replaced correctly
        // without duplication from LSP word-boundary-based insertion.
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let type_start = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Find the colon and position right after it (plus any whitespace)
            if let Some(colon_pos) = prefix.rfind(':') {
                let after_colon = &prefix[colon_pos + 1..];
                let whitespace_len = after_colon.len() - after_colon.trim_start().len();
                (colon_pos + 1 + whitespace_len) as u32
            } else {
                position.character
            }
        } else {
            position.character
        };

        let replacement_range = Range {
            start: Position {
                line: position.line,
                character: type_start,
            },
            end: position,
        };

        let mut completions = Vec::new();

        // Basic types
        for (name, detail) in [
            ("string", "String type"),
            ("int", "Integer type"),
            ("bool", "Boolean type"),
            ("float", "Float type"),
        ] {
            completions.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                detail: Some(detail.to_string()),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: name.to_string(),
                })),
                ..Default::default()
            });
        }

        // Generic type constructors
        for (name, detail) in [
            ("list(", "List type constructor"),
            ("map(", "Map type constructor"),
        ] {
            completions.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                detail: Some(detail.to_string()),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: name.to_string(),
                })),
                ..Default::default()
            });
        }

        // Custom types from provider validators
        for name in &self.custom_type_names {
            completions.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                detail: Some(format!("Custom type: {}", name)),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: name.clone(),
                })),
                ..Default::default()
            });
        }

        // Resource types from schemas
        for resource_type in self.schemas.keys() {
            let description = self
                .schemas
                .get(resource_type)
                .and_then(|s| s.description.as_deref())
                .unwrap_or("Resource reference");

            completions.push(CompletionItem {
                label: resource_type.clone(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                detail: Some(format!("{} reference", description)),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: resource_type.clone(),
                })),
                ..Default::default()
            });
        }

        completions
    }

    pub(super) fn availability_zone_completions(
        &self,
        namespace: &str,
        type_name: &str,
    ) -> Vec<CompletionItem> {
        let prefix = if namespace.is_empty() {
            type_name.to_string()
        } else {
            format!("{}.{}", namespace, type_name)
        };

        // Build region display names from region_completions_data, filtered by namespace
        let region_prefix = format!("{}.Region.", namespace);
        let region_names: std::collections::HashMap<String, String> = self
            .region_completions_data
            .iter()
            .filter(|c| c.value.starts_with(&region_prefix))
            .filter_map(|c| {
                let region_code = c.value.strip_prefix(&region_prefix)?;
                // Extract short name from description like "Asia Pacific (Tokyo)" -> "Tokyo"
                let short_name = c
                    .description
                    .find('(')
                    .and_then(|start| {
                        c.description[start + 1..]
                            .find(')')
                            .map(|end| &c.description[start + 1..start + 1 + end])
                    })
                    .unwrap_or(&c.description);
                Some((region_code.to_string(), short_name.to_string()))
            })
            .collect();

        // Generate AZ completions for each region with zone letters a-d
        let zone_letters = ['a', 'b', 'c', 'd'];
        let mut completions = Vec::new();

        for (region_code, region_name) in &region_names {
            for &zone_letter in &zone_letters {
                let az = format!("{}{}", region_code, zone_letter);
                let label = format!("{}.{}", prefix, az);
                let detail = format!("{} Zone {}", region_name, zone_letter);
                completions.push(CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(detail),
                    insert_text: Some(label),
                    ..Default::default()
                });
            }
        }

        // Sort by label for consistent ordering
        completions.sort_by(|a, b| a.label.cmp(&b.label));
        completions
    }

    pub(super) fn string_enum_completions(
        &self,
        type_name: &str,
        values: &[String],
        namespace: Option<&str>,
        to_dsl: Option<fn(&str) -> String>,
    ) -> Vec<CompletionItem> {
        match namespace {
            Some(ns) => {
                let prefix = format!("{}.{}", ns, type_name);
                values
                    .iter()
                    .map(|value| {
                        let dsl_value = to_dsl.map_or_else(|| value.clone(), |f| f(value));
                        CompletionItem {
                            label: format!("{}.{}", prefix, dsl_value),
                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                            detail: Some(value.clone()),
                            ..Default::default()
                        }
                    })
                    .collect()
            }
            None => values
                .iter()
                .map(|value| CompletionItem {
                    label: format!("\"{}\"", value),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    insert_text: Some(format!("\"{}\"", value)),
                    ..Default::default()
                })
                .collect(),
        }
    }
}
