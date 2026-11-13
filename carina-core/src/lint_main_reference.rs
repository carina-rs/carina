//! Frozen origin/main lint algorithms, used only as an output oracle.
use super::*;

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

fn extract_for_binding_names(after_for: &str) -> Vec<String> {
    let header = after_for.split(" in ").next().unwrap_or("");
    header
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty() && !part.starts_with('_'))
        .collect()
}

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

pub fn find_list_literal_attrs(source: &str, attr_names: &HashSet<String>) -> Vec<(String, usize)> {
    let mut results = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        #[allow(clippy::iter_over_hash_type)]
        // commutative: oracle equality uses the same input-set traversal on both sides
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
