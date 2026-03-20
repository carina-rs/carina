use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::schema::{CompletionValue, ResourceSchema};

/// Sanitize a description string by removing truncated markdown links.
/// If a `[text](url` is incomplete (no closing `)`) or the URL ends with `...`,
/// the entire link syntax is removed, leaving only the link text.
fn sanitize_truncated_markdown_links(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '[' {
            let link_start = i;
            i += 1;
            // Find closing ]
            let text_start = i;
            while i < len && chars[i] != ']' && chars[i] != '\n' {
                i += 1;
            }
            if i >= len || chars[i] == '\n' {
                // No closing ], output what we have as-is
                result.push_str(
                    &chars[link_start..=i.min(len - 1)]
                        .iter()
                        .collect::<String>(),
                );
                if i < len {
                    i += 1;
                }
                continue;
            }
            let link_text: String = chars[text_start..i].iter().collect();
            i += 1; // skip ]

            if i < len && chars[i] == '(' {
                i += 1; // skip (
                let url_start = i;
                while i < len && chars[i] != ')' && chars[i] != '\n' {
                    i += 1;
                }
                if i < len && chars[i] == ')' {
                    // Complete link - check if URL ends with "..."
                    let url: String = chars[url_start..i].iter().collect();
                    if url.ends_with("...") || url.ends_with("..") {
                        // Truncated URL - emit just the link text
                        result.push_str(&link_text);
                    } else {
                        // Valid link - keep it as-is
                        result.push_str(&chars[link_start..=i].iter().collect::<String>());
                    }
                    i += 1; // skip )
                } else {
                    // Incomplete link (no closing paren) - emit just the link text
                    result.push_str(&link_text);
                    // Also append any trailing "..." after the broken URL
                    let remaining: String = chars[url_start..i.min(len)].iter().collect();
                    if remaining.ends_with("...") {
                        result.push_str("...");
                    }
                }
            } else {
                // [text] without ( - just output as-is
                result.push('[');
                result.push_str(&link_text);
                result.push(']');
                // Don't consume the next char, let the main loop handle it
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

pub struct HoverProvider {
    schemas: Arc<HashMap<String, ResourceSchema>>,
    region_completions: Vec<CompletionValue>,
}

impl HoverProvider {
    pub fn new(
        schemas: Arc<HashMap<String, ResourceSchema>>,
        region_completions: Vec<CompletionValue>,
    ) -> Self {
        Self {
            schemas,
            region_completions,
        }
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
        // They don't start with "let" or a provider prefix
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
                        // Module calls: identifier { (not "let x = ..." or "provider." prefix)
                        // Check if any provider name prefix matches
                        let provider_prefixes: Vec<&str> = self
                            .schemas
                            .keys()
                            .filter_map(|k| k.split('.').next())
                            .collect();
                        let starts_with_provider = provider_prefixes
                            .iter()
                            .any(|p| trimmed.starts_with(&format!("{}.", p)));

                        if !trimmed.starts_with("let ")
                            && !starts_with_provider
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
        // Check against all schema keys
        for (resource_type, schema) in self.schemas.iter() {
            if word == resource_type || word.contains(resource_type.as_str()) {
                // Avoid matching substrings like "vpc_id" for "vpc"
                if word.contains(&format!("{}_", resource_type))
                    || word.contains(&format!("_{}", resource_type))
                {
                    continue;
                }
                return self.schema_hover(resource_type, schema);
            }
        }
        None
    }

    fn schema_hover(&self, resource_name: &str, schema: &ResourceSchema) -> Option<Hover> {
        let description = schema
            .description
            .as_deref()
            .unwrap_or("No description available");
        let description = sanitize_truncated_markdown_links(description);

        let mut content = format!(
            "## {}\n\n{}\n\n### Attributes\n\n",
            resource_name, description
        );

        for attr in schema.attributes.values() {
            let required = if attr.required { " **(required)**" } else { "" };
            let desc = sanitize_truncated_markdown_links(attr.description.as_deref().unwrap_or(""));
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
        for schema in self.schemas.values() {
            if let Some(attr) = schema.attributes.get(word) {
                let description = sanitize_truncated_markdown_links(
                    attr.description.as_deref().unwrap_or("No description"),
                );
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
        let content = Self::keyword_description(word)?;

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content.to_string(),
            }),
            range: None,
        })
    }

    fn keyword_description(word: &str) -> Option<&'static str> {
        match word {
            "provider" => Some(
                "## provider\n\nDefines a provider block with configuration.\n\n```carina\nprovider aws {\n    region = aws.Region.ap_northeast_1\n}\n```",
            ),
            "let" => Some(
                "## let\n\nDefines a named resource or variable binding.\n\n```carina\nlet my_bucket = aws.s3.bucket {\n    name = \"my-bucket\"\n    region = aws.Region.ap_northeast_1\n}\n```",
            ),
            "output" => Some(
                "## output\n\nDefines module output values that can be referenced by the caller.\n\n```carina\noutput {\n    bucket_name: String = my_bucket.name\n}\n```",
            ),
            "input" => Some(
                "## input\n\nDefines module input parameters that must be provided by the caller.\n\n```carina\ninput {\n    env: String\n    region: String\n}\n```",
            ),
            "import" => Some(
                "## import\n\nImports a module from a file or directory.\n\n```carina\nimport \"./modules/network/main.crn\" as network\n```",
            ),
            "backend" => Some(
                "## backend\n\nConfigures the state backend for storing resource state.\n\n```carina\nbackend s3 {\n    bucket = \"my-carina-state\"\n    key    = \"prod/carina.crnstate\"\n    region = aws.Region.ap_northeast_1\n}\n```",
            ),
            "read" => Some(
                "## read\n\nReads an existing resource as a data source without managing it.\n\n```carina\nlet my_vpc = read aws.ec2.vpc {\n    name = \"existing-vpc\"\n}\n```",
            ),
            _ => None,
        }
    }

    fn region_hover(&self, word: &str) -> Option<Hover> {
        if !word.contains("Region") && !word.contains("region") {
            return None;
        }

        // Find matching region from completions data
        for completion in &self.region_completions {
            // Extract region code from value like "aws.Region.ap_northeast_1"
            if let Some(code) = completion.value.split('.').next_back()
                && word.contains(code)
            {
                // Derive AWS format from underscore format
                let aws_code = code.replace('_', "-");

                // Collect all provider prefixes that have this region
                let prefixes: Vec<&str> = self
                    .region_completions
                    .iter()
                    .filter(|c| c.value.ends_with(code))
                    .filter_map(|c| c.value.strip_suffix(&format!(".Region.{}", code)))
                    .collect();

                let dsl_formats = prefixes
                    .iter()
                    .map(|p| format!("`{}.Region.{}`", p, code))
                    .collect::<Vec<_>>()
                    .join(" / ");

                let content = format!(
                    "## AWS Region\n\n**{}**\n\n- DSL format: {}\n- AWS format: `{}`",
                    completion.description, dsl_formats, aws_code
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

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::schema::{AttributeSchema, AttributeType};

    /// Helper: returns true if text contains a truncated or malformed markdown link.
    /// Checks for:
    /// 1. Links with URLs ending in "..." (truncated by codegen)
    /// 2. Incomplete links where `[text](url` has no closing `)`
    fn has_truncated_markdown_link(text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Look for start of markdown link: [
            if chars[i] == '[' {
                let link_start = i;
                i += 1;
                // Find closing ]
                while i < len && chars[i] != ']' {
                    i += 1;
                }
                if i >= len {
                    break;
                }
                i += 1; // skip ]
                // Check for (
                if i < len && chars[i] == '(' {
                    i += 1; // skip (
                    let url_start = i;
                    // Find closing ) - but only on the same "segment" (no newline-based link)
                    let mut found_close = false;
                    while i < len && chars[i] != ')' && chars[i] != '\n' {
                        i += 1;
                    }
                    if i < len && chars[i] == ')' {
                        // We found a complete link, check if URL ends with "..."
                        let url: String = chars[url_start..i].iter().collect();
                        if url.ends_with("...") || url.ends_with("..") {
                            return true;
                        }
                        found_close = true;
                        i += 1;
                    }
                    if !found_close {
                        // Incomplete link: [text](url-without-closing-paren
                        // Check if we actually had URL content (not just [text] followed by something else)
                        let _ = link_start; // suppress unused warning
                        return true;
                    }
                }
            } else {
                i += 1;
            }
        }

        false
    }

    fn create_hover_provider_with_description(
        resource_type: &str,
        description: &str,
    ) -> HoverProvider {
        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new(resource_type).with_description(description);
        schemas.insert(resource_type.to_string(), schema);
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn create_hover_provider_with_attr_description(
        resource_type: &str,
        attr_name: &str,
        attr_description: &str,
    ) -> HoverProvider {
        let mut schemas = HashMap::new();
        let schema = ResourceSchema::new(resource_type).attribute(
            AttributeSchema::new(attr_name, AttributeType::String)
                .with_description(attr_description),
        );
        schemas.insert(resource_type.to_string(), schema);
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    #[test]
    fn test_resource_hover_no_truncated_markdown_links() {
        // This description mirrors the real ec2.vpc description from codegen,
        // which gets truncated at 200 chars, cutting a markdown link mid-URL.
        let truncated_desc = "Specifies a virtual private cloud (VPC).  To add an IPv6 CIDR block to the VPC, see [AWS::EC2::VPCCidrBlock](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrbloc...";

        let provider = create_hover_provider_with_description("ec2.vpc", truncated_desc);
        let schema = provider.schemas.get("ec2.vpc").unwrap();
        let hover = provider.schema_hover("ec2.vpc", schema).unwrap();

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            !has_truncated_markdown_link(content),
            "Hover content should not contain truncated markdown links, but got:\n{}",
            content
        );
    }

    #[test]
    fn test_attribute_hover_no_truncated_markdown_links() {
        // This mirrors a real attribute description with a truncated link
        let truncated_desc = "Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-wi...";

        let provider = create_hover_provider_with_attr_description(
            "ec2.nat_gateway",
            "secondary_allocation_ids",
            truncated_desc,
        );

        let doc = Document::new("secondary_allocation_ids".to_string());
        let hover = provider
            .hover(&doc, Position::new(0, 5))
            .expect("Should find hover for attribute");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            !has_truncated_markdown_link(content),
            "Attribute hover should not contain truncated markdown links, but got:\n{}",
            content
        );
    }

    #[test]
    fn test_hover_description_with_complete_markdown_links_is_ok() {
        // A description with properly formed markdown links should be fine
        let good_desc =
            "See [VPC docs](https://docs.aws.amazon.com/vpc/latest/userguide/) for details.";

        let provider = create_hover_provider_with_description("ec2.vpc", good_desc);
        let schema = provider.schemas.get("ec2.vpc").unwrap();
        let hover = provider.schema_hover("ec2.vpc", schema).unwrap();

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            !has_truncated_markdown_link(content),
            "Complete markdown links should not be flagged as truncated:\n{}",
            content
        );
    }

    #[test]
    fn test_keyword_hover_provider() {
        assert!(HoverProvider::keyword_description("provider").is_some());
        let desc = HoverProvider::keyword_description("provider").unwrap();
        assert!(desc.contains("provider"));
    }

    #[test]
    fn test_keyword_hover_let() {
        assert!(HoverProvider::keyword_description("let").is_some());
        let desc = HoverProvider::keyword_description("let").unwrap();
        assert!(desc.contains("named resource"));
    }

    #[test]
    fn test_keyword_hover_output() {
        assert!(HoverProvider::keyword_description("output").is_some());
        let desc = HoverProvider::keyword_description("output").unwrap();
        assert!(desc.contains("output"));
    }

    #[test]
    fn test_keyword_hover_input() {
        assert!(HoverProvider::keyword_description("input").is_some());
        let desc = HoverProvider::keyword_description("input").unwrap();
        assert!(desc.contains("input"));
    }

    #[test]
    fn test_keyword_hover_import() {
        assert!(HoverProvider::keyword_description("import").is_some());
        let desc = HoverProvider::keyword_description("import").unwrap();
        assert!(desc.contains("import"));
    }

    #[test]
    fn test_keyword_hover_backend() {
        assert!(HoverProvider::keyword_description("backend").is_some());
        let desc = HoverProvider::keyword_description("backend").unwrap();
        assert!(desc.contains("backend"));
    }

    #[test]
    fn test_keyword_hover_read() {
        assert!(HoverProvider::keyword_description("read").is_some());
        let desc = HoverProvider::keyword_description("read").unwrap();
        assert!(desc.contains("data source"));
    }

    #[test]
    fn test_keyword_hover_unknown_returns_none() {
        assert!(HoverProvider::keyword_description("foobar").is_none());
    }
}
