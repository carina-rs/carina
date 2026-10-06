//! Semantic checks: provider region, module calls, unused bindings, undefined references.

use std::collections::{HashMap, HashSet, VecDeque};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::Document;
use crate::position;
use carina_core::binding_index::{BindingIndex, RefTargetKind, RefType, RefTypeError};
use carina_core::builtins;
use carina_core::config_loader::{DeclarationKind, DirectoryParseResult, DuplicateDeclaration};
use carina_core::parser::{ModuleCall, ParsedFile, TypeExpr};
use carina_core::resource::{ConcreteValue, DeferredValue, Value};
use carina_core::schema::suggest_similar_name;
use carina_core::upstream_exports::UpstreamRefDiagnostic;

use super::{DiagnosticEngine, carina_diagnostic};

/// Directory parse plus the module-expansion error, if expansion stopped.
///
/// Most diagnostics can still use the partially expanded parse after a
/// resolver failure, but concrete module constraint failures must not be
/// discarded: they are actionable editor diagnostics in their own right.
pub(super) struct MergedParseResult {
    pub(super) directory: DirectoryParseResult,
    pub(super) module_error: Option<carina_core::module_resolver::ModuleError>,
    pub(super) module_error_owner: Option<String>,
    pub(super) module_constraint_reports: carina_core::module_resolver::ModuleCallConstraintReports,
}

fn module_call_header_position(line: &str, call: &ModuleCall) -> Option<usize> {
    let module_pattern = format!("{} {{", call.module_name);
    line.match_indices(&module_pattern)
        .find_map(|(byte_pos, _)| {
            let Some(binding) = call.binding_name.as_deref() else {
                return Some(byte_pos);
            };
            let (lhs, _) = line[..byte_pos].rsplit_once('=')?;
            let written_binding = lhs.trim().strip_prefix("let")?.trim();
            (written_binding == binding).then_some(byte_pos)
        })
}

fn module_call_occurrence(module_calls: &[ModuleCall], call_index: usize) -> Option<usize> {
    let call = module_calls.get(call_index)?;
    Some(
        module_calls[..call_index]
            .iter()
            .filter(|candidate| {
                candidate.module_name == call.module_name
                    && candidate.binding_name == call.binding_name
            })
            .count(),
    )
}

/// Match an expanded call site back to an authored call without interpreting
/// its dot-separated instance path. Named calls match their structural
/// binding; anonymous calls match the deterministic instance identity that
/// was recorded during expansion.
fn composition_call_matches(
    expanded: &carina_core::resource::CompositionCall,
    authored: &ModuleCall,
) -> bool {
    if expanded.module_name != authored.module_name {
        return false;
    }

    match (&expanded.binding, &authored.binding_name) {
        (Some(expanded), Some(authored)) => expanded == authored,
        (None, None) => {
            expanded.instance == carina_core::module_resolver::instance_prefix_for_call(authored)
        }
        _ => false,
    }
}

/// Locate the `source = '<expected>'` or `source = "<expected>"` line inside
/// an `upstream_state { ... }` block whose value equals `expected`. Returns
/// `(line, start_col, end_col)` in character columns, positioned over the
/// inner value (quotes excluded).
///
/// Restricting to `upstream_state` blocks avoids false matches against
/// `provider` / `module` blocks that also take a `source` attribute.
fn find_source_value_position(text: &str, expected: &str) -> Option<(u32, u32, u32)> {
    let mut in_upstream_state = false;
    let mut brace_depth: u32 = 0;
    for (line_idx, line) in text.lines().enumerate() {
        // On the opening line, scan only the segment after the first `{`
        // so a single-line form
        //   `let orgs = upstream_state { source = '...' }`
        // also has its `source` attribute parsed without a separate line.
        let (scan_from_byte, is_opening_line) =
            if !in_upstream_state && line.contains("upstream_state") {
                match line.find('{') {
                    Some(idx) => {
                        in_upstream_state = true;
                        brace_depth = 1;
                        (idx + 1, true)
                    }
                    None => continue,
                }
            } else if in_upstream_state {
                (0, false)
            } else {
                continue;
            };

        let segment = &line[scan_from_byte..];

        // Track nested braces so a struct value inside upstream_state doesn't
        // prematurely close the block. On the opening line we've already
        // counted the `{` that opened it.
        brace_depth += segment.matches('{').count() as u32;
        brace_depth = brace_depth.saturating_sub(segment.matches('}').count() as u32);
        // Drop out of state mode at end of line, but still look for `source`
        // on this line first.
        let should_close = brace_depth == 0;

        // `source` can appear after the opening brace on the same line.
        let trimmed = segment.trim_start();
        let found = trimmed.starts_with("source")
            && trimmed
                .split_once('=')
                .map(|(lhs, _)| lhs.trim_end() == "source")
                .unwrap_or(false);

        if found {
            // trimmed: "source = '../x' ..."
            let (_, rhs) = trimmed.split_once('=').unwrap();
            let after_eq = rhs.trim_start();
            if let Some(quote) = after_eq.chars().next().filter(|c| *c == '\'' || *c == '"') {
                let inner = &after_eq[quote.len_utf8()..];
                if let Some(end_byte) = inner.find(quote)
                    && &inner[..end_byte] == expected
                {
                    // Compute columns: prefix up to start of inner string.
                    let trimmed_offset = segment.len() - trimmed.len();
                    let rhs_offset = trimmed.len() - rhs.len();
                    let after_eq_offset = rhs.len() - after_eq.len();
                    let value_byte_in_line = scan_from_byte
                        + trimmed_offset
                        + rhs_offset
                        + after_eq_offset
                        + quote.len_utf8();
                    let start_col = line[..value_byte_in_line].chars().count() as u32;
                    let end_col = start_col + inner[..end_byte].chars().count() as u32;
                    return Some((line_idx as u32, start_col, end_col));
                }
            }
        }

        // Close after processing this line, in case `source = '...'` was on
        // the same line as the closing `}`.
        if should_close {
            in_upstream_state = false;
        }

        // Silence unused-variable warnings for is_opening_line (reserved for
        // potential future multi-block tracking).
        let _ = is_opening_line;
    }
    None
}

/// On the known `line_one_based` of a `for <pat> in <binding>.<attr>` header,
/// find the character columns spanning `<binding>`. `DeferredForExpression`
/// already carries the line; this narrows the scan to that one line.
fn find_for_iterable_binding_column(
    text: &str,
    line_one_based: usize,
    binding: &str,
) -> Option<(u32, u32)> {
    let line = text.lines().nth(line_one_based.saturating_sub(1))?;
    let in_byte = line.find(" in ")?;
    let after_in = &line[in_byte + 4..];
    let after_in_trimmed = after_in.trim_start();
    let ident_end = after_in_trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after_in_trimmed.len());
    if &after_in_trimmed[..ident_end] != binding {
        return None;
    }
    let byte_in_line = in_byte + 4 + (after_in.len() - after_in_trimmed.len());
    let start_col = line[..byte_in_line].chars().count() as u32;
    let end_col = start_col + binding.chars().count() as u32;
    Some((start_col, end_col))
}

/// Binding names declared anywhere in the merged parse. Delegates to
/// [`carina_core::binding_index::BindingNameSet`] so LSP diagnostics
/// stay consistent with the CLI's
/// [`carina_core::parser::check_identifier_scope`] pass (which goes
/// through the same set).
fn collect_known_bindings(merged: &ParsedFile) -> carina_core::binding_index::BindingNameSet {
    carina_core::binding_index::BindingNameSet::from_parsed(merged)
}

/// Whether `deferred` was parsed from the editor's current document.
/// `DeferredForExpression.file` is stamped with the full source path, so we
/// compare by basename to the LSP-supplied `current_file_name`.
fn deferred_in_current_file(
    deferred: &carina_core::parser::DeferredForExpression,
    current_file_name: Option<&str>,
) -> bool {
    let Some(current) = current_file_name else {
        return false;
    };
    let Some(file) = deferred.file.as_deref() else {
        return false;
    };
    std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        == Some(current)
}

impl DiagnosticEngine {
    /// Report anonymous resources that would share an identity scheme derived
    /// from mutable attributes. The core check owns classification and
    /// grouping; this adapter only maps the directory-wide result back onto
    /// declarations in the current document.
    pub(super) fn attribute_derived_anonymous_resource_diagnostics(
        &self,
        doc: &Document,
        current_file: &ParsedFile,
        directory: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let conflicts =
            carina_core::identifier::check_attribute_derived_anonymous_resource_conflicts(
                &directory.resources,
                &self.schemas,
            );
        if conflicts.is_empty() {
            return Vec::new();
        }

        let Ok(authored_spans) =
            carina_core::parser::top_level_anonymous_resource_spans(&doc.text())
        else {
            return Vec::new();
        };
        let mut spans_by_kind = HashMap::<(String, String), VecDeque<_>>::new();
        for authored in authored_spans {
            spans_by_kind
                .entry((authored.provider, authored.resource_type))
                .or_default()
                .push_back(authored.span);
        }

        let mut diagnostics = Vec::new();
        for resource in current_file
            .resources
            .iter()
            .filter(|resource| resource.binding.is_none())
        {
            let key = (
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            );
            let Some(span) = spans_by_kind.get_mut(&key).and_then(VecDeque::pop_front) else {
                continue;
            };

            // The general schema diagnostic below is the actionable error in
            // this situation. Without a schema, the identity-basis classifier
            // returns no classification, so LSP must not infer an
            // attribute-derived conflict while provider schemas are unloaded.
            if self.schemas.get_for(resource).is_none() {
                continue;
            }

            let Some(conflict) = conflicts
                .iter()
                .find(|conflict| conflict.includes(resource, &self.schemas))
            else {
                continue;
            };
            let line = span.start_line.saturating_sub(1) as u32;
            let col = span.start_column.saturating_sub(1) as u32;
            diagnostics.push(carina_diagnostic(
                line,
                col,
                span.end_column.saturating_sub(1) as u32,
                DiagnosticSeverity::ERROR,
                conflict.to_string(),
            ));
        }

        diagnostics
    }

    /// Flag `arguments` blocks placed in a root configuration.
    ///
    /// `arguments` is a module-input declaration; it has no caller in a
    /// root config. Without a `use` site to feed values, its `default`
    /// would silently become a de-facto root variable (issue #2198).
    /// `backend` and `provider` blocks are root-only constructs, so the
    /// presence of either next to `arguments` — possibly in a sibling
    /// `.crn` file — unambiguously identifies a root configuration. The
    /// merged directory parse takes precedence so the signal can come
    /// from a sibling file.
    pub(super) fn check_arguments_in_root(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let is_root = match merged {
            Some(m) => m.backend.is_some() || !m.providers.is_empty(),
            None => parsed.backend.is_some() || !parsed.providers.is_empty(),
        };
        if parsed.arguments.is_empty() || !is_root {
            return Vec::new();
        }

        let mut diagnostics = Vec::new();
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Match the `arguments` keyword as a token, not a prefix —
            // `arguments_foo` (an unrelated identifier) must not match.
            let after_keyword = trimmed.strip_prefix("arguments");
            let is_keyword_block = after_keyword
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(char::is_whitespace));
            if is_keyword_block && trimmed.contains('{') {
                let col = position::leading_whitespace_chars(line);
                let end_col = trimmed
                    .find('{')
                    .map(|p| col + p as u32)
                    .unwrap_or(col + trimmed.len() as u32);
                diagnostics.push(carina_diagnostic(
                    line_idx as u32,
                    col,
                    end_col,
                    DiagnosticSeverity::ERROR,
                    "arguments blocks are only valid inside module definitions, not in root configurations.".to_string(),
                ));
            }
        }

        diagnostics
    }

    /// Check that provider blocks are not defined inside modules.
    ///
    /// The marker that identifies the directory as a module can live in a
    /// sibling `.crn` file, so the merged directory parse takes precedence
    /// when available. Diagnostics remain anchored to provider declarations
    /// in the current buffer.
    pub(super) fn check_provider_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let validation_input = merged.unwrap_or(parsed);
        let Err(message) =
            carina_core::validation::validate_no_provider_in_module(validation_input)
        else {
            return Vec::new();
        };
        if parsed.providers.is_empty() {
            return Vec::new();
        }

        let mut diagnostics = Vec::new();
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("provider ") {
                let col = position::leading_whitespace_chars(line);
                // Highlight "provider <name>" portion
                let end_col = trimmed
                    .find('{')
                    .map(|p| col + p as u32)
                    .unwrap_or(col + trimmed.len() as u32);
                diagnostics.push(carina_diagnostic(
                    line_idx as u32,
                    col,
                    end_col,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                ));
            }
        }

        diagnostics
    }

    /// Check that state blocks are not defined inside modules.
    ///
    /// The marker that identifies the directory as a module can live in a
    /// sibling `.crn` file, so the merged directory parse takes precedence
    /// when available. Diagnostics remain anchored to state block declarations
    /// in the current buffer.
    pub(super) fn check_state_blocks_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let validation_input = merged.unwrap_or(parsed);
        let Err(message) =
            carina_core::validation::validate_no_state_blocks_in_module(validation_input)
        else {
            return Vec::new();
        };
        if parsed.state_blocks.is_empty() {
            return Vec::new();
        }

        let text = doc.text();
        let Ok(spans) = carina_core::parser::top_level_state_block_spans(&text) else {
            return Vec::new();
        };
        // Only anchor spans that correspond one-to-one with the state blocks
        // in this buffer's parsed AST. A future grammar/AST mismatch must not
        // fall back to guessing from keyword-shaped text.
        if spans.len() != parsed.state_blocks.len() {
            return Vec::new();
        }

        spans
            .into_iter()
            .map(|span| {
                carina_diagnostic(
                    span.start_line.saturating_sub(1) as u32,
                    span.start_column.saturating_sub(1) as u32,
                    span.end_column.saturating_sub(1) as u32,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                )
            })
            .collect()
    }

    /// Check that a backend block is not defined inside a module.
    ///
    /// Module classification uses the merged directory parse, while the
    /// grammar-derived span keeps the diagnostic anchored to a top-level
    /// backend declaration in the current buffer.
    pub(super) fn check_backend_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let validation_input = merged.unwrap_or(parsed);
        let Err(message) = carina_core::validation::validate_no_backend_in_module(validation_input)
        else {
            return Vec::new();
        };
        if parsed.backend.is_none() {
            return Vec::new();
        }

        let text = doc.text();
        let Ok(spans) = carina_core::parser::top_level_backend_block_spans(&text) else {
            return Vec::new();
        };
        // `ParsedFile` carries at most one backend. Do not guess an anchor if
        // the grammar spans and the parsed AST stop corresponding one-to-one.
        if spans.len() != 1 {
            return Vec::new();
        }

        spans
            .into_iter()
            .map(|span| {
                carina_diagnostic(
                    span.start_line.saturating_sub(1) as u32,
                    span.start_column.saturating_sub(1) as u32,
                    span.end_column.saturating_sub(1) as u32,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                )
            })
            .collect()
    }

    /// Check that upstream state declarations are not defined inside modules.
    ///
    /// Module classification uses the merged directory parse. The current
    /// buffer's declaration anchors come from top-level `let_binding` grammar
    /// nodes that contain an `upstream_state_expr`.
    pub(super) fn check_upstream_states_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let validation_input = merged.unwrap_or(parsed);
        let Err(message) =
            carina_core::validation::validate_no_upstream_states_in_module(validation_input)
        else {
            return Vec::new();
        };
        if parsed.upstream_states.is_empty() {
            return Vec::new();
        }

        let text = doc.text();
        let Ok(spans) = carina_core::parser::top_level_upstream_state_spans(&text) else {
            return Vec::new();
        };
        if spans.len() != parsed.upstream_states.len() {
            return Vec::new();
        }

        spans
            .into_iter()
            .map(|span| {
                carina_diagnostic(
                    span.start_line.saturating_sub(1) as u32,
                    span.start_column.saturating_sub(1) as u32,
                    span.end_column.saturating_sub(1) as u32,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                )
            })
            .collect()
    }

    /// Check that exports blocks are not defined inside modules.
    ///
    /// Module classification uses the merged directory parse, while
    /// grammar-derived spans keep diagnostics on top-level exports blocks in
    /// the current buffer and exclude same-named nested schema blocks.
    pub(super) fn check_exports_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let validation_input = merged.unwrap_or(parsed);
        let Err(message) = carina_core::validation::validate_no_exports_in_module(validation_input)
        else {
            return Vec::new();
        };
        if parsed.export_params.is_empty() {
            return Vec::new();
        }

        let text = doc.text();
        let Ok(spans) = carina_core::parser::top_level_exports_block_spans(&text) else {
            return Vec::new();
        };
        // No span-count guard belongs here: `export_params` counts parameters,
        // while `spans` counts top-level exports blocks.

        spans
            .into_iter()
            .map(|span| {
                carina_diagnostic(
                    span.start_line.saturating_sub(1) as u32,
                    span.start_column.saturating_sub(1) as u32,
                    span.end_column.saturating_sub(1) as u32,
                    DiagnosticSeverity::ERROR,
                    message.clone(),
                )
            })
            .collect()
    }

    /// Check provider block attributes.
    ///
    /// Runs host-side type-level validation using
    /// `ProviderFactory::provider_config_attribute_types`, then delegates to
    /// `validate_config` for any provider-specific semantic checks. Mirrors
    /// the CLI flow in `carina_core::validation::validate_provider_config`
    /// so fixes to generic DSL format validation take effect in LSP without
    /// rebuilding providers.
    pub(super) fn check_provider_region(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for provider in &parsed.providers {
            let Some(factory) = self.factories.iter().find(|f| f.name() == provider.name) else {
                continue;
            };

            // Host-side type-level validation (catches malformed namespace
            // identifiers, invalid enum values, etc.). Provider configs
            // are flat (no cyclic CFN-style `AttributeType::Ref` today),
            // so a single empty-`defs` `Schema` view is sufficient.
            let attr_types = factory.provider_config_attribute_types();
            let schema_view =
                carina_core::schema::Schema::with_defs(std::collections::BTreeMap::new());
            for (attr_name, value) in &provider.attributes {
                if let Some(attr_type) = attr_types.get(attr_name)
                    && let Err(e) = schema_view.validate_attr(attr_type, value)
                    && let Some((line, col)) =
                        self.find_provider_attr_position(doc, &provider.name, attr_name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + attr_name.chars().count() as u32,
                        DiagnosticSeverity::WARNING,
                        format!("provider {}: {}: {}", provider.name, attr_name, e),
                    ));
                }
            }

            // Provider-specific validation (semantic checks not expressible
            // in the attribute type schema).
            if let Err(e) = factory.validate_config(&provider.attributes)
                && let Some((line, col)) = self.find_provider_region_position(doc, &provider.name)
            {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + 6, // "region"
                    DiagnosticSeverity::WARNING,
                    format!("provider {}: {}", provider.name, e),
                ));
            }
        }
        diagnostics
    }

    /// Check for providers that failed to load and show info-level diagnostics on the provider block.
    pub(super) fn check_unloaded_providers(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        for provider in &parsed.providers {
            let Some(reason) = self.provider_errors.get(&provider.name) else {
                continue;
            };

            // Find the provider block position
            let provider_pattern = format!("provider {}", provider.name);
            for (line_idx, line) in text.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with(&provider_pattern) {
                    let col = position::leading_whitespace_chars(line);
                    let end_col = col + trimmed.find('{').unwrap_or(trimmed.len()) as u32;
                    diagnostics.push(carina_diagnostic(
                        line_idx as u32,
                        col,
                        end_col,
                        DiagnosticSeverity::INFORMATION,
                        format!("Provider '{}' is not loaded: {}", provider.name, reason),
                    ));
                    break;
                }
            }
        }

        diagnostics
    }

    /// Find the position of the region attribute in a provider block
    pub(super) fn find_provider_region_position(
        &self,
        doc: &Document,
        provider_name: &str,
    ) -> Option<(u32, u32)> {
        self.find_provider_attr_position(doc, provider_name, "region")
    }

    /// Find the position of a named attribute in a provider block.
    pub(super) fn find_provider_attr_position(
        &self,
        doc: &Document,
        provider_name: &str,
        attr_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_provider = false;
        let provider_pattern = format!("provider {}", provider_name);

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with(&provider_pattern) {
                in_provider = true;
            }

            if in_provider {
                if trimmed.starts_with(attr_name) {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_provider = false;
                }
            }
        }
        None
    }

    /// Run a directory-scoped parse with the current editor buffer substituted
    /// for its on-disk copy, so diagnostics that need cross-file context
    /// (upstream-state exports, for-iterable bindings) update on keystrokes.
    ///
    /// After the directory parse, run module expansion +
    /// `resolve_provider_unresolved_attributes` + `finalize_provider_configs`
    /// so deferred provider attributes (e.g. `default_tags = mod.tags`) reach
    /// their typed fields and the LSP catches the same errors `carina validate`
    /// would (#2717). A finalize error is non-fatal here: the merged parse is
    /// still returned so identifier-scope and other diagnostics keep running;
    /// the error itself surfaces via [`Self::finalize_provider_diagnostic`].
    pub(super) fn parse_merged_with_buffer(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &std::path::Path,
    ) -> Option<MergedParseResult> {
        let mut overrides: HashMap<String, String> = HashMap::new();
        if let Some(name) = current_file_name {
            overrides.insert(name.to_string(), doc.text());
        }
        let mut result =
            carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
                base_path,
                &self.provider_context,
                &overrides,
            )
            .ok()?;
        // Module expansion is a no-op for configs without `module_call`.
        // Retain an error while continuing with the partially expanded parse:
        // source-level checks still have useful work to do, and concrete
        // value-constraint failures are surfaced at the owning call site.
        let root_imports = result.parsed.uses.clone();
        let module_resolution = carina_core::module_resolver::resolve_modules_with_diagnostics(
            &mut result.parsed,
            base_path,
            &self.provider_context,
        );
        let module_error = module_resolution.error;
        // Nested expansion can fail while a root import is being loaded,
        // before the resolver has a root instance prefix to attach. Identify
        // that import alias on the error path so the LSP can still anchor the
        // diagnostic at its authored root call.
        let module_error_owner = module_error.as_ref().and_then(|expected| {
            root_imports.iter().find_map(|import| {
                let mut resolver = carina_core::module_resolver::ModuleResolver::with_config(
                    base_path,
                    &self.provider_context,
                );
                match resolver.load_module(&import.path) {
                    Err(candidate) if candidate.to_string() == expected.to_string() => {
                        Some(import.alias.clone())
                    }
                    _ => None,
                }
            })
        });
        let _ = carina_core::parser::resolve_provider_unresolved_attributes(
            &mut result.parsed,
            &self.provider_context,
        );
        let _ = carina_core::parser::finalize_provider_configs(&mut result.parsed);
        Some(MergedParseResult {
            directory: result,
            module_error,
            module_error_owner,
            module_constraint_reports: module_resolution.constraint_reports,
        })
    }

    /// Anchor a concrete nested module-constraint resolver failure at the
    /// root call that owns its instance path. Direct-call constraints are
    /// emitted solely by `check_module_calls` from the shared core result;
    /// this fallback owns only failures below that root call.
    pub(super) fn module_resolver_constraint_diagnostic(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        error: &carina_core::module_resolver::ModuleError,
        owner: Option<&str>,
    ) -> Option<Diagnostic> {
        let instance = match error {
            carina_core::module_resolver::ModuleError::Constraint(diagnostic) => {
                diagnostic.instance.as_str()
            }
            _ => return None,
        };

        parsed
            .module_calls
            .iter()
            .enumerate()
            .find_map(|(call_index, call)| {
                let root_instance = carina_core::module_resolver::instance_prefix_for_call(call);
                let owns_failure = instance == root_instance
                    || instance
                        .strip_prefix(&root_instance)
                        .is_some_and(|suffix| suffix.starts_with('.'))
                    || owner.is_some_and(|alias| call.module_name == alias);
                if !owns_failure {
                    return None;
                }
                if instance == root_instance {
                    return None;
                }
                let occurrence = module_call_occurrence(&parsed.module_calls, call_index)?;
                let (line, col, width) = self
                    .find_module_call_position(doc, call, occurrence)
                    .map(|(line, col)| (line, col, call.module_name.chars().count() as u32))?;
                Some(carina_diagnostic(
                    line,
                    col,
                    col + width,
                    DiagnosticSeverity::ERROR,
                    error.to_string(),
                ))
            })
    }

    /// Check tag-key casing against the directory-wide CLI population while
    /// anchoring diagnostics only in the current editor buffer.
    pub(super) fn check_mixed_tag_key_styles(
        &self,
        current_file_name: &str,
        base_path: &std::path::Path,
        merged_result: &DirectoryParseResult,
    ) -> Vec<Diagnostic> {
        // Reuse the exact root sources that fed the merged parse. The shared
        // core assembler also adds the CLI-equivalent module population.
        let tag_keys = carina_core::lint::collect_all_tag_keys(
            &merged_result.source_files,
            &merged_result.parsed,
            base_path,
        );

        let current_path = base_path.join(current_file_name);
        carina_core::lint::find_mixed_tag_key_styles(&tag_keys)
            .into_iter()
            .filter(|warning| warning.file.as_deref() == Some(current_path.as_path()))
            .filter_map(|warning| {
                let line_index = warning.line.checked_sub(1)?;
                let start_col = warning.column as u32;
                let end_col = start_col + warning.key.chars().count() as u32;
                Some(carina_diagnostic(
                    line_index as u32,
                    start_col,
                    end_col,
                    DiagnosticSeverity::WARNING,
                    carina_core::lint::mixed_tag_key_style_message(&warning),
                ))
            })
            .collect()
    }

    /// Render duplicate declaration groups that contain a declaration in the
    /// current buffer. The loader owns provenance; this layer only locates the
    /// corresponding name in the editor text because declaration AST nodes do
    /// not carry spans.
    pub(super) fn duplicate_declaration_diagnostics(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        duplicates: &[DuplicateDeclaration],
    ) -> Vec<Diagnostic> {
        duplicates
            .iter()
            .filter(|duplicate| {
                current_file_name
                    .map(|name| duplicate.occurs_in_file(name))
                    .unwrap_or(true)
            })
            .filter_map(|duplicate| {
                let kind = current_file_name
                    .and_then(|name| duplicate.kind_in_file(name))
                    .or_else(|| {
                        duplicate
                            .occurrences()
                            .first()
                            .map(|occurrence| occurrence.kind())
                    })?;
                let position = match kind {
                    DeclarationKind::Export => {
                        self.find_exports_param_position(doc, duplicate.name())
                    }
                    DeclarationKind::Binding => {
                        self.find_let_binding_position(&doc.text(), duplicate.name())
                    }
                    DeclarationKind::Argument => self.find_declaration_block_param_position(
                        doc,
                        "arguments",
                        duplicate.name(),
                    ),
                    DeclarationKind::ModuleAttribute => self.find_declaration_block_param_position(
                        doc,
                        "attributes",
                        duplicate.name(),
                    ),
                    DeclarationKind::UserFunction => {
                        self.find_user_function_position(doc, duplicate.name())
                    }
                }?;
                Some(carina_diagnostic(
                    position.0,
                    position.1,
                    position.1 + duplicate.name().chars().count() as u32,
                    DiagnosticSeverity::ERROR,
                    duplicate.to_string(),
                ))
            })
            .collect()
    }

    fn find_declaration_block_param_position(
        &self,
        doc: &Document,
        block_name: &str,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        let mut in_block = false;
        for (line_idx, line) in doc.text().lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with(block_name) && trimmed.contains('{') {
                in_block = true;
                continue;
            }
            if in_block && trimmed == "}" {
                in_block = false;
                continue;
            }
            if in_block
                && trimmed.starts_with(param_name)
                && trimmed[param_name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|character| character == ':' || character.is_whitespace())
            {
                return Some((line_idx as u32, position::leading_whitespace_chars(line)));
            }
        }
        None
    }

    fn find_user_function_position(
        &self,
        doc: &Document,
        function_name: &str,
    ) -> Option<(u32, u32)> {
        let prefix = format!("fn {function_name}");
        for (line_idx, line) in doc.text().lines().enumerate() {
            let trimmed = line.trim();
            if let Some(after_name) = trimmed.strip_prefix(&prefix)
                && after_name
                    .chars()
                    .next()
                    .is_some_and(|character| character == '(' || character.is_whitespace())
            {
                let leading = position::leading_whitespace_chars(line);
                return Some((line_idx as u32, leading + 3));
            }
        }
        None
    }

    /// Run finalize-against-a-buffer in isolation so the engine can surface
    /// `default_tags must resolve to a map` etc. as diagnostics. The merged
    /// parse + module-expansion + resolve pair is rebuilt here intentionally:
    /// `parse_merged_with_buffer` swallows the finalize error to keep
    /// identifier-scope diagnostics running, but that error must surface
    /// somewhere (#2717 / #2753).
    pub(super) fn finalize_provider_diagnostic(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &std::path::Path,
    ) -> Option<carina_core::parser::ParseError> {
        let mut overrides: HashMap<String, String> = HashMap::new();
        if let Some(name) = current_file_name {
            overrides.insert(name.to_string(), doc.text());
        }
        let mut merged = carina_core::config_loader::parse_directory_with_overrides(
            base_path,
            &self.provider_context,
            &overrides,
        )
        .ok()?;
        let _ = carina_core::module_resolver::resolve_modules_with_config(
            &mut merged,
            base_path,
            &self.provider_context,
        );
        if let Err(e) = carina_core::parser::resolve_provider_unresolved_attributes(
            &mut merged,
            &self.provider_context,
        ) {
            return Some(e);
        }
        carina_core::parser::finalize_provider_configs(&mut merged).err()
    }

    /// Collect every binding name declared anywhere in `base_path` by
    /// parsing each sibling `.crn` independently. Used as a fallback
    /// when the full directory parse fails (`parse_merged_with_buffer`
    /// returns `None`). A per-file failure is non-fatal — that file's
    /// declarations are skipped. The current document's text replaces
    /// its on-disk copy so unsaved edits are honored.
    ///
    /// Mirrors the merge-success path's
    /// `BindingNameSet::from_parsed`, so the same eight binding kinds
    /// (resources, module-calls, upstream-states, arguments, uses,
    /// user-functions, structural, variables) stay covered when the
    /// merge fails. Without this, an unrelated parse error in one
    /// sibling redlines every cross-file binding as Unknown (#2445).
    pub(super) fn collect_sibling_binding_names(
        &self,
        buffer_text: &str,
        current_file_name: Option<&str>,
        base_path: &std::path::Path,
    ) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(base_path) else {
            return out;
        };
        for file in files {
            let file_name = file.file_name().and_then(|n| n.to_str());
            let content = match (file_name, current_file_name) {
                (Some(name), Some(current)) if name == current => buffer_text.to_string(),
                _ => match std::fs::read_to_string(&file) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
            };
            let Ok(parsed) = carina_core::parser::parse(&content, &self.provider_context) else {
                continue;
            };
            out.extend(
                carina_core::binding_index::BindingNameSet::from_parsed(&parsed)
                    .iter_names()
                    .map(String::from),
            );
        }
        out
    }

    /// Reject references like `orgs.account` whose field isn't declared by
    /// the upstream's `exports { }` block.
    ///
    /// Runs even when the single-file parse fails — common when a `for`
    /// iterates over a binding declared in a sibling file.
    pub(super) fn check_upstream_state_field_references(
        &self,
        doc: &Document,
        merged: &ParsedFile,
        exports: &carina_core::upstream_exports::UpstreamExports,
        resolve_errors: &[carina_core::upstream_exports::UpstreamResolveError],
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        // Broken-upstream diagnostics anchor to the `source = ...` line in
        // the current document (if this file is the one declaring that
        // upstream_state); otherwise they belong to the sibling.
        for err in resolve_errors {
            let source_str = err.source.to_string_lossy();
            let Some((line, col, end_col)) = find_source_value_position(&text, &source_str) else {
                continue;
            };
            diagnostics.push(carina_diagnostic(
                line,
                col,
                end_col,
                DiagnosticSeverity::ERROR,
                err.to_string(),
            ));
        }

        // Multiple `Upstream*Error`s can share the same `binding.field`
        // text (e.g. two `let` bindings that both reference `orgs.bad`).
        // `find_ref_value_position` returns the first occurrence; tracking
        // how many times we've already consumed each ref text lets us
        // anchor later diagnostics at subsequent occurrences instead of
        // stacking them on the first. The same counter is shared between
        // Phase 1 (unknown name) and Phase 2 (type mismatch) so two errors
        // on the same ref don't collide on the first occurrence either.
        let field_errors =
            carina_core::upstream_exports::check_upstream_state_untyped_field_references(
                merged, exports,
            );
        // #1894 (option 2): cross-directory `for`-iterable shape check.
        // Anchored at the same `binding.field` ref occurrence so the
        // editor squiggle lands on the iterable expression.
        let shape_errors = carina_core::upstream_exports::check_upstream_state_for_iterable_shapes(
            merged, exports,
        );
        // #1894 follow-up: cross-directory attribute-access shape check.
        // Anchored at `binding.field` so the squiggle lands at the start
        // of the access chain (the rest of `.foo.bar` is part of the
        // diagnostic message rather than the range).
        let attribute_access_errors =
            carina_core::upstream_exports::check_upstream_state_attribute_access_shape_fallbacks(
                merged, exports,
            );
        let subscript_errors =
            carina_core::upstream_exports::check_upstream_state_subscript_shapes(merged, exports);
        let mut seen_count: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // The four upstream-ref shape/existence checks return distinct concrete types
        // but share `UpstreamRefDiagnostic`; chain them through the
        // trait so adding another check is one extra `chain(...)`.
        self.push_upstream_ref_diagnostics(
            doc,
            &mut seen_count,
            &mut diagnostics,
            field_errors
                .iter()
                .map(|e| e as &dyn UpstreamRefDiagnostic)
                .chain(shape_errors.iter().map(|e| e as &dyn UpstreamRefDiagnostic))
                .chain(
                    attribute_access_errors
                        .iter()
                        .map(|e| e as &dyn UpstreamRefDiagnostic),
                )
                .chain(
                    subscript_errors
                        .iter()
                        .map(|e| e as &dyn UpstreamRefDiagnostic),
                )
                .map(|e| (e.binding(), e.field(), e.diagnostic_message())),
        );

        diagnostics
    }

    /// Emit ERROR diagnostics anchored at each `binding.field` ref occurrence
    /// in the current document. Shared by the Phase 1 (unknown name) and
    /// Phase 2 (type mismatch) `upstream_state` checks — they produce
    /// different errors but anchor the same way and need a shared
    /// `seen_count` to keep their ranges disjoint when both fire on the same
    /// ref.
    fn push_upstream_ref_diagnostics<'a, I>(
        &self,
        doc: &Document,
        seen_count: &mut std::collections::HashMap<String, usize>,
        diagnostics: &mut Vec<Diagnostic>,
        errs: I,
    ) where
        I: IntoIterator<Item = (&'a str, &'a str, String)>,
    {
        for (binding, field, message) in errs {
            let ref_text = format!("{}.{}", binding, field);
            let skip = *seen_count.get(&ref_text).unwrap_or(&0);
            let Some((line, col)) = self.find_ref_value_position_nth(doc, &ref_text, skip) else {
                continue;
            };
            *seen_count.entry(ref_text.clone()).or_insert(0) += 1;
            let end_col = col + ref_text.chars().count() as u32;
            diagnostics.push(carina_diagnostic(
                line,
                col,
                end_col,
                DiagnosticSeverity::ERROR,
                message,
            ));
        }
    }

    /// Flag `for _ in <name>.<attr>` whose root binding `<name>` is not
    /// declared anywhere in the directory-scoped parse.
    ///
    /// The same typo outside a `for` is rejected at single-file parse time,
    /// but for-iterables are deferred until directory merge (they may name a
    /// sibling `upstream_state` or `let`), so this check has to run on the
    /// merged buffer+disk parse too.
    pub(super) fn check_for_iterable_bindings(
        &self,
        doc: &Document,
        merged: &ParsedFile,
        current_file_name: Option<&str>,
    ) -> Vec<Diagnostic> {
        let known = collect_known_bindings(merged);
        let text = doc.text();
        let mut diagnostics = Vec::new();
        // Iterating the deferred list directly (rather than the subset of
        // errors `check_identifier_scope` produces for iterables) keeps a
        // 1:1 mapping between deferred expressions and diagnostics; two
        // sibling files with `for _ in <same>.attr` on the same line
        // would otherwise collide on the error's `(name, line)` key.
        for deferred in &merged.deferred_for_expressions {
            if !deferred_in_current_file(deferred, current_file_name) {
                continue;
            }
            if known.contains(&deferred.iterable_binding) {
                continue;
            }
            let line_zero_based = deferred.line.saturating_sub(1) as u32;
            let (col, end_col) =
                find_for_iterable_binding_column(&text, deferred.line, &deferred.iterable_binding)
                    .unwrap_or_else(|| {
                        // Multi-line `for` headers put the iterable on a later line;
                        // anchor the squiggle at the `for` keyword line so the user
                        // still sees the error.
                        let line_chars = text
                            .lines()
                            .nth(deferred.line.saturating_sub(1))
                            .map(|l| l.chars().count() as u32)
                            .unwrap_or(0);
                        (0, line_chars)
                    });
            // Build the same enriched UndefinedIdentifier the CLI would emit
            // so the editor shows the did-you-mean suggestion and the list of
            // in-scope bindings (#2038).
            let in_scope: Vec<String> = known.iter_names().map(String::from).collect();
            let err = carina_core::parser::ParseError::undefined_identifier(
                deferred.iterable_binding.clone(),
                deferred.line,
                in_scope,
            );
            diagnostics.push(carina_diagnostic(
                line_zero_based,
                col,
                end_col,
                DiagnosticSeverity::ERROR,
                err.to_string(),
            ));
        }
        diagnostics
    }

    /// Flag `upstream_state { source = ... }` paths that do not resolve to an
    /// existing directory relative to the project's base path.
    ///
    /// Mirrors the CLI-side check in `carina-cli::commands::validate` so editors
    /// surface typo'd source paths as squiggles instead of waiting until the
    /// user runs `carina validate` or `carina plan`. Cheap by design — no
    /// canonicalize, no remote state reads.
    ///
    /// Scope: **directory existence only.** Parsing the upstream's `.crn`
    /// files (across every sibling file in the upstream directory) and
    /// checking references against its declared exports is the job of
    /// [`Self::check_upstream_state_field_references`], which consumes the
    /// merged directory parse from [`Self::parse_merged_with_buffer`].
    pub(super) fn check_upstream_state_sources(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        base_path: &std::path::Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let text = doc.text();

        for us in &parsed.upstream_states {
            if base_path.join(&us.source).is_dir() {
                continue;
            }
            let source_str = us.source.to_string_lossy();
            let Some((line, col, end_col)) = find_source_value_position(&text, &source_str) else {
                continue;
            };
            diagnostics.push(carina_diagnostic(
                line,
                col,
                end_col,
                DiagnosticSeverity::ERROR,
                format!(
                    "upstream_state '{}': source '{}' does not exist",
                    us.binding, source_str
                ),
            ));
        }

        diagnostics
    }

    /// Check module calls against imported module definitions
    pub(super) fn check_module_calls(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        imported_modules: &carina_core::module_resolver::ResolvedModuleSignatures,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Check each module call
        for (call_index, call) in parsed.module_calls.iter().enumerate() {
            let call_occurrence = module_call_occurrence(&parsed.module_calls, call_index)
                .expect("enumerated module call has an occurrence");
            if let Some(signature) = imported_modules.get(&call.module_name) {
                let module_args = &signature.arguments;
                // Check for unknown parameters
                for (arg_name, arg_value) in &call.arguments {
                    let matching_arg = module_args.iter().find(|arg| &arg.name == arg_name);

                    if matching_arg.is_none() {
                        if let Some((line, col)) =
                            self.find_module_call_arg_position(doc, call, call_occurrence, arg_name)
                        {
                            // Find similar parameter names for suggestion
                            let suggestion = module_args
                                .iter()
                                .find(|arg| {
                                    arg.name.contains(arg_name) || arg_name.contains(&arg.name)
                                })
                                .map(|arg| format!(". Did you mean '{}'?", arg.name))
                                .unwrap_or_default();

                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                col + arg_name.len() as u32,
                                DiagnosticSeverity::WARNING,
                                format!(
                                    "Unknown parameter '{}' for module '{}'{}",
                                    arg_name, call.module_name, suggestion
                                ),
                            ));
                        }
                        continue;
                    }

                    // Type validation for known parameters
                    let arg = matching_arg.unwrap();
                    if let Some(type_error) =
                        self.validate_module_arg_type(&arg.type_expr, arg_value)
                        && let Some((line, col)) =
                            self.find_module_call_arg_position(doc, call, call_occurrence, arg_name)
                    {
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            col + arg_name.len() as u32,
                            DiagnosticSeverity::WARNING,
                            type_error,
                        ));
                    }
                }

                // Check for missing required parameters
                for arg in module_args {
                    if arg.default.is_none()
                        && !call.arguments.contains_key(&arg.name)
                        && let Some((line, col)) =
                            self.find_module_call_position(doc, call, call_occurrence)
                    {
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            col + call.module_name.len() as u32,
                            DiagnosticSeverity::ERROR,
                            format!(
                                "Missing required parameter '{}' for module '{}'",
                                arg.name, call.module_name
                            ),
                        ));
                    }
                }
            }
        }

        diagnostics
    }

    /// Map resolver-owned constraint outcomes onto their parser-authored
    /// source spans. This layer never rebuilds argument values or re-runs a
    /// constraint expression.
    pub(super) fn module_constraint_diagnostics(
        &self,
        current_file: &std::path::Path,
        reports: &carina_core::module_resolver::ModuleCallConstraintReports,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for report in reports.values() {
            if report.source.file != current_file {
                continue;
            }
            for outcome in &report.outcomes {
                let carina_core::module_resolver::ResolvedModuleConstraintStatus::Failed(failure) =
                    &outcome.status
                else {
                    continue;
                };
                let span = match &failure.kind {
                    carina_core::module_resolver::ModuleConstraintKind::ArgumentValidation {
                        argument,
                    } => report
                        .source
                        .argument_spans
                        .get(argument)
                        .copied()
                        .unwrap_or(report.source.call_span),
                    carina_core::module_resolver::ModuleConstraintKind::Require => {
                        report.source.call_span
                    }
                };
                let line = span.start_line.saturating_sub(1) as u32;
                let col = span.start_column.saturating_sub(1) as u32;
                let width = if span.start_line == span.end_line {
                    span.end_column.saturating_sub(span.start_column) as u32
                } else {
                    1
                };
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + width.max(1),
                    DiagnosticSeverity::ERROR,
                    failure.to_string(),
                ));
            }
        }
        diagnostics
    }

    /// Type-check reference-valued module arguments through the same
    /// lifted source→sink relation and [`BindingIndex`] resolver as the CLI.
    /// Literal arguments remain owned by `check_module_calls` / #2860.
    pub(super) fn check_module_call_ref_types(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        imported_modules: &carina_core::module_resolver::ResolvedModuleSignatures,
        binding_index: &BindingIndex<'_>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::validate_module_call_argument_ref_types_with_bindings(
            &parsed.module_calls,
            imported_modules,
            &HashSet::new(),
            binding_index,
        )
        .into_iter()
        .filter_map(|error| {
            let call = parsed.module_calls.get(error.call_index)?;
            let call_occurrence = module_call_occurrence(&parsed.module_calls, error.call_index)?;
            let argument_position = self
                .find_module_call_arg_position(doc, call, call_occurrence, &error.argument)
                .map(|(line, col)| (line, col, error.argument.chars().count() as u32));
            let call_position = self
                .find_module_call_position(doc, call, call_occurrence)
                .map(|(line, col)| (line, col, call.module_name.chars().count() as u32));
            let (line, col, width) = argument_position.or(call_position)?;
            Some(carina_diagnostic(
                line,
                col,
                col + width,
                DiagnosticSeverity::WARNING,
                error.to_string(),
            ))
        })
        .collect()
    }

    /// Validate all expanded module boundaries, including calls nested below
    /// the directory currently open in the editor.
    pub(super) fn check_composition_ref_types(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        compositions: &[carina_core::resource::Composition],
        binding_index: &BindingIndex<'_>,
    ) -> Vec<Diagnostic> {
        carina_core::validation::validate_composition_ref_types_with_bindings(
            compositions,
            binding_index,
        )
        .into_iter()
        .filter_map(|error| {
            let carina_core::validation::CompositionRefError::ModuleCall(expanded_error) = &error
            else {
                // Output declaration diagnostics are owned by the module file's
                // source-local `check_attributes_blocks` pass. Reporting their
                // expanded copies here would duplicate once per instance and
                // has no declaration range in a calling document.
                return None;
            };

            let immediate_call = parsed
                .module_calls
                .iter()
                .enumerate()
                .find(|(_, call)| composition_call_matches(&expanded_error.call, call));
            let (call_index, authored_call, anchor_argument) = match immediate_call {
                Some((index, call)) => (index, call, true),
                None => {
                    let (index, call) =
                        parsed.module_calls.iter().enumerate().find(|(_, call)| {
                            composition_call_matches(&expanded_error.root_call, call)
                        })?;
                    (index, call, false)
                }
            };
            let call_occurrence = module_call_occurrence(&parsed.module_calls, call_index)?;

            let position = if anchor_argument {
                self.find_module_call_arg_position(
                    doc,
                    authored_call,
                    call_occurrence,
                    &expanded_error.argument,
                )
                .map(|(line, col)| (line, col, expanded_error.argument.chars().count() as u32))
            } else {
                None
            }
            .or_else(|| {
                self.find_module_call_position(doc, authored_call, call_occurrence)
                    .map(|(line, col)| {
                        (line, col, authored_call.module_name.chars().count() as u32)
                    })
            })?;
            let (line, col, width) = position;

            Some(carina_diagnostic(
                line,
                col,
                col + width,
                DiagnosticSeverity::WARNING,
                error.to_string(),
            ))
        })
        .collect()
    }

    /// Validate a module argument value against its expected type.
    pub(super) fn validate_module_arg_type(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
    ) -> Option<String> {
        carina_core::validation::validate_type_expr_value(type_expr, value, &self.provider_context)
    }

    /// Find the position of a module call in the document
    pub(super) fn find_module_call_position(
        &self,
        doc: &Document,
        call: &ModuleCall,
        occurrence: usize,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut remaining = occurrence;

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = module_call_header_position(line, call) {
                if remaining > 0 {
                    remaining -= 1;
                    continue;
                }
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Find the position of an argument in a module call
    pub(super) fn find_module_call_arg_position(
        &self,
        doc: &Document,
        call: &ModuleCall,
        occurrence: usize,
        arg_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_module_call = false;
        let mut remaining = occurrence;

        for (line_idx, line) in text.lines().enumerate() {
            if module_call_header_position(line, call).is_some() {
                if remaining == 0 {
                    in_module_call = true;
                } else {
                    remaining -= 1;
                }
            }

            if in_module_call {
                let trimmed = line.trim_start();
                if trimmed.starts_with(arg_name)
                    && trimmed[arg_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ' ' || c == '=')
                {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                if trimmed == "}" {
                    in_module_call = false;
                }
            }
        }
        None
    }

    /// Format a stream of unused-binding names into LSP diagnostics.
    /// The caller decides which bindings count as unused (derived from
    /// `carina_core::validation::check_unused_bindings` on the merged
    /// parse) and which to anchor the warning on — this helper only
    /// handles the final position lookup and diagnostic construction.
    pub(super) fn unused_binding_diagnostics<I>(
        &self,
        doc: &Document,
        unused_bindings: I,
    ) -> Vec<Diagnostic>
    where
        I: IntoIterator<Item = String>,
    {
        let text = doc.text();
        let mut diagnostics = Vec::new();
        for binding_name in unused_bindings {
            if let Some((line, col)) = self.find_let_binding_position(&text, &binding_name) {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + binding_name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Unused let binding '{}'. Consider using an anonymous resource instead.",
                        binding_name
                    ),
                ));
            }
        }
        diagnostics
    }

    /// Find the position of a `let` binding name in the source text.
    pub(super) fn find_let_binding_position(
        &self,
        text: &str,
        binding_name: &str,
    ) -> Option<(u32, u32)> {
        for (line_idx, line) in text.lines().enumerate() {
            if let Some((name, _)) = crate::let_parse::parse_let_header(line)
                && name == binding_name
            {
                // Find the column of the binding name in the original line
                let let_byte_pos = line.find("let ").unwrap();
                let let_char_pos = position::byte_offset_to_char_offset(line, let_byte_pos);
                let name_col = let_char_pos + 4; // "let " is 4 chars
                return Some((line_idx as u32, name_col));
            }
        }
        None
    }

    /// Extract resource binding names from `src` (variables defined with
    /// `let binding_name = aws...` or `let binding_name = read aws...`).
    /// See [`DslSource`] for the explicit buffer-vs-directory choice.
    pub(super) fn extract_resource_bindings(
        &self,
        src: crate::completion::DslSource<'_>,
    ) -> HashSet<String> {
        let mut bindings = HashSet::new();
        let text = src.merged_text();
        for line in text.lines() {
            if let Some((name, _)) = crate::let_parse::parse_let_header(line) {
                bindings.insert(name.to_string());
            }
        }
        bindings
    }

    /// Extract provider names declared with `provider NAME {` in the current
    /// document text. Used to treat an unresolved `NAME.` prefix as a provider
    /// namespace rather than an undefined `let` binding when the provider is
    /// declared but not yet downloaded (issue #2019).
    pub(super) fn extract_declared_provider_names(&self, text: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("provider") else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(|c: char| c.is_ascii_whitespace()) else {
                continue;
            };
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if name_end == 0 {
                continue;
            }
            let name = &rest[..name_end];
            let after_name = rest[name_end..].trim_start();
            if after_name.starts_with('{') {
                names.insert(name.to_string());
            }
        }
        names
    }

    /// Check attributes blocks for type mismatches and undefined binding references.
    /// Buffer parameters are iterated while directory-wide context provides binding authority.
    pub(super) fn check_attributes_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        binding_index: &BindingIndex<'_>,
        known_bindings: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for attr_param in &parsed.attribute_params {
            if let Some(value) = &attr_param.value {
                // Check for undefined binding references in parser-native shapes
                // (`Value::Deferred(DeferredValue::ResourceRef)` for `name.attr`, `Value::Deferred(DeferredValue::BindingRef)`
                // for bare `name`, since #2847).
                let undefined_binding = match value {
                    Value::Deferred(DeferredValue::ResourceRef { path })
                        if !known_bindings.contains(path.binding()) =>
                    {
                        Some(path.binding().to_string())
                    }
                    Value::Deferred(DeferredValue::BindingRef { binding })
                        if !known_bindings.contains(binding) =>
                    {
                        Some(binding.clone())
                    }
                    // A single-file parse cannot classify an undeclared bare
                    // identifier as a binding, so untyped attribute values use
                    // the parser's enum-identifier fallback until directory
                    // binding authority is available here.
                    Value::Concrete(ConcreteValue::EnumIdentifier(identifier))
                        if attr_param.type_expr.is_none()
                            && !identifier.as_str().contains('.')
                            && !known_bindings.contains(identifier.as_str()) =>
                    {
                        Some(identifier.to_string())
                    }
                    _ => None,
                };
                if let Some(undefined) = undefined_binding
                    && let Some((line, col)) =
                        self.find_attributes_value_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + undefined.len() as u32,
                        DiagnosticSeverity::ERROR,
                        format!(
                            "Undefined resource '{}' in attributes '{}'. Define it with 'let {} = ...'",
                            undefined, attr_param.name, undefined
                        ),
                    ));
                }

                // Concrete/default-value validation is already shared with core.
                // Deferred resource refs intentionally pass through this function;
                // the BindingIndex-backed validator below is their sole authority.
                if let Some(type_expr) = &attr_param.type_expr
                    && !matches!(
                        value,
                        Value::Deferred(DeferredValue::ResourceRef { .. })
                            | Value::Deferred(DeferredValue::BindingRef { .. })
                    )
                    && let Some(type_error) = carina_core::validation::validate_type_expr_value(
                        type_expr,
                        value,
                        &self.provider_context,
                    )
                    && let Some((line, col)) =
                        self.find_attributes_param_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + attr_param.name.len() as u32,
                        DiagnosticSeverity::WARNING,
                        type_error,
                    ));
                }
            }
        }

        // Buffer parses use the bootstrap provider context, so dotted
        // boundary annotations remain `DottedUnresolved` even when schemas
        // are loaded. Resolve a local copy before the BindingIndex-backed
        // directional check. Expanded compositions handle imported-module
        // boundaries; this source-local walk covers the open module's own
        // `attributes {}` declaration.
        let mut resolved_attribute_params = parsed.attribute_params.clone();
        for parameter in &mut resolved_attribute_params {
            if let Some(type_expr) = &parameter.type_expr
                && let Ok(resolved) =
                    carina_core::validation::resolve_type_expr(type_expr, &self.provider_context)
            {
                parameter.type_expr = Some(resolved);
            }
        }
        if let Err(ref_errors) =
            carina_core::validation::validate_attribute_param_ref_types_with_bindings(
                &resolved_attribute_params,
                binding_index,
            )
        {
            // Ownership comes from passing only this buffer's parameters to core;
            // the supplied BindingIndex adds context without adding sibling params.
            for error_msg in ref_errors.lines() {
                let Some(param_name) = error_msg
                    .strip_prefix("attribute '")
                    .and_then(|rest| rest.split('\'').next())
                else {
                    continue;
                };
                if let Some((line, col)) = self.find_attributes_param_position(doc, param_name) {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + param_name.len() as u32,
                        DiagnosticSeverity::WARNING,
                        error_msg.to_string(),
                    ));
                }
            }
        }

        diagnostics
    }

    /// Find the direct-member line for an attributes parameter and return its
    /// name and value columns. Nested map members are excluded by brace depth.
    fn find_attributes_param_line_positions(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32, u32)> {
        let text = doc.text();
        let mut brace_depth = 0usize;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if brace_depth == 0 {
                if trimmed.starts_with("attributes ") && trimmed.contains('{') {
                    brace_depth = line.matches('{').count();
                    brace_depth = brace_depth.saturating_sub(line.matches('}').count());
                }
                continue;
            }

            if brace_depth == 1
                && let Some(after_name) = trimmed.strip_prefix(param_name)
                && (after_name.starts_with(':') || after_name.trim_start().starts_with('='))
                && let Some(eq_byte_pos) = line.find('=')
            {
                let after_eq = &line[eq_byte_pos + 1..];
                let trimmed_after = after_eq.trim_start();
                let ws_after_eq = after_eq.len() - trimmed_after.len();
                let value_col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                    + 1
                    + ws_after_eq as u32;
                return Some((
                    line_idx as u32,
                    position::leading_whitespace_chars(line),
                    value_col,
                ));
            }

            brace_depth = brace_depth.saturating_add(line.matches('{').count());
            brace_depth = brace_depth.saturating_sub(line.matches('}').count());
        }
        None
    }

    /// Find the position of an attributes parameter name in the document.
    fn find_attributes_param_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        self.find_attributes_param_line_positions(doc, param_name)
            .map(|(line, name_col, _)| (line, name_col))
    }

    /// Find the position of the value expression in an attributes parameter line.
    fn find_attributes_value_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        self.find_attributes_param_line_positions(doc, param_name)
            .map(|(line, _, value_col)| (line, value_col))
    }

    /// Validate export parameter values against their type annotations.
    ///
    /// `merged` is the directory-merged parse used to feed inference so
    /// module-call / `upstream_state` bindings declared in sibling
    /// `.crn` files are visible — without it the export inference would
    /// raise spurious `unknown binding` for an `exports.crn` that
    /// references a `let` bound elsewhere in the directory (#2493).
    pub(super) fn check_exports_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for param in &parsed.export_params {
            if let (Some(type_expr), Some(value)) = (&param.type_expr, &param.value)
                && let Some(type_error) = carina_core::validation::validate_type_expr_value(
                    type_expr,
                    value,
                    &self.provider_context,
                )
                && let Some((line, col)) = self.find_exports_param_position(doc, &param.name)
            {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + param.name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    type_error,
                ));
            }
        }

        // Check for unknown type names in type annotations
        for param in &parsed.export_params {
            if let Some(type_expr) = &param.type_expr
                && let Some(error) = self.check_unknown_type_names(type_expr)
                && let Some((line, col)) = self.find_exports_param_position(doc, &param.name)
            {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + param.name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    error,
                ));
            }
        }

        // Schema-level ref type checking for ResourceRef values in exports.
        // `infer_export_params` borrows the parse (avoiding the per-keystroke
        // deep clone the full `apply_inference` would force) so the LSP
        // can derive the post-inference shape without rebuilding the
        // whole `InferredFile`. Use the merged, module-expanded parse when
        // available so sibling-file bindings are visible and module calls
        // appear as inferable `Virtual` compositions (#2493).
        let inference_input = merged.unwrap_or(parsed);
        let (inferred_export_params, inference_errors) =
            carina_core::validation::inference::infer_export_params(inference_input, &self.schemas);
        // Surface inference failures as "type annotation required"
        // diagnostics, anchored at the export name. Errors for
        // exports declared in sibling `.crn` files (visible only via
        // `merged`) won't anchor in this buffer and are skipped here —
        // the buffer that owns the export will surface them on its
        // own pass.
        for inference_error in &inference_errors {
            if let Some((line, col)) = self.find_exports_param_position(doc, &inference_error.name)
            {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + inference_error.name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    carina_core::validation::inference::format_inference_error(
                        &inference_error.name,
                        &inference_error.error,
                    ),
                ));
            }
        }
        // Inference and the Unknown-type existence walk share export-param
        // indices as their diagnostic identity. The LSP inference input is
        // post-expansion, so it can report composition misses through a
        // `Virtual` binding; suppressing those exact indices prevents the
        // composition authority from reporting the same typo again.
        let reported_inference_error_indices: HashSet<usize> =
            inference_errors.iter().map(|error| error.index).collect();
        let export_bindings =
            carina_core::binding_index::BindingIndex::from_parsed(inference_input, &self.schemas);
        if let Err(ref_errors) =
            carina_core::validation::validate_export_param_ref_types_with_bindings(
                &inferred_export_params,
                &export_bindings,
                &reported_inference_error_indices,
            )
        {
            for error_msg in ref_errors.split('\n') {
                if let Some((line, col)) = self.find_ref_error_position(doc, error_msg) {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + 1,
                        DiagnosticSeverity::WARNING,
                        error_msg.to_string(),
                    ));
                }
            }
        }

        diagnostics
    }

    /// Find the position of a ref type error in exports by extracting the param name.
    fn find_ref_error_position(&self, doc: &Document, error_msg: &str) -> Option<(u32, u32)> {
        // Error format: "export 'NAME': type mismatch ..."
        let name = error_msg.strip_prefix("export '")?.split('\'').next()?;
        self.find_exports_param_position(doc, name)
    }

    /// Find the position of an exports parameter name in the document.
    fn find_exports_param_position(&self, doc: &Document, param_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_exports_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("exports") && trimmed.contains('{') {
                in_exports_block = true;
                continue;
            }

            if in_exports_block {
                if trimmed == "}" {
                    in_exports_block = false;
                    continue;
                }

                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':' || c == ' ')
                {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Check if a TypeExpr contains unknown type names.
    fn check_unknown_type_names(&self, type_expr: &TypeExpr) -> Option<String> {
        match type_expr {
            TypeExpr::Simple(name) => {
                if carina_core::parser::BUILTIN_BARE_CUSTOM_TYPES.contains(&name.as_str()) {
                    return None;
                }
                // `Simple` annotations name a built-in custom type; key
                // the validator lookup on the bare structured identity.
                let identity = carina_core::schema::TypeIdentity::bare(
                    carina_core::parser::snake_to_pascal(name),
                );
                if self.provider_context.validators.contains_key(&identity) {
                    return None;
                }
                Some(format!("Unknown type '{name}'."))
            }
            TypeExpr::List(inner) => self.check_unknown_type_names(inner),
            TypeExpr::Map(inner) => self.check_unknown_type_names(inner),
            TypeExpr::Struct { fields } => fields
                .iter()
                .find_map(|(_, ty)| self.check_unknown_type_names(ty)),
            _ => None,
        }
    }

    /// Check for undefined resource references in attribute values
    pub(super) fn check_undefined_references(
        &self,
        text: &str,
        defined_bindings: &HashSet<String>,
        declared_providers: &HashSet<String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            // Look for patterns like "binding_name.property" after "="
            if let Some(eq_byte_pos) = line.find('=') {
                let after_eq = &line[eq_byte_pos + 1..];
                let after_eq_trimmed = after_eq.trim_start();
                // Whitespace after '=' is ASCII spaces, so byte diff == char count
                let whitespace_chars = after_eq.len() - after_eq_trimmed.len();

                // Skip if it's a string literal
                if after_eq_trimmed.starts_with('"') {
                    continue;
                }

                // Skip if it starts with a provider prefix. Include providers
                // declared in the current document (issue #2019) so that an
                // enum reference like `awscc.Region.ap_northeast_1` is not
                // flagged as an undefined `let` binding when the provider is
                // declared but not yet downloaded.
                let is_provider_prefix = self
                    .provider_names
                    .iter()
                    .any(|name| after_eq_trimmed.starts_with(&format!("{}.", name)))
                    || declared_providers
                        .iter()
                        .any(|name| after_eq_trimmed.starts_with(&format!("{}.", name)));
                if is_provider_prefix {
                    continue;
                }

                // Check if it looks like a resource reference: identifier.property
                if let Some(dot_pos) = after_eq_trimmed.find('.') {
                    let identifier = &after_eq_trimmed[..dot_pos];
                    let after_dot = &after_eq_trimmed[dot_pos + 1..];

                    // Extract property name
                    let prop_end = after_dot
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(after_dot.len());
                    let property = &after_dot[..prop_end];

                    // Check if this looks like a resource reference (e.g., main_vpc.id, bucket.arn)
                    if !identifier.is_empty()
                        && !property.is_empty()
                        && identifier.chars().all(|c| c.is_alphanumeric() || c == '_')
                        && identifier.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                    {
                        // Check if the binding is defined
                        if !defined_bindings.contains(identifier) {
                            let col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                                + 1
                                + whitespace_chars as u32;
                            diagnostics.push(carina_diagnostic(
                                line_idx as u32,
                                col,
                                col + identifier.len() as u32,
                                DiagnosticSeverity::ERROR,
                                format!(
                                    "Undefined resource: '{}'. Define it with 'let {} = aws...'",
                                    identifier, identifier
                                ),
                            ));
                        }
                    }
                }
            }
        }

        diagnostics
    }

    /// Check for unknown built-in function calls in parsed resource attributes.
    ///
    /// User-defined functions declared in any sibling `.crn` are excluded
    /// from the unknown-function diagnostic — without this, `fn X(...)` in
    /// `helpers.crn` is flagged as Unknown when called from `main.crn`
    /// (#2442).
    ///
    /// The "known user-fns" set is built defensively:
    ///   * `merged.user_functions` when the directory-merged parse
    ///     succeeded (the cheap, common case); else
    ///   * a per-file walk of `base_path`'s `.crn` siblings via
    ///     `collect_sibling_user_fn_names`. Without this fallback any
    ///     unrelated parse-blocking error elsewhere in the directory
    ///     would redline every correctly-defined sibling `fn` as
    ///     Unknown. Else
    ///   * the buffer's own `parsed.user_functions` (single-file path,
    ///     no `base_path`).
    pub(super) fn check_unknown_functions(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        merged: Option<&ParsedFile>,
        base_path: Option<&std::path::Path>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        // Build a flat name set — the diagnostic only checks
        // `contains(name)`, never inspects the function body. Avoiding
        // `UserFunction` clones keeps the slow path cheap.
        let user_fns: HashSet<String> = match (merged, base_path) {
            (Some(m), _) => m.user_functions.keys().cloned().collect(),
            (None, Some(base)) => self.collect_sibling_user_fn_names(doc, base),
            (None, None) => parsed.user_functions.keys().cloned().collect(),
        };

        for rref in parsed.iter_all_resources() {
            for value in rref.attributes().values() {
                self.collect_unknown_function_diagnostics(doc, value, &user_fns, &mut diagnostics);
            }
        }

        diagnostics
    }

    /// Walk every `.crn` file in `base_path` independently and collect
    /// their user-function names. Used as a fallback when the full
    /// directory parse failed (`parse_directory_with_overrides` returns
    /// `Err` if any sibling has a parse error or the resolver bails).
    /// A per-file failure is non-fatal — we just skip that file. The
    /// open buffer's own user-fn names are taken from `doc` so unsaved
    /// edits are honored.
    fn collect_sibling_user_fn_names(
        &self,
        doc: &Document,
        base_path: &std::path::Path,
    ) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        // Buffer-defined fns first (unsaved edits beat on-disk).
        if let Some(parsed) = doc.parsed() {
            out.extend(parsed.user_functions.keys().cloned());
        }
        let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(base_path) else {
            return out;
        };
        for file in files {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(parsed) = carina_core::parser::parse(&content, &self.provider_context) else {
                continue;
            };
            out.extend(parsed.user_functions.into_keys());
        }
        out
    }

    /// Recursively walk a Value tree to find FunctionCall nodes with unknown names.
    fn collect_unknown_function_diagnostics(
        &self,
        doc: &Document,
        value: &Value,
        user_fns: &HashSet<String>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match value {
            Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
                if !builtins::is_known_builtin(name)
                    && !user_fns.contains(name)
                    && let Some((line, col)) = self.find_function_call_position(doc, name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + name.len() as u32,
                        DiagnosticSeverity::ERROR,
                        format!("Unknown function '{}'", name),
                    ));
                }
                // Also check nested function calls in arguments
                for arg in args {
                    self.collect_unknown_function_diagnostics(doc, arg, user_fns, diagnostics);
                }
            }
            Value::Concrete(ConcreteValue::List(items)) => {
                for item in items {
                    self.collect_unknown_function_diagnostics(doc, item, user_fns, diagnostics);
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values() {
                    self.collect_unknown_function_diagnostics(doc, v, user_fns, diagnostics);
                }
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    if let carina_core::resource::InterpolationPart::Expr(expr) = part {
                        self.collect_unknown_function_diagnostics(doc, expr, user_fns, diagnostics);
                    }
                }
            }
            _ => {}
        }
    }

    /// Find the position of a function call name in the document text.
    fn find_function_call_position(&self, doc: &Document, func_name: &str) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{}(", func_name);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(&pattern) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Check for unknown attributes on resource references (typo detection).
    ///
    /// When a ResourceRef like `igw.internet_gateway_idd` references an attribute
    /// that doesn't exist in the referenced resource's schema, emit a warning
    /// with a "did you mean" suggestion if a similar attribute exists.
    /// Attribute-parameter and module-call refs are excluded because
    /// `check_attributes_blocks` and `check_module_call_ref_types` map their
    /// shared core validation errors instead.
    pub(super) fn check_resource_ref_attributes(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        binding_index: &BindingIndex<'_>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for rref in parsed.iter_all_resources() {
            let attrs = rref.attributes();
            for (attr_name, attr_value) in attrs.iter() {
                if attr_name.starts_with('_') {
                    continue;
                }
                self.collect_ref_attr_diagnostics(doc, attr_value, binding_index, &mut diagnostics);
            }
        }

        diagnostics
    }

    /// Recursively check ResourceRef values for unknown attributes.
    fn collect_ref_attr_diagnostics(
        &self,
        doc: &Document,
        value: &Value,
        binding_index: &BindingIndex<'_>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        value.visit_resource_refs(&mut |path| {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let message = match binding_index.ref_type(path) {
                RefType::UnknownAttribute(RefTypeError::UnknownAttribute {
                    known_attributes,
                    target: RefTargetKind::Schema { resource_type },
                    ..
                }) => {
                    let known_attrs: Vec<&str> =
                        known_attributes.iter().map(String::as_str).collect();
                    let suggestion = suggest_similar_name(attribute_name, &known_attrs)
                        .map(|suggestion| format!(" Did you mean '{}'?", suggestion))
                        .unwrap_or_default();
                    format!(
                        "Unknown attribute '{}' on '{}' (type '{}'){}",
                        attribute_name, binding_name, resource_type, suggestion,
                    )
                }
                RefType::UnknownAttribute(RefTypeError::UnknownAttribute {
                    known_attributes,
                    target: RefTargetKind::Composition,
                    ..
                }) => {
                    let known_attrs: Vec<&str> =
                        known_attributes.iter().map(String::as_str).collect();
                    let suffix = suggest_similar_name(attribute_name, &known_attrs)
                        .map(|suggestion| format!(". Did you mean '{}'?", suggestion))
                        .unwrap_or_else(|| ".".to_string());
                    format!(
                        "Unknown attribute '{}' on '{}'{}",
                        attribute_name, binding_name, suffix,
                    )
                }
                RefType::UnknownAttribute(error) => error.to_string(),
                RefType::Typed(_) | RefType::Unchecked | RefType::UnknownBinding { .. } => return,
            };

            let ref_text = format!("{}.{}", binding_name, attribute_name);
            if let Some((line, col)) = self.find_ref_value_position(doc, &ref_text) {
                // Highlight just the attribute part (after the dot)
                let attr_col = col + binding_name.len() as u32 + 1; // +1 for the dot
                diagnostics.push(carina_diagnostic(
                    line,
                    attr_col,
                    attr_col + attribute_name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    message,
                ));
            }
        });
    }

    /// Locate the first occurrence of `ref_text` as a standalone identifier
    /// chain — so `orgs.acc` won't match inside `orgs.accounts`.
    fn find_ref_value_position(&self, doc: &Document, ref_text: &str) -> Option<(u32, u32)> {
        self.find_ref_value_position_nth(doc, ref_text, 0)
    }

    /// Locate the `skip + 1`-th identifier-chain occurrence of `ref_text`
    /// in the document. Used when the core checker emits multiple errors
    /// with identical `binding.field` strings so each diagnostic lands on
    /// its own source site instead of stacking on the first.
    fn find_ref_value_position_nth(
        &self,
        doc: &Document,
        ref_text: &str,
        skip: usize,
    ) -> Option<(u32, u32)> {
        fn is_ident_cont(c: char) -> bool {
            c.is_ascii_alphanumeric() || c == '_'
        }
        let text = doc.text();
        let mut skipped = 0usize;
        for (line_idx, line) in text.lines().enumerate() {
            let mut search_from = 0;
            while let Some(rel) = line[search_from..].find(ref_text) {
                let byte_pos = search_from + rel;
                let before_ok = byte_pos == 0
                    || line[..byte_pos]
                        .chars()
                        .next_back()
                        .map(|c| !is_ident_cont(c))
                        .unwrap_or(true);
                let after_idx = byte_pos + ref_text.len();
                let after_ok = after_idx >= line.len()
                    || line[after_idx..]
                        .chars()
                        .next()
                        .map(|c| !is_ident_cont(c))
                        .unwrap_or(true);
                if before_ok && after_ok {
                    if skipped == skip {
                        return Some((
                            line_idx as u32,
                            position::byte_offset_to_char_offset(line, byte_pos),
                        ));
                    }
                    skipped += 1;
                }
                search_from = byte_pos + ref_text.len();
            }
        }
        None
    }

    /// Walk the buffer line-by-line and emit a `WARNING` diagnostic for every
    /// empty `${}` interpolation inside a double-quoted string. The parser
    /// accepts the empty form (so the rest of the AST stays intact and other
    /// diagnostics keep running), but a buffer that ships with literal `${}`
    /// will produce a meaningless value at apply time — so the user wants a
    /// hint that the placeholder is unfilled. See #2480.
    ///
    /// Text scan rather than AST walk: the parser stamps the empty case as
    /// `Value::Deferred(DeferredValue::Unknown(UnknownReason::EmptyInterpolation))` but discards the
    /// source span, so the diagnostic range has to come from a source-level
    /// pass anyway.
    pub(super) fn check_empty_interpolations(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let mut out = Vec::new();
        for (line_idx, line) in text.lines().enumerate() {
            // 99% of lines have no `$` — bail before allocating chars.
            if !line.as_bytes().contains(&b'$') {
                continue;
            }
            let chars: Vec<char> = line.chars().collect();
            let mut i = 0;
            let mut in_double_quoted = false;
            while i < chars.len() {
                let c = chars[i];
                if !in_double_quoted {
                    if c == '"' {
                        in_double_quoted = true;
                    } else if c == '\'' {
                        // Skip past the single-quoted string entirely so a
                        // `${}` inside it (which is a literal in this DSL)
                        // doesn't trigger the diagnostic.
                        i += 1;
                        while i < chars.len() && chars[i] != '\'' {
                            i += 1;
                        }
                    }
                    i += 1;
                    continue;
                }

                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    in_double_quoted = false;
                    i += 1;
                    continue;
                }
                if c == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    let start = i;
                    // Brace-balance the interpolation body so `${ {}.a }`
                    // closes at the matching `}`, not the first one.
                    let mut depth = 1usize;
                    let mut j = i + 2;
                    let mut only_whitespace = true;
                    while j < chars.len() && depth > 0 {
                        match chars[j] {
                            '\\' if j + 1 < chars.len() => {
                                only_whitespace = false;
                                j += 2;
                                continue;
                            }
                            '{' => {
                                depth += 1;
                                only_whitespace = false;
                            }
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    j += 1;
                                    break;
                                }
                                only_whitespace = false;
                            }
                            ch if !ch.is_whitespace() => only_whitespace = false,
                            _ => {}
                        }
                        j += 1;
                    }
                    if depth == 0 && only_whitespace {
                        out.push(carina_diagnostic(
                            line_idx as u32,
                            start as u32,
                            j as u32,
                            DiagnosticSeverity::WARNING,
                            "empty interpolation `${}` — fill in the expression or remove it"
                                .to_string(),
                        ));
                    }
                    i = j;
                    continue;
                }
                i += 1;
            }
        }
        out
    }

    /// Mirror `validate_depends_on` errors/warnings in the editor.
    /// Per-diagnostic anchoring uses the structured `binding_name` /
    /// `dep_name` hints carried by `DependsOnDiagnostic` (#2874) to
    /// resolve a precise span. Falls back to whole-buffer scan only
    /// for diagnostics that have no hints (e.g. cycle errors).
    pub(super) fn check_depends_on(&self, doc: &Document, parsed: &ParsedFile) -> Vec<Diagnostic> {
        use carina_core::validation::depends_on::{Severity, validate_depends_on};
        let diags = validate_depends_on(parsed);
        if diags.is_empty() {
            return Vec::new();
        }
        let text = doc.text();
        diags
            .into_iter()
            .map(|d| {
                let severity = match d.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                };
                let (line, col, end_col) =
                    anchor_for_diagnostic(&text, d.binding_name.as_deref(), d.dep_name.as_deref());
                carina_diagnostic(line, col, end_col, severity, d.message)
            })
            .collect()
    }

    /// Lint `wait <target> { ... }` declarations by delegating to the
    /// shared `carina_core::validation::wait` pass. The diagnostics
    /// produced here use the same wording as `carina validate`; the
    /// LSP just adds source anchors via `wait_target_anchor` /
    /// `wait_until_attr_anchor`.
    pub(super) fn check_wait_bindings(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let diags = carina_core::validation::wait::validate_wait_bindings(parsed, &self.schemas);
        if diags.is_empty() {
            return Vec::new();
        }
        let text = doc.text();
        diags
            .into_iter()
            .map(|d| {
                let (line, col, end_col) = match d.attribute.as_deref() {
                    Some(attr) => wait_until_attr_anchor(&text, &d.binding_name, &d.target, attr),
                    None => wait_target_anchor(&text, &d.binding_name, &d.target),
                };
                carina_diagnostic(line, col, end_col, DiagnosticSeverity::ERROR, d.message)
            })
            .collect()
    }

    /// Lint chained references to schema-flagged "deferred-populate"
    /// attributes that lack a synchronizing `wait` block. Delegates to
    /// `carina_core::validation::deferred_populate` so the LSP and
    /// `carina validate` produce identical wording (carina#3034).
    ///
    /// Source anchor: the attribute key on the *holding* resource
    /// (e.g. `resource_records` on the route53.RecordSet). The full
    /// path of the offending ref (`cert.dvo[0].rrv`) appears verbatim
    /// in the message, so the user sees both the holder and the
    /// upstream they need to wait on.
    pub(super) fn check_deferred_populate_refs(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let diags = carina_core::validation::deferred_populate::validate_deferred_populate_refs(
            parsed,
            &self.schemas,
        );
        if diags.is_empty() {
            return Vec::new();
        }
        let text = doc.text();
        diags
            .into_iter()
            .map(|d| {
                let (line, col, end_col) =
                    deferred_populate_anchor(&text, d.holder_binding.as_deref(), &d.attribute_key);
                carina_diagnostic(line, col, end_col, DiagnosticSeverity::ERROR, d.message)
            })
            .collect()
    }
}

/// Anchor for a deferred-populate diagnostic: the attribute key as it
/// appears on the holding resource. Falls back to `(0, 0, 0)` so the
/// editor still surfaces the message even without a precise span.
fn deferred_populate_anchor(
    text: &str,
    holder_binding: Option<&str>,
    attribute_key: &str,
) -> (u32, u32, u32) {
    let lines: Vec<&str> = text.lines().collect();
    // Scope the forward scan to the holder binding's `let <binding> =`
    // line when available, so identical attribute keys on different
    // resources don't all anchor to the first occurrence.
    let start = holder_binding
        .and_then(|b| find_binding_line(&lines, b))
        .unwrap_or(0);
    for (i, line) in lines.iter().enumerate().skip(start) {
        if let Some((col, end_col)) = find_word_on_line(line, attribute_key) {
            return (i as u32, col, end_col);
        }
    }
    (0, 0, 0)
}

/// Find the source anchor for a wait diagnostic. Returns
/// `(line, col, end_col)` for the target identifier inside the
/// `wait <target>` line, falling back to the wait keyword's column when
/// the target can't be found verbatim.
fn wait_target_anchor(text: &str, binding: &str, target: &str) -> (u32, u32, u32) {
    let lines: Vec<&str> = text.lines().collect();
    // Find `let <binding> =` to scope the forward scan.
    let start = find_binding_line(&lines, binding).unwrap_or(0);
    for (i, line) in lines.iter().enumerate().skip(start) {
        if !line.contains("wait ") {
            continue;
        }
        if let Some((col, end_col)) = find_word_on_line(line, target) {
            return (i as u32, col, end_col);
        }
        // Fall back to the `wait` keyword span.
        if let Some((col, end_col)) = find_word_on_line(line, "wait") {
            return (i as u32, col, end_col);
        }
    }
    (0, 0, 0)
}

/// Find the source anchor for an `until = <target>.<attr> ...`
/// attribute reference inside a wait block.
fn wait_until_attr_anchor(text: &str, binding: &str, target: &str, attr: &str) -> (u32, u32, u32) {
    let lines: Vec<&str> = text.lines().collect();
    let start = find_binding_line(&lines, binding).unwrap_or(0);
    let dotted = format!("{}.{}", target, attr);
    for (i, line) in lines.iter().enumerate().skip(start) {
        if !line.contains("until") {
            continue;
        }
        if let Some(byte_pos) = line.find(&dotted) {
            let col = line[..byte_pos].chars().count() as u32;
            let end_col = col + dotted.chars().count() as u32;
            return (i as u32, col, end_col);
        }
    }
    wait_target_anchor(text, binding, target)
}

/// Resolve the source anchor for a depends_on diagnostic.
///
/// Strategy:
/// 1. Find the `let <binding_name> =` line for the offending binding
///    (or the resource's opening line if unbound).
/// 2. From there, scan forward for the `directives` block.
/// 3. If `dep_name` is given, find the matching identifier inside
///    the `depends_on = [...]` list. Otherwise anchor at `depends_on`.
/// 4. Fall back to first-`depends_on` whole-buffer scan when the
///    diagnostic carries no hints (cycle errors), or `(0, 0, 0)` if
///    even that fails.
fn anchor_for_diagnostic(
    text: &str,
    binding_name: Option<&str>,
    dep_name: Option<&str>,
) -> (u32, u32, u32) {
    let lines: Vec<&str> = text.lines().collect();

    let binding_line = binding_name.and_then(|b| find_binding_line(&lines, b));
    let search_start = binding_line.unwrap_or(0);

    let directives_line = find_token_after(&lines, search_start, "directives");
    let depends_on_line = directives_line
        .and_then(|d| find_token_after(&lines, d, "depends_on"))
        .or_else(|| find_token_after(&lines, search_start, "depends_on"))
        .or_else(|| find_token_after(&lines, 0, "depends_on"));

    let Some(dep_on_idx) = depends_on_line else {
        return (0, 0, 0);
    };

    if let Some(dep) = dep_name
        && let Some((line_idx, col, end_col)) =
            find_word_in_lines(&lines, dep_on_idx, dep_on_idx + 5, dep)
    {
        return (line_idx, col, end_col);
    }

    if let Some((col, end_col)) = find_word_on_line(lines[dep_on_idx], "depends_on") {
        return (dep_on_idx as u32, col, end_col);
    }
    (dep_on_idx as u32, 0, 0)
}

/// Find the line index of `let <binding_name> = ...`.
fn find_binding_line(lines: &[&str], binding: &str) -> Option<usize> {
    let needle = format!("let {} ", binding);
    let needle_eq = format!("let {}=", binding);
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&needle) || trimmed.starts_with(&needle_eq) {
            return Some(i);
        }
    }
    None
}

/// Find the line index >= `from` containing `token` as a whole word.
fn find_token_after(lines: &[&str], from: usize, token: &str) -> Option<usize> {
    for (offset, line) in lines.iter().enumerate().skip(from) {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if find_word_on_line(line, token).is_some() {
            return Some(offset);
        }
    }
    None
}

/// Find a whole-word `token` somewhere in any of `lines[from..to.min(end)]`.
fn find_word_in_lines(
    lines: &[&str],
    from: usize,
    to: usize,
    token: &str,
) -> Option<(u32, u32, u32)> {
    let end = to.min(lines.len());
    for (offset, line) in lines.iter().enumerate().take(end).skip(from) {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((col, end_col)) = find_word_on_line(line, token) {
            return Some((offset as u32, col, end_col));
        }
    }
    None
}

/// Find a whole-word `token` on a single line (character-column).
fn find_word_on_line(line: &str, token: &str) -> Option<(u32, u32)> {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(token) {
        let byte_col = search_from + rel;
        let after = byte_col + token.len();
        let prev_ok = byte_col == 0
            || !line.as_bytes()[byte_col - 1].is_ascii_alphanumeric()
                && line.as_bytes()[byte_col - 1] != b'_';
        let next_ok = after == line.len()
            || !line.as_bytes()[after].is_ascii_alphanumeric() && line.as_bytes()[after] != b'_';
        if prev_ok && next_ok {
            let prefix = &line[..byte_col];
            let col = prefix.chars().count() as u32;
            return Some((col, col + token.chars().count() as u32));
        }
        search_from = after;
    }
    None
}

#[cfg(test)]
mod depends_on_anchor_tests {
    use super::*;

    #[test]
    fn anchor_for_diagnostic_finds_specific_element_by_dep_name() {
        let text = "let role = aws.iam.Role { role_name = \"r\" }\nlet bucket = aws.s3.Bucket {\n  bucket_name = \"x\"\n  directives {\n    depends_on = [role, kms]\n  }\n}\n";
        let (line, col, end_col) = anchor_for_diagnostic(text, Some("bucket"), Some("kms"));
        assert_eq!(line, 4, "should anchor on the depends_on list line");
        let line_text = text.lines().nth(4).unwrap();
        let kms_col = line_text.find("kms").unwrap() as u32;
        assert_eq!(col, kms_col, "should point at start of `kms`");
        assert_eq!(end_col, kms_col + 3);
    }

    #[test]
    fn anchor_for_diagnostic_distinguishes_two_resources() {
        let text = "let role = aws.iam.Role { role_name = \"r\" }\nlet bucket1 = aws.s3.Bucket {\n  bucket_name = \"x1\"\n  directives { depends_on = [bad1] }\n}\nlet bucket2 = aws.s3.Bucket {\n  bucket_name = \"x2\"\n  directives { depends_on = [bad2] }\n}\n";
        let a = anchor_for_diagnostic(text, Some("bucket1"), Some("bad1"));
        let b = anchor_for_diagnostic(text, Some("bucket2"), Some("bad2"));
        assert_ne!(
            a.0, b.0,
            "two resources must anchor on different lines: {a:?} {b:?}"
        );
        assert_eq!(a.0, 3, "bucket1 anchor on line 3");
        assert_eq!(b.0, 7, "bucket2 anchor on line 7");
    }

    #[test]
    fn anchor_for_diagnostic_falls_back_when_no_hints() {
        let text = "let bucket = aws.s3.Bucket {\n  directives { depends_on = [role] }\n}\n";
        let (line, _, _) = anchor_for_diagnostic(text, None, None);
        assert_eq!(line, 1, "fallback should land on the depends_on line");
    }

    #[test]
    fn find_word_on_line_word_boundary() {
        let line = "let depends_on_role = something";
        assert!(find_word_on_line(line, "depends_on").is_none());
        let line2 = "    depends_on = [role]";
        assert!(find_word_on_line(line2, "depends_on").is_some());
    }
}
