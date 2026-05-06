//! Heredoc preprocessing for the Carina DSL.
//!
//! Heredocs are preprocessed before pest parsing, since pest's declarative
//! grammar cannot handle dynamic closing markers.
//!
//! Modes:
//! - `<<MARKER` — interpolating heredoc (`${...}` is expanded)
//! - `<<-MARKER` — interpolating + indented (common leading whitespace stripped)
//! - `<<'MARKER'` — literal heredoc (`${...}` is NOT expanded)
//! - `<<-'MARKER'` — literal + indented

/// Result of preprocessing: the transformed source and a list of heredoc
/// replacements (for the formatter to restore them in the output).
#[derive(Debug)]
pub struct PreprocessResult {
    /// Source with heredocs replaced by double-quoted strings
    pub source: String,
    /// Original heredoc texts, in order of replacement.
    /// Each entry is the full heredoc text from `<<MARKER` through the closing `MARKER` line.
    pub heredocs: Vec<String>,
}

/// Preprocess heredocs in the input, replacing them with double-quoted strings.
///
/// Returns the preprocessed source and a list of original heredoc texts
/// (for round-trip preservation in the formatter).
pub fn preprocess_heredocs(input: &str) -> Result<PreprocessResult, HeredocError> {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut result = String::with_capacity(input.len());
    let mut heredocs = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if let Some(heredoc_start) = find_heredoc_start(line) {
            // Emit the part of the line before the heredoc
            result.push_str(&line[..heredoc_start.prefix_end]);

            let marker = heredoc_start.marker;
            let strip_indent = heredoc_start.strip_indent;
            let quoted = heredoc_start.quoted;

            // Save original heredoc text for formatter round-trip
            let mut original = String::new();
            original.push_str(&line[heredoc_start.prefix_end..]);
            original.push('\n');

            // Collect body lines until we find the closing marker
            let mut body_lines: Vec<&str> = Vec::new();
            i += 1;
            let mut found_end = false;

            while i < lines.len() {
                let body_line = lines[i];
                let trimmed = body_line.trim();
                original.push_str(body_line);
                if trimmed == marker {
                    found_end = true;
                    break;
                }
                original.push('\n');
                body_lines.push(body_line);
                i += 1;
            }

            if !found_end {
                return Err(HeredocError::Unterminated {
                    line: i,
                    marker: marker.to_string(),
                });
            }

            heredocs.push(original);

            // Build the body string
            let body = if body_lines.is_empty() {
                String::new()
            } else if strip_indent {
                strip_common_indent(&body_lines)
            } else {
                body_lines.join("\n")
            };

            // Escape for embedding in a double-quoted string.
            // Quoted heredocs (<<'EOT') escape ${...} to prevent interpolation.
            // Unquoted heredocs (<<EOT) leave ${...} intact for interpolation.
            let escaped = if quoted {
                escape_for_double_quote(&body)
            } else {
                escape_for_double_quote_interpolating(&body)
            };
            result.push('"');
            result.push_str(&escaped);
            result.push('"');

            // The closing marker line is consumed; continue with the next line
            i += 1;
            if i < lines.len() {
                result.push('\n');
            }
        } else {
            result.push_str(line);
            i += 1;
            if i < lines.len() {
                result.push('\n');
            }
        }
    }

    Ok(PreprocessResult {
        source: result,
        heredocs,
    })
}

/// Restore heredocs in formatted output, replacing placeholder strings back
/// with their original heredoc form.
pub fn restore_heredocs(formatted: &str, heredocs: &[String]) -> String {
    if heredocs.is_empty() {
        return formatted.to_string();
    }

    let mut result = formatted.to_string();
    for original in heredocs {
        // Extract the heredoc body from the original text to build the placeholder
        let preprocessed = preprocess_single_heredoc(original);
        if let Some(placeholder) = preprocessed {
            // Replace the first occurrence of the placeholder with the original heredoc
            if let Some(pos) = result.find(&placeholder) {
                result = format!(
                    "{}{}{}",
                    &result[..pos],
                    original,
                    &result[pos + placeholder.len()..]
                );
            }
        }
    }
    result
}

/// Preprocess a single heredoc text (from `<<MARKER\n...\nMARKER`) into its
/// placeholder double-quoted string (for matching during restore).
fn preprocess_single_heredoc(heredoc_text: &str) -> Option<String> {
    let lines: Vec<&str> = heredoc_text.split('\n').collect();
    if lines.is_empty() {
        return None;
    }

    let first_line = lines[0];
    let heredoc_start = find_heredoc_start_in_fragment(first_line)?;
    let strip_indent = heredoc_start.strip_indent;
    let quoted = heredoc_start.quoted;

    // Body is everything except first and last lines
    let body_lines: Vec<&str> = if lines.len() > 2 {
        lines[1..lines.len() - 1].to_vec()
    } else {
        vec![]
    };

    let body = if body_lines.is_empty() {
        String::new()
    } else if strip_indent {
        strip_common_indent(&body_lines)
    } else {
        body_lines.join("\n")
    };

    let escaped = if quoted {
        escape_for_double_quote(&body)
    } else {
        escape_for_double_quote_interpolating(&body)
    };
    Some(format!("\"{}\"", escaped))
}

/// Find heredoc start in a fragment that begins with `<<MARKER` (used for restore).
fn find_heredoc_start_in_fragment(line: &str) -> Option<HeredocStart<'_>> {
    let bytes = line.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'<' || bytes[1] != b'<' {
        return None;
    }
    let mut j = 2;
    let strip_indent = j < bytes.len() && bytes[j] == b'-';
    if strip_indent {
        j += 1;
    }
    let quoted = j < bytes.len() && bytes[j] == b'\'';
    if quoted {
        j += 1;
    }
    let marker_start = j;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    let marker = &line[marker_start..j];
    if marker.is_empty() {
        return None;
    }
    Some(HeredocStart {
        prefix_end: 0,
        marker,
        strip_indent,
        quoted,
    })
}

/// Strip common leading whitespace from lines.
fn strip_common_indent(lines: &[&str]) -> String {
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.len() >= min_indent {
                &l[min_indent..]
            } else {
                l.trim()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Escape a string for embedding inside double quotes (literal — escapes `${`).
fn escape_for_double_quote(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace("${", "\\${")
}

/// Escape for double quotes but preserve `${...}` for interpolation.
fn escape_for_double_quote_interpolating(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Check if a line contains a heredoc start (`<<MARKER` or `<<-MARKER`),
/// returning `(marker, quoted)`. The `quoted` flag distinguishes
/// `<<'EOT'` (literal — `${...}` is NOT expanded) from `<<EOT`
/// (interpolation-hosting). Used by the LSP for semantic token
/// highlighting; occurrences inside string literals are skipped.
pub fn find_heredoc_marker_with_quoted(line: &str) -> Option<(String, bool)> {
    find_heredoc_start(line).map(|h| (h.marker.to_string(), h.quoted))
}

/// Error during heredoc preprocessing.
#[derive(Debug, thiserror::Error)]
pub enum HeredocError {
    #[error("Unterminated heredoc at line {line}: closing marker '{marker}' not found")]
    Unterminated { line: usize, marker: String },
}

struct HeredocStart<'a> {
    /// Byte offset where the heredoc operator starts
    prefix_end: usize,
    /// The marker string
    marker: &'a str,
    /// Whether to strip common leading whitespace (`<<-`)
    strip_indent: bool,
    /// Whether the marker is quoted (`<<'EOT'`) — literal, no interpolation
    quoted: bool,
}

/// Find a `<<MARKER` or `<<-MARKER` pattern in a line.
/// Skips occurrences inside string literals.
fn find_heredoc_start(line: &str) -> Option<HeredocStart<'_>> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_double_quote = false;
    let mut in_single_quote = false;
    let mut escaped = false;

    while i < bytes.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if bytes[i] == b'\\' && (in_double_quote || in_single_quote) {
            escaped = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }
        if bytes[i] == b'\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }

        // Skip line comments: # or //
        if !in_double_quote && !in_single_quote {
            if bytes[i] == b'#' {
                return None;
            }
            if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                return None;
            }
        }

        if !in_double_quote
            && !in_single_quote
            && bytes[i] == b'<'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'<'
        {
            let prefix_end = i;
            let mut j = i + 2;
            let strip_indent = j < bytes.len() && bytes[j] == b'-';
            if strip_indent {
                j += 1;
            }
            // Check for quoted marker: <<'EOT' or <<-'EOT'
            let quoted = j < bytes.len() && bytes[j] == b'\'';
            if quoted {
                j += 1;
            }
            let marker_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let marker = &line[marker_start..j];
            // Consume closing quote if quoted
            if quoted && j < bytes.len() && bytes[j] == b'\'' {
                j += 1;
            }
            if !marker.is_empty() {
                let rest = line[j..].trim();
                if rest.is_empty() {
                    return Some(HeredocStart {
                        prefix_end,
                        marker,
                        strip_indent,
                        quoted,
                    });
                }
            }
        }

        i += 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_heredoc() {
        let input = "x = <<EOT\nhello\nworld\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"hello\\nworld\"\n");
        assert_eq!(result.heredocs.len(), 1);
    }

    #[test]
    fn test_indented_heredoc() {
        let input = "x = <<-EOT\n    line1\n    line2\n    EOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"line1\\nline2\"\n");
    }

    #[test]
    fn test_empty_heredoc() {
        let input = "x = <<EOT\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"\"\n");
    }

    #[test]
    fn test_heredoc_with_quotes() {
        let input = "x = <<EOT\n\"hello\"\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"\\\"hello\\\"\"\n");
    }

    #[test]
    fn test_heredoc_skips_string_literals() {
        let input = "x = \"<<EOT\"\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"<<EOT\"\n");
        assert_eq!(result.heredocs.len(), 0);
    }

    #[test]
    fn test_unterminated_heredoc() {
        let input = "x = <<EOT\nhello\n";
        let result = preprocess_heredocs(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_unquoted_heredoc_allows_interpolation() {
        // <<EOT (unquoted) should NOT escape ${...} — allows interpolation
        let input = "x = <<EOT\n${hello}\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"${hello}\"\n");
    }

    #[test]
    fn test_quoted_heredoc_escapes_interpolation() {
        // <<'EOT' (quoted) should escape ${...} — literal, no interpolation
        let input = "x = <<'EOT'\n${hello}\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"\\${hello}\"\n");
    }

    #[test]
    fn test_quoted_indented_heredoc() {
        let input = "x = <<-'EOT'\n    hello\n    world\n    EOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"hello\\nworld\"\n");
    }

    #[test]
    fn test_restore_heredocs() {
        let input = "x = <<EOT\nhello\nworld\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        let restored = restore_heredocs(&result.source, &result.heredocs);
        assert_eq!(restored, input);
    }

    #[test]
    fn test_heredoc_in_comment_ignored() {
        let input = "# x = <<EOT\ny = \"hello\"\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, input);
        assert_eq!(result.heredocs.len(), 0);
    }

    #[test]
    fn test_heredoc_in_line_comment_ignored() {
        let input = "// x = <<EOT\ny = \"hello\"\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, input);
        assert_eq!(result.heredocs.len(), 0);
    }

    #[test]
    fn test_multiple_heredocs() {
        let input = "a = <<EOF\nhello\nEOF\nb = <<END\nworld\nEND\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "a = \"hello\"\nb = \"world\"\n");
        assert_eq!(result.heredocs.len(), 2);
    }

    #[test]
    fn test_heredoc_with_backslash() {
        let input = "x = <<EOT\npath\\to\\file\nEOT\n";
        let result = preprocess_heredocs(input).unwrap();
        assert_eq!(result.source, "x = \"path\\\\to\\\\file\"\n");
    }
}
