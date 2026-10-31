//! LSP adapter that runs core's path-tagged validator
//! ([`carina_core::schema::EffectiveType::validate_collect`]) and anchors
//! each error at its source position. The recursive
//! struct/list-of-struct walk lives entirely in `carina-core` after
//! #2214; this module just translates `(FieldPath, TypeError)` to LSP
//! diagnostics.

use std::collections::HashMap;

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::document::Document;
use crate::position;
use carina_core::resource::AuthoredValue as Value;
use carina_core::schema::{
    EffectiveType, FieldPath, FieldPathStep, TypeError, ValidatedResourceSchema,
};

use super::{DiagnosticEngine, build_enum_diagnostic_from_error, carina_diagnostic};

impl DiagnosticEngine {
    /// Run [`carina_core::schema::EffectiveType::validate_collect`] against
    /// the resource attribute's schema and translate each
    /// `(FieldPath, TypeError)` into an LSP `Diagnostic` anchored at
    /// the offending source position. The LSP no longer recurses into
    /// nested struct shapes — that responsibility belongs to the core
    /// validator (#2214).
    ///
    /// `effective_type` carries both the resource definition map and the
    /// selected attribute path's input ancestry.
    pub(super) fn validate_struct_value(
        &self,
        doc: &Document,
        attr_name: &str,
        value: &Value,
        effective_type: &EffectiveType<'_>,
        value_range: Option<Range>,
    ) -> Vec<Diagnostic> {
        let errors = effective_type.validate_collect_in_phase(value);
        let mut diagnostics = Vec::new();
        for (path, err) in errors {
            if let Some((line, col, end_col)) = self.range_for_path(doc, attr_name, &path) {
                if path.is_empty() {
                    let tagged = err.clone().with_attribute(attr_name);
                    if let Some(diagnostic) = build_enum_diagnostic_from_error(&tagged, value_range)
                    {
                        diagnostics.push(diagnostic);
                        continue;
                    }
                }
                let err = if matches!(&err, TypeError::FieldGroupViolation { .. }) {
                    err.with_attribute(attr_name)
                } else {
                    err
                };
                diagnostics.push(carina_diagnostic(
                    line,
                    col,
                    end_col,
                    severity_for_validation_error(&err),
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
            return self
                .find_attribute_position(doc, attr_name, None)
                .or_else(|| {
                    self.find_all_block_positions(doc, attr_name)
                        .first()
                        .copied()
                })
                .map(|(line, col)| (line, col, col + attr_name.len() as u32));
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
        schema: &ValidatedResourceSchema,
    ) -> Vec<Diagnostic> {
        use carina_core::schema::Shape;

        let mut diagnostics = Vec::new();

        for attr_schema in schema.effective_attributes() {
            let attr_name = attr_schema.name();
            // Resource-level validation reports read-only assignments; do not
            // offer authoring-style hints for attributes the user cannot set.
            if attr_schema.is_read_only() {
                continue;
            }

            // Only check List<Struct> attributes. Use `Shape` (Ref-peeled)
            // for both the attribute type and the list element type so
            // the hint also fires for cyclic-CFN shapes like
            // `Ref("LifecycleConfiguration")` whose def carries a
            // `List<Ref<Rule>>` field (same bug class as carina#3349).
            let is_list_struct = matches!(
                schema.as_schema().shape_of(attr_schema.field_type()),
                Shape::List { element_type: inner, .. } if matches!(
                    schema.as_schema().shape_of(inner),
                    Shape::Struct { .. }
                )
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

fn severity_for_validation_error(error: &TypeError) -> DiagnosticSeverity {
    match error {
        TypeError::ReadOnlyAttribute { .. } | TypeError::FieldGroupViolation { .. } => {
            DiagnosticSeverity::ERROR
        }
        TypeError::ListItemError { inner, .. }
        | TypeError::MapKeyError { inner, .. }
        | TypeError::MapValueError { inner, .. }
        | TypeError::StructFieldError { inner, .. } => severity_for_validation_error(inner),
        TypeError::UnionAlternativesMismatch { failures } => {
            if failures
                .iter()
                .all(|failure| severity_for_validation_error(failure) == DiagnosticSeverity::ERROR)
            {
                DiagnosticSeverity::ERROR
            } else {
                DiagnosticSeverity::WARNING
            }
        }
        _ => DiagnosticSeverity::WARNING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};
    use carina_core::schema::{
        AttributeType, RootlessType, TypeIdentity, ValidatedConfigSchema, validator,
    };
    use std::collections::BTreeMap;

    fn pathless_test_type(attr_type: AttributeType) -> RootlessType {
        ValidatedConfigSchema::validate(BTreeMap::from([("value".to_string(), attr_type)]))
            .expect("test config type must be valid")
            .rootless_type("value")
            .expect("test config type must exist")
    }

    fn tied_string_error(first: TypeError, second: TypeError) -> TypeError {
        let rejecting = |error: TypeError| {
            AttributeType::refined_string_with_validator(
                TypeIdentity::bare("TestValidator"),
                None,
                None,
                validator(move |_| Err(error.clone())),
                None,
            )
        };
        pathless_test_type(AttributeType::union(vec![
            rejecting(first),
            rejecting(second),
        ]))
        .validate(&Value::Concrete(ConcreteValue::String("invalid".into())))
        .expect_err("both Union arms must reject the value")
    }

    #[test]
    fn tied_union_is_warning_when_only_one_arm_is_an_error() {
        let error = tied_string_error(
            TypeError::ReadOnlyAttribute {
                name: "provider_value".to_string(),
            },
            TypeError::ValidationFailed {
                message: "not accepted".to_string(),
            },
        );

        assert_eq!(
            severity_for_validation_error(&error),
            DiagnosticSeverity::WARNING
        );
    }

    #[test]
    fn tied_union_is_error_when_every_arm_is_an_error() {
        let error = tied_string_error(
            TypeError::ReadOnlyAttribute {
                name: "first".to_string(),
            },
            TypeError::ReadOnlyAttribute {
                name: "second".to_string(),
            },
        );

        assert_eq!(
            severity_for_validation_error(&error),
            DiagnosticSeverity::ERROR
        );
    }
}
