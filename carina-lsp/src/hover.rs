use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::builtins;
use carina_core::parser::ArgumentParameter;
use carina_core::resource::Value;
use carina_core::schema::{CompletionValue, ResourceSchema};

/// Format a Value for hover display
fn format_value_for_hover(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(_) => "[...]".to_string(),
        Value::Map(_) => "{...}".to_string(),
        _ => format!("{:?}", value),
    }
}

/// Convert Markdown links `[text](url)` to plain text `text (url)` for hover display.
/// LSP hover popups render as plain text in many editors, so raw Markdown link syntax
/// appears as broken formatting. This converts links to a readable plain-text form
/// while keeping the URL visible.
fn convert_markdown_links_to_plain_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '[' {
            let link_start = i;
            i += 1;
            let text_start = i;
            // Find closing ]
            while i < len && chars[i] != ']' && chars[i] != '\n' {
                i += 1;
            }
            if i >= len || chars[i] == '\n' {
                // No closing ], output as-is
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
                    let url: String = chars[url_start..i].iter().collect();
                    i += 1; // skip )
                    // Convert [text](url) -> text (url)
                    result.push_str(&link_text);
                    result.push_str(" (");
                    result.push_str(&url);
                    result.push(')');
                } else {
                    // Incomplete link, output as-is
                    result.push('[');
                    result.push_str(&link_text);
                    result.push_str("](");
                    result.push_str(&chars[url_start..i.min(len)].iter().collect::<String>());
                }
            } else {
                // [text] without ( - output as-is
                result.push('[');
                result.push_str(&link_text);
                result.push(']');
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
        self.hover_with_base_path(doc, position, None)
    }

    pub fn hover_with_base_path(
        &self,
        doc: &Document,
        position: Position,
        base_path: Option<&Path>,
    ) -> Option<Hover> {
        let word = doc.word_at(position)?;

        // Check for resource type hover
        if let Some(hover) = self.resource_type_hover(&word) {
            return Some(hover);
        }

        // Check for module argument description hover (inside module calls)
        if self.is_in_module_call(doc, position)
            && let Some(base) = base_path
            && let Some(hover) = self.module_argument_hover(doc, position, &word, base)
        {
            return Some(hover);
        }

        // Check for attribute hover (but not in module call context)
        if !self.is_in_module_call(doc, position) {
            let enclosing_resource = self.find_enclosing_resource_type(doc, position);
            if let Some(hover) = self.attribute_hover(&word, enclosing_resource.as_deref()) {
                return Some(hover);
            }
        }

        // Check for built-in function hover
        if let Some(hover) = self.builtin_function_hover(&word) {
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

    fn builtin_function_hover(&self, word: &str) -> Option<Hover> {
        let func = builtins::builtin_functions()
            .iter()
            .find(|f| f.name == word)?;

        let content = format!(
            "## {}\n\n```\n{}\n```\n\n{}",
            func.name, func.signature, func.description
        );

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
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
                            && !trimmed.starts_with("arguments ")
                            && !trimmed.starts_with("attributes ")
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

    /// Show hover info for a module call argument, including its description if available.
    fn module_argument_hover(
        &self,
        doc: &Document,
        position: Position,
        word: &str,
        base_path: &Path,
    ) -> Option<Hover> {
        let parsed = doc.parsed()?;

        // Find the enclosing module call name
        let module_name = self.find_enclosing_module_call_name(doc, position)?;

        // Find the import for this module
        let import = parsed.imports.iter().find(|imp| imp.alias == module_name)?;

        // Load the module to get argument definitions
        let module_path = base_path.join(&import.path);
        let module_parsed = carina_core::module_resolver::load_module(&module_path)?;

        // Find the argument matching the word
        let arg = module_parsed.arguments.iter().find(|a| a.name == word)?;

        // Build hover content
        self.build_module_argument_hover(arg, &module_name)
    }

    fn build_module_argument_hover(
        &self,
        arg: &ArgumentParameter,
        module_name: &str,
    ) -> Option<Hover> {
        let mut content = format!("## {}.{}\n\n", module_name, arg.name);

        if let Some(desc) = &arg.description {
            content.push_str(desc);
            content.push_str("\n\n");
        }

        content.push_str(&format!("- **Type**: {}\n", arg.type_expr));

        if let Some(default) = &arg.default {
            let default_str = format_value_for_hover(default);
            content.push_str(&format!("- **Default**: {}\n", default_str));
        } else {
            content.push_str("- **Required**\n");
        }

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
    }

    /// Find the name of the enclosing module call block.
    fn find_enclosing_module_call_name(
        &self,
        doc: &Document,
        position: Position,
    ) -> Option<String> {
        let text = doc.text();
        let lines: Vec<&str> = text.lines().collect();
        let current_line = position.line as usize;

        let mut brace_depth = 0;

        for line_idx in (0..=current_line).rev() {
            let line = lines.get(line_idx).unwrap_or(&"");
            let trimmed = line.trim();

            for ch in trimmed.chars() {
                if ch == '}' {
                    brace_depth += 1;
                } else if ch == '{' {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                    } else {
                        // Found opening brace. Check if it's a module call.
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
                            && !trimmed.starts_with("arguments ")
                            && !trimmed.starts_with("attributes ")
                            && trimmed.ends_with('{')
                        {
                            // Extract module name (first word before '{')
                            let name = trimmed.split_whitespace().next()?;
                            return Some(name.to_string());
                        }
                        return None;
                    }
                }
            }
        }

        None
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
        let description = convert_markdown_links_to_plain_text(
            schema
                .description
                .as_deref()
                .unwrap_or("No description available"),
        );

        let mut content = format!(
            "## {}\n\n{}\n\n### Attributes\n\n",
            resource_name, description
        );

        for attr in schema.attributes.values() {
            let required = if attr.required { " **(required)**" } else { "" };
            let desc =
                convert_markdown_links_to_plain_text(attr.description.as_deref().unwrap_or(""));
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

    /// Walk backwards from the cursor position, tracking brace depth, to find
    /// the enclosing resource block and extract its resource type (schema key).
    fn find_enclosing_resource_type(&self, doc: &Document, position: Position) -> Option<String> {
        let text = doc.text();
        let lines: Vec<&str> = text.lines().collect();
        let current_line = position.line as usize;

        // Build a list of schema keys sorted longest-first for correct matching
        let mut schema_keys: Vec<&str> = self.schemas.keys().map(|s| s.as_str()).collect();
        schema_keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

        let mut brace_depth: i32 = 0;

        // Walk backwards from the current line
        for line_idx in (0..=current_line).rev() {
            let line = lines.get(line_idx).unwrap_or(&"");

            // Count braces in reverse character order
            for ch in line.chars().rev() {
                if ch == '}' {
                    brace_depth += 1;
                } else if ch == '{' {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                    } else {
                        // Found the opening brace of the enclosing block.
                        // Check if this line contains a resource type.
                        for key in &schema_keys {
                            if line.contains(key) {
                                return Some(key.to_string());
                            }
                        }
                        return None;
                    }
                }
            }
        }

        None
    }

    fn attribute_hover(&self, word: &str, enclosing_resource: Option<&str>) -> Option<Hover> {
        // If we know the enclosing resource type, look up only that schema
        if let Some(resource_type) = enclosing_resource
            && let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr) = schema.attributes.get(word)
        {
            return self.build_attribute_hover(attr);
        }

        // Fall back to iterating all schemas
        for schema in self.schemas.values() {
            if let Some(attr) = schema.attributes.get(word) {
                return self.build_attribute_hover(attr);
            }
        }
        None
    }

    fn build_attribute_hover(&self, attr: &carina_core::schema::AttributeSchema) -> Option<Hover> {
        let description = convert_markdown_links_to_plain_text(
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

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
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
            "attributes" => Some(
                "## attributes\n\nDefines module attribute values that can be referenced by the caller.\n\n```carina\nattributes {\n    bucket_name: String = my_bucket.name\n}\n```",
            ),
            "arguments" => Some(
                "## arguments\n\nDefines module argument parameters that must be provided by the caller.\n\n```carina\narguments {\n    env: String\n    region: String\n}\n```",
            ),
            "import" => Some(
                "## import\n\nImports a module from a file or directory.\n\n```carina\nlet network = import \"./modules/network\"\n```",
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
    fn test_convert_markdown_links_to_plain_text() {
        // Basic link conversion
        assert_eq!(
            convert_markdown_links_to_plain_text("[VPC docs](https://docs.aws.amazon.com/vpc/)"),
            "VPC docs (https://docs.aws.amazon.com/vpc/)"
        );

        // Text with no links passes through unchanged
        assert_eq!(
            convert_markdown_links_to_plain_text("No links here."),
            "No links here."
        );

        // Multiple links in same text
        assert_eq!(
            convert_markdown_links_to_plain_text("See [A](https://a.com) and [B](https://b.com)."),
            "See A (https://a.com) and B (https://b.com)."
        );
    }

    #[test]
    fn test_resource_hover_converts_markdown_links() {
        // Full description with a markdown link (no truncation)
        let desc = "Specifies a virtual private cloud (VPC). To add an IPv6 CIDR block to the VPC, see [AWS::EC2::VPCCidrBlock](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrblock.html).";

        let provider = create_hover_provider_with_description("ec2.vpc", desc);
        let schema = provider.schemas.get("ec2.vpc").unwrap();
        let hover = provider.schema_hover("ec2.vpc", schema).unwrap();

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        // Should NOT contain raw markdown link syntax
        assert!(
            !content.contains("[AWS::EC2::VPCCidrBlock]("),
            "Hover should not contain raw markdown links, but got:\n{}",
            content
        );
        // Should contain the converted plain text form
        assert!(
            content.contains("AWS::EC2::VPCCidrBlock (https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrblock.html)"),
            "Hover should contain plain text link, but got:\n{}",
            content
        );
    }

    #[test]
    fn test_attribute_hover_converts_markdown_links() {
        // Attribute description with a markdown link (full, not truncated)
        let desc = "Secondary EIP allocation IDs. For more information, see [Create a NAT gateway](https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-with.html) in the Amazon VPC User Guide.";

        let provider = create_hover_provider_with_attr_description(
            "ec2.nat_gateway",
            "secondary_allocation_ids",
            desc,
        );

        let doc = Document::new("secondary_allocation_ids".to_string());
        let hover = provider
            .hover(&doc, Position::new(0, 5))
            .expect("Should find hover for attribute");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        // Should NOT contain raw markdown link syntax
        assert!(
            !content.contains("[Create a NAT gateway]("),
            "Attribute hover should not contain raw markdown links, but got:\n{}",
            content
        );
        // Should contain the converted plain text form
        assert!(
            content.contains("Create a NAT gateway (https://docs.aws.amazon.com/vpc/latest/userguide/nat-gateway-working-with.html)"),
            "Attribute hover should contain plain text link, but got:\n{}",
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
    fn test_keyword_hover_attributes() {
        assert!(HoverProvider::keyword_description("attributes").is_some());
        let desc = HoverProvider::keyword_description("attributes").unwrap();
        assert!(desc.contains("attributes"));
    }

    #[test]
    fn test_keyword_hover_arguments() {
        assert!(HoverProvider::keyword_description("arguments").is_some());
        let desc = HoverProvider::keyword_description("arguments").unwrap();
        assert!(desc.contains("arguments"));
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

    #[test]
    fn test_attribute_hover_uses_enclosing_resource_context() {
        // Two schemas both have "internet_gateway_id" but with different descriptions.
        // When hovering inside a vpc_gateway_attachment block, the hover should show
        // the description from vpc_gateway_attachment's schema, not internet_gateway's.
        let mut schemas = HashMap::new();

        let igw_schema = ResourceSchema::new("awscc.ec2.internet_gateway").attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description("The ID of the internet gateway (from internet_gateway schema)."),
        );
        schemas.insert("awscc.ec2.internet_gateway".to_string(), igw_schema);

        let attachment_schema = ResourceSchema::new("awscc.ec2.vpc_gateway_attachment").attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description(
                    "The ID of the internet gateway attached to the VPC (from vpc_gateway_attachment schema).",
                ),
        );
        schemas.insert(
            "awscc.ec2.vpc_gateway_attachment".to_string(),
            attachment_schema,
        );

        let provider = HoverProvider::new(Arc::new(schemas), vec![]);

        // Document with cursor inside the vpc_gateway_attachment block.
        // DSL resource lines use the full schema key as the resource type.
        let doc = Document::new(
            r#"awscc.ec2.internet_gateway {
    name = "my-igw"
}

awscc.ec2.vpc_gateway_attachment {
    internet_gateway_id = "igw-123"
}
"#
            .to_string(),
        );

        // Hover over "internet_gateway_id" on line 5 (0-indexed), column 10
        let hover = provider
            .hover(&doc, Position::new(5, 10))
            .expect("Should find hover for internet_gateway_id");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        // The description should come from vpc_gateway_attachment, not internet_gateway
        assert!(
            content.contains("from vpc_gateway_attachment schema"),
            "Hover should show description from the enclosing resource (vpc_gateway_attachment), \
             but got:\n{}",
            content
        );
        assert!(
            !content.contains("from internet_gateway schema"),
            "Hover should NOT show description from a different resource (internet_gateway), \
             but got:\n{}",
            content
        );
    }

    #[test]
    fn test_builtin_function_hover_join() {
        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let doc = Document::new("join".to_string());
        let hover = provider
            .hover(&doc, Position::new(0, 1))
            .expect("Should find hover for 'join'");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("## join"),
            "Should have function name header"
        );
        assert!(
            content.contains("join(separator: string, list: list) -> string"),
            "Should show signature. Got:\n{}",
            content
        );
        assert!(
            content.contains("Joins list elements"),
            "Should show description. Got:\n{}",
            content
        );
    }

    #[test]
    fn test_builtin_function_hover_cidr_subnet() {
        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let doc = Document::new("cidr_subnet".to_string());
        let hover = provider
            .hover(&doc, Position::new(0, 3))
            .expect("Should find hover for 'cidr_subnet'");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("cidr_subnet(prefix: string, newbits: int, netnum: int) -> string"),
            "Should show cidr_subnet signature. Got:\n{}",
            content
        );
    }

    #[test]
    fn test_builtin_function_hover_unknown_returns_none() {
        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let doc = Document::new("not_a_function".to_string());
        let hover = provider.hover(&doc, Position::new(0, 3));
        assert!(hover.is_none(), "Unknown function should not show hover");
    }

    #[test]
    fn test_all_builtin_functions_have_hover() {
        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let names = [
            "cidr_subnet",
            "concat",
            "env",
            "flatten",
            "join",
            "keys",
            "length",
            "lookup",
            "lower",
            "map",
            "max",
            "min",
            "replace",
            "secret",
            "split",
            "trim",
            "upper",
            "values",
        ];
        for name in &names {
            let doc = Document::new(name.to_string());
            let hover = provider.hover(&doc, Position::new(0, 1));
            assert!(
                hover.is_some(),
                "Built-in function '{}' should have hover info",
                name
            );
        }
    }

    #[test]
    fn test_module_argument_hover_with_description() {
        use carina_core::parser::{ArgumentParameter, TypeExpr};

        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let arg = ArgumentParameter {
            name: "vpc".to_string(),
            type_expr: TypeExpr::Ref(carina_core::parser::ResourceTypePath::new(
                "awscc", "ec2.vpc",
            )),
            default: None,
            description: Some("The VPC to deploy into".to_string()),
        };

        let hover = provider
            .build_module_argument_hover(&arg, "web_tier")
            .expect("Should produce hover");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("web_tier.vpc"),
            "Should show module.arg name, got:\n{}",
            content
        );
        assert!(
            content.contains("The VPC to deploy into"),
            "Should show description, got:\n{}",
            content
        );
        assert!(
            content.contains("awscc.ec2.vpc"),
            "Should show type, got:\n{}",
            content
        );
        assert!(
            content.contains("Required"),
            "Should show required, got:\n{}",
            content
        );
    }

    #[test]
    fn test_module_argument_hover_with_default() {
        use carina_core::parser::{ArgumentParameter, TypeExpr};

        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let arg = ArgumentParameter {
            name: "port".to_string(),
            type_expr: TypeExpr::Int,
            default: Some(Value::Int(8080)),
            description: Some("Web server port".to_string()),
        };

        let hover = provider
            .build_module_argument_hover(&arg, "web_tier")
            .expect("Should produce hover");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("Web server port"),
            "Should show description, got:\n{}",
            content
        );
        assert!(
            content.contains("8080"),
            "Should show default value, got:\n{}",
            content
        );
    }

    #[test]
    fn test_module_argument_hover_without_description() {
        use carina_core::parser::{ArgumentParameter, TypeExpr};

        let provider = HoverProvider::new(Arc::new(HashMap::new()), vec![]);
        let arg = ArgumentParameter {
            name: "env".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
        };

        let hover = provider
            .build_module_argument_hover(&arg, "web_tier")
            .expect("Should produce hover");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("web_tier.env"),
            "Should show module.arg name, got:\n{}",
            content
        );
        assert!(
            content.contains("string"),
            "Should show type, got:\n{}",
            content
        );
    }
}
