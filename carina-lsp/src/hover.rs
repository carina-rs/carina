use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use crate::enclosing_block::{EnclosingBlock, ResourceTypePatterns, StructPath, enclosing_block};
use carina_core::builtins;
use carina_core::parser::{ArgumentParameter, display_string_literal};
use carina_core::resource::{
    ConcreteValue, ParserPlaceholder, RelativeBindingName, Value as GenericValue,
};
use carina_core::schema::EffectiveSchemaNode;
#[cfg(test)]
use carina_core::schema::ResourceSchema;
use carina_core::schema::{CompletionValue, SchemaRegistry, ValidatedResourceSchema};

type Value = GenericValue<ParserPlaceholder, RelativeBindingName>;

/// Find the `source` path of a `let <alias> = use {...}` declaration
/// for `alias`. Tries the buffer's own parse first, then walks sibling
/// `.crn` files individually so a parse error in an unrelated sibling
/// (e.g. a half-typed `providers.crn`) does not block hover (#2443).
/// The current document's text is used in place of its on-disk copy
/// so unsaved edits are honored.
fn find_use_import_path(
    doc: &Document,
    base_path: &Path,
    current_file_name: Option<&str>,
    alias: &str,
) -> Option<String> {
    if let Some(parsed) = doc.parsed()
        && let Some(import) = parsed.uses.iter().find(|imp| imp.alias == alias)
    {
        return Some(import.path.clone());
    }
    let files = carina_core::config_loader::find_crn_files_in_dir(base_path).ok()?;
    let ctx = doc.provider_context();
    for file in files {
        let file_name = file.file_name().and_then(|n| n.to_str());
        let content = match (file_name, current_file_name) {
            (Some(name), Some(current)) if name == current => doc.text(),
            _ => match std::fs::read_to_string(&file) {
                Ok(text) => text,
                Err(_) => continue,
            },
        };
        let Ok(parsed) = carina_core::parser::parse_file(&content, &file, ctx) else {
            continue;
        };
        if let Some(import) = parsed.uses.iter().find(|imp| imp.alias == alias) {
            return Some(import.path.clone());
        }
    }
    None
}

/// Format a Value for hover display
fn format_value_for_hover(value: &Value) -> String {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => s
            .diagnostic_literal()
            .map(display_string_literal)
            .unwrap_or_else(|| "string".to_string()),
        Value::Concrete(ConcreteValue::Int(n)) => n.to_string(),
        Value::Concrete(ConcreteValue::Float(f)) => f.to_string(),
        Value::Concrete(ConcreteValue::Bool(b)) => b.to_string(),
        Value::Concrete(ConcreteValue::Duration(d)) => carina_core::value::render_duration(*d),
        Value::Concrete(ConcreteValue::List(_)) | Value::Concrete(ConcreteValue::StringList(_)) => {
            "[...]".to_string()
        }
        Value::Concrete(ConcreteValue::Map(_)) => "{...}".to_string(),
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
    schemas: Arc<SchemaRegistry>,
    region_completions: Vec<CompletionValue>,
    resource_type_patterns: ResourceTypePatterns,
}

impl HoverProvider {
    pub fn new(schemas: Arc<SchemaRegistry>, region_completions: Vec<CompletionValue>) -> Self {
        let resource_type_patterns = ResourceTypePatterns::from_schemas(&schemas);
        Self {
            schemas,
            region_completions,
            resource_type_patterns,
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
        self.hover_with_context(doc, position, base_path, None)
    }

    /// Like [`Self::hover_with_base_path`] but also accepts the open
    /// document's filename. Required for hovers that rely on a directory-
    /// merged parse (`module_call_hover`, `module_argument_hover`) to
    /// honor the in-memory buffer instead of the on-disk copy.
    pub fn hover_with_context(
        &self,
        doc: &Document,
        position: Position,
        base_path: Option<&Path>,
        current_file_name: Option<&str>,
    ) -> Option<Hover> {
        let word = doc.word_at(position)?;
        let text = doc.text();
        let enclosing_block = enclosing_block(&text, position, &self.resource_type_patterns);

        // Check for resource type hover
        if let Some(hover) = self.resource_type_hover(&word) {
            return Some(hover);
        }

        // Check for module call name hover (e.g., hovering on "github" in "github {")
        if let Some(base) = base_path
            && let Some(hover) = self.module_call_hover(doc, &word, base, current_file_name)
        {
            return Some(hover);
        }

        match &enclosing_block {
            EnclosingBlock::Resource {
                resource_type,
                struct_path,
                binding: _binding,
            } => match struct_path {
                StructPath::Known(struct_path) => {
                    if let Some(hover) = self.attribute_hover(&word, resource_type, struct_path) {
                        return Some(hover);
                    }
                }
                StructPath::Unresolvable => {}
            },
            EnclosingBlock::ModuleCall {
                module_name,
                arg_path,
            } => {
                if arg_path.is_empty()
                    && let Some(base) = base_path
                    && let Some(hover) =
                        self.module_argument_hover(&word, module_name, base, doc, current_file_name)
                {
                    return Some(hover);
                }
            }
            EnclosingBlock::Other => {}
        }

        // Check for built-in function hover, but skip when the word is a type
        // constructor (`map`, `list`) appearing in a type annotation position.
        if !is_in_type_annotation_position(doc, position, &word)
            && let Some(hover) = self.builtin_function_hover(&word)
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

    /// Show hover info for a module call argument, including its description if available.
    /// Hover on a module call name (e.g., "github" in "github {").
    /// Shows the module source path and its arguments/attributes summary.
    fn module_call_hover(
        &self,
        doc: &Document,
        word: &str,
        base_path: &Path,
        current_file_name: Option<&str>,
    ) -> Option<Hover> {
        let import_path = find_use_import_path(doc, base_path, current_file_name, word)?;

        // Load the module to get its definition
        let module_path = base_path.join(&import_path);
        let module_parsed = carina_core::module_resolver::load_module(&module_path)?;

        let mut content = format!("## {}\n\n`{}`\n", word, import_path);

        // Show arguments
        if !module_parsed.arguments.is_empty() {
            content.push_str("\n### Arguments\n\n");
            for arg in &module_parsed.arguments {
                let required = if arg.default.is_none() {
                    " **(required)**"
                } else {
                    ""
                };
                let desc = arg
                    .description
                    .as_deref()
                    .map(|d| format!(" — {}", d))
                    .unwrap_or_default();
                content.push_str(&format!(
                    "- `{}`: `{}`{}{}\n",
                    arg.name, arg.static_type, desc, required
                ));
            }
        }

        // Show attributes (outputs)
        if !module_parsed.attribute_params.is_empty() {
            content.push_str("\n### Attributes\n\n");
            for attr in &module_parsed.attribute_params {
                let type_str = attr
                    .static_type
                    .as_ref()
                    .map(|type_expr| format!("`{type_expr}`"))
                    .unwrap_or_default();
                content.push_str(&format!("- `{}`: {}\n", attr.name, type_str));
            }
        }

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
    }

    fn module_argument_hover(
        &self,
        word: &str,
        module_name: &str,
        base_path: &Path,
        doc: &Document,
        current_file_name: Option<&str>,
    ) -> Option<Hover> {
        let import_path = find_use_import_path(doc, base_path, current_file_name, module_name)?;

        // Load the module to get argument definitions
        let module_path = base_path.join(&import_path);
        let module_parsed = carina_core::module_resolver::load_module(&module_path)?;

        // Find the argument matching the word
        let arg = module_parsed.arguments.iter().find(|a| a.name == word)?;

        // Build hover content
        self.build_module_argument_hover(arg, module_name)
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

        let type_expr = &arg.static_type;
        content.push_str(&format!("- **Type**: {type_expr}\n"));

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

    fn resource_type_hover(&self, word: &str) -> Option<Hover> {
        // Check against all schema keys (provider.resource_type)
        for (provider, resource_type, _kind, schema) in self.schemas.iter() {
            let key = if provider.is_empty() {
                resource_type.to_string()
            } else {
                format!("{}.{}", provider, resource_type)
            };
            if word == key || word.contains(key.as_str()) {
                // Avoid matching substrings like "vpc_id" for "vpc"
                if word.contains(&format!("{}_", key)) || word.contains(&format!("_{}", key)) {
                    continue;
                }
                return self.schema_hover(&key, schema);
            }
        }
        None
    }

    fn schema_hover(&self, resource_name: &str, schema: &ValidatedResourceSchema) -> Option<Hover> {
        let description = convert_markdown_links_to_plain_text(
            schema
                .as_schema()
                .description()
                .unwrap_or("No description available"),
        );

        let mut content = format!("## {}\n\n{}\n", resource_name, description);

        // Split attributes into arguments (writable) and attributes (read-only)
        let mut arguments: Vec<_> = schema
            .effective_attributes()
            .filter(|a| !a.is_read_only())
            .collect();
        arguments.sort_by_key(|a| a.name());

        let mut read_only_attrs: Vec<_> = schema
            .effective_attributes()
            .filter(|a| a.is_read_only())
            .collect();
        read_only_attrs.sort_by_key(|a| a.name());

        if !arguments.is_empty() {
            content.push_str("\n### Arguments\n\n");
            for attr in &arguments {
                let required = if attr.is_required() {
                    " **(required)**"
                } else {
                    ""
                };
                let create_only = if attr.is_create_only() {
                    " _(create-only)_"
                } else {
                    ""
                };
                let desc = convert_markdown_links_to_plain_text(attr.description().unwrap_or(""));
                content.push_str(&format!(
                    "- `{}`: {}{}{}\n",
                    attr.name(),
                    desc,
                    required,
                    create_only
                ));
            }
        }

        if !read_only_attrs.is_empty() {
            content.push_str("\n### Attributes\n\n");
            for attr in &read_only_attrs {
                let desc = convert_markdown_links_to_plain_text(attr.description().unwrap_or(""));
                content.push_str(&format!("- `{}`: {}\n", attr.name(), desc));
            }
        }

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        })
    }

    fn attribute_hover(
        &self,
        word: &str,
        resource_type: &str,
        struct_path: &[String],
    ) -> Option<Hover> {
        // When we know the enclosing resource, only its schema is authoritative.
        // Falling back to a global scan would return a lookalike attribute from
        // an unrelated schema (nondeterministic via HashMap iteration order) —
        // see #1988.
        let key = resource_type;
        // Try splitting on the first dot first (`provider.resource_type`); if that
        // doesn't resolve, fall back to treating the whole key as the resource type
        // under the empty provider — some test fixtures register schemas that way.
        let lookup = |provider: &str, resource_type: &str| {
            self.schemas
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Resource,
                )
                .or_else(|| {
                    self.schemas.get(
                        provider,
                        resource_type,
                        carina_core::schema::SchemaKind::DataSource,
                    )
                })
        };
        let schema = if let Some((provider, rest)) = key.split_once('.') {
            lookup(provider, rest).or_else(|| lookup("", key))
        } else {
            lookup("", key)
        }?;
        let mut path = struct_path.to_vec();
        path.push(word.to_string());
        let attribute = schema.effective_node_at_authored_path(&path)?;
        Some(self.build_attribute_hover(&attribute))
    }

    fn build_attribute_hover(&self, attr: &EffectiveSchemaNode<'_>) -> Hover {
        let description =
            convert_markdown_links_to_plain_text(attr.description().unwrap_or("No description"));
        let required = if attr.is_required() {
            "Required"
        } else {
            "Optional"
        };
        let type_name = format!("{}", attr.field_type());

        let content = format!(
            "## {}\n\n{}\n\n- **Type**: {}\n- **Required**: {}",
            attr.name(),
            description,
            type_name,
            required
        );

        Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: content,
            }),
            range: None,
        }
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
                "## let\n\nDefines a named resource or variable binding.\n\n```carina\nlet my_bucket = aws.s3.Bucket {\n    name = \"my-bucket\"\n    region = aws.Region.ap_northeast_1\n}\n```",
            ),
            "attributes" => Some(
                "## attributes\n\nDefines module attribute values that can be referenced by the caller.\n\n```carina\nattributes {\n    bucket_name: String = my_bucket.name\n}\n```",
            ),
            "arguments" => Some(
                "## arguments\n\nDefines module argument parameters that must be provided by the caller.\n\n```carina\narguments {\n    env: String\n    region: String\n}\n```",
            ),
            "use" => Some(
                "## use\n\nLoads a Carina module from the `source` directory.\n\n```carina\nlet network = use { source = \"./modules/network\" }\n```",
            ),
            "import" => Some(
                "## import\n\nAdopts an existing cloud resource into Carina's state.\n\n```carina\nimport {\n    to = awscc.ec2.Vpc 'imported_vpc'\n    id = 'vpc-0123456789abcdef0'\n}\n```",
            ),
            "backend" => Some(
                "## backend\n\nConfigures the state backend for storing resource state.\n\n```carina\nbackend s3 {\n    bucket = \"my-carina-state\"\n    key    = \"prod/carina.crnstate\"\n    region = aws.Region.ap_northeast_1\n}\n```",
            ),
            "read" => Some(
                "## read\n\nReads an existing resource as a data source without managing it.\n\n```carina\nlet my_vpc = read aws.ec2.Vpc {\n    name = \"existing-vpc\"\n}\n```",
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

/// Determine whether a word at `position` is in a type annotation position.
///
/// Returns true for `map` / `list` appearing immediately after `:` on the
/// same line, e.g., `accounts: map(...)` or `items: list(...)`.
/// In these cases the word is a type constructor, not a function call,
/// and builtin-function hover should be suppressed.
fn is_in_type_annotation_position(doc: &Document, position: Position, word: &str) -> bool {
    if word != "map" && word != "list" {
        return false;
    }
    let line_str = match doc.line_at(position.line) {
        Some(l) => l,
        None => return false,
    };
    let col = position.character as usize;
    let chars: Vec<char> = line_str.chars().collect();
    if col > chars.len() {
        return false;
    }
    // Walk back from the cursor to find the start of the word
    let mut start = col;
    while start > 0 {
        let prev = chars[start - 1];
        if prev.is_alphanumeric() || prev == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    // Check what's before the word (trimmed of whitespace)
    let before: String = chars[..start].iter().collect();
    before.trim_end().ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::ProviderContext;
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    #[test]
    fn format_value_for_hover_uses_canonical_string_literals() {
        let value = Value::Concrete(ConcreteValue::String(r#"say "hi""#.into()));
        assert_eq!(format_value_for_hover(&value), r#"'say "hi"'"#);

        let value = Value::Concrete(ConcreteValue::String("${value}".into()));
        assert_eq!(format_value_for_hover(&value), r#""\${value}""#);
    }

    #[test]
    fn format_value_for_hover_renders_duration_as_canonical_form() {
        // Regression: Round 1 added the Value::Concrete(ConcreteValue::Duration) arm so editor
        // hover doesn't show the {:?} fall-through
        // `Duration { secs: 60, nanos: 0 }`. Pin the canonical surface
        // form here.
        use carina_core::resource::{ConcreteValue, Value};
        let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60)));
        assert_eq!(format_value_for_hover(&v), "1min");
        let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
            4500,
        )));
        assert_eq!(format_value_for_hover(&v), "75min");
    }

    fn create_hover_provider_with_description(
        resource_type: &str,
        description: &str,
    ) -> HoverProvider {
        let mut schemas = SchemaRegistry::new();
        let schema = ResourceSchema::new(resource_type).with_description(description);
        schemas.insert("", schema.validate().expect("schema fixture must be valid"));
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn create_hover_provider_with_attr_description(
        resource_type: &str,
        attr_name: &str,
        attr_description: &str,
    ) -> HoverProvider {
        let mut schemas = SchemaRegistry::new();
        let schema = ResourceSchema::new(resource_type).attribute(
            AttributeSchema::optional(attr_name, AttributeType::string())
                .with_description(attr_description)
                .build(),
        );
        schemas.insert("", schema.validate().expect("schema fixture must be valid"));
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn hover_position(text: &str, needle: &str, occurrence: usize) -> Position {
        let byte_offset = text
            .match_indices(needle)
            .nth(occurrence)
            .map(|(offset, _)| offset)
            .unwrap_or_else(|| panic!("missing occurrence {occurrence} of {needle:?}"));
        let prefix = &text[..byte_offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let character = prefix
            .rsplit_once('\n')
            .map_or(prefix, |(_, current_line)| current_line)
            .chars()
            .count() as u32;
        Position::new(line, character + 1)
    }

    fn hover_markdown(
        provider: &HoverProvider,
        doc: &Document,
        position: Position,
        base_path: Option<&Path>,
    ) -> Option<String> {
        provider
            .hover_with_base_path(doc, position, base_path)
            .map(|hover| match hover.contents {
                HoverContents::Markup(markup) => markup.value,
                HoverContents::Scalar(_) | HoverContents::Array(_) => {
                    panic!("expected markdown hover content")
                }
            })
    }

    fn nested_attribute_hover_provider() -> HoverProvider {
        let alias_target = AttributeType::struct_(
            "AliasTarget",
            vec![
                StructField::required("hosted_zone_id", AttributeType::string())
                    .with_description("NESTED alias zone")
                    .build(),
                StructField::optional("dns_name", AttributeType::string())
                    .with_description("Alias DNS name")
                    .build(),
                StructField::optional("evaluate_target_health", AttributeType::bool())
                    .with_description("Whether to evaluate target health")
                    .build(),
            ],
        );
        let record_set = ResourceSchema::new("route53.RecordSet")
            .attribute(
                AttributeSchema::optional("hosted_zone_id", AttributeType::string())
                    .with_description("TOP-LEVEL zone")
                    .build(),
            )
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("alias_target", alias_target)
                    .with_description("Alias target configuration")
                    .build(),
            );
        let bucket = ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("hosted_zone_id", AttributeType::string()).build(),
            );

        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            record_set.validate().expect("schema fixture must be valid"),
        );
        schemas.insert(
            "aws",
            bucket.validate().expect("schema fixture must be valid"),
        );
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn resource_header_hover_provider() -> HoverProvider {
        let security_group = ResourceSchema::new("ec2.SecurityGroup").attribute(
            AttributeSchema::optional("description", AttributeType::string())
                .with_description("REAL RESOURCE DESCRIPTION")
                .build(),
        );
        let vpc = ResourceSchema::new("ec2.Vpc").attribute(
            AttributeSchema::optional("description", AttributeType::string())
                .with_description("REAL RESOURCE DESCRIPTION")
                .build(),
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "aws",
            security_group
                .validate()
                .expect("schema fixture must be valid"),
        );
        schemas.insert("aws", vpc.validate().expect("schema fixture must be valid"));
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn unnamed_expression_hover_provider() -> HoverProvider {
        let x = AttributeType::struct_(
            "X",
            vec![
                StructField::optional("field", AttributeType::string())
                    .with_description("NESTED field")
                    .build(),
            ],
        );
        let rule = AttributeType::struct_(
            "Rule",
            vec![
                StructField::optional("name", AttributeType::string())
                    .with_description("NESTED name")
                    .build(),
            ],
        );
        let schema = ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("x", x).build())
            .attribute(AttributeSchema::optional("rules", AttributeType::list(rule)).build())
            .attribute(
                AttributeSchema::optional("field", AttributeType::string())
                    .with_description("TOP-LEVEL field")
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .with_description("TOP-LEVEL name")
                    .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn assert_module_argument_hover(text: &str) {
        let temp_dir = tempfile::tempdir().unwrap();
        let module_dir = temp_dir.path().join("modules").join("example");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            module_dir.join("main.crn"),
            "arguments {\n  arg: String\n}\n",
        )
        .unwrap();
        let consumer_dir = temp_dir.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        std::fs::write(
            consumer_dir.join("imports.crn"),
            "let mymod = use { source = '../modules/example' }\n",
        )
        .unwrap();
        std::fs::write(consumer_dir.join("main.crn"), text).unwrap();

        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));
        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "arg", 0),
            Some(&consumer_dir),
        )
        .unwrap_or_else(|| panic!("module argument should have hover in:\n{text}"));
        assert!(content.contains("mymod.arg"), "{text}\n{content}");
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
        let desc = "Specifies a composition private cloud (VPC). To add an IPv6 CIDR block to the VPC, see [AWS::EC2::VPCCidrBlock](https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-ec2-vpccidrblock.html).";

        let provider = create_hover_provider_with_description("ec2.Vpc", desc);
        let schema = provider
            .schemas
            .get("", "ec2.Vpc", carina_core::schema::SchemaKind::Resource)
            .unwrap();
        let hover = provider.schema_hover("ec2.Vpc", schema.schema()).unwrap();

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

        // The attribute must be hovered inside its enclosing resource block so
        // the resolver has the schema context to anchor on (see #1988).
        let doc = Document::new(
            "ec2.nat_gateway {\n  secondary_allocation_ids\n}\n".to_string(),
            Arc::new(ProviderContext::default()),
        );
        // Hover on the attribute name (line 1, inside the identifier).
        let hover = provider
            .hover(&doc, Position::new(1, 5))
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
    fn test_keyword_hover_use() {
        assert!(HoverProvider::keyword_description("use").is_some());
        let desc = HoverProvider::keyword_description("use").unwrap();
        assert!(desc.contains("use"));
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
        let mut schemas = SchemaRegistry::new();

        let igw_schema = ResourceSchema::new("ec2.internet_gateway").attribute(
            AttributeSchema::optional("internet_gateway_id", AttributeType::string())
                .with_description("The ID of the internet gateway (from internet_gateway schema).")
                .build(),
        );
        schemas.insert(
            "awscc",
            igw_schema.validate().expect("schema fixture must be valid"),
        );

        let attachment_schema = ResourceSchema::new("ec2.vpc_gateway_attachment").attribute(
            AttributeSchema::optional("internet_gateway_id", AttributeType::string())
                .with_description(
                    "The ID of the internet gateway attached to the VPC (from vpc_gateway_attachment schema).",
                ).build(),
        );
        schemas.insert(
            "awscc",
            attachment_schema
                .validate()
                .expect("schema fixture must be valid"),
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
            Arc::new(ProviderContext::default()),
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
    fn test_attribute_hover_unknown_in_enclosing_resource_returns_none() {
        // Regression for #1988: when a word is NOT an attribute of the
        // enclosing resource, hover must return None — not fall back to a
        // lookalike attribute from an unrelated resource schema.
        let mut schemas = SchemaRegistry::new();

        // `account_id` lives on organizations.account only.
        let account_schema = ResourceSchema::new("organizations.account").attribute(
            AttributeSchema::optional("account_id", AttributeType::string())
                .with_description("The unique identifier (ID) of the new account.")
                .build(),
        );
        schemas.insert(
            "awscc",
            account_schema
                .validate()
                .expect("schema fixture must be valid"),
        );

        // `awscc.sso.Assignment` intentionally has NO `account_id`.
        let assignment_schema = ResourceSchema::new("sso.Assignment").attribute(
            AttributeSchema::optional("target_id", AttributeType::string())
                .with_description("Target id.")
                .build(),
        );
        schemas.insert(
            "awscc",
            assignment_schema
                .validate()
                .expect("schema fixture must be valid"),
        );

        let provider = HoverProvider::new(Arc::new(schemas), vec![]);

        // Simulates the repro: a for-loop binding named `account_id` used as
        // an attribute value inside an sso.assignment block.
        let doc = Document::new(
            r#"for _, account_id in orgs.accounts {
    awscc.sso.Assignment {
        target_id = account_id
    }
}
"#
            .to_string(),
            Arc::new(ProviderContext::default()),
        );

        // Hover on the `account_id` token in `target_id = account_id`
        // (line 2, char 21 puts the cursor inside "account_id").
        let hover = provider.hover(&doc, Position::new(2, 25));

        assert!(
            hover.is_none(),
            "Hover on a non-attribute identifier must be None; \
             must not fall back to an unrelated schema. Got: {:?}",
            hover.map(|h| match h.contents {
                HoverContents::Markup(m) => m.value,
                _ => String::new(),
            })
        );
    }

    #[test]
    fn nested_struct_attribute_hover_in_multifile_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let main_text = r#"let bucket = aws.s3.Bucket {
  name = 'website-bucket'
}

aws.route53.RecordSet {
  hosted_zone_id = 'Z0123456789ABC'
  name = 'example.com'
  alias_target = {
    hosted_zone_id         = bucket.hosted_zone_id
    dns_name               = 's3-website.example.com'
    evaluate_target_health = false
  }
}
"#;
        std::fs::write(temp_dir.path().join("main.crn"), main_text).unwrap();
        std::fs::write(
            temp_dir.path().join("providers.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join("exports.crn"),
            "exports {\n  bucket_name: String = bucket.name\n}\n",
        )
        .unwrap();

        let provider = nested_attribute_hover_provider();
        let doc = Document::new(main_text.to_string(), Arc::new(ProviderContext::default()));

        let nested_zone = hover_markdown(
            &provider,
            &doc,
            hover_position(main_text, "hosted_zone_id", 1),
            Some(temp_dir.path()),
        )
        .expect("nested hosted_zone_id should have hover");
        assert!(nested_zone.contains("NESTED alias zone"), "{nested_zone}");
        assert!(!nested_zone.contains("TOP-LEVEL zone"), "{nested_zone}");
        assert!(
            nested_zone.contains("- **Required**: Required"),
            "{nested_zone}"
        );

        let dns_name = hover_markdown(
            &provider,
            &doc,
            hover_position(main_text, "dns_name", 0),
            Some(temp_dir.path()),
        )
        .expect("nested dns_name should have hover");
        assert!(dns_name.contains("Alias DNS name"), "{dns_name}");

        let evaluate_target_health = hover_markdown(
            &provider,
            &doc,
            hover_position(main_text, "evaluate_target_health", 0),
            Some(temp_dir.path()),
        )
        .expect("nested evaluate_target_health should have hover");
        assert!(
            evaluate_target_health.contains("Whether to evaluate target health"),
            "{evaluate_target_health}"
        );

        let alias_target = hover_markdown(
            &provider,
            &doc,
            hover_position(main_text, "alias_target", 0),
            Some(temp_dir.path()),
        )
        .expect("alias_target on a same-line struct opener should have hover");
        assert!(
            alias_target.contains("Alias target configuration"),
            "{alias_target}"
        );

        let top_level_zone = hover_markdown(
            &provider,
            &doc,
            hover_position(main_text, "hosted_zone_id", 0),
            Some(temp_dir.path()),
        )
        .expect("top-level hosted_zone_id should retain hover");
        assert!(
            top_level_zone.contains("TOP-LEVEL zone"),
            "{top_level_zone}"
        );
    }

    #[test]
    fn nested_struct_hover_supports_block_syntax() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  alias_target {
    hosted_zone_id = 'Z0123456789ABC'
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 0),
            None,
        )
        .expect("field inside alias_target block should have hover");
        assert!(content.contains("NESTED alias zone"), "{content}");
    }

    #[test]
    fn nested_struct_hover_supports_list_and_repeated_block_syntax() {
        let rule_type = AttributeType::struct_(
            "Rule",
            vec![
                StructField::optional("name", AttributeType::string())
                    .with_description("NESTED rule name")
                    .build(),
            ],
        );
        let schema = ResourceSchema::new("Policy")
            .attribute(
                AttributeSchema::optional("name", AttributeType::string())
                    .with_description("TOP-LEVEL policy name")
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("rules", AttributeType::list(rule_type))
                    .with_block_name("rule")
                    .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        let provider = HoverProvider::new(Arc::new(schemas), vec![]);

        let list_text = r#"test.Policy {
  rules = [
    {
      name = 'first'
    }
  ]
}
"#;
        let list_doc = Document::new(list_text.to_string(), Arc::new(ProviderContext::default()));
        let list_content = hover_markdown(
            &provider,
            &list_doc,
            hover_position(list_text, "name", 0),
            None,
        )
        .expect("field inside a list-of-struct literal should have hover");
        assert!(list_content.contains("NESTED rule name"), "{list_content}");
        assert!(
            !list_content.contains("TOP-LEVEL policy name"),
            "{list_content}"
        );

        let block_text = r#"test.Policy {
  rule {
    name = 'first'
  }
}
"#;
        let block_doc = Document::new(block_text.to_string(), Arc::new(ProviderContext::default()));
        let block_content = hover_markdown(
            &provider,
            &block_doc,
            hover_position(block_text, "name", 0),
            None,
        )
        .expect("field inside repeated block syntax should have hover");
        assert!(
            block_content.contains("NESTED rule name"),
            "{block_content}"
        );
    }

    #[test]
    fn nested_struct_hover_resolves_ref_fields_two_levels_deep() {
        let outer = AttributeType::struct_(
            "Outer",
            vec![StructField::optional("inner", AttributeType::ref_("Inner".to_string())).build()],
        );
        let inner = AttributeType::struct_(
            "Inner",
            vec![
                StructField::required("leaf", AttributeType::string())
                    .with_description("NESTED field behind refs")
                    .build(),
            ],
        );
        let schema = ResourceSchema::new("RefResource")
            .attribute(
                AttributeSchema::optional("config", AttributeType::ref_("Outer".to_string()))
                    .build(),
            )
            .with_def("Outer", outer)
            .with_def("Inner", inner);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        let provider = HoverProvider::new(Arc::new(schemas), vec![]);
        let text = r#"test.RefResource {
  config = {
    inner = {
      leaf = 'value'
    }
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(&provider, &doc, hover_position(text, "leaf", 0), None)
            .expect("field reached through two Ref-backed struct levels should have hover");
        assert!(content.contains("NESTED field behind refs"), "{content}");
        assert!(content.contains("- **Required**: Required"), "{content}");
    }

    #[test]
    fn nested_struct_value_identifier_has_no_attribute_hover() {
        let alias_target = AttributeType::struct_(
            "AliasTarget",
            vec![StructField::optional("dns_name", AttributeType::string()).build()],
        );
        let schema = ResourceSchema::new("RecordSet")
            .attribute(AttributeSchema::optional("alias_target", alias_target).build())
            .attribute(
                AttributeSchema::optional("foo_binding", AttributeType::string())
                    .with_description("TOP-LEVEL lookalike")
                    .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        let provider = HoverProvider::new(Arc::new(schemas), vec![]);
        let text = r#"test.RecordSet {
  alias_target = {
    dns_name = foo_binding
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "foo_binding", 0));
        assert!(
            hover.is_none(),
            "nested value identifier must not fall back to a top-level attribute: {hover:?}"
        );
    }

    #[test]
    fn same_line_resource_and_struct_openers_keep_nested_path() {
        let provider = nested_attribute_hover_provider();
        for text in [
            r#"aws.route53.RecordSet { alias_target = {
  hosted_zone_id = 'Z0123456789ABC'
}
}
"#,
            "aws.route53.RecordSet { alias_target = { hosted_zone_id = 'Z0123456789ABC' } }",
        ] {
            let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));
            let content = hover_markdown(
                &provider,
                &doc,
                hover_position(text, "hosted_zone_id", 0),
                None,
            )
            .unwrap_or_else(|| {
                panic!("nested field after same-line openers has no hover:\n{text}")
            });
            assert!(content.contains("NESTED alias zone"), "{text}\n{content}");
            assert!(!content.contains("TOP-LEVEL zone"), "{text}\n{content}");
        }
    }

    #[test]
    fn comments_and_strings_do_not_change_resource_structure() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  name = '[{'
  # an unmatched bracket is not structure: [
  hosted_zone_id = 'Z0123456789ABC'
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 0),
            None,
        )
        .expect("comment and string punctuation must not hide the resource attribute");
        assert!(content.contains("TOP-LEVEL zone"), "{content}");
    }

    #[test]
    fn crlf_lines_keep_lexer_and_walker_positions_aligned() {
        let provider = nested_attribute_hover_provider();
        let text = "aws.route53.RecordSet {\r\n  hosted_zone_id = 'Z0123456789ABC'\r\n}\r\n";
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 0),
            None,
        )
        .expect("CRLF source should retain resource attribute hover");
        assert!(content.contains("TOP-LEVEL zone"), "{content}");
    }

    #[test]
    fn heredoc_body_does_not_change_resource_structure() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  name = <<EOT
if [ -f x ]; then {
EOT
  hosted_zone_id = 'Z0123456789ABC'
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 0),
            None,
        )
        .expect("heredoc punctuation must not hide the resource attribute");
        assert!(content.contains("TOP-LEVEL zone"), "{content}");
    }

    #[test]
    fn interpolation_code_does_not_change_nested_structure() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  alias_target = {
    dns_name = "${replace('x', 'x', '{')}"
    hosted_zone_id = 'Z0123456789ABC'
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 0),
            None,
        )
        .expect("interpolation punctuation must not hide the nested field");
        assert!(content.contains("NESTED alias zone"), "{content}");
        assert!(!content.contains("TOP-LEVEL zone"), "{content}");
    }

    #[test]
    fn union_path_finds_field_in_later_struct_member() {
        let union = AttributeType::union(vec![
            AttributeType::struct_(
                "First",
                vec![StructField::optional("first_only", AttributeType::string()).build()],
            ),
            AttributeType::struct_(
                "Second",
                vec![
                    StructField::optional("second_only", AttributeType::string())
                        .with_description("FIELD FROM SECOND UNION MEMBER")
                        .build(),
                ],
            ),
        ]);
        let schema = ResourceSchema::new("UnionResource")
            .attribute(AttributeSchema::optional("config", union).build());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        let provider = HoverProvider::new(Arc::new(schemas), vec![]);
        let text = r#"test.UnionResource {
  config = {
    second_only = 'value'
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "second_only", 0),
            None,
        )
        .expect("field from the later union struct should have hover");
        assert!(
            content.contains("FIELD FROM SECOND UNION MEMBER"),
            "{content}"
        );
    }

    #[test]
    fn jsonencode_object_inside_resource_is_not_a_module_call() {
        let temp_dir = tempfile::tempdir().unwrap();
        let module_dir = temp_dir.path().join("modules").join("policy");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            module_dir.join("main.crn"),
            "arguments {\n  Version: Int\n}\n",
        )
        .unwrap();
        let consumer_dir = temp_dir.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        std::fs::write(
            consumer_dir.join("imports.crn"),
            "let policy = use { source = '../modules/policy' }\n",
        )
        .unwrap();
        let text = r#"aws.route53.RecordSet {
  policy = jsonencode({
    Version = 1
  })
}
"#;
        std::fs::write(consumer_dir.join("main.crn"), text).unwrap();
        let provider = nested_attribute_hover_provider();
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover_with_base_path(
            &doc,
            hover_position(text, "Version", 0),
            Some(&consumer_dir),
        );
        assert!(
            hover.is_none(),
            "jsonencode object field must not route to a module argument hover: {hover:?}"
        );
    }

    #[test]
    fn assignment_expressions_and_control_blocks_keep_attribute_context() {
        let nested = AttributeType::struct_(
            "Nested",
            vec![
                StructField::optional("field", AttributeType::string())
                    .with_description("NESTED expression field")
                    .build(),
            ],
        );
        let schema = ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("outer", nested.clone()).build())
            .attribute(AttributeSchema::optional("x", nested).build())
            .attribute(
                AttributeSchema::optional("field", AttributeType::string())
                    .with_description("TOP-LEVEL field")
                    .build(),
            );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        let provider = HoverProvider::new(Arc::new(schemas), vec![]);

        for text in [
            "test.Review {\n  outer = if true {\n    field = 'value'\n  }\n}\n",
            "test.Review {\n  outer = for value in values {\n    field = value\n  }\n}\n",
            "test.Review {\n  x = merge(y, {\n    field = 'value'\n  })\n}\n",
        ] {
            let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));
            let content = hover_markdown(&provider, &doc, hover_position(text, "field", 0), None)
                .expect("field in an assignment expression should have nested hover");
            assert!(content.contains("NESTED expression field"), "{content}");
            assert!(!content.contains("TOP-LEVEL field"), "{content}");
        }

        let control_text = "test.Review {\n  if true {\n    field = 'value'\n  }\n}\n";
        let control_doc = Document::new(
            control_text.to_string(),
            Arc::new(ProviderContext::default()),
        );
        let content = hover_markdown(
            &provider,
            &control_doc,
            hover_position(control_text, "field", 0),
            None,
        )
        .expect("a control-flow opener should be transparent to resource hover");
        assert!(content.contains("TOP-LEVEL field"), "{content}");
    }

    #[test]
    fn module_argument_hover_works_inside_control_flow_bodies() {
        for text in [
            "for a in xs {\n  mymod {\n    arg = a\n  }\n}\n",
            "if enabled {\n  mymod {\n    arg = 'value'\n  }\n}\n",
            "for k, v in m {\n  mymod {\n    arg = v\n  }\n}\n",
            "for a in ['x', 'y'] {\n  mymod {\n    arg = a\n  }\n}\n",
            "let ms = for a in xs {\n  mymod {\n    arg = a\n  }\n}\n",
            "let m = if enabled {\n  mymod {\n    arg = 'value'\n  }\n}\n",
            "let m = if c {\n  'left'\n} else {\n  mymod {\n    arg = 'right'\n  }\n}\n",
        ] {
            assert_module_argument_hover(text);
        }
    }

    #[test]
    fn arguments_type_is_not_a_resource_header() {
        let provider = resource_header_hover_provider();
        let text = r#"arguments {
  sg: aws.ec2.SecurityGroup {
    description = 'x'
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "description", 0));
        assert!(
            hover.is_none(),
            "a type nested in arguments is not a resource header: {hover:?}"
        );
    }

    #[test]
    fn exports_type_is_not_a_resource_header() {
        let provider = resource_header_hover_provider();
        let text = r#"exports {
  x: map(aws.ec2.Vpc) = {
    description = 'x'
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "description", 0));
        assert!(
            hover.is_none(),
            "a type nested in exports is not a resource header: {hover:?}"
        );
    }

    #[test]
    fn valid_resource_headers_keep_attribute_hover() {
        let provider = resource_header_hover_provider();
        for text in [
            "aws.ec2.SecurityGroup {\n  description = 'direct'\n}\n",
            "let x = read aws.ec2.Vpc {\n  description = 'read'\n}\n",
            "for item in items {\n  aws.ec2.SecurityGroup {\n    description = item\n  }\n}\n",
        ] {
            let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));
            let content = hover_markdown(
                &provider,
                &doc,
                hover_position(text, "description", 0),
                None,
            )
            .unwrap_or_else(|| panic!("valid resource header lost attribute hover:\n{text}"));
            assert!(content.contains("REAL RESOURCE DESCRIPTION"), "{content}");
        }
    }

    #[test]
    fn unnamed_object_element_in_merge_has_no_attribute_hover() {
        let provider = unnamed_expression_hover_provider();
        let text = r#"test.Review {
  x = merge(
    {
      field = 'a'
    }
  )
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "field", 0));
        assert!(
            hover.is_none(),
            "an unnamed merge element has no schema path: {hover:?}"
        );
    }

    #[test]
    fn second_unnamed_object_element_in_merge_has_no_attribute_hover() {
        let provider = unnamed_expression_hover_provider();
        let text = r#"test.Review {
  x = merge({
    a = 1
  }, {
    field = 'a'
  })
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "field", 0));
        assert!(
            hover.is_none(),
            "a later unnamed merge element has no schema path: {hover:?}"
        );
    }

    #[test]
    fn unnamed_list_element_in_concat_has_no_attribute_hover() {
        let provider = unnamed_expression_hover_provider();
        let text = r#"test.Review {
  rules = concat(
    [
      {
        name = 'a'
      }
    ]
  )
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "name", 0));
        assert!(
            hover.is_none(),
            "an unnamed concat list element has no schema path: {hover:?}"
        );
    }

    #[test]
    fn named_module_call_argument_hover_uses_imported_schema() {
        assert_module_argument_hover("let m = mymod {\n  arg = 'value'\n}\n");
    }

    #[test]
    fn else_branch_keeps_nested_attribute_hover() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  alias_target = if true {
    {
      hosted_zone_id = 'a'
    }
  } else {
    {
      hosted_zone_id = 'b'
    }
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let content = hover_markdown(
            &provider,
            &doc,
            hover_position(text, "hosted_zone_id", 1),
            None,
        )
        .expect("the else branch field should retain nested hover");
        assert!(content.contains("NESTED alias zone"), "{content}");
        assert!(!content.contains("TOP-LEVEL zone"), "{content}");
    }

    #[test]
    fn else_if_chain_keeps_nested_attribute_hover() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  alias_target = if first {
    {
      hosted_zone_id = 'a'
    }
  } else if second {
    {
      hosted_zone_id = 'b'
    }
  } else {
    {
      hosted_zone_id = 'c'
    }
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        for occurrence in [1, 2] {
            let content = hover_markdown(
                &provider,
                &doc,
                hover_position(text, "hosted_zone_id", occurrence),
                None,
            )
            .expect("every else-if branch field should retain nested hover");
            assert!(content.contains("NESTED alias zone"), "{content}");
            assert!(!content.contains("TOP-LEVEL zone"), "{content}");
        }
    }

    #[test]
    fn quoted_map_key_has_no_attribute_hover() {
        let provider = nested_attribute_hover_provider();
        let text = r#"aws.route53.RecordSet {
  alias_target = {
    "k" = {
      hosted_zone_id = 'a'
    }
  }
}
"#;
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));

        let hover = provider.hover(&doc, hover_position(text, "hosted_zone_id", 0));
        assert!(
            hover.is_none(),
            "quoted map-key fields have no schema: {hover:?}"
        );
    }

    #[test]
    fn test_builtin_function_hover_join() {
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let doc = Document::new("join".to_string(), Arc::new(ProviderContext::default()));
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
            content.contains("join(separator: String, list: list) -> String"),
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
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let doc = Document::new(
            "cidr_subnet".to_string(),
            Arc::new(ProviderContext::default()),
        );
        let hover = provider
            .hover(&doc, Position::new(0, 3))
            .expect("Should find hover for 'cidr_subnet'");

        let content = match &hover.contents {
            HoverContents::Markup(m) => &m.value,
            _ => panic!("Expected markup content"),
        };

        assert!(
            content.contains("cidr_subnet(prefix: String, newbits: Int, netnum: Int) -> String"),
            "Should show cidr_subnet signature. Got:\n{}",
            content
        );
    }

    #[test]
    fn test_builtin_function_hover_unknown_returns_none() {
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let doc = Document::new(
            "not_a_function".to_string(),
            Arc::new(ProviderContext::default()),
        );
        let hover = provider.hover(&doc, Position::new(0, 3));
        assert!(hover.is_none(), "Unknown function should not show hover");
    }

    #[test]
    fn test_all_builtin_functions_have_hover() {
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let names = [
            "cidr_subnet",
            "concat",
            "decrypt",
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
            let doc = Document::new(name.to_string(), Arc::new(ProviderContext::default()));
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

        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let arg = ArgumentParameter {
            name: "vpc".to_string(),
            static_type: TypeExpr::Ref(carina_core::parser::ResourceTypePath::new(
                "awscc", "ec2.Vpc",
            )),
            default: None,
            description: Some("The VPC to deploy into".to_string()),
            validations: vec![],
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
            content.contains("awscc.ec2.Vpc"),
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

        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let arg = ArgumentParameter {
            name: "port".to_string(),
            static_type: TypeExpr::Int,
            default: Some(Value::Concrete(ConcreteValue::Int(8080))),
            description: Some("Web server port".to_string()),
            validations: vec![],
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

        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        let arg = ArgumentParameter {
            name: "env".to_string(),
            static_type: TypeExpr::String,
            default: None,
            description: None,
            validations: vec![],
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
            content.contains("String"),
            "Should show type, got:\n{}",
            content
        );
    }

    #[test]
    fn test_no_builtin_hover_for_map_in_type_annotation() {
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        // Line 1 (0-indexed): "  accounts: map(AwsAccountId) = {"
        // Position on "map" → should NOT show function hover
        let doc = Document::new(
            "exports {\n  accounts: map(AwsAccountId) = {}\n}".to_string(),
            Arc::new(ProviderContext::default()),
        );
        // Column 14 is inside "map" on line 1
        let hover = provider.hover(&doc, Position::new(1, 14));
        assert!(
            hover.is_none(),
            "map() in type annotation should not trigger builtin function hover, got: {:?}",
            hover
        );
    }

    #[test]
    fn test_builtin_hover_for_map_in_function_call() {
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        // Normal function call (no preceding ':') — should show hover
        let doc = Document::new(
            "let x = map(\".id\", items)".to_string(),
            Arc::new(ProviderContext::default()),
        );
        let hover = provider.hover(&doc, Position::new(0, 10));
        assert!(
            hover.is_some(),
            "map() in function call position should show builtin function hover"
        );
    }

    /// Build the multi-file fixture used by the #2443 sibling-use
    /// tests. Returns `(tempdir, consumer_dir, doc, provider)`. Layout:
    ///
    ///   <tmp>/modules/github-oidc/main.crn   — arguments { github_repo, role_name }
    ///   <tmp>/consumer/imports.crn           — let github = use { source = '../modules/github-oidc' }
    ///   <tmp>/consumer/main.crn              — github { github_repo = 'carina-rs/infra' }
    ///
    /// Hover positions used by the call sites:
    ///   line 0 col 2 → on the alias `github`
    ///   line 1 col 4 → on the argument name `github_repo`
    fn sibling_use_fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Document,
        HoverProvider,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let module_dir = tmp.path().join("modules").join("github-oidc");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            module_dir.join("main.crn"),
            r#"
                arguments {
                    github_repo: String
                    role_name: String
                }
            "#,
        )
        .unwrap();
        let consumer_dir = tmp.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        std::fs::write(
            consumer_dir.join("imports.crn"),
            r#"let github = use { source = '../modules/github-oidc' }
"#,
        )
        .unwrap();
        let main_text = "github {\n  github_repo = 'carina-rs/infra'\n}\n";
        std::fs::write(consumer_dir.join("main.crn"), main_text).unwrap();
        let doc = Document::new(main_text.to_string(), Arc::new(ProviderContext::default()));
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);
        (tmp, consumer_dir, doc, provider)
    }

    /// Issue #2443: hover on the module alias when its `let X = use {...}`
    /// declaration lives in a sibling `.crn` returns module-doc hover.
    #[test]
    fn test_module_call_hover_with_sibling_use_decl() {
        let (_tmp, consumer_dir, doc, provider) = sibling_use_fixture();
        let hover = provider
            .hover_with_base_path(&doc, Position::new(0, 2), Some(&consumer_dir))
            .expect("hover on sibling-defined module alias must return Some");
        let content = match &hover.contents {
            HoverContents::Markup(m) => m.value.clone(),
            _ => panic!("expected markup content"),
        };
        assert!(
            content.contains("github") && content.contains("github_repo"),
            "hover should mention alias and module arguments, got:\n{content}"
        );
    }

    /// Issue #2443: hover on a module argument inside `X { ... }` when
    /// the `let X = use {...}` lives in a sibling `.crn` returns the
    /// argument hover.
    #[test]
    fn test_module_argument_hover_with_sibling_use_decl() {
        let (_tmp, consumer_dir, doc, provider) = sibling_use_fixture();
        let hover = provider
            .hover_with_base_path(&doc, Position::new(1, 4), Some(&consumer_dir))
            .expect("hover on sibling-defined module argument must return Some");
        let content = match &hover.contents {
            HoverContents::Markup(m) => m.value.clone(),
            _ => panic!("expected markup content"),
        };
        assert!(
            content.contains("github_repo"),
            "argument hover should mention the parameter name, got:\n{content}"
        );
    }

    #[test]
    fn nested_module_argument_object_is_not_a_module_call() {
        let temp_dir = tempfile::tempdir().unwrap();
        let github_module = temp_dir.path().join("modules").join("github-oidc");
        let cfg_module = temp_dir.path().join("modules").join("cfg");
        std::fs::create_dir_all(&github_module).unwrap();
        std::fs::create_dir_all(&cfg_module).unwrap();
        std::fs::write(
            github_module.join("main.crn"),
            "arguments {\n  github_repo: String\n}\n",
        )
        .unwrap();
        std::fs::write(cfg_module.join("main.crn"), "arguments {\n  x: Int\n}\n").unwrap();

        let consumer_dir = temp_dir.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        std::fs::write(
            consumer_dir.join("imports.crn"),
            concat!(
                "let github = use { source = '../modules/github-oidc' }\n",
                "let cfg = use { source = '../modules/cfg' }\n",
            ),
        )
        .unwrap();
        let text = r#"github {
  github_repo = 'carina-rs/infra'
  cfg = {
    x = 1
  }
}
"#;
        std::fs::write(consumer_dir.join("main.crn"), text).unwrap();
        let doc = Document::new(text.to_string(), Arc::new(ProviderContext::default()));
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);

        let nested_hover =
            provider.hover_with_base_path(&doc, hover_position(text, "x", 0), Some(&consumer_dir));
        assert!(
            nested_hover.is_none(),
            "nested module argument fields have no module schema: {nested_hover:?}"
        );

        let top_level_hover = provider
            .hover_with_base_path(
                &doc,
                hover_position(text, "github_repo", 0),
                Some(&consumer_dir),
            )
            .expect("top-level module argument should retain hover");
        let content = match top_level_hover.contents {
            HoverContents::Markup(markup) => markup.value,
            HoverContents::Scalar(_) | HoverContents::Array(_) => {
                panic!("expected markdown hover content")
            }
        };
        assert!(content.contains("github.github_repo"), "{content}");
    }

    /// Companion to `test_module_call_hover_same_buffer_fast_path`:
    /// argument-hover on `github_repo` inside a `github { ... }` block
    /// when the `let github = use {...}` lives in the same buffer
    /// (original single-file behavior). End-to-end through
    /// `hover_with_base_path` so the fast-path branch of
    /// `find_use_import_path` is exercised on the argument side too.
    #[test]
    fn test_module_argument_hover_same_buffer_fast_path() {
        let tmp = tempfile::tempdir().unwrap();
        let module_dir = tmp.path().join("modules").join("github-oidc");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            module_dir.join("main.crn"),
            r#"
                arguments {
                    github_repo: String
                }
            "#,
        )
        .unwrap();
        let consumer_dir = tmp.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        let main_text = "let github = use { source = '../modules/github-oidc' }\ngithub {\n  github_repo = 'x'\n}\n";
        std::fs::write(consumer_dir.join("main.crn"), main_text).unwrap();

        let doc = Document::new(main_text.to_string(), Arc::new(ProviderContext::default()));
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);

        // Hover on `github_repo` at line 2, column 4 (inside the identifier).
        let hover = provider
            .hover_with_base_path(&doc, Position::new(2, 4), Some(&consumer_dir))
            .expect("same-buffer fast path for argument hover must return Some");
        let content = match &hover.contents {
            HoverContents::Markup(m) => m.value.clone(),
            _ => panic!("expected markup content"),
        };
        assert!(
            content.contains("github_repo"),
            "fast-path argument hover should mention the parameter name, got:\n{content}"
        );
    }

    /// Fast-path regression guard: when `let X = use {...}` lives in
    /// the *same* buffer the user is hovering in, the helper's first
    /// branch (`doc.parsed()`) must satisfy the lookup without touching
    /// the disk. This was the original single-file behavior before
    /// #2443 — a future refactor of `find_use_import_path` must not
    /// drop it.
    #[test]
    fn test_module_call_hover_same_buffer_fast_path() {
        let tmp = tempfile::tempdir().unwrap();
        let module_dir = tmp.path().join("modules").join("github-oidc");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            module_dir.join("main.crn"),
            r#"
                arguments {
                    github_repo: String
                }
            "#,
        )
        .unwrap();
        let consumer_dir = tmp.path().join("consumer");
        std::fs::create_dir(&consumer_dir).unwrap();
        // Both the `let` and the call live in the same buffer.
        let main_text = "let github = use { source = '../modules/github-oidc' }\ngithub {\n  github_repo = 'x'\n}\n";
        std::fs::write(consumer_dir.join("main.crn"), main_text).unwrap();

        let doc = Document::new(main_text.to_string(), Arc::new(ProviderContext::default()));
        let provider = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]);

        // Hover on `github` at line 1 (the call site, not the let).
        let hover = provider
            .hover_with_base_path(&doc, Position::new(1, 2), Some(&consumer_dir))
            .expect("same-buffer fast path must return Some");
        let content = match &hover.contents {
            HoverContents::Markup(m) => m.value.clone(),
            _ => panic!("expected markup content"),
        };
        assert!(
            content.contains("github_repo"),
            "fast-path hover should still list module arguments, got:\n{content}"
        );
    }

    /// A typo'd alias has no `use` declaration anywhere in the directory
    /// — hover must return None rather than synthesize a bogus result.
    #[test]
    fn test_module_call_hover_unknown_alias_returns_none() {
        let (_tmp, consumer_dir, _doc, provider) = sibling_use_fixture();
        // Document with a typo'd alias that has no matching `use` decl
        // anywhere in the directory.
        let bad_text = "githubb {\n  github_repo = 'x'\n}\n";
        let bad_doc = Document::new(bad_text.to_string(), Arc::new(ProviderContext::default()));
        let hover =
            provider.hover_with_base_path(&bad_doc, Position::new(0, 2), Some(&consumer_dir));
        assert!(
            hover.is_none(),
            "hover on unknown alias must return None, got: {hover:?}"
        );
    }

    /// A parse error in an *unrelated* sibling `.crn` (e.g. half-typed
    /// `providers.crn`) must not block hover on a valid alias declared
    /// in another sibling. The per-file resilient walk in
    /// `find_use_import_path` is what makes this work — guard against
    /// regression to the all-or-nothing `parse_directory_with_overrides`
    /// shape.
    #[test]
    fn test_module_call_hover_survives_unrelated_sibling_parse_error() {
        let (_tmp, consumer_dir, doc, provider) = sibling_use_fixture();
        // Add a deliberately broken sibling — must not affect hover on
        // `github` whose declaration is in `imports.crn`.
        std::fs::write(
            consumer_dir.join("providers.crn"),
            "provider awscc { region = ", // unterminated, parse error
        )
        .unwrap();
        let hover = provider.hover_with_base_path(&doc, Position::new(0, 2), Some(&consumer_dir));
        assert!(
            hover.is_some(),
            "hover should survive an unrelated sibling parse error"
        );
    }
}
