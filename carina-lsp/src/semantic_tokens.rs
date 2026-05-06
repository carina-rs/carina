use crate::position;
use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};

/// Token types supported by this language server.
///
/// DSL keywords and boolean literals are deliberately NOT emitted here; the
/// TextMate grammar handles them via dedicated scopes so themes can style
/// each category independently (#1948). The `KEYWORD` entry stays in the
/// legend only to keep the indices of later entries stable.
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,  // 0: (unused — kept for index stability)
    SemanticTokenType::TYPE,     // 1: aws.s3.Bucket, aws.ec2.Vpc, aws.Region.*
    SemanticTokenType::VARIABLE, // 2: variable names
    SemanticTokenType::PROPERTY, // 3: attribute names (name, region, etc.)
    SemanticTokenType::STRING,   // 4: string literals
    SemanticTokenType::NUMBER,   // 5: number literals
    SemanticTokenType::OPERATOR, // 6: =
    SemanticTokenType::COMMENT,  // 7: comments
    SemanticTokenType::FUNCTION, // 8: function names
    SemanticTokenType::MACRO,    // 9: ${...} interpolation spans inside double-quoted strings
];

/// Create the semantic tokens legend for capability registration
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: vec![],
    }
}

pub struct SemanticTokensProvider {
    /// Precomputed region patterns like "aws.Region.us_east_1", "awscc.Region.ap_northeast_1"
    region_patterns: Vec<String>,
}

impl SemanticTokensProvider {
    pub fn new(region_completions: &[carina_core::schema::CompletionValue]) -> Self {
        // Extract region patterns directly from completion data
        let region_patterns: Vec<String> =
            region_completions.iter().map(|c| c.value.clone()).collect();
        Self { region_patterns }
    }

    pub fn tokenize(&self, text: &str) -> Vec<SemanticToken> {
        let mut tokens = Vec::new();
        let mut prev_line = 0u32;
        let mut prev_start = 0u32;
        let mut block_comment_depth: usize = 0;
        // Track both the closing marker and whether the heredoc is
        // quoted (`<<'EOT'`). Quoted heredocs are literal — `${...}`
        // is NOT expanded by the parser, so the LSP must not split
        // MACRO tokens inside their bodies. See #2482.
        let mut heredoc_state: Option<(String, bool)> = None;

        for (line_idx, line) in text.lines().enumerate() {
            // Heredoc bodies allow `${...}` interpolation iff the
            // marker is unquoted (#2473 / #2482); reuse the same
            // splitter so the highlight matches the parser. The
            // helper short-circuits when no `$` is present, so plain
            // bodies and closing-marker lines still emit a single
            // STRING token.
            if let Some((marker, quoted)) = heredoc_state.as_ref() {
                let trimmed = line.trim();
                let line_chars: Vec<char> = line.chars().collect();
                let line_len = line_chars.len() as u32;
                if line_len > 0 {
                    let mut body_tokens: Vec<(u32, u32, u32)> = Vec::new();
                    push_interpolation_aware_string(
                        &line_chars,
                        0,
                        line_len,
                        !quoted,
                        &mut body_tokens,
                    );
                    push_with_delta(
                        &mut tokens,
                        &mut prev_line,
                        &mut prev_start,
                        line_idx as u32,
                        body_tokens,
                    );
                }
                if trimmed == marker {
                    heredoc_state = None;
                }
                continue;
            }

            // Check if this line starts a heredoc
            if let Some((marker, quoted)) =
                carina_core::heredoc::find_heredoc_marker_with_quoted(line)
            {
                let line_tokens = self.tokenize_line_with_block_comments(
                    line,
                    line_idx as u32,
                    &mut block_comment_depth,
                );
                push_with_delta(
                    &mut tokens,
                    &mut prev_line,
                    &mut prev_start,
                    line_idx as u32,
                    line_tokens,
                );
                heredoc_state = Some((marker, quoted));
                continue;
            }

            let line_tokens = self.tokenize_line_with_block_comments(
                line,
                line_idx as u32,
                &mut block_comment_depth,
            );
            push_with_delta(
                &mut tokens,
                &mut prev_line,
                &mut prev_start,
                line_idx as u32,
                line_tokens,
            );
        }

        tokens
    }

    /// Tokenize a single line with block comment tracking across lines.
    /// `block_comment_depth` is updated in place to track nesting depth.
    fn tokenize_line_with_block_comments(
        &self,
        line: &str,
        line_idx: u32,
        block_comment_depth: &mut usize,
    ) -> Vec<(u32, u32, u32)> {
        let chars: Vec<char> = line.chars().collect();
        let char_len = chars.len() as u32;

        // If we're entirely inside a block comment, scan for nested /* and */
        if *block_comment_depth > 0 {
            let mut i = 0;
            while i < chars.len() {
                if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                    *block_comment_depth += 1;
                    i += 2;
                } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '/' {
                    *block_comment_depth -= 1;
                    i += 2;
                    if *block_comment_depth == 0 {
                        // Block comment ended mid-line.
                        // Highlight comment portion, then tokenize the rest.
                        let comment_end = i as u32;
                        let mut tokens = vec![(0u32, comment_end, 7u32)]; // COMMENT
                        let rest: String = chars[i..].iter().collect();
                        if !rest.trim().is_empty() {
                            let rest_tokens = self.tokenize_line_with_block_comments(
                                &rest,
                                line_idx,
                                block_comment_depth,
                            );
                            for (start, len, typ) in rest_tokens {
                                tokens.push((start + comment_end, len, typ));
                            }
                        }
                        return tokens;
                    }
                } else {
                    i += 1;
                }
            }
            // Entire line is inside block comment
            if char_len > 0 {
                return vec![(0, char_len, 7)]; // COMMENT
            }
            return vec![];
        }

        // Not inside a block comment. Check if this line starts one.
        // Scan for /* to find inline block comments. While scanning, skip over
        // single- and double-quoted string literals so a `/*` inside an ARN
        // like 'arn:aws:s3:::bucket/*' does not open a block comment (#2436).
        // Also stop at `#` / `//` line-comment markers — pest treats line
        // and block comments as disjoint productions, so a `/*` inside a
        // line comment must not open one (#2448).
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\'' || chars[i] == '"' {
                i = skip_string_literal(&chars, i);
                continue;
            }
            if chars[i] == '#' || (i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/') {
                break;
            }
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                *block_comment_depth = 1;
                let comment_start = i as u32;
                i += 2;
                // Scan for end of block comment on same line. A nested /*
                // increments depth; a */ decrements. String literals inside
                // a block comment are inert in the pest grammar, so we do
                // not skip them here.
                while i < chars.len() {
                    if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                        *block_comment_depth += 1;
                        i += 2;
                    } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '/' {
                        *block_comment_depth -= 1;
                        i += 2;
                        if *block_comment_depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let comment_end = i as u32;

                // Tokenize part before the block comment
                let before: String = chars[..comment_start as usize].iter().collect();
                let mut tokens = if !before.trim().is_empty() {
                    self.tokenize_line(&before, line_idx)
                } else {
                    vec![]
                };

                // Add the block comment token
                tokens.push((comment_start, comment_end - comment_start, 7)); // COMMENT

                // If comment closed on this line, tokenize the rest
                if *block_comment_depth == 0 {
                    let rest: String = chars[i..].iter().collect();
                    if !rest.trim().is_empty() {
                        let rest_tokens = self.tokenize_line_with_block_comments(
                            &rest,
                            line_idx,
                            block_comment_depth,
                        );
                        for (start, len, typ) in rest_tokens {
                            tokens.push((start + comment_end, len, typ));
                        }
                    }
                } else if comment_end < char_len {
                    // Comment extends beyond this line but there's content after /*
                    // Already included in the comment token above
                }

                // Sort and dedup
                tokens.sort_by_key(|(start, _, _)| *start);
                tokens.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
                return tokens;
            }
            i += 1;
        }

        // No block comment on this line, use normal tokenization
        self.tokenize_line(line, line_idx)
    }

    /// Tokenize a single line, returning (start_col, length, token_type_index)
    /// All positions and lengths are in characters (not bytes) for LSP compatibility.
    fn tokenize_line(&self, line: &str, _line_idx: u32) -> Vec<(u32, u32, u32)> {
        let mut tokens = Vec::new();
        let trimmed = line.trim_start();
        let indent = position::leading_whitespace_chars(line);

        // Skip empty lines
        if trimmed.is_empty() {
            return tokens;
        }

        // Comment
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            tokens.push((indent, position::char_len(line) - indent, 7)); // COMMENT
            return tokens;
        }

        // Detect an inline `#` / `//` line comment outside string literals.
        // If found, split the line so the code-side tokenizers see only the
        // prefix and we emit one COMMENT token for the trailing comment.
        // Mirrors the pest grammar's disjoint `line_comment` /
        // `block_comment` productions. The rest of this function operates
        // on the truncated `line` shadow — any future code that needs the
        // full original line must read it before this point.
        let (line, comment_token) = match find_inline_comment_split(line) {
            Some(split) => (
                &line[..split.byte_pos],
                Some((split.char_pos, split.total_char_len - split.char_pos, 7u32)),
            ),
            None => (line, None),
        };
        let trimmed = line.trim_start();

        // DSL keywords (let, fn, provider, for, if, etc.) are intentionally NOT
        // emitted as semantic tokens: the TextMate grammar paints them via
        // dedicated scopes (`storage.type.carina`, `keyword.control.carina`,
        // etc.) and a blanket `KEYWORD` semantic token would override that.
        // See #1948. We still emit the *name* that follows a declaration
        // keyword (provider/backend/let/fn) so those names get typed/variable/
        // function highlighting from the semantic layer.
        if trimmed.starts_with("provider ")
            && let Some(name_start) = line.find("provider ")
        {
            let after = &line[name_start + 9..];
            let leading_spaces = after.len() - after.trim_start().len();
            let after_trimmed = after.trim_start();
            if let Some(name_end) = after_trimmed.find([' ', '{']) {
                let name = &after_trimmed[..name_end];
                if !name.is_empty() {
                    let name_pos = name_start + 9 + leading_spaces;
                    tokens.push((name_pos as u32, name.len() as u32, 1)); // TYPE
                }
            }
        } else if trimmed.starts_with("backend ")
            && let Some(name_start) = line.find("backend ")
        {
            let after = &line[name_start + 8..];
            let leading_spaces = after.len() - after.trim_start().len();
            let after_trimmed = after.trim_start();
            if let Some(name_end) = after_trimmed.find([' ', '{']) {
                let name = &after_trimmed[..name_end];
                if !name.is_empty() {
                    let name_pos = name_start + 8 + leading_spaces;
                    tokens.push((name_pos as u32, name.len() as u32, 1)); // TYPE
                }
            }
        } else if trimmed.starts_with("let ")
            && let Some(let_start) = line.find("let ")
        {
            let after_let = &line[let_start + 4..];
            let leading_spaces = after_let.len() - after_let.trim_start().len();
            let after_let_trimmed = after_let.trim_start();
            if let Some(name_end) = after_let_trimmed.find([' ', '=']) {
                let name = &after_let_trimmed[..name_end];
                if !name.is_empty() {
                    let name_start = let_start + 4 + leading_spaces;
                    tokens.push((name_start as u32, name.len() as u32, 2)); // VARIABLE
                }
            }
        } else if trimmed.starts_with("fn ")
            && let Some(fn_start) = line.find("fn ")
        {
            let after_fn = &line[fn_start + 3..];
            let leading_spaces = after_fn.len() - after_fn.trim_start().len();
            let after_fn_trimmed = after_fn.trim_start();
            if let Some(name_end) = after_fn_trimmed.find(['(', ' ']) {
                let name = &after_fn_trimmed[..name_end];
                if !name.is_empty() {
                    let name_pos = fn_start + 3 + leading_spaces;
                    tokens.push((name_pos as u32, name.len() as u32, 8)); // FUNCTION
                }
            }
        }

        // Nested block names: "identifier {" without "=" (e.g., "security_group_ingress {")
        // Highlight as PROPERTY since these are attribute names in block form
        if !trimmed.starts_with("provider ")
            && !trimmed.starts_with("backend ")
            && !trimmed.starts_with("let ")
            && !trimmed.starts_with("attributes ")
            && !trimmed.starts_with("attributes{")
            && !trimmed.starts_with("exports ")
            && !trimmed.starts_with("exports{")
            && !trimmed.starts_with("arguments ")
            && !trimmed.starts_with("arguments{")
            && !trimmed.starts_with("import ")
            && !trimmed.starts_with("import{")
            && !trimmed.starts_with("use ")
            && !trimmed.starts_with("use{")
            && !trimmed.starts_with("removed ")
            && !trimmed.starts_with("removed{")
            && !trimmed.starts_with("moved ")
            && !trimmed.starts_with("moved{")
            && !trimmed.starts_with("for ")
            && !trimmed.starts_with("if ")
            && !trimmed.contains('=')
            && !trimmed.contains('.')
            && trimmed.ends_with('{')
        {
            let name = trimmed.trim_end_matches('{').trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                tokens.push((indent, name.len() as u32, 3)); // PROPERTY
            }
        }

        // Resource type: aws.service.resource pattern
        self.find_resource_types(line, &mut tokens);

        // Bare PascalCase type annotations: after a `:` (type position in
        // arguments/attributes/exports/fn params), tag the following
        // identifier as TYPE if it starts with an ASCII uppercase letter.
        // Dotted forms (aws.ec2.Vpc, awscc.ec2.VpcId) are handled elsewhere.
        self.find_pascal_type_annotations(line, &mut tokens);

        // Region patterns from registered providers (e.g., aws.Region.us_east_1)
        for region in &self.region_patterns {
            self.find_and_add_pattern(line, region, 1, &mut tokens);
        }

        // Property names (before =)
        if let Some(eq_byte_pos) = line.find('=') {
            let before_eq = &line[..eq_byte_pos];
            let prop_name = before_eq.trim();
            if !prop_name.is_empty()
                && !prop_name.starts_with("provider")
                && !prop_name.starts_with("let")
                && !prop_name.contains('.')
                && let Some(prop_byte_start) = line.find(prop_name)
            {
                tokens.push((
                    position::byte_offset_to_char_offset(line, prop_byte_start),
                    position::char_len(prop_name),
                    3,
                )); // PROPERTY
            }
            // Operator =
            tokens.push((
                position::byte_offset_to_char_offset(line, eq_byte_pos),
                1,
                6,
            )); // OPERATOR
        }

        // String literals (double-quoted and single-quoted). Double-quoted
        // strings additionally have their `${...}` interpolation spans
        // split out as MACRO tokens so editor themes can render them
        // distinctly from the surrounding string body.
        {
            let line_chars: Vec<char> = line.chars().collect();
            let mut in_string = false;
            let mut string_start_char = 0u32;
            let mut string_quote_char = '"';
            let mut escaped = false;
            for (char_idx, c) in line_chars.iter().copied().enumerate() {
                let char_idx = char_idx as u32;
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == string_quote_char {
                        push_interpolation_aware_string(
                            &line_chars,
                            string_start_char,
                            char_idx + 1,
                            string_quote_char == '"',
                            &mut tokens,
                        );
                        in_string = false;
                    }
                } else if c == '"' || c == '\'' {
                    string_start_char = char_idx;
                    string_quote_char = c;
                    in_string = true;
                }
            }
        }

        // Number literals - use character-level operations for correct UTF-8 handling
        let chars: Vec<char> = line.chars().collect();
        for (char_idx, &c) in chars.iter().enumerate() {
            if c.is_ascii_digit() {
                let prev_char = if char_idx > 0 {
                    Some(chars[char_idx - 1])
                } else {
                    None
                };
                let next_char = if char_idx + 1 < chars.len() {
                    Some(chars[char_idx + 1])
                } else {
                    None
                };

                let prev_is_word =
                    prev_char.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
                let next_is_word =
                    next_char.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');

                let char_pos = char_idx as u32;

                if !prev_is_word && !next_is_word {
                    // Single digit number
                    tokens.push((char_pos, 1, 5)); // NUMBER
                } else if !prev_is_word {
                    // Multi-digit number - find the end
                    let num_end = chars[char_idx..]
                        .iter()
                        .position(|ch| !ch.is_ascii_digit())
                        .unwrap_or(chars.len() - char_idx);
                    tokens.push((char_pos, num_end as u32, 5)); // NUMBER
                }
            }
        }

        // Booleans are left to the TextMate grammar
        // (`constant.language.boolean.carina`) to avoid a blanket KEYWORD
        // semantic token overriding it.

        // Inline comment tail: added after code-side tokenization so it
        // does not interfere with property/operator/string scanners that
        // walk the (now-truncated) `line`.
        if let Some(comment) = comment_token {
            tokens.push(comment);
        }

        // Sort by position and deduplicate
        tokens.sort_by_key(|(start, _, _)| *start);
        tokens.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);

        tokens
    }

    /// Tag bare PascalCase identifiers in type-annotation position
    /// (`ident: PascalCase` or `: PascalCase`) as `TYPE`. Dotted forms
    /// like `aws.ec2.Vpc` are handled by `find_resource_types`.
    fn find_pascal_type_annotations(&self, line: &str, tokens: &mut Vec<(u32, u32, u32)>) {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b':' {
                i += 1;
                continue;
            }
            // Skip `::` (not expected in DSL, but be defensive)
            if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                i += 2;
                continue;
            }
            // Find the first non-whitespace character after `:`
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            // Must start with ASCII uppercase to be PascalCase
            if !bytes[j].is_ascii_uppercase() {
                i = j;
                continue;
            }
            // Scan the identifier [A-Za-z0-9_]+
            let ident_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            // Reject dotted forms — those are handled by find_resource_types.
            if j < bytes.len() && bytes[j] == b'.' {
                i = j;
                continue;
            }
            let length = (j - ident_start) as u32;
            let start_char = position::byte_offset_to_char_offset(line, ident_start);
            tokens.push((start_char, length, 1)); // TYPE
            i = j;
        }
    }

    /// Find resource type patterns like aws.s3.Bucket, aws.ec2.Vpc
    fn find_resource_types(&self, line: &str, tokens: &mut Vec<(u32, u32, u32)>) {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            // Look for potential start of resource type (letter at word boundary)
            if chars[i].is_ascii_lowercase() {
                let before_ok = i == 0 || (!chars[i - 1].is_alphanumeric() && chars[i - 1] != '_');

                if before_ok {
                    // Try to match provider.service.resource pattern
                    if let Some((end, pattern)) = self.match_resource_type(&chars, i) {
                        // Verify it's followed by whitespace or {
                        let after_ok = end >= chars.len()
                            || chars[end] == ' '
                            || chars[end] == '{'
                            || chars[end] == '\t'
                            || chars[end] == '\n';

                        if after_ok {
                            tokens.push((i as u32, pattern.len() as u32, 1)); // TYPE
                            i = end;
                            continue;
                        }
                    }
                }
            }
            i += 1;
        }
    }

    /// Match a resource type pattern starting at position i
    /// Returns (end_position, matched_string) if found
    fn match_resource_type(&self, chars: &[char], start: usize) -> Option<(usize, String)> {
        let mut parts = Vec::new();
        let mut current_part = String::new();
        let mut i = start;

        while i < chars.len() {
            let c = chars[i];
            if c.is_ascii_alphanumeric() || c == '_' {
                current_part.push(c);
            } else if c == '.' && !current_part.is_empty() {
                parts.push(current_part.clone());
                current_part.clear();
            } else {
                break;
            }
            i += 1;
        }

        if !current_part.is_empty() {
            parts.push(current_part);
        }

        // Must have at least 3 parts: provider.service.resource (e.g., aws.ec2.Vpc, awscc.ec2.Vpc)
        if parts.len() >= 2 && parts.len() <= 3 {
            // Exclude enum patterns like aws.Region, aws.Protocol (2nd part starts with uppercase)
            if parts.len() == 2 && parts[1].starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let pattern = parts.join(".");
            return Some((i, pattern));
        }

        None
    }

    fn find_and_add_pattern(
        &self,
        line: &str,
        pattern: &str,
        token_type: u32,
        tokens: &mut Vec<(u32, u32, u32)>,
    ) {
        let mut search_start = 0;
        while let Some(pos) = line[search_start..].find(pattern) {
            let absolute_byte_pos = search_start + pos;
            // Check word boundaries using byte-level access.
            // Patterns and boundary characters are ASCII, so byte access is safe.
            let before_byte = if absolute_byte_pos > 0 {
                Some(line.as_bytes()[absolute_byte_pos - 1])
            } else {
                None
            };
            let after_byte_pos = absolute_byte_pos + pattern.len();
            let after_byte = if after_byte_pos < line.len() {
                Some(line.as_bytes()[after_byte_pos])
            } else {
                None
            };

            let before_ok =
                before_byte.is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'.');
            let after_ok =
                after_byte.is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'.');

            if before_ok && after_ok {
                let char_pos = position::byte_offset_to_char_offset(line, absolute_byte_pos);
                // Pattern is ASCII, so byte length == char length
                tokens.push((char_pos, pattern.len() as u32, token_type));
            }
            search_start = absolute_byte_pos + pattern.len();
        }
    }
}

/// Result of scanning a line for an inline `#` / `//` line-comment marker.
struct InlineCommentSplit {
    /// Byte offset of the marker in the original `line`.
    byte_pos: usize,
    /// Char offset of the marker (LSP positions use char counts).
    char_pos: u32,
    /// Total char length of the line, returned alongside the position so
    /// callers can compute the comment-token length without re-walking.
    total_char_len: u32,
}

/// Locate an inline `#` or `//` line-comment marker in `line`, skipping
/// over single- and double-quoted string literals. Returns the marker's
/// byte and char offsets plus the line's total char length, all from a
/// single pass over the input. Returns `None` if the line has no
/// line-comment marker outside a string.
///
/// Mirrors the pest grammar's `line_comment = ("//" | "#") ~ …` production
/// (carina-core/src/parser/carina.pest). The block-comment scanner has its
/// own line-comment short-circuit at the top of
/// `tokenize_line_with_block_comments` (#2448); this helper covers the
/// `tokenize_line` (no-block-comment) path.
fn find_inline_comment_split(line: &str) -> Option<InlineCommentSplit> {
    let chars: Vec<char> = line.chars().collect();
    let mut char_idx: usize = 0;
    let mut byte_idx: usize = 0;
    let mut marker: Option<(usize, u32)> = None;
    while char_idx < chars.len() {
        let c = chars[char_idx];
        if marker.is_none() {
            if c == '\'' || c == '"' {
                let next_char = skip_string_literal(&chars, char_idx);
                while char_idx < next_char {
                    byte_idx += chars[char_idx].len_utf8();
                    char_idx += 1;
                }
                continue;
            }
            if c == '#' || (c == '/' && char_idx + 1 < chars.len() && chars[char_idx + 1] == '/') {
                marker = Some((byte_idx, char_idx as u32));
            }
        }
        byte_idx += c.len_utf8();
        char_idx += 1;
    }
    marker.map(|(byte_pos, char_pos)| InlineCommentSplit {
        byte_pos,
        char_pos,
        total_char_len: char_idx as u32,
    })
}

/// Skip past a single- or double-quoted string literal starting at `start`
/// (which must point at the opening quote). Returns the index of the
/// character after the closing quote, or `chars.len()` if the line ends
/// before the string closes. Honors `\\` escapes inside double-quoted
/// strings; single-quoted strings have no escape mechanism in Carina's
/// pest grammar, so the first matching `'` closes them.
fn skip_string_literal(chars: &[char], start: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if quote == '"' && c == '\\' && i + 1 < chars.len() {
            i += 2;
            continue;
        }
        if c == quote {
            return i + 1;
        }
        i += 1;
    }
    chars.len()
}

/// Convert raw `(start, length, token_type)` tuples for a single line
/// into LSP-encoded `SemanticToken`s appended to `tokens`. Updates
/// `prev_line` / `prev_start` to track the running delta cursor across
/// the next call.
fn push_with_delta(
    tokens: &mut Vec<SemanticToken>,
    prev_line: &mut u32,
    prev_start: &mut u32,
    line_idx: u32,
    raw_tokens: Vec<(u32, u32, u32)>,
) {
    for (start, length, token_type) in raw_tokens {
        let delta_line = line_idx - *prev_line;
        let delta_start = if delta_line == 0 {
            start - *prev_start
        } else {
            start
        };
        tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        });
        *prev_line = line_idx;
        *prev_start = start;
    }
}

/// Emit STRING tokens for the literal segments of `line_chars[start..end]`
/// and MACRO tokens for any `${...}` interpolation spans inside it, when
/// `allow_interpolation` is true. Brace-balancing lets nested object /
/// struct literals like `${ {a = 1}.a }` close at the matching `}`
/// rather than the first one; nested string literals are skipped via
/// `skip_string_literal` so a `}` inside them doesn't terminate the
/// interpolation early.
///
/// Callers that host interpolation (double-quoted strings, heredoc
/// bodies) pass `true`; literal-only contexts (single-quoted strings)
/// pass `false` and get a single STRING token covering the range.
fn push_interpolation_aware_string(
    line_chars: &[char],
    start: u32,
    end: u32,
    allow_interpolation: bool,
    tokens: &mut Vec<(u32, u32, u32)>,
) {
    // Literal-only contexts and ranges without `$` short-circuit to a
    // single STRING token covering the whole range.
    if !allow_interpolation || !line_chars[start as usize..end as usize].contains(&'$') {
        tokens.push((start, end - start, 4));
        return;
    }

    let mut i = start as usize;
    let end = end as usize;
    let mut segment_start = i;
    while i + 1 < end {
        let c = line_chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == '$' && line_chars[i + 1] == '{' {
            if i > segment_start {
                tokens.push((segment_start as u32, (i - segment_start) as u32, 4));
            }
            let interp_start = i;
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < end && depth > 0 {
                match line_chars[j] {
                    '\\' => {
                        j += 2;
                        continue;
                    }
                    '"' | '\'' => {
                        j = skip_string_literal(line_chars, j);
                        continue;
                    }
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            tokens.push((interp_start as u32, (j - interp_start) as u32, 9));
            i = j;
            segment_start = i;
            continue;
        }
        i += 1;
    }
    if end > segment_start {
        tokens.push((segment_start as u32, (end - segment_start) as u32, 4));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_type_at_line_start() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws.s3.Bucket {");

        // Should have at least one TYPE token for aws.s3.Bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.Bucket as TYPE");
    }

    #[test]
    fn test_resource_type_after_let() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("let bucket = aws.s3.Bucket {");

        // Should have TYPE token for aws.s3.Bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.Bucket as TYPE");
    }

    #[test]
    fn test_find_resource_types_directly() {
        let provider = SemanticTokensProvider::new(&[]);
        let mut tokens = Vec::new();
        provider.find_resource_types("aws.s3.Bucket {", &mut tokens);

        assert_eq!(tokens.len(), 1, "Should find one resource type");
        assert_eq!(
            tokens[0],
            (0, 13, 1),
            "Should be at position 0, length 13, type 1"
        );
    }

    #[test]
    fn test_tokenize_line_resource_type() {
        let provider = SemanticTokensProvider::new(&[]);
        let line_tokens = provider.tokenize_line("aws.s3.Bucket {", 0);

        println!("Line tokens: {:?}", line_tokens);

        // Check that aws.s3.Bucket is in the tokens as TYPE (1)
        let has_resource_type = line_tokens
            .iter()
            .any(|(start, len, typ)| *start == 0 && *len == 13 && *typ == 1);
        assert!(
            has_resource_type,
            "Should have aws.s3.Bucket as TYPE at position 0. Got: {:?}",
            line_tokens
        );
    }

    #[test]
    fn test_nested_block_name_highlighted_as_property() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("    security_group_ingress {", 0);

        // Should have PROPERTY token for security_group_ingress
        let property_token = tokens.iter().find(|(_, _, typ)| *typ == 3);
        assert!(
            property_token.is_some(),
            "Should highlight nested block name as PROPERTY. Got: {:?}",
            tokens
        );
        let (start, len, _) = property_token.unwrap();
        assert_eq!(*start, 4, "Should start at column 4 (after indent)");
        assert_eq!(*len, 22, "Should span 'security_group_ingress'");
    }

    #[test]
    fn test_nested_block_name_not_highlighted_for_keywords() {
        let provider = SemanticTokensProvider::new(&[]);

        // "provider aws {" should NOT get PROPERTY for "provider"
        let tokens = provider.tokenize_line("provider aws {", 0);
        let prop_at_0 = tokens.iter().find(|(s, _, typ)| *s == 0 && *typ == 3);
        assert!(
            prop_at_0.is_none(),
            "Keywords should not be highlighted as PROPERTY. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_tokenize_full_file() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "aws.s3.Bucket {\n    name = \"test\"\n}";
        let tokens = provider.tokenize(content);

        println!("Full tokenize result:");
        for token in &tokens {
            println!(
                "  delta_line={}, delta_start={}, length={}, token_type={}",
                token.delta_line, token.delta_start, token.length, token.token_type
            );
        }

        // First token should be aws.s3.Bucket (TYPE = 1)
        assert!(!tokens.is_empty(), "Should have tokens");
        let first = &tokens[0];
        assert_eq!(
            first.token_type, 1,
            "First token should be TYPE (1), got {}",
            first.token_type
        );
        assert_eq!(
            first.length, 13,
            "First token length should be 13 (aws.s3.Bucket)"
        );
    }

    #[test]
    fn test_region_highlighting_with_dynamic_data() {
        use carina_core::schema::CompletionValue;

        let regions = vec![
            CompletionValue::new("aws.Region.us_east_1", "US East (N. Virginia)"),
            CompletionValue::new("awscc.Region.ap_northeast_1", "Asia Pacific (Tokyo)"),
        ];
        let provider = SemanticTokensProvider::new(&regions);

        // Should highlight aws.Region.us_east_1 as TYPE
        let tokens = provider.tokenize_line("    region = aws.Region.us_east_1", 0);
        let type_token = tokens.iter().find(|(_, _, typ)| *typ == 1);
        assert!(
            type_token.is_some(),
            "Should highlight aws.Region.us_east_1 as TYPE. Got: {:?}",
            tokens
        );
        let (start, len, _) = type_token.unwrap();
        assert_eq!(*start, 13);
        assert_eq!(*len, "aws.Region.us_east_1".len() as u32);

        // Should highlight awscc.Region.ap_northeast_1 as TYPE
        let tokens = provider.tokenize_line("    region = awscc.Region.ap_northeast_1", 0);
        let type_token = tokens.iter().find(|(_, _, typ)| *typ == 1);
        assert!(
            type_token.is_some(),
            "Should highlight awscc.Region.ap_northeast_1 as TYPE. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_non_ascii_comment_no_panic() {
        let provider = SemanticTokensProvider::new(&[]);
        // Japanese comment should not panic and should be highlighted as COMMENT
        let tokens = provider.tokenize_line("// これはコメントです", 0);
        let comment_token = tokens.iter().find(|(_, _, typ)| *typ == 7);
        assert!(
            comment_token.is_some(),
            "Should highlight Japanese comment as COMMENT. Got: {:?}",
            tokens
        );
        // Position should be 0, length should be char count (not byte count)
        let (start, len, _) = comment_token.unwrap();
        assert_eq!(*start, 0);
        assert_eq!(
            *len,
            "// これはコメントです".chars().count() as u32,
            "Comment length should be in characters, not bytes"
        );
    }

    #[test]
    fn test_non_ascii_string_literal_no_panic() {
        let provider = SemanticTokensProvider::new(&[]);
        // String with multi-byte characters
        let tokens = provider.tokenize_line("    name = \"日本語の名前\"", 0);
        // Should not panic and should find the string literal
        let string_token = tokens.iter().find(|(_, _, typ)| *typ == 4);
        assert!(
            string_token.is_some(),
            "Should highlight Japanese string as STRING. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_non_ascii_number_after_multibyte() {
        let provider = SemanticTokensProvider::new(&[]);
        // Number literal appearing after multi-byte characters
        // "// コメント 3" - the number 3 appears after multi-byte chars
        let tokens = provider.tokenize_line("    count = 3 // 日本語", 0);
        // Should not panic
        let number_token = tokens.iter().find(|(_, _, typ)| *typ == 5);
        assert!(
            number_token.is_some(),
            "Should find number token. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_find_and_add_pattern_with_non_ascii() {
        let provider = SemanticTokensProvider::new(&[]);
        // Pattern search after multi-byte characters
        let mut tokens = Vec::new();
        provider.find_and_add_pattern("    value = true // 日本語", "true", 0, &mut tokens);
        assert!(
            !tokens.is_empty(),
            "Should find 'true' pattern. Got: {:?}",
            tokens
        );
        // Position should be in characters, not bytes
        let (pos, _, _) = tokens[0];
        assert_eq!(
            pos,
            "    value = ".chars().count() as u32,
            "Position should be in characters"
        );
    }

    #[test]
    fn test_non_ascii_full_tokenize() {
        let provider = SemanticTokensProvider::new(&[]);
        // Full file with mixed ASCII and non-ASCII
        let content = "// 日本語コメント\naws.s3.Bucket {\n    name = \"テスト\"\n}";
        // Should not panic
        let tokens = provider.tokenize(content);
        assert!(!tokens.is_empty(), "Should produce tokens");
    }

    #[test]
    fn test_indent_with_non_ascii() {
        let provider = SemanticTokensProvider::new(&[]);
        // Indented line with non-ASCII content
        let tokens = provider.tokenize_line("    name = \"あいう\"", 0);
        // indent should be 4 (characters), not affected by multi-byte
        let prop_token = tokens.iter().find(|(_, _, typ)| *typ == 3);
        assert!(
            prop_token.is_some(),
            "Should find property token. Got: {:?}",
            tokens
        );
        let (start, _, _) = prop_token.unwrap();
        assert_eq!(*start, 4, "Property should start at char position 4");
    }

    #[test]
    fn test_use_let_binding_variable_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("let web = use { source = \"./modules/web\" }", 0);
        // The module alias `web` is still emitted as VARIABLE even though `let`
        // itself is handled by the TextMate grammar (see #1948).
        let var_token = tokens.iter().find(|(_, _, typ)| *typ == 2);
        assert!(
            var_token.is_some(),
            "Should highlight module alias as VARIABLE in let use. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_two_part_region_not_highlighted_without_data() {
        use carina_core::schema::CompletionValue;

        // Two-part region patterns like "aws.Region" are excluded by find_resource_types
        // (2nd part starts with uppercase). They need explicit registration to be highlighted.
        let provider_without = SemanticTokensProvider::new(&[]);
        let tokens = provider_without.tokenize_line("    region = custom.Region.my_region_1", 0);
        // find_resource_types will match 3-part pattern, but with registered data it should also match
        let type_count_without = tokens.iter().filter(|(_, _, typ)| *typ == 1).count();

        let regions = vec![CompletionValue::new(
            "custom.Region.my_region_1",
            "My Region",
        )];
        let provider_with = SemanticTokensProvider::new(&regions);
        let tokens = provider_with.tokenize_line("    region = custom.Region.my_region_1", 0);
        let type_count_with = tokens.iter().filter(|(_, _, typ)| *typ == 1).count();

        // Both should highlight as TYPE (find_resource_types catches 3-part patterns),
        // but with registration, the pattern is matched twice (then deduped)
        assert!(
            type_count_without >= 1,
            "Should highlight 3-part pattern even without registration"
        );
        assert!(
            type_count_with >= 1,
            "Should highlight with registration too"
        );
    }

    #[test]
    fn test_dedup_only_removes_exact_duplicates() {
        // Regression test for issue #725: dedup_by should compare all three fields
        // (start, length, type), not just start position.
        use carina_core::schema::CompletionValue;

        // Register a region pattern that overlaps with find_resource_types:
        // "custom.Region.my_region_1" is a 3-part dotted pattern, so find_resource_types
        // will match it as TYPE, and the region pattern will also match it as TYPE.
        // These are exact duplicates (same start, length, type) and should be deduped to one.
        let regions = vec![CompletionValue::new(
            "custom.Region.my_region_1",
            "My Region",
        )];
        let provider = SemanticTokensProvider::new(&regions);
        let tokens = provider.tokenize_line("    region = custom.Region.my_region_1", 0);

        // Exact duplicates should be removed
        let type_tokens: Vec<_> = tokens.iter().filter(|(_, _, typ)| *typ == 1).collect();
        assert_eq!(
            type_tokens.len(),
            1,
            "Exact duplicate TYPE tokens should be deduped to one. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_dedup_preserves_different_tokens_at_same_position() {
        // Regression test for issue #725: the dedup logic in tokenize_line should only
        // remove exact duplicates (same start, length, and type), not drop tokens that
        // merely share the same start position.
        //
        // This test verifies the dedup contract directly, as the current tokenization
        // rules don't naturally produce different tokens at the same position. However,
        // adding new token patterns in the future could create such overlaps, and the
        // dedup must handle them correctly.
        let mut tokens: Vec<(u32, u32, u32)> = vec![
            (0, 13, 1), // TYPE token: e.g., aws.s3.Bucket
            (0, 3, 0),  // KEYWORD token at same position but different length/type
            (5, 4, 3),  // PROPERTY token
            (5, 4, 3),  // Exact duplicate of above - should be removed
            (10, 2, 6), // OPERATOR token
        ];
        tokens.sort_by_key(|(start, _, _)| *start);
        tokens.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
        assert_eq!(
            tokens.len(),
            4,
            "Should keep both tokens at position 0 (different type/length), dedup exact duplicate at position 5, and keep position 10. Got: {:?}",
            tokens
        );
        // Verify the tokens at position 0 are both present
        let at_0: Vec<_> = tokens.iter().filter(|(s, _, _)| *s == 0).collect();
        assert_eq!(at_0.len(), 2, "Both tokens at position 0 should survive");
    }

    #[test]
    fn test_provider_name_with_extra_whitespace() {
        let provider = SemanticTokensProvider::new(&[]);
        // Double space after "provider" - the name should still be highlighted
        let tokens = provider.tokenize_line("provider  aws {", 0);

        // Should have TYPE token for "aws"
        let type_token = tokens.iter().find(|(_, _, typ)| *typ == 1);
        assert!(
            type_token.is_some(),
            "Should highlight provider name 'aws' even with extra whitespace. Got: {:?}",
            tokens
        );
        let (start, len, _) = type_token.unwrap();
        assert_eq!(*len, 3, "Provider name 'aws' should have length 3");
        assert_eq!(
            *start, 10,
            "Provider name 'aws' should start at column 10 (after 'provider  ')"
        );
    }

    #[test]
    fn test_backend_name_with_extra_whitespace() {
        let provider = SemanticTokensProvider::new(&[]);
        // Double space after "backend" - the name should still be highlighted
        let tokens = provider.tokenize_line("backend  s3 {", 0);

        // Should have TYPE token for "s3"
        let type_token = tokens.iter().find(|(_, _, typ)| *typ == 1);
        assert!(
            type_token.is_some(),
            "Should highlight backend name 's3' even with extra whitespace. Got: {:?}",
            tokens
        );
        let (start, len, _) = type_token.unwrap();
        assert_eq!(*len, 2, "Backend name 's3' should have length 2");
        assert_eq!(
            *start, 9,
            "Backend name 's3' should start at column 9 (after 'backend  ')"
        );
    }

    #[test]
    fn test_provider_name_with_many_extra_spaces() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("provider    awscc {", 0);

        let type_token = tokens.iter().find(|(_, _, typ)| *typ == 1);
        assert!(
            type_token.is_some(),
            "Should highlight provider name 'awscc' with many extra spaces. Got: {:?}",
            tokens
        );
        let (start, len, _) = type_token.unwrap();
        assert_eq!(*len, 5, "Provider name 'awscc' should have length 5");
        assert_eq!(*start, 12, "Provider name should start at column 12");
    }

    #[test]
    fn test_let_binding_with_extra_whitespace() {
        let provider = SemanticTokensProvider::new(&[]);
        // Double space after "let" - the variable name should still be highlighted
        let tokens = provider.tokenize_line("let  x = aws.s3.Bucket {", 0);

        // Should have VARIABLE token for "x"
        let var_token = tokens.iter().find(|(_, _, typ)| *typ == 2);
        assert!(
            var_token.is_some(),
            "Should highlight variable 'x' even with extra whitespace after 'let'. Got: {:?}",
            tokens
        );
        let (start, len, _) = var_token.unwrap();
        assert_eq!(*len, 1, "Variable 'x' should have length 1");
        assert_eq!(
            *start, 5,
            "Variable 'x' should start at column 5 (after 'let  ')"
        );
    }

    #[test]
    fn test_let_binding_with_multiple_extra_spaces() {
        let provider = SemanticTokensProvider::new(&[]);
        // Multiple spaces after "let"
        let tokens = provider.tokenize_line("let    bucket = aws.s3.Bucket {", 0);

        let var_token = tokens.iter().find(|(_, _, typ)| *typ == 2);
        assert!(
            var_token.is_some(),
            "Should highlight variable 'bucket' with multiple spaces after 'let'. Got: {:?}",
            tokens
        );
        let (start, len, _) = var_token.unwrap();
        assert_eq!(*len, 6, "Variable 'bucket' should have length 6");
        assert_eq!(*start, 7, "Variable 'bucket' should start at column 7");
    }

    #[test]
    fn test_hash_comment_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("# shell-style comment", 0);
        let comment_token = tokens.iter().find(|(_, _, typ)| *typ == 7);
        assert!(
            comment_token.is_some(),
            "Should highlight # comment as COMMENT. Got: {:?}",
            tokens
        );
        let (start, len, _) = comment_token.unwrap();
        assert_eq!(*start, 0);
        assert_eq!(*len, "# shell-style comment".chars().count() as u32);
    }

    #[test]
    fn test_indented_hash_comment_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("    # indented comment", 0);
        let comment_token = tokens.iter().find(|(_, _, typ)| *typ == 7);
        assert!(
            comment_token.is_some(),
            "Should highlight indented # comment as COMMENT. Got: {:?}",
            tokens
        );
        let (start, len, _) = comment_token.unwrap();
        assert_eq!(*start, 4);
        assert_eq!(*len, "# indented comment".chars().count() as u32);
    }

    #[test]
    fn test_block_comment_single_line() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("/* single line block comment */");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            !comment_tokens.is_empty(),
            "Should highlight single-line block comment as COMMENT. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_block_comment_multi_line() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "/*\n  Multi-line block comment.\n*/";
        let tokens = provider.tokenize(content);
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        // Each line within the block comment should be highlighted
        assert!(
            comment_tokens.len() >= 3,
            "Should highlight all lines of multi-line block comment as COMMENT. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_block_comment_inline_with_code() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = /* comment */ \"test\"");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            !comment_tokens.is_empty(),
            "Should highlight inline block comment as COMMENT. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_block_comment_nested() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "/* outer /* inner */ still commented */";
        let tokens = provider.tokenize(content);
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            !comment_tokens.is_empty(),
            "Should highlight nested block comment as COMMENT. Got: {:?}",
            tokens
        );
    }

    /// Regression test for #2436: `/*` inside a single-quoted string must not
    /// open a block comment. ARNs like `'arn:aws:s3:::bucket/*'` previously
    /// caused every line below to be emitted as `COMMENT`, killing semantic
    /// highlighting in VS Code from that point onward.
    #[test]
    fn test_block_comment_not_triggered_inside_single_quoted_string() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "resource = 'arn:aws:s3:::bucket/*'\nname = \"after\"";
        let tokens = provider.tokenize(content);
        // No COMMENT tokens should be emitted; the `/*` is inside the string.
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            comment_tokens.is_empty(),
            "/* inside a single-quoted string must not start a block comment. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_block_comment_not_triggered_inside_double_quoted_string() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "resource = \"arn:aws:s3:::bucket/*\"\nname = \"after\"";
        let tokens = provider.tokenize(content);
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            comment_tokens.is_empty(),
            "/* inside a double-quoted string must not start a block comment. Got: {:?}",
            tokens
        );
    }

    /// Regression test for #2448: `/*` inside a `#` line comment must not
    /// open a block comment. A path like `management/*` in a `#` comment
    /// previously caused every following line to be emitted as `COMMENT`
    /// until a `*/` was seen, even though pest treats line comments as a
    /// production disjoint from block comments.
    #[test]
    fn test_block_comment_not_triggered_inside_hash_line_comment() {
        let provider = SemanticTokensProvider::new(&[]);
        let content =
            "# Cross-account read for the management/* state objects\nname = \"after\"\nvalue = 42";
        let tokens = provider.tokenize(content);
        // Only line 0 (the `#` comment itself) should produce a COMMENT
        // token. The buggy scanner emits one COMMENT token per subsequent
        // line because it stays in "inside block comment" state.
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert_eq!(
            comment_tokens.len(),
            1,
            "/* inside a `#` line comment must not leak block-comment state to following lines. Got: {:?}",
            tokens
        );
    }

    /// Sibling of the above for `//` line comments, which the pest grammar
    /// treats identically to `#` line comments.
    #[test]
    fn test_block_comment_not_triggered_inside_slash_line_comment() {
        let provider = SemanticTokensProvider::new(&[]);
        let content = "// Cross-account read for the management/* state objects\nname = \"after\"\nvalue = 42";
        let tokens = provider.tokenize(content);
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert_eq!(
            comment_tokens.len(),
            1,
            "/* inside a `//` line comment must not leak block-comment state to following lines. Got: {:?}",
            tokens
        );
    }

    /// Regression test for #2454: an inline `#` line comment (e.g.
    /// `name = "x" # foo`) must produce a `COMMENT` semantic token covering
    /// the trailing comment. Pre-fix, `tokenize_line` only emitted a
    /// COMMENT when the *whole* trimmed line started with `#` or `//`, so
    /// inline comments were silently uncolored at the semantic-token
    /// layer.
    #[test]
    fn test_inline_hash_comment_emits_comment_token() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = \"x\" # foo");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert_eq!(
            comment_tokens.len(),
            1,
            "inline `#` comment must produce one COMMENT token. Got: {:?}",
            tokens
        );
    }

    /// Sibling of the above for inline `//` comments.
    #[test]
    fn test_inline_slash_comment_emits_comment_token() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = \"x\" // foo");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert_eq!(
            comment_tokens.len(),
            1,
            "inline `//` comment must produce one COMMENT token. Got: {:?}",
            tokens
        );
    }

    /// `#` inside a string literal must NOT be classified as a comment.
    /// (Symmetric to #2436's protection for `/*`.)
    #[test]
    fn test_hash_inside_string_is_not_a_comment() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = \"no # here\"");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            comment_tokens.is_empty(),
            "`#` inside a string literal must not produce a COMMENT token. Got: {:?}",
            tokens
        );
    }

    /// `//` inside a string literal must NOT be classified as a comment.
    #[test]
    fn test_slashes_inside_string_are_not_a_comment() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = \"https://example.com\"");
        let comment_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 7).collect();
        assert!(
            comment_tokens.is_empty(),
            "`//` inside a string literal must not produce a COMMENT token. Got: {:?}",
            tokens
        );
    }

    /// The inline COMMENT token must cover the marker and the rest of the
    /// line exactly — verifies no off-by-one in `total_char_len - char_pos`.
    /// `name = "x" # foo` has the `#` at char 11 (0-indexed) and the line
    /// is 16 chars long, so the COMMENT token is (start=11, length=5).
    #[test]
    fn test_inline_comment_token_position_and_length() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("name = \"x\" # foo");
        // Tokens are LSP-encoded with delta_line / delta_start. For a
        // single-line input the COMMENT is the last token; its
        // delta_start equals (absolute_start - prev_token_start).
        let comment = tokens
            .iter()
            .find(|t| t.token_type == 7)
            .expect("expected one COMMENT token");
        // Compare against character length of the trailing comment slice.
        assert_eq!(
            comment.length, 5,
            "COMMENT token must cover `# foo` (5 chars). Got: {:?}",
            tokens
        );
    }

    /// An inline `#` comment on one line must not bleed into the next
    /// line's tokenization. `tokenize_line` is per-line, but assert it
    /// explicitly to mirror the line-independence invariants from #2436
    /// and #2448.
    #[test]
    fn test_inline_comment_does_not_leak_to_next_line() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("x = 1 # foo\ny = 2");
        // Line 0 has its own COMMENT (the `# foo` tail). Line 1 must not
        // produce any COMMENT token.
        let line1_comment_tokens: Vec<_> = tokens
            .iter()
            .scan(0u32, |line, t| {
                *line += t.delta_line;
                Some((*line, t))
            })
            .filter(|(line, t)| *line == 1 && t.token_type == 7)
            .collect();
        assert!(
            line1_comment_tokens.is_empty(),
            "line 2 must not be classified as COMMENT. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_heredoc_highlighted_as_string() {
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<EOT\n{\"Version\": \"2012-10-17\"}\nEOT";
        let tokens = provider.tokenize(input);

        // Lines 2 and 3 (body + closing marker) should have STRING tokens
        let string_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 4).collect();
        assert!(
            string_tokens.len() >= 2,
            "Heredoc body and closing marker should be highlighted as STRING. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn find_heredoc_marker_classifies_quoted_form() {
        use carina_core::heredoc::find_heredoc_marker_with_quoted;
        assert_eq!(
            find_heredoc_marker_with_quoted("policy = <<EOT"),
            Some(("EOT".to_string(), false))
        );
        assert_eq!(
            find_heredoc_marker_with_quoted("x = <<-EOF"),
            Some(("EOF".to_string(), false))
        );
        assert_eq!(
            find_heredoc_marker_with_quoted("x = <<'EOT'"),
            Some(("EOT".to_string(), true))
        );
        assert_eq!(find_heredoc_marker_with_quoted("x = \"<<EOT\""), None);
        assert_eq!(find_heredoc_marker_with_quoted("x = 123"), None);
    }

    // Issue #1948 — the LSP stopped emitting semantic-token `KEYWORD`s for DSL
    // keywords so the TextMate split from #1934 (`storage.type.carina`,
    // `keyword.declaration.carina`, `keyword.control.carina`, etc.) can drive
    // the coloring without being overridden by a blanket `keyword` token.

    #[test]
    fn keywords_do_not_emit_keyword_semantic_token() {
        let provider = SemanticTokensProvider::new(&[]);
        let sources = [
            "let x = aws.s3_bucket { name = \"b\" }",
            "fn greet(name) { name }",
            "provider aws { region = aws.Region.ap_northeast_1 }",
            "backend s3 { bucket = \"b\" }",
            "let orgs = upstream_state { source = \"../o\" }",
            "attributes { a: String = \"x\" }",
            "arguments { a: String }",
            "exports { a: String = \"x\" }",
            "import { to = aws.s3_bucket \"x\" }",
            "moved { from = aws.s3_bucket \"a\" to = aws.s3_bucket \"b\" }",
            "removed { from = aws.s3_bucket \"x\" }",
            "for az in [\"a\", \"b\"] { aws.s3_bucket { name = az } }",
            "if cond { aws.s3_bucket { name = \"x\" } }",
            "} else { aws.s3_bucket { name = \"y\" } }",
            "require !empty(name), \"must be set\"",
            "let r = read aws.s3_bucket { name = \"x\" }",
            "let m = use { source = \"./modules/web\" }",
            r#"let subnets = for az in ["a", "b"] { aws.s3_bucket { name = az } }"#,
            "let x = if cond { aws.s3_bucket { name = \"y\" } }",
        ];
        for src in sources {
            let tokens = provider.tokenize(src);
            let keyword_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 0).collect();
            assert!(
                keyword_tokens.is_empty(),
                "Expected no KEYWORD semantic tokens for {src:?}. Got: {tokens:?}"
            );
        }
    }

    #[test]
    fn booleans_do_not_emit_keyword_semantic_token() {
        // `true` / `false` also fell under token_type 0 before this change.
        // They stay colored by TextMate's `constant.language.boolean.carina`.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("enabled = true");
        let keyword_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 0).collect();
        assert!(
            keyword_tokens.is_empty(),
            "true/false should not emit KEYWORD tokens. Got: {tokens:?}"
        );
    }

    #[test]
    fn semantic_tokens_tags_pascal_case_type_annotation() {
        let provider = SemanticTokensProvider::new(&[]);
        let line = "    x: AwsAccountId";
        let tokens = provider.tokenize_line(line, 0);

        // Find a TYPE token covering "AwsAccountId"
        let expected_start = line.find("AwsAccountId").unwrap() as u32;
        let expected_len = "AwsAccountId".len() as u32;
        let ty_tok = tokens
            .iter()
            .find(|(start, len, kind)| {
                *kind == 1 && *start == expected_start && *len == expected_len
            })
            .unwrap_or_else(|| {
                panic!("AwsAccountId should be tagged as TYPE, got tokens: {tokens:?}")
            });
        assert_eq!(ty_tok.2, 1, "token kind should be TYPE (1)");
    }

    #[test]
    fn semantic_tokens_tags_pascal_case_primitive_in_type_annotation() {
        let provider = SemanticTokensProvider::new(&[]);
        let line = "    port: Int";
        let tokens = provider.tokenize_line(line, 0);

        let expected_start = line.find("Int").unwrap() as u32;
        assert!(
            tokens
                .iter()
                .any(|(start, len, kind)| *kind == 1 && *start == expected_start && *len == 3),
            "Int after ':' should be tagged as TYPE, got: {tokens:?}"
        );
    }

    #[test]
    fn semantic_tokens_does_not_tag_lowercase_after_colon_as_type() {
        // A lowercase identifier after `:` is no longer a valid type (Phase
        // C rejects the old spellings at parse time), and in any case is a
        // binding/property name in the editor — it must not be tagged as
        // a TYPE token.
        let provider = SemanticTokensProvider::new(&[]);
        let line = "    port: int";
        let tokens = provider.tokenize_line(line, 0);

        let int_start = line.find("int").unwrap() as u32;
        assert!(
            !tokens
                .iter()
                .any(|(start, len, kind)| *kind == 1 && *start == int_start && *len == 3),
            "lowercase 'int' should not be tagged as TYPE, got: {tokens:?}"
        );
    }

    #[test]
    fn non_keyword_semantic_tokens_still_emitted() {
        // Defensive: make sure we didn't delete too much. TYPE, VARIABLE,
        // PROPERTY, STRING, NUMBER, OPERATOR, FUNCTION should all still appear.
        let provider = SemanticTokensProvider::new(&[]);
        let src = r#"let bucket = aws.s3_bucket {
    name = "b"
    count = 3
}
"#;
        let tokens = provider.tokenize(src);
        let kinds: std::collections::HashSet<u32> = tokens.iter().map(|t| t.token_type).collect();
        // 1 TYPE, 2 VARIABLE, 3 PROPERTY, 4 STRING, 5 NUMBER, 6 OPERATOR
        for expected in [1u32, 2, 3, 4, 5, 6] {
            assert!(
                kinds.contains(&expected),
                "Expected token_type {expected} in output. Got kinds: {kinds:?}"
            );
        }
    }

    /// Resolve LSP-encoded `(delta_line, delta_start, length, kind)` tokens
    /// to absolute `(line, start, length, kind)` quadruples. `delta_start`
    /// is line-relative when `delta_line == 0`, otherwise absolute.
    #[cfg(test)]
    fn absolute_tokens(
        tokens: &[tower_lsp::lsp_types::SemanticToken],
    ) -> Vec<(u32, u32, u32, u32)> {
        let mut line = 0u32;
        let mut start = 0u32;
        let mut out = Vec::with_capacity(tokens.len());
        for t in tokens {
            if t.delta_line == 0 {
                start += t.delta_start;
            } else {
                line += t.delta_line;
                start = t.delta_start;
            }
            out.push((line, start, t.length, t.token_type));
        }
        out
    }

    #[test]
    fn interpolation_split_emits_macro_token_inside_double_quoted_string() {
        let provider = SemanticTokensProvider::new(&[]);
        // `aws = "arn:${orgs}:root"`
        let tokens = provider.tokenize("aws = \"arn:${orgs}:root\"");
        let abs = absolute_tokens(&tokens);

        // Collect tokens on line 0 that fall inside the string literal
        // span (cols 6..24 inclusive of opening/closing quotes).
        let in_string: Vec<_> = abs
            .iter()
            .copied()
            .filter(|(line, start, _, _)| *line == 0 && *start >= 6 && *start <= 23)
            .collect();
        // Expected: STRING `"arn:` (cols 6..11), MACRO `${orgs}` (cols 11..18),
        // STRING `:root"` (cols 18..24).
        let kinds: Vec<u32> = in_string.iter().map(|(_, _, _, k)| *k).collect();
        assert!(
            kinds.contains(&9),
            "expected a MACRO (9) token for ${{orgs}}, got: {:?}",
            in_string
        );
        let macro_token = in_string
            .iter()
            .find(|(_, _, _, k)| *k == 9)
            .expect("MACRO token");
        assert_eq!(
            (macro_token.1, macro_token.2),
            (11, 7),
            "MACRO span must cover `${{orgs}}` (start=11, length=7); got: {:?}",
            macro_token
        );
    }

    #[test]
    fn interpolation_split_with_dotted_expression() {
        // `aws = "${a.b.c}"` — MACRO covers the whole `${a.b.c}` span,
        // not just `${...}`.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"${a.b.c}\"");
        let abs = absolute_tokens(&tokens);
        let macro_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 9).collect();
        assert_eq!(
            macro_tokens.len(),
            1,
            "expected one MACRO token, got: {:?}",
            abs
        );
        // `${a.b.c}` is 8 chars (`$`, `{`, `a`, `.`, `b`, `.`, `c`, `}`).
        assert_eq!(macro_tokens[0].2, 8, "MACRO length, got: {:?}", abs);
    }

    #[test]
    fn interpolation_split_handles_multiple_spans_on_one_line() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"${a}-${b}\"");
        let abs = absolute_tokens(&tokens);
        let macro_count = abs.iter().filter(|(_, _, _, k)| *k == 9).count();
        assert_eq!(
            macro_count, 2,
            "expected two MACRO tokens for two interpolations, got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_split_skipped_for_escaped_dollar_brace() {
        // `\${foo}` is a literal `${foo}` — no MACRO emitted.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"\\${foo}\"");
        let abs = absolute_tokens(&tokens);
        assert!(
            abs.iter().all(|(_, _, _, k)| *k != 9),
            "escaped `\\${{` must not produce a MACRO token; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_split_with_escaped_backslash_then_real_interpolation() {
        // `"\\${x}"` — `\\` is an escape pair for a literal `\`; the
        // following `${x}` is a real interpolation. Must emit MACRO.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"\\\\${x}\"");
        let abs = absolute_tokens(&tokens);
        let macro_count = abs.iter().filter(|(_, _, _, k)| *k == 9).count();
        assert_eq!(
            macro_count, 1,
            "real interpolation after `\\\\` must still emit MACRO; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_split_with_nested_object_literal() {
        // `${ {a = 1}.a }` — the brace-counter must descend into the
        // nested `{...}` map literal and close at the matching `}`,
        // not the first one.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"${ {a = 1}.a }\"");
        let abs = absolute_tokens(&tokens);
        let macro_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 9).collect();
        assert_eq!(
            macro_tokens.len(),
            1,
            "nested object literal inside `${{}}` must produce exactly one MACRO; got: {:?}",
            abs
        );
        // `${ {a = 1}.a }` is 14 chars long.
        assert_eq!(
            macro_tokens[0].2, 14,
            "MACRO span must cover the full balanced brace expression; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_split_with_multibyte_chars() {
        // Multi-byte chars (Japanese) before and inside the interpolation.
        // Char-count semantics must not be confused by UTF-8 byte width.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"日本${名前}本日\"");
        let abs = absolute_tokens(&tokens);
        let macro_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 9).collect();
        assert_eq!(
            macro_tokens.len(),
            1,
            "multi-byte chars must not affect MACRO detection; got: {:?}",
            abs
        );
        // `${名前}` is 5 chars (`$`, `{`, `名`, `前`, `}`).
        assert_eq!(
            macro_tokens[0].2, 5,
            "MACRO length is char-count, not byte-count; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_split_matches_issue_repro_arn_path() {
        // Verbatim repro from issue #2473: a 3-segment dotted interpolation
        // wrapped in literal text on both sides.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"arn:aws:iam::${orgs.accounts.registry_dev}:root\"");
        let abs = absolute_tokens(&tokens);
        let macro_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 9).collect();
        assert_eq!(
            macro_tokens.len(),
            1,
            "issue repro must emit exactly one MACRO token; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_empty_string_emits_single_string_token() {
        // `""` — opening + closing quote with no body. Must emit one
        // STRING token (length 2) and no MACRO.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"\"");
        let abs = absolute_tokens(&tokens);
        let string_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 4).collect();
        assert_eq!(
            string_tokens.len(),
            1,
            "empty string `\"\"` must emit exactly one STRING token; got: {:?}",
            abs
        );
        assert_eq!(string_tokens[0].2, 2, "STRING length covers both quotes");
        assert!(
            abs.iter().all(|(_, _, _, k)| *k != 9),
            "empty string must not produce a MACRO token; got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_unclosed_brace_does_not_panic() {
        // Mid-edit state: `${` opens but the user hasn't typed `}` yet.
        // The MACRO token covers from `${` to the closing `"`; the only
        // requirement is no panic and no missing-tail STRING.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = \"${unclosed\"");
        let abs = absolute_tokens(&tokens);
        let macro_count = abs.iter().filter(|(_, _, _, k)| *k == 9).count();
        assert_eq!(
            macro_count, 1,
            "expected one MACRO token for unclosed `${{`, got: {:?}",
            abs
        );
    }

    #[test]
    fn interpolation_not_split_in_single_quoted_string() {
        // Single-quoted strings are literal-only — `${foo}` is not an
        // interpolation, so the whole literal remains STRING.
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("aws = '${foo}'");
        let abs = absolute_tokens(&tokens);
        assert!(
            abs.iter().all(|(_, _, _, k)| *k != 9),
            "single-quoted strings must not produce MACRO tokens; got: {:?}",
            abs
        );
    }

    // =====================================================================
    // Heredoc body `${...}` MACRO split (#2482, parity with #2473)
    // =====================================================================

    #[test]
    fn heredoc_body_emits_macro_for_interpolation() {
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<EOT\nhello ${name}\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        let macro_tokens: Vec<_> = abs.iter().filter(|(_, _, _, k)| *k == 9).collect();
        assert_eq!(
            macro_tokens.len(),
            1,
            "expected one MACRO token in heredoc body for `${{name}}`; got: {:?}",
            abs
        );
        // `${name}` is 7 chars (`$`, `{`, `n`, `a`, `m`, `e`, `}`).
        assert_eq!(
            macro_tokens[0].2, 7,
            "MACRO span must cover the full `${{name}}`; got: {:?}",
            abs
        );
    }

    #[test]
    fn heredoc_body_without_dollar_stays_one_string_token() {
        let provider = SemanticTokensProvider::new(&[]);
        // No `$` anywhere — must still produce a single STRING token per
        // body line, not split.
        let input = "policy = <<EOT\nplain literal text\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        let body_line: Vec<_> = abs
            .iter()
            .filter(|(line, _, _, _)| *line == 1)
            .copied()
            .collect();
        assert!(
            body_line.iter().all(|(_, _, _, k)| *k != 9),
            "heredoc body without `$` must not emit MACRO; got: {:?}",
            body_line
        );
    }

    #[test]
    fn heredoc_body_handles_multiple_interpolations_per_line() {
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<EOT\n${a} and ${b}\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        let macro_count = abs.iter().filter(|(_, _, _, k)| *k == 9).count();
        assert_eq!(
            macro_count, 2,
            "two interpolations on one heredoc body line must produce two MACRO tokens; got: {:?}",
            abs
        );
    }

    #[test]
    fn heredoc_quoted_marker_does_not_split_interpolation() {
        // `<<'EOT'` is a literal heredoc — `${name}` is NOT expanded
        // by the parser, so the LSP must not split MACRO tokens
        // inside it either. See #2482.
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<'EOT'\nhello ${name}\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        assert!(
            abs.iter().all(|(_, _, _, k)| *k != 9),
            "quoted heredoc body must not produce MACRO tokens; got: {:?}",
            abs
        );
    }

    #[test]
    fn heredoc_indented_marker_still_splits_interpolation() {
        // `<<-EOT` (indented form) behaves the same as `<<EOT` for
        // interpolation purposes — the body still expands `${...}`.
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<-EOT\n  hello ${name}\n  EOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        let macro_count = abs.iter().filter(|(_, _, _, k)| *k == 9).count();
        assert_eq!(
            macro_count, 1,
            "`<<-EOT` body must split `${{name}}` as MACRO; got: {:?}",
            abs
        );
    }

    #[test]
    fn heredoc_escaped_dollar_brace_does_not_split() {
        // `\${name}` is a literal escape inside the heredoc body —
        // the splitter must skip the `\$` pair and not emit MACRO.
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<EOT\n\\${name}\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        assert!(
            abs.iter().all(|(_, _, _, k)| *k != 9),
            "escaped `\\${{` must not produce MACRO; got: {:?}",
            abs
        );
    }

    #[test]
    fn heredoc_closing_marker_line_stays_string() {
        // The closing marker line itself (e.g. `EOT`) is highlighted as
        // STRING by the existing path. The new split must not turn the
        // marker into something else.
        let provider = SemanticTokensProvider::new(&[]);
        let input = "policy = <<EOT\n${name}\nEOT";
        let tokens = provider.tokenize(input);
        let abs = absolute_tokens(&tokens);
        let last = abs
            .iter()
            .filter(|(line, _, _, _)| *line == 2)
            .copied()
            .next()
            .expect("expected a token on the closing-marker line");
        assert_eq!(
            last.3, 4,
            "closing marker must remain STRING; got kind {} on line 2",
            last.3
        );
    }
}
