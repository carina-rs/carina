//! Value rendering helpers: color inference, span building, and structural splitting.

use ratatui::prelude::*;

/// Split a string by `, ` at the top level, respecting nested brackets, braces, and quotes.
pub(super) fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut in_quote = false;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'[' | b'{' if !in_quote => depth += 1,
            b']' | b'}' if !in_quote => depth -= 1,
            b',' if !in_quote && depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b' ' => {
                parts.push(&s[start..i]);
                start = i + 2;
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Infer a color for a rendered attribute value based on its string form.
///
/// - Quoted strings (`"..."`) → Green
/// - Booleans (`true` / `false`) → Yellow
/// - Numbers (integer or float) → default (White)
/// - DSL identifiers (dot-separated, e.g. `awscc.Region.ap_northeast_1`) → Magenta
/// - Everything else → None (caller decides)
pub(super) fn value_color(rendered: &str) -> Option<Color> {
    if rendered.starts_with('"') && rendered.ends_with('"') {
        return Some(Color::Green);
    }
    if rendered == "true" || rendered == "false" {
        return Some(Color::Yellow);
    }
    // Integer or float
    if !rendered.is_empty()
        && rendered
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        // Must start with a digit or '-'
        let first = rendered.chars().next().unwrap();
        if first.is_ascii_digit() || first == '-' {
            return Some(Color::White);
        }
    }
    // DSL identifier: contains dots, no quotes, no spaces (e.g. binding.attr or awscc.Region.x)
    // ResourceRef is handled separately (cyan), so this catches remaining dot-notation identifiers
    if rendered.contains('.') && !rendered.contains(' ') && !rendered.starts_with('{') {
        return Some(Color::Magenta);
    }
    None
}

/// Build styled spans for a rendered value, coloring sub-elements individually for
/// lists and maps.
pub(super) fn value_spans<'a>(rendered: &str, ref_binding: bool) -> Vec<Span<'a>> {
    let base_style = if ref_binding {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), base_style)];
        }
        let elements = split_top_level(inner);
        let mut spans = vec![Span::raw("[")];
        for (i, elem) in elements.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            spans.extend(value_spans(elem.trim(), ref_binding));
        }
        spans.push(Span::raw("]"));
        return spans;
    }

    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), base_style)];
        }
        let entries = split_top_level(inner);
        let mut spans = vec![Span::raw("{")];
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(", "));
            }
            if let Some(colon_pos) = entry.find(": ") {
                let key = &entry[..colon_pos];
                let val = &entry[colon_pos + 2..];
                spans.push(Span::raw(format!("{}: ", key)));
                spans.extend(value_spans(val, false));
            } else {
                spans.push(Span::raw(entry.to_string()));
            }
        }
        spans.push(Span::raw("}"));
        return spans;
    }

    // Atomic value
    let style = if ref_binding {
        Style::default().fg(Color::Cyan)
    } else if let Some(color) = value_color(rendered) {
        Style::default().fg(color)
    } else {
        Style::default()
    };
    vec![Span::styled(rendered.to_string(), style)]
}

/// Build styled spans for a rendered value with dimmed modifier (for default values).
pub(super) fn value_spans_dimmed<'a>(rendered: &str) -> Vec<Span<'a>> {
    let dim_style = Style::default().fg(Color::DarkGray);

    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), dim_style)];
        }
        let elements = split_top_level(inner);
        let mut spans = vec![Span::styled("[".to_string(), dim_style)];
        for (i, elem) in elements.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ".to_string(), dim_style));
            }
            spans.extend(value_spans_dimmed(elem.trim()));
        }
        spans.push(Span::styled("]".to_string(), dim_style));
        return spans;
    }

    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return vec![Span::styled(rendered.to_string(), dim_style)];
        }
        let entries = split_top_level(inner);
        let mut spans = vec![Span::styled("{".to_string(), dim_style)];
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(", ".to_string(), dim_style));
            }
            if let Some(colon_pos) = entry.find(": ") {
                let key = &entry[..colon_pos];
                let val = &entry[colon_pos + 2..];
                spans.push(Span::styled(format!("{}: ", key), dim_style));
                spans.extend(value_spans_dimmed(val));
            } else {
                spans.push(Span::styled(entry.to_string(), dim_style));
            }
        }
        spans.push(Span::styled("}".to_string(), dim_style));
        return spans;
    }

    // Atomic value
    let style = if let Some(color) = value_color(rendered) {
        Style::default().fg(color).add_modifier(Modifier::DIM)
    } else {
        dim_style
    };
    vec![Span::styled(rendered.to_string(), style)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_color_quoted_string_is_green() {
        assert_eq!(value_color("\"hello\""), Some(Color::Green));
        assert_eq!(value_color("\"10.0.0.0/16\""), Some(Color::Green));
        assert_eq!(value_color("\"\""), Some(Color::Green));
    }

    #[test]
    fn value_color_boolean_is_yellow() {
        assert_eq!(value_color("true"), Some(Color::Yellow));
        assert_eq!(value_color("false"), Some(Color::Yellow));
    }

    #[test]
    fn value_color_number_is_white() {
        assert_eq!(value_color("42"), Some(Color::White));
        assert_eq!(value_color("3.14"), Some(Color::White));
        assert_eq!(value_color("-1"), Some(Color::White));
        assert_eq!(value_color("0"), Some(Color::White));
    }

    #[test]
    fn value_color_dsl_identifier_is_magenta() {
        // DSL identifiers with dots (not quoted, not ResourceRef which is handled separately)
        assert_eq!(
            value_color("awscc.Region.ap_northeast_1"),
            Some(Color::Magenta)
        );
        assert_eq!(
            value_color("aws.s3.VersioningStatus.Enabled"),
            Some(Color::Magenta)
        );
    }

    #[test]
    fn value_color_other_values_return_none() {
        assert_eq!(value_color("[1, 2, 3]"), None);
        assert_eq!(value_color("{key: val}"), None);
        assert_eq!(value_color(""), None);
    }

    #[test]
    fn split_top_level_simple() {
        assert_eq!(split_top_level(r#""a", "b""#), vec![r#""a""#, r#""b""#]);
    }

    #[test]
    fn split_top_level_nested() {
        assert_eq!(split_top_level("[1, 2], [3]"), vec!["[1, 2]", "[3]"]);
    }

    #[test]
    fn value_spans_list_creates_multiple_spans() {
        let spans = value_spans(r#"["hello", 42]"#, false);
        // Should have: "[", "hello" (green), ", ", "42" (white), "]"
        assert!(spans.len() > 1, "List should produce multiple spans");
        // Check that individual elements got colored
        let has_green = spans.iter().any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green, "Quoted string element should be green");
        let has_white = spans.iter().any(|s| s.style.fg == Some(Color::White));
        assert!(has_white, "Number element should be white");
    }

    #[test]
    fn value_spans_map_colors_values() {
        let spans = value_spans(r#"{Name: "test"}"#, false);
        assert!(spans.len() > 1, "Map should produce multiple spans");
        let has_green = spans.iter().any(|s| s.style.fg == Some(Color::Green));
        assert!(has_green, "Quoted string value should be green");
    }

    #[test]
    fn value_spans_ref_binding_cyan() {
        let spans = value_spans("[binding.attr]", true);
        let has_cyan = spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "Ref binding elements should be cyan");
    }

    #[test]
    fn value_spans_dimmed_list() {
        let spans = value_spans_dimmed(r#"["hello"]"#);
        assert!(spans.len() > 1, "Dimmed list should produce multiple spans");
        let has_dim_green = spans.iter().any(|s| {
            s.style.fg == Some(Color::Green) && s.style.add_modifier.contains(Modifier::DIM)
        });
        assert!(has_dim_green, "Dimmed list string should be green+dim");
    }
}
