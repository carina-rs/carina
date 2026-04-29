//! Main formatting logic
//!
//! This module owns the public API (`format`, `format_with_block_names`,
//! `needs_format`), the [`Formatter`] state struct, the top-level
//! [`Formatter::format_file`] / [`Formatter::format_node`] dispatch, and
//! shared low-level helpers (`write*`, `is_*`, `format_default`,
//! `normalize_string_quotes`).
//!
//! Per-rule methods live in `formatter::rules::*`. Each submodule there
//! contributes additional `impl Formatter` blocks for one DSL topic
//! (resources, providers, attributes, expressions, values, functions,
//! modules). Rust supports multiple `impl` blocks for the same type
//! across files, so the public API is bit-identical to the pre-split
//! layout.

use super::config::FormatConfig;
use super::cst::{Cst, CstChild, CstNode, NodeKind, Trivia};
use super::cst_builder::build_cst;
use super::parser::{self, FormatParseError};

/// Format a .crn file
pub fn format(source: &str, config: &FormatConfig) -> Result<String, FormatParseError> {
    let preprocess_result =
        crate::heredoc::preprocess_heredocs(source).map_err(|e| FormatParseError {
            message: e.to_string(),
            line: 0,
            column: 0,
        })?;
    let pairs = parser::parse(&preprocess_result.source)?;
    let cst = build_cst(&preprocess_result.source, pairs);
    let formatter = Formatter::new(config.clone());
    let formatted = formatter.format(&cst);
    Ok(crate::heredoc::restore_heredocs(
        &formatted,
        &preprocess_result.heredocs,
    ))
}

/// Format a .crn file, converting `= [{...}]` to block syntax for attributes
/// listed in `block_names`. The map key is the attribute name (e.g., "operating_regions")
/// and the value is the block name to use (e.g., "operating_region").
pub fn format_with_block_names(
    source: &str,
    config: &FormatConfig,
    block_names: &std::collections::HashMap<String, String>,
) -> Result<String, FormatParseError> {
    let preprocess_result =
        crate::heredoc::preprocess_heredocs(source).map_err(|e| FormatParseError {
            message: e.to_string(),
            line: 0,
            column: 0,
        })?;
    let pairs = parser::parse(&preprocess_result.source)?;
    let cst = build_cst(&preprocess_result.source, pairs);
    let formatter = Formatter::with_block_names(config.clone(), block_names.clone());
    let formatted = formatter.format(&cst);
    Ok(crate::heredoc::restore_heredocs(
        &formatted,
        &preprocess_result.heredocs,
    ))
}

/// Check if a file needs formatting
pub fn needs_format(source: &str, config: &FormatConfig) -> Result<bool, FormatParseError> {
    let formatted = format(source, config)?;
    Ok(formatted != source)
}

pub(in crate::formatter) struct Formatter {
    pub(in crate::formatter) config: FormatConfig,
    pub(in crate::formatter) output: String,
    pub(in crate::formatter) current_indent: usize,
    pub(in crate::formatter) block_names: std::collections::HashMap<String, String>,
}

impl Formatter {
    fn new(config: FormatConfig) -> Self {
        Self {
            config,
            output: String::new(),
            current_indent: 0,
            block_names: std::collections::HashMap::new(),
        }
    }

    fn with_block_names(
        config: FormatConfig,
        block_names: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            config,
            output: String::new(),
            current_indent: 0,
            block_names,
        }
    }

    fn format(mut self, cst: &Cst) -> String {
        self.format_file(&cst.root);
        self.output
    }

    fn format_file(&mut self, node: &CstNode) {
        let mut prev_was_block = false;
        let mut pending_comments: Vec<&Trivia> = Vec::new();
        let mut blank_line_count = 0;

        for child in &node.children {
            match child {
                CstChild::Trivia(trivia) => match trivia {
                    Trivia::LineComment(_) | Trivia::BlockComment(_) => {
                        pending_comments.push(trivia);
                        blank_line_count = 0;
                    }
                    Trivia::Newline => {
                        blank_line_count += 1;
                    }
                    Trivia::Whitespace(_) => {
                        // Normalize whitespace
                    }
                },
                CstChild::Node(child_node) => {
                    // Add blank lines between blocks
                    if prev_was_block {
                        self.write_newlines(self.config.blank_lines_between_blocks);
                    }

                    // Write pending comments before the block
                    if !pending_comments.is_empty() {
                        for comment in pending_comments.drain(..) {
                            self.write_trivia(comment);
                            self.write_newline();
                        }
                        // Add blank line after comments if there was one in the original
                        if blank_line_count > 1 {
                            self.write_newline();
                        }
                    }

                    self.format_node(child_node);
                    prev_was_block = true;
                    blank_line_count = 0;
                }
                CstChild::Token(_) => {}
            }
        }

        // Write any remaining comments at end of file
        for comment in pending_comments {
            self.write_trivia(comment);
            self.write_newline();
        }

        // Ensure file ends with exactly one newline (trim extra trailing newlines)
        let trimmed = self.output.trim_end();
        self.output = format!("{}\n", trimmed);
    }

    pub(in crate::formatter) fn format_node(&mut self, node: &CstNode) {
        match node.kind {
            NodeKind::UseExpr => self.format_use_expr(node),
            NodeKind::BackendBlock => self.format_backend_block(node),
            NodeKind::ProviderBlock => self.format_provider_block(node),
            NodeKind::ArgumentsBlock => self.format_arguments_block(node),
            NodeKind::AttributesBlock => self.format_attributes_block(node),
            NodeKind::ExportsBlock => self.format_attributes_block(node), // same format as attributes
            NodeKind::LetBinding => self.format_let_binding(node),
            NodeKind::LocalBinding => self.format_let_binding(node),
            NodeKind::ModuleCall => self.format_module_call(node),
            NodeKind::ImportStateBlock => self.format_state_block(node, "import"),
            NodeKind::RemovedBlock => self.format_state_block(node, "removed"),
            NodeKind::MovedBlock => self.format_state_block(node, "moved"),
            NodeKind::RequireStatement => self.format_require_statement(node),
            NodeKind::ImportToAttr
            | NodeKind::ImportIdAttr
            | NodeKind::RemovedFromAttr
            | NodeKind::MovedFromAttr
            | NodeKind::MovedToAttr => self.format_state_block_attr(node),
            NodeKind::ResourceAddress => self.format_resource_address(node),
            NodeKind::AnonymousResource => self.format_anonymous_resource(node),
            NodeKind::ResourceExpr => self.format_resource_expr(node),
            NodeKind::ReadResourceExpr => self.format_read_resource_expr(node),
            NodeKind::UpstreamStateExpr => self.format_upstream_state_expr(node),
            NodeKind::FnDef => self.format_fn_def(node),
            NodeKind::FnParam => self.format_default(node),
            NodeKind::ForExpr => self.format_for_expr(node),
            NodeKind::IfExpr => self.format_if_expr(node),
            NodeKind::ElseClause => self.format_else_clause(node),
            NodeKind::Attribute => self.format_attribute(node, 0),
            NodeKind::NestedBlock => self.format_nested_block(node),
            NodeKind::ArgumentsParam => self.format_arguments_param(node, 0),
            NodeKind::ArgumentsParamBlock => self.format_arguments_param_block(node),
            NodeKind::ArgumentsParamAttr => self.format_arguments_param_attr(node, 0),
            NodeKind::AttributesParam => self.format_attributes_param(node, 0),
            NodeKind::ExportsParam => self.format_attributes_param(node, 0), // same format as attributes
            NodeKind::PipeExpr => self.format_pipe_expr(node),
            NodeKind::ComposeExpr => self.format_compose_expr(node),
            NodeKind::FunctionCall => self.format_function_call(node),
            NodeKind::VariableRef => self.format_variable_ref(node),
            NodeKind::List => self.format_list(node),
            NodeKind::Map => self.format_map(node),
            NodeKind::MapEntry => self.format_map_entry(node),
            NodeKind::TypeExpr => self.format_type_expr(node),
            _ => self.format_default(node),
        }
    }

    fn normalize_string_quotes(s: &str) -> String {
        if !s.starts_with('"') {
            return s.to_string();
        }
        let inner = &s[1..s.len() - 1];
        if inner.contains("${") || inner.contains('\'') {
            return s.to_string();
        }
        if inner.contains("\\n")
            || inner.contains("\\r")
            || inner.contains("\\t")
            || inner.contains("\\\"")
        {
            return s.to_string();
        }
        format!("'{}'", inner)
    }

    pub(in crate::formatter) fn write_token(&mut self, text: &str) {
        if text.starts_with('"') {
            self.write(&Self::normalize_string_quotes(text));
        } else {
            self.write(text);
        }
    }

    pub(in crate::formatter) fn format_default(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    self.write_token(&token.text);
                }
                CstChild::Node(n) => {
                    self.format_node(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    // Helper methods

    pub(in crate::formatter) fn write(&mut self, s: &str) {
        self.output.push_str(s);
    }

    pub(in crate::formatter) fn write_indent(&mut self) {
        let indent = self.config.indent_string().repeat(self.current_indent);
        self.output.push_str(&indent);
    }

    pub(in crate::formatter) fn write_newline(&mut self) {
        self.output.push('\n');
    }

    pub(in crate::formatter) fn write_newlines(&mut self, count: usize) {
        for _ in 0..count {
            self.write_newline();
        }
    }

    pub(in crate::formatter) fn write_trivia(&mut self, trivia: &Trivia) {
        match trivia {
            Trivia::LineComment(s) | Trivia::BlockComment(s) => self.write(s),
            Trivia::Newline => self.write_newline(),
            Trivia::Whitespace(s) => self.write(s),
        }
    }

    pub(in crate::formatter) fn is_identifier(&self, s: &str) -> bool {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    /// Check if a token is a valid key (identifier or quoted string).
    pub(in crate::formatter) fn is_key_token(&self, s: &str) -> bool {
        self.is_identifier(s) || self.is_quoted_string(s)
    }

    fn is_quoted_string(&self, s: &str) -> bool {
        (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
            || (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_provider_block() {
        let input = "provider aws {\nregion=aws.Region.ap_northeast_1\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_preserves_comments() {
        let input = "# This is a comment\nprovider aws {\n  region = aws.Region.ap_northeast_1\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert!(formatted.contains("# This is a comment"));
    }

    #[test]
    fn test_format_normalizes_indentation() {
        let input = "provider aws {\n    region = aws.Region.ap_northeast_1\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_struct_type_canonical_spacing() {
        let input = "attributes {\nperson:struct{name:string,age:int}\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "attributes {\n  person: struct { name: string, age: int }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_struct_type_empty() {
        let input = "attributes {\nperson:struct{}\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "attributes {\n  person: struct {}\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_struct_type_nested_in_list_and_map() {
        let input = "attributes {\npeople:list(struct{name:string,age:int})\nlookup:map(struct{flag:bool})\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "attributes {\n  people: list(struct { name: string, age: int })\n  lookup: map(struct { flag: bool })\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_struct_type_field_is_itself_struct() {
        let input = "attributes {\np:struct{addr:struct{zip:string,city:string},age:int}\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "attributes {\n  p: struct { addr: struct { zip: string, city: string }, age: int }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_aligns_attributes() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1
  profile = "default"
  account_id = "123456789012"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region     = aws.Region.ap_northeast_1
  profile    = 'default'
  account_id = '123456789012'
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_idempotent() {
        let input = r#"provider aws {
  region     = aws.Region.ap_northeast_1
  profile    = 'default'
  account_id = '123456789012'
}
"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_let_binding() {
        let input = "let v = aws.ec2.Vpc {\nname=\"main\"\ncidr_block=\"10.0.0.0/16\"\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert!(formatted.contains("let v = aws.ec2.Vpc"));
    }

    #[test]
    fn test_needs_format() {
        let unformatted = "provider aws{region=aws.Region.ap_northeast_1}";
        let formatted = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        let config = FormatConfig::default();
        assert!(needs_format(unformatted, &config).unwrap());
        assert!(!needs_format(formatted, &config).unwrap());
    }

    #[test]
    fn test_format_map() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1
  default_tags = {
    Environment = "prod"
    ManagedBy = "carina"
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region = aws.Region.ap_northeast_1

  default_tags = {
    Environment = 'prod'
    ManagedBy   = 'carina'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_map_aligns_entries() {
        let input = r#"provider aws {
  default_tags = {
    Environment = "prod"
    ManagedBy = "carina"
    Owner = "team-platform"
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  default_tags = {
    Environment = 'prod'
    ManagedBy   = 'carina'
    Owner       = 'team-platform'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_map_idempotent() {
        let input = r#"provider aws {
  default_tags = {
    Environment = 'prod'
    ManagedBy   = 'carina'
  }
}
"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_preserves_blank_lines_between_attributes() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1

  profile = "default"
  account_id = "123456789012"

  default_tags = {
    Environment = "prod"
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region = aws.Region.ap_northeast_1

  profile    = 'default'
  account_id = '123456789012'

  default_tags = {
    Environment = 'prod'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_blank_lines_idempotent() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1

  profile    = 'default'
  account_id = '123456789012'
}
"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_aligns_within_groups_separated_by_blank_lines() {
        let input = r#"provider aws {
  short = "a"
  much_longer_key = "b"

  x = "c"
  longer_key = "d"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // First group: "short" and "much_longer_key" align together
        // Second group: "x" and "longer_key" align together (independent of first group)
        let expected = r#"provider aws {
  short           = 'a'
  much_longer_key = 'b'

  x          = 'c'
  longer_key = 'd'
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_empty_anonymous_resource_block() {
        let input = "aws.s3.bucket {\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "aws.s3.bucket {}\n";
        assert_eq!(formatted, expected);

        // Test variations of empty blocks
        let input2 = "aws.s3.bucket {}";
        let formatted2 = format(input2, &config).unwrap();
        assert_eq!(formatted2, expected);

        let input3 = "aws.s3.bucket {\n\n\n}";
        let formatted3 = format(input3, &config).unwrap();
        assert_eq!(formatted3, expected);
    }

    #[test]
    fn test_format_empty_let_binding_resource_block() {
        let input = "let bucket = aws.s3.bucket {\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let bucket = aws.s3.bucket {}\n";
        assert_eq!(formatted, expected);

        // Test that it stays compact even with multiple empty lines
        let input2 = "let bucket = aws.s3.bucket {\n\n\n}";
        let formatted2 = format(input2, &config).unwrap();
        assert_eq!(formatted2, expected);
    }

    #[test]
    fn test_format_nonempty_block_remains_multiline() {
        let input = r#"aws.s3.bucket {
  name = "my-bucket"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "aws.s3.bucket {\n  name = 'my-bucket'\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_empty_block_idempotent() {
        let inputs = vec!["aws.s3.bucket {}\n", "let v = aws.ec2.Vpc {}\n"];
        let config = FormatConfig::default();
        for input in inputs {
            let formatted = format(input, &config).unwrap();
            assert_eq!(formatted, input, "Input was not idempotent: {input}");
        }
    }

    #[test]
    fn test_format_nested_block() {
        let input = r#"provider aws {
  assume_role {
    role_arn = "arn:aws:iam::123456789012:role/MyRole"
    session_name = "my-session"
  }
  region = aws.Region.ap_northeast_1
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  assume_role {
    role_arn     = 'arn:aws:iam::123456789012:role/MyRole'
    session_name = 'my-session'
  }
  region = aws.Region.ap_northeast_1
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_nested_block_in_map() {
        let input = r#"provider aws {
  default_tags = {
    Environment = "prod"
    nested {
      key1 = "v1"
    }
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Just verify it doesn't crash and contains expected content
        assert!(formatted.contains("nested {"));
        assert!(formatted.contains("key1 = 'v1'"));
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_simple() {
        let input = r#"provider aws {
  operating_regions = [{
    region_name = "ap-northeast-1"
  }]
}"#;
        let config = FormatConfig::default();
        let mut block_names = std::collections::HashMap::new();
        block_names.insert(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        );
        let formatted = format_with_block_names(input, &config, &block_names).unwrap();
        let expected = r#"provider aws {
  operating_region {
    region_name = 'ap-northeast-1'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_multiple_items() {
        let input = r#"provider aws {
  operating_regions = [
    { region_name = "ap-northeast-1" },
    { region_name = "us-east-1" },
  ]
}"#;
        let config = FormatConfig::default();
        let mut block_names = std::collections::HashMap::new();
        block_names.insert(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        );
        let formatted = format_with_block_names(input, &config, &block_names).unwrap();
        let expected = r#"provider aws {
  operating_region {
    region_name = 'ap-northeast-1'
  }
  operating_region {
    region_name = 'us-east-1'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_nested() {
        let input = r#"aws.ipam.IpamPool {
  name = "my-pool"
  ipam_scope_id = "ipam-scope-12345"
  operating_regions = [{ region_name = "ap-northeast-1" }]
}"#;
        let config = FormatConfig::default();
        let mut block_names = std::collections::HashMap::new();
        block_names.insert(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        );
        let formatted = format_with_block_names(input, &config, &block_names).unwrap();
        let expected = r#"aws.ipam.IpamPool {
  name          = 'my-pool'
  ipam_scope_id = 'ipam-scope-12345'

  operating_region {
    region_name = 'ap-northeast-1'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_convert_block_syntax_is_idempotent() {
        let input = r#"provider aws {
  operating_region {
    region_name = 'ap-northeast-1'
  }
}
"#;
        let config = FormatConfig::default();
        let mut block_names = std::collections::HashMap::new();
        block_names.insert(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        );
        let formatted = format_with_block_names(input, &config, &block_names).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_attributes_without_type() {
        let input = "module my_module {\nattributes {\nshort_key = \"value1\"\nmuch_longer_key = \"value2\"\n}\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "module my_module {\n  attributes {\n    short_key       = 'value1'\n    much_longer_key = 'value2'\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_attributes_mixed_typed_and_untyped() {
        let input = "module my_module {\nattributes {\nshort_key: string\nmuch_longer_key = \"value2\"\nanother_key: int\n}\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Both typed and untyped attributes should be aligned together
        let expected = "module my_module {\n  attributes {\n    short_key      : string\n    much_longer_key = 'value2'\n    another_key    : int\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_index_access() {
        let input = r#"let first_subnet = subnets[0]
let by_az = subnets["us-east-1a"]
let nested = data.subnets[0]"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let first_subnet = subnets[0]\nlet by_az = subnets['us-east-1a']\nlet nested = data.subnets[0]\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_string_index_access() {
        let input = r#"let region = config["region"]"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let region = config['region']\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_for_expression() {
        let input = r#"for cidr in cidrs {
  aws.ec2.Subnet {
    cidr_block = cidr
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Just verify it doesn't crash and preserves the structure
        assert!(formatted.contains("for cidr in cidrs"));
        assert!(formatted.contains("aws.ec2.Subnet"));
        assert!(formatted.contains("cidr_block"));
    }

    #[test]
    fn test_format_read_resource_expr() {
        let input = "let pool = read aws.ipam.IpamPool {\nipam_scope_id = scope.ipam_scope_id\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected =
            "let pool = read aws.ipam.IpamPool {\n  ipam_scope_id = scope.ipam_scope_id\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_function_call_in_primary() {
        let input = "let vpc = aws.ec2.Vpc {\nname = format(\"vpc-%s\", env)\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Just verify the function call is preserved
        assert!(formatted.contains("format("));
    }

    #[test]
    fn issue_1177_blank_lines_around_map_attributes() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1
  profile = "default"
  default_tags = {
    Environment = "prod"
  }
  account_id = "123456789012"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region  = aws.Region.ap_northeast_1
  profile = 'default'

  default_tags = {
    Environment = 'prod'
  }

  account_id = '123456789012'
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn issue_1177_blank_lines_around_map_alignment_reset() {
        let input = r#"provider aws {
  short = "a"
  much_longer_key = "b"
  default_tags = {
    Environment = "prod"
  }
  x = "c"
  longer_key = "d"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  short           = 'a'
  much_longer_key = 'b'

  default_tags = {
    Environment = 'prod'
  }

  x          = 'c'
  longer_key = 'd'
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn issue_1177_map_first_attribute_no_leading_blank_line() {
        let input = r#"provider aws {
  default_tags = {
    Environment = "prod"
  }
  region = aws.Region.ap_northeast_1
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  default_tags = {
    Environment = 'prod'
  }

  region = aws.Region.ap_northeast_1
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn issue_1177_map_last_attribute_no_trailing_blank_line() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1
  default_tags = {
    Environment = "prod"
  }
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region = aws.Region.ap_northeast_1

  default_tags = {
    Environment = 'prod'
  }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn issue_1177_empty_map_no_blank_lines() {
        let input = r#"provider aws {
  region = aws.Region.ap_northeast_1
  default_tags = {}
  profile = "default"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  region       = aws.Region.ap_northeast_1
  default_tags = {}
  profile      = 'default'
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn issue_1177_idempotent() {
        let input = r#"provider aws {
  region  = aws.Region.ap_northeast_1
  profile = 'default'

  default_tags = {
    Environment = 'prod'
  }

  account_id = '123456789012'
}
"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn format_arguments_param_block_form() {
        let input = "module my_module {
arguments {
name: string {
description = \"Name of the resource\"
default = \"default-name\"
}
}
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "module my_module {\n  arguments {\n    name: string {\n      description = 'Name of the resource'\n      default     = 'default-name'\n    }\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_arguments_mixed_simple_and_block_form() {
        let input = "module my_module {
arguments {
simple_arg: string
complex_arg: int {
description = \"A complex argument\"
default = 42
}
}
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "module my_module {\n  arguments {\n    simple_arg: string\n    complex_arg: int {\n      description = 'A complex argument'\n      default     = 42\n    }\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_arguments_block_form_idempotent() {
        let input = "module my_module {\n  arguments {\n    name: string {\n      description = 'Name of the resource'\n      default     = 'default-name'\n    }\n  }\n}\n";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn format_arguments_mixed_with_alignment() {
        let input = "module my_module {
arguments {
short: string
much_longer_name: int
mid: bool {
default = true
}
}
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Block-form params should NOT be aligned with simple ones
        let expected = "module my_module {\n  arguments {\n    short           : string\n    much_longer_name: int\n    mid: bool {\n      default = true\n    }\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_arguments_block_form_with_validation_block() {
        let input = "module my_module {
arguments {
name: string {
description = \"The name\"
validation {
condition = length(name) > 0
error_message = \"Name cannot be empty\"
}
}
}
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Just verify it doesn't crash and preserves key elements
        assert!(formatted.contains("name: string {"));
        assert!(formatted.contains("description = 'The name'"));
        assert!(formatted.contains("validation {"));
        assert!(formatted.contains("condition"));
    }

    #[test]
    fn format_fn_def_simple() {
        let input = "fn my_func(x) {\nx + 1\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x) {\n  x + 1\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_default_param() {
        let input = "fn my_func(x = 0) {\nx\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x = 0) {\n  x\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_local_let() {
        let input = "fn my_func(x) {\nlet y = x + 1\ny * 2\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x) {\n  let y = x + 1\n  y * 2\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_typed_params() {
        let input = "fn my_func(x: int, y: string) {\nx\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x: int, y: string) {\n  x\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_typed_param_and_default() {
        let input = "fn my_func(x: int = 0) {\nx\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x: int = 0) {\n  x\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_resource_type_param() {
        let input = "fn my_func(vpc: aws.ec2.Vpc) {\nvpc\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(vpc: aws.ec2.Vpc) {\n  vpc\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_mixed_typed_untyped() {
        let input = "fn my_func(x: int, y) {\ny\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x: int, y) {\n  y\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_return_type() {
        let input = "fn my_func(x: int): int {\nx + 1\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "fn my_func(x: int): int {\n  x + 1\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_with_resource_return_type() {
        let input = "fn my_func(name: string): aws.ec2.Vpc {\naws.ec2.Vpc { name = name }\n}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected =
            "fn my_func(name: string): aws.ec2.Vpc {\n  aws.ec2.Vpc { name = name }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn format_fn_def_without_return_type_unchanged() {
        let input = "fn my_func(x) {\n  x + 1\n}\n";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn format_custom_schema_type_annotations() {
        let input = "module my_module {
arguments {
versioning: aws.s3.Bucket.VersioningConfiguration
acl: aws.s3.Bucket.AccessControl = aws.s3.Bucket.AccessControl.private
}
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "module my_module {\n  arguments {\n    versioning: aws.s3.Bucket.VersioningConfiguration\n    acl       : aws.s3.Bucket.AccessControl = aws.s3.Bucket.AccessControl.private\n  }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_require_statement() {
        // Single require
        let input = "require carina_version >= '0.1.0'";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "require carina_version >= '0.1.0'\n";
        assert_eq!(formatted, expected);

        // Multiple requires with comma
        let input = "require carina_version >= '0.1.0', carina_version < '1.0.0'";
        let formatted = format(input, &config).unwrap();
        let expected = "require carina_version >= '0.1.0', carina_version < '1.0.0'\n";
        assert_eq!(formatted, expected);

        // Idempotent
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn format_heredoc_preserved() {
        let input = r#"let policy = aws.iam.Policy {
  name = "test"
  policy = <<-EOF
  {
    "Version": "2012-10-17"
  }
  EOF
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // Heredoc content should be preserved unchanged
        assert!(formatted.contains("<<-EOF"));
        assert!(formatted.contains("\"Version\": \"2012-10-17\""));
        assert!(formatted.contains("EOF"));
    }

    #[test]
    fn format_heredoc_idempotent() {
        let input = "let x = <<-EOF\n  hello world\n  EOF\n";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn test_format_normalizes_double_to_single_quotes() {
        let input = r#"let v = "hello""#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let v = 'hello'\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_preserves_double_quotes_for_interpolation() {
        let input = r#"let v = "hello ${name}""#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let v = \"hello ${name}\"\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_preserves_single_quotes() {
        let input = r#"let v = 'hello'"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let v = 'hello'\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_normalizes_quotes_in_list() {
        let input = r#"let v = ["a", "b", "c"]"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let v = ['a', 'b', 'c']\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_preserves_double_quotes_with_single_quote_char() {
        let input = r#"let v = "it's a test""#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let v = \"it's a test\"\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_normalizes_use_source_quotes() {
        let input = r#"use {
  source = "../shared"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "use {\n  source = '../shared'\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_quote_normalization_idempotent() {
        let input = "let v = 'hello'\nlet w = \"hello ${name}\"\n";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn test_format_quoted_map_keys() {
        let input = r#"let m = {
  "10.0.1.0/24" = "ap-northeast-1a"
  "10.0.2.0/24" = "ap-northeast-1c"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let m = {\n  '10.0.1.0/24' = 'ap-northeast-1a'\n  '10.0.2.0/24' = 'ap-northeast-1c'\n}\n";
        assert_eq!(formatted, expected);

        // Idempotent
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn test_format_quoted_attribute_key_in_block() {
        let input = r#"aws.ec2.Vpc {
  "name" = "main"
  cidr_block = "10.0.0.0/16"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "aws.ec2.Vpc {\n  'name'     = 'main'\n  cidr_block = '10.0.0.0/16'\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_comment_stays_above_attribute() {
        let input = r#"provider aws {
  # Comment for region
  region = aws.Region.ap_northeast_1

  # Comment for profile
  profile = "default"

  # Multi-line group comment
  # Second line
  account_id = "123456789012"
}"#;
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = r#"provider aws {
  # Comment for region
  region = aws.Region.ap_northeast_1

  # Comment for profile
  profile = 'default'

  # Multi-line group comment
  # Second line
  account_id = '123456789012'
}
"#;
        assert_eq!(formatted, expected);

        // Idempotent
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn test_format_upstream_state_expr() {
        // Test the upstream_state expression is formatted correctly
        let input = "let scope = upstream_state {
state_path = \"../carina-state\"
binding = \"ipam_scope\"
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        let expected = "let scope = upstream_state {\n  state_path = '../carina-state'\n  binding    = 'ipam_scope'\n}\n";
        assert_eq!(formatted, expected);

        // Idempotent
        let formatted2 = format(&formatted, &config).unwrap();
        assert_eq!(formatted, formatted2);
    }

    #[test]
    fn test_format_for_binding_discard_pattern() {
        // Test that `for _, region in regions` is formatted correctly
        let input = "for _, region in regions {
  aws.ec2.Subnet {
    cidr_block = region
  }
}";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        // The `_, region` pattern should be preserved
        assert!(formatted.contains("for _, region in regions"));
        assert!(formatted.contains("aws.ec2.Subnet"));
    }

    #[test]
    fn test_format_upstream_state_and_for_discard_idempotent() {
        // Combined test that both features round-trip
        let input = "let scope = upstream_state {\n  state_path = '../carina-state'\n  binding    = 'ipam_scope'\n}\n\nfor _, region in regions {\n  aws.ec2.Subnet {}\n}\n";
        let config = FormatConfig::default();
        let formatted = format(input, &config).unwrap();
        assert_eq!(formatted, input);
    }
}
