//! Lint utilities for detecting common DSL style issues
//!
//! This module provides functions for static analysis of `.crn` source files,
//! such as detecting list literal syntax where block syntax is preferred.

use std::collections::{HashMap, HashSet};

use crate::schema::{AttributeType, ResourceSchema};

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

/// Collect all List<Struct> attribute names from a schema.
pub fn list_struct_attr_names(schema: &ResourceSchema) -> HashSet<String> {
    schema
        .attributes
        .iter()
        .filter(|(_, attr_schema)| {
            matches!(
                &attr_schema.attr_type,
                AttributeType::List { inner, .. } if matches!(inner.as_ref(), AttributeType::Struct { .. })
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

/// Extract a binding name from the text after a keyword (`let`, `for`, `import`).
///
/// If `stop_at_eq` is true, the name is delimited by whitespace or `=` (for `let`/`import`).
/// If false, the name is delimited by whitespace only (for `for`).
/// Returns `None` if the name is empty or starts with `_`.
fn extract_binding_name(after_keyword: &str, stop_at_eq: bool) -> Option<String> {
    let trimmed = after_keyword.trim_start();
    let name: String = if stop_at_eq {
        trimmed
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '=')
            .collect()
    } else {
        trimmed.chars().take_while(|c| !c.is_whitespace()).collect()
    };

    if name.is_empty() || name.starts_with('_') {
        return None;
    }

    Some(name)
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

        // Extract binding name from `let`, `for`, or `import` patterns
        let name = if let Some(rest) = trimmed.strip_prefix("let ") {
            extract_binding_name(rest, true)
        } else if let Some(rest) = trimmed.strip_prefix("for ") {
            extract_binding_name(rest, false)
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            extract_binding_name(rest, true)
        } else {
            None
        };

        if let Some(name) = name
            && !is_snake_case(&name)
        {
            warnings.push(NamingWarning {
                name,
                line: line_idx + 1,
            });
        }
    }

    warnings
}

/// A warning for binding names that redundantly include the resource type.
#[derive(Debug, Clone, PartialEq)]
pub struct RedundantTypeWarning {
    /// The binding name
    pub binding: String,
    /// The resource type that is redundantly included
    pub resource_type: String,
    /// 1-indexed line number
    pub line: usize,
}

/// Find `let` bindings whose names redundantly include the resource type.
///
/// Detects patterns like `let security_group_sg = aws.ec2.security_group { ... }`
/// where the binding name contains the full resource type as a word-boundary
/// substring. Short resource types (4 chars or less, e.g., "vpc", "eip") are
/// excluded because they are commonly used as binding names themselves.
pub fn find_redundant_type_in_binding(source: &str) -> Vec<RedundantTypeWarning> {
    let mut warnings = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Match `let <name> = <provider>.<service>.<resource_type> {`
        if let Some(rest) = trimmed.strip_prefix("let ") {
            let rest = rest.trim_start();
            // Extract binding name (until whitespace or '=')
            let binding: String = rest
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '=')
                .collect();
            if binding.is_empty() || binding.starts_with('_') {
                continue;
            }

            // Find the resource expression after '='
            let after_name = &rest[binding.len()..];
            let after_eq = after_name.trim_start();
            if let Some(after_eq) = after_eq.strip_prefix('=') {
                let expr = after_eq.trim_start();
                // Match provider.service.resource_type pattern
                if let Some(resource_type) = extract_resource_type_from_expr(expr) {
                    // Skip short resource types (4 chars or fewer)
                    if resource_type.len() <= 4 {
                        continue;
                    }
                    // Check if binding contains the resource type as word-boundary match
                    if contains_as_word_segment(&binding, &resource_type) {
                        warnings.push(RedundantTypeWarning {
                            binding,
                            resource_type,
                            line: line_idx + 1,
                        });
                    }
                }
            }
        }
    }

    warnings
}

/// Extract the resource type (last segment) from a `provider.service.resource_type` expression.
/// Returns `None` if the expression does not match the expected pattern.
fn extract_resource_type_from_expr(expr: &str) -> Option<String> {
    // Take characters that form the dotted identifier
    let ident: String = expr
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();

    let parts: Vec<&str> = ident.split('.').collect();
    // Expect at least provider.service.resource_type (3 parts)
    if parts.len() >= 3 {
        // Resource type is everything after service (may contain underscores)
        // e.g., "aws.ec2.security_group" -> "security_group"
        // e.g., "awscc.ec2.vpc_gateway_attachment" -> "vpc_gateway_attachment"
        Some(parts[2..].join("_"))
    } else {
        None
    }
}

/// Check if `haystack` contains `needle` as a whole word segment,
/// using underscore and string boundaries as word delimiters.
///
/// For example:
/// - `contains_as_word_segment("security_group_sg", "security_group")` -> true
/// - `contains_as_word_segment("vpcflow", "vpc")` -> false (no boundary after "vpc")
/// - `contains_as_word_segment("security_group", "security_group")` -> true (exact match)
fn contains_as_word_segment(haystack: &str, needle: &str) -> bool {
    if haystack == needle {
        return true;
    }

    // Split both by underscores and check if needle segments appear consecutively
    let haystack_parts: Vec<&str> = haystack.split('_').collect();
    let needle_parts: Vec<&str> = needle.split('_').collect();

    if needle_parts.len() > haystack_parts.len() {
        return false;
    }

    for start in 0..=(haystack_parts.len() - needle_parts.len()) {
        if haystack_parts[start..start + needle_parts.len()] == needle_parts[..] {
            return true;
        }
    }

    false
}

/// A warning for tag keys that don't follow PascalCase convention.
#[derive(Debug, Clone, PartialEq)]
pub struct TagKeyWarning {
    /// The tag key that is not PascalCase
    pub key: String,
    /// 1-indexed line number
    pub line: usize,
}

/// Check whether a tag key follows PascalCase convention.
///
/// PascalCase means: starts with an uppercase letter, no underscores,
/// no consecutive uppercase letters (simple heuristic).
fn is_pascal_case(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return false;
    }
    // Must not contain underscores or hyphens
    if name.contains('_') || name.contains('-') {
        return false;
    }
    // All chars must be alphanumeric
    name.chars().all(|c| c.is_alphanumeric())
}

/// Find tag keys that don't follow PascalCase convention within `tags = { ... }` blocks.
///
/// Scans source text for `tags = {` blocks and checks each key assignment inside.
/// Tag keys are expected to be PascalCase (e.g., `Name`, `Environment`, `ManagedBy`).
pub fn find_inconsistent_tag_keys(source: &str) -> Vec<TagKeyWarning> {
    let mut warnings = Vec::new();
    let mut in_tags_block = false;
    let mut brace_depth: usize = 0;

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let line_number = line_idx + 1;

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        // Detect start of a tags block: `tags = {`
        if !in_tags_block {
            let tag_pattern = trimmed.strip_prefix("tags");
            if let Some(after) = tag_pattern {
                let after = after.trim_start();
                if let Some(after_eq) = after.strip_prefix('=') {
                    let after_eq = after_eq.trim_start();
                    if after_eq.starts_with('{') {
                        in_tags_block = true;
                        brace_depth = 1;
                        // Check for keys on the same line after `{`
                        // (unlikely in practice but handle it)
                        continue;
                    }
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
                let key = trimmed[..eq_pos].trim();
                // Must be a simple identifier
                if !key.is_empty()
                    && key.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !is_pascal_case(key)
                {
                    warnings.push(TagKeyWarning {
                        key: key.to_string(),
                        line: line_number,
                    });
                }
            }
        }
    }

    warnings
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
/// This works on all block types: resource blocks, provider blocks, backend
/// blocks, and nested blocks.
pub fn find_duplicate_attrs(source: &str) -> Vec<DuplicateAttr> {
    let mut results = Vec::new();
    let mut block_stack: Vec<HashMap<String, usize>> = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let line_number = line_idx + 1; // 1-indexed

        // Count braces to handle patterns like `= [{`, `}]`, or single-line blocks
        let opens = trimmed.chars().filter(|&c| c == '{').count();
        let closes = trimmed.chars().filter(|&c| c == '}').count();

        // Push new blocks for each opening brace
        for _ in 0..opens {
            block_stack.push(HashMap::new());
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
                && let Some(current_block) = block_stack.last_mut()
            {
                if let Some(&first_line) = current_block.get(key_part) {
                    results.push(DuplicateAttr {
                        name: key_part.to_string(),
                        line: line_number,
                        first_line,
                    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AttributeType, StructField};

    #[test]
    fn test_find_list_literal_attrs_detects_list_literal() {
        let source = r#"
awscc.ec2.security_group {
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
awscc.ec2.security_group {
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
awscc.ec2.security_group {
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
awscc.ec2.vpc {
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
awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
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
awscc.ec2.security_group {
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
awscc.ec2.security_group {
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
awscc.ec2.security_group {
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
    fn test_list_struct_attr_names() {
        let schema = ResourceSchema::new("ec2.security_group")
            .attribute(crate::schema::AttributeSchema::new(
                "security_group_ingress",
                AttributeType::list(AttributeType::Struct {
                    name: "Ingress".to_string(),
                    fields: vec![StructField::new("ip_protocol", AttributeType::String)],
                }),
            ))
            .attribute(crate::schema::AttributeSchema::new(
                "tags",
                AttributeType::list(AttributeType::String),
            ))
            .attribute(crate::schema::AttributeSchema::new(
                "group_description",
                AttributeType::String,
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
        let source = r#"let myVpc = awscc.ec2.vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "myVpc");
        assert_eq!(results[0].line, 1);
    }

    #[test]
    fn test_naming_pascal_case_warns() {
        let source = r#"let MyVpc = awscc.ec2.vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "MyVpc");
    }

    #[test]
    fn test_naming_snake_case_no_warning() {
        let source = r#"let my_vpc = awscc.ec2.vpc { cidr_block = "10.0.0.0/16" }"#;
        let results = find_non_snake_case_bindings(source);
        assert!(results.is_empty(), "snake_case should not warn");
    }

    #[test]
    fn test_naming_underscore_prefix_skipped() {
        let source = r#"let _internal = awscc.ec2.vpc { cidr_block = "10.0.0.0/16" }"#;
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
    fn test_naming_import_binding_warns() {
        let source = r#"import myModule = "./modules/web""#;
        let results = find_non_snake_case_bindings(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "myModule");
    }

    #[test]
    fn test_naming_import_snake_case_no_warning() {
        let source = r#"import web_tier = "./modules/web""#;
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
        let source = "let myVpc = awscc.ec2.vpc {}\nlet MySubnet = awscc.ec2.subnet {}\nlet good_name = awscc.ec2.igw {}";
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

    // --- Resource type redundancy in binding name tests ---

    #[test]
    fn test_redundant_type_in_binding_warns() {
        // "security_group_sg" contains "security_group" which is the resource type
        let source = r#"let security_group_sg = aws.ec2.security_group {
    group_name = "test"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].binding, "security_group_sg");
        assert_eq!(results[0].resource_type, "security_group");
        assert_eq!(results[0].line, 1);
    }

    #[test]
    fn test_redundant_type_full_match_warns() {
        // Binding name is exactly the resource type
        let source = r#"let security_group = aws.ec2.security_group {
    group_name = "test"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].binding, "security_group");
    }

    #[test]
    fn test_redundant_type_abbreviated_no_warning() {
        // "sg" does not contain the full resource type "security_group"
        let source = r#"let sg = aws.ec2.security_group {
    group_name = "test"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert!(results.is_empty(), "Abbreviated binding should not warn");
    }

    #[test]
    fn test_redundant_type_descriptive_no_warning() {
        // "web_server" does not contain "instance" as a substring
        let source = r#"let web_server = aws.ec2.instance {
    instance_type = "t3.micro"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert!(results.is_empty(), "Descriptive binding should not warn");
    }

    #[test]
    fn test_redundant_type_partial_word_no_warning() {
        // "vpcflow" contains "vpc" but not as a whole word segment
        let source = r#"let vpcflow = aws.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert!(results.is_empty(), "Partial word match should not warn");
    }

    #[test]
    fn test_redundant_type_multiword_resource_warns() {
        // "route_table_main" contains "route_table"
        let source = r#"let route_table_main = awscc.ec2.route_table {
    vpc_id = vpc.vpc_id
}"#;
        let results = find_redundant_type_in_binding(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].binding, "route_table_main");
        assert_eq!(results[0].resource_type, "route_table");
    }

    #[test]
    fn test_redundant_type_short_resource_type_no_warning() {
        // Short resource types like "vpc" (3 chars) are commonly used in binding names
        // and should not trigger warnings
        let source = r#"let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
}"#;
        let results = find_redundant_type_in_binding(source);
        assert!(
            results.is_empty(),
            "Short resource types used as binding names should not warn"
        );
    }

    #[test]
    fn test_redundant_type_comment_line_no_warning() {
        let source = r#"// let security_group_sg = aws.ec2.security_group {"#;
        let results = find_redundant_type_in_binding(source);
        assert!(results.is_empty(), "Comment lines should not warn");
    }

    // --- Tag key casing consistency tests ---

    #[test]
    fn test_tag_mixed_casing_warns() {
        let source = r#"
let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
    tags = {
        Name = "my-vpc"
        environment = "prod"
    }
}"#;
        let results = find_inconsistent_tag_keys(source);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, 6); // "environment" line
        assert_eq!(results[0].key, "environment");
    }

    #[test]
    fn test_tag_all_pascal_case_no_warning() {
        let source = r#"
let vpc = awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"
    tags = {
        Name = "my-vpc"
        Environment = "prod"
        ManagedBy = "carina"
    }
}"#;
        let results = find_inconsistent_tag_keys(source);
        assert!(results.is_empty(), "All PascalCase should not warn");
    }

    #[test]
    fn test_tag_snake_case_warns() {
        let source = r#"
tags = {
    managed_by = "carina"
    env_name = "prod"
}"#;
        let results = find_inconsistent_tag_keys(source);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_tag_comment_line_no_warning() {
        let source = r#"
// tags = {
//     bad_key = "value"
// }"#;
        let results = find_inconsistent_tag_keys(source);
        assert!(results.is_empty(), "Comment lines should not warn");
    }

    #[test]
    fn test_tag_multiple_tag_blocks() {
        let source = r#"
let vpc = awscc.ec2.vpc {
    tags = {
        Name = "vpc"
        environment = "prod"
    }
}

let subnet = awscc.ec2.subnet {
    tags = {
        Name = "subnet"
        managed_by = "carina"
    }
}"#;
        let results = find_inconsistent_tag_keys(source);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].key, "environment");
        assert_eq!(results[1].key, "managed_by");
    }
}
