use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::providers::s3;

pub struct HoverProvider;

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

        // Check for attribute hover
        if let Some(hover) = self.attribute_hover(&word) {
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

    fn resource_type_hover(&self, word: &str) -> Option<Hover> {
        if word == "aws.s3.bucket" || word.contains("s3.bucket") {
            let schema = s3::bucket_schema();
            let description = schema
                .description
                .as_deref()
                .unwrap_or("S3 bucket resource");

            let mut content = format!("## aws.s3.bucket\n\n{}\n\n### Attributes\n\n", description);

            for attr in schema.attributes.values() {
                let required = if attr.required { " **(required)**" } else { "" };
                let desc = attr.description.as_deref().unwrap_or("");
                content.push_str(&format!("- `{}`: {}{}\n", attr.name, desc, required));
            }

            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: None,
            });
        }
        None
    }

    fn attribute_hover(&self, word: &str) -> Option<Hover> {
        let schema = s3::bucket_schema();

        if let Some(attr) = schema.attributes.get(word) {
            let description = attr.description.as_deref().unwrap_or("No description");
            let required = if attr.required {
                "Required"
            } else {
                "Optional"
            };
            let type_name = format!("{:?}", attr.attr_type);

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
                    "## AWS Region\n\n**{}**\n\n- DSL format: `aws.Region.{}`\n- AWS format: `{}`",
                    name, code, aws_code
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
