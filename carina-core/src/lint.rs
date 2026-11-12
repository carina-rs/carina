//! Lint utilities for detecting common DSL style issues
//!
//! This module provides functions for static analysis of `.crn` source files,
//! such as detecting list literal syntax where block syntax is preferred.

#[cfg(test)]
#[path = "lint_main_reference.rs"]
mod round11_main_reference;

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

use pest::Span;
use pest::iterators::Pair;

use crate::parser::{PreprocessedFile, Rule, extract_key_string, preprocess_file};
use crate::schema::CompiledResourceSchema as ResourceSchema;

/// Find list literal syntax (`attr = [...]`) for the given attribute names.
/// Returns attribute name and 1-indexed line number for each occurrence.
pub fn find_list_literal_attrs(source: &str, attr_names: &HashSet<String>) -> Vec<(String, usize)> {
    find_list_literal_attrs_in_source(&crate::parser::DiagnosticFileText::new(source), attr_names)
}

pub fn find_list_literal_attrs_in_source(
    source: &crate::parser::DiagnosticFileText<'_>,
    attr_names: &HashSet<String>,
) -> Vec<(String, usize)> {
    let Some(admitted) = crate::parser::LintSource::new(source) else {
        return Vec::new();
    };
    let source = admitted.original_text();
    let mut results = Vec::new();
    let names = crate::lint_text::PrefixNames::new(attr_names.iter().map(String::as_str));
    #[cfg(any(test, feature = "test-utils"))]
    crate::test_work::record(crate::test_work::Operation::LintScanBytes, source.len());

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        let matches = names.matches(trimmed);
        if matches.is_empty() {
            continue;
        }
        // Index whitespace once too: overlapping schema prefixes must not
        // repeatedly trim the same remaining suffix.
        let mut next_non_whitespace = vec![trimmed.len(); trimmed.len() + 1];
        let mut next = trimmed.len();
        for (byte, ch) in trimmed.char_indices().rev() {
            if !ch.is_whitespace() {
                next = byte;
            }
            next_non_whitespace[byte] = next;
        }
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::LintScanBytes, trimmed.len());
        for attr_name in matches {
            let end = attr_name.len();
            if !matches!(trimmed.as_bytes().get(end), Some(b' ' | b'=')) {
                continue;
            }
            let equal = next_non_whitespace[end];
            if trimmed.as_bytes().get(equal) != Some(&b'=') {
                continue;
            }
            let value = next_non_whitespace[equal + 1];
            if trimmed.as_bytes().get(value) == Some(&b'[') {
                results.push((attr_name.to_owned(), line_idx + 1));
            }
        }
    }

    results
}

/// Collect all `List<Struct>` attribute names from a schema.
///
/// Peels [`AttributeType::Ref`] against `schema.defs` for both the
/// attribute type and the list element type, so cyclic-CFN attributes
/// (`Ref("LifecycleConfiguration")` whose def carries a
/// `List<Ref<Rule>>` field) still classify as `List<Struct>` and get
/// the "use block syntax instead of `[...]`" lint warning. Same bug
/// class as carina#3349 — a raw `matches!` shape gate would silently
/// drop `Ref`-typed attributes.
pub fn list_struct_attr_names(schema: &ResourceSchema) -> HashSet<String> {
    use crate::schema::Shape;
    schema
        .attributes
        .iter()
        .filter(|(_, attr_schema)| {
            // Project onto `Shape` so any `Ref` chain is peeled at
            // the type level (carina#3349). `Shape` has no `Ref`
            // variant, so a `Ref`-typed attribute cannot be missed.
            matches!(
                attr_schema.attr_type.shape_with_defs(&schema.defs),
                Shape::List { element_type: inner, .. } if matches!(
                    inner.shape_with_defs(&schema.defs),
                    Shape::Struct { .. }
                )
            )
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Functions that follow data-last convention for pipe compatibility.
/// Direct calls with 2+ args work but have unintuitive argument order;
/// pipe form is preferred.
const PIPE_PREFERRED_FUNCTIONS: &[&str] = &["join", "split", "map", "concat", "replace"];

/// A warning for direct calls to pipe-preferred functions.
#[derive(Debug, Clone, PartialEq)]
pub struct PipePreferredWarning {
    /// The function name
    pub name: String,
    /// 1-indexed line number
    pub line: usize,
}

/// A lint and its admitted source range. Construction is confined to the
/// production collectors; legacy wrappers can discard positions purely.
#[derive(Debug)]
pub struct LintFinding<T> {
    detail: T,
    span: crate::parser::ParseWarningSpan,
    editor: (usize, usize, usize),
}

impl<T> LintFinding<T> {
    pub fn detail(&self) -> &T {
        &self.detail
    }
    pub fn span(&self) -> &crate::parser::ParseWarningSpan {
        &self.span
    }
    /// Zero-based editor presentation, preserving legacy public positions.
    pub fn editor_range(&self) -> (u32, u32, u32) {
        (
            self.editor.0 as u32,
            self.editor.1 as u32,
            self.editor.2 as u32,
        )
    }
    pub fn into_detail(self) -> T {
        self.detail
    }
}

/// Find direct calls to pipe-preferred transformation functions.
///
/// Detects patterns like `join("-", parts)` where the pipe form
/// `parts |> join("-")` is recommended. Only warns when the function
/// call is NOT preceded by `|>` on the same line.
pub fn find_pipe_preferred_direct_calls(source: &str) -> Vec<PipePreferredWarning> {
    pipe_preferred_direct_call_findings(source)
        .into_iter()
        .map(LintFinding::into_detail)
        .collect()
}

/// Collect pipe hints with source ownership and admitted editor positions.
pub fn pipe_preferred_direct_call_findings(source: &str) -> Vec<LintFinding<PipePreferredWarning>> {
    pipe_preferred_direct_call_findings_in_source(&crate::parser::DiagnosticFileText::new(source))
}

pub fn pipe_preferred_direct_call_findings_in_source(
    source: &crate::parser::DiagnosticFileText<'_>,
) -> Vec<LintFinding<PipePreferredWarning>> {
    let Some(admitted) = crate::parser::LintSource::new(source) else {
        return Vec::new();
    };
    let source = admitted.original_text();
    let mut warnings = Vec::new();
    let patterns = PIPE_PREFERRED_FUNCTIONS
        .iter()
        .map(|name| format!("{name}("))
        .collect::<Vec<_>>();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Scan once, preserving the existing heuristic: any raw pipe marker
        // stops hints for the rest of the line, even inside a quoted string,
        // and every double quote toggles string state (including escaped ones).
        // Buckets retain the legacy per-function ordering on each line.
        let mut calls: [Vec<usize>; PIPE_PREFERRED_FUNCTIONS.len()] =
            std::array::from_fn(|_| Vec::new());
        let mut in_string = false;
        let mut previous_non_whitespace = None;
        for (pos, ch) in trimmed.char_indices() {
            #[cfg(any(test, feature = "test-utils"))]
            crate::test_work::record(crate::test_work::Operation::PipeHintCharacter, 1);
            let rest = &trimmed[pos..];
            if rest.starts_with("|>") {
                break;
            }
            let previous_identifier = pos > 0 && {
                let previous = trimmed.as_bytes()[pos - 1];
                previous.is_ascii_alphanumeric() || previous == b'_'
            };
            if !in_string && !previous_identifier && previous_non_whitespace != Some(':') {
                for (index, pattern) in patterns.iter().enumerate() {
                    if rest.starts_with(pattern) {
                        calls[index].push(pos);
                    }
                }
            }
            if ch == '"' {
                in_string = !in_string;
            }
            if !ch.is_whitespace() {
                previous_non_whitespace = Some(ch);
            }
        }
        for ((&func_name, pattern), calls) in
            PIPE_PREFERRED_FUNCTIONS.iter().zip(&patterns).zip(calls)
        {
            for pos in calls {
                let start = trimmed.as_ptr() as usize - source.as_ptr() as usize + pos;
                let (span, editor) =
                    admitted.lint_ranges(start..start + func_name.len(), pattern, func_name.len());
                warnings.push(LintFinding {
                    detail: PipePreferredWarning {
                        name: func_name.to_string(),
                        line: line_idx + 1,
                    },
                    span,
                    editor,
                });
            }
        }
    }

    warnings
}

/// A warning for non-snake_case binding names.
#[derive(Debug, Clone, PartialEq)]
pub struct NamingWarning {
    /// The binding name that violates snake_case convention
    pub name: String,
    /// 1-indexed line number
    pub line: usize,
}

/// Check whether a name follows snake_case convention.
///
/// Rules:
/// - Only lowercase ASCII letters, digits, and underscores
/// - Cannot start with a digit
/// - Cannot start or end with underscore
/// - Cannot have consecutive underscores
fn is_snake_case(name: &str) -> bool {
    if name.is_empty() || name.starts_with('_') || name.ends_with('_') {
        return false;
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    if name.contains("__") {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Extract a binding name from the text after `let` or `import`.
///
/// The name is delimited by whitespace or `=`. Returns `None` if the name is
/// empty or starts with `_`. `for` lines use `extract_for_binding_names`
/// instead because they can carry two names (map form).
fn extract_binding_name(after_keyword: &str) -> Option<&str> {
    let trimmed = after_keyword.trim_start();
    let end = trimmed
        .char_indices()
        .find_map(|(byte, ch)| (ch.is_whitespace() || ch == '=').then_some(byte))
        .unwrap_or(trimmed.len());
    let name = &trimmed[..end];

    if name.is_empty() || name.starts_with('_') {
        return None;
    }

    Some(name)
}

/// Extract binding names from a `for` line.
///
/// Handles both forms:
/// - Single: `for v in xs` → `["v"]`
/// - Map: `for k, v in m` → `["k", "v"]`
///
/// The "header" is the portion before ` in `. Names beginning with `_` are
/// dropped (discard/unused markers are not subject to the naming check).
fn extract_for_binding_names(after_for: &str) -> Vec<&str> {
    let header = after_for.split(" in ").next().unwrap_or("");
    header
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !part.starts_with('_'))
        .collect()
}

/// Find `let` bindings with non-snake_case names in source text.
///
/// Scans lines for `let <name> =` patterns and checks if `<name>` follows
/// snake_case convention. Bindings starting with `_` are skipped (internal/synthetic).
pub fn find_non_snake_case_bindings(source: &str) -> Vec<NamingWarning> {
    non_snake_case_binding_findings(source)
        .into_iter()
        .map(LintFinding::into_detail)
        .collect()
}

/// Collect naming findings without losing the admitted occurrence's range.
pub fn non_snake_case_binding_findings(source: &str) -> Vec<LintFinding<NamingWarning>> {
    non_snake_case_binding_findings_in_source(&crate::parser::DiagnosticFileText::new(source))
}

pub fn non_snake_case_binding_findings_in_source(
    source: &crate::parser::DiagnosticFileText<'_>,
) -> Vec<LintFinding<NamingWarning>> {
    let Some(admitted) = crate::parser::LintSource::new(source) else {
        return Vec::new();
    };
    let source = admitted.text();
    let mut candidates = Vec::new();
    #[cfg(any(test, feature = "test-utils"))]
    crate::test_work::record(crate::test_work::Operation::LintScanBytes, source.len());

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Extract binding name(s) from `let`, `for`, or `import` patterns.
        // `for` may have two names (map-form: `for k, v in m`); every other
        // form has at most one.
        let names: Vec<&str> = if let Some(rest) = trimmed.strip_prefix("let ") {
            extract_binding_name(rest).into_iter().collect()
        } else if let Some(rest) = trimmed.strip_prefix("for ") {
            extract_for_binding_names(rest)
        } else {
            Vec::new()
        };

        for name in names {
            if !is_snake_case(name) {
                let name_start = name.as_ptr() as usize - source.as_ptr() as usize;
                let range = admitted.first_public_occurrence(name_start..name_start + name.len());
                candidates.push((range, line_idx + 1));
            }
        }
    }

    admitted.collect_names(candidates, |name, line, range| {
        let (span, editor) = admitted.lint_ranges(range, &name, name.len());
        LintFinding {
            detail: NamingWarning { name, line },
            span,
            editor,
        }
    })
}

/// Casing style for a tag key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagKeyStyle {
    PascalCase,
    SnakeCase,
    Other,
}

/// A tag key extracted from source text.
#[derive(Debug, Clone, PartialEq)]
pub struct TagKeyEntry {
    pub key: String,
    pub style: TagKeyStyle,
    /// 1-indexed line number
    pub line: usize,
    /// 0-indexed character offset of the key start.
    pub column: usize,
    /// Source file when collected as part of a directory-wide population.
    pub file: Option<PathBuf>,
}

/// A warning for tag keys whose casing style is inconsistent with the majority.
#[derive(Debug, Clone, PartialEq)]
pub struct TagKeyWarning {
    pub key: String,
    pub expected_style: TagKeyStyle,
    /// 1-indexed line number
    pub line: usize,
    /// 0-indexed character offset of the key start.
    pub column: usize,
    /// File path inherited from the corresponding [`TagKeyEntry`].
    pub file: Option<PathBuf>,
}

/// Classify a tag key's casing style.
fn classify_tag_key_style(name: &str) -> TagKeyStyle {
    if name.is_empty() {
        return TagKeyStyle::Other;
    }
    // PascalCase: starts uppercase, no underscores/hyphens, all alphanumeric
    if name.starts_with(|c: char| c.is_ascii_uppercase())
        && !name.contains('_')
        && !name.contains('-')
        && name.chars().all(|c| c.is_alphanumeric())
    {
        return TagKeyStyle::PascalCase;
    }
    // snake_case: all lowercase/digits/underscores, must contain underscore or be all lowercase
    if name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return TagKeyStyle::SnakeCase;
    }
    TagKeyStyle::Other
}

/// Collect all tag keys from `tags = { ... }` blocks in source text.
///
/// Returns entries with key name, detected style, line, and character column.
/// Does not judge consistency — call `find_mixed_tag_key_styles` on the aggregated entries.
pub fn collect_tag_keys(source: &str) -> Vec<TagKeyEntry> {
    collect_tag_keys_in_source(&crate::parser::DiagnosticFileText::new(source))
}

pub fn collect_tag_keys_in_source(
    source: &crate::parser::DiagnosticFileText<'_>,
) -> Vec<TagKeyEntry> {
    let Some(admitted) = crate::parser::LintSource::new(source) else {
        return Vec::new();
    };
    let source = admitted.text();
    let mut candidates = Vec::new();
    let mut in_tags_block = false;
    let mut brace_depth: usize = 0;

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let trimmed_start_byte = line.len() - line.trim_start().len();
        let line_number = line_idx + 1;

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Detect start of a tags block: `tags = {`
        if !in_tags_block && let Some(after) = trimmed.strip_prefix("tags") {
            let after = after.trim_start();
            if let Some(after_eq) = after.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                if after_eq.starts_with('{') {
                    in_tags_block = true;
                    brace_depth = 1;
                    continue;
                }
            }
        }

        if in_tags_block {
            // Count braces
            for ch in trimmed.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => {
                        brace_depth = brace_depth.saturating_sub(1);
                        if brace_depth == 0 {
                            in_tags_block = false;
                        }
                    }
                    _ => {}
                }
            }

            // Check for key = value pattern
            if !trimmed.starts_with('}')
                && let Some(eq_pos) = trimmed.find('=')
            {
                let key_segment = &trimmed[..eq_pos];
                let key = key_segment.trim();
                if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let key_start_in_trimmed = key_segment.len() - key_segment.trim_start().len();
                    let key_start_byte = trimmed_start_byte + key_start_in_trimmed;
                    let line_start = line.as_ptr() as usize - source.as_ptr() as usize;
                    let start = line_start + key_start_byte;
                    candidates.push((
                        start..start + key.len(),
                        (line_number, admitted.position(start).1 - 1),
                    ));
                }
            }
        }
    }

    admitted.collect_names(candidates, |key, (line, column), _| TagKeyEntry {
        style: classify_tag_key_style(&key),
        column,
        key,
        line,
        file: None,
    })
}

/// Collect tag keys from one source file and attach its path to every entry.
pub fn collect_tag_keys_for_file(source: &str, file: &Path) -> Vec<TagKeyEntry> {
    collect_tag_keys(source)
        .into_iter()
        .map(|mut entry| {
            entry.file = Some(file.to_path_buf());
            entry
        })
        .collect()
}

/// Collect the complete tag-key population from root sources and called modules.
///
/// `root_inputs` must contain every root `.crn` file. Callers provide the text
/// so editor buffers can override an on-disk file without changing population
/// semantics. Root inputs are collected first; called module directories are
/// then scanned from disk.
///
/// Every file has one vote. Pinned identities deduplicate root/module overlap,
/// symlink aliases, repeated imports, and multiple aliases to one module.
/// Root inputs retain the pins used to read their source text; resolution
/// failures are reported by the caller when collecting these inputs.
pub fn collect_all_tag_keys<E, D, S: AsRef<str>, B: crate::resource::BindingAddress>(
    root_inputs: &[(crate::paths::ResolvedPath, S)],
    parsed: &crate::parser::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    base_dir: &crate::paths::ResolvedPath,
) -> Vec<TagKeyEntry> {
    let mut seen_files = HashSet::new();
    let mut entries = Vec::new();
    for (file, source) in root_inputs {
        if seen_files.insert(file.identity()) {
            entries.extend(collect_tag_keys_for_file(
                source.as_ref(),
                &file.source_label(),
            ));
        }
    }

    let aliases_used: HashSet<&str> = parsed
        .module_calls
        .iter()
        .map(|call| call.module_name.as_str())
        .collect();

    // The caller already supplied the complete root directory population.
    // Mark it visited so `source = "."` and symlinks back to the root do not
    // trigger a redundant directory read.
    let mut seen_module_dirs = HashSet::from([base_dir.identity()]);
    for import in &parsed.uses {
        if !aliases_used.contains(import.alias.as_str()) {
            continue;
        }
        let Ok(module_dir) = base_dir.join(&import.path) else {
            continue;
        };
        if !module_dir.is_dir() {
            continue;
        }
        if !seen_module_dirs.insert(module_dir.identity()) {
            continue;
        }
        let Ok(module_files) = crate::config_loader::find_crn_files_in_dir(&module_dir) else {
            continue;
        };
        for module_file in module_files {
            if !seen_files.insert(module_file.identity()) {
                continue;
            }
            let Ok(content) = module_file.read_to_string() else {
                continue;
            };
            entries.extend(collect_tag_keys_for_file(
                &content,
                &module_file.source_label(),
            ));
        }
    }
    entries
}

/// Detect tag keys whose casing style is inconsistent with the majority.
///
/// Determines the dominant style (PascalCase or snake_case) by counting occurrences,
/// then returns warnings for keys that don't match. If all keys use the same style
/// (or there are fewer than 2 keys), no warnings are produced.
pub fn find_mixed_tag_key_styles(entries: &[TagKeyEntry]) -> Vec<TagKeyWarning> {
    if entries.len() < 2 {
        return vec![];
    }

    let mut pascal_count = 0usize;
    let mut snake_count = 0usize;
    for e in entries {
        match e.style {
            TagKeyStyle::PascalCase => pascal_count += 1,
            TagKeyStyle::SnakeCase => snake_count += 1,
            TagKeyStyle::Other => {}
        }
    }

    // No mixed styles if everything is one style (or all Other)
    if pascal_count == 0 || snake_count == 0 {
        return vec![];
    }

    // Dominant style is whichever has more keys
    let dominant = if pascal_count >= snake_count {
        TagKeyStyle::PascalCase
    } else {
        TagKeyStyle::SnakeCase
    };

    entries
        .iter()
        .filter(|e| e.style != dominant && e.style != TagKeyStyle::Other)
        .map(|e| TagKeyWarning {
            key: e.key.clone(),
            expected_style: dominant,
            line: e.line,
            column: e.column,
            file: e.file.clone(),
        })
        .collect()
}

pub fn located_mixed_tag_key_styles<D, B: crate::resource::BindingAddress>(
    tree: &crate::parser::AnalysisTree<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    entries: &[TagKeyEntry],
) -> Vec<crate::parser::Located<TagKeyWarning>>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    use crate::parser::AttributePathSegment;
    let warnings = find_mixed_tag_key_styles(entries);
    let mut by_key = HashMap::new();
    for warning in &warnings {
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::LintWarningMatch, 1);
        by_key
            .entry((warning.file.as_deref(), warning.line, warning.column))
            .or_insert(warning);
    }
    tree.resources()
        .flat_map(|resource| resource.attribute_keys())
        .chain(tree.syntax().attribute_keys())
        .filter_map(|key| {
            let [
                AttributePathSegment::Attribute(attribute),
                AttributePathSegment::Key(_),
            ] = key.path()
            else {
                return None;
            };
            if attribute != "tags" {
                return None;
            }
            #[cfg(any(test, feature = "test-utils"))]
            crate::test_work::record(crate::test_work::Operation::LintWarningMatch, 1);
            by_key
                .get(&(
                    Some(key.location().file().path()),
                    key.location().span().start_line(),
                    key.location().span().start_column() - 1,
                ))
                .map(|warning| {
                    crate::parser::Located::new(key.location().clone(), (*warning).clone())
                })
        })
        .collect()
}

/// Format the shared CLI/LSP message for a mixed tag-key style warning.
pub fn mixed_tag_key_style_message(warning: &TagKeyWarning) -> String {
    let style_name = match warning.expected_style {
        TagKeyStyle::PascalCase => "PascalCase",
        TagKeyStyle::SnakeCase => "snake_case",
        // `find_mixed_tag_key_styles` only chooses one of the two counted
        // casing styles as dominant, so an `Other` warning is invalid state.
        TagKeyStyle::Other => unreachable!("mixed tag-key dominant style cannot be Other"),
    };
    format!(
        "Tag key '{}' doesn't match the dominant style ({}). Use consistent casing for tag keys.",
        warning.key, style_name
    )
}

/// The lint discards a secret key before constructing a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateAttrName {
    Public(String),
    InsideSecret,
}

impl std::fmt::Display for DuplicateAttrName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Public(name) => f.write_str(name),
            Self::InsideSecret => f.write_str("<redacted>"),
        }
    }
}

/// A duplicate attribute warning with its source position and first occurrence line.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateAttr {
    /// The attribute name that is duplicated
    name: DuplicateAttrName,
    /// 1-indexed line number of the duplicate occurrence
    pub line: usize,
    /// 1-indexed character column of the duplicate occurrence
    pub column: usize,
    /// 1-indexed exclusive end column of the duplicate key span
    pub end_column: usize,
    /// 1-indexed line number of the first occurrence
    pub first_line: usize,
}

/// Find duplicate keyed members within the same parsed container.
///
/// This walks the pest parse tree, so syntactically distinct nested containers
/// are checked independently. If `source` does not parse, this returns no
/// results because the parse error is reported elsewhere.
///
/// Direct members of `exports`, `attributes`, and `arguments` blocks are not
/// compared: they use their own grammar rules, and directory loading reports
/// their name collisions as hard duplicate-declaration errors. Emitting a
/// second warning that says the last value wins would contradict that error.
pub fn find_duplicate_attrs(source: &str) -> Vec<DuplicateAttr> {
    find_duplicate_attrs_in_source(&crate::parser::DiagnosticFileText::new(source))
}

pub fn find_duplicate_attrs_in_source(
    source: &crate::parser::DiagnosticFileText<'_>,
) -> Vec<DuplicateAttr> {
    let Some(text) = source.public_text() else {
        return Vec::new();
    };

    let Ok(preprocessed_file) = preprocess_file(text) else {
        return Vec::new();
    };
    let Ok(pairs) = preprocessed_file.parse() else {
        return Vec::new();
    };

    let mut results = Vec::new();
    let source = crate::parser::LintSource::new(source).expect("public source was admitted");
    for pair in pairs {
        find_duplicate_attrs_in_pair(pair, &preprocessed_file, &source, &mut results);
    }
    results.sort_by_key(|duplicate| (duplicate.line, duplicate.column));
    results
}

impl DuplicateAttr {
    pub fn name(&self) -> &DuplicateAttrName {
        &self.name
    }

    /// Location-free wording shared by diagnostic clients.
    pub fn message(&self) -> String {
        self.format_message(false)
    }

    /// Legacy validate wording, including the duplicate occurrence line.
    pub fn message_with_location(&self) -> String {
        self.format_message(true)
    }

    fn format_message(&self, include_location: bool) -> String {
        let occurrence = if include_location {
            format!(" at line {}", self.line)
        } else {
            String::new()
        };
        format!(
            "Duplicate attribute '{}'{occurrence} (first defined on line {}). The last value will be used.",
            self.name, self.first_line
        )
    }
}

#[cfg(test)]
mod secret_duplicate_tests {
    use super::*;

    #[test]
    fn constraint_secret_calls_own_duplicate_names_and_locations() {
        let source = "require secret(x[{ 'private-key' = 1 'private-key' = 2 }]), 'no'";
        let findings = find_duplicate_attrs(source);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0].name(),
            DuplicateAttrName::InsideSecret
        ));
        assert!(!format!("{:?}", findings[0]).contains("private-key"));
        assert_eq!(findings[0].column, source.find("secret").unwrap() + 1);
        assert_eq!(findings[0].end_column, findings[0].column + 6);
    }

    #[test]
    fn duplicate_attribute_name_is_typed_before_rendering() {
        let secret =
            find_duplicate_attrs("let hidden = secret({ 'private-key' = 1 'private-key' = 2 })");
        assert!(matches!(secret[0].name(), DuplicateAttrName::InsideSecret));
        assert!(!format!("{:?}", secret[0]).contains("private-key"));
        assert_eq!(
            secret[0].message(),
            "Duplicate attribute '<redacted>' (first defined on line 1). The last value will be used."
        );
        assert_eq!(
            secret[0].message_with_location(),
            "Duplicate attribute '<redacted>' at line 1 (first defined on line 1). The last value will be used."
        );
        // Identical display text must not turn a public authored key into a secret.
        let public = find_duplicate_attrs("let value = { '<redacted>' = 1 '<redacted>' = 2 }");
        assert!(matches!(public[0].name(), DuplicateAttrName::Public(_)));
        assert_eq!(public[0].name().to_string(), "<redacted>");
    }

    #[test]
    fn duplicate_keys_inside_secrets_never_retain_private_names_or_spans() {
        for expression in [
            "secret({ 'private-key' = 'a' 'private-key' = 'b' })",
            "secret([{ nested = { 'private-key' = 'a' 'private-key' = 'b' } }])",
            "secret(secret({ 'private-key' = 'a' 'private-key' = 'b' }))",
            "secret({\n 'private-key' = 'a'\n 'private-key' = 'b'\n})",
        ] {
            let source = format!("exports {{ hidden = {expression} }}");
            let findings = find_duplicate_attrs(&source);
            assert_eq!(findings.len(), 1);
            let finding = &findings[0];
            assert_eq!(finding.name.to_string(), "<redacted>");
            assert!(!finding.message().contains("private-key"));
            assert_eq!(finding.column, source.find("secret").unwrap() + 1);
            assert_eq!(finding.end_column, finding.column + 6);
            assert_eq!(finding.first_line, 1);
        }
    }

    #[test]
    fn piped_secrets_own_the_duplicates_in_their_input() {
        let source = "let hidden = { 'private-key' = 'a' 'private-key' = 'b' } |> secret()";
        let findings = find_duplicate_attrs(source);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.name.to_string(), "<redacted>");
        assert_eq!(finding.column, source.find("secret").unwrap() + 1);
        assert_eq!(finding.end_column, finding.column + 6);
    }

    #[test]
    fn pipeline_arguments_after_secret_keep_their_public_locations() {
        let source = "let hidden = { 'private-key' = 'a' 'private-key' = 'b' } |> secret() |> passthrough({ public = 1 public = 2 })";
        let findings = find_duplicate_attrs(source);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].name.to_string(), "<redacted>");
        assert_eq!(findings[1].name.to_string(), "public");
        assert_eq!(findings[1].column, source.rfind("public").unwrap() + 1);
    }
}

fn find_duplicate_attrs_in_pair<'source>(
    pair: Pair<'source, Rule>,
    preprocessed_file: &PreprocessedFile,
    source: &crate::parser::LintSource<'_>,
    results: &mut Vec<DuplicateAttr>,
) {
    let children: Vec<_> = pair.into_inner().collect();
    let mut first_spans: HashMap<String, Span<'source>> = HashMap::new();

    for child in &children {
        let Some((name, key_span)) = keyed_member_name(child.clone()) else {
            continue;
        };

        if let Some(first_span) = first_spans.get(&name) {
            // Map byte ownership first; LintSource's shared position index
            // supplies scalar/editor coordinates without recounting the line.
            let key = source.admit_key(
                name,
                preprocessed_file.original_byte(key_span.start())
                    ..preprocessed_file.original_byte(key_span.end()),
                preprocessed_file.original_byte(first_span.start()),
            );
            let start = source.position(key.range.start);
            let end = source.position(key.range.end);
            let first = source.position(key.first);
            results.push(DuplicateAttr {
                name: match key.name {
                    crate::parser::AdmittedLintName::Public(name) => {
                        DuplicateAttrName::Public(name)
                    }
                    crate::parser::AdmittedLintName::InsideSecret => {
                        DuplicateAttrName::InsideSecret
                    }
                },
                line: start.0,
                column: start.1,
                end_column: end.1,
                first_line: first.0,
            });
        } else {
            first_spans.insert(name, key_span);
        }
    }

    for child in children {
        find_duplicate_attrs_in_pair(child, preprocessed_file, source, results);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberKeyKind {
    Wrapper,
    KeyedFirstChild,
    Fixed,
}

fn member_key_kind(rule: Rule) -> Option<MemberKeyKind> {
    match rule {
        Rule::block_content
        | Rule::arguments_param_attr
        | Rule::validation_block_attr
        | Rule::import_state_attr
        | Rule::moved_attr => Some(MemberKeyKind::Wrapper),
        Rule::attribute | Rule::map_entry | Rule::module_call_arg => {
            Some(MemberKeyKind::KeyedFirstChild)
        }
        Rule::arg_description_attr
        | Rule::arg_default_attr
        | Rule::validation_condition_attr
        | Rule::validation_error_message_attr
        | Rule::import_to_attr
        | Rule::import_id_attr
        | Rule::removed_attr
        | Rule::moved_from_attr
        | Rule::moved_to_attr => Some(MemberKeyKind::Fixed),
        _ => None,
    }
}

fn keyed_member_name<'i>(pair: Pair<'i, Rule>) -> Option<(String, Span<'i>)> {
    match member_key_kind(pair.as_rule())? {
        MemberKeyKind::Wrapper => keyed_member_name(pair.into_inner().next()?),
        MemberKeyKind::KeyedFirstChild => {
            let key_pair = pair.into_inner().next()?;
            let key_span = key_pair.as_span();
            let name = extract_key_string(key_pair).ok()?;
            Some((name, key_span))
        }
        MemberKeyKind::Fixed => fixed_keyword(pair),
    }
}

fn fixed_keyword<'i>(pair: Pair<'i, Rule>) -> Option<(String, Span<'i>)> {
    let text = pair.as_str();
    let key_end = text
        .char_indices()
        .find_map(|(index, character)| {
            (character.is_whitespace() || character == '=').then_some(index)
        })
        .unwrap_or(text.len());
    let keyword = text.get(..key_end)?;
    if keyword.is_empty() {
        return None;
    }
    let key_span = pair.as_span().get(..key_end)?;
    Some((keyword.to_string(), key_span))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parsed_list_literal_lints_keep_their_existing_projection() {
        let source = "let hidden = secret({\n  items = []\n})\n";
        let attributes = ["items".to_owned()].into_iter().collect();
        assert_eq!(
            find_list_literal_attrs(source, &attributes),
            vec![("items".to_owned(), 2)]
        );
    }

    #[test]
    fn round7_secret_braces_do_not_influence_public_tag_style() {
        let source = "let value = {\n  tags = {\n    hidden = secret('}')\n    PublicKey = 'x'\n    one = 'x'\n    two = 'x'\n  }\n}\n";
        let warnings = super::find_mixed_tag_key_styles(&super::collect_tag_keys(source));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "PublicKey");
        assert_eq!(warnings[0].line, 4);
        let public = source.replace("secret('}')", "'}'");
        assert!(super::find_mixed_tag_key_styles(&super::collect_tag_keys(&public)).is_empty());
    }

    use super::*;

    #[test]
    fn secret_tag_keys_never_influence_public_style_dominance() {
        let source = "let public = {\n tags = {\n Public = 'x'\n one = 'x'\n two = 'x'\n }\n}\nlet hidden = secret({\n tags = {\n A = 'x'\n B = 'x'\n C = 'x'\n }\n})\n";
        let keys = collect_tag_keys(source);
        assert_eq!(
            keys.iter().map(|key| key.key.as_str()).collect::<Vec<_>>(),
            ["Public", "one", "two"]
        );
        let warnings = find_mixed_tag_key_styles(&keys);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "Public");
        assert_eq!(warnings[0].expected_style, TagKeyStyle::SnakeCase);
        assert_eq!(warnings[0].line, 3);
    }

    #[test]
    fn naming_warnings_never_retain_headers_owned_by_a_secret() {
        let body = "<<EOF\nlet PrivateName = 'CANARY'\nfor PrivateIndex in [] {}\nEOF";
        let source = format!("let hidden = secret({body}\n)\nlet PublicName = 1\n");
        let warnings = find_non_snake_case_bindings(&source);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].name, "PublicName");
        assert_eq!(warnings[0].line, 6);

        let public = format!("let text = {body}\nlet PublicName = 1\n");
        let warnings = find_non_snake_case_bindings(&public);
        assert_eq!(
            warnings
                .iter()
                .map(|warning| warning.name.as_str())
                .collect::<Vec<_>>(),
            ["PrivateName", "PrivateIndex", "PublicName"]
        );
        assert_eq!(warnings[2].line, 5);

        for source in [
            "let secret('NAME_CANARY') = 1",
            "let holder(secret('NAME_CANARY')) = 1",
            "for secret('A','B') in [] {}",
        ] {
            assert!(find_non_snake_case_bindings(source).is_empty());
        }
    }

    #[test]
    fn tag_key_population_does_not_retain_secret_map_keys() {
        let map = "{\n  tags = {\n    PublicKey = 'a'\n    private_key = 'b'\n  }\n}";
        for source in [
            format!("let hidden = secret({map})\n"),
            format!("let hidden = {map} |> secret()\n"),
        ] {
            let keys = collect_tag_keys(&source);
            assert!(keys.is_empty(), "{keys:?}");
        }
        assert_eq!(
            collect_tag_keys(map)
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["PublicKey", "private_key"]
        );
    }
    use crate::parser::{ModuleCall, ParsedFile, UseStatement};
    use crate::schema::{
        CompiledAttributeType as AttributeType, CompiledStructField as StructField,
    };

    const NOT_COMPARED: &[(Rule, &str)] = &[
        (
            Rule::arguments_param,
            "directory loading reports duplicate declarations as hard errors",
        ),
        (
            Rule::attributes_param,
            "directory loading reports duplicate declarations as hard errors",
        ),
        (
            Rule::exports_param,
            "directory loading reports duplicate declarations as hard errors",
        ),
        (
            Rule::wait_until_attr,
            "the semantic parser rejects duplicate wait members",
        ),
        (
            Rule::wait_timeout_attr,
            "the semantic parser rejects duplicate wait members",
        ),
        (
            Rule::wait_depends_on_attr,
            "the semantic parser rejects duplicate wait members",
        ),
        (Rule::let_binding, "bindings are not keyed members"),
        (Rule::local_binding, "bindings are not keyed members"),
        (Rule::fn_local_let, "bindings are not keyed members"),
        (Rule::for_local_binding, "bindings are not keyed members"),
        (Rule::if_local_binding, "bindings are not keyed members"),
        (
            Rule::fn_param,
            "function parameters are not keyed members; duplicate parameters are a separate concern",
        ),
    ];

    const WRAPPER_RULES: &[Rule] = &[
        Rule::block_content,
        Rule::arguments_param_attr,
        Rule::validation_block_attr,
        Rule::import_state_attr,
        Rule::moved_attr,
    ];

    fn grammar_rule_name(line: &str) -> Option<(&str, &str)> {
        if line.starts_with(char::is_whitespace) {
            return None;
        }
        let (name, body) = line.split_once('=')?;
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return None;
        }
        let body = body.trim_start();
        ["{", "@{", "${", "_{", "!{"]
            .iter()
            .any(|prefix| body.starts_with(prefix))
            .then_some((name, body))
    }

    fn grammar_rule_bodies(grammar: &str) -> Vec<(String, String)> {
        let mut rules = Vec::new();
        let mut current: Option<(String, String)> = None;

        for line in grammar.lines() {
            if let Some((name, body)) = grammar_rule_name(line) {
                if let Some(rule) = current.take() {
                    rules.push(rule);
                }
                current = Some((name.to_string(), body.to_string()));
            } else if let Some((_, body)) = &mut current {
                body.push('\n');
                body.push_str(line);
            }
        }
        if let Some(rule) = current {
            rules.push(rule);
        }

        rules
    }

    fn contains_equals_token(body: &str) -> bool {
        let bytes = body.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            if bytes[index] != b'"' {
                index += 1;
                continue;
            }

            let content_start = index + 1;
            index = content_start;
            let mut escaped = false;
            while index < bytes.len() {
                match (bytes[index], escaped) {
                    (_, true) => escaped = false,
                    (b'\\', false) => escaped = true,
                    (b'"', false) => break,
                    _ => {}
                }
                index += 1;
            }
            if bytes.get(content_start..index) == Some(b"=".as_slice()) {
                return true;
            }
            index += usize::from(index < bytes.len());
        }
        false
    }

    #[test]
    fn every_assignment_rule_has_an_explicit_duplicate_member_policy() {
        let grammar = include_str!("parser/carina.pest");
        let assignment_rule_names: HashSet<_> = grammar_rule_bodies(grammar)
            .into_iter()
            .filter_map(|(name, body)| contains_equals_token(&body).then_some(name))
            .collect();

        assert!(!assignment_rule_names.is_empty());
        assert!(assignment_rule_names.contains("attribute"));

        let rules_by_name: HashMap<_, _> = Rule::all_rules()
            .iter()
            .copied()
            .map(|rule| (format!("{rule:?}"), rule))
            .collect();

        for &(rule, reason) in NOT_COMPARED {
            assert!(!reason.is_empty(), "{rule:?} needs an exclusion reason");
            assert_eq!(
                member_key_kind(rule),
                None,
                "excluded rule {rule:?} must not be classified",
            );
            assert!(
                assignment_rule_names.contains(&format!("{rule:?}")),
                "excluded rule {rule:?} no longer contains an assignment token",
            );
        }

        for &rule in WRAPPER_RULES {
            assert_eq!(
                member_key_kind(rule),
                Some(MemberKeyKind::Wrapper),
                "wrapper rule {rule:?} must remain explicitly classified",
            );
        }

        for name in assignment_rule_names {
            let rule = *rules_by_name
                .get(&name)
                .unwrap_or_else(|| panic!("grammar rule {name} is missing from Rule::all_rules()"));
            let excluded = NOT_COMPARED
                .iter()
                .any(|&(not_compared, _)| not_compared == rule);
            assert!(
                member_key_kind(rule).is_some() || excluded,
                "assignment rule {name} is neither classified nor explicitly excluded",
            );
        }
    }

    fn assert_duplicate_lint_source_parses(source: &str) {
        let preprocessed_file = crate::parser::preprocess_file(source)
            .expect("duplicate-lint suppression fixture must preprocess");
        preprocessed_file
            .parse()
            .expect("duplicate-lint suppression fixture must parse");
    }

    #[test]
    fn test_find_list_literal_attrs_detects_list_literal() {
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress = [{
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
    }]
}
"#;

        let attr_names: HashSet<String> =
            ["security_group_ingress".to_string()].into_iter().collect();
        let results = find_list_literal_attrs(source, &attr_names);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "security_group_ingress");
        assert_eq!(results[0].1, 4); // line 4 (1-indexed)
    }

    #[test]
    fn test_find_list_literal_attrs_ignores_block_syntax() {
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 80
        to_port = 80
    }
}
"#;

        let attr_names: HashSet<String> =
            ["security_group_ingress".to_string()].into_iter().collect();
        let results = find_list_literal_attrs(source, &attr_names);
        assert!(
            results.is_empty(),
            "Block syntax should not produce lint warnings"
        );
    }

    #[test]
    fn test_find_list_literal_attrs_ignores_non_listed_attrs() {
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    tags = ["a", "b"]
}
"#;

        // "tags" is not in the list of List<Struct> attr names
        let attr_names: HashSet<String> =
            ["security_group_ingress".to_string()].into_iter().collect();
        let results = find_list_literal_attrs(source, &attr_names);
        assert!(
            results.is_empty(),
            "Non-listed attributes should not produce lint warnings"
        );
    }

    #[test]
    fn test_find_duplicate_attrs_detects_duplicate() {
        let source = r#"
let igw_attachment = awscc.ec2.vpc_gateway_attachment {
    vpc_id              = vpc.vpc_id
    internet_gateway_id = igw.internet_gateway_id
    internet_gateway_id = igw.internet_gateway_id
}
"#;
        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.to_string(), "internet_gateway_id");
        assert_eq!(results[0].line, 5); // duplicate on line 5
        assert_eq!(results[0].first_line, 4); // first on line 4
    }

    #[test]
    fn test_find_duplicate_attrs_maps_lines_after_heredoc() {
        let source = r#"let thing = mock.test.Thing {
    policy = <<EOT
hello
EOT
    a = 1
    a = 2
}
"#;

        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1, "expected one duplicate: {results:?}");
        assert_eq!(results[0].name.to_string(), "a");
        assert_eq!(results[0].line, 6);
        assert_eq!(results[0].first_line, 5);
    }

    #[test]
    fn test_find_duplicate_attrs_detects_duplicate_before_heredoc() {
        let source = r#"let thing = mock.test.Thing {
    a = 1
    a = 2
    policy = <<EOT
hello
EOT
}
"#;

        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1, "expected one duplicate: {results:?}");
        assert_eq!(results[0].name.to_string(), "a");
        assert_eq!(results[0].line, 3);
        assert_eq!(results[0].first_line, 2);
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_same_name_in_nested_map() {
        let source = r#"let policy = awscc.cloudfront.ResponseHeadersPolicy {
    response_headers_policy_config = {
        security_headers_config = {
            content_security_policy = {
                content_security_policy = "default-src 'self'"
                override = true
            }
        }
    }
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert_eq!(find_duplicate_attrs(source), Vec::<DuplicateAttr>::new());
    }

    #[test]
    fn test_find_duplicate_attrs_reports_duplicate_in_same_resource() {
        let source = r#"let thing = test.example.Thing {
    a = 1
    a = 2
}
"#;

        assert_eq!(
            find_duplicate_attrs(source),
            vec![DuplicateAttr {
                name: DuplicateAttrName::Public("a".to_string()),
                line: 3,
                column: 5,
                end_column: 6,
                first_line: 2,
            }]
        );
    }

    #[test]
    fn test_find_duplicate_attrs_reports_duplicate_in_nested_map_literal() {
        let source = r#"let thing = test.example.Thing {
    config = {
        a = 1
        a = 2
    }
}
"#;

        assert_eq!(
            find_duplicate_attrs(source),
            vec![DuplicateAttr {
                name: DuplicateAttrName::Public("a".to_string()),
                line: 4,
                column: 9,
                end_column: 10,
                first_line: 3,
            }]
        );
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_open_brace_in_string() {
        let source = r#"let thing = test.example.Thing {
    a = 1
    description = "{"
    a = 2
}
"#;

        assert_eq!(
            find_duplicate_attrs(source),
            vec![DuplicateAttr {
                name: DuplicateAttrName::Public("a".to_string()),
                line: 4,
                column: 5,
                end_column: 6,
                first_line: 2,
            }]
        );
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_close_brace_in_string() {
        let source = r#"let thing = test.example.Thing {
    a = 1
    nested {
        description = "}"
        a = 2
    }
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert_eq!(find_duplicate_attrs(source), Vec::<DuplicateAttr>::new());
    }

    #[test]
    fn test_find_duplicate_attrs_allows_repeated_nested_blocks() {
        let source = r#"let thing = test.example.Thing {
    rule {
        value = 1
    }
    rule {
        value = 2
    }
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert_eq!(find_duplicate_attrs(source), Vec::<DuplicateAttr>::new());
    }

    #[test]
    fn test_find_duplicate_attrs_no_false_positive() {
        let source = r#"
awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
    enable_dns_support = true
}
"#;
        assert_duplicate_lint_source_parses(source);
        let results = find_duplicate_attrs(source);
        assert!(results.is_empty(), "No duplicates should be found");
    }

    #[test]
    fn test_find_duplicate_attrs_different_blocks() {
        // Same attr name in different blocks should NOT be flagged
        let source = r#"
awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.Subnet {
    cidr_block = "10.0.1.0/24"
}
"#;
        assert_duplicate_lint_source_parses(source);
        let results = find_duplicate_attrs(source);
        assert!(
            results.is_empty(),
            "Same attr in different blocks should not be flagged"
        );
    }

    #[test]
    fn test_find_duplicate_attrs_nested_block() {
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress {
        ip_protocol = "tcp"
        from_port = 80
        from_port = 443
    }
}
"#;
        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.to_string(), "from_port");
    }

    #[test]
    fn test_find_duplicate_attrs_list_literal_block() {
        // List literal syntax: attr = [{ ... }]
        // Duplicate within the list literal block should be detected
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress = [{
        ip_protocol = "tcp"
        ip_protocol = "udp"
    }]
}
"#;
        let results = find_duplicate_attrs(source);
        assert_eq!(
            results.len(),
            1,
            "Should detect duplicate in list literal block. Got: {:?}",
            results
        );
        assert_eq!(results[0].name.to_string(), "ip_protocol");
    }

    #[test]
    fn test_find_duplicate_attrs_list_literal_no_cross_block() {
        // group_description in the outer block should not conflict with
        // attrs inside the list literal block after }] closes the inner block
        let source = r#"
awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress = [{
        ip_protocol = "tcp"
    }]
    group_description = "duplicate"
}
"#;
        let results = find_duplicate_attrs(source);
        // Should detect the duplicate group_description in the outer block
        assert_eq!(
            results.len(),
            1,
            "Should detect duplicate in outer block despite list literal. Got: {:?}",
            results
        );
        assert_eq!(results[0].name.to_string(), "group_description");
    }

    #[test]
    fn test_find_duplicate_attrs_provider_block() {
        let source = r#"
provider awscc {
    region = aws.Region.ap_northeast_1
    region = aws.Region.us_east_1
}
"#;
        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name.to_string(), "region");
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_direct_exports_members() {
        let source = r#"
exports {
    x = "one"
    x = "two"
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert!(find_duplicate_attrs(source).is_empty());
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_direct_module_attribute_members() {
        let source = r#"
attributes {
    x = "one"
    x = "two"
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert!(find_duplicate_attrs(source).is_empty());
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_direct_argument_members() {
        let source = r#"
arguments {
    x: String
    x: String
}
"#;

        assert_duplicate_lint_source_parses(source);
        assert!(find_duplicate_attrs(source).is_empty());
    }

    #[test]
    fn test_find_duplicate_attrs_does_not_treat_identifier_prefix_as_declaration_block() {
        let source = r#"
let value = mock.test.Thing {
    exports_config = {
        key = "one"
        key = "two"
    }
}
"#;

        let results = find_duplicate_attrs(source);
        assert_eq!(
            results.len(),
            1,
            "expected duplicate-key warning: {results:?}"
        );
        assert_eq!(results[0].name.to_string(), "key");
    }

    #[test]
    fn test_find_duplicate_attrs_reports_duplicate_module_call_arguments() {
        let source = r#"let network_module = use { source = "./network" }
let network = network_module {
    cidr = "10.0.0.0/16"
    cidr = "10.1.0.0/16"
}
"#;

        crate::parser::parse(
            source,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("the parser silently keeps the last module-call argument");

        let results = find_duplicate_attrs(source);
        assert_eq!(results.len(), 1, "expected one duplicate: {results:?}");
        assert_eq!(results[0].name.to_string(), "cidr");
        assert_eq!(results[0].line, 4);
        assert_eq!(results[0].first_line, 3);
    }

    #[test]
    fn test_find_duplicate_attrs_reports_silently_overwritten_argument_members() {
        let source = r#"arguments {
    port: Int {
        description = "first"
        description = "second"
        default = 80
        default = 443
        validation {
            condition = port > 0
            condition = port > 1
            error_message = "first"
            error_message = "second"
        }
    }
}
"#;

        crate::parser::parse(
            source,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("the parser silently keeps the last dedicated argument member");

        let actual: Vec<_> = find_duplicate_attrs(source)
            .into_iter()
            .map(|duplicate| {
                (
                    duplicate.name.to_string(),
                    duplicate.line,
                    duplicate.first_line,
                )
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                ("description".to_string(), 4, 3),
                ("default".to_string(), 6, 5),
                ("condition".to_string(), 9, 8),
                ("error_message".to_string(), 11, 10),
            ]
        );
    }

    #[test]
    fn test_find_duplicate_attrs_reports_silently_overwritten_state_members() {
        let source = r#"import {
    to = awscc.ec2.Vpc "first"
    to = awscc.ec2.Vpc "second"
    id = "first"
    id = "second"
}
removed {
    from = awscc.ec2.Vpc "first"
    from = awscc.ec2.Vpc "second"
}
moved {
    from = awscc.ec2.Vpc "first"
    from = awscc.ec2.Vpc "second"
    to = awscc.ec2.Vpc "first"
    to = awscc.ec2.Vpc "second"
}
"#;

        crate::parser::parse(
            source,
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("the parser silently keeps the last dedicated state member");

        let actual: Vec<_> = find_duplicate_attrs(source)
            .into_iter()
            .map(|duplicate| {
                (
                    duplicate.name.to_string(),
                    duplicate.line,
                    duplicate.first_line,
                )
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                ("to".to_string(), 3, 2),
                ("id".to_string(), 5, 4),
                ("from".to_string(), 9, 8),
                ("from".to_string(), 13, 12),
                ("to".to_string(), 15, 14),
            ]
        );
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_wait_members_rejected_by_parser() {
        let cases = [
            (
                "until",
                r#"let source = mock.test.Thing {}
let ready = wait source {
    until = source.ready == true
    until = source.ready == false
}
"#,
            ),
            (
                "timeout",
                r#"let source = mock.test.Thing {}
let ready = wait source {
    until = source.ready == true
    timeout = 30s
    timeout = 60s
}
"#,
            ),
            (
                "depends_on",
                r#"let source = mock.test.Thing {}
let ready = wait source {
    until = source.ready == true
    depends_on = [source]
    depends_on = []
}
"#,
            ),
        ];

        for (name, source) in cases {
            assert_duplicate_lint_source_parses(source);
            let error = crate::parser::parse(
                source,
                &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .expect_err("the parser must reject duplicate wait members");
            assert!(
                error.to_string().contains(&format!("duplicate `{name}`")),
                "unexpected parser error for {name}: {error}"
            );
            assert!(
                find_duplicate_attrs(source).is_empty(),
                "a hard parser error must not also produce a last-value warning for {name}"
            );
        }
    }

    #[test]
    fn test_find_duplicate_attrs_returns_duplicates_in_source_order() {
        let source = r#"let thing = mock.test.Thing {
    config = {
        nested = "first"
        nested = "second"
    }
    top_level = "first"
    top_level = "second"
}
"#;

        let actual: Vec<_> = find_duplicate_attrs(source)
            .into_iter()
            .map(|duplicate| (duplicate.name.to_string(), duplicate.line))
            .collect();
        assert_eq!(
            actual,
            vec![("nested".to_string(), 4), ("top_level".to_string(), 7),]
        );
    }

    #[test]
    fn test_list_struct_attr_names() {
        let schema = ResourceSchema::new("ec2.SecurityGroup")
            .attribute(
                crate::schema::CompiledAttributeSchema::optional(
                    "security_group_ingress",
                    AttributeType::list(AttributeType::struct_(
                        "Ingress".to_string(),
                        vec![StructField::optional("ip_protocol", AttributeType::string()).build()],
                    )),
                )
                .build(),
            )
            .attribute(
                crate::schema::CompiledAttributeSchema::optional(
                    "tags",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .attribute(
                crate::schema::CompiledAttributeSchema::optional(
                    "group_description",
                    AttributeType::string(),
                )
                .build(),
            );

        let names = list_struct_attr_names(&schema);
        assert!(names.contains("security_group_ingress"));
        assert!(
            !names.contains("tags"),
            "List<String> should not be included"
        );
        assert!(
            !names.contains("group_description"),
            "String should not be included"
        );
    }

    #[test]
    fn round11_collectors_match_main_messages_positions_and_order() {
        for source in [
            "for BadName, BadName, OtherName, Name in [] {}",
            "for For, or, BadName, Name, _Discard in [] {}",
            "  let BadName = '😀é'\r\nfor ÉName, BadName in [] {}\n",
            "// for BadName in [] {}\n# let BadName = 1\n/*\nfor CommentName in [] {}\n*/",
            "let text = '\nfor StringName in [] {}\n'\nlet ActualName = 1",
            "let hidden = secret('\nfor PrivateName in [] {}\n')\nfor PublicName in [] {}",
            "for BadName, BadName, BadName in [] {} // BadName",
            "let value = {\n tags = {\n PublicKey = 'x'\n one = 'x'\n two = 'x'\n }\n}\nlet hidden = secret({ tags = { PrivateKey = 'x' } })",
            "let value = {\n tags = {\n hidden = secret('}')\n PublicKey = 'x'\n one = 'x'\n two = 'x'\n }\n}",
            "",
        ] {
            let text = crate::parser::DiagnosticFileText::new(source);
            let Some(admitted) = crate::parser::LintSource::new(&text) else {
                continue;
            };
            let public = admitted.text();
            let expected_names = round11_main_reference::find_non_snake_case_bindings(public);
            let expected = expected_names
                .iter()
                .flat_map(|warning| {
                    let line = public.lines().nth(warning.line - 1).unwrap();
                    let byte = line.find(&warning.name).unwrap();
                    let start = line.as_ptr() as usize - public.as_ptr() as usize + byte;
                    admitted.collect_names(
                        [(start..start + warning.name.len(), warning.line)],
                        |name, line_number, range| {
                            let original_line = source.lines().nth(line_number - 1).unwrap();
                            let first = original_line.find(&name).unwrap();
                            let column = original_line[..first].encode_utf16().count();
                            let (span, _) = admitted.lint_ranges(range, &name, name.len());
                            (
                                name.clone(),
                                line_number,
                                span,
                                (
                                    (line_number - 1) as u32,
                                    column as u32,
                                    (column + name.len()) as u32,
                                ),
                            )
                        },
                    )
                })
                .collect::<Vec<_>>();
            let actual = non_snake_case_binding_findings(source)
                .into_iter()
                .map(|f| {
                    (
                        f.detail.name.clone(),
                        f.detail.line,
                        f.span.clone(),
                        f.editor_range(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{source}");
            let keys = collect_tag_keys(source);
            let main_keys = round11_main_reference::collect_tag_keys(public);
            assert_eq!(keys, main_keys, "{source}");
            assert_eq!(
                find_mixed_tag_key_styles(&keys),
                round11_main_reference::find_mixed_tag_key_styles(&main_keys),
                "{source}"
            );
        }
        let attributes = ["a", "a ", "a  ", "items", "items_extra", "日本", ""]
            .into_iter()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let source =
            "a   = []\nitems = []\nitems_extra = []\n日本 = []\n = []\n// items = []\n'items = []'";
        assert_eq!(
            find_list_literal_attrs(source, &attributes),
            round11_main_reference::find_list_literal_attrs(source, &attributes)
        );
    }

    fn round11_work(work: crate::test_work::Work) -> usize {
        use crate::test_work::Operation;
        [
            Operation::LintScanBytes,
            Operation::LintSearchBytes,
            Operation::LintIndexStep,
            Operation::LintWarningMatch,
            Operation::LintPositionBytes,
        ]
        .into_iter()
        .map(|operation| work.get(operation))
        .sum()
    }

    #[test]
    fn round11_for_binding_name_work_is_linear() {
        for upper in [false, true] {
            let run = |count| {
                let names = (0..count)
                    .map(|n| format!("{}{n:06}", if upper { "A" } else { "a" }))
                    .collect::<Vec<_>>()
                    .join(",");
                let source = format!("for {names} in [] {{}}");
                let (findings, work) =
                    crate::test_work::measure(|| non_snake_case_binding_findings(&source));
                assert_eq!(findings.len(), if upper { count } else { 0 });
                round11_work(work)
            };
            let small = run(16_000);
            let large = run(32_000);
            assert!(small > 0);
            assert!(
                large <= 3 * small,
                "for-name work {upper}: {small} -> {large}"
            );
        }
    }

    #[test]
    fn round11_list_literal_schema_lookup_work_is_linear() {
        let run = |count| {
            let attrs = (0..count)
                .map(|n| format!("attr{n:06}"))
                .collect::<HashSet<_>>();
            let source = (0..count)
                .map(|n| format!("attr{n:06} = []\n"))
                .collect::<String>();
            let (found, work) =
                crate::test_work::measure(|| find_list_literal_attrs(&source, &attrs));
            assert_eq!(found.len(), count);
            round11_work(work)
        };
        let small = run(128);
        let large = run(256);
        assert!(small > 0);
        assert!(large <= 3 * small, "schema-name work: {small} -> {large}");
    }

    #[test]
    fn round11_legacy_projection_work_is_linear() {
        let run = |count| {
            let source = format!(
                "??? {}",
                (0..count)
                    .map(|n| format!("Bad{n:06} "))
                    .collect::<String>()
            );
            let (_, work) = crate::test_work::measure(|| {
                let text = crate::parser::DiagnosticFileText::new(&source);
                let admitted = crate::parser::LintSource::new(&text).expect("public fixture");
                for (index, word) in source.split_whitespace().skip(1).enumerate() {
                    let start = 4 + index * 10;
                    admitted.lint_ranges(start..start + word.len(), word, word.len());
                }
            });
            round11_work(work)
        };
        let small = run(128);
        let large = run(256);
        assert!(small > 0);
        assert!(large <= 3 * small, "projection work: {small} -> {large}");
    }

    #[test]
    fn round11_tag_warning_attachment_work_is_linear() {
        let run = |count| {
            let keys = (0..count)
                .map(|n| format!("lower{n:06} = 'x'\n"))
                .chain((0..=count).map(|n| format!("Upper{n:06} = 'x'\n")))
                .collect::<String>();
            let source = format!(
                "let example = mock.test.resource {{ name = 'x'\ntags = {{\n{keys}}}\n}}\n"
            );
            let file = std::path::Path::new("round11.crn");
            let parsed = crate::parser::parse_file(
                &source,
                file,
                &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
            let entries = collect_tag_keys_for_file(&source, file);
            let (findings, work) =
                crate::test_work::measure(|| located_mixed_tag_key_styles(&tree, &entries));
            assert_eq!(findings.len(), count);
            round11_work(work)
        };
        let small = run(32);
        let large = run(64);
        assert!(small > 0);
        assert!(
            large <= 3 * small,
            "tag-attachment work: {small} -> {large}"
        );
    }

    #[test]
    fn test_pipe_preferred_direct_call_warns() {
        let source = r#"let name = join("-", parts)"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "join");
        assert_eq!(results[0].line, 1);
    }

    #[test]
    fn round8_many_pipe_hints_scan_in_bounded_time() {
        use crate::test_work::{Operation, measure};
        let source = format!("??? {}", "join('-', []) ".repeat(50_000));
        let last = 4 + 49_999 * "join('-', []) ".len();
        let (findings, work) = measure(|| pipe_preferred_direct_call_findings(&source));
        assert_eq!(work.get(Operation::ColumnBytes), 0);
        assert_eq!(work.get(Operation::PositionIndexBytes), source.len());
        assert!(work.get(Operation::PipeHintCharacter) <= source.chars().count());
        assert!(work.get(Operation::PositionQuery) <= 4 * 50_000);
        assert_eq!(findings.len(), 50_000);
        assert_eq!(findings[49_999].span().start_column, last + 1);
        // Preserve the legacy editor's first-pattern lookup on each line.
        assert_eq!(findings[49_999].editor_range(), (0, 4, 8));
    }

    #[test]
    fn round9_pipe_hint_work_scales_with_input() {
        use crate::test_work::{Operation, measure};
        let run = |count| {
            let source = format!("??? {}", "join('-', []) ".repeat(count));
            let (findings, work) = measure(|| pipe_preferred_direct_call_findings(&source));
            assert_eq!(findings.len(), count);
            work
        };
        let small = run(128);
        let large = run(256);
        assert!(small.get(Operation::PipeHintCharacter) > 0);
        for operation in [
            Operation::PipeHintCharacter,
            Operation::PositionIndexBytes,
            Operation::PositionQuery,
        ] {
            assert!(
                large.get(operation) <= 2 * small.get(operation) + 16,
                "{small:?} -> {large:?}"
            );
        }
    }

    #[test]
    fn round8_pipe_hints_match_legacy_messages_positions_and_order() {
        // Reference the old prefix-scanning algorithm, including its quote
        // heuristic and grouping by function rather than source position.
        type Snapshot = (
            String,
            usize,
            crate::parser::ParseWarningSpan,
            (u32, u32, u32),
        );
        fn legacy(source: &str) -> Vec<Snapshot> {
            let text = crate::parser::DiagnosticFileText::new(source);
            let Some(admitted) = crate::parser::LintSource::new(&text) else {
                return Vec::new();
            };
            let source = admitted.original_text();
            let mut results = Vec::new();
            for (line_idx, line) in source.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with('#') {
                    continue;
                }
                for &name in PIPE_PREFERRED_FUNCTIONS {
                    let pattern = format!("{name}(");
                    let mut search_from = 0;
                    while let Some(relative) = trimmed[search_from..].find(&pattern) {
                        let pos = search_from + relative;
                        search_from = pos + pattern.len();
                        if pos > 0 {
                            let prev = trimmed.as_bytes()[pos - 1];
                            if prev.is_ascii_alphanumeric() || prev == b'_' {
                                continue;
                            }
                        }
                        let before = &trimmed[..pos];
                        if before.contains("|>")
                            || before.chars().filter(|ch| *ch == '"').count() % 2 == 1
                            || before.trim_end().ends_with(':')
                        {
                            continue;
                        }
                        let start = trimmed.as_ptr() as usize - source.as_ptr() as usize + pos;
                        let (span, editor) =
                            admitted.lint_ranges(start..start + name.len(), &pattern, name.len());
                        results.push((
                            name.to_owned(),
                            line_idx + 1,
                            span,
                            (editor.0 as u32, editor.1 as u32, editor.2 as u32),
                        ));
                    }
                }
            }
            results
        }

        for source in [
            "let name = join(\"-\", parts)",
            "concat(a, b) join('-', xs) split(',', x) map('.id', xs) replace('a', 'b', x) join('-', ys)",
            "join('-', join('-', xs)) my_join('-', xs) rejoin('-', xs)",
            "'|>' join('-', xs)\n\"|>\" join('-', xs)\njoin('|>', xs) join('-', ys)",
            "\"join(a,b)\" join('-', xs)\n\"escaped \\\" join(a,b)\" join('-', ys)",
            "join('-', xs) |> join('-')\n# join('-', xs)\n// split(',', xs)",
            "tags: \u{2003}map(String) value = map('.id', xs)",
            "\tαjoin('-', xs) 😀split(',', xs) éjoin('-', ys)\r\n replace('é', '😀', text)",
            "??? join('-', []) secret('CANARY') join('-', [])",
            "let hidden = secret(join('-', []))\n",
            "",
        ] {
            let actual = pipe_preferred_direct_call_findings(source)
                .into_iter()
                .map(|finding| {
                    (
                        finding.detail.name.clone(),
                        finding.detail.line,
                        finding.span.clone(),
                        finding.editor_range(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, legacy(source), "{source}");
        }
    }

    #[test]
    fn test_pipe_preferred_pipe_form_no_warning() {
        let source = r#"let name = parts |> join("-")"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(results.is_empty(), "Pipe form should not warn");
    }

    #[test]
    fn test_pipe_preferred_single_arg_no_warning() {
        // flatten is not in PIPE_PREFERRED_FUNCTIONS
        let source = r#"let flat = flatten(list)"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(results.is_empty(), "Single-arg functions should not warn");
    }

    #[test]
    fn test_pipe_preferred_computation_no_warning() {
        // cidr_subnet is not in PIPE_PREFERRED_FUNCTIONS
        let source = r#"let subnet = cidr_subnet("10.0.0.0/16", 8, 1)"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(results.is_empty(), "Computation functions should not warn");
    }

    #[test]
    fn test_pipe_preferred_all_functions() {
        let source = r#"
let a = join("-", parts)
let b = split(",", str)
let c = map(".id", list)
let d = concat(extra, base)
let e = replace("old", "new", str)
"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].name, "join");
        assert_eq!(results[1].name, "split");
        assert_eq!(results[2].name, "map");
        assert_eq!(results[3].name, "concat");
        assert_eq!(results[4].name, "replace");
    }

    #[test]
    fn test_pipe_preferred_inside_string_no_warning() {
        let source = r#"let x = "join(a, b)""#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(
            results.is_empty(),
            "Function name inside string literal should not warn"
        );
    }

    #[test]
    fn test_pipe_preferred_no_false_positive_on_similar_name() {
        // "my_join(" should not match "join("
        let source = r#"let x = my_join("-", parts)"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(
            results.is_empty(),
            "Should not match when function name is part of a longer identifier"
        );
    }

    #[test]
    fn test_pipe_preferred_type_annotation_no_warning() {
        // `map(aws_account_id)` in a type position is a type annotation, not a
        // function call. Should not trigger pipe-form warning.
        let source = r#"exports {
  accounts: map(AwsAccountId) = {
    prod = x.y
  }
}"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert!(
            results.is_empty(),
            "map() in type annotation position should not trigger pipe warning. Got: {:?}",
            results
        );
    }

    #[test]
    fn test_pipe_preferred_type_annotation_list_no_warning() {
        let source = r#"attributes {
  items: list(String) = ["a", "b"]
}"#;
        let results = find_pipe_preferred_direct_calls(source);
        // `list` is not in PIPE_PREFERRED_FUNCTIONS, but ensure no regression
        assert!(results.is_empty());
    }

    #[test]
    fn test_pipe_preferred_comment_lines_no_warning() {
        let source = "# join(\"-\", parts)\n// split(\",\", str)";
        let results = find_pipe_preferred_direct_calls(source);
        assert!(
            results.is_empty(),
            "Comment lines should not produce warnings"
        );
    }

    // --- Naming convention tests ---

    #[test]
    fn test_is_snake_case_valid() {
        assert!(is_snake_case("my_vpc"));
        assert!(is_snake_case("vpc"));
        assert!(is_snake_case("a1"));
        assert!(is_snake_case("web_server_2"));
    }

    #[test]
    fn test_is_snake_case_invalid() {
        assert!(!is_snake_case("myVpc")); // camelCase
        assert!(!is_snake_case("MyVpc")); // PascalCase
        assert!(!is_snake_case("_internal")); // leading underscore
        assert!(!is_snake_case("trailing_")); // trailing underscore
        assert!(!is_snake_case("double__underscore")); // consecutive underscores
        assert!(!is_snake_case("1start")); // starts with digit
        assert!(!is_snake_case("")); // empty
    }

    #[test]
    fn test_naming_camel_case_warns() {
        let source = r#"let myVpc = awscc.ec2.Vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "myVpc");
        assert_eq!(results[0].line, 1);
    }

    #[test]
    fn test_naming_pascal_case_warns() {
        let source = r#"let MyVpc = awscc.ec2.Vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "MyVpc");
    }

    #[test]
    fn test_naming_snake_case_no_warning() {
        let source = r#"let my_vpc = awscc.ec2.Vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert!(results.is_empty(), "snake_case should not warn");
    }

    #[test]
    fn test_naming_underscore_prefix_skipped() {
        let source = r#"let _internal = awscc.ec2.Vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "Bindings starting with _ should be skipped"
        );
    }

    #[test]
    fn test_naming_for_loop_variable_warns() {
        let source = "for badName in items {\n    let x = badName\n}";
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "badName");
        assert_eq!(results[0].line, 1);
    }

    #[test]
    fn test_naming_for_loop_snake_case_no_warning() {
        let source = "for item in items {\n    let x = item\n}";
        let results = find_non_snake_case_bindings(source);
        assert!(results.is_empty());
    }

    #[test]
    fn test_naming_for_map_form_snake_case_no_warning() {
        // Map-form iteration: `for k, v in m`. Both bindings snake_case — no warning.
        // Previously the lint took the first whitespace-delimited token ("name,")
        // and warned because of the trailing comma.
        let source = "for name, account_id in orgs.accounts {\n    let x = name\n}";
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "map-form snake_case bindings should not warn, got: {:?}",
            results
        );
    }

    #[test]
    fn test_naming_for_map_form_pascal_case_warns_both() {
        // Both map-form bindings should be checked independently.
        let source = "for K, V in m {\n    let x = K\n}";
        let results = find_non_snake_case_bindings(source);
        assert_eq!(
            results.len(),
            2,
            "both map-form bindings should warn, got: {:?}",
            results
        );
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"K"), "expected K in {:?}", names);
        assert!(names.contains(&"V"), "expected V in {:?}", names);
        // Neither name should carry a comma.
        assert!(
            results.iter().all(|r| !r.name.contains(',')),
            "warning names must not contain commas, got: {:?}",
            results
        );
    }

    #[test]
    fn test_naming_for_map_form_mixed_warns_only_bad() {
        // One snake_case + one PascalCase — only the bad one warns.
        let source = "for key, BadValue in m {\n    let x = key\n}";
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1, "only one warning expected: {:?}", results);
        assert_eq!(results[0].name, "BadValue");
    }

    #[test]
    fn test_naming_for_iterable_contains_in_word() {
        // If the iterable's name contains " in " (as a substring of an
        // identifier), we still only split on the first occurrence. This
        // happens to be safe for identifiers because ` in ` — with its
        // surrounding spaces — cannot appear inside a single identifier.
        let source = "for v in items_in_group {\n}";
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "iterable containing 'in' substring should not confuse parser, got: {:?}",
            results
        );
    }

    #[test]
    fn test_naming_for_map_form_extra_whitespace() {
        // Extra spaces around the comma should not matter.
        let source = "for key,   value in m {\n}";
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "extra whitespace around comma should still yield snake_case names, got: {:?}",
            results
        );
    }

    #[test]
    fn test_naming_for_map_form_underscore_discard_skipped() {
        // A `_`-prefixed binding on either side should be skipped.
        let source = "for _k, v in m {\n    let x = v\n}";
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "discard binding should be skipped, got: {:?}",
            results
        );
    }

    #[test]
    fn test_naming_use_binding_warns() {
        let source = r#"let myModule = use { source = "./modules/web" }"#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "myModule");
    }

    #[test]
    fn test_naming_use_snake_case_no_warning() {
        let source = r#"let web_tier = use { source = "./modules/web" }"#;
        let results = find_non_snake_case_bindings(source);
        assert!(results.is_empty());
    }

    #[test]
    fn test_naming_comment_lines_no_warning() {
        let source = "// let myBadName = something\n# let AnotherBad = thing";
        let results = find_non_snake_case_bindings(source);
        assert!(
            results.is_empty(),
            "Comment lines should not produce warnings"
        );
    }

    #[test]
    fn test_naming_multiple_warnings() {
        let source = "let myVpc = awscc.ec2.Vpc {}\nlet MySubnet = awscc.ec2.Subnet {}\nlet good_name = awscc.ec2.igw {}";
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "myVpc");
        assert_eq!(results[1].name, "MySubnet");
    }

    #[test]
    fn test_naming_let_inside_block_checked() {
        // `let` inside a for body should still be checked
        let source = "for item in items {\n    let badName = item.value\n}";
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "badName");
        assert_eq!(results[0].line, 2);
    }

    // --- Tag key casing consistency tests ---

    #[test]
    fn test_collect_tag_keys_extracts_keys() {
        let source = r#"
tags = {
    Name = "my-vpc"
    environment = "prod"
}"#;
        let keys = collect_tag_keys(source);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, "Name");
        assert_eq!(keys[0].style, TagKeyStyle::PascalCase);
        assert_eq!(keys[0].column, 4);
        assert_eq!(keys[1].key, "environment");
        assert_eq!(keys[1].style, TagKeyStyle::SnakeCase);
        assert_eq!(keys[1].column, 4);
    }

    #[test]
    fn collect_tag_keys_uses_character_column_after_multibyte_whitespace() {
        let source = "tags = {\n\u{3000}\u{3000}managed_by = \"carina\"\n}";

        let keys = collect_tag_keys(source);

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "managed_by");
        assert_eq!(keys[0].line, 2);
        assert_eq!(
            keys[0].column, 2,
            "two U+3000 spaces are two characters but six UTF-8 bytes"
        );
    }

    #[test]
    fn collect_tag_keys_for_file_attaches_path_and_preserves_position() {
        let file = PathBuf::from("config/main.crn");
        let source = "tags = {\n\tmanaged_by = \"carina\"\n}";

        let keys = collect_tag_keys_for_file(source, &file);

        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].file.as_deref(), Some(file.as_path()));
        assert_eq!(keys[0].line, 2);
        assert_eq!(keys[0].column, 1);
    }

    #[test]
    fn mixed_tag_key_style_message_matches_cli_text() {
        let warning = TagKeyWarning {
            key: "managed_by".to_string(),
            expected_style: TagKeyStyle::PascalCase,
            line: 3,
            column: 8,
            file: Some(PathBuf::from("main.crn")),
        };

        assert_eq!(
            mixed_tag_key_style_message(&warning),
            "Tag key 'managed_by' doesn't match the dominant style (PascalCase). Use consistent casing for tag keys."
        );
    }

    #[test]
    fn find_mixed_tag_key_styles_propagates_file_and_column_provenance() {
        let majority_file = PathBuf::from("a.crn");
        let minority_file = PathBuf::from("b.crn");
        let mut keys = collect_tag_keys_for_file(
            "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n}",
            &majority_file,
        );
        keys.extend(collect_tag_keys_for_file(
            "tags = {\n    managed_by = \"carina\"\n}",
            &minority_file,
        ));

        let warnings = find_mixed_tag_key_styles(&keys);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].file.as_deref(), Some(minority_file.as_path()));
        assert_eq!(warnings[0].line, 2);
        assert_eq!(warnings[0].column, 4);
    }

    #[test]
    fn collect_all_tag_keys_deduplicates_two_aliases_to_one_module_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_file = temp.path().join("main.crn");
        let module_dir = temp.path().join("shared");
        let module_file = module_dir.join("main.crn");
        let root_source =
            "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n    Owner = \"team\"\n}";
        let module_source = "attributes {\n  tags = {\n    managed_by = \"carina\"\n    cost_center = \"infra\"\n  }\n}";
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(&root_file, root_source).unwrap();
        fs::write(&module_file, module_source).unwrap();

        let parsed = parsed_with_uses(
            vec![("first", "./shared"), ("second", "./shared")],
            vec!["first", "second"],
        );
        let root_inputs = vec![(
            crate::paths::ResolvedPath::new(root_file).unwrap(),
            root_source.to_string(),
        )];

        let entries = collect_all_tag_keys(
            &root_inputs,
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );
        let warnings = find_mixed_tag_key_styles(&entries);
        let warned_keys: Vec<_> = warnings
            .iter()
            .map(|warning| warning.key.as_str())
            .collect();

        assert_eq!(entries.len(), 5, "module keys must have single weight");
        assert_eq!(warned_keys, vec!["managed_by", "cost_center"]);
        assert!(
            warnings
                .iter()
                .all(|warning| warning.expected_style == TagKeyStyle::PascalCase)
        );
    }

    #[test]
    fn collect_all_tag_keys_deduplicates_self_referential_root_directory() {
        let temp = tempfile::tempdir().unwrap();
        let main_file = temp.path().join("main.crn");
        let db_file = temp.path().join("db.crn");
        let main_source = "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n}";
        let db_source = "tags = {\n    managed_by = \"carina\"\n}";
        fs::write(&main_file, main_source).unwrap();
        fs::write(&db_file, db_source).unwrap();

        let parsed = parsed_with_uses(vec![("self_module", ".")], vec!["self_module"]);
        let root_inputs = vec![
            (
                crate::paths::ResolvedPath::new(main_file).unwrap(),
                main_source.to_string(),
            ),
            (
                crate::paths::ResolvedPath::new(&db_file).unwrap(),
                db_source.to_string(),
            ),
        ];

        let entries = collect_all_tag_keys(
            &root_inputs,
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );
        let warnings = find_mixed_tag_key_styles(&entries);

        assert_eq!(entries.len(), 3, "each root tag key must be counted once");
        assert_eq!(warnings.len(), 1, "the minority key must warn once");
        assert_eq!(warnings[0].key, "managed_by");
        assert_eq!(warnings[0].file.as_deref(), Some(db_file.as_path()));
    }

    fn parsed_with_uses(uses: Vec<(&str, &str)>, calls: Vec<&str>) -> ParsedFile {
        ParsedFile {
            uses: uses
                .into_iter()
                .map(|(alias, path)| UseStatement::dummy(path.to_string(), alias.to_string()))
                .collect(),
            module_calls: calls
                .into_iter()
                .map(|name| ModuleCall::test(name.to_string(), None, HashMap::new()))
                .collect(),
            ..ParsedFile::default()
        }
    }

    #[test]
    fn collect_all_tag_keys_scans_directory_imports() {
        let temp = tempfile::tempdir().unwrap();
        let modules_dir = temp.path().join("modules").join("network");
        fs::create_dir_all(&modules_dir).unwrap();
        fs::write(
            modules_dir.join("main.crn"),
            "let vpc = awscc.ec2.Vpc {\n  tags = {\n    Name = 'x'\n  }\n}\n",
        )
        .unwrap();

        let parsed = parsed_with_uses(vec![("net", "./modules/network")], vec!["net"]);

        let results = collect_all_tag_keys::<_, _, String, _>(
            &[],
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );
        assert!(
            results.iter().any(|entry| entry.key == "Name"),
            "tag key 'Name' must be collected from the module's main.crn; got {:?}",
            results.iter().map(|entry| &entry.key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_all_tag_keys_skips_unused_imports() {
        let temp = tempfile::tempdir().unwrap();
        let module_dir = temp.path().join("unused");
        fs::create_dir_all(&module_dir).unwrap();
        fs::write(
            module_dir.join("main.crn"),
            "let vpc = awscc.ec2.Vpc {\n  tags = {\n    Name = 'x'\n  }\n}\n",
        )
        .unwrap();

        let parsed = parsed_with_uses(vec![("unused", "./unused")], vec![]);

        let results = collect_all_tag_keys::<_, _, String, _>(
            &[],
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );
        assert!(
            results.is_empty(),
            "tag keys from uncalled imports must not be collected; got {:?}",
            results.iter().map(|entry| &entry.key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn collect_all_tag_keys_skips_nonexistent_module_directory() {
        let temp = tempfile::tempdir().unwrap();
        let parsed = parsed_with_uses(vec![("missing", "./does-not-exist")], vec!["missing"]);

        let results = collect_all_tag_keys::<_, _, String, _>(
            &[],
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );

        assert!(results.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn collect_all_tag_keys_deduplicates_real_and_symlinked_module_directories() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_dir = temp.path().join("real-module");
        let alias_dir = temp.path().join("module-alias");
        fs::create_dir(&real_dir).unwrap();
        fs::write(
            real_dir.join("main.crn"),
            "attributes {\n  tags = {\n    Name = \"shared\"\n  }\n}\n",
        )
        .unwrap();
        symlink(&real_dir, &alias_dir).unwrap();

        let parsed = parsed_with_uses(
            vec![("real", "./real-module"), ("alias", "./module-alias")],
            vec!["real", "alias"],
        );

        let results = collect_all_tag_keys::<_, _, String, _>(
            &[],
            &parsed,
            &crate::paths::ResolvedPath::new(temp.path()).unwrap(),
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "Name");
    }

    #[test]
    fn test_tag_mixed_casing_warns() {
        // Majority is PascalCase (2 vs 1), so snake_case key should be flagged
        let source = r#"
let vpc = awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
    tags = {
        Name = "my-vpc"
        Environment = "staging"
        environment = "prod"
    }
}"#;
        let keys = collect_tag_keys(source);
        let results = find_mixed_tag_key_styles(&keys);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "environment");
        assert_eq!(results[0].expected_style, TagKeyStyle::PascalCase);
    }

    #[test]
    fn test_tag_all_pascal_case_no_warning() {
        let source = r#"
tags = {
    Name = "my-vpc"
    Environment = "prod"
    ManagedBy = "carina"
}"#;
        let keys = collect_tag_keys(source);
        let results = find_mixed_tag_key_styles(&keys);
        assert!(results.is_empty(), "All PascalCase should not warn");
    }

    #[test]
    fn test_tag_all_snake_case_no_warning() {
        let source = r#"
tags = {
    managed_by = "carina"
    env_name = "prod"
}"#;
        let keys = collect_tag_keys(source);
        let results = find_mixed_tag_key_styles(&keys);
        assert!(results.is_empty(), "All snake_case should not warn");
    }

    #[test]
    fn test_tag_comment_line_no_warning() {
        let source = r#"
// tags = {
//     bad_key = "value"
// }"#;
        let keys = collect_tag_keys(source);
        assert!(keys.is_empty(), "Comment lines should not produce keys");
    }

    #[test]
    fn test_tag_cross_file_mixed_styles() {
        // Simulate two files: file1 uses PascalCase, file2 uses snake_case
        let source1 = r#"
tags = {
    Name = "vpc"
    Environment = "prod"
}"#;
        let source2 = r#"
tags = {
    name = "subnet"
    managed_by = "carina"
}"#;
        let mut all_keys = collect_tag_keys(source1);
        all_keys.extend(collect_tag_keys(source2));
        let results = find_mixed_tag_key_styles(&all_keys);
        // PascalCase (2) == snake_case (2), PascalCase wins on tie
        // So snake_case keys are flagged
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|w| w.expected_style == TagKeyStyle::PascalCase)
        );
    }

    #[test]
    fn test_tag_single_key_no_warning() {
        let source = r#"
tags = {
    name = "only-one"
}"#;
        let keys = collect_tag_keys(source);
        let results = find_mixed_tag_key_styles(&keys);
        assert!(results.is_empty(), "Single key should not warn");
    }
}
