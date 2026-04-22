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
        let mut heredoc_marker: Option<String> = None;

        for (line_idx, line) in text.lines().enumerate() {
            // Handle heredoc state: if we're inside a heredoc, highlight the whole line as string
            if let Some(ref marker) = heredoc_marker {
                let trimmed = line.trim();
                let line_len = position::char_len(line);
                if line_len > 0 {
                    let delta_line = line_idx as u32 - prev_line;
                    // Heredoc body is always on a new line, so delta_start is 0
                    let delta_start = 0;
                    tokens.push(SemanticToken {
                        delta_line,
                        delta_start,
                        length: line_len,
                        token_type: 4, // STRING
                        token_modifiers_bitset: 0,
                    });
                    prev_line = line_idx as u32;
                    prev_start = 0;
                }
                if trimmed == marker {
                    heredoc_marker = None;
                }
                continue;
            }

            // Check if this line starts a heredoc
            if let Some(marker) = carina_core::heredoc::find_heredoc_marker(line) {
                // Tokenize the part before the heredoc normally
                let line_tokens = self.tokenize_line_with_block_comments(
                    line,
                    line_idx as u32,
                    &mut block_comment_depth,
                );

                for (start, length, token_type) in line_tokens {
                    let delta_line = line_idx as u32 - prev_line;
                    let delta_start = if delta_line == 0 {
                        start - prev_start
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

                    prev_line = line_idx as u32;
                    prev_start = start;
                }

                heredoc_marker = Some(marker);
                continue;
            }

            let line_tokens = self.tokenize_line_with_block_comments(
                line,
                line_idx as u32,
                &mut block_comment_depth,
            );

            for (start, length, token_type) in line_tokens {
                let delta_line = line_idx as u32 - prev_line;
                let delta_start = if delta_line == 0 {
                    start - prev_start
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

                prev_line = line_idx as u32;
                prev_start = start;
            }
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
        // Scan for /* to find inline block comments.
        let mut i = 0;
        while i < chars.len() {
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
                *block_comment_depth = 1;
                let comment_start = i as u32;
                i += 2;
                // Scan for end of block comment on same line
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

        // String literals (double-quoted and single-quoted)
        {
            let mut in_string = false;
            let mut string_start_char = 0u32;
            let mut string_quote_char = '"';
            let mut escaped = false;
            for (char_idx, (_byte_idx, c)) in line.char_indices().enumerate() {
                let char_idx = char_idx as u32;
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == string_quote_char {
                        tokens.push((string_start_char, char_idx - string_start_char + 1, 4));
                        // STRING
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
    fn test_import_let_binding_variable_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("let web = import \"./modules/web.crn\"", 0);
        // The module alias `web` is still emitted as VARIABLE even though `let`
        // itself is handled by the TextMate grammar (see #1948).
        let var_token = tokens.iter().find(|(_, _, typ)| *typ == 2);
        assert!(
            var_token.is_some(),
            "Should highlight module alias as VARIABLE in let import. Got: {:?}",
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
    fn test_find_heredoc_marker() {
        use carina_core::heredoc::find_heredoc_marker;
        assert_eq!(
            find_heredoc_marker("policy = <<EOT"),
            Some("EOT".to_string())
        );
        assert_eq!(find_heredoc_marker("x = <<-EOF"), Some("EOF".to_string()));
        assert_eq!(find_heredoc_marker("x = \"<<EOT\""), None);
        assert_eq!(find_heredoc_marker("x = 123"), None);
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
            "attributes { a: string = \"x\" }",
            "arguments { a: string }",
            "exports { a: string = \"x\" }",
            "import { to = aws.s3_bucket \"x\" }",
            "moved { from = aws.s3_bucket \"a\" to = aws.s3_bucket \"b\" }",
            "removed { from = aws.s3_bucket \"x\" }",
            "for az in [\"a\", \"b\"] { aws.s3_bucket { name = az } }",
            "if cond { aws.s3_bucket { name = \"x\" } }",
            "} else { aws.s3_bucket { name = \"y\" } }",
            "require !empty(name), \"must be set\"",
            "let r = read aws.s3_bucket { name = \"x\" }",
            "let m = import \"./modules/web\"",
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
        // A lowercase identifier after `:` is either the old spelling (still
        // accepted by the parser but not a bare PascalCase type) or a
        // binding/property name; don't emit a type token for it.
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
}
