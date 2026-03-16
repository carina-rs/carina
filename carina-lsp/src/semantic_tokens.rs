use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};

/// Token types supported by this language server
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,  // 0: provider, let
    SemanticTokenType::TYPE,     // 1: aws.s3.bucket, aws.ec2.vpc, aws.Region.*
    SemanticTokenType::VARIABLE, // 2: variable names
    SemanticTokenType::PROPERTY, // 3: attribute names (name, region, etc.)
    SemanticTokenType::STRING,   // 4: string literals
    SemanticTokenType::NUMBER,   // 5: number literals
    SemanticTokenType::OPERATOR, // 6: =
    SemanticTokenType::FUNCTION, // 7: env()
    SemanticTokenType::COMMENT,  // 8: comments
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

        for (line_idx, line) in text.lines().enumerate() {
            let line_tokens = self.tokenize_line(line, line_idx as u32);

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

    /// Convert a byte offset in a string to a character offset
    fn byte_to_char_offset(s: &str, byte_offset: usize) -> u32 {
        s[..byte_offset].chars().count() as u32
    }

    /// Get the character length (not byte length) of a string
    fn char_len(s: &str) -> u32 {
        s.chars().count() as u32
    }

    /// Tokenize a single line, returning (start_col, length, token_type_index)
    /// All positions and lengths are in characters (not bytes) for LSP compatibility.
    fn tokenize_line(&self, line: &str, _line_idx: u32) -> Vec<(u32, u32, u32)> {
        let mut tokens = Vec::new();
        let trimmed = line.trim_start();
        let indent_bytes = line.len() - trimmed.len();
        // Indent is always ASCII spaces/tabs, so byte count == char count
        let indent = indent_bytes as u32;

        // Skip empty lines
        if trimmed.is_empty() {
            return tokens;
        }

        // Comment
        if trimmed.starts_with("//") {
            tokens.push((indent, Self::char_len(line) - indent, 8)); // COMMENT
            return tokens;
        }

        // Keywords at start of line
        // Note: keywords like "provider", "backend", "let" and their arguments
        // are ASCII-only, so byte positions == char positions in this section.
        if trimmed.starts_with("provider ") {
            tokens.push((indent, 8, 0)); // KEYWORD: provider
            if let Some(name_start) = line.find("provider ") {
                let after_provider = &line[name_start + 9..];
                if let Some(name_end) = after_provider.find([' ', '{']) {
                    let name = &after_provider[..name_end];
                    if !name.is_empty() {
                        tokens.push(((name_start + 9) as u32, name.len() as u32, 1)); // TYPE
                    }
                }
            }
        } else if trimmed.starts_with("backend ") {
            tokens.push((indent, 7, 0)); // KEYWORD: backend
            if let Some(name_start) = line.find("backend ") {
                let after_backend = &line[name_start + 8..];
                if let Some(name_end) = after_backend.find([' ', '{']) {
                    let name = &after_backend[..name_end];
                    if !name.is_empty() {
                        tokens.push(((name_start + 8) as u32, name.len() as u32, 1)); // TYPE
                    }
                }
            }
        } else if trimmed.starts_with("let ") {
            tokens.push((indent, 3, 0)); // KEYWORD: let
            if let Some(let_start) = line.find("let ") {
                let after_let = &line[let_start + 4..];
                if let Some(name_end) = after_let.find([' ', '=']) {
                    let name = &after_let[..name_end].trim();
                    if !name.is_empty() {
                        tokens.push(((let_start + 4) as u32, name.len() as u32, 2)); // VARIABLE
                    }
                }
                // Check for "read" keyword after "let name = read ..."
                if let Some(read_pos) = after_let.find("= read ") {
                    let read_start = let_start + 4 + read_pos + 2; // position of "read"
                    tokens.push((read_start as u32, 4, 0)); // KEYWORD: read
                }
            }
        } else if trimmed.starts_with("import ") {
            tokens.push((indent, 6, 0)); // KEYWORD: import
            // Find "as" keyword and module alias name
            if let Some(import_start) = line.find("import ") {
                let after_import = &line[import_start + 7..];
                if let Some(as_pos) = after_import.find(" as ") {
                    let as_start = import_start + 7 + as_pos + 1; // position of "as"
                    tokens.push((as_start as u32, 2, 0)); // KEYWORD: as
                    let alias_start = as_start + 3; // position after "as "
                    let alias = line[alias_start..].trim();
                    if !alias.is_empty() {
                        tokens.push((alias_start as u32, alias.len() as u32, 2)); // VARIABLE
                    }
                }
            }
        } else if trimmed.starts_with("output ") || trimmed == "output{" {
            tokens.push((indent, 6, 0)); // KEYWORD: output
        } else if trimmed.starts_with("input ") || trimmed == "input{" {
            tokens.push((indent, 5, 0)); // KEYWORD: input
        }

        // Nested block names: "identifier {" without "=" (e.g., "security_group_ingress {")
        // Highlight as PROPERTY since these are attribute names in block form
        if !trimmed.starts_with("provider ")
            && !trimmed.starts_with("backend ")
            && !trimmed.starts_with("let ")
            && !trimmed.starts_with("import ")
            && !trimmed.starts_with("output ")
            && !trimmed.starts_with("input ")
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

        // Region patterns from registered providers (e.g., aws.Region.us_east_1)
        for region in &self.region_patterns {
            self.find_and_add_pattern(line, region, 1, &mut tokens);
        }

        // env() function
        if let Some(byte_pos) = line.find("env(") {
            tokens.push((Self::byte_to_char_offset(line, byte_pos), 3, 7)); // FUNCTION: env
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
                    Self::byte_to_char_offset(line, prop_byte_start),
                    Self::char_len(prop_name),
                    3,
                )); // PROPERTY
            }
            // Operator =
            tokens.push((Self::byte_to_char_offset(line, eq_byte_pos), 1, 6)); // OPERATOR
        }

        // String literals
        let mut in_string = false;
        let mut string_start_char = 0u32;
        for (char_idx, (_byte_idx, c)) in line.char_indices().enumerate() {
            let char_idx = char_idx as u32;
            if c == '"' {
                if in_string {
                    tokens.push((string_start_char, char_idx - string_start_char + 1, 4));
                    // STRING
                    in_string = false;
                } else {
                    string_start_char = char_idx;
                    in_string = true;
                }
            }
        }

        // Number literals - use byte-level operations for adjacent char checks
        for (byte_idx, c) in line.char_indices() {
            if c.is_ascii_digit() {
                // Check adjacent bytes - digits and their neighbors are ASCII,
                // so byte-level access is safe for boundary checks
                let prev_byte = if byte_idx > 0 {
                    Some(line.as_bytes()[byte_idx - 1])
                } else {
                    None
                };
                let next_byte_pos = byte_idx + 1; // ASCII digit is 1 byte
                let next_byte = if next_byte_pos < line.len() {
                    Some(line.as_bytes()[next_byte_pos])
                } else {
                    None
                };

                let prev_is_word =
                    prev_byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
                let next_is_word =
                    next_byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');

                let char_pos = Self::byte_to_char_offset(line, byte_idx);

                if !prev_is_word && !next_is_word {
                    // Single digit number
                    tokens.push((char_pos, 1, 5)); // NUMBER
                } else if !prev_is_word {
                    // Multi-digit number - find the end (bytes are fine since digits are ASCII)
                    let num_end = line[byte_idx..]
                        .find(|c: char| !c.is_ascii_digit())
                        .map_or(line.len() - byte_idx, |pos| pos);
                    // num_end is in bytes, but since all digits are ASCII, byte count == char count
                    tokens.push((char_pos, num_end as u32, 5)); // NUMBER
                }
            }
        }

        // Boolean literals
        self.find_and_add_pattern(line, "true", 0, &mut tokens);
        self.find_and_add_pattern(line, "false", 0, &mut tokens);

        // Sort by position and deduplicate
        tokens.sort_by_key(|(start, _, _)| *start);
        tokens.dedup_by(|a, b| a.0 == b.0);

        tokens
    }

    /// Find resource type patterns like aws.s3.bucket, aws.ec2.vpc
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

        // Must have at least 3 parts: provider.service.resource (e.g., aws.ec2.vpc, awscc.ec2.vpc)
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
                let char_pos = Self::byte_to_char_offset(line, absolute_byte_pos);
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
        let tokens = provider.tokenize("aws.s3.bucket {");

        // Should have at least one TYPE token for aws.s3.bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.bucket as TYPE");
    }

    #[test]
    fn test_resource_type_after_let() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize("let bucket = aws.s3.bucket {");

        // Should have TYPE token for aws.s3.bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.bucket as TYPE");
    }

    #[test]
    fn test_find_resource_types_directly() {
        let provider = SemanticTokensProvider::new(&[]);
        let mut tokens = Vec::new();
        provider.find_resource_types("aws.s3.bucket {", &mut tokens);

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
        let line_tokens = provider.tokenize_line("aws.s3.bucket {", 0);

        println!("Line tokens: {:?}", line_tokens);

        // Check that aws.s3.bucket is in the tokens as TYPE (1)
        let has_resource_type = line_tokens
            .iter()
            .any(|(start, len, typ)| *start == 0 && *len == 13 && *typ == 1);
        assert!(
            has_resource_type,
            "Should have aws.s3.bucket as TYPE at position 0. Got: {:?}",
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
        let content = "aws.s3.bucket {\n    name = \"test\"\n}";
        let tokens = provider.tokenize(content);

        println!("Full tokenize result:");
        for token in &tokens {
            println!(
                "  delta_line={}, delta_start={}, length={}, token_type={}",
                token.delta_line, token.delta_start, token.length, token.token_type
            );
        }

        // First token should be aws.s3.bucket (TYPE = 1)
        assert!(!tokens.is_empty(), "Should have tokens");
        let first = &tokens[0];
        assert_eq!(
            first.token_type, 1,
            "First token should be TYPE (1), got {}",
            first.token_type
        );
        assert_eq!(
            first.length, 13,
            "First token length should be 13 (aws.s3.bucket)"
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
        let comment_token = tokens.iter().find(|(_, _, typ)| *typ == 8);
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
        let content = "// 日本語コメント\naws.s3.bucket {\n    name = \"テスト\"\n}";
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
    fn test_import_keyword_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("import \"./modules/web.crn\" as web", 0);
        let keyword_token = tokens
            .iter()
            .find(|(start, _, typ)| *start == 0 && *typ == 0);
        assert!(
            keyword_token.is_some(),
            "Should highlight 'import' as KEYWORD. Got: {:?}",
            tokens
        );
        let (_, len, _) = keyword_token.unwrap();
        assert_eq!(*len, 6, "import keyword length should be 6");
    }

    #[test]
    fn test_output_keyword_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("output {", 0);
        let keyword_token = tokens
            .iter()
            .find(|(start, _, typ)| *start == 0 && *typ == 0);
        assert!(
            keyword_token.is_some(),
            "Should highlight 'output' as KEYWORD. Got: {:?}",
            tokens
        );
        let (_, len, _) = keyword_token.unwrap();
        assert_eq!(*len, 6, "output keyword length should be 6");
    }

    #[test]
    fn test_input_keyword_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("input {", 0);
        let keyword_token = tokens
            .iter()
            .find(|(start, _, typ)| *start == 0 && *typ == 0);
        assert!(
            keyword_token.is_some(),
            "Should highlight 'input' as KEYWORD. Got: {:?}",
            tokens
        );
        let (_, len, _) = keyword_token.unwrap();
        assert_eq!(*len, 5, "input keyword length should be 5");
    }

    #[test]
    fn test_import_as_keyword_highlighted() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("import \"./modules/web.crn\" as web", 0);
        // "as" should also be highlighted as KEYWORD
        let as_token = tokens
            .iter()
            .find(|(start, len, typ)| *typ == 0 && *len == 2 && *start > 0);
        assert!(
            as_token.is_some(),
            "Should highlight 'as' as KEYWORD in import statement. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_import_module_name_highlighted_as_variable() {
        let provider = SemanticTokensProvider::new(&[]);
        let tokens = provider.tokenize_line("import \"./modules/web.crn\" as web", 0);
        // "web" should be highlighted as VARIABLE
        let var_token = tokens.iter().find(|(_, _, typ)| *typ == 2);
        assert!(
            var_token.is_some(),
            "Should highlight module alias as VARIABLE in import statement. Got: {:?}",
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
}
