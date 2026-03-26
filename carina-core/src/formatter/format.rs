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

/// Format a .crn file, converting `= [{...}]` to block syntax for attributes
/// listed in `block_names`. The map key is the attribute name (e.g., "operating_regions")
/// and the value is the block name to use (e.g., "operating_region").
pub fn format_with_block_names(
    source: &str,
    config: &FormatConfig,
    block_names: &std::collections::HashMap<String, String>,
) -> Result<String, FormatParseError> {
    let pairs = parser::parse(source)?;
    let cst = build_cst(source, pairs);
    let formatter = Formatter::with_block_names(config.clone(), block_names.clone());
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
    block_names: std::collections::HashMap<String, String>,
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
            NodeKind::ImportExpr => self.format_import_expr(node),
            NodeKind::BackendBlock => self.format_backend_block(node),
            NodeKind::ProviderBlock => self.format_provider_block(node),
            NodeKind::ArgumentsBlock => self.format_arguments_block(node),
            NodeKind::AttributesBlock => self.format_attributes_block(node),
            NodeKind::LetBinding => self.format_let_binding(node),
            NodeKind::LocalBinding => self.format_let_binding(node),
            NodeKind::ModuleCall => self.format_module_call(node),
            NodeKind::AnonymousResource => self.format_anonymous_resource(node),
            NodeKind::ResourceExpr => self.format_resource_expr(node),
            NodeKind::ReadResourceExpr => self.format_read_resource_expr(node),
            NodeKind::ForExpr => self.format_for_expr(node),
            NodeKind::Attribute => self.format_attribute(node, 0),
            NodeKind::NestedBlock => self.format_nested_block(node),
            NodeKind::ArgumentsParam => self.format_arguments_param(node, 0),
            NodeKind::AttributesParam => self.format_attributes_param(node, 0),
            NodeKind::PipeExpr => self.format_pipe_expr(node),
            NodeKind::FunctionCall => self.format_function_call(node),
            NodeKind::VariableRef => self.format_variable_ref(node),
            NodeKind::List => self.format_list(node),
            NodeKind::Map => self.format_map(node),
            NodeKind::MapEntry => self.format_map_entry(node),
            NodeKind::TypeExpr => self.format_type_expr(node),
            _ => self.format_default(node),
        }
    }

    fn format_import_expr(&mut self, node: &CstNode) {
        self.write("import ");

        for child in &node.children {
            if let CstChild::Token(token) = child {
                if token.text == "import" {
                    continue;
                }
                if token.text.starts_with('"') {
                    self.write(&token.text);
                    break;
                }
            }
        }
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

    fn format_arguments_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("arguments {");
        self.write_newline();
        self.current_indent += 1;

        self.format_arguments_params(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_attributes_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("attributes {");
        self.write_newline();
        self.current_indent += 1;

        self.format_attributes_params(node);

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    fn format_arguments_params(&mut self, node: &CstNode) {
        // Collect arguments params
        let params: Vec<&CstNode> = node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child
                    && n.kind == NodeKind::ArgumentsParam
                {
                    return Some(n);
                }
                None
            })
            .collect();

        // Calculate max key length for alignment
        let max_key_len = if self.config.align_attributes {
            params
                .iter()
                .filter_map(|p| self.get_param_name(p))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        for param in params {
            self.format_arguments_param(param, max_key_len);
        }
    }

    fn format_attributes_params(&mut self, node: &CstNode) {
        // Collect attributes params
        let params: Vec<&CstNode> = node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child
                    && n.kind == NodeKind::AttributesParam
                {
                    return Some(n);
                }
                None
            })
            .collect();

        // Calculate max key length for alignment
        let max_key_len = if self.config.align_attributes {
            params
                .iter()
                .filter_map(|p| self.get_param_name(p))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        for param in params {
            self.format_attributes_param(param, max_key_len);
        }
    }

    fn get_param_name(&self, node: &CstNode) -> Option<String> {
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                return Some(token.text.clone());
            }
        }
        None
    }

    fn format_arguments_param(&mut self, node: &CstNode, align_to: usize) {
        self.write_indent();

        let mut key_len: usize = 0;
        let mut wrote_name = false;
        let mut wrote_colon = false;
        let mut wrote_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_name && self.is_identifier(&token.text) && token.text != "arguments" {
                        key_len = token.text.len();
                        self.write(&token.text);
                        wrote_name = true;
                    } else if token.text == ":" && !wrote_colon {
                        // Add padding for alignment before colon
                        if align_to > 0 && key_len < align_to {
                            let padding = align_to - key_len;
                            self.write(&" ".repeat(padding));
                        }
                        self.write(": ");
                        wrote_colon = true;
                    } else if token.text == "=" && !wrote_equals {
                        self.write(" = ");
                        wrote_equals = true;
                    } else if wrote_colon && !wrote_equals {
                        // Type primitive
                        self.write(&token.text);
                    } else if wrote_equals {
                        // Default value
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if wrote_colon && !wrote_equals {
                        self.format_type_expr(n);
                    } else if wrote_equals {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write_newline();
    }

    fn format_attributes_param(&mut self, node: &CstNode, align_to: usize) {
        self.write_indent();

        let mut key_len: usize = 0;
        let mut wrote_name = false;
        let mut wrote_colon = false;
        let mut wrote_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_name && self.is_identifier(&token.text) && token.text != "attributes"
                    {
                        key_len = token.text.len();
                        self.write(&token.text);
                        wrote_name = true;
                    } else if token.text == ":" && !wrote_colon {
                        // Add padding for alignment before colon
                        if align_to > 0 && key_len < align_to {
                            let padding = align_to - key_len;
                            self.write(&" ".repeat(padding));
                        }
                        self.write(": ");
                        wrote_colon = true;
                    } else if token.text == "=" && !wrote_equals {
                        self.write(" = ");
                        wrote_equals = true;
                    } else if wrote_colon && !wrote_equals {
                        // Type primitive
                        self.write(&token.text);
                    } else if wrote_equals {
                        // Value
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if wrote_colon && !wrote_equals {
                        self.format_type_expr(n);
                    } else if wrote_equals {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write_newline();
    }

    fn format_type_expr(&mut self, node: &CstNode) {
        // Type expressions: aws.vpc, list(cidr), map(string), string, bool, int, cidr
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "(" {
                        self.write("(");
                    } else if token.text == ")" {
                        self.write(")");
                    } else {
                        self.write(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    // Recursively format nested type expressions
                    self.format_type_expr(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
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

        if self.block_has_content(node) {
            self.write(" {");
            self.write_newline();
            self.current_indent += 1;

            self.format_block_attributes(node);

            self.current_indent -= 1;
            self.write_indent();
            self.write("}");
        } else {
            self.write(" {}");
        }
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

        if self.block_has_content(node) {
            self.write(" {");
            self.write_newline();
            self.current_indent += 1;

            self.format_block_attributes(node);

            self.current_indent -= 1;
            self.write_indent();
            self.write("}");
        } else {
            self.write(" {}");
        }
    }

    fn format_read_resource_expr(&mut self, node: &CstNode) {
        self.write("read ");
        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write(&token.text);
                break;
            }
        }

        if self.block_has_content(node) {
            self.write(" {");
            self.write_newline();
            self.current_indent += 1;

            self.format_block_attributes(node);

            self.current_indent -= 1;
            self.write_indent();
            self.write("}");
        } else {
            self.write(" {}");
        }
    }

    fn format_for_expr(&mut self, node: &CstNode) {
        // For expressions are preserved as-is with proper indentation
        // Format: for <binding> in <iterable> { <body> }
        self.write("for ");

        let mut saw_open_brace = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "for" {
                        continue; // Already written
                    }
                    if token.text == "in" {
                        self.write(" in ");
                        continue;
                    }
                    if token.text == "{" {
                        self.write(" {");
                        self.write_newline();
                        self.current_indent += 1;
                        saw_open_brace = true;
                        continue;
                    }
                    if token.text == "}" {
                        self.current_indent -= 1;
                        self.write_indent();
                        self.write("}");
                        continue;
                    }
                    self.write(&token.text);
                }
                CstChild::Node(n) => {
                    if n.kind == NodeKind::ForBinding {
                        self.format_for_binding(n);
                    } else if !saw_open_brace {
                        // Iterable
                        self.format_node(n);
                    } else {
                        // Body content
                        if n.kind == NodeKind::ResourceExpr
                            || n.kind == NodeKind::ReadResourceExpr
                            || n.kind == NodeKind::LocalBinding
                        {
                            self.write_indent();
                            self.format_node(n);
                            self.write_newline();
                        } else {
                            self.format_node(n);
                        }
                    }
                }
                CstChild::Trivia(_) => {
                    // Skip trivia - we control whitespace
                }
            }
        }
    }

    fn format_for_binding(&mut self, node: &CstNode) {
        // Collect all identifiers and check for parens
        let mut has_open_paren = false;
        let mut tokens: Vec<&str> = Vec::new();

        for child in &node.children {
            if let CstChild::Token(token) = child {
                if token.text == "(" {
                    has_open_paren = true;
                } else if token.text == ")" || token.text == "," {
                    // skip
                } else {
                    tokens.push(&token.text);
                }
            }
        }

        if has_open_paren {
            // Indexed binding: (i, x)
            self.write(&format!("({}, {})", tokens[0], tokens[1]));
        } else if tokens.len() == 2 {
            // Map binding: k, v
            self.write(&format!("{}, {}", tokens[0], tokens[1]));
        } else {
            // Simple binding: x
            self.write(tokens[0]);
        }
    }

    fn format_nested_block(&mut self, node: &CstNode) {
        self.write_indent();

        // Find and write block name (identifier)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                self.write(&token.text);
                break;
            }
        }

        if self.block_has_content(node) {
            self.write(" {");
            self.write_newline();
            self.current_indent += 1;

            self.format_block_attributes(node);

            self.current_indent -= 1;
            self.write_indent();
            self.write("}");
        } else {
            self.write(" {}");
        }
        self.write_newline();
    }

    fn block_has_content(&self, node: &CstNode) -> bool {
        node.children.iter().any(|child| {
            matches!(child, CstChild::Node(n) if n.kind == NodeKind::Attribute || n.kind == NodeKind::NestedBlock || n.kind == NodeKind::LocalBinding)
                || matches!(child, CstChild::Trivia(Trivia::LineComment(_)))
        })
    }

    fn format_block_attributes(&mut self, node: &CstNode) {
        // Collect attributes into groups separated by blank lines
        let mut groups: Vec<Vec<&CstNode>> = Vec::new();
        let mut current_group: Vec<&CstNode> = Vec::new();
        let mut inline_comments: std::collections::HashMap<usize, &Trivia> =
            std::collections::HashMap::new();
        let mut pending_comments: Vec<&Trivia> = Vec::new();

        let mut attr_index = 0;
        let mut newline_count = 0;
        for child in &node.children {
            match child {
                CstChild::Node(n)
                    if n.kind == NodeKind::Attribute
                        || n.kind == NodeKind::NestedBlock
                        || n.kind == NodeKind::LocalBinding =>
                {
                    // Write any pending standalone comments
                    for comment in pending_comments.drain(..) {
                        self.write_indent();
                        self.write_trivia(comment);
                        self.write_newline();
                    }
                    // Start a new group if there was a blank line
                    if newline_count > 1 && !current_group.is_empty() {
                        groups.push(std::mem::take(&mut current_group));
                    }
                    current_group.push(n);
                    attr_index += 1;
                    newline_count = 0;
                }
                CstChild::Trivia(Trivia::LineComment(s)) => {
                    // Check if this is an inline comment (on same line as previous attribute)
                    // For simplicity, we treat comments after a newline as standalone
                    if attr_index > 0 && !s.is_empty() && newline_count == 0 {
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
                    newline_count = 0;
                }
                CstChild::Trivia(Trivia::Newline) => {
                    newline_count += 1;
                }
                _ => {}
            }
        }
        // Don't forget the last group
        if !current_group.is_empty() {
            groups.push(current_group);
        }

        // Format each group with its own alignment
        let mut global_attr_index = 0;
        for (group_index, group) in groups.iter().enumerate() {
            // Add blank line between groups
            if group_index > 0 {
                self.write_newline();
            }

            // Calculate max key length for this group only (excluding nested blocks and block-converted attrs)
            let max_key_len = if self.config.align_attributes {
                group
                    .iter()
                    .filter(|attr| {
                        attr.kind == NodeKind::Attribute
                            && self.should_convert_to_blocks(attr).is_none()
                    })
                    .filter_map(|attr| self.get_attribute_key(attr))
                    .map(|k| k.len())
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };

            // Format each attribute/nested block/local binding in this group
            for attr in group {
                if attr.kind == NodeKind::LocalBinding {
                    self.format_let_binding(attr);
                } else if attr.kind == NodeKind::NestedBlock {
                    self.format_nested_block(attr);
                } else if let Some(block_name) = self.should_convert_to_blocks(attr) {
                    self.emit_list_as_blocks(attr, &block_name);
                } else {
                    let inline_comment = inline_comments.get(&global_attr_index);
                    self.format_attribute_aligned(attr, max_key_len, inline_comment.copied());
                }
                global_attr_index += 1;
            }
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

    /// Get the first child node after `=` in any node (Attribute, MapEntry, etc.)
    fn get_value_after_equals<'a>(&self, node: &'a CstNode) -> Option<&'a CstNode> {
        let mut found_equals = false;
        for child in &node.children {
            match child {
                CstChild::Token(token) if token.text == "=" => {
                    found_equals = true;
                }
                CstChild::Node(n) if found_equals => {
                    return Some(n);
                }
                _ => {}
            }
        }
        None
    }

    /// Check if a List node contains only Map children (possibly wrapped in PipeExpr)
    fn list_contains_only_maps(&self, list_node: &CstNode) -> bool {
        let nodes: Vec<&CstNode> = list_node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        !nodes.is_empty() && nodes.iter().all(|n| self.is_map_node(n))
    }

    /// Check if a node is a Map, possibly wrapped in a PipeExpr
    fn is_map_node(&self, node: &CstNode) -> bool {
        if node.kind == NodeKind::Map {
            return true;
        }
        // A PipeExpr with no pipe operators just wraps a single primary
        if node.kind == NodeKind::PipeExpr {
            return node
                .children
                .iter()
                .filter_map(|child| {
                    if let CstChild::Node(n) = child {
                        Some(n)
                    } else {
                        None
                    }
                })
                .any(|n| n.kind == NodeKind::Map);
        }
        false
    }

    /// Extract Map nodes from a List node (unwrapping PipeExpr wrappers)
    fn extract_maps_from_list<'a>(&self, list_node: &'a CstNode) -> Vec<&'a CstNode> {
        list_node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child {
                    self.unwrap_to_map(n)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Unwrap a node to get the inner Map, handling PipeExpr wrappers
    fn unwrap_to_map<'a>(&self, node: &'a CstNode) -> Option<&'a CstNode> {
        if node.kind == NodeKind::Map {
            return Some(node);
        }
        if node.kind == NodeKind::PipeExpr {
            for child in &node.children {
                if let CstChild::Node(n) = child
                    && n.kind == NodeKind::Map
                {
                    return Some(n);
                }
            }
        }
        None
    }

    /// Unwrap a node to get the inner List, handling PipeExpr wrappers
    fn unwrap_to_list<'a>(&self, node: &'a CstNode) -> Option<&'a CstNode> {
        if node.kind == NodeKind::List {
            return Some(node);
        }
        if node.kind == NodeKind::PipeExpr {
            for child in &node.children {
                if let CstChild::Node(n) = child
                    && n.kind == NodeKind::List
                {
                    return Some(n);
                }
            }
        }
        None
    }

    /// Check if an attribute should be converted to block syntax
    fn should_convert_to_blocks(&self, node: &CstNode) -> Option<String> {
        self.should_convert_to_blocks_generic(node, |s, n| s.get_attribute_key(n))
    }

    /// Check if a node (Attribute or MapEntry) should be converted to block syntax.
    /// Returns the block name if the node's key is in block_names and its value
    /// is a list of maps.
    fn should_convert_to_blocks_generic(
        &self,
        node: &CstNode,
        key_fn: impl Fn(&Self, &CstNode) -> Option<String>,
    ) -> Option<String> {
        let key = key_fn(self, node)?;
        let block_name = self.block_names.get(&key)?;
        let value_node = self.get_value_after_equals(node)?;
        let list_node = self.unwrap_to_list(value_node)?;
        if self.list_contains_only_maps(list_node) {
            Some(block_name.clone())
        } else {
            None
        }
    }

    /// Check if a map entry should be converted to block syntax
    fn should_convert_map_entry_to_blocks(&self, node: &CstNode) -> Option<String> {
        self.should_convert_to_blocks_generic(node, |s, n| s.get_map_entry_key(n))
    }

    /// Emit a node's `= [{...}, {...}]` value as multiple `block_name { ... }` blocks.
    /// Works for both Attribute and MapEntry nodes.
    fn emit_list_as_blocks(&mut self, node: &CstNode, block_name: &str) {
        let value_node = self.get_value_after_equals(node).unwrap();
        let list_node = self.unwrap_to_list(value_node).unwrap();
        let maps = self.extract_maps_from_list(list_node);

        for map_node in maps {
            self.write_indent();
            self.write(block_name);

            let items: Vec<&CstNode> = map_node
                .children
                .iter()
                .filter_map(|child| {
                    if let CstChild::Node(n) = child
                        && (n.kind == NodeKind::MapEntry || n.kind == NodeKind::NestedBlock)
                    {
                        Some(n)
                    } else {
                        None
                    }
                })
                .collect();

            if items.is_empty() {
                self.write(" {}");
                self.write_newline();
            } else {
                self.write(" {");
                self.write_newline();
                self.current_indent += 1;
                self.format_map_entries_as_block_attrs(&items);
                self.current_indent -= 1;
                self.write_indent();
                self.write("}");
                self.write_newline();
            }
        }
    }

    /// Format map entries as block attributes (used when converting list-of-maps to blocks)
    fn format_map_entries_as_block_attrs(&mut self, items: &[&CstNode]) {
        // Calculate max key length for alignment (excluding entries that will be converted to blocks)
        let max_key_len = if self.config.align_attributes {
            items
                .iter()
                .filter(|item| {
                    item.kind == NodeKind::MapEntry
                        && self.should_convert_map_entry_to_blocks(item).is_none()
                })
                .filter_map(|entry| self.get_map_entry_key(entry))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        for item in items {
            if item.kind == NodeKind::NestedBlock {
                self.format_nested_block(item);
            } else if let Some(block_name) = self.should_convert_map_entry_to_blocks(item) {
                self.emit_list_as_blocks(item, &block_name);
            } else {
                self.format_map_entry_aligned(item, max_key_len);
            }
        }
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

    fn format_variable_ref(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) if self.is_identifier(&token.text) => {
                    self.write(&token.text);
                }
                CstChild::Node(n) if n.kind == NodeKind::FieldAccess => {
                    self.format_field_access(n);
                }
                CstChild::Node(n) if n.kind == NodeKind::IndexAccess => {
                    self.format_index_access(n);
                }
                _ => {}
            }
        }
    }

    fn format_field_access(&mut self, node: &CstNode) {
        self.write(".");
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                self.write(&token.text);
            }
        }
    }

    fn format_index_access(&mut self, node: &CstNode) {
        self.write("[");
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "[" || token.text == "]" {
                        continue;
                    }
                    self.write(&token.text);
                }
                CstChild::Node(n) => {
                    self.format_node(n);
                }
                _ => {}
            }
        }
        self.write("]");
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

    fn format_map(&mut self, node: &CstNode) {
        self.write("{");

        // Collect map entries and nested blocks
        let items: Vec<&CstNode> = node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child
                    && (n.kind == NodeKind::MapEntry || n.kind == NodeKind::NestedBlock)
                {
                    return Some(n);
                }
                None
            })
            .collect();

        if items.is_empty() {
            self.write("}");
            return;
        }

        self.write_newline();
        self.current_indent += 1;

        // Calculate max key length for alignment (only map entries)
        let max_key_len = if self.config.align_attributes {
            items
                .iter()
                .filter(|item| item.kind == NodeKind::MapEntry)
                .filter_map(|entry| self.get_map_entry_key(entry))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        // Format each item
        for item in items {
            if item.kind == NodeKind::NestedBlock {
                self.format_nested_block(item);
            } else if item.kind == NodeKind::MapEntry {
                // Check if this map entry should be converted to block syntax
                if let Some(block_name) = self.should_convert_map_entry_to_blocks(item) {
                    self.emit_list_as_blocks(item, &block_name);
                } else {
                    self.format_map_entry_aligned(item, max_key_len);
                }
            } else {
                self.format_map_entry_aligned(item, max_key_len);
            }
        }

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
    }

    fn get_map_entry_key(&self, node: &CstNode) -> Option<String> {
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                return Some(token.text.clone());
            }
        }
        None
    }

    fn format_map_entry(&mut self, node: &CstNode) {
        self.format_map_entry_aligned(node, 0);
    }

    fn format_map_entry_aligned(&mut self, node: &CstNode, align_to: usize) {
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
                    } else if token.text == "," {
                        // Skip trailing comma - we'll handle it consistently
                        continue;
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

        self.write_newline();
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

    #[test]
    fn test_format_map() {
        let input = "awscc.ec2.vpc {\ntags = {Environment=\"dev\"Project=\"test\"}\n}";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        // Map should be formatted with entries on separate lines
        assert!(result.contains("tags = {"), "missing 'tags = {{'");
        assert!(
            result.contains("Environment = \"dev\""),
            "missing Environment"
        );
        // With alignment, Project has extra spaces to align with Environment
        assert!(
            result.contains("Project") && result.contains("= \"test\""),
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
        let input = "awscc.ec2.vpc {\ntags = {Environment=\"dev\"\nProject=\"test\"}\n}";
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
        let input = "awscc.ec2.vpc {\n  tags = {\n    Environment = \"dev\"\n    Project = \"test\"\n  }\n}\n";
        let config = FormatConfig::default();

        let first = format(input, &config).unwrap();
        let second = format(&first, &config).unwrap();

        assert_eq!(first, second, "Map formatting should be idempotent");
    }

    #[test]
    fn test_format_preserves_blank_lines_between_attributes() {
        let input = "awscc.ec2.vpc {\n  name = \"test\"\n  cidr = \"10.0.0.0/16\"\n\n  tags = {\n    Env = \"dev\"\n  }\n}\n";
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
        let input = "awscc.ec2.vpc {\n  name = \"test\"\n\n  tags = {\n    Env = \"dev\"\n  }\n}\n";
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
            "awscc.ec2.vpc {\nenable_dns_hostnames = true\nname = \"test\"\n\ntags = {}\n}\n";
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
        let input = "awscc.ec2.vpc {\n  cidr_block = \"10.0.0.0/16\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();

        assert_eq!(
            result, "awscc.ec2.vpc {\n  cidr_block = \"10.0.0.0/16\"\n}\n",
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
        let input = r#"awscc.ec2.security_group {
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
        assert!(result.contains("    ip_protocol = \"tcp\""));
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
        assert!(result.contains("effect = \"Allow\""));

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
        let expected = r#"awscc.ec2.ipam {
  operating_region {
    region_name = "ap-northeast-1"
  }
}
"#;
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
        let input = r#"awscc.s3.bucket {
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
        let expected = r#"awscc.s3.bucket {
  lifecycle_configuration = {
    rule {
      id     = "expire-old-objects"
      status = "Enabled"
    }
    rule {
      id     = "transition-to-glacier"
      status = "Enabled"
    }
  }
}
"#;
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
        let input = r#"awscc.s3.bucket {
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
        let expected = r#"awscc.s3.bucket {
  bucket_encryption = {
    server_side_encryption_configuration {
      bucket_key_enabled                = true
      server_side_encryption_by_default = {
        sse_algorithm = "AES256"
      }
    }
  }
}
"#;
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
        let input = r#"awscc.ec2.ipam {
  operating_region {
    region_name = "ap-northeast-1"
  }
}
"#;
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
            result.contains("config[\"key\"].value"),
            "Expected string index access in:\n{}",
            result
        );
    }

    #[test]
    fn test_format_for_expression() {
        let input = "let subnets = for subnet in subnets {\n  awscc.ec2.subnet {\n    cidr_block = subnet.cidr\n  }\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("for subnet in subnets"),
            "Expected for expression in:\n{}",
            result
        );
        assert!(
            result.contains("awscc.ec2.subnet"),
            "Expected resource in for body:\n{}",
            result
        );
    }

    #[test]
    fn test_format_read_resource_expr() {
        let input = "let vpc = read awscc.ec2.vpc {\n  vpc_id = \"vpc-123\"\n}\n";
        let config = FormatConfig::default();
        let result = format(input, &config).unwrap();
        assert!(
            result.contains("read awscc.ec2.vpc"),
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
}
