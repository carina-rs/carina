//! Attribute, value, and type-specific completions.

use std::path::Path;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::builtins;
use carina_core::parser::snake_to_pascal;
use carina_core::schema::AttributeType;

use super::{CompletionProvider, DslSource};

/// How far up the directory tree to walk when suggesting upstream_state sources.
const UPSTREAM_SOURCE_MAX_UP: usize = 6;
/// Safety cap on the number of suggestions returned.
const UPSTREAM_SOURCE_MAX_ITEMS: usize = 100;

/// Context when the user has typed `binding_name.` after `=`.
struct BindingDotContext {
    binding_name: String,
    resource_type: String,
}

fn type_completion_item(label: String, detail: String, range: Range) -> CompletionItem {
    CompletionItem {
        label: label.clone(),
        kind: Some(CompletionItemKind::TYPE_PARAMETER),
        detail: Some(detail),
        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: label,
        })),
        ..Default::default()
    }
}

impl CompletionProvider {
    pub(super) fn attribute_completions_for_type(
        &self,
        resource_type: &str,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Command to trigger suggestions after inserting the completion
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        // Get schema for specific resource type
        if let Some(schema) = self.schemas.get(resource_type) {
            for attr in schema.attributes.values().filter(|a| !a.read_only) {
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

                // For List(Struct) attributes with block_name, offer block syntax completion
                if let Some(bn) = &attr.block_name
                    && matches!(
                        &attr.attr_type,
                        AttributeType::List { inner, .. } if matches!(inner.as_ref(), AttributeType::Struct { .. })
                    )
                {
                    completions.push(CompletionItem {
                        label: bn.clone(),
                        kind: Some(CompletionItemKind::SNIPPET),
                        detail: Some(format!("Block syntax for '{}'", attr.name)),
                        insert_text: Some(format!("{} {{\n  $0\n}}", bn)),
                        insert_text_format: Some(InsertTextFormat::SNIPPET),
                        ..Default::default()
                    });
                }
            }
        }
        // When resource type is unknown, return no attribute completions
        // rather than showing irrelevant attributes from all schemas.

        completions
    }

    pub(super) fn value_completions_for_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        text: &str,
        current_binding: Option<&str>,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();

        // Compute the text_edit range for resource reference completions.
        // When the user has typed "igw." after "=", we need the range to cover
        // from the start of "igw" to the cursor, so accepting a completion like
        // "igw.internet_gateway_id" replaces "igw." instead of being appended.
        let ref_edit_range = self.compute_value_prefix_range(text, position);

        // Binding scans here see the current buffer *and* every sibling
        // `.crn` under `base_path`. Helpers take `DslSource` to make the
        // choice explicit at the call site — see `dsl_source.rs`. The
        // sibling read happens exactly once; the same `src` is reused
        // across every helper below.
        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);

        // Check if the user has typed "binding." after "=" — if so, show only
        // that binding's resource attributes, not built-in functions or generic completions.
        if let Some(dot_binding) =
            self.detect_binding_dot_context(text, position, current_binding, src)
        {
            return self.binding_attribute_completions(
                &dot_binding.binding_name,
                &dot_binding.resource_type,
                ref_edit_range,
            );
        }

        // Type-based resource reference completions:
        // Look up the attribute's type from the schema. If it's a Custom type,
        // find bindings whose resource schema has an attribute with the same Custom type name.
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            let target_type_name = Self::extract_custom_type_name(&attr_schema.attr_type);
            if let Some(target_name) = target_type_name {
                for (binding_name, binding_resource_type) in &self.extract_resource_bindings(src) {
                    if binding_resource_type.is_empty() {
                        continue;
                    }
                    // Skip self-references: don't suggest the current resource's own binding
                    if current_binding.is_some_and(|cb| cb == binding_name) {
                        continue;
                    }
                    // Look up the binding's resource schema and find attributes
                    // with matching Custom type name
                    if let Some(binding_schema) = self.schemas.get(binding_resource_type) {
                        for binding_attr in binding_schema.attributes.values() {
                            if let Some(binding_type_name) =
                                Self::extract_custom_type_name(&binding_attr.attr_type)
                                && binding_type_name == target_name
                            {
                                let full_ref = format!("{}.{}", binding_name, binding_attr.name);
                                completions.push(CompletionItem {
                                    label: full_ref.clone(),
                                    kind: Some(CompletionItemKind::REFERENCE),
                                    detail: Some(format!(
                                        "Reference to {}'s {} ({})",
                                        binding_name, binding_attr.name, target_name
                                    )),
                                    text_edit: Some(
                                        tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                                            range: ref_edit_range,
                                            new_text: full_ref,
                                        }),
                                    ),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }

        // Add argument parameter references (lexically scoped — direct name access).
        // The shared `src` surfaces `arguments { ... }` even when split into
        // a dedicated `arguments.crn`.
        for (name, type_hint) in self.extract_argument_parameters(src) {
            completions.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format!("argument: {}", type_hint)),
                insert_text: Some(name),
                ..Default::default()
            });
        }

        // Add for-loop binding names in scope, filtered by inferred element
        // type where possible. When the iterable is an `upstream_state`
        // export with a declared type, we infer the binding's type and
        // only suggest it at attribute positions whose type accepts it.
        // Inference failure (no type annotation, non-upstream iterable,
        // no schema for the target attribute) falls back to an
        // unconditional suggest so the user still gets autocomplete on
        // the bare name.
        let attr_type_for_for_filter = self
            .schemas
            .get(resource_type)
            .and_then(|s| s.attributes.get(attr_name))
            .map(|a| &a.attr_type);
        let for_bindings = super::top_level::extract_for_bindings_in_scope(text, position);
        // Cache the resolved exports per upstream. Without this, each
        // for-binding re-parses the upstream project every keystroke.
        // The sibling scan itself reuses the already-resolved `src`.
        let upstream_sources: std::collections::HashMap<String, String> =
            if for_bindings.iter().any(|b| b.iterable.is_some()) {
                super::collect_upstream_state_bindings(src)
            } else {
                std::collections::HashMap::new()
            };
        let mut exports_cache: std::collections::HashMap<
            String,
            std::collections::HashMap<String, Option<carina_core::parser::TypeExpr>>,
        > = std::collections::HashMap::new();
        for binding in &for_bindings {
            if let Some(attr_type) = attr_type_for_for_filter
                && let Some(element_type) = infer_for_binding_type(
                    binding,
                    &upstream_sources,
                    base_path,
                    &mut exports_cache,
                )
                && !carina_core::validation::is_type_expr_compatible_with_schema(
                    &element_type,
                    attr_type,
                )
            {
                continue;
            }
            completions.push(CompletionItem {
                label: binding.name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("for-loop binding".to_string()),
                insert_text: Some(binding.name.clone()),
                ..Default::default()
            });
        }

        // Look up the attribute type from schema
        if let Some(schema) = self.schemas.get(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            // Struct types: offer `{ }` snippet only, no built-in functions
            if matches!(&attr_schema.attr_type, AttributeType::Struct { .. }) {
                completions.push(CompletionItem {
                    label: "{ }".to_string(),
                    kind: Some(CompletionItemKind::SNIPPET),
                    detail: Some("Open struct block".to_string()),
                    insert_text: Some("{\n  $0\n}".to_string()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
                return completions;
            }

            // Built-in function completions filtered by return-type fit.
            // Offering every built-in at e.g. an `aws_account_id` cursor
            // would pollute the popup with suggestions that can't produce
            // the right value.
            completions.extend(Self::builtin_function_completions_for_type(
                &attr_schema.attr_type,
            ));

            // First check if schema defines completions for this attribute
            if let Some(schema_completions) = &attr_schema.completions {
                completions.extend(schema_completions.iter().map(|c| CompletionItem {
                    label: c.value.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(c.description.clone()),
                    insert_text: Some(c.value.clone()),
                    ..Default::default()
                }));
                return completions;
            }
            // Fall back to type-based completions
            completions
                .extend(self.completions_for_type(&attr_schema.attr_type, Some(resource_type)));
            return completions;
        }

        // No schema found — offer only type-neutral candidates. Built-in
        // functions are safe (their concrete value types depend on use),
        // but injecting `true`/`false` or every region would pollute the
        // list with values that can't possibly fit an unknown attribute.
        completions.extend(self.builtin_function_completions());
        completions
    }

    /// Detect if the user has typed `binding_name.` after `=` on the current
    /// line. Returns the binding name and its resource type if detected.
    ///
    /// `src` must be [`DslSource::DirectoryScoped`] in normal use so that a
    /// binding declared in a sibling `.crn` can be resolved. `BufferOnly` is
    /// only correct when the feature is genuinely buffer-local.
    fn detect_binding_dot_context(
        &self,
        text: &str,
        position: Position,
        current_binding: Option<&str>,
        src: DslSource<'_>,
    ) -> Option<BindingDotContext> {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }

        let col = position.character as usize;
        let prefix: String = lines[line_idx].chars().take(col).collect();

        // Extract the value part after "="
        let after_eq = prefix.rsplit('=').next()?.trim();

        // Check if it looks like "identifier." (ends with dot or has dot followed by partial text)
        let dot_pos = after_eq.find('.')?;
        let candidate_binding = &after_eq[..dot_pos];

        // Validate binding name: alphanumeric + underscore
        if candidate_binding.is_empty()
            || !candidate_binding
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_')
        {
            return None;
        }

        for (binding_name, binding_resource_type) in &self.extract_resource_bindings(src) {
            if binding_name == candidate_binding && !binding_resource_type.is_empty() {
                // Skip self-references
                if current_binding.is_some_and(|cb| cb == binding_name) {
                    return None;
                }
                return Some(BindingDotContext {
                    binding_name: binding_name.clone(),
                    resource_type: binding_resource_type.clone(),
                });
            }
        }

        None
    }

    /// Provide completions for a binding's resource attributes.
    /// Shows all attributes of the binding's resource type as `binding.attribute` completions.
    fn binding_attribute_completions(
        &self,
        binding_name: &str,
        binding_resource_type: &str,
        edit_range: Range,
    ) -> Vec<CompletionItem> {
        let mut completions = Vec::new();
        if let Some(schema) = self.schemas.get(binding_resource_type) {
            for attr in schema.attributes.values() {
                let full_ref = format!("{}.{}", binding_name, attr.name);
                completions.push(CompletionItem {
                    label: full_ref.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: attr.description.clone(),
                    text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                        range: edit_range,
                        new_text: full_ref,
                    })),
                    ..Default::default()
                });
            }
        }
        completions
    }

    /// Compute a text edit range covering the value prefix the user has already typed
    /// after the `=` sign. This allows resource reference completions like `igw.internet_gateway_id`
    /// to replace the already-typed prefix (e.g., `igw.`) instead of being appended after it.
    fn compute_value_prefix_range(&self, text: &str, position: Position) -> Range {
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let start_col = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Find the position after "= " (the value start)
            if let Some(eq_pos) = prefix.rfind('=') {
                let after_eq = &prefix[eq_pos + 1..];
                let whitespace_len = after_eq.len() - after_eq.trim_start().len();
                (eq_pos + 1 + whitespace_len) as u32
            } else {
                position.character
            }
        } else {
            position.character
        };

        Range {
            start: Position {
                line: position.line,
                character: start_col,
            },
            end: position,
        }
    }

    /// Extract the Custom type name from an AttributeType, if it is a Custom type.
    fn extract_custom_type_name(attr_type: &AttributeType) -> Option<&str> {
        match attr_type {
            AttributeType::Custom {
                semantic_name: Some(name),
                ..
            } => Some(name),
            _ => None,
        }
    }

    pub(super) fn completions_for_type(
        &self,
        attr_type: &AttributeType,
        resource_type: Option<&str>,
    ) -> Vec<CompletionItem> {
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
            AttributeType::StringEnum {
                name,
                values,
                namespace,
                to_dsl,
            } => {
                // Use explicit namespace if available, otherwise derive from resource_type
                let effective_ns = namespace.as_deref().or(if !name.is_empty() {
                    resource_type
                } else {
                    None
                });
                self.string_enum_completions(name, values, effective_ns, *to_dsl)
            }
            AttributeType::Int => {
                vec![] // No specific completions for integers
            }
            AttributeType::Float => {
                vec![] // No specific completions for floats
            }
            AttributeType::Custom {
                semantic_name: Some(name),
                ..
            } if name == "Cidr" || name == "Ipv4Cidr" => self.cidr_completions(),
            AttributeType::Custom {
                semantic_name: Some(name),
                ..
            } if name == "Ipv6Cidr" => self.ipv6_cidr_completions(),
            AttributeType::Custom {
                semantic_name: Some(name),
                ..
            } if name == "Arn" => self.arn_completions(),
            AttributeType::Custom {
                semantic_name: Some(name),
                namespace,
                ..
            } if name == "AvailabilityZone" => {
                self.availability_zone_completions(namespace.as_deref().unwrap_or(""), name)
            }
            // List(non-Struct): delegate to inner type completions
            AttributeType::List { inner, .. } => self.completions_for_type(inner, resource_type),
            // Map: delegate to inner value type completions
            AttributeType::Map { value: inner, .. } => {
                self.completions_for_type(inner, resource_type)
            }
            // Union: collect completions from all member types
            AttributeType::Union(members) => {
                let mut completions = Vec::new();
                let mut seen_labels = std::collections::HashSet::new();
                for member in members {
                    for item in self.completions_for_type(member, resource_type) {
                        if seen_labels.insert(item.label.clone()) {
                            completions.push(item);
                        }
                    }
                }
                completions
            }
            _ => vec![],
        }
    }

    pub(super) fn provider_block_completions(&self) -> Vec<CompletionItem> {
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };

        vec![
            CompletionItem {
                label: "region".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Provider region".to_string()),
                insert_text: Some("region = ".to_string()),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "source".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Provider source (e.g., github.com/owner/repo)".to_string()),
                insert_text: Some("source = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
            CompletionItem {
                label: "version".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Version constraint (e.g., ~0.5.0)".to_string()),
                insert_text: Some("version = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
            CompletionItem {
                label: "revision".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(
                    "Git revision (branch, tag, or SHA) for CI artifact resolution".to_string(),
                ),
                insert_text: Some("revision = \"$0\"".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            },
        ]
    }

    /// Completions for `for <pat> in <HERE>` — every binding that
    /// `check_deferred_for_iterables` treats as in-scope: `let`,
    /// `upstream_state`, module calls, imports, and argument parameters.
    ///
    /// Bindings commonly live in sibling `.crn` files (a typical pattern
    /// is `let orgs = upstream_state { ... }` in `backend.crn` iterated
    /// from `main.crn`), so we read every `.crn` in `base_path`, not just
    /// the current buffer.
    pub(super) fn for_iterable_completions(
        &self,
        text: &str,
        position: Position,
        partial: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let partial_chars = partial.chars().count() as u32;
        let range = Range {
            start: Position {
                line: position.line,
                character: position.character.saturating_sub(partial_chars),
            },
            end: position,
        };

        let mut items = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let push = |items: &mut Vec<CompletionItem>,
                    seen: &mut std::collections::HashSet<String>,
                    name: String,
                    detail: &str| {
            if !seen.insert(name.clone()) {
                return;
            }
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(detail.to_string()),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: name,
                })),
                ..Default::default()
            });
        };

        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        for (name, rhs) in Self::extract_let_bindings(src) {
            let detail = if rhs.starts_with("upstream_state") {
                "upstream_state binding"
            } else if rhs.starts_with("use ") || rhs.starts_with("use{") {
                "module use"
            } else {
                "binding"
            };
            push(&mut items, &mut seen, name, detail);
        }
        for (name, _) in self.extract_argument_parameters(src) {
            push(&mut items, &mut seen, name, "argument");
        }

        items
    }

    /// Completions for `<binding>.<partial>` where `<binding>` is an
    /// `upstream_state`. Lists every key declared in the upstream's
    /// `exports { }` block, via `resolve_upstream_exports`.
    pub(super) fn upstream_state_dot_completions(
        &self,
        binding: &str,
        partial: &str,
        source: &str,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let Some(base) = base_path else {
            return Vec::new();
        };
        let upstream = carina_core::parser::UpstreamState {
            binding: binding.to_string(),
            source: std::path::PathBuf::from(source),
        };
        let (exports, _errors) = carina_core::upstream_exports::resolve_upstream_exports(
            base,
            &[upstream],
            &Default::default(),
        );
        let Some(keys) = exports.get(binding) else {
            return Vec::new();
        };

        // Replace just `<partial>` (the characters after the dot), so
        // accepting a suggestion slots into the existing `<binding>.` prefix
        // instead of duplicating it.
        let partial_chars = partial.chars().count() as u32;
        let range = Range {
            start: Position {
                line: position.line,
                character: position.character.saturating_sub(partial_chars),
            },
            end: position,
        };

        // Render the export's declared `TypeExpr` into the detail when it
        // exists so the user can see `map(aws_account_id)` at the popup
        // instead of cross-referencing the upstream's `exports.crn`. Exports
        // declared without a type annotation fall back to the generic
        // phrasing (still useful; tells the user which binding it came from).
        let mut items: Vec<CompletionItem> = keys
            .iter()
            .map(|(key, type_expr)| CompletionItem {
                label: key.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(match type_expr {
                    Some(t) => format!("export from upstream_state `{}`: {}", binding, t),
                    None => format!("export from upstream_state `{}`", binding),
                }),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: key.clone(),
                })),
                ..Default::default()
            })
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        items
    }

    pub(super) fn upstream_state_block_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "source".to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some("Path to the upstream project directory".to_string()),
            insert_text: Some("source = \"$0\"".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        }]
    }

    /// Sole attribute of a `use { ... }` block. Mirrors the
    /// `upstream_state` version, which has the same `source = '...'` shape.
    pub(super) fn use_block_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "source".to_string(),
            kind: Some(CompletionItemKind::PROPERTY),
            detail: Some("Module directory path".to_string()),
            insert_text: Some("source = '$0'".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        }]
    }

    /// Suggest sibling (and uncle/grand-uncle) directories that look like carina
    /// projects, for use as the `source` attribute of an `upstream_state` block.
    pub(super) fn upstream_state_source_completions(
        &self,
        partial_path: &str,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let Some(base) = base_path else {
            return vec![];
        };
        let base_abs = canonical_or_self(base);

        // Skip the directory that is the current project itself; everything else
        // we emit at most once, preferring the nearest ancestor encoding.
        let mut seen_targets: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        seen_targets.insert(base_abs.clone());
        let mut suggestions: Vec<String> = Vec::new();

        let mut current = base_abs;
        for depth in 1..=UPSTREAM_SOURCE_MAX_UP {
            let Some(parent) = current.parent().map(Path::to_path_buf) else {
                break;
            };
            find_carina_projects_under(&parent, depth, &mut seen_targets, &mut suggestions);
            if suggestions.len() >= UPSTREAM_SOURCE_MAX_ITEMS {
                suggestions.truncate(UPSTREAM_SOURCE_MAX_ITEMS);
                break;
            }
            current = parent;
        }

        // Truncated by ancestor discovery order (nearest first), then sorted for display.
        suggestions.sort();

        // Replace the typed partial (from just after the opening quote to the
        // cursor) with the full suggestion. A plain `insert_text` would leave
        // the client to infer the replacement range via word boundaries, which
        // splits on `/` and `.` and produces `../../foo` when the user typed
        // `../or` — an explicit TextEdit avoids that.
        let partial_chars = partial_path.chars().count() as u32;
        let range = Range {
            start: Position {
                line: position.line,
                character: position.character.saturating_sub(partial_chars),
            },
            end: position,
        };

        suggestions
            .into_iter()
            .filter(|p| partial_path.is_empty() || p.starts_with(partial_path))
            .map(|p| CompletionItem {
                // Label keeps quotes so the popup reads like the source form;
                // the TextEdit inserts the bare path, since the cursor is
                // already inside an open quote.
                label: format!("'{}'", p),
                kind: Some(CompletionItemKind::FOLDER),
                detail: Some("Carina project".to_string()),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: p,
                })),
                ..Default::default()
            })
            .collect()
    }

    /// Provide completions for built-in function names.
    pub(super) fn builtin_function_completions(&self) -> Vec<CompletionItem> {
        Self::builtin_completions_matching(|_| true)
    }

    /// Type-aware completions at a value position inside an `exports`
    /// block. `type_expr_text` is the raw annotation (`string`,
    /// `map(aws_account_id)`, `list(string)`, …) pulled off the
    /// enclosing entry. When `in_nested` is true the cursor is inside
    /// that entry's `{ ... }` map/list body, so the effective type is
    /// the annotation unwrapped by one level.
    ///
    /// When the annotation can't be parsed this returns the empty
    /// list — "silent rather than noisy" stays the right fallback
    /// since an unknown type offers no basis for suggestion.
    pub(super) fn exports_value_completions(
        &self,
        type_expr_text: &str,
        in_nested: bool,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let Some(annotation) = parse_exports_type_text(type_expr_text) else {
            return Vec::new();
        };
        let effective = if in_nested {
            match &annotation {
                AttributeType::List { inner, .. } | AttributeType::Map { value: inner, .. } => {
                    (**inner).clone()
                }
                // Inside `{ ... }` of a non-collection annotation we
                // don't know what the user means — fall silent.
                _ => return Vec::new(),
            }
        } else {
            annotation
        };

        let mut items = Self::builtin_function_completions_for_type(&effective);
        items.extend(self.resource_ref_completions_for_type(&effective, text, base_path));
        items
    }

    /// Find resource-ref paths (`<binding>.<attr>`) whose leaf attribute is
    /// assignable to `target` and return them as completion items. Uses a
    /// directory-scoped [`DslSource`] so exports in `exports.crn` can
    /// reference bindings declared in a sibling `main.crn`
    /// (see #2043 follow-up).
    fn resource_ref_completions_for_type(
        &self,
        target: &AttributeType,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        for (binding_name, resource_type) in self.extract_resource_bindings(src) {
            if resource_type.is_empty() {
                continue;
            }
            let Some(schema) = self.schemas.get(&resource_type) else {
                continue;
            };
            for attr in schema.attributes.values() {
                if !attr.attr_type.is_assignable_to(target) {
                    continue;
                }
                let full_ref = format!("{}.{}", binding_name, attr.name);
                items.push(CompletionItem {
                    label: full_ref.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!(
                        "Reference to {}'s {} ({})",
                        binding_name,
                        attr.name,
                        attr.attr_type.type_name()
                    )),
                    insert_text: Some(full_ref),
                    ..Default::default()
                });
            }
        }
        items.sort_by(|a, b| a.label.cmp(&b.label));
        items
    }

    /// Return built-in function completions whose declared return type is
    /// compatible with the attribute's declared `AttributeType`. Used by
    /// value-position completion to avoid suggesting `concat` / `join` /
    /// etc. at cursors whose type is e.g. an `aws_account_id`.
    pub(super) fn builtin_function_completions_for_type(
        attr_type: &AttributeType,
    ) -> Vec<CompletionItem> {
        Self::builtin_completions_matching(|ret| return_type_fits(ret, attr_type))
    }

    fn builtin_completions_matching(
        accept: impl Fn(builtins::BuiltinReturnType) -> bool,
    ) -> Vec<CompletionItem> {
        builtins::builtin_functions()
            .iter()
            .filter(|func| accept(func.return_type))
            .map(|func| CompletionItem {
                label: func.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(func.signature.to_string()),
                insert_text: Some(format!("{}($0)", func.name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn region_completions_for_provider(
        &self,
        provider_name: &str,
    ) -> Vec<CompletionItem> {
        let prefix = format!("{}.Region.", provider_name);
        self.region_completions_data
            .iter()
            .filter(|c| c.value.starts_with(&prefix))
            .map(|c| CompletionItem {
                label: c.value.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(c.description.clone()),
                insert_text: Some(c.value.clone()),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn cidr_completions(&self) -> Vec<CompletionItem> {
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
                label: format!("'{}'", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("'{}'", cidr)),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn ipv6_cidr_completions(&self) -> Vec<CompletionItem> {
        let cidrs = vec![
            ("::/0", "All IPv6 addresses"),
            ("2001:db8::/32", "Documentation range"),
            ("fe80::/10", "Link-local addresses"),
            ("fc00::/7", "Unique local addresses"),
            ("::1/128", "Loopback address"),
        ];

        cidrs
            .into_iter()
            .map(|(cidr, description)| CompletionItem {
                label: format!("'{}'", cidr),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(description.to_string()),
                insert_text: Some(format!("'{}'", cidr)),
                ..Default::default()
            })
            .collect()
    }

    pub(super) fn arn_completions(&self) -> Vec<CompletionItem> {
        vec![CompletionItem {
            label: "'arn:aws:...'".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            insert_text: Some(
                "'arn:aws:${1:service}:${2:region}:${3:account}:${4:resource}'".to_string(),
            ),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("ARN format: Arn:partition:service:region:account:resource".to_string()),
            ..Default::default()
        }]
    }

    pub(super) fn ref_type_completions(
        &self,
        position: Position,
        text: &str,
    ) -> Vec<CompletionItem> {
        // Calculate the replacement range: from right after ":" to the cursor position.
        // This ensures dotted identifiers like "aws.ec2.Vpc" are replaced correctly
        // without duplication from LSP word-boundary-based insertion.
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        let col = position.character as usize;

        let (type_start, inside_generic) = if line_idx < lines.len() {
            let prefix: String = lines[line_idx].chars().take(col).collect();
            // Check if cursor is inside list() or map() — find last open paren after colon
            if let Some(colon_pos) = prefix.rfind(':') {
                let after_colon = &prefix[colon_pos + 1..];
                if let Some(paren_pos) = after_colon.rfind('(') {
                    // Inside list() or map(): start after the open paren
                    let abs_paren = colon_pos + 1 + paren_pos + 1;
                    (abs_paren as u32, true)
                } else {
                    let whitespace_len = after_colon.len() - after_colon.trim_start().len();
                    ((colon_pos + 1 + whitespace_len) as u32, false)
                }
            } else {
                (position.character, false)
            }
        } else {
            (position.character, false)
        };

        let replacement_range = Range {
            start: Position {
                line: position.line,
                character: type_start,
            },
            end: position,
        };

        // Basic types
        let basic = [
            ("String", "String type"),
            ("Int", "Integer type"),
            ("Bool", "Boolean type"),
            ("Float", "Float type"),
        ]
        .iter()
        .map(|(name, detail)| {
            type_completion_item(name.to_string(), detail.to_string(), replacement_range)
        });

        // Generic type constructors (skip when already inside list()/map())
        let generic: Vec<CompletionItem> = if inside_generic {
            vec![]
        } else {
            [
                ("list(", "List type constructor"),
                ("map(", "Map type constructor"),
            ]
            .iter()
            .map(|(name, detail)| {
                type_completion_item(name.to_string(), detail.to_string(), replacement_range)
            })
            .collect()
        };

        // Custom types: built-in types + provider-extracted types (deduplicated).
        // Internal registry is snake_case; the LSP surface is PascalCase.
        let builtin_custom = ["ipv4_cidr", "ipv4_address", "ipv6_cidr", "ipv6_address"];
        let mut seen_custom = std::collections::HashSet::new();
        let custom: Vec<CompletionItem> = builtin_custom
            .iter()
            .map(|s| s.to_string())
            .chain(self.custom_type_names.iter().cloned())
            .filter(|name| seen_custom.insert(name.clone()))
            .map(|snake| {
                let label = snake_to_pascal(&snake);
                let detail = format!("Custom type: {label}");
                type_completion_item(label, detail, replacement_range)
            })
            .collect();

        // Resource types from schemas
        let resource = self.schemas.iter().map(move |(resource_type, schema)| {
            let description = schema
                .description
                .as_deref()
                .unwrap_or("Resource reference");
            type_completion_item(
                resource_type.clone(),
                format!("{} reference", description),
                replacement_range,
            )
        });

        basic.chain(generic).chain(custom).chain(resource).collect()
    }

    pub(super) fn availability_zone_completions(
        &self,
        namespace: &str,
        type_name: &str,
    ) -> Vec<CompletionItem> {
        let prefix = if namespace.is_empty() {
            type_name.to_string()
        } else {
            format!("{}.{}", namespace, type_name)
        };

        // Build region display names from region_completions_data, filtered by namespace
        let region_prefix = format!("{}.Region.", namespace);
        let region_names: std::collections::HashMap<String, String> = self
            .region_completions_data
            .iter()
            .filter(|c| c.value.starts_with(&region_prefix))
            .filter_map(|c| {
                let region_code = c.value.strip_prefix(&region_prefix)?;
                // Extract short name from description like "Asia Pacific (Tokyo)" -> "Tokyo"
                let short_name = c
                    .description
                    .find('(')
                    .and_then(|start| {
                        c.description[start + 1..]
                            .find(')')
                            .map(|end| &c.description[start + 1..start + 1 + end])
                    })
                    .unwrap_or(&c.description);
                Some((region_code.to_string(), short_name.to_string()))
            })
            .collect();

        // Generate AZ completions for each region with zone letters a-d
        let zone_letters = ['a', 'b', 'c', 'd'];
        let mut completions = Vec::new();

        for (region_code, region_name) in &region_names {
            for &zone_letter in &zone_letters {
                let az = format!("{}{}", region_code, zone_letter);
                let label = format!("{}.{}", prefix, az);
                let detail = format!("{} Zone {}", region_name, zone_letter);
                completions.push(CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(detail),
                    insert_text: Some(label),
                    ..Default::default()
                });
            }
        }

        // Sort by label for consistent ordering
        completions.sort_by(|a, b| a.label.cmp(&b.label));
        completions
    }

    pub(super) fn string_enum_completions(
        &self,
        type_name: &str,
        values: &[String],
        namespace: Option<&str>,
        to_dsl: Option<fn(&str) -> String>,
    ) -> Vec<CompletionItem> {
        match namespace {
            Some(ns) => {
                // Offer the fully-qualified form
                // `<namespace>.<TypeName>.<Variant>`. The bare tail alone
                // used to leak into sibling-attribute popups via the
                // generic identifier pool; the qualified form is always
                // valid and unambiguous.
                values
                    .iter()
                    .map(|value| {
                        let dsl_value = to_dsl.map_or_else(|| value.clone(), |f| f(value));
                        let full = format!("{}.{}.{}", ns, type_name, dsl_value);
                        CompletionItem {
                            label: full.clone(),
                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                            detail: Some(value.clone()),
                            insert_text: Some(full),
                            ..Default::default()
                        }
                    })
                    .collect()
            }
            None => values
                .iter()
                .map(|value| CompletionItem {
                    label: format!("'{}'", value),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    insert_text: Some(format!("'{}'", value)),
                    ..Default::default()
                })
                .collect(),
        }
    }
}

/// Infer the static type of a for-loop binding by resolving its iterable
/// to an `upstream_state` export's declared `TypeExpr`. Returns `None`
/// when inference isn't possible — the caller falls back to
/// unconditional suggestion so the user still gets autocomplete on the
/// bare name.
///
/// Current reach: `for _, v in <binding>.<export>` where `<binding>` is
/// an `upstream_state` binding and `<export>` has a `: map(T) = ...` or
/// `: list(T) = ...` annotation. Direct `let` iterables and inferred
/// `ResourceRef` types are future work.
///
/// `exports_cache` memoizes the per-upstream exports map across bindings
/// in the same call site so multiple `for`-variables in scope don't each
/// re-read and re-parse the upstream project directory.
fn infer_for_binding_type(
    binding: &super::top_level::ForScopeBinding,
    upstream_sources: &std::collections::HashMap<String, String>,
    base_path: Option<&Path>,
    exports_cache: &mut std::collections::HashMap<
        String,
        std::collections::HashMap<String, Option<carina_core::parser::TypeExpr>>,
    >,
) -> Option<carina_core::parser::TypeExpr> {
    use super::top_level::ForBindingSlot;
    use carina_core::parser::TypeExpr;

    let iterable = binding.iterable.as_ref()?;
    let base = base_path?;
    let source = upstream_sources.get(&iterable.binding)?;

    let exports = exports_cache
        .entry(iterable.binding.clone())
        .or_insert_with(|| {
            let upstream = carina_core::parser::UpstreamState {
                binding: iterable.binding.clone(),
                source: std::path::PathBuf::from(source),
            };
            let (resolved, _errors) = carina_core::upstream_exports::resolve_upstream_exports(
                base,
                &[upstream],
                &Default::default(),
            );
            resolved.get(&iterable.binding).cloned().unwrap_or_default()
        });
    let export_type = exports.get(&iterable.export)?.as_ref()?;

    match (export_type, binding.slot) {
        (TypeExpr::List(inner), ForBindingSlot::Value | ForBindingSlot::PairValue) => {
            Some((**inner).clone())
        }
        (TypeExpr::List(_), ForBindingSlot::PairKey) => Some(TypeExpr::Int),
        (TypeExpr::Map(inner), ForBindingSlot::Value | ForBindingSlot::PairValue) => {
            Some((**inner).clone())
        }
        (TypeExpr::Map(_), ForBindingSlot::PairKey) => Some(TypeExpr::String),
        // Iterating a struct binds each field as a (name, value) pair,
        // matching the runtime's map-iteration semantics. Field types
        // may differ, so we only surface a value type when all fields
        // share it — otherwise no completion is inferred.
        (TypeExpr::Struct { fields }, ForBindingSlot::Value | ForBindingSlot::PairValue) => {
            let mut iter = fields.iter().map(|(_, ty)| ty);
            let first = iter.next()?;
            if iter.all(|ty| ty == first) {
                Some(first.clone())
            } else {
                None
            }
        }
        (TypeExpr::Struct { .. }, ForBindingSlot::PairKey) => Some(TypeExpr::String),
        _ => None,
    }
}

/// Decide whether a built-in's declared return type is assignable to the
/// attribute's declared `AttributeType`.
///
/// Matching rules:
/// * `BuiltinReturnType::Any` fits any base-typed attribute (String, Int,
///   List, Map) — the built-in's concrete shape depends on its arguments.
///   It does not fit `Custom` or `StringEnum`: no argument-derived result
///   can prove the semantic invariant those types require.
/// * A built-in returning plain `String` fits only a schema-declared
///   `String` or a `Union` containing one. It does **not** fit a `Custom`
///   type (even one whose base is `String`) because the Custom type
///   carries a semantic meaning the built-in cannot produce (e.g. an
///   `aws_account_id` must be a 12-digit string, and `join(...)` offers
///   no such guarantee).
/// * `List`, `Map`, `Int`, `Secret` match their corresponding
///   `AttributeType` constructor. For `List` / `Map` the inner type
///   doesn't participate in the check — the built-in's declared element
///   type is unknown at this layer.
fn return_type_fits(ret: builtins::BuiltinReturnType, attr_type: &AttributeType) -> bool {
    use builtins::BuiltinReturnType as R;
    match attr_type {
        AttributeType::Union(members) => members.iter().any(|m| return_type_fits(ret, m)),
        AttributeType::String => matches!(ret, R::String | R::Any),
        AttributeType::Int => matches!(ret, R::Int | R::Any),
        // No built-in currently returns a Bool; leave as unfit.
        AttributeType::Bool => false,
        AttributeType::List { .. } => matches!(ret, R::List | R::Any),
        AttributeType::Map { .. } => matches!(ret, R::Map | R::Any),
        // StringEnum expects a specific identifier form; no built-in
        // currently produces such values, and `Any` alone doesn't give us
        // enough confidence to suggest one.
        AttributeType::StringEnum { .. } => false,
        // Custom types carry a semantic meaning (Cidr, AwsAccountId, Arn,
        // …) that built-ins don't declare. Not even `Any` fits — we need
        // a semantic return annotation before a built-in can be suggested
        // here.
        AttributeType::Custom { .. } => false,
        // Float and Struct attributes — no matching built-in today.
        AttributeType::Float => false,
        AttributeType::Struct { .. } => false,
    }
}

/// Parse the raw text of an `exports` entry's type annotation into an
/// `AttributeType` good enough to drive value-position completion
/// filtering. Accepts the shapes the DSL grammar produces at this
/// surface:
///
/// * primitive names (`string`, `int`, `float`, `bool`)
/// * `list(T)` / `map(T)` with recursive inner parsing
/// * a bare identifier interpreted as a custom semantic subtype — the
///   name is PascalCase'd (`aws_account_id` → `AwsAccountId`) and
///   stored as `Custom { semantic_name: Some(PascalCase), base: String }`
///   so `is_assignable_to` matches schemas that declare the same
///   semantic name.
///
/// Namespaced identifiers (`aws.vpc.VpcId`) are **not** parsed today —
/// they fall into `None` and the caller silently drops to the empty
/// completion set. Add a dotted-path branch when a real corpus shows
/// users reaching for that shape in `exports` annotations.
///
/// Returns `None` for anything we don't understand; the caller falls
/// back to the empty completion set rather than dumping everything.
fn parse_exports_type_text(text: &str) -> Option<AttributeType> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(inner) = strip_generic("list", text) {
        return parse_exports_type_text(inner).map(AttributeType::list);
    }
    if let Some(inner) = strip_generic("map", text) {
        let inner_ty = parse_exports_type_text(inner)?;
        return Some(AttributeType::Map {
            key: Box::new(AttributeType::String),
            value: Box::new(inner_ty),
        });
    }
    match text {
        // Post-Phase C, primitive types are PascalCase. Accept only the new
        // spellings at the surface.
        "String" => Some(AttributeType::String),
        "Int" => Some(AttributeType::Int),
        "Float" => Some(AttributeType::Float),
        "Bool" => Some(AttributeType::Bool),
        // Custom types also ship in PascalCase now (e.g. `AwsAccountId`,
        // `Ipv4Cidr`). Carry the PascalCase form as `semantic_name` and
        // canonicalise the internal key to snake_case.
        name if name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            Some(AttributeType::Custom {
                semantic_name: Some(name.to_string()),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: noop_validate,
                namespace: None,
                to_dsl: None,
            })
        }
        _ => None,
    }
}

/// `list(...)` / `map(...)` wrapper stripper. Returns the inner text
/// between balanced parens, `None` for any other shape.
fn strip_generic<'a>(prefix: &str, text: &'a str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    let rest = rest.strip_suffix(')')?;
    Some(rest)
}

fn noop_validate(_v: &carina_core::resource::Value) -> Result<(), String> {
    Ok(())
}

/// Scan `ancestor` for directories that look like carina projects.
///
/// At `up_depth == 1` (the direct parent of `base`), only the immediate
/// children are considered. From `up_depth >= 2` the scan also descends one
/// more level, so patterns like `../../modules/web` are reachable even when
/// the `modules` dir itself is not a project. The descent is deliberately
/// capped at grandchildren to keep the search bounded at every ancestor.
///
/// `seen` holds the canonical paths already emitted at a nearer ancestor (and
/// the base itself); entries already in `seen` are skipped so a sibling never
/// reappears via a longer ancestor route.
fn find_carina_projects_under(
    ancestor: &Path,
    up_depth: usize,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(ancestor) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        let Some(child_name) = visible_dir_name(&child) else {
            continue;
        };
        let child_abs = canonical_or_self(&child);
        if dir_has_crn_file(&child) {
            if seen.insert(child_abs) {
                out.push(join_relative(up_depth, &[&child_name]));
            }
            continue;
        }
        if up_depth < 2 {
            continue;
        }
        let Ok(grandchildren) = std::fs::read_dir(&child) else {
            continue;
        };
        for grand in grandchildren.flatten() {
            let grand_path = grand.path();
            let Some(grand_name) = visible_dir_name(&grand_path) else {
                continue;
            };
            let grand_abs = canonical_or_self(&grand_path);
            if dir_has_crn_file(&grand_path) && seen.insert(grand_abs) {
                out.push(join_relative(up_depth, &[&child_name, &grand_name]));
            }
        }
    }
}

/// `Some(name)` if `path` is a directory with a non-hidden name, else `None`.
fn visible_dir_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') || !path.is_dir() {
        return None;
    }
    Some(name.to_string())
}

fn canonical_or_self(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn dir_has_crn_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().extension().is_some_and(|ext| ext == "crn"))
}

fn join_relative(up_depth: usize, parts: &[&str]) -> String {
    let mut s = String::new();
    for _ in 0..up_depth {
        s.push_str("../");
    }
    s.push_str(&parts.join("/"));
    s
}
