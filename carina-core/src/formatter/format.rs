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
            NodeKind::SubscriptedId => self.format_subscripted_id(node),
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
        let result = format(input, &config).unwrap();

        assert!(result.contains("provider aws {"));
        assert!(result.contains("  region = aws.Region.ap_northeast_1"));
    }

    #[test]
    fn test_format_preserves_comments() {
        let input = "# Header comment\nprovider aws {}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("# Header comment"));
    }

    #[test]
    fn test_format_normalizes_indentation() {
        let input = "aws.s3.Bucket {\n    name = \"test\"\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("  name = 'test'"));
    }

    #[test]
    fn test_format_struct_type_canonical_spacing() {
        let input = "attributes {\n  config: struct{a: Int,b: String} = { a = 1, b = 'x' }\n}\n";
        let result = format(input, &FormatConfig::default()).unwrap();
        assert!(
            result.contains("struct { a: Int, b: String }"),
            "expected canonical struct spacing, got:\n{result}"
        );
    }

    #[test]
    fn test_format_struct_type_empty() {
        let input = "attributes {\n  x: struct{} = {}\n}\n";
        let result = format(input, &FormatConfig::default()).unwrap();
        assert!(
            result.contains("struct {}"),
            "expected `struct {{}}`, got:\n{result}"
        );
    }

    #[test]
    fn test_format_struct_type_nested_in_list_and_map() {
        let input =
            "attributes {\n  xs: list(struct{a: Int}) = []\n  m: map(struct{b: String}) = {}\n}\n";
        let result = format(input, &FormatConfig::default()).unwrap();
        assert!(
            result.contains("list(struct { a: Int })"),
            "expected list(struct {{ a: Int }}), got:\n{result}"
        );
        assert!(
            result.contains("map(struct { b: String })"),
            "expected map(struct {{ b: String }}), got:\n{result}"
        );
    }

    #[test]
    fn test_format_struct_type_field_is_itself_struct() {
        let input =
            "attributes {\n  outer: struct{inner:struct{x: Int}} = { inner = { x = 1 } }\n}\n";
        let result = format(input, &FormatConfig::default()).unwrap();
        assert!(
            result.contains("struct { inner: struct { x: Int } }"),
            "expected nested struct spacing, got:\n{result}"
        );
    }

    #[test]
    fn test_format_aligns_attributes() {
        let input = "aws.s3.Bucket {\nname = \"test\"\nversioning = true\n}";
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        // Both "=" should be at the same column
        let lines: Vec<&str> = result.lines().collect();
        let name_eq_pos = lines.iter().find(|l| l.contains("name")).unwrap().find('=');
        let vers_eq_pos = lines
            .iter()
            .find(|l| l.contains("versioning"))
            .unwrap()
            .find('=');

        assert_eq!(name_eq_pos, vers_eq_pos);
    }

    #[test]
    fn test_format_idempotent() {
        let input = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        let config = FormatConfig::default();

        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();

        assert_eq!(first, second, "Formatting should be idempotent");
    }

    #[test]
    fn test_format_let_binding() {
        let input = "let bucket=aws.s3.Bucket {\nname=\"test\"\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("let bucket = aws.s3.Bucket {"));
    }

    #[test]
    fn test_needs_format() {
        let config = FormatConfig::default();

        let formatted = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        assert!(!needs_format(formatted, &config).unwrap());

        let unformatted = "provider aws {\nregion=aws.Region.ap_northeast_1\n}";
        assert!(needs_format(unformatted, &config).unwrap());
    }

    #[test]
    fn test_format_map() {
        let input = "awscc.ec2.Vpc {\ntags = {Environment=\"dev\"Project=\"test\"}\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        // Map should be formatted with entries on separate lines
        assert!(result.contains("tags = {"), "missing 'tags = {{'");
        assert!(
            result.contains("Environment = 'dev'"),
            "missing Environment"
        );
        // With alignment, Project has extra spaces to align with Environment
        assert!(
            result.contains("Project") && result.contains("= 'test'"),
            "missing Project"
        );
        // Map entries should be on separate lines (not all on one line)
        let lines: Vec<&str> = result.lines().collect();
        assert!(
            lines.iter().any(|l| l.contains("Environment")),
            "Environment should be on its own line"
        );
        assert!(
            lines.iter().any(|l| l.contains("Project")),
            "Project should be on its own line"
        );
    }

    #[test]
    fn test_format_map_aligns_entries() {
        let input = "awscc.ec2.Vpc {\ntags = {Environment=\"dev\"\nProject=\"test\"}\n}";
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        // Map entries should be aligned
        let lines: Vec<&str> = result.lines().collect();
        let env_eq_pos = lines
            .iter()
            .find(|l| l.contains("Environment"))
            .unwrap()
            .find('=');
        let proj_eq_pos = lines
            .iter()
            .find(|l| l.contains("Project"))
            .unwrap()
            .find('=');

        assert_eq!(env_eq_pos, proj_eq_pos);
    }

    #[test]
    fn test_format_map_idempotent() {
        let input = "awscc.ec2.Vpc {\n  tags = {\n    Environment = \"dev\"\n    Project = \"test\"\n  }\n}\n";
        let config = FormatConfig::default();

        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();

        assert_eq!(first, second, "Map formatting should be idempotent");
    }

    #[test]
    fn test_format_preserves_blank_lines_between_attributes() {
        let input = "awscc.ec2.Vpc {\n  name = \"test\"\n  cidr = \"10.0.0.0/16\"\n\n  tags = {\n    Env = \"dev\"\n  }\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        // Should preserve blank line before tags
        assert!(result.contains("cidr"), "should have cidr");
        assert!(result.contains("tags"), "should have tags");

        // Check that there's a blank line between cidr and tags
        let lines: Vec<&str> = result.lines().collect();
        let cidr_line = lines.iter().position(|l| l.contains("cidr")).unwrap();
        let tags_line = lines.iter().position(|l| l.contains("tags")).unwrap();

        // There should be an empty line between them (difference should be > 1)
        assert!(
            tags_line - cidr_line > 1,
            "Expected blank line between cidr and tags, but cidr is at line {} and tags at line {}",
            cidr_line,
            tags_line
        );
    }

    #[test]
    fn test_format_blank_lines_idempotent() {
        let input = "awscc.ec2.Vpc {\n  name = \"test\"\n\n  tags = {\n    Env = \"dev\"\n  }\n}\n";
        let config = FormatConfig::default();

        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();

        assert_eq!(first, second, "Blank line formatting should be idempotent");
    }

    #[test]
    fn test_format_aligns_within_groups_separated_by_blank_lines() {
        // Attributes before blank line should be aligned together
        // Attributes after blank line should be aligned separately
        let input =
            "awscc.ec2.Vpc {\nenable_dns_hostnames = true\nname = \"test\"\n\ntags = {}\n}\n";
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        let lines: Vec<&str> = result.lines().collect();

        // Find the = positions for each attribute
        let dns_line = lines
            .iter()
            .find(|l| l.contains("enable_dns_hostnames"))
            .unwrap();
        let name_line = lines.iter().find(|l| l.contains("name")).unwrap();
        let tags_line = lines.iter().find(|l| l.contains("tags")).unwrap();

        let dns_eq_pos = dns_line.find('=').unwrap();
        let name_eq_pos = name_line.find('=').unwrap();
        let tags_eq_pos = tags_line.find('=').unwrap();

        // dns and name should be aligned (same group)
        assert_eq!(dns_eq_pos, name_eq_pos, "dns and name should be aligned");

        // tags should NOT be aligned with dns/name (different group)
        assert_ne!(
            tags_eq_pos, dns_eq_pos,
            "tags should not be aligned with dns/name"
        );

        // tags should have minimal padding (just "tags = ")
        assert!(
            tags_line.trim().starts_with("tags ="),
            "tags should have minimal padding"
        );
    }

    #[test]
    fn test_format_empty_anonymous_resource_block() {
        // Empty anonymous resource block should be formatted on a single line
        let input = "awscc.ec2.internet_gateway {\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert_eq!(
            result, "awscc.ec2.internet_gateway {}\n",
            "Empty anonymous resource block should be on a single line, got: {:?}",
            result
        );
    }

    #[test]
    fn test_format_empty_let_binding_resource_block() {
        // Empty let binding resource block should be formatted on a single line
        let input = "let igw = awscc.ec2.internet_gateway {\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert_eq!(
            result, "let igw = awscc.ec2.internet_gateway {}\n",
            "Empty let binding resource block should be on a single line, got: {:?}",
            result
        );
    }

    #[test]
    fn test_format_nonempty_block_remains_multiline() {
        // Non-empty blocks should remain multi-line
        let input = "awscc.ec2.Vpc {\n  cidr_block = \"10.0.0.0/16\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert_eq!(
            result, "awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n}\n",
            "Non-empty block should remain multi-line, got: {:?}",
            result
        );
    }

    #[test]
    fn test_format_empty_block_idempotent() {
        // Formatting an already-formatted empty block should be idempotent
        let input = "let igw = awscc.ec2.internet_gateway {}\n";
        let config = FormatConfig::default();

        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();

        assert_eq!(first, second, "Empty block formatting should be idempotent");
        assert_eq!(
            first, "let igw = awscc.ec2.internet_gateway {}\n",
            "Empty block should stay on a single line"
        );
    }

    #[test]
    fn test_format_nested_block() {
        let input = r#"awscc.ec2.SecurityGroup {
  vpc_id = "vpc-123"

  security_group_ingress {
    ip_protocol = "tcp"
    from_port   = 80
    to_port     = 80
  }
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("security_group_ingress {"));
        assert!(result.contains("    ip_protocol = 'tcp'"));
        assert!(result.contains("    from_port"));
        assert!(result.contains("    to_port"));

        // Idempotency
        let second = format(&result, &config).unwrap();
        assert_eq!(
            result, second,
            "Nested block formatting should be idempotent"
        );
    }

    #[test]
    fn test_format_nested_block_in_map() {
        let input = r#"awscc.iam.role {
  assume_role_policy_document = {
    version = "2012-10-17"
    statement {
      effect = "Allow"
      action = "sts:AssumeRole"
    }
  }
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("statement {"));
        assert!(result.contains("effect = 'Allow'"));

        // Idempotency
        let second = format(&result, &config).unwrap();
        assert_eq!(
            result, second,
            "Nested block in map formatting should be idempotent"
        );
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_simple() {
        // Issue #908: `attr = [{...}]` should be converted to `attr { ... }` block syntax
        // when the attribute is known to use block syntax (via block_name mapping).
        let input = r#"awscc.ec2.ipam {
  operating_regions = [{
    region_name = "ap-northeast-1"
  }]
}
"#;
        let expected =
            "awscc.ec2.ipam {\n  operating_region {\n    region_name = 'ap-northeast-1'\n  }\n}\n";
        let config = FormatConfig::default();
        // block_names maps attribute name -> block name for conversion
        let block_names: std::collections::HashMap<String, String> = [(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        )]
        .into_iter()
        .collect();
        let result = format_with_block_names(input, &config, &block_names).unwrap();

        assert_eq!(
            result, expected,
            "List literal `= [{{...}}]` should be converted to block syntax.\nGot:\n{}",
            result
        );
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_multiple_items() {
        // Multiple items in `= [{...}, {...}]` should become multiple blocks
        let input = r#"awscc.s3.Bucket {
  lifecycle_configuration = {
    rules = [{
      id     = "expire-old-objects"
      status = "Enabled"
    }, {
      id     = "transition-to-glacier"
      status = "Enabled"
    }]
  }
}
"#;
        let expected = "awscc.s3.Bucket {\n  lifecycle_configuration = {\n    rule {\n      id     = 'expire-old-objects'\n      status = 'Enabled'\n    }\n    rule {\n      id     = 'transition-to-glacier'\n      status = 'Enabled'\n    }\n  }\n}\n";
        let config = FormatConfig::default();
        let block_names: std::collections::HashMap<String, String> =
            [("rules".to_string(), "rule".to_string())]
                .into_iter()
                .collect();
        let result = format_with_block_names(input, &config, &block_names).unwrap();

        assert_eq!(
            result, expected,
            "Multiple list items should become multiple blocks.\nGot:\n{}",
            result
        );
    }

    #[test]
    fn test_convert_list_literal_to_block_syntax_nested() {
        // Nested `= [{...}]` within a map should also be converted
        let input = r#"awscc.s3.Bucket {
  bucket_encryption = {
    server_side_encryption_configuration = [{
      bucket_key_enabled                = true
      server_side_encryption_by_default = {
        sse_algorithm = "AES256"
      }
    }]
  }
}
"#;
        let expected = "awscc.s3.Bucket {\n  bucket_encryption = {\n    server_side_encryption_configuration {\n      bucket_key_enabled                = true\n      server_side_encryption_by_default = {\n        sse_algorithm = 'AES256'\n      }\n    }\n  }\n}\n";
        let config = FormatConfig::default();
        let block_names: std::collections::HashMap<String, String> = [(
            "server_side_encryption_configuration".to_string(),
            "server_side_encryption_configuration".to_string(),
        )]
        .into_iter()
        .collect();
        let result = format_with_block_names(input, &config, &block_names).unwrap();

        assert_eq!(
            result, expected,
            "Nested list literal should be converted to block syntax.\nGot:\n{}",
            result
        );
    }

    #[test]
    fn test_convert_block_syntax_is_idempotent() {
        // Already in block syntax should remain unchanged
        let input =
            "awscc.ec2.ipam {\n  operating_region {\n    region_name = 'ap-northeast-1'\n  }\n}\n";
        let config = FormatConfig::default();
        let block_names: std::collections::HashMap<String, String> = [(
            "operating_regions".to_string(),
            "operating_region".to_string(),
        )]
        .into_iter()
        .collect();
        let result = format_with_block_names(input, &config, &block_names).unwrap();

        assert_eq!(
            result, input,
            "Already-converted block syntax should be idempotent.\nGot:\n{}",
            result
        );
    }

    #[test]
    fn test_format_attributes_without_type() {
        let input = "attributes {\nsecurity_group = sg.id\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(
            result.contains("security_group = sg.id"),
            "Expected 'security_group = sg.id' in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_attributes_mixed_typed_and_untyped() {
        let input = "attributes {\nvpc_id: awscc.ec2.VpcId = vpc.vpc_id\nsecurity_group = sg.id\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        // Typed form (may have alignment padding)
        assert!(
            result.contains("vpc_id") && result.contains("awscc.ec2.VpcId = vpc.vpc_id"),
            "Expected typed form in:\n{}",
            result
        );
        assert!(
            result.contains("security_group = sg.id"),
            "Expected untyped form in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_index_access() {
        let input = "let x = items[0].name\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("items[0].name"),
            "Expected index access in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_string_index_access() {
        let input = "let x = config[\"key\"].value\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("config['key'].value"),
            "Expected string index access in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_for_expression() {
        let input = "let subnets = for subnet in subnets {\n  awscc.ec2.Subnet {\n    cidr_block = subnet.cidr\n  }\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("for subnet in subnets"),
            "Expected for expression in:\n{}",
            result
        );
        assert!(
            result.contains("awscc.ec2.Subnet"),
            "Expected resource in for body:\n{}",
            result
        );
    }

    #[test]
    fn test_format_read_resource_expr() {
        let input = "let vpc = read awscc.ec2.Vpc {\n  vpc_id = \"vpc-123\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("read awscc.ec2.Vpc"),
            "Expected read resource expr in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_function_call_in_primary() {
        let input = "let x = concat(a, b)\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("concat(a, b)"),
            "Expected function call in:\n{}",
            result
        );
    }

    #[test]
    fn issue_1177_blank_lines_around_map_attributes() {
        // Map block attributes should have blank lines before and after,
        // and alignment should reset at blank line boundaries.
        let input = r#"awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
  tags = {
    Name        = "test"
    Environment = "dev"
  }
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        let expected = "awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n\n  tags = {\n    Name        = 'test'\n    Environment = 'dev'\n  }\n}\n";
        assert_eq!(result, expected, "Expected blank line before map attribute");
    }

    #[test]
    fn issue_1177_blank_lines_around_map_alignment_reset() {
        // Alignment should reset across blank line boundaries,
        // so `tags` should NOT be padded to match `cidr_block`.
        let input = r#"awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
  tags       = {
    Name = "test"
  }
  enable_dns = true
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        // tags should be in its own group (no padding)
        // enable_dns should be in its own group (no padding)
        let expected = "awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n\n  tags = {\n    Name = 'test'\n  }\n\n  enable_dns = true\n}\n";
        assert_eq!(
            result, expected,
            "Alignment should reset at blank line boundaries"
        );
    }

    #[test]
    fn issue_1177_map_first_attribute_no_leading_blank_line() {
        // If map attribute is the first attribute, no leading blank line
        let input = r#"awscc.ec2.Vpc {
  tags = {
    Name = "test"
  }
  cidr_block = "10.0.0.0/16"
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        let expected = "awscc.ec2.Vpc {\n  tags = {\n    Name = 'test'\n  }\n\n  cidr_block = '10.0.0.0/16'\n}\n";
        assert_eq!(
            result, expected,
            "No leading blank line when map is first attribute"
        );
    }

    #[test]
    fn issue_1177_map_last_attribute_no_trailing_blank_line() {
        // If map attribute is the last attribute, no trailing blank line
        let input = r#"awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
  tags = {
    Name = "test"
  }
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        let expected = "awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n\n  tags = {\n    Name = 'test'\n  }\n}\n";
        assert_eq!(
            result, expected,
            "No trailing blank line when map is last attribute"
        );
    }

    #[test]
    fn issue_1177_empty_map_no_blank_lines() {
        // Empty maps should NOT trigger blank line insertion
        let input = r#"awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
  tags       = {}
  enable_dns = true
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let result = format(input, &config).unwrap();

        let expected = "awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n  tags       = {}\n  enable_dns = true\n}\n";
        assert_eq!(result, expected, "Empty maps should not get blank lines");
    }

    #[test]
    fn issue_1177_idempotent() {
        // Formatting should be idempotent
        let input = r#"awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
  tags = {
    Name        = "test"
    Environment = "dev"
  }
  enable_dns = true
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();
        assert_eq!(first, second, "Formatting should be idempotent");
    }

    #[test]
    fn format_arguments_param_block_form() {
        let input = r#"arguments {
  vpc: awscc.ec2.Vpc {
    description = "The VPC to deploy into"
  }
  port: Int {
    description = "Web server port"
    default     = 8080
  }
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        let expected = "arguments {\n  vpc: awscc.ec2.Vpc {\n    description = 'The VPC to deploy into'\n  }\n  port: Int {\n    description = 'Web server port'\n    default     = 8080\n  }\n}\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn format_arguments_mixed_simple_and_block_form() {
        let input = r#"arguments {
  enable_https: Bool = true
  vpc: awscc.ec2.Vpc {
    description = "The VPC to deploy into"
  }
  port: Int {
    description = "Web server port"
    default     = 8080
  }
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        let expected = "arguments {\n  enable_https: Bool = true\n  vpc: awscc.ec2.Vpc {\n    description = 'The VPC to deploy into'\n  }\n  port: Int {\n    description = 'Web server port'\n    default     = 8080\n  }\n}\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn format_arguments_block_form_idempotent() {
        let input = r#"arguments {
  vpc: awscc.ec2.Vpc {
    description = "The VPC"
  }
  port: Int {
    description = "Port"
    default     = 8080
  }
}
"#;
        let config = FormatConfig::default();
        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();
        assert_eq!(first, second, "Formatting should be idempotent");
    }

    #[test]
    fn format_arguments_mixed_with_alignment() {
        let input = r#"arguments {
  short: Bool = true
  longer_name: String = "hello"
  vpc: awscc.ec2.Vpc {
    description = "The VPC"
  }
}
"#;
        let config = FormatConfig {
            align_attributes: true,
            ..Default::default()
        };
        let expected = "arguments {\n  short      : Bool = true\n  longer_name: String = 'hello'\n  vpc: awscc.ec2.Vpc {\n    description = 'The VPC'\n  }\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    #[ignore] // TODO: formatter doesn't handle validation { ... } block yet
    fn format_arguments_block_form_with_validation_block() {
        let input = r#"arguments {
  port: Int {
    description = "Web server port"
    default     = 8080
    validation {
      condition     = port >= 1 && port <= 65535
      error_message = "Port must be between 1 and 65535"
    }
  }
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn format_fn_def_simple() {
        let config = FormatConfig::default();
        let input = "fn greet(name) {\n  join(\" \", [\"hello\",name])\n}\n";
        let expected = "fn greet(name) {\n  join(' ', ['hello', name])\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_default_param() {
        let config = FormatConfig::default();
        let input = "fn tag(env,suffix=\"default\") {\n  join(\"-\", [env, suffix])\n}\n";
        let expected = "fn tag(env, suffix = 'default') {\n  join('-', [env, suffix])\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_local_let() {
        let config = FormatConfig::default();
        let input = "fn name(env,az) {\n  let prefix=join(\"-\",[env,\"subnet\"])\n  join(\"-\",[prefix,az])\n}\n";
        let expected = "fn name(env, az) {\n  let prefix = join('-', [env, 'subnet'])\n  join('-', [prefix, az])\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_typed_params() {
        let config = FormatConfig::default();
        let input = "fn greet(name:String) {\n  name\n}\n";
        let expected = "fn greet(name: String) {\n  name\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_typed_param_and_default() {
        let config = FormatConfig::default();
        let input =
            "fn tag(env:String,suffix:String=\"default\") {\n  join(\"-\", [env, suffix])\n}\n";
        let expected =
            "fn tag(env: String, suffix: String = 'default') {\n  join('-', [env, suffix])\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_resource_type_param() {
        let config = FormatConfig::default();
        let input = "fn make(vpc:awscc.ec2.Vpc,cidr:String) {\n  vpc\n}\n";
        let expected = "fn make(vpc: awscc.ec2.Vpc, cidr: String) {\n  vpc\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_mixed_typed_untyped() {
        let config = FormatConfig::default();
        let input = "fn tag(env,suffix:String) {\n  suffix\n}\n";
        let expected = "fn tag(env, suffix: String) {\n  suffix\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_return_type() {
        let config = FormatConfig::default();
        let input = "fn greet(name:String):String {\n  name\n}\n";
        let expected = "fn greet(name: String): String {\n  name\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_with_resource_return_type() {
        let config = FormatConfig::default();
        let input = "fn make():awscc.ec2.Vpc {\n  awscc.ec2.Vpc {\n    cidr_block = \"10.0.0.0/16\"\n  }\n}\n";
        let expected = "fn make(): awscc.ec2.Vpc {\n  awscc.ec2.Vpc {\n    cidr_block = '10.0.0.0/16'\n  }\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_fn_def_without_return_type_unchanged() {
        let config = FormatConfig::default();
        let input = "fn greet(name) {\n  name\n}\n";
        let expected = "fn greet(name) {\n  name\n}\n";
        let result = format(input, &config).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn format_custom_schema_type_annotations() {
        let config = FormatConfig::default();

        // Custom type in arguments block — PascalCase per Strategy Y.
        let input = "arguments {\nvpc_cidr: Cidr\nserver_ip: Ipv4Address\n}\n";
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("vpc_cidr") && result.contains("Cidr"),
            "Expected 'vpc_cidr' and 'Cidr' in:\n{}",
            result
        );
        assert!(
            result.contains("server_ip") && result.contains("Ipv4Address"),
            "Expected 'server_ip' and 'Ipv4Address' in:\n{}",
            result
        );

        // Custom type in fn param
        let input = "fn f(addr: Arn) {\n  addr\n}\n";
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("addr: Arn"),
            "Expected 'addr: Arn' in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_require_statement() {
        let input = r#"arguments {
  port: Int
}
require   port >= 1 && port <= 65535  , "port must be valid"
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        // The formatter normalizes spacing around "require" keyword and comma,
        // but preserves validate expression content as-is (opaque)
        assert!(
            result.contains("require port >= 1 && port <= 65535, 'port must be valid'"),
            "Unexpected output:\n{}",
            result
        );
    }

    #[test]
    fn format_heredoc_preserved() {
        let input = "aws.iam.Role {\n  name   = \"my-role\"\n  policy = <<EOT\n{\n  \"Version\": \"2012-10-17\"\n}\nEOT\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        // name should be normalized to single quotes
        assert!(
            result.contains("'my-role'"),
            "name should be normalized to single quotes. Got:\n{}",
            result
        );
        // Heredoc should be preserved in output
        assert!(
            result.contains("<<EOT"),
            "Heredoc marker should be preserved. Got:\n{}",
            result
        );
        assert!(
            result.contains("EOT\n"),
            "Closing marker should be preserved. Got:\n{}",
            result
        );
    }

    #[test]
    fn format_heredoc_idempotent() {
        // Formatting a file with heredoc should be idempotent (formatting twice gives same result)
        let input = "aws.iam.Role {\n  name   = \"my-role\"\n  policy = <<EOT\n{\n  \"Version\": \"2012-10-17\"\n}\nEOT\n}\n";
        let config = FormatConfig::default();
        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();
        assert_eq!(first, second, "Formatting should be idempotent");
    }

    #[test]
    fn test_format_normalizes_double_to_single_quotes() {
        let input = "aws.s3.Bucket {\n  name = \"my-bucket\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("name = 'my-bucket'"),
            "Double-quoted literal should be normalized to single quotes. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_preserves_double_quotes_for_interpolation() {
        let input = "aws.s3.Bucket {\n  name = \"vpc-${env}\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("name = \"vpc-${env}\""),
            "Interpolated string should keep double quotes. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_preserves_single_quotes() {
        let input = "aws.s3.Bucket {\n  name = 'my-bucket'\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("name = 'my-bucket'"),
            "Single-quoted string should be preserved. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_normalizes_quotes_in_list() {
        let input = "aws.s3.Bucket {\n  tags = [\"a\", \"b\"]\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("'a'") && result.contains("'b'"),
            "Double-quoted literals in lists should be normalized. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_preserves_double_quotes_with_single_quote_char() {
        let input = "aws.s3.Bucket {\n  name = \"it's\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("name = \"it's\""),
            "String containing single quote should keep double quotes. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_normalizes_use_source_quotes() {
        let input = "let m = use { source = \"./modules/web\" }\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("source = './modules/web'"),
            "`source` path should be normalized to single quotes. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_quote_normalization_idempotent() {
        let input = "aws.s3.Bucket {\n  name = \"my-bucket\"\n  tag  = \"vpc-${env}\"\n}\n";
        let config = FormatConfig::default();
        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();
        assert_eq!(first, second, "Quote normalization should be idempotent");
    }

    #[test]
    fn test_format_quoted_map_keys() {
        let input =
            "let m = {\n  'token.actions.githubusercontent.com:aud' = 'sts.amazonaws.com'\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("'token.actions.githubusercontent.com:aud'"),
            "Quoted map key should be preserved. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_quoted_attribute_key_in_block() {
        let input = "aws.iam.Role {\n  name = 'test'\n  'aws:condition' = 'value'\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("'aws:condition'"),
            "Quoted attribute key should be preserved. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_format_comment_stays_above_attribute() {
        let input = r#"let x = awscc.iam.oidc_provider {
  url            = 'https://example.com'
  client_id_list = ['sts.amazonaws.com']
  # AWS requires this field but does not validate it
  thumbprint_list = ['ffffffffffffffffffffffffffffffffffffffff']
}
"#;
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        // The comment should appear directly before thumbprint_list, not before url
        let lines: Vec<&str> = result.lines().collect();
        let comment_line = lines
            .iter()
            .position(|l| l.contains("# AWS requires"))
            .expect("Comment should exist in output");
        let thumbprint_line = lines
            .iter()
            .position(|l| l.contains("thumbprint_list"))
            .expect("thumbprint_list should exist in output");
        assert_eq!(
            comment_line + 1,
            thumbprint_line,
            "Comment should be directly above thumbprint_list. Got:\n{}",
            result
        );
    }

    /// Regression for #2117: the formatter grammar missed `upstream_state`
    /// entirely, so `let orgs = upstream_state { source = '..' }` couldn't
    /// be formatted.
    #[test]
    fn test_format_upstream_state_expr() {
        let input = "let orgs = upstream_state {\n    source = '../organizations'\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("upstream_state"),
            "upstream_state keyword must be preserved. Got:\n{}",
            result
        );
        assert!(
            result.contains("source = '../organizations'"),
            "source attribute must be preserved. Got:\n{}",
            result
        );
    }

    /// Regression for #2117: `for _, v in map { ... }` uses the discard
    /// pattern `_`, which the formatter's `for_*_binding` productions did
    /// not accept.
    #[test]
    fn test_format_for_binding_discard_pattern() {
        let input =
            "for _, account_id in orgs {\n  aws.s3.Bucket {\n    name = account_id\n  }\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("for _, account_id in orgs"),
            "discard pattern `_` must survive formatting. Got:\n{}",
            result
        );
    }

    /// Idempotence for both constructs above.
    #[test]
    fn test_format_upstream_state_and_for_discard_idempotent() {
        let input = "let orgs = upstream_state {\n  source = '../organizations'\n}\n\nfor _, account_id in orgs {\n  aws.s3.Bucket {\n    name = account_id\n  }\n}\n";
        let config = FormatConfig::default();
        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();
        assert_eq!(
            first, second,
            "Formatting must be idempotent.\nfirst:\n{}\nsecond:\n{}",
            first, second
        );
    }
}
