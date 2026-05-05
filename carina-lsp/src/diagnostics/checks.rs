//! Semantic checks: provider region, module calls, unused bindings, undefined references.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::Document;
use crate::position;
use carina_core::builtins;
use carina_core::parser::{ArgumentParameter, ParsedFile, TypeExpr};
use carina_core::resource::{Resource, Value};
use carina_core::schema::{ResourceSchema, suggest_similar_name};
use carina_core::upstream_exports::UpstreamRefDiagnostic;

use super::{DiagnosticEngine, carina_diagnostic};

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

/// Check if a string looks like an unresolved cross-file reference ("binding.attribute").
fn is_dot_notation_ref(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 2
        && !s.contains(' ')
        && !s.starts_with('/')
        && parts[0]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && parts[1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl DiagnosticEngine {
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
    pub(super) fn check_provider_in_module(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
        if !is_module || parsed.providers.is_empty() {
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
                    "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
                ));
            }
        }

        diagnostics
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
            // identifiers, invalid enum values, etc.).
            let attr_types = factory.provider_config_attribute_types();
            for (attr_name, value) in &provider.attributes {
                if let Some(attr_type) = attr_types.get(attr_name)
                    && let Err(e) = attr_type.validate(value)
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
    pub(super) fn parse_merged_with_buffer(
        &self,
        doc: &Document,
        current_file_name: Option<&str>,
        base_path: &std::path::Path,
    ) -> Option<ParsedFile> {
        let mut overrides: HashMap<String, String> = HashMap::new();
        if let Some(name) = current_file_name {
            overrides.insert(name.to_string(), doc.text());
        }
        carina_core::config_loader::parse_directory_with_overrides(
            base_path,
            &self.provider_context,
            &overrides,
        )
        .ok()
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
        let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(&base_path.to_path_buf())
        else {
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
        base_path: &std::path::Path,
    ) -> Vec<Diagnostic> {
        let (exports, resolve_errors) =
            carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
                base_path,
                &merged.upstream_states,
                &self.provider_context,
                Some(&self.schemas),
            );

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
            carina_core::upstream_exports::check_upstream_state_field_references(merged, &exports);
        let type_errors = carina_core::upstream_exports::check_upstream_state_field_types(
            merged,
            &exports,
            &self.schemas,
        );
        // #1894 (option 2): cross-directory `for`-iterable shape check.
        // Anchored at the same `binding.field` ref occurrence so the
        // editor squiggle lands on the iterable expression.
        let shape_errors = carina_core::upstream_exports::check_upstream_state_for_iterable_shapes(
            merged, &exports,
        );
        // #1894 follow-up: cross-directory attribute-access shape check.
        // Anchored at `binding.field` so the squiggle lands at the start
        // of the access chain (the rest of `.foo.bar` is part of the
        // diagnostic message rather than the range).
        let attribute_access_errors =
            carina_core::upstream_exports::check_upstream_state_attribute_access_shapes(
                merged, &exports,
            );
        let subscript_errors =
            carina_core::upstream_exports::check_upstream_state_subscript_shapes(merged, &exports);
        let mut seen_count: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // The five upstream-ref checks return distinct concrete types
        // but share `UpstreamRefDiagnostic`; chain them through the
        // trait so adding a sixth check is one extra `chain(...)`.
        self.push_upstream_ref_diagnostics(
            doc,
            &mut seen_count,
            &mut diagnostics,
            field_errors
                .iter()
                .map(|e| e as &dyn UpstreamRefDiagnostic)
                .chain(type_errors.iter().map(|e| e as &dyn UpstreamRefDiagnostic))
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
        base_path: &std::path::Path,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Build a map of imported modules: alias -> argument parameters
        let mut imported_modules: HashMap<String, Vec<ArgumentParameter>> = HashMap::new();

        for import in &parsed.uses {
            let module_path = base_path.join(&import.path);
            if let Some(module_parsed) = carina_core::module_resolver::load_module(&module_path) {
                imported_modules.insert(import.alias.clone(), module_parsed.arguments);
            }
        }

        // Check each module call
        for call in &parsed.module_calls {
            if let Some(module_args) = imported_modules.get(&call.module_name) {
                // Check for unknown parameters
                for (arg_name, arg_value) in &call.arguments {
                    let matching_arg = module_args.iter().find(|arg| &arg.name == arg_name);

                    if matching_arg.is_none() {
                        if let Some((line, col)) =
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
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
                            self.find_module_call_arg_position(doc, &call.module_name, arg_name)
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
                            self.find_module_call_position(doc, &call.module_name)
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
        module_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{} {{", module_name);

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

    /// Find the position of an argument in a module call
    pub(super) fn find_module_call_arg_position(
        &self,
        doc: &Document,
        module_name: &str,
        arg_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_module_call = false;
        let module_pattern = format!("{} {{", module_name);

        for (line_idx, line) in text.lines().enumerate() {
            if line.contains(&module_pattern) {
                in_module_call = true;
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
    pub(super) fn check_attributes_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Collect defined binding names from parsed resources, including
        // bindings declared inside for-body templates.
        let mut defined_bindings: HashSet<String> = HashSet::new();
        for (_ctx, resource) in parsed.iter_all_resources() {
            if let Some(ref binding_name) = resource.binding {
                defined_bindings.insert(binding_name.clone());
            }
        }

        for attr_param in &parsed.attribute_params {
            if let Some(value) = &attr_param.value {
                // Check for undefined binding references
                if let Value::ResourceRef { path } = value
                    && !defined_bindings.contains(path.binding())
                    && let Some((line, col)) =
                        self.find_attributes_value_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + path.binding().len() as u32,
                        DiagnosticSeverity::ERROR,
                        format!(
                            "Undefined resource '{}' in attributes '{}'. Define it with 'let {} = ...'",
                            path.binding(), attr_param.name, path.binding()
                        ),
                    ));
                }

                // Type validation (only when explicit type annotation is present)
                if let Some(ref type_expr) = attr_param.type_expr
                    && let Some(type_error) =
                        self.validate_attributes_type(type_expr, value, &HashMap::new())
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

                // ResourceRef type validation: check that the referenced attribute's
                // schema type is compatible with the declared TypeExpr
                if let Some(ref type_expr) = attr_param.type_expr
                    && let Value::ResourceRef { path } = value
                    && let Some(ref_type_error) = self.check_attribute_ref_type(
                        type_expr,
                        path,
                        &attr_param.name,
                        &parsed.resources,
                    )
                    && let Some((line, col)) =
                        self.find_attributes_value_position(doc, &attr_param.name)
                {
                    diagnostics.push(carina_diagnostic(
                        line,
                        col,
                        col + path.to_dot_string().len() as u32,
                        DiagnosticSeverity::WARNING,
                        ref_type_error,
                    ));
                }
            }
        }

        diagnostics
    }

    /// Check a ResourceRef value against the declared TypeExpr by looking up
    /// the referenced resource's schema attribute type.
    fn check_attribute_ref_type(
        &self,
        type_expr: &TypeExpr,
        path: &carina_core::resource::AccessPath,
        param_name: &str,
        resources: &[carina_core::resource::Resource],
    ) -> Option<String> {
        let expected_type = match type_expr {
            TypeExpr::Simple(name) => name.as_str(),
            _ => return None,
        };

        let ref_binding = path.binding();
        let ref_attr = path.attribute();

        // Find the referenced resource
        let ref_resource = resources
            .iter()
            .find(|r| r.binding.as_deref() == Some(ref_binding))?;
        let ref_schema = self.schemas.get_for(ref_resource)?;
        let ref_attr_schema = ref_schema.attributes.get(ref_attr)?;
        let ref_type_name = ref_attr_schema.attr_type.type_name();
        let ref_type_snake = carina_core::parser::pascal_to_snake(&ref_type_name);

        if ref_type_snake == expected_type {
            return None;
        }

        Some(format!(
            "attribute '{}': type mismatch: expected {}, got {} (from {}.{})",
            param_name, expected_type, ref_type_snake, ref_binding, ref_attr
        ))
    }

    /// Validate an attributes value against its declared type.
    ///
    /// Skips ResourceRef values (type is resolved at runtime), then delegates all
    /// validation to `carina_core::validation::validate_type_expr_value`.
    fn validate_attributes_type(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
        sibling_bindings: &HashMap<String, String>,
    ) -> Option<String> {
        // ResourceRef is always allowed (type is resolved at runtime)
        if matches!(value, Value::ResourceRef { .. }) {
            return None;
        }

        self.validate_type_with_ref_awareness(type_expr, value, sibling_bindings)
    }

    /// Type-check a value against a TypeExpr, resolving cross-file references
    /// against sibling bindings and schemas for proper type checking.
    fn validate_type_with_ref_awareness(
        &self,
        type_expr: &TypeExpr,
        value: &Value,
        sibling_bindings: &HashMap<String, String>,
    ) -> Option<String> {
        match (type_expr, value) {
            // Cross-file ref: look up schema type via sibling bindings
            (_, Value::String(s)) if is_dot_notation_ref(s) => {
                let parts: Vec<&str> = s.split('.').collect();
                let binding = parts[0];
                let attr = parts[1];

                // Look up resource type from sibling bindings
                if let Some(resource_type) = sibling_bindings.get(binding) {
                    // Look up attribute schema type
                    let (provider, rt) = resource_type
                        .split_once('.')
                        .unwrap_or(("", resource_type.as_str()));
                    if let Some(schema) = self
                        .schemas
                        .get(provider, rt, carina_core::schema::SchemaKind::Managed)
                        .or_else(|| {
                            self.schemas.get(
                                provider,
                                rt,
                                carina_core::schema::SchemaKind::DataSource,
                            )
                        })
                        && let Some(attr_schema) = schema.attributes.get(attr)
                    {
                        let ref_type = &attr_schema.attr_type;
                        if !carina_core::validation::is_type_expr_compatible_with_schema(
                            type_expr, ref_type,
                        ) {
                            return Some(format!(
                                "type mismatch: expected {}, got {} (from {}.{})",
                                type_expr,
                                ref_type.type_name(),
                                binding,
                                attr
                            ));
                        }
                        return None;
                    }
                }
                // Can't resolve: skip (will be validated by CLI)
                None
            }
            // List: recurse into elements
            (TypeExpr::List(inner), Value::List(items)) => {
                for (i, item) in items.iter().enumerate() {
                    if let Some(e) =
                        self.validate_type_with_ref_awareness(inner, item, sibling_bindings)
                    {
                        return Some(format!("Element {}: {}", i, e));
                    }
                }
                None
            }
            // Map: recurse into values
            (TypeExpr::Map(inner), Value::Map(map)) => {
                for (key, val) in map {
                    if let Some(e) =
                        self.validate_type_with_ref_awareness(inner, val, sibling_bindings)
                    {
                        return Some(format!("Key '{}': {}", key, e));
                    }
                }
                None
            }
            // Recurse through this ref-aware walker — not `validate_type_expr_value` —
            // so cross-file refs inside struct fields still resolve against sibling bindings.
            (TypeExpr::Struct { fields }, Value::Map(map)) => {
                if let Some(e) = carina_core::validation::struct_field_shape_errors(fields, map) {
                    return Some(e);
                }
                for (name, field_ty) in fields {
                    if let Some(v) = map.get(name)
                        && let Some(e) =
                            self.validate_type_with_ref_awareness(field_ty, v, sibling_bindings)
                    {
                        return Some(format!("field '{}': {}", name, e));
                    }
                }
                None
            }
            // ResourceRef: skip (resolved at runtime)
            (_, Value::ResourceRef { .. }) => None,
            // Everything else: normal validation
            _ => carina_core::validation::validate_type_expr_value(
                type_expr,
                value,
                &self.provider_context,
            ),
        }
    }

    /// Find the position of an attributes parameter name in the document.
    fn find_attributes_param_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_attributes_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("attributes ") && trimmed.contains('{') {
                in_attributes_block = true;
                continue;
            }

            if in_attributes_block {
                if trimmed == "}" {
                    in_attributes_block = false;
                    continue;
                }

                // Look for "param_name:" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Find the position of the value expression in an attributes parameter line.
    fn find_attributes_value_position(
        &self,
        doc: &Document,
        param_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let mut in_attributes_block = false;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("attributes ") && trimmed.contains('{') {
                in_attributes_block = true;
                continue;
            }

            if in_attributes_block {
                if trimmed == "}" {
                    in_attributes_block = false;
                    continue;
                }

                // Look for "param_name: type = value" pattern
                if trimmed.starts_with(param_name)
                    && trimmed[param_name.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c == ':')
                {
                    // Find the "=" and return position after it
                    if let Some(eq_byte_pos) = line.find('=') {
                        let after_eq = &line[eq_byte_pos + 1..];
                        let trimmed_after = after_eq.trim_start();
                        // Whitespace after '=' is ASCII, so byte diff == char count
                        let ws_after_eq = after_eq.len() - trimmed_after.len();
                        let value_col = position::byte_offset_to_char_offset(line, eq_byte_pos)
                            + 1
                            + ws_after_eq as u32;
                        return Some((line_idx as u32, value_col));
                    }
                }
            }
        }
        None
    }

    /// Validate export parameter values against their type annotations.
    ///
    /// `all_resources` provides cross-file resources for schema-level ref type
    /// checking. If None, falls back to the current file's resources.
    pub(super) fn check_exports_blocks(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        all_resources: Option<&[Resource]>,
        sibling_bindings: &HashMap<String, String>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for param in &parsed.export_params {
            if let (Some(type_expr), Some(value)) = (&param.type_expr, &param.value)
                && let Some(type_error) =
                    self.validate_attributes_type(type_expr, value, sibling_bindings)
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
        // `infer_export_params` borrows `parsed` (avoiding the per-keystroke
        // deep clone the full `apply_inference` would force) so the LSP
        // can derive the post-inference shape without rebuilding the
        // whole `InferredFile`.
        let resources = all_resources.unwrap_or(&parsed.resources);
        let (inferred_export_params, inference_errors) =
            carina_core::validation::inference::infer_export_params(parsed, &self.schemas);
        // Surface inference failures as "type annotation required"
        // diagnostics, anchored at the export name.
        for (name, err) in &inference_errors {
            if let Some((line, col)) = self.find_exports_param_position(doc, name) {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    carina_core::validation::inference::format_inference_error(name, err),
                ));
            }
        }
        if let Err(ref_errors) = carina_core::validation::validate_export_param_ref_types(
            &inferred_export_params,
            resources,
            &self.schemas,
        ) {
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
                let builtin = ["ipv4_cidr", "ipv4_address", "ipv6_cidr", "ipv6_address"];
                if builtin.contains(&name.as_str()) {
                    return None;
                }
                if self.provider_context.validators.contains_key(name) {
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

        for (_ctx, resource) in parsed.iter_all_resources() {
            for value in resource.attributes.values() {
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
        let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(&base_path.to_path_buf())
        else {
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
            Value::FunctionCall { name, args } => {
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
            Value::List(items) => {
                for item in items {
                    self.collect_unknown_function_diagnostics(doc, item, user_fns, diagnostics);
                }
            }
            Value::Map(map) => {
                for v in map.values() {
                    self.collect_unknown_function_diagnostics(doc, v, user_fns, diagnostics);
                }
            }
            Value::Interpolation(parts) => {
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
    pub(super) fn check_resource_ref_attributes(
        &self,
        doc: &Document,
        parsed: &ParsedFile,
        binding_schema_map: &HashMap<&str, &ResourceSchema>,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for (_ctx, resource) in parsed.iter_all_resources() {
            for (attr_name, attr_value) in &resource.attributes {
                if attr_name.starts_with('_') {
                    continue;
                }
                self.collect_ref_attr_diagnostics(
                    doc,
                    attr_value,
                    binding_schema_map,
                    &mut diagnostics,
                );
            }
        }

        // Also check module call arguments
        for call in &parsed.module_calls {
            for value in call.arguments.values() {
                self.collect_ref_attr_diagnostics(doc, value, binding_schema_map, &mut diagnostics);
            }
        }

        // Also check attribute parameter values
        for attr_param in &parsed.attribute_params {
            if let Some(value) = &attr_param.value {
                self.collect_ref_attr_diagnostics(doc, value, binding_schema_map, &mut diagnostics);
            }
        }

        diagnostics
    }

    /// Recursively check ResourceRef values for unknown attributes.
    fn collect_ref_attr_diagnostics(
        &self,
        doc: &Document,
        value: &Value,
        binding_schema_map: &HashMap<&str, &ResourceSchema>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        value.visit_refs(&mut |path| {
            let binding_name = path.binding();
            let attribute_name = path.attribute();
            let Some(ref_schema) = binding_schema_map.get(binding_name) else {
                return;
            };
            if ref_schema.attributes.contains_key(attribute_name) {
                return;
            }
            // Attribute not found - build "did you mean" suggestion
            let known_attrs: Vec<&str> = ref_schema.attributes.keys().map(|s| s.as_str()).collect();
            let suggestion = suggest_similar_name(attribute_name, &known_attrs)
                .map(|s| format!(" Did you mean '{}'?", s))
                .unwrap_or_default();

            let ref_text = format!("{}.{}", binding_name, attribute_name);
            if let Some((line, col)) = self.find_ref_value_position(doc, &ref_text) {
                // Highlight just the attribute part (after the dot)
                let attr_col = col + binding_name.len() as u32 + 1; // +1 for the dot
                diagnostics.push(carina_diagnostic(
                    line,
                    attr_col,
                    attr_col + attribute_name.len() as u32,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Unknown attribute '{}' on '{}' (type '{}'){}",
                        attribute_name, binding_name, ref_schema.resource_type, suggestion,
                    ),
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
}
