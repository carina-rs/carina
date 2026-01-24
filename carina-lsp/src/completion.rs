use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position,
};

use crate::document::Document;
use carina_core::schema::{AttributeType, ResourceSchema};
use carina_provider_aws::schemas;
use std::collections::HashMap;

pub struct CompletionProvider {
    schemas: HashMap<String, ResourceSchema>,
}

impl CompletionProvider {
    pub fn new() -> Self {
        let mut schema_map = HashMap::new();
        for schema in schemas::all_schemas() {
            schema_map.insert(schema.resource_type.clone(), schema);
        }
        Self {
            schemas: schema_map,
        }
    }

    pub fn complete(&self, doc: &Document, position: Position) -> Vec<CompletionItem> {
        let text = doc.text();
        let context = self.get_completion_context(&text, position);

        match context {
            CompletionContext::TopLevel => self.top_level_completions(),
            CompletionContext::InsideResourceBlock { resource_type } => {
                self.attribute_completions_for_type(&resource_type)
            }
            CompletionContext::AfterEquals {
                resource_type,
                attr_name,
            } => self.value_completions_for_attr(&resource_type, &attr_name, &text),
            CompletionContext::AfterAwsRegion => self.region_completions(),
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

        // Check if we're typing after "aws.Region."
        if prefix.contains("aws.Region.") || prefix.ends_with("aws.Region") {
            return CompletionContext::AfterAwsRegion;
        }

        // Check if we're inside a resource block and find the resource type
        let mut brace_depth = 0;
        let mut resource_type = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > line_idx {
                break;
            }
            // Look for resource type declaration: "aws.vpc {" or "let x = aws.vpc {"
            if let Some(rt) = self.extract_resource_type(line)
                && brace_depth == 0
            {
                resource_type = rt;
            }
            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        resource_type.clear();
                    }
                }
            }
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

        // Inside resource block but not after equals
        if brace_depth > 0 {
            return CompletionContext::InsideResourceBlock { resource_type };
        }

        CompletionContext::TopLevel
    }

    /// Extract resource type from a line like "aws.vpc {" or "let x = aws.vpc {"
    fn extract_resource_type(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();

        // Pattern: "aws.xxx.yyy {" or "let name = aws.xxx.yyy {"
        for pattern in [
            "aws.s3.bucket",
            "aws.vpc",
            "aws.subnet",
            "aws.internet_gateway",
            "aws.route_table",
            "aws.route",
            "aws.security_group.ingress_rule",
            "aws.security_group.egress_rule",
            "aws.security_group",
        ] {
            if trimmed.contains(pattern) {
                // Convert DSL format to internal format
                let internal_type = match pattern {
                    "aws.s3.bucket" => "s3_bucket",
                    "aws.vpc" => "vpc",
                    "aws.subnet" => "subnet",
                    "aws.internet_gateway" => "internet_gateway",
                    "aws.route_table" => "route_table",
                    "aws.route" => "route",
                    "aws.security_group.ingress_rule" => "security_group.ingress_rule",
                    "aws.security_group.egress_rule" => "security_group.egress_rule",
                    "aws.security_group" => "security_group",
                    _ => continue,
                };
                return Some(internal_type.to_string());
            }
        }
        None
    }

    /// Extract attribute name from a line prefix like "    enable_dns_hostnames = "
    fn extract_attr_name(&self, prefix: &str) -> String {
        let before_eq = prefix.split('=').next().unwrap_or("").trim();
        before_eq.to_string()
    }

    fn top_level_completions(&self) -> Vec<CompletionItem> {
        vec![
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
            // S3 resources
            CompletionItem {
                label: "aws.s3.bucket".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.s3.bucket {\n    name = \"${1:bucket-name}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("S3 bucket resource".to_string()),
                ..Default::default()
            },
            // VPC resources
            CompletionItem {
                label: "aws.vpc".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.vpc {\n    name       = \"${1:vpc-name}\"\n    cidr_block = \"${2:10.0.0.0/16}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("VPC resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.subnet".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.subnet {\n    name              = \"${1:subnet-name}\"\n    vpc_id            = ${2:vpc.id}\n    cidr_block        = \"${3:10.0.1.0/24}\"\n    availability_zone = aws.AvailabilityZone.${4:ap_northeast_1a}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Subnet resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.internet_gateway".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.internet_gateway {\n    name   = \"${1:igw-name}\"\n    vpc_id = ${2:vpc.id}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Internet Gateway resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.route_table".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.route_table {\n    name   = \"${1:rt-name}\"\n    vpc_id = ${2:vpc.id}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Route Table resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.route".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.route {\n    name                   = \"${1:route-name}\"\n    route_table_id         = ${2:rt.id}\n    destination_cidr_block = \"${3:0.0.0.0/0}\"\n    gateway_id             = ${4:igw.id}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Route in a Route Table".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group {\n    name        = \"${1:sg-name}\"\n    vpc_id      = ${2:vpc.id}\n    description = \"${3:Security group description}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.ingress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group.ingress_rule {\n    name              = \"${1:rule-name}\"\n    security_group_id = ${2:sg.id}\n    protocol          = aws.Protocol.${3:tcp}\n    from_port         = ${4:80}\n    to_port           = ${5:80}\n    cidr              = \"${6:0.0.0.0/0}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group Ingress Rule".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.egress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group.egress_rule {\n    name              = \"${1:rule-name}\"\n    security_group_id = ${2:sg.id}\n    protocol          = aws.Protocol.${3:all}\n    from_port         = ${4:0}\n    to_port           = ${5:0}\n    cidr              = \"${6:0.0.0.0/0}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group Egress Rule".to_string()),
                ..Default::default()
            },
        ]
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

        // Look up the attribute type from schema
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            completions.extend(self.completions_for_type(&attr_schema.attr_type));
            return completions;
        }

        // Fall back to generic value completions
        completions.extend(self.generic_value_completions());
        completions
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
                // Check if this is a protocol enum
                if variants
                    .iter()
                    .any(|v| v == "tcp" || v == "udp" || v == "icmp")
                {
                    return self.protocol_completions();
                }
                // Generic enum completions
                variants
                    .iter()
                    .map(|v| CompletionItem {
                        label: v.clone(),
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        ..Default::default()
                    })
                    .collect()
            }
            AttributeType::Int => {
                vec![] // No specific completions for integers
            }
            AttributeType::String | AttributeType::Custom { .. } => {
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
        let regions = vec![
            ("ap_northeast_1", "Tokyo"),
            ("ap_northeast_2", "Seoul"),
            ("ap_northeast_3", "Osaka"),
            ("ap_south_1", "Mumbai"),
            ("ap_southeast_1", "Singapore"),
            ("ap_southeast_2", "Sydney"),
            ("ca_central_1", "Canada"),
            ("eu_central_1", "Frankfurt"),
            ("eu_west_1", "Ireland"),
            ("eu_west_2", "London"),
            ("eu_west_3", "Paris"),
            ("eu_north_1", "Stockholm"),
            ("sa_east_1", "Sao Paulo"),
            ("us_east_1", "N. Virginia"),
            ("us_east_2", "Ohio"),
            ("us_west_1", "N. California"),
            ("us_west_2", "Oregon"),
        ];

        regions
            .into_iter()
            .map(|(code, name)| CompletionItem {
                label: format!("aws.Region.{}", code),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(name.to_string()),
                insert_text: Some(format!("aws.Region.{}", code)),
                ..Default::default()
            })
            .collect()
    }

    fn protocol_completions(&self) -> Vec<CompletionItem> {
        let protocols = vec![
            ("tcp", "Transmission Control Protocol"),
            ("udp", "User Datagram Protocol"),
            ("icmp", "Internet Control Message Protocol"),
            ("all", "All protocols (-1)"),
        ];

        protocols
            .into_iter()
            .map(|(code, description)| CompletionItem {
                label: format!("aws.Protocol.{}", code),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(description.to_string()),
                insert_text: Some(format!("aws.Protocol.{}", code)),
                ..Default::default()
            })
            .collect()
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
                    label: format!("aws.AvailabilityZone.{}", az),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(detail),
                    insert_text: Some(format!("aws.AvailabilityZone.{}", az)),
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
    AfterEquals {
        resource_type: String,
        attr_name: String,
    },
    AfterAwsRegion,
    None,
}
