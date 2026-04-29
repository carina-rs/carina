//! Formatter methods for value-shaped CST nodes: lists, maps, map entries,
//! and type expressions (including struct types).

use super::super::cst::{CstChild, CstNode, NodeKind};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_type_expr(&mut self, node: &CstNode) {
        // Type expressions: aws.vpc, list(cidr), map(string), string, bool,
        // int, cidr, struct { name: type, ... }.
        //
        // Struct types need canonical spacing (`struct { a: int, b: string }`);
        // handle them via a dedicated path so the default fall-through
        // doesn't collapse whitespace between `struct`, `{`, `:`, `,`, `}`.
        if Self::type_expr_is_struct(node) {
            self.format_struct_type_expr(node);
            return;
        }
        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if token.text == "(" {
                        self.write("(");
                    } else if token.text == ")" {
                        self.write(")");
                    } else {
                        self.write_token(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    self.format_type_expr(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    /// A `type_expr` node is a struct when it either carries the `struct`
    /// keyword directly or wraps a single child that does. Intermediate
    /// wrapping happens because `type_struct` is itself reparented to
    /// `NodeKind::TypeExpr` by the CST builder.
    fn type_expr_is_struct(node: &CstNode) -> bool {
        for child in &node.children {
            match child {
                CstChild::Token(t) if t.text == "struct" => return true,
                CstChild::Token(_) => return false,
                CstChild::Node(_) | CstChild::Trivia(_) => continue,
            }
        }
        false
    }

    fn format_struct_type_expr(&mut self, node: &CstNode) {
        // Emit `struct ` from the first Token child (the `struct` keyword),
        // then format each nested struct_field child. struct_field_list is a
        // single wrapper node; we descend into it transparently.
        let mut wrote_struct_kw = false;
        let mut fields: Vec<&CstNode> = Vec::new();
        Self::collect_struct_parts(node, &mut wrote_struct_kw, &mut fields);

        self.write("struct");
        if fields.is_empty() {
            self.write(" {}");
            return;
        }
        self.write(" { ");
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.format_struct_field(field);
        }
        self.write(" }");
    }

    /// Walk the struct subtree and collect the per-field nodes while
    /// tolerating the CstBuilder's flattening of struct_field_list /
    /// struct_field into NodeKind::TypeExpr.
    fn collect_struct_parts<'a>(
        node: &'a CstNode,
        saw_kw: &mut bool,
        fields: &mut Vec<&'a CstNode>,
    ) {
        for child in &node.children {
            match child {
                CstChild::Token(t) if t.text == "struct" => *saw_kw = true,
                CstChild::Token(_) => {}
                CstChild::Node(n) => {
                    if Self::type_expr_is_struct_field(n) {
                        fields.push(n);
                    } else {
                        Self::collect_struct_parts(n, saw_kw, fields);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    /// Heuristic: a struct_field node contains a name Token, a `:` Token,
    /// and a nested type_expr Node — and crucially no `struct` keyword at
    /// its own top level. We recognize it by "has an identifier Token
    /// directly followed (ignoring trivia) by a `:` Token."
    fn type_expr_is_struct_field(node: &CstNode) -> bool {
        let mut saw_ident = false;
        for child in &node.children {
            match child {
                CstChild::Token(t) => {
                    if t.text == ":" && saw_ident {
                        return true;
                    }
                    if t.text == "struct" || t.text == "{" || t.text == "}" {
                        return false;
                    }
                    saw_ident = true;
                }
                CstChild::Node(_) => return false,
                CstChild::Trivia(_) => {}
            }
        }
        false
    }

    fn format_struct_field(&mut self, node: &CstNode) {
        let mut wrote_name = false;
        for child in &node.children {
            match child {
                CstChild::Token(t) => {
                    if t.text == ":" {
                        self.write(": ");
                    } else if !wrote_name {
                        self.write(&t.text);
                        wrote_name = true;
                    } else {
                        // Unexpected extra token in field; emit defensively.
                        self.write_token(&t.text);
                    }
                }
                CstChild::Node(n) => {
                    self.format_type_expr(n);
                }
                CstChild::Trivia(_) => {}
            }
        }
    }

    pub(in crate::formatter) fn format_list(&mut self, node: &CstNode) {
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
                    self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_map(&mut self, node: &CstNode) {
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

    pub(in crate::formatter) fn get_map_entry_key(&self, node: &CstNode) -> Option<String> {
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_key_token(&token.text)
            {
                return Some(token.text.clone());
            }
        }
        None
    }

    pub(in crate::formatter) fn format_map_entry(&mut self, node: &CstNode) {
        self.format_map_entry_aligned(node, 0);
    }

    pub(in crate::formatter) fn format_map_entry_aligned(
        &mut self,
        node: &CstNode,
        align_to: usize,
    ) {
        self.write_indent();

        let mut key_len: usize;
        let mut wrote_key = false;
        let mut wrote_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_key && self.is_key_token(&token.text) {
                        key_len = token.text.len();
                        self.write_token(&token.text);
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
                        self.write_token(&token.text);
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
}
