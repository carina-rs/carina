//! Formatter methods for value-shaped CST nodes: lists, maps, map entries,
//! and type expressions (including struct types).

use super::super::cst::{CstChild, CstNode, NodeKind, Trivia};
use super::super::format::Formatter;

/// A map child paired with the trivia that immediately preceded it
/// (leading comments and an optional blank line). Used by `format_map`
/// so comments stay attached to the next item rather than being
/// dropped on emit.
struct MapItem<'a> {
    node: &'a CstNode,
    leading_comments: Vec<&'a Trivia>,
    leading_blank_line: bool,
}

/// Return the trivia (`comments`, `Newline`s, `Whitespace`) that appears
/// after the value-expression node inside a `MapEntry`. Pest's grammar
/// (`map_entry = ... ~ expression ~ trivia* ~ comma?`) absorbs the
/// trailing trivia into the entry, but visually those comments and blank
/// lines belong to the *next* sibling — `format_map` reattaches them.
///
/// Trivia *between* key/`=`/value (the two intermediate `trivia*` slots in
/// the rule) is intentionally not returned here: those positions belong
/// inside the entry itself and are tracked separately under #2535. Adding
/// them to the returned tail would re-emit them between siblings, which
/// is the wrong attachment site.
fn map_entry_trailing_trivia(entry: &CstNode) -> Vec<&Trivia> {
    let mut seen_equals = false;
    let mut past_value = false;
    let mut tail: Vec<&Trivia> = Vec::new();
    for child in &entry.children {
        match child {
            CstChild::Token(t) if t.text == "=" => {
                seen_equals = true;
            }
            CstChild::Node(_) if seen_equals && !past_value => {
                past_value = true;
            }
            CstChild::Trivia(t) if past_value => {
                tail.push(t);
            }
            // Optional trailing comma is consumed silently; trivia after
            // it (e.g. `version = '...', # comment`) keeps flowing in.
            CstChild::Token(t) if past_value && t.text == "," => {}
            _ => {}
        }
    }
    tail
}

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
                    } else if token.text == "|" {
                        // Closed-set string type union separator: surface as
                        // ` | ` so unions read as cleanly as the surrounding
                        // ` = ` (#2615).
                        self.write(" | ");
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
        // carina-rs/carina#2586 + #2588: preserve the user's list
        // layout AND any comments inside the list. Multi-line input
        // renders multi-line; comments above an element become its
        // leading comments (own line at element indent), comments on
        // the same line as the closing comma become trailing
        // comments (kept inline with the element). Single-line input
        // stays single-line *unless* it contains a line comment, in
        // which case it is promoted to multi-line — a line comment
        // intrinsically ends its line and cannot live inline.
        let mut items = collect_list_items(node);
        // #2872: `directives { depends_on = [...] }` sorts elements
        // alphabetically on emission. Sort is order-insensitive in
        // semantics, so this is purely cosmetic — but produces
        // stable diffs across edits.
        if self.in_directives_depends_on() {
            sort_depends_on_items(&mut items);
        }
        let multiline = list_is_multiline(node) || any_line_comment(&items);

        self.write("[");
        if multiline {
            self.write_newline();
            self.current_indent += 1;
            for item in &items {
                for c in &item.leading_comments {
                    self.write_indent();
                    self.write_trivia(c);
                    self.write_newline();
                }
                self.write_indent();
                self.emit_list_element(&item.element);
                self.write(",");
                for c in &item.trailing_comments {
                    self.write(" ");
                    self.write_trivia(c);
                }
                self.write_newline();
            }
            // Trailing comments after the last element but before `]`
            // (e.g. a comment block at the very bottom of the list).
            for c in &trailing_after_last(node) {
                self.write_indent();
                self.write_trivia(c);
                self.write_newline();
            }
            self.current_indent -= 1;
            self.write_indent();
        } else {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    self.write(", ");
                }
                // Block comments that became leading-of-this-element
                // (e.g. `'a', /* mid */ 'b'`) must stay inline.
                // `any_line_comment` already promoted any list with
                // line comments to multi-line, so we know these are
                // all `BlockComment`.
                for c in &item.leading_comments {
                    self.write_trivia(c);
                    self.write(" ");
                }
                self.emit_list_element(&item.element);
                for c in &item.trailing_comments {
                    self.write(" ");
                    self.write_trivia(c);
                }
            }
        }
        self.write("]");
    }

    fn emit_list_element(&mut self, element: &ListElement<'_>) {
        match element {
            ListElement::Token(t) => self.write_token(t),
            ListElement::Node(n) => self.format_node(n),
        }
    }

    fn in_directives_depends_on(&self) -> bool {
        self.block_stack.last().map(String::as_str) == Some("directives")
            && self.attr_stack.last().map(String::as_str) == Some("depends_on")
    }
}

/// Identifier text of a `depends_on` list element, used as the sort
/// key. Returns `None` for shapes the formatter shouldn't touch
/// (block-bodied expressions, malformed CST). Bare identifiers parse
/// as a single token, so the common case is straightforward; comments
/// attached to elements travel with them via `ListItem`.
fn list_element_sort_key<'a>(element: &ListElement<'a>) -> Option<String> {
    match element {
        ListElement::Token(t) => Some((*t).to_string()),
        ListElement::Node(n) => element_node_first_token(n),
    }
}

fn element_node_first_token(node: &CstNode) -> Option<String> {
    for child in &node.children {
        match child {
            CstChild::Token(tok) if !tok.text.trim().is_empty() => return Some(tok.text.clone()),
            CstChild::Node(n) => {
                if let Some(t) = element_node_first_token(n) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

fn sort_depends_on_items<'a>(items: &mut [ListItem<'a>]) {
    items.sort_by(|a, b| {
        let ka = list_element_sort_key(&a.element).unwrap_or_default();
        let kb = list_element_sort_key(&b.element).unwrap_or_default();
        ka.cmp(&kb)
    });
}

impl Formatter {
    pub(in crate::formatter) fn format_map(&mut self, node: &CstNode) {
        self.write("{");

        // Issue #2515: walk children in order and attach trivia (comments,
        // blank lines) to the *next* item — earlier the printer dropped
        // every comment between siblings. Pest's `map_entry` rule also
        // greedily absorbs trailing trivia into the entry itself, so we
        // additionally harvest each MapEntry's post-value tail.
        let mut items: Vec<MapItem<'_>> = Vec::new();
        let mut pending_comments: Vec<&Trivia> = Vec::new();
        let mut pending_blank_line = false;
        let mut newline_count: usize = 0;

        fn classify<'a>(
            t: &'a Trivia,
            pending_comments: &mut Vec<&'a Trivia>,
            pending_blank_line: &mut bool,
            newline_count: &mut usize,
        ) {
            match t {
                Trivia::LineComment(_) | Trivia::BlockComment(_) => {
                    pending_comments.push(t);
                    *newline_count = 0;
                }
                Trivia::Newline => {
                    *newline_count += 1;
                    if *newline_count > 1 {
                        *pending_blank_line = true;
                    }
                }
                Trivia::Whitespace(_) => {}
            }
        }

        for child in &node.children {
            match child {
                CstChild::Node(n)
                    if n.kind == NodeKind::MapEntry || n.kind == NodeKind::NestedBlock =>
                {
                    items.push(MapItem {
                        node: n,
                        leading_comments: std::mem::take(&mut pending_comments),
                        leading_blank_line: pending_blank_line && !items.is_empty(),
                    });
                    pending_blank_line = false;
                    newline_count = 0;

                    if n.kind == NodeKind::MapEntry {
                        for t in map_entry_trailing_trivia(n) {
                            classify(
                                t,
                                &mut pending_comments,
                                &mut pending_blank_line,
                                &mut newline_count,
                            );
                        }
                    }
                }
                CstChild::Trivia(t) => {
                    classify(
                        t,
                        &mut pending_comments,
                        &mut pending_blank_line,
                        &mut newline_count,
                    );
                }
                _ => {}
            }
        }

        if items.is_empty() && pending_comments.is_empty() {
            self.write("}");
            return;
        }

        self.write_newline();
        self.current_indent += 1;

        // Calculate max key length for alignment (only map entries)
        let max_key_len = if self.config.align_attributes {
            items
                .iter()
                .filter(|item| item.node.kind == NodeKind::MapEntry)
                .filter_map(|item| self.get_map_entry_key(item.node))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        // Format each item, with its leading comments preserved.
        for item in &items {
            if item.leading_blank_line {
                self.write_newline();
            }
            for comment in &item.leading_comments {
                self.write_indent();
                self.write_trivia(comment);
                self.write_newline();
            }
            if item.node.kind == NodeKind::NestedBlock {
                self.format_nested_block(item.node);
            } else if let Some(block_name) = self.should_convert_map_entry_to_blocks(item.node) {
                // MapEntry that should be converted to block syntax
                self.emit_list_as_blocks(item.node, &block_name);
            } else {
                self.format_map_entry_aligned(item.node, max_key_len);
            }
        }

        // Trailing comments after the last item but before `}` — keep them,
        // along with a blank-line separator if the source had one.
        if !pending_comments.is_empty() && pending_blank_line && !items.is_empty() {
            self.write_newline();
        }
        for comment in &pending_comments {
            self.write_indent();
            self.write_trivia(comment);
            self.write_newline();
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

/// Was this list written across multiple lines in the source?
///
/// True iff any `Trivia::Newline` appears among the list node's
/// children (i.e. between `[` and `]`). Used by `format_list` to
/// preserve the user's layout instead of forcing every list onto
/// one line — see carina-rs/carina#2586.
fn list_is_multiline(node: &CstNode) -> bool {
    node.children
        .iter()
        .any(|c| matches!(c, CstChild::Trivia(Trivia::Newline)))
}

/// One element of a list literal, paired with its leading comments
/// (above, on their own line in the source) and trailing comments
/// (on the same line as the element's closing comma in the source).
///
/// Used by `format_list` to keep both kinds of comments attached to
/// the right element when re-emitting — see carina-rs/carina#2588.
pub(in crate::formatter) struct ListItem<'a> {
    element: ListElement<'a>,
    leading_comments: Vec<&'a Trivia>,
    trailing_comments: Vec<&'a Trivia>,
}

/// A list element is either a leaf token (a string/number literal)
/// or a nested node (an expression or sub-list).
pub(in crate::formatter) enum ListElement<'a> {
    Token(&'a str),
    Node(&'a CstNode),
}

/// Returns true if any item has a `LineComment` attached. A line
/// comment intrinsically ends its line, so a single-line list that
/// contains one must be promoted to multi-line on emit.
fn any_line_comment(items: &[ListItem<'_>]) -> bool {
    items.iter().any(|i| {
        i.leading_comments
            .iter()
            .chain(i.trailing_comments.iter())
            .any(|t| matches!(t, Trivia::LineComment(_)))
    })
}

/// Walk the list CST node and group its children into `ListItem`s.
///
/// Trivia attachment rule (terraform-style P-trailing):
///
/// - A `LineComment` on the same logical line as the preceding
///   element (i.e. between that element's comma and the next
///   `Newline`) becomes that element's **trailing** comment, even
///   when it physically sits *after* the comma. Line comments run
///   to end of line, so they intrinsically belong to that line.
/// - A `BlockComment` on the same logical line as the preceding
///   element, but *after* that element's comma, becomes the
///   **leading** comment of the next element — terraform's natural
///   reading of `'a', /* mid */ 'b'`.
/// - Any comment that crosses a `Newline` after the previous
///   element becomes a **leading** comment of the next element.
/// - Comments at the very end of the list (after the last comma,
///   below the last element) are collected separately by
///   `trailing_after_last`.
fn collect_list_items(node: &CstNode) -> Vec<ListItem<'_>> {
    let mut items: Vec<ListItem<'_>> = Vec::new();
    let mut pending_leading: Vec<&Trivia> = Vec::new();
    // Comments that appeared after the most recent element but
    // before its closing comma (e.g. `'a' /* x */ , 'b'`). Get
    // attached to the previous element as trailing on the next
    // newline (or to the next element as leading on a fresh comma).
    let mut pre_comma_pending: Vec<&Trivia> = Vec::new();
    // Comments that appeared after the most recent element's comma
    // but before the next newline. Line comments here go back onto
    // the previous element as trailing; block comments go forward
    // as leading for the next element.
    let mut post_comma_pending: Vec<&Trivia> = Vec::new();
    // What we've passed since pushing the last element.
    let mut seen_comma = false;
    let mut crossed_newline = false;

    // Resolve `post_comma_pending` against the just-emitted-or-
    // about-to-be-emitted state. Line comments go back as trailing
    // of the previous element; block comments stay as leading of
    // the next element.
    fn flush_post_comma<'a>(
        items: &mut Vec<ListItem<'a>>,
        pending_leading: &mut Vec<&'a Trivia>,
        post_comma_pending: &mut Vec<&'a Trivia>,
    ) {
        for t in std::mem::take(post_comma_pending) {
            match t {
                Trivia::LineComment(_) => {
                    if let Some(prev) = items.last_mut() {
                        prev.trailing_comments.push(t);
                    } else {
                        pending_leading.push(t);
                    }
                }
                _ => pending_leading.push(t),
            }
        }
    }

    for child in &node.children {
        match child {
            CstChild::Token(token) => {
                let text = token.text.as_str();
                if text == "[" || text == "]" {
                    continue;
                }
                if text == "," {
                    // Comments seen between the element and the
                    // comma are still pre-comma — flush them as
                    // leading for the *next* element (rare shape).
                    pending_leading.append(&mut pre_comma_pending);
                    seen_comma = true;
                    crossed_newline = false;
                    continue;
                }
                flush_post_comma(&mut items, &mut pending_leading, &mut post_comma_pending);
                items.push(ListItem {
                    element: ListElement::Token(text),
                    leading_comments: std::mem::take(&mut pending_leading),
                    trailing_comments: Vec::new(),
                });
                seen_comma = false;
                crossed_newline = false;
            }
            CstChild::Node(n) => {
                flush_post_comma(&mut items, &mut pending_leading, &mut post_comma_pending);
                items.push(ListItem {
                    element: ListElement::Node(n),
                    leading_comments: std::mem::take(&mut pending_leading),
                    trailing_comments: Vec::new(),
                });
                seen_comma = false;
                crossed_newline = false;
            }
            CstChild::Trivia(t) => match t {
                Trivia::Whitespace(_) => {}
                Trivia::Newline => {
                    if !crossed_newline {
                        // First newline since the previous element:
                        // line comments are trailing-of-previous;
                        // block comments are leading-of-next.
                        flush_post_comma(&mut items, &mut pending_leading, &mut post_comma_pending);
                        // pre-comma pending (rare) also becomes
                        // leading for the next element on newline.
                        pending_leading.append(&mut pre_comma_pending);
                    }
                    crossed_newline = true;
                }
                Trivia::LineComment(_) | Trivia::BlockComment(_) => {
                    if crossed_newline {
                        pending_leading.push(t);
                    } else if seen_comma {
                        post_comma_pending.push(t);
                    } else if items.is_empty() {
                        pending_leading.push(t);
                    } else {
                        pre_comma_pending.push(t);
                    }
                }
            },
        }
    }

    // Anything still in post_comma_pending / pre_comma_pending /
    // pending_leading at end belongs after the last element and is
    // collected separately by `trailing_after_last`.
    items
}

/// Trivia (line/block comments) that appears after the last list
/// element's *trailing* slot (i.e. on its own line below the last
/// element) but before the closing `]`. These are emitted as
/// standalone indented lines in multi-line output so a comment block
/// at the very bottom of a list is not lost.
///
/// Comments that sit on the same logical line as the last element
/// (between its comma and the next newline) are *not* collected
/// here — `collect_list_items` has already attached them as trailing
/// comments on that element.
fn trailing_after_last<'a>(node: &'a CstNode) -> Vec<&'a Trivia> {
    let mut comments: Vec<&'a Trivia> = Vec::new();
    let mut seen_any_element = false;
    let mut crossed_newline_since_last_element = false;

    for child in &node.children {
        match child {
            CstChild::Token(token) => {
                let text = token.text.as_str();
                if text == "[" {
                    continue;
                }
                if text == "]" {
                    break;
                }
                if text == "," {
                    continue;
                }
                seen_any_element = true;
                crossed_newline_since_last_element = false;
                comments.clear();
            }
            CstChild::Node(_) => {
                seen_any_element = true;
                crossed_newline_since_last_element = false;
                comments.clear();
            }
            CstChild::Trivia(t) => match t {
                Trivia::Newline => {
                    if seen_any_element {
                        crossed_newline_since_last_element = true;
                    }
                }
                Trivia::LineComment(_) | Trivia::BlockComment(_) => {
                    if seen_any_element && crossed_newline_since_last_element {
                        comments.push(t);
                    }
                }
                Trivia::Whitespace(_) => {}
            },
        }
    }
    comments
}
