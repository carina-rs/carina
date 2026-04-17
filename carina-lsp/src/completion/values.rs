//! Attribute, value, and type-specific completions.

use std::path::Path;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::builtins;
use carina_core::schema::AttributeType;

use super::CompletionProvider;

/// How far up the directory tree to walk when suggesting upstream_state sources.
const UPSTREAM_SOURCE_MAX_UP: usize = 6;
/// Safety cap on the number of suggestions returned.
const UPSTREAM_SOURCE_MAX_ITEMS: usize = 100;

/// Context when the user has typed `binding_name.` after `=`.
struct BindingDotContext {
    binding_name: String,
    resource_type: String,
}

fn type_completion_item(label: String, detail: String, range: Range) -> CompletionItem {
    CompletionItem {
        label: label.clone(),
        kind: Some(CompletionItemKind::TYPE_PARAMETER),
        detail: Some(detail),
        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: label,
        })),
        ..Default::default()
    }
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

        // Look up the attribute type from schema
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            // Struct types: offer `{ }` snippet only, no built-in functions
            if matches!(&attr_schema.attr_type, AttributeType::Struct { .. }) {
                completions.push(CompletionItem {
                    label: "{ }".to_string(),
                    kind: Some(CompletionItemKind::SNIPPET),
                    detail: Some("Open struct block".to_string()),
                    insert_text: Some("{\n  $0\n}".to_string()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
                return completions;
            }

            // Include built-in function completions for non-Struct value positions
            completions.extend(self.builtin_function_completions());

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
            completions
                .extend(self.completions_for_type(&attr_schema.attr_type, Some(resource_type)));
            return completions;
        }

        // No schema found — include built-in function completions as fallback
        completions.extend(self.builtin_function_completions());

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

    pub(super) fn completions_for_type(
        &self,
        attr_type: &AttributeType,
        resource_type: Option<&str>,
    ) -> Vec<CompletionItem> {
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
            } => {
                // Use explicit namespace if available, otherwise derive from resource_type
                let effective_ns = namespace.as_deref().or(if !name.is_empty() {
                    resource_type
                } else {
                    None
                });
                self.string_enum_completions(name, values, effective_ns, *to_dsl)
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
            AttributeType::Custom {
                name, namespace, ..
            } if name == "AvailabilityZone" => {
                self.availability_zone_completions(namespace.as_deref().unwrap_or(""), name)
            }
            // List(non-Struct): delegate to inner type completions
            AttributeType::List { inner, .. } => self.completions_for_type(inner, resource_type),
            // Map: delegate to inner value type completions
            AttributeType::Map { value: inner, .. } => {
                self.completions_for_type(inner, resource_type)
            }
            // Union: collect completions from all member types
            AttributeType::Union(members) => {
                let mut completions = Vec::new();
                let mut seen_labels = std::collections::HashSet::new();
                for member in members {
                    for item in self.completions_for_type(member, resource_type) {
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

        vec![
            CompletionItem {
                label: "region".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Provider region".to_string()),
                insert_text: Some("region = ".to_string()),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "source".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Provider source (e.g., github.com/owner/repo)".to_string()),
                insert_text: Some("source = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
            CompletionItem {
                label: "version".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Version constraint (e.g., ~0.5.0)".to_string()),
                insert_text: Some("version = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
            CompletionItem {
                label: "revision".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(
                    "Git revision (branch, tag, or SHA) for CI artifact resolution".to_string(),
                ),
                insert_text: Some("revision = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
        ]
    }

    pub(super) fn upstream_state_block_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "source".to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some("Path to the upstream project directory".to_string()),
            insert_text: Some("source = \"$0\"".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        }]
    }

    /// Suggest sibling (and uncle/grand-uncle) directories that look like carina
    /// projects, for use as the `source` attribute of an `upstream_state` block.
    pub(super) fn upstream_state_source_completions(
        &self,
        partial_path: &str,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let Some(base) = base_path else {
            return vec![];
        };
        let base_abs = canonical_or_self(base);

        // Skip the directory that is the current project itself; everything else
        // we emit at most once, preferring the nearest ancestor encoding.
        let mut seen_targets: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        seen_targets.insert(base_abs.clone());
        let mut suggestions: Vec<String> = Vec::new();

        let mut current = base_abs;
        for depth in 1..=UPSTREAM_SOURCE_MAX_UP {
            let Some(parent) = current.parent().map(Path::to_path_buf) else {
                break;
            };
            find_carina_projects_under(&parent, depth, &mut seen_targets, &mut suggestions);
            if suggestions.len() >= UPSTREAM_SOURCE_MAX_ITEMS {
                suggestions.truncate(UPSTREAM_SOURCE_MAX_ITEMS);
                break;
            }
            current = parent;
        }

        // Truncated by ancestor discovery order (nearest first), then sorted for display.
        suggestions.sort();

        // Replace the typed partial (from just after the opening quote to the
        // cursor) with the full suggestion. A plain `insert_text` would leave
        // the client to infer the replacement range via word boundaries, which
        // splits on `/` and `.` and produces `../../foo` when the user typed
        // `../or` — an explicit TextEdit avoids that.
        let partial_chars = partial_path.chars().count() as u32;
        let range = Range {
            start: Position {
                line: position.line,
                character: position.character.saturating_sub(partial_chars),
            },
            end: position,
        };

        suggestions
            .into_iter()
            .filter(|p| partial_path.is_empty() || p.starts_with(partial_path))
            .map(|p| CompletionItem {
                // Label keeps quotes so the popup reads like the source form;
                // the TextEdit inserts the bare path, since the cursor is
                // already inside an open quote.
                label: format!("'{}'", p),
                kind: Some(CompletionItemKind::FOLDER),
                detail: Some("Carina project".to_string()),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: p,
                })),
                ..Default::default()
            })
            .collect()
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
                label: format!("'{}'", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("'{}'", cidr)),
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
                label: format!("'{}'", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("'{}'", cidr)),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn arn_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "'arn:aws:...'".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            insert_text: Some(
                "'arn:aws:${1:service}:${2:region}:${3:account}:${4:resource}'".to_string(),
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

        let (type_start, inside_generic) = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Check if cursor is inside list() or map() — find last open paren after colon
            if let Some(colon_pos) = prefix.rfind(':') {
                let after_colon = &prefix[colon_pos + 1..];
                if let Some(paren_pos) = after_colon.rfind('(') {
                    // Inside list() or map(): start after the open paren
                    let abs_paren = colon_pos + 1 + paren_pos + 1;
                    (abs_paren as u32, true)
                } else {
                    let whitespace_len = after_colon.len() - after_colon.trim_start().len();
                    ((colon_pos + 1 + whitespace_len) as u32, false)
                }
            } else {
                (position.character, false)
            }
        } else {
            (position.character, false)
        };

        let replacement_range = Range {
            start: Position {
                line: position.line,
                character: type_start,
            },
            end: position,
        };

        // Basic types
        let basic = [
            ("string", "String type"),
            ("int", "Integer type"),
            ("bool", "Boolean type"),
            ("float", "Float type"),
        ]
        .iter()
        .map(|(name, detail)| {
            type_completion_item(name.to_string(), detail.to_string(), replacement_range)
        });

        // Generic type constructors (skip when already inside list()/map())
        let generic: Vec<CompletionItem> = if inside_generic {
            vec![]
        } else {
            [
                ("list(", "List type constructor"),
                ("map(", "Map type constructor"),
            ]
            .iter()
            .map(|(name, detail)| {
                type_completion_item(name.to_string(), detail.to_string(), replacement_range)
            })
            .collect()
        };

        // Custom types: built-in types + provider-extracted types (deduplicated)
        let builtin_custom = ["ipv4_cidr", "ipv4_address", "ipv6_cidr", "ipv6_address"];
        let mut seen_custom = std::collections::HashSet::new();
        let custom: Vec<CompletionItem> = builtin_custom
            .iter()
            .map(|s| s.to_string())
            .chain(self.custom_type_names.iter().cloned())
            .filter(|name| seen_custom.insert(name.clone()))
            .map(|name| {
                type_completion_item(
                    name.clone(),
                    format!("Custom type: {}", name),
                    replacement_range,
                )
            })
            .collect();

        // Resource types from schemas
        let resource = self.schemas.iter().map(move |(resource_type, schema)| {
            let description = schema
                .description
                .as_deref()
                .unwrap_or("Resource reference");
            type_completion_item(
                resource_type.clone(),
                format!("{} reference", description),
                replacement_range,
            )
        });

        basic.chain(generic).chain(custom).chain(resource).collect()
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
        _type_name: &str,
        values: &[String],
        namespace: Option<&str>,
        to_dsl: Option<fn(&str) -> String>,
    ) -> Vec<CompletionItem> {
        match namespace {
            Some(_) => {
                // Bare enum values — the schema context resolves them automatically
                values
                    .iter()
                    .map(|value| {
                        let dsl_value = to_dsl.map_or_else(|| value.clone(), |f| f(value));
                        CompletionItem {
                            label: dsl_value,
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
                    label: format!("'{}'", value),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    insert_text: Some(format!("'{}'", value)),
                    ..Default::default()
                })
                .collect(),
        }
    }
}

/// Scan `ancestor` for directories that look like carina projects.
///
/// At `up_depth == 1` (the direct parent of `base`), only the immediate
/// children are considered. From `up_depth >= 2` the scan also descends one
/// more level, so patterns like `../../modules/web` are reachable even when
/// the `modules` dir itself is not a project. The descent is deliberately
/// capped at grandchildren to keep the search bounded at every ancestor.
///
/// `seen` holds the canonical paths already emitted at a nearer ancestor (and
/// the base itself); entries already in `seen` are skipped so a sibling never
/// reappears via a longer ancestor route.
fn find_carina_projects_under(
    ancestor: &Path,
    up_depth: usize,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(ancestor) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let Some(child_name) = visible_dir_name(&child) else {
            continue;
        };
        let child_abs = canonical_or_self(&child);
        if dir_has_crn_file(&child) {
            if seen.insert(child_abs) {
                out.push(join_relative(up_depth, &[&child_name]));
            }
            continue;
        }
        if up_depth < 2 {
            continue;
        }
        let Ok(grandchildren) = std::fs::read_dir(&child) else {
            continue;
        };
        for grand in grandchildren.flatten() {
            let grand_path = grand.path();
            let Some(grand_name) = visible_dir_name(&grand_path) else {
                continue;
            };
            let grand_abs = canonical_or_self(&grand_path);
            if dir_has_crn_file(&grand_path) && seen.insert(grand_abs) {
                out.push(join_relative(up_depth, &[&child_name, &grand_name]));
            }
        }
    }
}

/// `Some(name)` if `path` is a directory with a non-hidden name, else `None`.
fn visible_dir_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') || !path.is_dir() {
        return None;
    }
    Some(name.to_string())
}

fn canonical_or_self(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn dir_has_crn_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().extension().is_some_and(|ext| ext == "crn"))
}

fn join_relative(up_depth: usize, parts: &[&str]) -> String {
    let mut s = String::new();
    for _ in 0..up_depth {
        s.push_str("../");
    }
    s.push_str(&parts.join("/"));
    s
}
