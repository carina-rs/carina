use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::schema::ResourceSchema;
use carina_provider_aws::schemas::generated as aws_generated;

pub struct HoverProvider;

impl Default for HoverProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl HoverProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn hover(&self, doc: &Document, position: Position) -> Option<Hover> {
        let word = doc.word_at(position)?;

        // Check for resource type hover
        if let Some(hover) = self.resource_type_hover(&word) {
            return Some(hover);
        }

        // Check for attribute hover (but not in module call context)
        if !self.is_in_module_call(doc, position)
            && let Some(hover) = self.attribute_hover(&word)
        {
            return Some(hover);
        }

        // Check for keyword hover
        if let Some(hover) = self.keyword_hover(&word) {
            return Some(hover);
        }

        // Check for region hover
        if let Some(hover) = self.region_hover(&word) {
            return Some(hover);
        }

        None
    }

    /// Check if the position is inside a module call block
    fn is_in_module_call(&self, doc: &Document, position: Position) -> bool {
        let text = doc.text();
        let lines: Vec<&str> = text.lines().collect();
        let current_line = position.line as usize;

        // Look backwards to find if we're in a module call block
        // Module calls look like: module_name { ... }
        // They don't start with "let" or "aws."
        let mut brace_depth = 0;

        for line_idx in (0..=current_line).rev() {
            let line = lines.get(line_idx).unwrap_or(&"");
            let trimmed = line.trim();

            // Count braces in this line (simplified)
            for ch in trimmed.chars() {
                if ch == '}' {
                    brace_depth += 1;
                } else if ch == '{' {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                    } else {
                        // Found opening brace, check if it's a module call
                        // Module calls: identifier { (not "let x = ..." or "aws.x.y {")
                        if !trimmed.starts_with("let ")
                            && !trimmed.starts_with("aws.")
                            && !trimmed.starts_with("provider ")
                            && !trimmed.starts_with("input ")
                            && !trimmed.starts_with("output ")
                            && trimmed.ends_with('{')
                        {
                            return true;
                        }
                        return false;
                    }
                }
            }
        }

        false
    }

    fn resource_type_hover(&self, word: &str) -> Option<Hover> {
        // S3 resources
        if word == "aws.s3.bucket" || word.contains("s3.bucket") {
            return self.schema_hover(
                "aws.s3.bucket",
                &aws_generated::s3_bucket::s3_bucket_config().schema,
            );
        }

        // EC2/VPC resources
        if word == "aws.ec2.vpc" || word.contains("ec2.vpc") && !word.contains("vpc_id") {
            return self.schema_hover(
                "aws.ec2.vpc",
                &aws_generated::ec2_vpc::ec2_vpc_config().schema,
            );
        }

        if word == "aws.ec2.subnet" || word.contains("ec2.subnet") && !word.contains("subnet_id") {
            return self.schema_hover(
                "aws.ec2.subnet",
                &aws_generated::ec2_subnet::ec2_subnet_config().schema,
            );
        }

        if word == "aws.ec2.internet_gateway" || word.contains("ec2.internet_gateway") {
            return self.schema_hover(
                "aws.ec2.internet_gateway",
                &aws_generated::ec2_internet_gateway::ec2_internet_gateway_config().schema,
            );
        }

        if word == "aws.ec2.route_table" || word.contains("ec2.route_table") {
            return self.schema_hover(
                "aws.ec2.route_table",
                &aws_generated::ec2_route_table::ec2_route_table_config().schema,
            );
        }

        if word == "aws.ec2.security_group_ingress" || word.contains("ec2.security_group_ingress") {
            return self.schema_hover(
                "aws.ec2.security_group_ingress",
                &aws_generated::ec2_security_group_ingress::ec2_security_group_ingress_config()
                    .schema,
            );
        }

        if word == "aws.ec2.security_group_egress" || word.contains("ec2.security_group_egress") {
            return self.schema_hover(
                "aws.ec2.security_group_egress",
                &aws_generated::ec2_security_group_egress::ec2_security_group_egress_config()
                    .schema,
            );
        }

        if word == "aws.ec2.security_group" || word.contains("ec2.security_group") {
            return self.schema_hover(
                "aws.ec2.security_group",
                &aws_generated::ec2_security_group::ec2_security_group_config().schema,
            );
        }

        None
    }

    fn schema_hover(&self, resource_name: &str, schema: &ResourceSchema) -> Option<Hover> {
        let description = schema
            .description
            .as_deref()
            .unwrap_or("No description available");

        let mut content = format!(
            "## {}\n\n{}\n\n### Attributes\n\n",
            resource_name, description
        );

        for attr in schema.attributes.values() {
            let required = if attr.required { " **(required)**" } else { "" };
            let desc = attr.description.as_deref().unwrap_or("");
            content.push_str(&format!("- `{}`: {}{}\n", attr.name, desc, required));
        }

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
    }

    fn attribute_hover(&self, word: &str) -> Option<Hover> {
        // Check all schemas for the attribute
        let schemas = vec![
            aws_generated::s3_bucket::s3_bucket_config().schema,
            aws_generated::ec2_vpc::ec2_vpc_config().schema,
            aws_generated::ec2_subnet::ec2_subnet_config().schema,
            aws_generated::ec2_internet_gateway::ec2_internet_gateway_config().schema,
            aws_generated::ec2_route_table::ec2_route_table_config().schema,
            aws_generated::ec2_security_group::ec2_security_group_config().schema,
            aws_generated::ec2_security_group_ingress::ec2_security_group_ingress_config().schema,
            aws_generated::ec2_security_group_egress::ec2_security_group_egress_config().schema,
        ];

        for schema in schemas {
            if let Some(attr) = schema.attributes.get(word) {
                let description = attr.description.as_deref().unwrap_or("No description");
                let required = if attr.required {
                    "Required"
                } else {
                    "Optional"
                };
                let type_name = format!("{}", attr.attr_type);

                let content = format!(
                    "## {}\n\n{}\n\n- **Type**: {}\n- **Required**: {}",
                    attr.name, description, type_name, required
                );

                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: content,
                    }),
                    range: None,
                });
            }
        }
        None
    }

    fn keyword_hover(&self, word: &str) -> Option<Hover> {
        let content = match word {
            "provider" => {
                "## provider\n\nDefines a provider block with configuration.\n\n```carina\nprovider aws {\n    region = aws.Region.ap_northeast_1\n}\n```"
            }
            "let" => {
                "## let\n\nDefines a named resource or variable binding.\n\n```carina\nlet my_bucket = aws.s3.bucket {\n    name = \"my-bucket\"\n    region = aws.Region.ap_northeast_1\n}\n```"
            }
            "env" => {
                "## env()\n\nReads a value from an environment variable.\n\n```carina\nname = env(\"BUCKET_NAME\")\n```"
            }
            _ => return None,
        };

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content.to_string(),
            }),
            range: None,
        })
    }

    fn region_hover(&self, word: &str) -> Option<Hover> {
        if !word.contains("Region") && !word.contains("region") {
            return None;
        }

        let regions = vec![
            ("ap_northeast_1", "Asia Pacific (Tokyo)", "ap-northeast-1"),
            ("ap_northeast_2", "Asia Pacific (Seoul)", "ap-northeast-2"),
            ("ap_northeast_3", "Asia Pacific (Osaka)", "ap-northeast-3"),
            ("ap_south_1", "Asia Pacific (Mumbai)", "ap-south-1"),
            (
                "ap_southeast_1",
                "Asia Pacific (Singapore)",
                "ap-southeast-1",
            ),
            ("ap_southeast_2", "Asia Pacific (Sydney)", "ap-southeast-2"),
            ("ca_central_1", "Canada (Central)", "ca-central-1"),
            ("eu_central_1", "Europe (Frankfurt)", "eu-central-1"),
            ("eu_west_1", "Europe (Ireland)", "eu-west-1"),
            ("eu_west_2", "Europe (London)", "eu-west-2"),
            ("eu_west_3", "Europe (Paris)", "eu-west-3"),
            ("eu_north_1", "Europe (Stockholm)", "eu-north-1"),
            ("sa_east_1", "South America (Sao Paulo)", "sa-east-1"),
            ("us_east_1", "US East (N. Virginia)", "us-east-1"),
            ("us_east_2", "US East (Ohio)", "us-east-2"),
            ("us_west_1", "US West (N. California)", "us-west-1"),
            ("us_west_2", "US West (Oregon)", "us-west-2"),
        ];

        for (code, name, aws_code) in regions {
            if word.contains(code) {
                let content = format!(
                    "## AWS Region\n\n**{}**\n\n- DSL format: `aws.Region.{}` / `awscc.Region.{}`\n- AWS format: `{}`",
                    name, code, code, aws_code
                );

                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: content,
                    }),
                    range: None,
                });
            }
        }

        None
    }
}
