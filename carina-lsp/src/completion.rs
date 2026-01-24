use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat, Position};

use crate::document::Document;
use carina_core::providers::{ec2, s3};

pub struct CompletionProvider;

impl CompletionProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn complete(&self, doc: &Document, position: Position) -> Vec<CompletionItem> {
        let text = doc.text();
        let context = self.get_completion_context(&text, position);

        match context {
            CompletionContext::TopLevel => self.top_level_completions(),
            CompletionContext::InsideResourceBlock => self.attribute_completions(),
            CompletionContext::AfterEquals => self.value_completions(),
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

        // Check if we're after an equals sign
        if prefix.contains('=') {
            let after_eq = prefix.split('=').next_back().unwrap_or("").trim();
            if after_eq.is_empty() || after_eq.starts_with("aws") {
                return CompletionContext::AfterEquals;
            }
        }

        // Check if we're inside a resource block
        let mut brace_depth = 0;
        for (i, line) in lines.iter().enumerate() {
            if i > line_idx {
                break;
            }
            for c in line.chars() {
                if c == '{' {
                    brace_depth += 1;
                } else if c == '}' {
                    brace_depth -= 1;
                }
            }
        }

        if brace_depth > 0 {
            return CompletionContext::InsideResourceBlock;
        }

        CompletionContext::TopLevel
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
                insert_text: Some("aws.s3.bucket {\n    name   = \"${1:bucket-name}\"\n    region = aws.Region.${2:ap_northeast_1}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("S3 bucket resource".to_string()),
                ..Default::default()
            },
            // VPC resources
            CompletionItem {
                label: "aws.vpc".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.vpc {\n    name       = \"${1:vpc-name}\"\n    region     = aws.Region.${2:ap_northeast_1}\n    cidr_block = \"${3:10.0.0.0/16}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("VPC resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.subnet".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.subnet {\n    name              = \"${1:subnet-name}\"\n    region            = aws.Region.${2:ap_northeast_1}\n    vpc_id            = ${3:vpc.id}\n    cidr_block        = \"${4:10.0.1.0/24}\"\n    availability_zone = \"${5:ap-northeast-1a}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Subnet resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.internet_gateway".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.internet_gateway {\n    name   = \"${1:igw-name}\"\n    region = aws.Region.${2:ap_northeast_1}\n    vpc_id = ${3:vpc.id}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Internet Gateway resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.route_table".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.route_table {\n    name   = \"${1:rt-name}\"\n    region = aws.Region.${2:ap_northeast_1}\n    vpc_id = ${3:vpc.id}\n    routes = [\n        { destination = \"${4:0.0.0.0/0}\", gateway_id = ${5:igw.id} }\n    ]\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Route Table resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group {\n    name        = \"${1:sg-name}\"\n    region      = aws.Region.${2:ap_northeast_1}\n    vpc_id      = ${3:vpc.id}\n    description = \"${4:Security group description}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group resource".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.ingress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group.ingress_rule {\n    name              = \"${1:rule-name}\"\n    region            = aws.Region.${2:ap_northeast_1}\n    security_group_id = ${3:sg.id}\n    protocol          = \"${4:tcp}\"\n    from_port         = ${5:80}\n    to_port           = ${6:80}\n    cidr              = \"${7:0.0.0.0/0}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group Ingress Rule".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "aws.security_group.egress_rule".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                insert_text: Some("aws.security_group.egress_rule {\n    name              = \"${1:rule-name}\"\n    region            = aws.Region.${2:ap_northeast_1}\n    security_group_id = ${3:sg.id}\n    protocol          = \"${4:-1}\"\n    from_port         = ${5:0}\n    to_port           = ${6:0}\n    cidr              = \"${7:0.0.0.0/0}\"\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Security Group Egress Rule".to_string()),
                ..Default::default()
            },
        ]
    }

    fn attribute_completions(&self) -> Vec<CompletionItem> {
        // Collect attributes from all schemas
        let schemas = vec![
            s3::bucket_schema(),
            ec2::vpc_schema(),
            ec2::subnet_schema(),
            ec2::internet_gateway_schema(),
            ec2::route_table_schema(),
            ec2::security_group_schema(),
            ec2::security_group_ingress_rule_schema(),
            ec2::security_group_egress_rule_schema(),
        ];

        let mut seen = std::collections::HashSet::new();
        let mut completions = Vec::new();

        for schema in schemas {
            for attr in schema.attributes.values() {
                if seen.insert(attr.name.clone()) {
                    let detail = attr.description.clone();
                    let required_marker = if attr.required { " (required)" } else { "" };

                    completions.push(CompletionItem {
                        label: attr.name.clone(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: detail.map(|d| format!("{}{}", d, required_marker)),
                        insert_text: Some(format!("{} = ", attr.name)),
                        ..Default::default()
                    });
                }
            }
        }

        completions
    }

    fn value_completions(&self) -> Vec<CompletionItem> {
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
}

#[derive(Debug)]
#[allow(dead_code)]
enum CompletionContext {
    TopLevel,
    InsideResourceBlock,
    AfterEquals,
    AfterAwsRegion,
    None,
}
