//! Attribute, value, and type-specific completions.

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::schema::AttributeType;

use super::CompletionProvider;

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
            _ => self.generic_value_completions(),
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
        // Calculate the replacement range: from right after "ref(" to the cursor position.
        // This ensures dotted identifiers like "aws.ec2.vpc" are replaced correctly
        // without duplication from LSP word-boundary-based insertion.
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let ref_content_start = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Find the last unclosed "ref(" and position right after the "("
            if let Some(ref_pos) = prefix.rfind("ref(") {
                (ref_pos + 4) as u32
            } else {
                position.character
            }
        } else {
            position.character
        };

        let replacement_range = Range {
            start: Position {
                line: position.line,
                character: ref_content_start,
            },
            end: position,
        };

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
                    text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                        range: replacement_range,
                        new_text: format!("{})", resource_type),
                    })),
                    ..Default::default()
                }
            })
            .collect()
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
