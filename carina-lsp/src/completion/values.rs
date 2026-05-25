//! Attribute, value, and type-specific completions.

use std::path::Path;

use tower_lsp::lsp_types::{
    Command, CompletionItem, CompletionItemKind, InsertTextFormat, Position, Range, TextEdit,
};

use carina_core::builtins;
use carina_core::parser::snake_to_pascal;
use carina_core::schema::{AttributeType, TypeIdentity, legacy_validator};

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

/// Caller mode for [`CompletionProvider::in_scope_binding_completions`].
///
/// The two arms reflect the two real call sites — partial-replacing
/// (for-iterable / interpolation) and value-position (resource / nested
/// struct attribute = ▉). Splitting them keeps illegal mixes (e.g.
/// `range: Some(_)` *and* `current_binding: Some(_)`) unrepresentable.
pub(super) enum InScopeBindingMode<'a> {
    /// `for ... in <HERE>` and `"${<HERE>}"` — accepting a candidate
    /// replaces the partial the user has already typed, so emit a
    /// `text_edit` covering that range.
    PartialReplace { range: Range },
    /// Value position inside a resource block (`attr = ▉`). No partial
    /// to replace; emit `insert_text` and skip self-references via
    /// `current_binding`. The fuller `argument: <type>` detail is
    /// surfaced here because compatibility with the surrounding
    /// attribute is part of the popup's job.
    ValuePosition { current_binding: Option<&'a str> },
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
        if let Some(schema) = self.lookup_schema(resource_type) {
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

    // 8 args is over clippy's 7-default; bundling them into a context
    // struct is a worthwhile cleanup but out of scope for #2643. Track
    // it as a follow-up if the param list grows again.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn value_completions_for_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        text: &str,
        current_binding: Option<&str>,
        position: Position,
        base_path: Option<&Path>,
        provider_ctx: &carina_core::parser::ProviderContext,
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

        // Cache resolved upstream `exports` maps for the lifetime of this
        // call. The upstream-binding REFERENCE pass below and the
        // for-binding type-inference pass further down both call
        // `resolve_upstream_exports`, which re-parses the upstream
        // directory on each invocation — sharing one cache holds the
        // cost to a single parse per upstream binding per keystroke.
        let upstream_sources = super::collect_upstream_state_bindings(src);
        let mut exports_cache: std::collections::HashMap<
            String,
            carina_core::upstream_exports::UpstreamExportEntries,
        > = std::collections::HashMap::new();

        // Type-based reference completions: walk every binding in scope and
        // emit `<binding>.<field>` candidates whose type matches the target
        // attribute. Two binding sources contribute, sharing the same
        // schema lookup but using different matching strategies:
        //
        //   - Resource bindings: each binding's resource schema is queried
        //     directly. A field is offered when its `Custom`-type name
        //     equals the target attribute's `Custom`-type name. This is a
        //     string compare — adequate because both ends sit inside the
        //     compiled provider schema and share a closed type vocabulary.
        //
        //   - `upstream_state` bindings (#2353): exports are declared by
        //     the user with a `TypeExpr` that lives in a different type
        //     system than the schema's `AttributeType`, so name equality
        //     does not apply. `is_type_expr_compatible_with_schema`
        //     bridges the two — it walks `Custom` base chains and accepts
        //     structural shapes (list/map/struct), so e.g. an export
        //     declared `: String` matches a schema attribute typed
        //     `Custom { semantic_name: "Arn", base: String, .. }`.
        //
        // Resource bindings skip self-references (`current_binding`) because
        // their own attributes are accessible by bare attribute name within
        // the block. Upstream bindings have no such bare-name shortcut — the
        // only way to reference an export is `<binding>.<export>` — and a
        // resource block being edited can never be an `upstream_state`
        // block (the dispatcher routes those through
        // `InsideUpstreamStateBlock`), so the self-skip is unreachable
        // for upstream bindings and intentionally omitted.
        if let Some(schema) = self.lookup_schema(resource_type)
            && let Some(attr_schema) = schema.attributes.get(attr_name)
        {
            if let Some(target_name) = Self::extract_custom_type_name(&attr_schema.attr_type) {
                for (binding_name, binding_resource_type) in &self.extract_resource_bindings(src) {
                    if binding_resource_type.is_empty() {
                        continue;
                    }
                    if current_binding.is_some_and(|cb| cb == binding_name) {
                        continue;
                    }
                    if let Some(binding_schema) = self.lookup_schema(binding_resource_type) {
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

            for (binding, source) in &upstream_sources {
                let exports = resolve_upstream_exports_cached(
                    binding,
                    source,
                    base_path,
                    &self.schemas,
                    &mut exports_cache,
                );
                for (export_name, entry) in exports {
                    let Some(export_type) = &entry.type_expr else {
                        continue;
                    };
                    if !carina_core::validation::is_type_expr_compatible_with_schema(
                        export_type,
                        &attr_schema.attr_type,
                    ) {
                        continue;
                    }
                    let full_ref = format!("{}.{}", binding, export_name);
                    completions.push(CompletionItem {
                        label: full_ref.clone(),
                        kind: Some(CompletionItemKind::REFERENCE),
                        detail: Some(format!(
                            "export from upstream_state `{}` ({})",
                            binding,
                            self.format_type_expr(export_type)
                        )),
                        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                            range: ref_edit_range,
                            new_text: full_ref,
                        })),
                        ..Default::default()
                    });
                }
            }
        }

        // Target attribute type, if the schema knows about this
        // (resource_type, attr_name) pair. Used by both the arguments
        // filter just below and the for-binding filter further down.
        let target_attr_type = self
            .lookup_schema(resource_type)
            .and_then(|s| s.attributes.get(attr_name))
            .map(|a| &a.attr_type);

        // In-scope bare identifiers: `let` bindings + `arguments {}`
        // parameters (#2624 / #2642). When the target attribute's
        // type resolves, drop arguments whose declared `TypeExpr`
        // can't produce that type — a `Bool` argument can't satisfy
        // a `String` cursor, a generic `String` can't satisfy a
        // `Custom { semantic_name: Some(_) }`. Bare `let` names are
        // never filtered (no scalar value type to judge), and the
        // filter is bypassed when the target type is unknown or the
        // argument's type hint fails to parse — "show everything"
        // is safer than "show nothing" mid-edit. (#2643)
        let in_scope = self.in_scope_binding_completions_with_src(
            src,
            InScopeBindingMode::ValuePosition { current_binding },
        );
        match target_attr_type {
            None => completions.extend(in_scope),
            Some(attr_type) => {
                // Argument count is small (usually < 10), so a Vec
                // walked linearly is cheaper than a HashMap.
                let typed_arguments: Vec<(String, carina_core::parser::TypeExpr)> = self
                    .extract_argument_parameters(src)
                    .into_iter()
                    .filter_map(|(name, type_hint)| {
                        carina_core::parser::parse_type_expr_str(&type_hint, provider_ctx)
                            .map(|t| (name, t))
                    })
                    .collect();
                completions.extend(in_scope.into_iter().filter(|item| {
                    let Some((_, arg_type_expr)) =
                        typed_arguments.iter().find(|(n, _)| n == &item.label)
                    else {
                        return true;
                    };
                    carina_core::validation::is_type_expr_compatible_with_schema(
                        arg_type_expr,
                        attr_type,
                    )
                }));
            }
        }

        // Add for-loop binding names in scope, filtered by inferred element
        // type where possible. When the iterable is an `upstream_state`
        // export with a declared type, we infer the binding's type and
        // only suggest it at attribute positions whose type accepts it.
        // Inference failure (no type annotation, non-upstream iterable,
        // no schema for the target attribute) falls back to an
        // unconditional suggest so the user still gets autocomplete on
        // the bare name.
        let attr_type_for_for_filter = target_attr_type;
        let for_bindings = super::top_level::extract_for_bindings_in_scope(text, position);
        for binding in &for_bindings {
            if let Some(attr_type) = attr_type_for_for_filter
                && let Some(element_type) = infer_for_binding_type(
                    binding,
                    &upstream_sources,
                    base_path,
                    &self.schemas,
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
        if let Some(schema) = self.lookup_schema(resource_type)
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

            // Built-in functions are intentionally NOT emitted at the
            // bare value position. The return-type filter is too coarse
            // to honor `Custom { semantic_name: ... }` receivers — it
            // lets `lower(x): String` reach a `Custom { IamRoleName }`
            // cursor even though no String can satisfy that identity.
            // Continuing to surface them at every typed position taught
            // "anything goes" and crowded out the in-scope reference
            // candidates that are the real point of the popup. The
            // pre-existing precedents are `971f1b78` (Struct attrs hide
            // builtins) and `45f9d0b6` (post-`binding.` hides builtins) —
            // this extends the same principle to bare value position.
            // Builtins are still reachable by typing the function name
            // directly; a future PR can add a `<id>(`-triggered surface
            // if we want explicit completion for that intent. (#2643)

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

        // No schema found — return whatever in-scope identifiers the
        // earlier passes already pushed. Builtins are intentionally
        // suppressed here too: when the receiver type is unknown we
        // cannot honor it, and the noise drowns out the actual scope
        // candidates. See the comment on the typed branch above.
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
        if let Some(schema) = self.lookup_schema(binding_resource_type) {
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

    /// Extract the Custom type's kind name from an AttributeType, if it
    /// is a Custom type with a structured identity.
    fn extract_custom_type_name(attr_type: &AttributeType) -> Option<&str> {
        match attr_type {
            AttributeType::Custom {
                identity: Some(id), ..
            } => Some(&id.kind),
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
                identity,
                dsl_aliases,
            } => {
                // Derive the dotted prefix from the structured
                // identity. Fall back to the surrounding resource
                // type when the enum has no provider scope of its
                // own — this matches the pre-#3222 `namespace`
                // fallback rule for bare enums.
                let id_prefix = identity.as_ref().and_then(|id| id.dotted_prefix());
                let effective_ns = id_prefix.as_deref().or(if !name.is_empty() {
                    resource_type
                } else {
                    None
                });
                self.string_enum_completions(name, values, effective_ns, dsl_aliases)
            }
            AttributeType::Int => {
                vec![] // No specific completions for integers
            }
            AttributeType::Float => {
                vec![] // No specific completions for floats
            }
            // Curated unit-snippet completions for Duration attributes,
            // per `notes/specs/2026-05-10-duration-design.md` §"LSP /
            // formatter / diagnostics". The list is intentionally
            // short — common timeout magnitudes — rather than every
            // unit alias, because the user types a digit prefix
            // anyway and only needs hint candidates for the unit.
            AttributeType::Duration => vec![
                CompletionItem {
                    label: "30s".to_string(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some("Duration: 30 seconds".to_string()),
                    ..Default::default()
                },
                CompletionItem {
                    label: "1min".to_string(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some("Duration: 1 minute".to_string()),
                    ..Default::default()
                },
                CompletionItem {
                    label: "5min".to_string(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some("Duration: 5 minutes".to_string()),
                    ..Default::default()
                },
                CompletionItem {
                    label: "1h".to_string(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some("Duration: 1 hour".to_string()),
                    ..Default::default()
                },
            ],
            AttributeType::Custom {
                identity: Some(id), ..
            } if id.kind == "Cidr" || id.kind == "Ipv4Cidr" => self.cidr_completions(),
            AttributeType::Custom {
                identity: Some(id), ..
            } if id.kind == "Ipv6Cidr" => self.ipv6_cidr_completions(),
            // Generic ARN snippet only for the bare `Arn` type. Specific
            // ARN families (`IamRoleArn`, `IamPolicyArn`,
            // `IamOidcProviderArn`, `KmsKeyArn`, …) already have shape
            // constraints that the generic `arn:partition:service:…`
            // template can't satisfy — surfacing it for those types is
            // pure noise next to a properly-typed binding ref. Per-
            // family snippets can be added as additional arms later if
            // the per-type formats are useful enough to justify the
            // surface. See #2621.
            AttributeType::Custom {
                identity: Some(id), ..
            } if id.kind == "Arn" => self.arn_completions(),
            // AvailabilityZone is split into two distinct types post-S2.5:
            // `aws.AvailabilityZone.ZoneName` and `aws.AvailabilityZone.ZoneId`.
            // The `kind` axis is `"ZoneName"` / `"ZoneId"`; `"AvailabilityZone"`
            // lives in `segments`. We surface AZ-letter completions only for
            // zone-name typed sinks — zone-id values look like `usw2-az1` and
            // are not derivable from region code + letter.
            AttributeType::Custom {
                identity: Some(id), ..
            } if id.kind == "ZoneName"
                && id.segments.first().map(String::as_str) == Some("AvailabilityZone") =>
            {
                self.availability_zone_completions(id)
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

    /// Attribute-name candidates for `directives { | }` (#2873). The
    /// five directives — `create_before_destroy`, `depends_on`,
    /// `force_delete`, `prevent_destroy`, `provider` — exhaustively.
    /// Order is alphabetical to match `KEYWORDS` ordering in
    /// `keywords.rs`. `provider` was added in carina#2191 Phase 5 for
    /// routing to named provider instances.
    pub(super) fn directives_block_completions(&self) -> Vec<CompletionItem> {
        let trigger_suggest = Command {
            title: "Trigger Suggest".to_string(),
            command: "editor.action.triggerSuggest".to_string(),
            arguments: None,
        };
        vec![
            CompletionItem {
                label: "create_before_destroy".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(
                    "Create the replacement before destroying the old resource".to_string(),
                ),
                insert_text: Some("create_before_destroy = ".to_string()),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "depends_on".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Explicit ordering edges to sibling let bindings".to_string()),
                insert_text: Some("depends_on = [$0]".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "force_delete".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Force-delete the resource (e.g., non-empty S3 buckets)".to_string()),
                insert_text: Some("force_delete = ".to_string()),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "prevent_destroy".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Block any plan that would destroy this resource".to_string()),
                insert_text: Some("prevent_destroy = ".to_string()),
                command: Some(trigger_suggest.clone()),
                ..Default::default()
            },
            CompletionItem {
                label: "provider".to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some(
                    "Route this resource to a named provider instance \
                     (`let <name> = provider <kind> { ... }`)"
                        .to_string(),
                ),
                insert_text: Some("provider = ".to_string()),
                command: Some(trigger_suggest),
                ..Default::default()
            },
        ]
    }

    /// Value candidates for `directives { provider = | }` (carina#2191
    /// Phase 5). Suggests every named provider instance binding
    /// declared anywhere in the directory. The kind's default
    /// instance (a top-level `provider <kind> { ... }` block without
    /// a `let` prefix) carries no binding name and is intentionally
    /// not surfaced — omitting `directives.provider` routes there.
    pub(super) fn directives_provider_completions(
        &self,
        provider_ctx: &carina_core::parser::ProviderContext,
        base_path: Option<&std::path::Path>,
    ) -> Vec<CompletionItem> {
        let bindings = collect_named_provider_instance_bindings(provider_ctx, base_path);
        bindings
            .into_iter()
            .map(|binding| CompletionItem {
                label: binding.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format!(
                    "Named provider instance `let {} = provider <kind> {{ ... }}`",
                    binding
                )),
                insert_text: Some(binding),
                ..Default::default()
            })
            .collect()
    }

    /// Element candidates for `directives { depends_on = [|] }`
    /// (#2873). Suggests every in-scope `let` binding that names a
    /// resource (managed) or module call. Excludes:
    ///   - data source bindings (rejected by `validate_depends_on`)
    ///   - upstream_state bindings (rejected by `validate_depends_on`)
    ///   - the enclosing binding (self-reference is an error)
    ///   - names already present in the list before the cursor
    pub(super) fn directives_depends_on_completions(
        &self,
        text: &str,
        current_binding: Option<&str>,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let already_present = self.depends_on_elements_before_cursor(text, position);

        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        let raw = self.in_scope_binding_completions_with_src(
            src,
            InScopeBindingMode::ValuePosition { current_binding },
        );

        // `in_scope_binding_completions_with_src` returns every in-scope
        // binding (resources, modules, upstream_state, arguments). For
        // depends_on we drop upstream_state via the `detail` text — the
        // helper sets `detail = "upstream_state binding"` for those.
        // Same for arguments (not let-bound resources).
        raw.into_iter()
            .filter(|item| {
                let detail = item.detail.as_deref().unwrap_or("");
                if detail.contains("upstream_state") || detail.contains("argument") {
                    return false;
                }
                !already_present.contains(&item.label)
            })
            .collect()
    }

    /// Parse `depends_on = [<elements...><cursor>` from `text`,
    /// returning bare identifier names already typed before the
    /// cursor. Used to suppress duplicates in
    /// `directives_depends_on_completions`.
    fn depends_on_elements_before_cursor(
        &self,
        text: &str,
        position: Position,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        let lines: Vec<&str> = text.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return out;
        }
        let line = lines[line_idx];
        let col = position.character as usize;
        let prefix_chars: String = line.chars().take(col).collect();
        let Some(open) = prefix_chars.rfind('[') else {
            return out;
        };
        let inner = &prefix_chars[open + 1..];
        for raw in inner.split(',') {
            let name = raw.trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                out.insert(name.to_string());
            }
        }
        out
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

    /// Surface every in-scope identifier — `let` bindings (resource,
    /// module use, upstream_state) and `arguments {}` parameters — as
    /// candidates. Bindings commonly live in sibling `.crn` files (e.g.
    /// `let orgs = upstream_state { ... }` in `backend.crn` referenced
    /// from `main.crn`), so we read every `.crn` in `base_path`.
    ///
    /// Two callers share this:
    ///   - `for ... in <HERE>` and `"${<HERE>"` (partial-replacing): a
    ///     `Range` is supplied so accepting a candidate replaces the
    ///     already-typed prefix.
    ///   - Value position inside a resource / nested struct (#2624):
    ///     `range = None`, `current_binding = Some(...)` to skip
    ///     self-references.
    pub(super) fn in_scope_binding_completions(
        &self,
        text: &str,
        base_path: Option<&Path>,
        mode: InScopeBindingMode<'_>,
    ) -> Vec<CompletionItem> {
        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        self.in_scope_binding_completions_with_src(src, mode)
    }

    /// Same as [`in_scope_binding_completions`] but reuses an
    /// already-built [`DslSource`]. Hot-path callers (e.g.
    /// [`value_completions_for_attr`]) build `src` once for several
    /// passes; passing it in avoids re-reading every sibling `.crn`
    /// file per keystroke.
    pub(super) fn in_scope_binding_completions_with_src(
        &self,
        src: DslSource<'_>,
        mode: InScopeBindingMode<'_>,
    ) -> Vec<CompletionItem> {
        let current_binding = match &mode {
            InScopeBindingMode::ValuePosition { current_binding } => *current_binding,
            InScopeBindingMode::PartialReplace { .. } => None,
        };

        let mut items = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut push = |name: String, detail: String| {
            if current_binding.is_some_and(|cb| cb == name) {
                return;
            }
            if !seen.insert(name.clone()) {
                return;
            }
            let mut item = CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(detail),
                ..Default::default()
            };
            match &mode {
                InScopeBindingMode::PartialReplace { range } => {
                    item.text_edit =
                        Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                            range: *range,
                            new_text: name,
                        }));
                }
                InScopeBindingMode::ValuePosition { .. } => {
                    item.insert_text = Some(name);
                }
            }
            items.push(item);
        };

        for (name, rhs) in Self::extract_let_bindings(src) {
            let detail = if rhs.starts_with("upstream_state") {
                "upstream_state binding"
            } else if rhs.starts_with("use ") || rhs.starts_with("use{") {
                "module use"
            } else {
                "binding"
            };
            push(name, detail.to_string());
        }
        for (name, type_hint) in self.extract_argument_parameters(src) {
            let detail = match mode {
                InScopeBindingMode::ValuePosition { .. } => format!("argument: {}", type_hint),
                InScopeBindingMode::PartialReplace { .. } => "argument".to_string(),
            };
            push(name, detail);
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
        let Some(keys) = resolve_upstream_export_keys(binding, source, base_path, &self.schemas)
        else {
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
            .map(|(key, entry)| CompletionItem {
                label: key.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(match &entry.type_expr {
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

    /// Completions for `<binding>.<key>.<partial>` — depth-2 descent
    /// into an `upstream_state` export's declared `TypeExpr`.
    ///
    /// `TypeExpr::Struct` offers every field name with the field type
    /// rendered into `detail`. `TypeExpr::Map` mines the upstream's
    /// literal map for its statically-declared keys (#2490) — only
    /// identifier-shaped keys, since `<binding>.<key>` dot access
    /// can't reach quoted string keys. List / scalar / `Simple` /
    /// `Ref` / `SchemaType` have no named child positions at this
    /// depth and produce an empty list.
    pub(super) fn upstream_state_depth2_dot_completions(
        &self,
        binding: &str,
        key: &str,
        partial: &str,
        source: &str,
        position: Position,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let Some(keys) = resolve_upstream_export_keys(binding, source, base_path, &self.schemas)
        else {
            return Vec::new();
        };
        let Some(entry) = keys.get(key) else {
            return Vec::new();
        };
        let Some(type_expr) = &entry.type_expr else {
            // Untyped or unknown key: we have no fields to descend into.
            return Vec::new();
        };
        let partial_chars = partial.chars().count() as u32;
        let range = Range {
            start: Position {
                line: position.line,
                character: position.character.saturating_sub(partial_chars),
            },
            end: position,
        };

        match type_expr {
            carina_core::parser::TypeExpr::Struct { fields } => {
                let mut items: Vec<CompletionItem> = fields
                    .iter()
                    .map(|(name, field_type)| CompletionItem {
                        label: name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(format!("{}: {}", name, field_type)),
                        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: name.clone(),
                        })),
                        ..Default::default()
                    })
                    .collect();
                items.sort_by(|a, b| a.label.cmp(&b.label));
                items
            }
            carina_core::parser::TypeExpr::Map(value_type) => {
                // Read the upstream's literal map keys from the value
                // already carried alongside the TypeExpr. Empty map →
                // empty completion set. A non-Map value (annotation-only
                // `accounts: map(String)` with no body, or a transient
                // mid-edit upstream where the body hasn't parsed yet)
                // also yields no candidates — silently, so cross-file
                // completion stays online while the user types (#2490).
                let Some(carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::Map(entries),
                )) = &entry.value
                else {
                    return Vec::new();
                };
                let value_type = value_type.to_string();
                let mut items: Vec<CompletionItem> = entries
                    .keys()
                    // Filter to identifier-shaped keys only. Map literals
                    // accept string keys in the grammar (`"prod-1" = ...`),
                    // but `<binding>.<key>` dot access on the consumer
                    // side only works for identifier keys.
                    .filter(|k| carina_core::utils::is_identifier_safe(k))
                    .map(|name| CompletionItem {
                        label: name.clone(),
                        // VARIABLE rather than FIELD — these are runtime
                        // map entries, not type-declared struct fields.
                        kind: Some(CompletionItemKind::VARIABLE),
                        detail: Some(format!("{}: {}", name, value_type)),
                        text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: name.clone(),
                        })),
                        ..Default::default()
                    })
                    .collect();
                items.sort_by(|a, b| a.label.cmp(&b.label));
                items
            }
            _ => {
                // List / scalars: depth-2 names are runtime values, not
                // part of the type. Suggest nothing rather than something
                // potentially invalid (#2041).
                Vec::new()
            }
        }
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

    /// Provide completions for built-in function names. No callers
    /// in production after #2643 (builtin suppression at value
    /// position). Kept available for unit tests that assert label /
    /// signature shape and full coverage of the builtin registry,
    /// and as the hook for a future `<id>(`-triggered completion
    /// surface. Remove once that surface lands or the tests move.
    #[allow(dead_code)]
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

    /// Module-call value-position completion entry point (#2621). The
    /// `exports {}` value position deliberately does NOT funnel through
    /// here — its existing two-layer surface (built-ins + binding refs)
    /// is preserved untouched so this PR's scope stays bounded to
    /// module-call value position.
    ///
    /// Layers, all driven by the same `AttributeType`:
    ///
    /// 1. **Structural / type-driven candidates** —
    ///    [`Self::completions_for_type`] knows that `Bool` →
    ///    `true`/`false`, `Cidr` → CIDR snippets, `Arn` → an ARN
    ///    template, `StringEnum` → the enum members, etc. Lifts unions
    ///    and unwraps lists/maps recursively.
    /// 2. **Built-in function calls** whose return type is assignable
    ///    to the target — `concat`, `join`, `format!`, … filtered by
    ///    [`Self::builtin_function_completions_for_type`].
    /// 3. **Existing-binding references** (`<binding>.<field>`) whose
    ///    leaf attribute is assignable to the target —
    ///    [`Self::resource_ref_completions_for_type`] for resource and
    ///    module-call bindings, plus
    ///    [`Self::upstream_state_ref_completions_for_type`] for
    ///    `upstream_state` exports.
    pub(super) fn value_completions_for_attribute_type(
        &self,
        attr_type: &AttributeType,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut items = self.completions_for_type(attr_type, None);
        items.extend(Self::builtin_function_completions_for_type(attr_type));
        items.extend(self.resource_ref_completions_for_type(attr_type, text, base_path));
        items.extend(self.module_call_binding_ref_completions_for_type(attr_type, text, base_path));
        items.extend(self.upstream_state_ref_completions_for_type(attr_type, text, base_path));
        items
    }

    /// `<upstream_binding>.<exported_name>` references whose declared
    /// `TypeExpr` is compatible with `target`. Mirrors
    /// [`Self::resource_ref_completions_for_type`] but the source of
    /// truth is the upstream project's `exports {}` block (read via
    /// `resolve_upstream_exports_cached`) rather than a local schema.
    /// Shared by every `value_completions_for_attribute_type` caller —
    /// without this the consumer of an upstream `oidc_provider_arn`
    /// wouldn't see `<upstream>.oidc_provider_arn` after `=` (#2621).
    fn upstream_state_ref_completions_for_type(
        &self,
        target: &AttributeType,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        let upstream_sources = super::collect_upstream_state_bindings(src);
        let mut exports_cache: std::collections::HashMap<
            String,
            carina_core::upstream_exports::UpstreamExportEntries,
        > = std::collections::HashMap::new();
        for (binding, source) in &upstream_sources {
            let exports = resolve_upstream_exports_cached(
                binding,
                source,
                base_path,
                &self.schemas,
                &mut exports_cache,
            );
            for (export_name, entry) in exports {
                let Some(export_type) = &entry.type_expr else {
                    continue;
                };
                if !carina_core::validation::is_type_expr_compatible_with_schema(
                    export_type,
                    target,
                ) {
                    continue;
                }
                let full_ref = format!("{}.{}", binding, export_name);
                items.push(CompletionItem {
                    label: full_ref.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!(
                        "export from upstream_state `{}` ({})",
                        binding,
                        self.format_type_expr(export_type)
                    )),
                    insert_text: Some(full_ref),
                    ..Default::default()
                });
            }
        }
        items.sort_by(|a, b| a.label.cmp(&b.label));
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
            let Some(schema) = self.lookup_schema(&resource_type) else {
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

    /// `<module_call_binding>.<exported_name>` references whose
    /// declared `TypeExpr` is compatible with `target`. Walks every
    /// `let X = <module> { ... }` in scope, loads the called module,
    /// and emits an entry per type-compatible export. Only invoked
    /// from the module-call value-position handler — the `exports {}`
    /// value position keeps its pre-#2621 surface untouched. See #2621.
    fn module_call_binding_ref_completions_for_type(
        &self,
        target: &AttributeType,
        text: &str,
        base_path: Option<&Path>,
    ) -> Vec<CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut src_buf = String::new();
        let src = DslSource::resolve_directory(text, base_path, &mut src_buf);
        for (binding_name, rhs) in Self::extract_let_bindings(src) {
            // Resource bindings are handled by
            // `resource_ref_completions_for_type`; skip them so the
            // two helpers don't double-emit.
            if self.extract_resource_type(&rhs).is_some() {
                continue;
            }
            let header = format!(
                "let {} = {} {{",
                binding_name,
                rhs.trim_end_matches('{').trim()
            );
            let Some(module_name) = super::extract_let_module_call_name(header.trim()) else {
                continue;
            };
            let Some(parsed) = self.load_called_module(&module_name, text, base_path) else {
                continue;
            };
            for export in &parsed.export_params {
                let Some(ref export_ty) = export.type_expr else {
                    continue;
                };
                if !carina_core::validation::is_type_expr_compatible_with_schema(export_ty, target)
                {
                    continue;
                }
                let full_ref = format!("{}.{}", binding_name, export.name);
                items.push(CompletionItem {
                    label: full_ref.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!(
                        "Reference to {}'s exported {} ({})",
                        binding_name, export.name, export_ty,
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
        let resource = self
            .schemas
            .iter()
            .map(move |(provider, resource_type, _kind, schema)| {
                let key = if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{}.{}", provider, resource_type)
                };
                let description = schema
                    .description
                    .as_deref()
                    .unwrap_or("Resource reference");
                type_completion_item(key, format!("{} reference", description), replacement_range)
            });

        basic.chain(generic).chain(custom).chain(resource).collect()
    }

    /// Emit completion items for an availability-zone typed sink, in the
    /// unified value form `{provider}.{segments...}.{kind}.{az}` derived
    /// straight from the [`TypeIdentity`] (S2.5a/b convention — see
    /// `notes/specs/2026-05-16-semantic-name-redesign-design.md`).
    ///
    /// For `aws.AvailabilityZone.ZoneName` (identity: provider=`aws`,
    /// segments=`["AvailabilityZone"]`, kind=`"ZoneName"`) the emitted
    /// labels look like `aws.AvailabilityZone.ZoneName.us_east_1a`.
    pub(super) fn availability_zone_completions(
        &self,
        identity: &TypeIdentity,
    ) -> Vec<CompletionItem> {
        // Region prefix is derived from the same provider axis as the
        // sink identity, so the AZ candidates align with the regions the
        // provider actually exposes. A bare (provider: None) AZ identity
        // — should one ever appear — falls back to the unqualified
        // `Region.` prefix.
        let region_prefix = match identity.provider.as_deref() {
            Some(p) => format!("{}.Region.", p),
            None => "Region.".to_string(),
        };
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
                // Identity's Display impl renders the dotted type form
                // (`aws.AvailabilityZone.ZoneName`); the value form
                // simply appends the bare value as the trailing segment.
                let label = format!("{}.{}", identity, az);
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
        dsl_aliases: &[(String, String)],
    ) -> Vec<CompletionItem> {
        match namespace {
            Some(ns) => {
                // Offer the fully-qualified form
                // `<namespace>.<TypeName>.<Variant>`. With `namespace`
                // carrying the dotted `{provider}.{segments...}` prefix
                // (e.g. `"aws.s3.Bucket"`) and `type_name` the enum's
                // kind (e.g. `"VersioningStatus"`), this composes the
                // unified `{provider}.{segments...}.{kind}.{value}`
                // value form established by S2.5a/b. Cases with no
                // segments (e.g. `aws.Region`) render as
                // `aws.Region.<v>` — still the new form, since `segments`
                // is allowed to be empty. The bare tail alone used to
                // leak into sibling-attribute popups via the generic
                // identifier pool; the qualified form is always valid
                // and unambiguous.
                values
                    .iter()
                    .map(|value| {
                        let dsl_value = dsl_aliases
                            .iter()
                            .find_map(|(api, dsl)| (api == value).then(|| dsl.clone()))
                            .unwrap_or_else(|| value.clone());
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

/// Resolve the export name → entry map for an `upstream_state`
/// binding. Returns `None` when `base_path` is missing or the upstream
/// directory has no export entry for `binding`. Both depth-1 and
/// depth-2 dot completion build on this — they only differ in how they
/// consume the resulting map. Map-literal-key completion (depth-2 over
/// a `: map(T) = { ... }` export) reads the keys directly from the
/// entry's `Value`, so there is no second `parse_directory` round-trip
/// per keystroke.
///
/// Use [`resolve_upstream_exports_cached`] instead when calling from
/// `value_completions_for_attr`, which has multiple consumers and pays
/// for re-parsing without a shared cache.
fn resolve_upstream_export_keys(
    binding: &str,
    source: &str,
    base_path: Option<&Path>,
    schemas: &carina_core::schema::SchemaRegistry,
) -> Option<carina_core::upstream_exports::UpstreamExportEntries> {
    let base = base_path?;
    let upstream = carina_core::parser::UpstreamState {
        binding: binding.to_string(),
        source: std::path::PathBuf::from(source),
    };
    let (mut exports, _errors) =
        carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
            base,
            &[upstream],
            &Default::default(),
            Some(schemas),
        );
    exports.remove(binding)
}

/// Cached variant of [`resolve_upstream_export_keys`] for use inside a
/// single `value_completions_for_attr` call. Differences from the
/// non-cached form:
///
/// - **Returns `&HashMap`, never `None`.** A failed resolution (missing
///   `base_path`, missing source directory, parse error) is collapsed
///   to an empty map so callers can iterate uniformly with the
///   resolved-but-empty case. Callers that need to distinguish failure
///   from "resolved with zero exports" must use [`resolve_upstream_export_keys`].
/// - **Caches per binding name.** A failed lookup poisons the cache
///   entry to an empty map for the lifetime of the call, which is
///   intentional — every keystroke creates a fresh cache, and re-trying
///   the same failing parse mid-call would be wasted work.
fn resolve_upstream_exports_cached<'a>(
    binding: &str,
    source: &str,
    base_path: Option<&Path>,
    schemas: &carina_core::schema::SchemaRegistry,
    cache: &'a mut std::collections::HashMap<
        String,
        carina_core::upstream_exports::UpstreamExportEntries,
    >,
) -> &'a carina_core::upstream_exports::UpstreamExportEntries {
    cache.entry(binding.to_string()).or_insert_with(|| {
        resolve_upstream_export_keys(binding, source, base_path, schemas).unwrap_or_default()
    })
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
    schemas: &carina_core::schema::SchemaRegistry,
    exports_cache: &mut std::collections::HashMap<
        String,
        carina_core::upstream_exports::UpstreamExportEntries,
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
            let (mut resolved, _errors) =
                carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
                    base,
                    &[upstream],
                    &Default::default(),
                    Some(schemas),
                );
            // Move the entry out of `resolved` rather than cloning so we
            // don't pay a deep `Value` clone on the per-keystroke path.
            resolved.remove(&iterable.binding).unwrap_or_default()
        });
    let export_type = exports.get(&iterable.export)?.type_expr.as_ref()?;

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
        // here. CustomEnum (carina#3222) is even more specific — the
        // value must be a namespaced enum identifier, which no built-in
        // can synthesise.
        AttributeType::Custom { .. } | AttributeType::CustomEnum { .. } => false,
        // Float, Duration, and Struct attributes — no matching built-in today.
        AttributeType::Float => false,
        AttributeType::Duration => false,
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
        // `Ipv4Cidr`). Carry the PascalCase form as a bare identity.
        name if name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && name.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            Some(AttributeType::Custom {
                identity: Some(carina_core::schema::TypeIdentity::bare(name)),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: legacy_validator(noop_validate),
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

/// Collect every named provider instance binding declared in the
/// directory that `base_path` lives in. A *named* instance is one
/// declared as `let <name> = provider <kind> { ... }`; the kind's
/// default instance (a top-level `provider <kind> { ... }` without
/// `let`) carries no binding name and is excluded.
///
/// The walk is directory-scoped: a binding declared in `providers.crn`
/// must surface as a completion candidate while the user is editing
/// `main.crn` in the same directory ([[feedback_directory_scoped_features]]).
/// `parse_directory` is the right entry point because it merges every
/// sibling `.crn` regardless of whether the directory is a module
/// (input/output shape) or a root config — `load_module` returns
/// `None` for root configs and would miss the bindings entirely.
fn collect_named_provider_instance_bindings(
    provider_ctx: &carina_core::parser::ProviderContext,
    base_path: Option<&Path>,
) -> Vec<String> {
    let Some(base) = base_path else {
        return Vec::new();
    };
    let dir = if base.is_file() {
        base.parent().unwrap_or(base).to_path_buf()
    } else {
        base.to_path_buf()
    };
    let parsed = match carina_core::config_loader::parse_directory(&dir, provider_ctx) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut bindings: Vec<String> = parsed
        .providers
        .iter()
        .filter_map(|p| p.binding.clone())
        .collect();
    bindings.sort();
    bindings.dedup();
    bindings
}
