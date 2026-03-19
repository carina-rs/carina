use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::schema::{CompletionValue, ResourceSchema};

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
        for schema in self.schemas.values() {
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
