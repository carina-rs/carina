use tower_lsp::lsp_types::{SemanticToken, SemanticTokenType, SemanticTokensLegend};

/// Token types supported by this language server
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,  // 0: provider, let
    SemanticTokenType::TYPE,     // 1: aws.s3.bucket, aws.Region.*
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

pub struct SemanticTokensProvider;

impl SemanticTokensProvider {
    pub fn new() -> Self {
        Self
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

    /// Tokenize a single line, returning (start_col, length, token_type_index)
    fn tokenize_line(&self, line: &str, _line_idx: u32) -> Vec<(u32, u32, u32)> {
        let mut tokens = Vec::new();
        let trimmed = line.trim_start();
        let indent = (line.len() - trimmed.len()) as u32;

        // Skip empty lines
        if trimmed.is_empty() {
            return tokens;
        }

        // Comment
        if trimmed.starts_with("//") {
            tokens.push((indent, line.len() as u32 - indent, 8)); // COMMENT
            return tokens;
        }

        // Keywords at start of line
        if trimmed.starts_with("provider ") {
            tokens.push((indent, 8, 0)); // KEYWORD: provider
            // Provider name after "provider "
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
            // Backend type after "backend "
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
            // Variable name after "let "
            if let Some(let_start) = line.find("let ") {
                let after_let = &line[let_start + 4..];
                if let Some(name_end) = after_let.find([' ', '=']) {
                    let name = &after_let[..name_end].trim();
                    if !name.is_empty() {
                        tokens.push(((let_start + 4) as u32, name.len() as u32, 2)); // VARIABLE
                    }
                }
            }
        }

        // Resource type: aws.<service>.<resource> pattern
        self.find_resource_types(line, &mut tokens);

        // Region patterns: aws.Region.*
        for region in &[
            "aws.Region.ap_northeast_1",
            "aws.Region.ap_northeast_2",
            "aws.Region.ap_northeast_3",
            "aws.Region.ap_south_1",
            "aws.Region.ap_southeast_1",
            "aws.Region.ap_southeast_2",
            "aws.Region.ca_central_1",
            "aws.Region.eu_central_1",
            "aws.Region.eu_west_1",
            "aws.Region.eu_west_2",
            "aws.Region.eu_west_3",
            "aws.Region.eu_north_1",
            "aws.Region.sa_east_1",
            "aws.Region.us_east_1",
            "aws.Region.us_east_2",
            "aws.Region.us_west_1",
            "aws.Region.us_west_2",
        ] {
            self.find_and_add_pattern(line, region, 1, &mut tokens);
        }

        // env() function
        if let Some(start) = line.find("env(") {
            tokens.push((start as u32, 3, 7)); // FUNCTION: env
        }

        // Property names (before =)
        if let Some(eq_pos) = line.find('=') {
            let before_eq = &line[..eq_pos];
            let prop_name = before_eq.trim();
            if !prop_name.is_empty()
                && !prop_name.starts_with("provider")
                && !prop_name.starts_with("let")
                && !prop_name.contains('.')
                && let Some(prop_start) = line.find(prop_name)
            {
                tokens.push((prop_start as u32, prop_name.len() as u32, 3)); // PROPERTY
            }
            // Operator =
            tokens.push((eq_pos as u32, 1, 6)); // OPERATOR
        }

        // String literals
        let mut in_string = false;
        let mut string_start = 0;
        for (i, c) in line.char_indices() {
            if c == '"' {
                if in_string {
                    tokens.push((string_start as u32, (i - string_start + 1) as u32, 4)); // STRING
                    in_string = false;
                } else {
                    string_start = i;
                    in_string = true;
                }
            }
        }

        // Number literals
        for (i, c) in line.char_indices() {
            if c.is_ascii_digit() {
                // Check if it's a standalone number (not part of identifier)
                let prev_char = if i > 0 { line.chars().nth(i - 1) } else { None };
                let next_char = line.chars().nth(i + 1);

                if prev_char.is_none_or(|c| !c.is_alphanumeric() && c != '_')
                    && next_char.is_none_or(|c| !c.is_alphanumeric() && c != '_')
                {
                    // Single digit number
                    tokens.push((i as u32, 1, 5)); // NUMBER
                } else if prev_char.is_none_or(|c| !c.is_alphanumeric() && c != '_') {
                    // Multi-digit number - find the end
                    let num_end = line[i..]
                        .find(|c: char| !c.is_ascii_digit())
                        .map_or(line.len() - i, |pos| pos);
                    tokens.push((i as u32, num_end as u32, 5)); // NUMBER
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

    /// Find resource type patterns like aws.s3.bucket, gcp.storage.bucket
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

        // Must have exactly 3 parts: provider.service.resource
        if parts.len() == 3 {
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
            let absolute_pos = search_start + pos;
            // Check word boundaries - allow dots within identifiers
            let before_char = if absolute_pos > 0 {
                line.chars().nth(absolute_pos - 1)
            } else {
                None
            };
            let after_char = line.chars().nth(absolute_pos + pattern.len());

            let before_ok =
                before_char.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.');
            let after_ok = after_char.is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.');

            if before_ok && after_ok {
                tokens.push((absolute_pos as u32, pattern.len() as u32, token_type));
            }
            search_start = absolute_pos + pattern.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_type_at_line_start() {
        let provider = SemanticTokensProvider::new();
        let tokens = provider.tokenize("aws.s3.bucket {");

        // Should have at least one TYPE token for aws.s3.bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.bucket as TYPE");
    }

    #[test]
    fn test_resource_type_after_let() {
        let provider = SemanticTokensProvider::new();
        let tokens = provider.tokenize("let bucket = aws.s3.bucket {");

        // Should have TYPE token for aws.s3.bucket
        let type_tokens: Vec<_> = tokens.iter().filter(|t| t.token_type == 1).collect();
        assert!(!type_tokens.is_empty(), "Should find aws.s3.bucket as TYPE");
    }

    #[test]
    fn test_find_resource_types_directly() {
        let provider = SemanticTokensProvider::new();
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
        let provider = SemanticTokensProvider::new();
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
    fn test_tokenize_full_file() {
        let provider = SemanticTokensProvider::new();
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
}
