use std::path::Path;
use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use crate::document::Document;
use carina_core::parser;
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
            CompletionContext::AfterAwsRegion => self.region_completions(),
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

        // Check if we're typing after "aws.Region."
        if prefix.contains("aws.Region.") || prefix.ends_with("aws.Region") {
            return CompletionContext::AfterAwsRegion;
        }

        // Check if we're typing after "ref("
        if prefix.ends_with("ref(") || prefix.contains("ref(") && !prefix.contains(')') {
            return CompletionContext::AfterRefType;
        }

        // Check if we're inside a resource block or module call and find the type
        let mut brace_depth = 0;
        let mut resource_type = String::new();
        let mut module_name: Option<String> = None;

        for (i, line) in lines.iter().enumerate() {
            if i > line_idx {
                break;
            }
            let trimmed = line.trim();

            // Look for resource type declaration: "aws.vpc {" or "let x = aws.vpc {"
            if let Some(rt) = self.extract_resource_type(line)
                && brace_depth == 0
            {
                resource_type = rt;
                module_name = None;
            } else if brace_depth == 0
                && trimmed.ends_with('{')
                && !trimmed.starts_with("let ")
                && !trimmed.starts_with("aws.")
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

            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        resource_type.clear();
                        module_name = None;
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

        // Inside module call block
        if brace_depth > 0 {
            if let Some(name) = module_name {
                return CompletionContext::InsideModuleCall { module_name: name };
            }
            return CompletionContext::InsideResourceBlock { resource_type };
        }

        CompletionContext::TopLevel
    }

    /// Extract resource type from a line like "aws.vpc {" or "let x = aws.vpc {"
    fn extract_resource_type(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();

        // Pattern: "aws.xxx.yyy {" or "let name = aws.xxx.yyy {"
        // Maps DSL format to schema resource_type
        for (pattern, schema_type) in [
            ("aws.s3.bucket", "s3.bucket"),
            ("aws.vpc", "vpc"),
            ("aws.subnet", "subnet"),
            ("aws.internet_gateway", "internet_gateway"),
            ("aws.route_table", "route_table"),
            ("aws.route", "route"),
            (
                "aws.security_group.ingress_rule",
                "security_group.ingress_rule",
            ),
            (
                "aws.security_group.egress_rule",
                "security_group.egress_rule",
            ),
            ("aws.security_group", "security_group"),
        ] {
            if trimmed.contains(pattern) {
                return Some(schema_type.to_string());
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
                label: "ref".to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                insert_text: Some("ref(${1:aws.vpc})".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Typed resource reference".to_string()),
                ..Default::default()
            },
            // S3 resources
            CompletionItem {
                label: "aws.s3.bucket".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.s3.bucket {\n    name = \"${1:bucket-name}\"\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("S3 bucket resource".to_string()),
                ..Default::default()
            },
            // VPC resources
            CompletionItem {
                label: "aws.vpc".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.vpc {\n    name       = \"${1:vpc-name}\"\n    cidr_block = \"${2:10.0.0.0/16}\"\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("VPC resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.subnet".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.subnet {\n    name              = \"${1:subnet-name}\"\n    vpc_id            = ${2:vpc.id}\n    cidr_block        = \"${3:10.0.1.0/24}\"\n    availability_zone = aws.AvailabilityZone.${4:ap_northeast_1a}\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Subnet resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.internet_gateway".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.internet_gateway {\n    name   = \"${1:igw-name}\"\n    vpc_id = ${2:vpc.id}\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Internet Gateway resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.route_table".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.route_table {\n    name   = \"${1:rt-name}\"\n    vpc_id = ${2:vpc.id}\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Route Table resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.route".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.route {\n    name                   = \"${1:route-name}\"\n    route_table_id         = ${2:rt.id}\n    destination_cidr_block = \"${3:0.0.0.0/0}\"\n    gateway_id             = ${4:igw.id}\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Route in a Route Table".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.security_group {\n    name        = \"${1:sg-name}\"\n    vpc_id      = ${2:vpc.id}\n    description = \"${3:Security group description}\"\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.ingress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.security_group.ingress_rule {\n    name              = \"${1:rule-name}\"\n    security_group_id = ${2:sg.id}\n    protocol          = aws.Protocol.${3:tcp}\n    from_port         = ${4:80}\n    to_port           = ${5:80}\n    cidr              = \"${6:0.0.0.0/0}\"\n}".to_string(),
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group Ingress Rule".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.egress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: "aws.security_group.egress_rule {\n    name              = \"${1:rule-name}\"\n    security_group_id = ${2:sg.id}\n    protocol          = aws.Protocol.${3:all}\n    from_port         = ${4:0}\n    to_port           = ${5:0}\n    cidr              = \"${6:0.0.0.0/0}\"\n}".to_string(),
                })),
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
            AttributeType::Custom { name, .. } if name == "Cidr" => self.cidr_completions(),
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

    fn ref_type_completions(&self) -> Vec<CompletionItem> {
        // Provide completions for ref(aws.xxx) type syntax
        let resource_types = vec![
            ("aws.vpc", "VPC resource reference"),
            ("aws.subnet", "Subnet resource reference"),
            (
                "aws.internet_gateway",
                "Internet Gateway resource reference",
            ),
            ("aws.route_table", "Route Table resource reference"),
            ("aws.security_group", "Security Group resource reference"),
            (
                "aws.security_group.ingress_rule",
                "Security Group Ingress Rule reference",
            ),
            (
                "aws.security_group.egress_rule",
                "Security Group Egress Rule reference",
            ),
            ("aws.s3.bucket", "S3 Bucket resource reference"),
        ];

        resource_types
            .into_iter()
            .map(|(type_path, description)| CompletionItem {
                label: type_path.to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                detail: Some(description.to_string()),
                insert_text: Some(format!("{})", type_path)),
                ..Default::default()
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
            if let Ok(module_content) = std::fs::read_to_string(&module_path)
                && let Ok(parsed) = parser::parse(&module_content)
            {
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
    InsideModuleCall {
        module_name: String,
    },
    AfterEquals {
        resource_type: String,
        attr_name: String,
    },
    AfterAwsRegion,
    AfterRefType,
    AfterInputDot,
    None,
}
