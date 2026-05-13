//! Formatter methods for `provider` and `backend` blocks.

use super::super::cst::{CstChild, CstNode};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_backend_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("backend ");

        // Find and write backend type (e.g., "s3")
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
                && token.text != "backend"
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

    pub(in crate::formatter) fn format_provider_block(&mut self, node: &CstNode) {
        self.write_indent();
        self.write("provider ");

        // Find and write provider name
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
                && token.text != "provider"
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

    /// Format `provider <kind> { ... }` when it appears as the RHS of a
    /// `let` binding. The surrounding `let` formatter has already emitted
    /// the indent and `let name = ` prefix, so this only emits the
    /// keyword, kind identifier, and block body.
    pub(in crate::formatter) fn format_provider_expr(&mut self, node: &CstNode) {
        self.write("provider ");
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
                && token.text != "provider"
            {
                self.write_token(&token.text);
                break;
            }
        }
        self.format_block_body_tail(node);
    }
}
