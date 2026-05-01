//! LSP adapter that runs core's path-tagged validator
//! ([`AttributeType::validate_collect`]) and anchors each error at
//! its source position. The recursive struct/list-of-struct walk
//! lives entirely in `carina-core` after #2214; this module just
//! translates `(FieldPath, TypeError)` to LSP diagnostics.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::document::Document;
use crate::position;
use carina_core::resource::Value;
use carina_core::schema::{AttributeType, FieldPath, FieldPathStep, ResourceSchema};

use super::{DiagnosticEngine, carina_diagnostic};

impl DiagnosticEngine {
    /// Run [`AttributeType::validate_collect`] against the resource
    /// attribute's schema and translate each `(FieldPath, TypeError)`
    /// into an LSP `Diagnostic` anchored at the offending source
    /// position. The LSP no longer recurses into nested struct shapes
    /// — that responsibility belongs to the core validator (#2214).
    ///
    /// Takes `&AttributeType` (rather than `&[StructField]`) so the
    /// per-keystroke diagnostic pass borrows the schema instead of
    /// deep-cloning every `StructField` (which itself contains
    /// recursive `AttributeType` sub-trees).
    pub(super) fn validate_struct_value(
        &self,
        doc: &Document,
        attr_name: &str,
        value: &Value,
        ty: &AttributeType,
    ) -> Vec<Diagnostic> {
        let errors = ty.validate_collect(value);
        let mut diagnostics = Vec::new();
        for (path, err) in errors {
            if let Some((line, col, end_col)) = self.range_for_path(doc, attr_name, &path) {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    end_col,
                    DiagnosticSeverity::WARNING,
                    err.to_string(),
                ));
            }
        }
        diagnostics
    }

    /// Walk a [`FieldPath`] across the source `doc` and return the
    /// `(line, col, end_col)` triple to underline. Returns `None` when
    /// the path cannot be located — better to drop the diagnostic
    /// than to attach it to the wrong line.
    fn range_for_path(
        &self,
        doc: &Document,
        attr_name: &str,
        path: &FieldPath,
    ) -> Option<(u32, u32, u32)> {
        let steps = path.steps();
        if steps.is_empty() {
            return None;
        }

        // For a List<Struct> at the top level (block syntax `attr {
        // ... } attr { ... }`), the first step is a `[i]` index that
        // selects which block to walk into; the *next* step is the
        // first field name to find inside that block.
        let (block_index, name_steps): (usize, &[FieldPathStep]) = match &steps[0] {
            FieldPathStep::Index(i) => (*i, &steps[1..]),
            FieldPathStep::Field(_) => (0, steps),
        };

        let Some(FieldPathStep::Field(first_name)) = name_steps.first() else {
            // Path ends with an index — anchor on the block header itself.
            let positions = self.find_all_block_positions(doc, attr_name);
            return positions
                .get(block_index)
                .map(|(line, col)| (*line, *col, *col + attr_name.len() as u32));
        };

        // The first name lookup goes through the LSP's existing
        // nested-block scanner, which already handles repeated blocks
        // and assignment-style `attr = { ... }`.
        let positions = self.find_all_nested_field_positions(doc, attr_name, first_name);
        let Some(Some((mut line, mut col))) = positions.get(block_index).copied() else {
            return None;
        };

        // For deeper paths (struct nested inside struct), descend by
        // re-using the same scanner with the previous field name as
        // the block name. This is approximate — LSP-side struct path
        // resolution is best-effort — but matches what the legacy
        // `validate_struct_value` was already doing.
        let mut current_block = first_name.as_str();
        for step in name_steps.iter().skip(1) {
            if let FieldPathStep::Field(child) = step {
                let nested_positions =
                    self.find_all_nested_field_positions(doc, current_block, child);
                if let Some(Some((nl, nc))) = nested_positions.first().copied() {
                    line = nl;
                    col = nc;
                }
                current_block = child.as_str();
            }
            // `Index` steps inside a nested struct (List<Struct> field
            // of a struct) keep the `current_block` cursor where it
            // is; the existing scanner walks all matching blocks and
            // we already pinned the outer index above.
        }

        let end_col = col + current_block.len() as u32;
        Some((line, col, end_col))
    }

    /// Find the start positions of ALL blocks with the given name.
    /// Returns `(line, col)` for each occurrence of `block_name {`.
    pub(super) fn find_all_block_positions(
        &self,
        doc: &Document,
        block_name: &str,
    ) -> Vec<(u32, u32)> {
        let text = doc.text();
        let mut positions = Vec::new();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Look for "block_name {" (without "=")
            if trimmed.starts_with(block_name) && !trimmed.contains('=') {
                let after = trimmed[block_name.len()..].trim();
                if after.starts_with('{') {
                    positions.push((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }

        positions
    }

    /// Find position of list literal syntax (`attr_name = [`) in source text.
    /// Returns `(line, col)` if found.
    pub(super) fn find_list_literal_position(
        &self,
        doc: &Document,
        attr_name: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after = &trimmed[attr_name.len()..];
            // Must be followed by whitespace or '=' (not part of a longer identifier)
            if !after.starts_with(' ') && !after.starts_with('=') {
                continue;
            }
            // Check for `= [` pattern (list literal)
            let after_trimmed = after.trim_start();
            if let Some(rest) = after_trimmed.strip_prefix('=') {
                let rest_trimmed = rest.trim_start();
                if rest_trimmed.starts_with('[') {
                    return Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }
            }
        }
        None
    }

    /// Check List<Struct> attributes for list literal syntax and suggest block syntax.
    pub(super) fn check_list_struct_syntax(
        &self,
        doc: &Document,
        resource_attrs: &HashMap<String, Value>,
        schema: &ResourceSchema,
    ) -> Vec<Diagnostic> {
        use carina_core::schema::AttributeType;

        let mut diagnostics = Vec::new();

        for (attr_name, attr_schema) in &schema.attributes {
            // Only check List<Struct> attributes
            let is_list_struct = matches!(
                &attr_schema.attr_type,
                AttributeType::List { inner, .. } if matches!(inner.as_ref(), AttributeType::Struct { .. })
            );
            if !is_list_struct {
                continue;
            }

            // Only check attributes that actually exist in the resource
            if !resource_attrs.contains_key(attr_name) {
                continue;
            }

            if let Some((line, col)) = self.find_list_literal_position(doc, attr_name) {
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    col + attr_name.len() as u32,
                    DiagnosticSeverity::HINT,
                    format!(
                        "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                        attr_name, attr_name, attr_name
                    ),
                ));
            }
        }

        diagnostics
    }

    /// Find the positions of a field inside ALL matching nested blocks
    /// Returns a Vec with one entry per block occurrence. Each entry is `Some((line, col))`
    /// if the field was found in that block, or `None` if not.
    pub(super) fn find_all_nested_field_positions(
        &self,
        doc: &Document,
        block_name: &str,
        field_name: &str,
    ) -> Vec<Option<(u32, u32)>> {
        let text = doc.text();
        let mut positions = Vec::new();
        let mut in_block = false;
        let mut brace_depth = 0;
        let mut found_in_current_block: Option<(u32, u32)> = None;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();

            // Look for "block_name {" (without "=") or "block_name = {" (with "=")
            if !in_block && trimmed.starts_with(block_name) {
                let after_name = trimmed[block_name.len()..].trim();
                let has_opening_brace = if after_name.starts_with('{') {
                    true
                } else if let Some(after_eq) = after_name.strip_prefix('=') {
                    after_eq.trim().starts_with('{')
                } else {
                    false
                };
                if has_opening_brace {
                    in_block = true;
                    brace_depth = 1;
                    found_in_current_block = None;
                    continue;
                }
            }

            if in_block {
                // Check for field name BEFORE counting braces so that lines like
                // "field_name {" are detected at the current depth (before '{' increments it)
                if brace_depth == 1
                    && let Some(after) = trimmed.strip_prefix(field_name)
                    && (after.starts_with(' ') || after.starts_with('=') || after.starts_with('{'))
                {
                    found_in_current_block =
                        Some((line_idx as u32, position::leading_whitespace_chars(line)));
                }

                for ch in trimmed.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                    } else if ch == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            in_block = false;
                            positions.push(found_in_current_block);
                            found_in_current_block = None;
                            break;
                        }
                    }
                }
            }
        }

        // Handle case where file ends while still in a block
        if in_block {
            positions.push(found_in_current_block);
        }

        positions
    }
}
