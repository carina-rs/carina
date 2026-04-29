//! Formatter methods for resources, `let` bindings, and state-mutation
//! blocks (`import`, `removed`, `moved`).

use super::super::cst::{CstChild, CstNode};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_state_block(&mut self, node: &CstNode, keyword: &str) {
        self.write_indent();
        self.write(keyword);
        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        // Format inner attributes (to, from, id)
        for child in &node.children {
            if let CstChild::Node(child_node) = child {
                self.format_node(child_node);
            }
        }

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
        self.write_newline();
    }

    /// Format a state block attribute (to = ..., from = ..., id = ...)
    pub(in crate::formatter) fn format_state_block_attr(&mut self, node: &CstNode) {
        self.write_indent();

        // Collect tokens: keyword, "=", and value
        let mut wrote_keyword = false;
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_keyword
                        && (token.text == "to" || token.text == "from" || token.text == "id")
                    {
                        self.write_token(&token.text);
                        wrote_keyword = true;
                    } else if token.text == "=" {
                        self.write(" = ");
                    } else {
                        self.write_token(&token.text);
                    }
                }
                CstChild::Node(child_node) => {
                    self.format_node(child_node);
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write_newline();
    }

    /// Format a resource address: `provider.service.type "name"`
    pub(in crate::formatter) fn format_resource_address(&mut self, node: &CstNode) {
        let mut first = true;
        for child in &node.children {
            if let CstChild::Token(token) = child {
                if first {
                    self.write_token(&token.text);
                    first = false;
                } else {
                    self.write(" ");
                    self.write_token(&token.text);
                }
            }
        }
    }

    pub(in crate::formatter) fn format_let_binding(&mut self, node: &CstNode) {
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
                        self.write_token(&token.text);
                        found_name = true;
                        continue;
                    }
                    if found_equals {
                        self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_anonymous_resource(&mut self, node: &CstNode) {
        self.write_indent();

        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_resource_expr(&mut self, node: &CstNode) {
        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write_token(&token.text);
                break;
            }
        }
        self.format_block_body_tail(node);
    }

    pub(in crate::formatter) fn format_upstream_state_expr(&mut self, node: &CstNode) {
        self.write("upstream_state");
        self.format_block_body_tail(node);
    }

    pub(in crate::formatter) fn format_read_resource_expr(&mut self, node: &CstNode) {
        self.write("read ");
        // Write resource type (namespaced_id)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && token.text.contains('.')
            {
                self.write_token(&token.text);
                break;
            }
        }
        self.format_block_body_tail(node);
    }
}
