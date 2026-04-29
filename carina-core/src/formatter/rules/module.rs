//! Formatter methods for `module`, `use`, and `require` constructs.

use super::super::cst::{CstChild, CstNode, NodeKind};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_use_expr(&mut self, node: &CstNode) {
        self.write("use");
        self.format_block_body_tail(node);
    }

    pub(in crate::formatter) fn format_module_call(&mut self, node: &CstNode) {
        self.write_indent();

        // Find and write module name
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                self.write_token(&token.text);
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

    /// Format a state block (import, removed, moved)
    pub(in crate::formatter) fn format_require_statement(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("require ");

        // Write children: validate_expr, comma, string
        let mut wrote_expr = false;
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "require" {
                        continue;
                    }
                    if token.text == "," {
                        self.write(", ");
                        continue;
                    }
                    self.write_token(&token.text);
                }
                CstChild::Node(n) => {
                    if n.kind == NodeKind::ValidateExpr {
                        if wrote_expr {
                            continue;
                        }
                        self.format_default(n);
                        wrote_expr = true;
                    } else {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }
        self.write_newline();
    }
}
