//! Main formatting logic

use super::config::FormatConfig;
use super::cst::{Cst, CstChild, CstNode, NodeKind, Trivia};
use super::cst_builder::build_cst;
use super::parser::{self, FormatParseError};

/// Format a .crn file
pub fn format(source: &str, config: &FormatConfig) -> Result<String, FormatParseError> {
    let pairs = parser::parse(source)?;
    let cst = build_cst(source, pairs);
    let formatter = Formatter::new(config.clone());
    Ok(formatter.format(&cst))
}

/// Check if a file needs formatting
pub fn needs_format(source: &str, config: &FormatConfig) -> Result<bool, FormatParseError> {
    let formatted = format(source, config)?;
    Ok(formatted != source)
}

struct Formatter {
    config: FormatConfig,
    output: String,
    current_indent: usize,
}

impl Formatter {
    fn new(config: FormatConfig) -> Self {
        Self {
            config,
            output: String::new(),
            current_indent: 0,
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
                    Trivia::LineComment(_) => {
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

    fn format_node(&mut self, node: &CstNode) {
        match node.kind {
            NodeKind::ImportStmt => self.format_import_stmt(node),
            NodeKind::BackendBlock => self.format_backend_block(node),
            NodeKind::ProviderBlock => self.format_provider_block(node),
            NodeKind::LetBinding => self.format_let_binding(node),
            NodeKind::ModuleCall => self.format_module_call(node),
            NodeKind::AnonymousResource => self.format_anonymous_resource(node),
            NodeKind::ResourceExpr => self.format_resource_expr(node),
            NodeKind::Attribute => self.format_attribute(node, 0),
            NodeKind::PipeExpr => self.format_pipe_expr(node),
            NodeKind::FunctionCall => self.format_function_call(node),
            NodeKind::EnvVar => self.format_env_var(node),
            NodeKind::VariableRef => self.format_variable_ref(node),
            NodeKind::List => self.format_list(node),
            _ => self.format_default(node),
        }
    }

    fn format_import_stmt(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("import ");

        let mut found_path = false;
        let mut found_as = false;

        for child in &node.children {
            if let CstChild::Token(token) = child {
                if token.text == "import" {
                    continue;
                }
                if token.text.starts_with('"') && !found_path {
                    self.write(&token.text);
                    found_path = true;
                    continue;
                }
                if token.text == "as" {
                    self.write(" as ");
                    found_as = true;
                    continue;
                }
                if found_as && self.is_identifier(&token.text) {
                    self.write(&token.text);
                    break;
                }
            }
        }

        self.write_newline();
    }

    fn format_backend_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("backend ");

        // Find and write backend type (e.g., "s3")
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
                && token.text != "backend"
            {
                self.write(&token.text);
                break;
            }
        }

        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        self.format_block_attributes(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_module_call(&mut self, node: &CstNode) {
        self.write_indent();

        // Find and write module name
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                self.write(&token.text);
                break;
            }
        }

        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        self.format_block_attributes(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_provider_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("provider ");

        // Find and write provider name
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
                && token.text != "provider"
            {
                self.write(&token.text);
                break;
            }
        }

        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        self.format_block_attributes(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_let_binding(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("let ");

        let mut found_name = false;
        let mut found_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "let" {
                        continue;
                    }
                    if token.text == "=" {
                        self.write(" = ");
                        found_equals = true;
                        continue;
                    }
                    if !found_name && self.is_identifier(&token.text) {
                        self.write(&token.text);
                        found_name = true;
                        continue;
                    }
                    if found_equals {
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if found_equals {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write_newline();
    }

    fn format_anonymous_resource(&mut self, node: &CstNode) {
        self.write_indent();

        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write(&token.text);
                break;
            }
        }

        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        self.format_block_attributes(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_resource_expr(&mut self, node: &CstNode) {
        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write(&token.text);
                break;
            }
        }

        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        self.format_block_attributes(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
    }

    fn format_block_attributes(&mut self, node: &CstNode) {
        // Collect attributes and comments
        let mut attributes: Vec<&CstNode> = Vec::new();
        let mut inline_comments: std::collections::HashMap<usize, &Trivia> =
            std::collections::HashMap::new();
        let mut pending_comments: Vec<&Trivia> = Vec::new();

        let mut attr_index = 0;
        for child in &node.children {
            match child {
                CstChild::Node(n) if n.kind == NodeKind::Attribute => {
                    // Write any pending standalone comments
                    for comment in pending_comments.drain(..) {
                        self.write_indent();
                        self.write_trivia(comment);
                        self.write_newline();
                    }
                    attributes.push(n);
                    attr_index += 1;
                }
                CstChild::Trivia(Trivia::LineComment(s)) => {
                    // Check if this is an inline comment (on same line as previous attribute)
                    // For simplicity, we treat comments after a newline as standalone
                    if !attributes.is_empty() && !s.is_empty() {
                        // Store as potential inline comment for previous attribute
                        inline_comments.insert(
                            attr_index - 1,
                            match child {
                                CstChild::Trivia(t) => t,
                                _ => unreachable!(),
                            },
                        );
                    } else {
                        pending_comments.push(match child {
                            CstChild::Trivia(t) => t,
                            _ => unreachable!(),
                        });
                    }
                }
                CstChild::Trivia(Trivia::Newline) => {
                    // Newline means pending comments become standalone
                }
                _ => {}
            }
        }

        // Calculate max key length for alignment
        let max_key_len = if self.config.align_attributes {
            attributes
                .iter()
                .filter_map(|attr| self.get_attribute_key(attr))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        // Format each attribute
        for (i, attr) in attributes.iter().enumerate() {
            let inline_comment = inline_comments.get(&i);
            self.format_attribute_aligned(attr, max_key_len, inline_comment.copied());
        }

        // Write any trailing standalone comments
        for comment in pending_comments {
            self.write_indent();
            self.write_trivia(comment);
            self.write_newline();
        }
    }

    fn get_attribute_key(&self, node: &CstNode) -> Option<String> {
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                return Some(token.text.clone());
            }
        }
        None
    }

    fn format_attribute(&mut self, node: &CstNode, align_to: usize) {
        self.format_attribute_aligned(node, align_to, None);
    }

    fn format_attribute_aligned(
        &mut self,
        node: &CstNode,
        align_to: usize,
        inline_comment: Option<&Trivia>,
    ) {
        self.write_indent();

        let mut key_len: usize;
        let mut wrote_key = false;
        let mut wrote_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_key && self.is_identifier(&token.text) {
                        key_len = token.text.len();
                        self.write(&token.text);
                        wrote_key = true;

                        // Add padding for alignment
                        if align_to > 0 && key_len < align_to {
                            let padding = align_to - key_len;
                            self.write(&" ".repeat(padding));
                        }
                    } else if token.text == "=" && !wrote_equals {
                        self.write(" = ");
                        wrote_equals = true;
                    } else if wrote_equals {
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if wrote_equals {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }

        // Write inline comment if present
        if let Some(comment) = inline_comment {
            self.write("  ");
            self.write_trivia(comment);
        }

        self.write_newline();
    }

    fn format_pipe_expr(&mut self, node: &CstNode) {
        let mut first = true;
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "|>" {
                        self.write(" |> ");
                    } else {
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if !first && n.kind == NodeKind::FunctionCall {
                        self.format_function_call(n);
                    } else {
                        self.format_node(n);
                    }
                    first = false;
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    fn format_function_call(&mut self, node: &CstNode) {
        let mut in_args = false;
        let mut first_arg = true;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "(" {
                        self.write("(");
                        in_args = true;
                    } else if token.text == ")" {
                        self.write(")");
                        in_args = false;
                    } else if token.text == "," {
                        self.write(", ");
                    } else {
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if in_args {
                        if !first_arg {
                            // comma already handled
                        }
                        self.format_node(n);
                        first_arg = false;
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    fn format_env_var(&mut self, node: &CstNode) {
        self.write("env(");
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.starts_with('"')
            {
                self.write(&token.text);
                break;
            }
        }
        self.write(")");
    }

    fn format_variable_ref(&mut self, node: &CstNode) {
        let mut parts: Vec<&str> = Vec::new();
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                parts.push(&token.text);
            }
        }
        self.write(&parts.join("."));
    }

    fn format_list(&mut self, node: &CstNode) {
        self.write("[");
        let mut first = true;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "[" || token.text == "]" {
                        continue;
                    }
                    if token.text == "," {
                        continue;
                    }
                    // String or other literal
                    if !first {
                        self.write(", ");
                    }
                    self.write(&token.text);
                    first = false;
                }
                CstChild::Node(n) => {
                    if !first {
                        self.write(", ");
                    }
                    self.format_node(n);
                    first = false;
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write("]");
    }

    fn format_default(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    self.write(&token.text);
                }
                CstChild::Node(n) => {
                    self.format_node(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    // Helper methods

    fn write(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn write_indent(&mut self) {
        let indent = self.config.indent_string().repeat(self.current_indent);
        self.output.push_str(&indent);
    }

    fn write_newline(&mut self) {
        self.output.push('\n');
    }

    fn write_newlines(&mut self, count: usize) {
        for _ in 0..count {
            self.write_newline();
        }
    }

    fn write_trivia(&mut self, trivia: &Trivia) {
        match trivia {
            Trivia::LineComment(s) => self.write(s),
            Trivia::Newline => self.write_newline(),
            Trivia::Whitespace(s) => self.write(s),
        }
    }

    fn is_identifier(&self, s: &str) -> bool {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
        let input = "aws.s3.bucket {\n    name = \"test\"\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("  name = \"test\""));
    }

    #[test]
    fn test_format_aligns_attributes() {
        let input = "aws.s3.bucket {\nname = \"test\"\nversioning = true\n}";
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
        let input = "let bucket=aws.s3.bucket {\nname=\"test\"\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert!(result.contains("let bucket = aws.s3.bucket {"));
    }

    #[test]
    fn test_needs_format() {
        let config = FormatConfig::default();

        let formatted = "provider aws {\n  region = aws.Region.ap_northeast_1\n}\n";
        assert!(!needs_format(formatted, &config).unwrap());

        let unformatted = "provider aws {\nregion=aws.Region.ap_northeast_1\n}";
        assert!(needs_format(unformatted, &config).unwrap());
    }
}
