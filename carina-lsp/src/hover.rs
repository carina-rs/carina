use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

use crate::document::Document;
use carina_core::builtins;
use carina_core::parser::ArgumentParameter;
use carina_core::resource::Value;
use carina_core::schema::{CompletionValue, ResourceSchema, SchemaRegistry};

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
        let Ok(parsed) = carina_core::parser::parse(&content, ctx) else {
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
    schemas: Arc<SchemaRegistry>,
    region_completions: Vec<CompletionValue>,
}

impl HoverProvider {
    pub fn new(schemas: Arc<SchemaRegistry>, region_completions: Vec<CompletionValue>) -> Self {
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

        // Check for module argument description hover (inside module calls)
        if self.is_in_module_call(doc, position)
            && let Some(base) = base_path
            && let Some(hover) =
                self.module_argument_hover(doc, position, &word, base, current_file_name)
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
                        // Iterate registry entries but skip empty-provider ones —
                        // the synthetic `format!("{}.", "")` would match every line
                        // and block the resource-block path entirely.
                        let provider_prefixes: Vec<&str> = self
                            .schemas
                            .iter()
                            .map(|(provider, _, _, _)| provider)
                            .filter(|p| !p.is_empty())
                            .collect();
                        // Treat schemas registered without a provider (e.g.
                        // test fixtures with `ResourceSchema::new("ec2.X")` and
                        // `insert("", ...)`) as ordinary resource lines too.
                        let starts_with_known_resource_type = self
                            .schemas
                            .iter()
                            .filter(|(p, _, _, _)| p.is_empty())
                            .any(|(_, rt, _, _)| trimmed.starts_with(&format!("{} ", rt)));
                        let starts_with_provider = provider_prefixes
                            .iter()
                            .any(|p| trimmed.starts_with(&format!("{}.", p)))
                            || starts_with_known_resource_type;

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
                    arg.name, arg.type_expr, desc, required
                ));
            }
        }

        // Show attributes (outputs)
        if !module_parsed.attribute_params.is_empty() {
            content.push_str("\n### Attributes\n\n");
            for attr in &module_parsed.attribute_params {
                let type_str = attr
                    .type_expr
                    .as_ref()
                    .map(|t| format!("`{}`", t))
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
        doc: &Document,
        position: Position,
        word: &str,
        base_path: &Path,
        current_file_name: Option<&str>,
    ) -> Option<Hover> {
        let module_name = self.find_enclosing_module_call_name(doc, position)?;
        let import_path = find_use_import_path(doc, base_path, current_file_name, &module_name)?;

        // Load the module to get argument definitions
        let module_path = base_path.join(&import_path);
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
                        // Iterate registry entries but skip empty-provider ones —
                        // the synthetic `format!("{}.", "")` would match every line
                        // and block the resource-block path entirely.
                        let provider_prefixes: Vec<&str> = self
                            .schemas
                            .iter()
                            .map(|(provider, _, _, _)| provider)
                            .filter(|p| !p.is_empty())
                            .collect();
                        // Treat schemas registered without a provider (e.g.
                        // test fixtures with `ResourceSchema::new("ec2.X")` and
                        // `insert("", ...)`) as ordinary resource lines too.
                        let starts_with_known_resource_type = self
                            .schemas
                            .iter()
                            .filter(|(p, _, _, _)| p.is_empty())
                            .any(|(_, rt, _, _)| trimmed.starts_with(&format!("{} ", rt)));
                        let starts_with_provider = provider_prefixes
                            .iter()
                            .any(|p| trimmed.starts_with(&format!("{}.", p)))
                            || starts_with_known_resource_type;

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

    fn schema_hover(&self, resource_name: &str, schema: &ResourceSchema) -> Option<Hover> {
        let description = convert_markdown_links_to_plain_text(
            schema
                .description
                .as_deref()
                .unwrap_or("No description available"),
        );

        let mut content = format!("## {}\n\n{}\n", resource_name, description);

        // Split attributes into arguments (writable) and attributes (read-only)
        let mut arguments: Vec<&carina_core::schema::AttributeSchema> = schema
            .attributes
            .values()
            .filter(|a| !a.read_only)
            .collect();
        arguments.sort_by_key(|a| &a.name);

        let mut read_only_attrs: Vec<&carina_core::schema::AttributeSchema> =
            schema.attributes.values().filter(|a| a.read_only).collect();
        read_only_attrs.sort_by_key(|a| &a.name);

        if !arguments.is_empty() {
            content.push_str("\n### Arguments\n\n");
            for attr in &arguments {
                let required = if attr.required { " **(required)**" } else { "" };
                let create_only = if attr.create_only {
                    " _(create-only)_"
                } else {
                    ""
                };
                let desc =
                    convert_markdown_links_to_plain_text(attr.description.as_deref().unwrap_or(""));
                content.push_str(&format!(
                    "- `{}`: {}{}{}\n",
                    attr.name, desc, required, create_only
                ));
            }
        }

        if !read_only_attrs.is_empty() {
            content.push_str("\n### Attributes\n\n");
            for attr in &read_only_attrs {
                let desc =
                    convert_markdown_links_to_plain_text(attr.description.as_deref().unwrap_or(""));
                content.push_str(&format!("- `{}`: {}\n", attr.name, desc));
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

    /// Walk backwards from the cursor position, tracking brace depth, to find
    /// the enclosing resource block and extract its resource type (schema key).
    fn find_enclosing_resource_type(&self, doc: &Document, position: Position) -> Option<String> {
        let text = doc.text();
        let lines: Vec<&str> = text.lines().collect();
        let current_line = position.line as usize;

        // Build a list of schema keys sorted longest-first for correct matching
        let mut schema_keys: Vec<String> = self
            .schemas
            .iter()
            .map(|(provider, resource_type, _kind, _schema)| {
                if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{}.{}", provider, resource_type)
                }
            })
            .collect();
        schema_keys.sort();
        schema_keys.dedup();
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
        // When we know the enclosing resource, only its schema is authoritative.
        // Falling back to a global scan would return a lookalike attribute from
        // an unrelated schema (nondeterministic via HashMap iteration order) —
        // see #1988.
        let key = enclosing_resource?;
        // Try splitting on the first dot first (`provider.resource_type`); if that
        // doesn't resolve, fall back to treating the whole key as the resource type
        // under the empty provider — some test fixtures register schemas that way.
        let lookup = |provider: &str, resource_type: &str| {
            self.schemas
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Managed,
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
        schema
            .attributes
            .get(word)
            .and_then(|attr| self.build_attribute_hover(attr))
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
    use carina_core::schema::{AttributeSchema, AttributeType};

    fn create_hover_provider_with_description(
        resource_type: &str,
        description: &str,
    ) -> HoverProvider {
        let mut schemas = SchemaRegistry::new();
        let schema = ResourceSchema::new(resource_type).with_description(description);
        schemas.insert("", schema);
        HoverProvider::new(Arc::new(schemas), vec![])
    }

    fn create_hover_provider_with_attr_description(
        resource_type: &str,
        attr_name: &str,
        attr_description: &str,
    ) -> HoverProvider {
        let mut schemas = SchemaRegistry::new();
        let schema = ResourceSchema::new(resource_type).attribute(
            AttributeSchema::new(attr_name, AttributeType::String)
                .with_description(attr_description),
        );
        schemas.insert("", schema);
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

        let provider = create_hover_provider_with_description("ec2.Vpc", desc);
        let schema = provider
            .schemas
            .get("", "ec2.Vpc", carina_core::schema::SchemaKind::Managed)
            .unwrap();
        let hover = provider.schema_hover("ec2.Vpc", schema).unwrap();

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
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description("The ID of the internet gateway (from internet_gateway schema)."),
        );
        schemas.insert("awscc", igw_schema);

        let attachment_schema = ResourceSchema::new("ec2.vpc_gateway_attachment").attribute(
            AttributeSchema::new("internet_gateway_id", AttributeType::String)
                .with_description(
                    "The ID of the internet gateway attached to the VPC (from vpc_gateway_attachment schema).",
                ),
        );
        schemas.insert("awscc", attachment_schema);

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
            AttributeSchema::new("account_id", AttributeType::String)
                .with_description("The unique identifier (ID) of the new account."),
        );
        schemas.insert("awscc", account_schema);

        // `awscc.sso.Assignment` intentionally has NO `account_id`.
        let assignment_schema = ResourceSchema::new("sso.Assignment").attribute(
            AttributeSchema::new("target_id", AttributeType::String).with_description("Target id."),
        );
        schemas.insert("awscc", assignment_schema);

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
            type_expr: TypeExpr::Ref(carina_core::parser::ResourceTypePath::new(
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
            type_expr: TypeExpr::Int,
            default: Some(Value::Int(8080)),
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
            type_expr: TypeExpr::String,
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
