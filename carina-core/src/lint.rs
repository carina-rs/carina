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
}
