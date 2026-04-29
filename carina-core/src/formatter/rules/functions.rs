//! Formatter methods for `fn` definitions and their parameters.

use super::super::cst::{CstChild, CstNode, NodeKind};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_fn_def(&mut self, node: &CstNode) {
        // Format: fn name(params) { body }
        self.write("fn ");

        let mut saw_close_paren = false;
        let mut saw_open_brace = false;
        let mut param_count = 0;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "fn" {
                        continue; // Already written
                    }
                    if token.text == "(" {
                        self.write("(");
                        continue;
                    }
                    if token.text == ")" {
                        self.write(")");
                        saw_close_paren = true;
                        continue;
                    }
                    if token.text == ":" && saw_close_paren && !saw_open_brace {
                        // Return type colon - handled when we see the TypeExpr node
                        continue;
                    }
                    if token.text == "," && !saw_close_paren {
                        // Comma between params - handled by param_count logic
                        continue;
                    }
                    if token.text == "{" && saw_close_paren {
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
                    self.write_token(&token.text);
                }
                CstChild::Node(n) => {
                    if n.kind == NodeKind::FnParam {
                        if param_count > 0 {
                            self.write(", ");
                        }
                        self.format_fn_param(n);
                        param_count += 1;
                    } else if n.kind == NodeKind::TypeExpr && saw_close_paren && !saw_open_brace {
                        // Return type annotation: ): type {
                        self.write(": ");
                        self.format_node(n);
                    } else if saw_open_brace {
                        // Body content (local let or expression)
                        if n.kind == NodeKind::LocalBinding {
                            // LocalBinding formats its own indent and newline via format_let_binding
                            self.format_node(n);
                        } else {
                            self.write_indent();
                            self.format_node(n);
                            self.write_newline();
                        }
                    } else {
                        self.format_node(n);
                    }
                }
                CstChild::Trivia(_) => {
                    // Skip trivia - we control whitespace
                }
            }
        }
    }

    fn format_fn_param(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "=" {
                        self.write(" = ");
                    } else if token.text == ":" {
                        self.write(": ");
                    } else {
                        self.write_token(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    self.format_node(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
    }
}
