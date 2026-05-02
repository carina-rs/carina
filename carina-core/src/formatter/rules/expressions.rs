//! Formatter methods for expression-shaped constructs: `for`, `if`/`else`,
//! pipe (`|>`), compose (`>>`), function calls, variable refs, field
//! access, and index access.

use super::super::cst::{CstChild, CstNode, NodeKind};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_for_expr(&mut self, node: &CstNode) {
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
                    self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_if_expr(&mut self, node: &CstNode) {
        // Format: if <condition> { <body> } else { <body> }
        self.write("if ");

        let mut saw_open_brace = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "if" {
                        continue; // Already written
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
                    self.write_token(&token.text);
                }
                CstChild::Node(n) => {
                    if n.kind == NodeKind::ElseClause {
                        self.write(" ");
                        self.format_else_clause(n);
                    } else if !saw_open_brace {
                        // Condition expression
                        self.format_node(n);
                    } else {
                        // Body content
                        self.write_indent();
                        self.format_node(n);
                        self.write_newline();
                    }
                }
                CstChild::Trivia(_) => {
                    // Skip trivia
                }
            }
        }
    }

    pub(in crate::formatter) fn format_else_clause(&mut self, node: &CstNode) {
        self.write("else {");
        self.write_newline();
        self.current_indent += 1;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "else" || token.text == "{" {
                        continue; // Already handled
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
                    self.write_indent();
                    self.format_node(n);
                    self.write_newline();
                }
                CstChild::Trivia(_) => {}
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

    pub(in crate::formatter) fn format_pipe_expr(&mut self, node: &CstNode) {
        let mut first = true;
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "|>" {
                        self.write(" |> ");
                    } else {
                        self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_compose_expr(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == ">>" {
                        self.write(" >> ");
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

    pub(in crate::formatter) fn format_function_call(&mut self, node: &CstNode) {
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
                        self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_variable_ref(&mut self, node: &CstNode) {
        for child in &node.children {
            match child {
                CstChild::Token(token) if self.is_identifier(&token.text) => {
                    self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_subscripted_id(&mut self, node: &CstNode) {
        // `binding.field[idx]…` — the namespaced_id portion is a single
        // token (the `@{ }` rule produces no inner pairs), and each
        // `index_access` child carries its own `[` / expression / `]`.
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    self.write_token(&token.text);
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
                self.write_token(&token.text);
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
                    self.write_token(&token.text);
                }
                CstChild::Node(n) => {
                    self.format_node(n);
                }
                _ => {}
            }
        }
        self.write("]");
    }
}
