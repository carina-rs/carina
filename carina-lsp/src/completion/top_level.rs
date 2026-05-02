//! Top-level, module, and input parameter completions.

use std::path::Path;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::parser;

use super::{CompletionProvider, DslSource};

impl CompletionProvider {
    pub(super) fn top_level_completions(
        &self,
        position: Position,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
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

        // Avoid emitting a duplicate `let` when the line already has `let <name> =`.
        let after_let_binding =
            line_idx < lines.len() && is_after_let_binding(lines[line_idx], prefix_start as usize);

        let read_snippet = if after_let_binding {
            "read ${1:aws.s3.Bucket} {\n    name = \"${2:existing-resource}\"\n}"
        } else {
            "let ${1:name} = read ${2:aws.s3.Bucket} {\n    name = \"${3:existing-resource}\"\n}"
        };
        let let_use_snippet = if after_let_binding {
            "use {\n    source = '${1:./modules/name}'\n}"
        } else {
            "let ${1:module_name} = use {\n    source = '${2:./modules/name}'\n}"
        };
        let upstream_state_snippet = if after_let_binding {
            "upstream_state {\n    source = '${1:../other-project}'\n}"
        } else {
            "let ${1:binding} = upstream_state {\n    source = '${2:../other-project}'\n}"
        };

        let mut completions = vec![
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
                label: "read".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some(read_snippet.to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Read existing resource (data source)".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "arguments".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("arguments {\n    ${1:param}: ${2:type}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module argument parameters".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "attributes".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("attributes {\n    ${1:name}: ${2:type} = ${3:value}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define module attribute values".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "exports".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("exports {\n    ${1:name}: ${2:type} = ${3:value}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Publish values for upstream_state consumers".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "fn".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("fn ${1:name}(${2:params}) {\n    ${3:body}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Define a pure function".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "let use".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some(let_use_snippet.to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Load a module from a directory".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "import".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("import {\n    to = ${1:awscc.ec2.Vpc} '${2:name}'\n    id = '${3:resource-id}'\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Import existing resource into state".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "removed".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("removed {\n    from = ${1:awscc.ec2.Vpc} '${2:name}'\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Remove resource from state without destroying".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "moved".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("moved {\n    from = ${1:awscc.ec2.Vpc} '${2:old-name}'\n    to   = ${3:awscc.ec2.Vpc} '${4:new-name}'\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Move/rename resource in state".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "backend".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("backend s3 {\n    bucket = \"${1:my-carina-state}\"\n    key    = \"${2:prod/carina.crnstate}\"\n    region = aws.Region.${3:ap_northeast_1}\n}".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Configure state backend (S3)".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "upstream_state".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some(upstream_state_snippet.to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Reference another project's exported state".to_string()),
                ..Default::default()
            },
            CompletionItem {
                label: "require".to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some("require ${1:condition}, \"${2:error message}\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some("Cross-argument constraint".to_string()),
                ..Default::default()
            },
        ];

        // Generate module binding completions from `use` statements
        // e.g., "let github = use { source = '...' }" → suggest "github" with call scaffold
        for line in lines.iter() {
            if let Some((binding, after_eq)) = crate::let_parse::parse_let_header(line)
                && binding != "_"
                && (after_eq.starts_with("use ") || after_eq.starts_with("use{"))
            {
                let snippet = self.build_module_call_snippet(binding, after_eq, base_path);
                completions.push(CompletionItem {
                    label: binding.to_string(),
                    kind: Some(CompletionItemKind::MODULE),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    detail: Some("Module call".to_string()),
                    ..Default::default()
                });
            }
        }

        // Generate resource type completions from schemas
        for (provider, resource_type, _kind, schema) in self.schemas.iter() {
            let key = if provider.is_empty() {
                resource_type.to_string()
            } else {
                format!("{}.{}", provider, resource_type)
            };
            let description = schema
                .description
                .as_deref()
                .unwrap_or("Resource")
                .to_string();

            // Build snippet with required attributes
            let mut snippet = format!("{} {{\n", key);
            let mut tab_stop = 1;
            for attr in schema.attributes.values() {
                if attr.required {
                    snippet.push_str(&format!("    {} = ${{{}}}\n", attr.name, tab_stop));
                    tab_stop += 1;
                }
            }
            snippet.push('}');

            completions.push(CompletionItem {
                label: key.clone(),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replacement_range,
                    new_text: snippet,
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                detail: Some(description),
                ..Default::default()
            });
        }

        completions
    }

    pub(super) fn extract_argument_parameters(&self, src: DslSource<'_>) -> Vec<(String, String)> {
        let text = src.merged_text();
        let mut params = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut in_arguments_block = false;
        let mut brace_depth = 0;

        for line in text.lines() {
            let trimmed = line.trim();

            // Check for "arguments {" block start
            if trimmed.starts_with("arguments ") && trimmed.contains('{') {
                in_arguments_block = true;
                brace_depth = 1;
                continue;
            }

            if in_arguments_block {
                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_arguments_block = false;
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
                        if !name.is_empty() && seen.insert(name.clone()) {
                            params.push((name, type_hint));
                        }
                    }
                }
            }
        }

        params
    }

    /// Extract every `let <name> = <rhs>` from `src`.
    ///
    /// Returns `Vec<(name, rhs)>` where `rhs` is the trimmed text after `=`
    /// (e.g. `awscc.ec2.Vpc { ... }`, `upstream_state { ... }`, `import '...'`).
    /// Callers classify the rhs themselves — `extract_resource_bindings` maps
    /// it to a schema key, the for-iterable handler keys it into a detail
    /// label, etc. Duplicates (same binding name appearing in the buffer
    /// and in a sibling `.crn`) are deduped, buffer-first.
    pub(super) fn extract_let_bindings(src: DslSource<'_>) -> Vec<(String, String)> {
        let text = src.merged_text();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        text.lines()
            .filter_map(|line| {
                crate::let_parse::parse_let_header(line)
                    .filter(|(name, _)| seen.insert(name.to_string()))
                    .map(|(name, rhs)| (name.to_string(), rhs.to_string()))
            })
            .collect()
    }

    /// Extract resource binding names and their resource types from `src`
    /// (variables defined with `let binding_name = awscc.ec2.Vpc {`).
    /// Returns Vec<(binding_name, resource_type)> where resource_type is the schema key
    /// (e.g., "awscc.ec2.Vpc"). See [`DslSource`] for buffer-vs-directory choice.
    pub(super) fn extract_resource_bindings(&self, src: DslSource<'_>) -> Vec<(String, String)> {
        Self::extract_let_bindings(src)
            .into_iter()
            .map(|(name, rhs)| (name, self.extract_resource_type(&rhs).unwrap_or_default()))
            .collect()
    }

    pub(super) fn module_parameter_completions(
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
            if let Some(parsed) = carina_core::module_resolver::load_module(&module_path) {
                // Extract argument parameters from the module
                for input in &parsed.arguments {
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

    pub(super) fn format_type_expr(&self, type_expr: &parser::TypeExpr) -> String {
        match type_expr {
            parser::TypeExpr::String => "string".to_string(),
            parser::TypeExpr::Bool => "bool".to_string(),
            parser::TypeExpr::Int => "int".to_string(),
            parser::TypeExpr::Float => "float".to_string(),
            parser::TypeExpr::Simple(name) => name.clone(),
            parser::TypeExpr::List(inner) => format!("list({})", self.format_type_expr(inner)),
            parser::TypeExpr::Map(inner) => format!("map({})", self.format_type_expr(inner)),
            parser::TypeExpr::Ref(resource_path) => {
                format!("{}.{}", resource_path.provider, resource_path.resource_type)
            }
            schema_type @ parser::TypeExpr::SchemaType { .. } => schema_type.to_string(),
            struct_type @ parser::TypeExpr::Struct { .. } => struct_type.to_string(),
        }
    }

    /// Find the module `source` path for a given module binding from `let name = use { source = '...' }`.
    pub(super) fn find_module_import_path(&self, module_name: &str, text: &str) -> Option<String> {
        for line in text.lines() {
            if let Some((alias, after_eq)) = crate::let_parse::parse_let_header(line)
                && alias == module_name
                && let Some(path) = extract_use_source_path(after_eq)
            {
                return Some(path);
            }
        }
        None
    }

    /// Build a snippet for a module call with argument placeholders.
    ///
    /// If the module can be loaded, generates a snippet with all arguments
    /// as tab stops. Falls back to a simple `name { ${1} }` if loading fails.
    fn build_module_call_snippet(
        &self,
        binding: &str,
        after_eq: &str,
        base_path: Option<&Path>,
    ) -> String {
        // Extract source path from "use { source = 'path' }" or with double quotes.
        let import_path = extract_use_source_path(after_eq);

        if let Some(path) = import_path.as_deref()
            && let Some(base) = base_path
            && let Some(parsed) = carina_core::module_resolver::load_module(&base.join(path))
            && !parsed.arguments.is_empty()
        {
            let mut snippet = format!("{} {{\n", binding);
            let max_len = parsed
                .arguments
                .iter()
                .map(|a| a.name.len())
                .max()
                .unwrap_or(0);
            for (i, arg) in parsed.arguments.iter().enumerate() {
                let padding = " ".repeat(max_len - arg.name.len());
                snippet.push_str(&format!("  {}{} = ${{{}}}\n", arg.name, padding, i + 1));
            }
            snippet.push('}');
            return snippet;
        }

        // Fallback: simple scaffold
        format!("{} {{\n  ${{1}}\n}}", binding)
    }
}

/// Extract the `source` path from a `let` RHS of the shape `use { source = 'path' }`
/// (or with double quotes). Returns `None` if `after_eq` doesn't look like a `use` block
/// or if the `source` value can't be parsed.
pub(super) fn extract_use_source_path(after_eq: &str) -> Option<String> {
    let trimmed = after_eq.trim_start();
    let rest = trimmed
        .strip_prefix("use ")
        .or_else(|| trimmed.strip_prefix("use{"))
        .map(|s| s.trim_start_matches('{').trim_start())?;

    let idx = rest.find("source")?;
    let after = rest[idx + "source".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let body = &after[1..];
    let end = body.find(quote)?;
    Some(body[..end].to_string())
}

/// Build a navigation-anchor `CompletionItem` for `./` or `../`.
///
/// The `label` and `insert_text` are the same literal text — the LSP
/// client replaces the current path "word" with this. Triggers a follow-up
/// suggest so the user immediately sees the resolved directory's entries.
fn path_anchor(text: &str) -> CompletionItem {
    CompletionItem {
        label: text.to_string(),
        kind: Some(CompletionItemKind::FOLDER),
        insert_text: Some(text.to_string()),
        detail: Some("Relative path".to_string()),
        sort_text: Some(format!("0_{}", text)),
        command: Some(Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        }),
        ..Default::default()
    }
}

impl CompletionProvider {
    /// Generate import path completions from filesystem.
    ///
    /// Lists directories and `.crn` files relative to the base_path,
    /// using the partial_path to determine which directory to list.
    pub(super) fn import_path_completions(
        &self,
        partial_path: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let base = match base_path {
            Some(b) => b,
            None => return vec![],
        };

        let mut completions = Vec::new();

        // Navigation anchors. A user typing `'|`, `'.|`, or `'..|` is starting
        // to build a relative path — offer `./` and `../` so the list is
        // never empty even when the current dir has no module subdirs (the
        // common case for leaf config dirs like `infra/aws/management/github-oidc/`).
        // Once the partial contains a `/` the user has committed to a
        // direction and wants entries from the resolved dir, not anchors.
        if !partial_path.contains('/') {
            if partial_path.is_empty() || "./".starts_with(partial_path) {
                completions.push(path_anchor("./"));
            }
            if partial_path.is_empty() || "../".starts_with(partial_path) {
                completions.push(path_anchor("../"));
            }
        }

        // Split partial_path into directory prefix and filename prefix
        let (dir_part, name_prefix) = if let Some(last_slash) = partial_path.rfind('/') {
            (
                &partial_path[..=last_slash],
                &partial_path[last_slash + 1..],
            )
        } else {
            ("", partial_path)
        };

        let search_dir = base.join(dir_part);
        let entries = match std::fs::read_dir(&search_dir) {
            Ok(entries) => entries,
            Err(_) => return completions,
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            // Skip hidden files/dirs
            if name.starts_with('.') {
                continue;
            }

            let path = entry.path();
            // Modules are directory-scoped, so only directories are valid
            // import targets; .crn files on their own are rejected by the
            // resolver (ModuleError::NotADirectory).
            if path.is_dir() && name.starts_with(name_prefix) {
                completions.push(CompletionItem {
                    label: format!("{}/", name),
                    kind: Some(CompletionItemKind::FOLDER),
                    insert_text: Some(format!("{}/", name)),
                    detail: Some("Module directory".to_string()),
                    command: Some(Command {
                        title: "Trigger Suggest".to_string(),
                        command: "editor.action.triggerSuggest".to_string(),
                        arguments: None,
                    }),
                    ..Default::default()
                });
            }
        }

        completions.sort_by(|a, b| a.label.cmp(&b.label));
        completions
    }
}

/// Return true when `line[..prefix_start]` already forms a `let <name> =` binding header.
/// `prefix_start` is a char offset, converted to a byte offset for slicing.
fn is_after_let_binding(line: &str, prefix_start: usize) -> bool {
    let byte_end = line
        .char_indices()
        .nth(prefix_start)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    crate::let_parse::parse_let_header(&line[..byte_end]).is_some()
}

/// Which slot of a `for` header introduced a given binding. Encodes the
/// header shape so a downstream type inferencer can project the correct
/// element type out of the iterable's own type: e.g. `Value` against
/// `list(T)` yields `T`, `PairKey` against `map(T)` yields `String`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ForBindingSlot {
    /// `for x in iter` — `x` is the element.
    Value,
    /// First slot of `for a, b in iter` — the index (for lists) or key
    /// (for maps). The scanner can't distinguish `for (i, x) in list`
    /// from `for k, v in map` from text alone; the type inferencer
    /// resolves the ambiguity using the iterable's actual type.
    PairKey,
    /// Second slot of `for a, b in iter` — the element value.
    PairValue,
}

/// An `upstream_state` export referenced by a `for` header's iterable.
/// Only the two-segment `binding.export` shape is captured because it's
/// the only form we can currently resolve to a declared type.
#[derive(Debug, Clone)]
pub(super) struct IterableExportRef {
    pub binding: String,
    pub export: String,
}

/// One binding introduced by a `for` header that is in scope at a given
/// position. Carries enough context for the LSP to resolve the binding's
/// inferred type: the iterable export the header refers to and the
/// binding's slot in the header.
#[derive(Debug, Clone)]
pub(super) struct ForScopeBinding {
    /// The bare identifier the user typed (never `_`).
    pub name: String,
    /// Slot in the header that introduced this name — see `ForBindingSlot`.
    pub slot: ForBindingSlot,
    /// Parsed iterable reference. `None` when the iterable isn't a
    /// two-segment `binding.export` shape (e.g. a local `let`, a
    /// function call, or a bare identifier) — the type inferencer bails
    /// in that case and the caller falls back to the permissive suggest.
    pub iterable: Option<IterableExportRef>,
}

/// Extract the full binding info for every `for`-loop binding in scope at
/// `position`. See `ForScopeBinding` for what each entry carries.
///
/// Parsing is intentionally line- and brace-based rather than AST-based:
/// the document may not parse while the user is typing, so we cannot
/// depend on a successful parse of the partial text.
pub(super) fn extract_for_bindings_in_scope(
    text: &str,
    position: tower_lsp::lsp_types::Position,
) -> Vec<ForScopeBinding> {
    // Byte offset of the cursor inside `text`.
    let cursor_byte_offset: usize = {
        let mut offset = 0usize;
        for (i, line) in text.split('\n').enumerate() {
            if i as u32 == position.line {
                let char_col = position.character as usize;
                let byte_col = line
                    .char_indices()
                    .nth(char_col)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
                offset += byte_col;
                break;
            }
            offset += line.len() + 1; // +1 for the `\n`
        }
        offset
    };

    // Each frame on the stack represents one open `for` body: the bindings
    // it introduced plus a depth counter of *nested* `{` inside it. The
    // frame pops when that depth reaches -1 (the loop's own `}`).
    let mut stack: Vec<(Vec<ForScopeBinding>, i32)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;

    while i < cursor_byte_offset {
        let line_start = i;
        while i < cursor_byte_offset && bytes[i] != b'\n' {
            i += 1;
        }
        let line_end = i.min(text.len());
        let line = &text[line_start..line_end];
        if i < cursor_byte_offset && bytes.get(i) == Some(&b'\n') {
            i += 1;
        }

        let trimmed = line.trim_start();
        // `for ...` header: collect bindings; the body opens at the first `{`
        // on this line.
        let mut pending_frame: Option<Vec<ForScopeBinding>> = None;
        if let Some(rest) = trimmed.strip_prefix("for ")
            && let Some((header, after_in)) = rest.split_once(" in ")
        {
            let iterable_raw = after_in.split_once('{').map(|(l, _)| l).unwrap_or(after_in);
            let mut segments = iterable_raw.trim().split('.').filter(|s| !s.is_empty());
            let iterable = match (segments.next(), segments.next(), segments.next()) {
                (Some(binding), Some(export), None) => Some(IterableExportRef {
                    binding: binding.to_string(),
                    export: export.to_string(),
                }),
                _ => None,
            };
            let cleaned = header.trim().trim_start_matches('(').trim_end_matches(')');
            let raw_parts: Vec<&str> = cleaned.split(',').map(|p| p.trim()).collect();
            let is_pair = raw_parts.len() == 2;
            let mut bindings: Vec<ForScopeBinding> = Vec::new();
            for (index, part) in raw_parts.iter().enumerate() {
                if part.is_empty() || *part == "_" {
                    continue;
                }
                let slot = match (is_pair, index) {
                    (false, _) => ForBindingSlot::Value,
                    (true, 0) => ForBindingSlot::PairKey,
                    (true, 1) => ForBindingSlot::PairValue,
                    _ => continue,
                };
                bindings.push(ForScopeBinding {
                    name: part.to_string(),
                    slot,
                    iterable: iterable.clone(),
                });
            }
            pending_frame = Some(bindings);
        }

        for b in line.bytes() {
            match b {
                b'{' => {
                    if let Some(bindings) = pending_frame.take() {
                        // This `{` starts the body of the `for` header on this line.
                        stack.push((bindings, 0));
                    } else if let Some(frame) = stack.last_mut() {
                        frame.1 += 1;
                    }
                }
                b'}' => {
                    if let Some(frame) = stack.last_mut() {
                        if frame.1 == 0 {
                            stack.pop();
                        } else {
                            frame.1 -= 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    stack
        .into_iter()
        .flat_map(|(bindings, _)| bindings)
        .collect()
}

#[cfg(test)]
mod helper_tests {
    use super::{extract_for_bindings_in_scope, is_after_let_binding};
    use tower_lsp::lsp_types::Position;

    fn names(text: &str, position: Position) -> Vec<String> {
        extract_for_bindings_in_scope(text, position)
            .into_iter()
            .map(|b| b.name)
            .collect()
    }

    #[test]
    fn detects_after_let_binding() {
        assert!(is_after_let_binding("let orgs = u", 11));
        assert!(is_after_let_binding("let orgs =u", 10));
        assert!(is_after_let_binding("  let x = ", 10));
    }

    #[test]
    fn rejects_plain_top_level() {
        assert!(!is_after_let_binding("u", 1));
        assert!(!is_after_let_binding("let ", 4));
        assert!(!is_after_let_binding("let orgs", 8));
    }

    fn pos(line: u32, col: u32) -> Position {
        Position {
            line,
            character: col,
        }
    }

    #[test]
    fn for_scope_simple_binding_inside_body() {
        let text = "for item in items {\n  x\n}\n";
        // cursor on line 1 ("  x")
        let names = names(text, pos(1, 3));
        assert_eq!(names, vec!["item".to_string()]);
    }

    #[test]
    fn for_scope_map_bindings_inside_body() {
        let text = "for name, account_id in orgs.accounts {\n  x\n}\n";
        let names = names(text, pos(1, 3));
        assert_eq!(names, vec!["name".to_string(), "account_id".to_string()]);
    }

    #[test]
    fn for_scope_indexed_bindings_inside_body() {
        let text = "for (i, item) in items {\n  x\n}\n";
        let names = names(text, pos(1, 3));
        assert_eq!(names, vec!["i".to_string(), "item".to_string()]);
    }

    #[test]
    fn for_scope_discard_excluded() {
        // `_` is a discard marker and must not appear as a candidate.
        let text = "for _, v in m {\n  x\n}\n";
        let names = names(text, pos(1, 3));
        assert_eq!(names, vec!["v".to_string()]);
    }

    #[test]
    fn for_scope_outside_body_no_bindings() {
        // Cursor after the closing `}` — loop variable out of scope.
        let text = "for item in items {\n  x\n}\nfoo\n";
        let names = names(text, pos(3, 2));
        assert!(names.is_empty(), "expected empty, got: {:?}", names);
    }

    #[test]
    fn for_scope_nested_stacks_outer_and_inner() {
        let text = "for outer in xs {\n  for inner in ys {\n    x\n  }\n}\n";
        // cursor on line 2 ("    x") — both outer and inner visible
        let names = names(text, pos(2, 5));
        assert_eq!(names, vec!["outer".to_string(), "inner".to_string()]);
    }

    #[test]
    fn for_scope_braces_in_strings_do_not_break_tracking() {
        // Braces inside string literals should not throw off the depth
        // counter. A string containing `{` or `}` appears in attribute
        // values (e.g. JSON embedded as a string).
        let text = "for item in items {\n  test.foo.bar {\n    attr = \"hello { world }\"\n    x\n  }\n}\n";
        // Cursor on line 3 ("    x") — still inside the for body
        let names = names(text, pos(3, 5));
        assert_eq!(names, vec!["item".to_string()]);
    }

    #[test]
    fn for_scope_sibling_loops_dont_leak() {
        // After the first loop closes, its binding must go out of scope
        // even while a second loop with a different binding is open.
        let text = "for a in xs {\n  x\n}\nfor b in ys {\n  y\n}\n";
        let on_b = names(text, pos(4, 3));
        assert_eq!(on_b, vec!["b".to_string()]);
    }
}
