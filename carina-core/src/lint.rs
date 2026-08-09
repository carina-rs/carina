//! Lint utilities for detecting common DSL style issues
//!
//! This module provides functions for static analysis of `.crn` source files,
//! such as detecting list literal syntax where block syntax is preferred.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::schema::ResourceSchema;

/// Find list literal syntax (`attr = [...]`) for the given attribute names.
/// Returns attribute name and 1-indexed line number for each occurrence.
pub fn find_list_literal_attrs(source: &str, attr_names: &HashSet<String>) -> Vec<(String, usize)> {
    let mut results = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        for attr_name in attr_names {
            if !trimmed.starts_with(attr_name.as_str()) {
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
                    results.push((attr_name.clone(), line_idx + 1)); // 1-indexed line
                }
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

/// Find direct calls to pipe-preferred transformation functions.
///
/// Detects patterns like `join("-", parts)` where the pipe form
/// `parts |> join("-")` is recommended. Only warns when the function
/// call is NOT preceded by `|>` on the same line.
pub fn find_pipe_preferred_direct_calls(source: &str) -> Vec<PipePreferredWarning> {
    let mut warnings = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        for &func_name in PIPE_PREFERRED_FUNCTIONS {
            let pattern = format!("{}(", func_name);
            // Search for all occurrences of the pattern on this line
            let mut search_from = 0;
            while let Some(rel_pos) = trimmed[search_from..].find(&pattern) {
                let pos = search_from + rel_pos;
                search_from = pos + pattern.len();

                // Check that this is not part of a longer identifier
                // (e.g., "my_join(" should not match "join(")
                if pos > 0 {
                    let prev_char = trimmed.as_bytes()[pos - 1];
                    if prev_char.is_ascii_alphanumeric() || prev_char == b'_' {
                        continue;
                    }
                }

                // Check if this is a pipe call (|> before the function on this line)
                let before = &trimmed[..pos];
                if before.contains("|>") {
                    continue;
                }

                // Rough check: skip if inside a string literal
                if is_inside_string(trimmed, pos) {
                    continue;
                }

                // Skip if this is a type annotation position: `name: map(...)`
                // A ':' followed only by whitespace before the function name means
                // we're in a type position, not a function call.
                let before_trimmed = before.trim_end();
                if before_trimmed.ends_with(':') {
                    continue;
                }

                warnings.push(PipePreferredWarning {
                    name: func_name.to_string(),
                    line: line_idx + 1,
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
fn extract_binding_name(after_keyword: &str) -> Option<String> {
    let trimmed = after_keyword.trim_start();
    let name: String = trimmed
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '=')
        .collect();

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
fn extract_for_binding_names(after_for: &str) -> Vec<String> {
    let header = after_for.split(" in ").next().unwrap_or("");
    header
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty() && !part.starts_with('_'))
        .collect()
}

/// Find `let` bindings with non-snake_case names in source text.
///
/// Scans lines for `let <name> =` patterns and checks if `<name>` follows
/// snake_case convention. Bindings starting with `_` are skipped (internal/synthetic).
pub fn find_non_snake_case_bindings(source: &str) -> Vec<NamingWarning> {
    let mut warnings = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Extract binding name(s) from `let`, `for`, or `import` patterns.
        // `for` may have two names (map-form: `for k, v in m`); every other
        // form has at most one.
        let names: Vec<String> = if let Some(rest) = trimmed.strip_prefix("let ") {
            extract_binding_name(rest).into_iter().collect()
        } else if let Some(rest) = trimmed.strip_prefix("for ") {
            extract_for_binding_names(rest)
        } else {
            Vec::new()
        };

        for name in names {
            if !is_snake_case(&name) {
                warnings.push(NamingWarning {
                    name,
                    line: line_idx + 1,
                });
            }
        }
    }

    warnings
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
    let mut entries = Vec::new();
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
                    entries.push(TagKeyEntry {
                        key: key.to_string(),
                        style: classify_tag_key_style(key),
                        line: line_number,
                        column: line[..key_start_byte].chars().count(),
                        file: None,
                    });
                }
            }
        }
    }

    entries
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
/// Every file has one vote. Canonical paths deduplicate root/module overlap,
/// symlink aliases, repeated imports, and multiple aliases to one module. When
/// canonicalization fails, the original path is the identity, matching the
/// module walk's cycle-guard convention.
pub fn collect_all_tag_keys<E, S: AsRef<str>>(
    root_inputs: &[(PathBuf, S)],
    parsed: &crate::parser::File<E>,
    base_dir: &Path,
) -> Vec<TagKeyEntry> {
    fn canonical_guard_path(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    let mut seen_files = HashSet::new();
    let mut entries = Vec::new();
    for (file, source) in root_inputs {
        if seen_files.insert(canonical_guard_path(file)) {
            entries.extend(collect_tag_keys_for_file(source.as_ref(), file));
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
    let mut seen_module_dirs = HashSet::from([canonical_guard_path(base_dir)]);
    for import in &parsed.uses {
        if !aliases_used.contains(import.alias.as_str()) {
            continue;
        }
        let module_dir = base_dir.join(&import.path);
        if !module_dir.is_dir() {
            continue;
        }
        if !seen_module_dirs.insert(canonical_guard_path(&module_dir)) {
            continue;
        }
        let Ok(module_files) = crate::config_loader::find_crn_files_in_dir(&module_dir) else {
            continue;
        };
        for module_file in module_files {
            if !seen_files.insert(canonical_guard_path(&module_file)) {
                continue;
            }
            let Ok(content) = fs::read_to_string(&module_file) else {
                continue;
            };
            entries.extend(collect_tag_keys_for_file(&content, &module_file));
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

/// Rough heuristic to check if a byte position is inside a string literal.
fn is_inside_string(line: &str, pos: usize) -> bool {
    let mut in_string = false;
    for (i, ch) in line.char_indices() {
        if i >= pos {
            break;
        }
        if ch == '"' {
            in_string = !in_string;
        }
    }
    in_string
}

/// A duplicate attribute warning with attribute name, 1-indexed line number, and first occurrence line.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateAttr {
    /// The attribute name that is duplicated
    pub name: String,
    /// 1-indexed line number of the duplicate occurrence
    pub line: usize,
    /// 1-indexed line number of the first occurrence
    pub first_line: usize,
}

/// Find duplicate attribute keys within the same block in source text.
///
/// Scans the source for blocks (delimited by `{` and `}`) and detects
/// attribute assignments (`key = value`) where the same key appears more
/// than once in the same block. Returns a list of duplicates found.
///
/// This works on resource, provider, backend, and nested blocks. Direct members
/// of declaration blocks (`exports`, `attributes`, and `arguments`) are
/// excluded because directory loading reports their name collisions as hard
/// duplicate-declaration errors; emitting a second warning that says the last
/// value wins would be contradictory.
pub fn find_duplicate_attrs(source: &str) -> Vec<DuplicateAttr> {
    let mut results = Vec::new();
    let mut block_stack: Vec<(HashMap<String, usize>, bool)> = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let line_number = line_idx + 1; // 1-indexed

        // Count braces to handle patterns like `= [{`, `}]`, or single-line blocks
        let opens = trimmed.chars().filter(|&c| c == '{').count();
        let closes = trimmed.chars().filter(|&c| c == '}').count();

        // Push new blocks for each opening brace
        let opens_declaration_block = opens_duplicate_owned_declaration_block(trimmed);
        for open_index in 0..opens {
            block_stack.push((HashMap::new(), opens_declaration_block && open_index == 0));
        }

        // Check for attribute assignment: `key = value` or `key =`
        // Only check if the line doesn't start with `}` (closing brace line)
        if !trimmed.starts_with('}')
            && let Some(eq_pos) = trimmed.find('=')
        {
            // The key is everything before '=' trimmed
            let key_part = trimmed[..eq_pos].trim();

            // Must be a simple identifier (no dots, no spaces, not empty)
            if !key_part.is_empty()
                && key_part
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_')
                && key_part.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                // Skip internal attributes
                && !key_part.starts_with('_')
            && let Some((current_block, suppress_duplicate)) = block_stack.last_mut()
            {
                if let Some(&first_line) = current_block.get(key_part) {
                    if !*suppress_duplicate {
                        results.push(DuplicateAttr {
                            name: key_part.to_string(),
                            line: line_number,
                            first_line,
                        });
                    }
                } else {
                    current_block.insert(key_part.to_string(), line_number);
                }
            }
        }

        // Pop blocks for each closing brace
        for _ in 0..closes {
            block_stack.pop();
        }
    }

    results
}

fn opens_duplicate_owned_declaration_block(trimmed: &str) -> bool {
    ["exports", "attributes", "arguments"]
        .into_iter()
        .any(|keyword| {
            let Some(after_keyword) = trimmed.strip_prefix(keyword) else {
                return false;
            };
            let has_identifier_boundary = after_keyword
                .chars()
                .next()
                .is_some_and(|character| character == '{' || character.is_whitespace());
            has_identifier_boundary && after_keyword.contains('{')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ModuleCall, ParsedFile, UseStatement};
    use crate::schema::{AttributeType, StructField};

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
        assert_eq!(results[0].name, "internet_gateway_id");
        assert_eq!(results[0].line, 5); // duplicate on line 5
        assert_eq!(results[0].first_line, 4); // first on line 4
    }

    #[test]
    fn test_find_duplicate_attrs_no_false_positive() {
        let source = r#"
awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
    enable_dns_support = true
}
"#;
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
        assert_eq!(results[0].name, "from_port");
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
        assert_eq!(results[0].name, "ip_protocol");
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
        assert_eq!(results[0].name, "group_description");
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
        assert_eq!(results[0].name, "region");
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_direct_exports_members() {
        let source = r#"
exports {
    x = "one"
    x = "two"
}
"#;

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

        assert!(find_duplicate_attrs(source).is_empty());
    }

    #[test]
    fn test_find_duplicate_attrs_ignores_direct_argument_members() {
        // Use assignment-shaped members to prove suppression comes from the
        // declaration-block frame rather than today's typed-member filter.
        let source = r#"
arguments {
    x = "one"
    x = "two"
}
"#;

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
        assert_eq!(results[0].name, "key");
    }

    #[test]
    fn test_list_struct_attr_names() {
        let schema = ResourceSchema::new("ec2.SecurityGroup")
            .attribute(crate::schema::AttributeSchema::new(
                "security_group_ingress",
                AttributeType::list(AttributeType::struct_(
                    "Ingress".to_string(),
                    vec![StructField::new("ip_protocol", AttributeType::string())],
                )),
            ))
            .attribute(crate::schema::AttributeSchema::new(
                "tags",
                AttributeType::list(AttributeType::string()),
            ))
            .attribute(crate::schema::AttributeSchema::new(
                "group_description",
                AttributeType::string(),
            ));

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
    fn test_pipe_preferred_direct_call_warns() {
        let source = r#"let name = join("-", parts)"#;
        let results = find_pipe_preferred_direct_calls(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "join");
        assert_eq!(results[0].line, 1);
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
        let root_inputs = vec![(root_file, root_source.to_string())];

        let entries = collect_all_tag_keys(&root_inputs, &parsed, temp.path());
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
            (main_file, main_source.to_string()),
            (db_file.clone(), db_source.to_string()),
        ];

        let entries = collect_all_tag_keys(&root_inputs, &parsed, temp.path());
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
                .map(|(alias, path)| UseStatement {
                    alias: alias.to_string(),
                    path: path.to_string(),
                })
                .collect(),
            module_calls: calls
                .into_iter()
                .map(|name| ModuleCall {
                    module_name: name.to_string(),
                    binding_name: None,
                    arguments: HashMap::new(),
                })
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

        let results = collect_all_tag_keys::<_, String>(&[], &parsed, temp.path());
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

        let results = collect_all_tag_keys::<_, String>(&[], &parsed, temp.path());
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

        let results = collect_all_tag_keys::<_, String>(&[], &parsed, temp.path());

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

        let results = collect_all_tag_keys::<_, String>(&[], &parsed, temp.path());

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
