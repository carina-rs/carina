//! Lint utilities for detecting common DSL style issues
//!
//! This module provides functions for static analysis of `.crn` source files,
//! such as detecting list literal syntax where block syntax is preferred.

use std::collections::HashSet;

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
}
