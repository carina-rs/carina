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

use crate::parser::escape_double_quoted_character;

/// Result of preprocessing: the transformed source and a list of heredoc
/// replacements (for the formatter to restore them in the output).
#[derive(Debug)]
pub struct PreprocessResult {
    /// Source with heredocs replaced by double-quoted strings
    pub source: String,
    /// Original heredoc texts, in order of replacement.
    /// Each entry is the full heredoc text from `<<MARKER` through the closing `MARKER` line.
    pub heredocs: Vec<String>,
    /// Non-identity byte mappings, one per collapsed heredoc. Everywhere else
    /// maps through the byte delta established by the preceding segment.
    position_segments: Vec<PositionSegment>,
    /// Original line starts used to turn restored bytes into line/column
    /// coordinates by binary search.
    original_line_starts: Vec<usize>,
    /// The authored source is needed only when heredocs changed the text.
    /// Heredoc-free files reuse `source` and allocate no mapping segments.
    original_source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OriginalPosition {
    byte: usize,
    line: usize,
    column: usize,
}

impl OriginalPosition {
    pub(crate) const fn byte(self) -> usize {
        self.byte
    }

    pub(crate) const fn line(self) -> usize {
        self.line
    }

    pub(crate) const fn column(self) -> usize {
        self.column
    }
}

impl PreprocessResult {
    /// Return the exact authored text corresponding to a byte range in the
    /// heredoc-preprocessed source.
    pub(crate) fn original_slice(&self, preprocessed_range: std::ops::Range<usize>) -> &str {
        assert!(
            preprocessed_range.start <= preprocessed_range.end
                && preprocessed_range.end <= self.source.len(),
            "preprocessed range must be within the transformed source"
        );
        let start = self.original_byte(preprocessed_range.start);
        let end = self.original_byte(preprocessed_range.end);
        let original = self.original_source.as_deref().unwrap_or(&self.source);
        &original[start..end]
    }

    pub(crate) fn original_position(&self, preprocessed_byte: usize) -> OriginalPosition {
        assert!(
            preprocessed_byte <= self.source.len(),
            "preprocessed byte must be within the transformed source"
        );
        let byte = self.original_byte(preprocessed_byte);
        let line_index = self
            .original_line_starts
            .partition_point(|line_start| *line_start <= byte)
            .saturating_sub(1);
        let line_start = self.original_line_starts[line_index];
        let original_source = self.original_source.as_deref().unwrap_or(&self.source);
        let column = original_source[line_start..byte].chars().count() + 1;
        OriginalPosition {
            byte,
            line: line_index + 1,
            column,
        }
    }

    fn original_byte(&self, preprocessed_byte: usize) -> usize {
        let preceding = self
            .position_segments
            .partition_point(|segment| segment.preprocessed_start <= preprocessed_byte);
        if let Some(segment) = preceding
            .checked_sub(1)
            .map(|index| &self.position_segments[index])
        {
            if preprocessed_byte <= segment.preprocessed_end {
                return segment.original_byte(preprocessed_byte);
            }
            return shift_byte(
                preprocessed_byte,
                segment.original_end as isize - segment.preprocessed_end as isize,
            );
        }
        preprocessed_byte
    }
}

#[derive(Debug)]
struct PositionSegment {
    preprocessed_start: usize,
    preprocessed_end: usize,
    mapping_pieces: Vec<MappingPiece>,
    mapped_end_original: usize,
    /// Authored byte consumed by the collapsed heredoc, including whitespace
    /// after the marker but excluding the following newline.
    original_end: usize,
}

impl PositionSegment {
    fn original_byte(&self, preprocessed_byte: usize) -> usize {
        mapped_original_byte(
            &self.mapping_pieces,
            self.preprocessed_end - self.preprocessed_start,
            self.mapped_end_original,
            preprocessed_byte - self.preprocessed_start,
        )
    }
}

#[cfg(test)]
impl PositionSegment {
    fn mapping_entry_count(&self) -> usize {
        self.mapping_pieces.len()
    }
}

fn shift_byte(byte: usize, delta: isize) -> usize {
    if delta >= 0 {
        byte + delta as usize
    } else {
        byte - delta.unsigned_abs()
    }
}

#[derive(Debug)]
struct MappedText {
    text: String,
    mapping_pieces: Vec<MappingPiece>,
    end_original: usize,
}

#[derive(Debug, Clone, Copy)]
struct MappingPiece {
    output_start: usize,
    output_end: usize,
    original_start: usize,
    original_end: usize,
    kind: MappingKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MappingKind {
    Exact,
    Transformed,
}

impl MappedText {
    fn new(initial: usize, capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            mapping_pieces: Vec::new(),
            end_original: initial,
        }
    }

    fn set_boundary(&mut self, byte: usize) {
        self.end_original = byte;
    }

    fn start_original(&self) -> usize {
        self.mapping_pieces
            .first()
            .map_or(self.end_original, |piece| piece.original_start)
    }

    fn original_byte(&self, output_byte: usize) -> usize {
        mapped_original_byte(
            &self.mapping_pieces,
            self.text.len(),
            self.end_original,
            output_byte,
        )
    }

    fn push_exact(&mut self, text: &str, original_start: usize) {
        let output_start = self.text.len();
        let original_end = original_start + text.len();
        self.set_boundary(original_start);
        self.text.push_str(text);
        if text.is_empty() {
            return;
        }
        let output_end = self.text.len();
        if let Some(previous) = self.mapping_pieces.last_mut()
            && previous.kind == MappingKind::Exact
            && previous.output_end == output_start
            && previous.original_end == original_start
        {
            previous.output_end = output_end;
            previous.original_end = original_end;
        } else {
            self.mapping_pieces.push(MappingPiece {
                output_start,
                output_end,
                original_start,
                original_end,
                kind: MappingKind::Exact,
            });
        }
        self.set_boundary(original_end);
    }

    fn push_transformed(&mut self, text: &str, original_start: usize, original_end: usize) {
        let output_start = self.text.len();
        self.set_boundary(original_start);
        self.text.push_str(text);
        if text.is_empty() {
            self.set_boundary(original_end);
            return;
        }
        self.mapping_pieces.push(MappingPiece {
            output_start,
            output_end: self.text.len(),
            original_start,
            original_end,
            kind: MappingKind::Transformed,
        });
        self.set_boundary(original_end);
    }

    fn push_mapped(&mut self, mapped: &MappedText) {
        self.push_mapped_range(mapped, 0, mapped.text.len());
    }

    fn push_mapped_range(&mut self, mapped: &MappedText, start: usize, end: usize) {
        assert!(start <= end && end <= mapped.text.len());
        if start == end {
            self.set_boundary(mapped.original_byte(end));
            return;
        }

        let first_piece = mapped
            .mapping_pieces
            .partition_point(|piece| piece.output_start <= start)
            .saturating_sub(1);
        for piece in &mapped.mapping_pieces[first_piece..] {
            if piece.output_start >= end {
                break;
            }
            let piece_start = start.max(piece.output_start);
            let piece_end = end.min(piece.output_end);
            if piece_start >= piece_end {
                continue;
            }
            let text = &mapped.text[piece_start..piece_end];
            match piece.kind {
                MappingKind::Exact => self.push_exact(
                    text,
                    piece.original_start + (piece_start - piece.output_start),
                ),
                MappingKind::Transformed => {
                    let original_end = if piece_end == piece.output_end {
                        piece.original_end
                    } else {
                        piece.original_start
                    };
                    self.push_transformed(text, piece.original_start, original_end);
                }
            }
        }
        self.set_boundary(mapped.original_byte(end));
    }
}

fn mapped_original_byte(
    pieces: &[MappingPiece],
    output_len: usize,
    end_original: usize,
    output_byte: usize,
) -> usize {
    assert!(output_byte <= output_len);
    if output_byte == output_len {
        return end_original;
    }
    let piece = &pieces[pieces
        .partition_point(|piece| piece.output_start <= output_byte)
        .saturating_sub(1)];
    debug_assert!(output_byte < piece.output_end);
    match piece.kind {
        MappingKind::Exact => piece.original_start + (output_byte - piece.output_start),
        MappingKind::Transformed => piece.original_start,
    }
}

fn line_starts(input: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        input
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    starts
}

/// Preprocess heredocs in the input, replacing them with double-quoted strings.
///
/// Returns the preprocessed source and a list of original heredoc texts
/// (for round-trip preservation in the formatter).
pub fn preprocess_heredocs(input: &str) -> Result<PreprocessResult, HeredocError> {
    let lines: Vec<&str> = input.split('\n').collect();
    let original_line_starts = line_starts(input);
    let mut source = String::with_capacity(input.len());
    let mut heredocs = Vec::new();
    let mut position_segments = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if let Some(heredoc_start) = find_heredoc_start(line) {
            let opening_line = i;
            let opening_line_start = original_line_starts[opening_line];
            // Emit the part of the line before the heredoc
            source.push_str(&line[..heredoc_start.prefix_end]);

            let marker = heredoc_start.marker;
            let strip_indent = heredoc_start.strip_indent;
            let quoted = heredoc_start.quoted;

            // Save original heredoc text for formatter round-trip
            let mut original = String::new();
            original.push_str(&line[heredoc_start.prefix_end..]);
            original.push('\n');

            // Collect body lines until we find the closing marker
            let mut body_lines: Vec<(usize, &str)> = Vec::new();
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
                body_lines.push((i, body_line));
                i += 1;
            }

            if !found_end {
                return Err(HeredocError::Unterminated {
                    line: i,
                    marker: marker.to_string(),
                });
            }

            heredocs.push(original);

            let closing_line = i;
            let closing_line_start = original_line_starts[closing_line];
            let closing_text = lines[closing_line];
            let closing_indent = closing_text.len() - closing_text.trim_start().len();
            let closing_marker_start = closing_line_start + closing_indent;
            let closing_marker_end = closing_marker_start + marker.len();

            let body = mapped_heredoc_body(
                &body_lines,
                strip_indent,
                &original_line_starts,
                closing_marker_start,
            );
            let escaped = escape_mapped_heredoc_body(&body, !quoted);

            let opening_start = opening_line_start + heredoc_start.prefix_end;
            let opening_end = escaped.start_original();
            let mut placeholder = MappedText::new(opening_start, escaped.text.len() + 2);
            placeholder.push_transformed("\"", opening_start, opening_end);
            placeholder.push_mapped(&escaped);
            placeholder.push_transformed("\"", closing_marker_start, closing_marker_end);
            let preprocessed_start = source.len();
            source.push_str(&placeholder.text);
            position_segments.push(PositionSegment {
                preprocessed_start,
                preprocessed_end: source.len(),
                mapping_pieces: placeholder.mapping_pieces,
                mapped_end_original: placeholder.end_original,
                original_end: closing_line_start + closing_text.len(),
            });

            // The closing marker line is consumed; continue with the next line
            i += 1;
            if i < lines.len() {
                source.push('\n');
            }
        } else {
            source.push_str(line);
            i += 1;
            if i < lines.len() {
                source.push('\n');
            }
        }
    }

    let original_source = (!position_segments.is_empty()).then(|| input.to_string());
    Ok(PreprocessResult {
        source,
        heredocs,
        position_segments,
        original_line_starts,
        original_source,
    })
}

fn common_indent(lines: &[(usize, &str)]) -> usize {
    lines
        .iter()
        .map(|(_, line)| *line)
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0)
}

fn mapped_heredoc_body(
    lines: &[(usize, &str)],
    strip_indent: bool,
    line_starts: &[usize],
    empty_position: usize,
) -> MappedText {
    let indent = if strip_indent {
        common_indent(lines)
    } else {
        0
    };
    let first_start = lines
        .first()
        .map(|(line_index, line)| {
            line_starts[*line_index] + heredoc_line_content_start(line, indent)
        })
        .unwrap_or(empty_position);
    let mut body = MappedText::new(first_start, 0);

    for (body_index, (line_index, line)) in lines.iter().enumerate() {
        let content_start = heredoc_line_content_start(line, indent);
        body.push_exact(
            &line[content_start..],
            line_starts[*line_index] + content_start,
        );
        if body_index + 1 < lines.len() {
            body.push_exact("\n", line_starts[*line_index] + line.len());
        }
    }

    body
}

fn heredoc_line_content_start(line: &str, indent: usize) -> usize {
    if line.len() >= indent {
        indent
    } else {
        line.len()
    }
}

fn escape_mapped_heredoc_body(body: &MappedText, preserve_interpolation: bool) -> MappedText {
    let mut escaped = MappedText::new(body.start_original(), body.text.len());
    let mut chars = body.text.char_indices().peekable();
    let mut unescaped_start = 0;
    while let Some((start, ch)) = chars.next() {
        let end = start + ch.len_utf8();
        let rendered = escape_double_quoted_character(
            ch,
            chars.peek().map(|(_, next)| *next),
            !preserve_interpolation,
        );
        if let Some(rendered) = rendered {
            escaped.push_mapped_range(body, unescaped_start, start);
            escaped.push_transformed(rendered, body.original_byte(start), body.original_byte(end));
            unescaped_start = end;
        }
    }
    escaped.push_mapped_range(body, unescaped_start, body.text.len());
    escaped
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
    let result = preprocess_heredocs(heredoc_text).ok()?;
    (result.heredocs.len() == 1).then_some(result.source)
}

/// Check if a line contains a heredoc start (`<<MARKER` or `<<-MARKER`),
/// returning `(marker, quoted)`. The `quoted` flag distinguishes
/// `<<'EOT'` (literal — `${...}` is NOT expanded) from `<<EOT`
/// (interpolation-hosting). Used by the LSP for semantic token
/// highlighting; occurrences inside string literals are skipped.
pub fn find_heredoc_marker_with_quoted(line: &str) -> Option<(String, bool)> {
    find_heredoc_start(line).map(|h| (h.marker.to_string(), h.quoted))
}

/// Return the byte range occupied by a parser-recognized heredoc opener.
/// LSP code uses this alongside [`find_heredoc_marker_with_quoted`] so the
/// opener is treated as syntax, not as three ordinary `<`, `<`, `-` operators.
pub fn find_heredoc_opener_range(line: &str) -> Option<std::ops::Range<usize>> {
    find_heredoc_start(line).map(|start| start.prefix_end..line.len())
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
    fn heredoc_free_source_has_no_position_segments() {
        let result = preprocess_heredocs("x = 'plain'\n").unwrap();
        assert!(result.position_segments.is_empty());
    }

    #[test]
    fn unescaped_heredoc_uses_sparse_position_mapping() {
        let body = "a".repeat(8 * 1024);
        let input = format!("x = <<EOT\n{body}\nEOT\n");
        let result = preprocess_heredocs(&input).unwrap();

        assert_eq!(result.position_segments.len(), 1);
        assert!(
            result.position_segments[0].mapping_entry_count() <= 4,
            "one unescaped body run should not allocate per-byte mappings; got {} entries",
            result.position_segments[0].mapping_entry_count(),
        );
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
    fn formatter_restores_unquoted_heredoc_with_the_shared_escaper() {
        let input = "mock.test.resource {\n  name = <<EOT\npath\\to \\\"${name}\\\"\nEOT\n}\n";
        let formatted = crate::formatter::format(input, &crate::formatter::FormatConfig::default())
            .expect("heredoc fixture must format");

        assert!(
            formatted.contains("<<EOT\npath\\to \\\"${name}\\\"\nEOT"),
            "formatter must restore the authored heredoc: {formatted}"
        );
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
