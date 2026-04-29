//! Formatter methods for attribute lists, argument lists, nested blocks,
//! and the supporting block-conversion / alignment helpers.

use super::super::cst::{CstChild, CstNode, NodeKind, Trivia};
use super::super::format::Formatter;

impl Formatter {
    pub(in crate::formatter) fn format_arguments_block(&mut self, node: &CstNode) {
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

    pub(in crate::formatter) fn format_attributes_block(&mut self, node: &CstNode) {
        let keyword = match node.kind {
            NodeKind::ExportsBlock => "exports",
            _ => "attributes",
        };
        self.write_indent();
        self.write(&format!("{keyword} {{"));
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

        // Calculate max key length for alignment (only simple-form params)
        let max_key_len = if self.config.align_attributes {
            params
                .iter()
                .filter(|p| {
                    // Only consider simple-form params for alignment
                    !p.children.iter().any(|child| {
                        matches!(child, CstChild::Node(n) if n.kind == NodeKind::ArgumentsParamBlock)
                    })
                })
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
                    && (n.kind == NodeKind::AttributesParam || n.kind == NodeKind::ExportsParam)
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

    pub(in crate::formatter) fn format_arguments_param(&mut self, node: &CstNode, align_to: usize) {
        // Check if this is a block form (has ArgumentsParamBlock child)
        let has_block = node.children.iter().any(
            |child| matches!(child, CstChild::Node(n) if n.kind == NodeKind::ArgumentsParamBlock),
        );

        if has_block {
            self.format_arguments_param_block_form(node);
        } else {
            self.format_arguments_param_simple(node, align_to);
        }
    }

    fn format_arguments_param_simple(&mut self, node: &CstNode, align_to: usize) {
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
                        self.write_token(&token.text);
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
                        self.write_token(&token.text);
                    } else if wrote_equals {
                        // Default value
                        self.write_token(&token.text);
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

    fn format_arguments_param_block_form(&mut self, node: &CstNode) {
        self.write_indent();

        let mut wrote_name = false;
        let mut wrote_colon = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_name && self.is_identifier(&token.text) && token.text != "arguments" {
                        self.write_token(&token.text);
                        wrote_name = true;
                    } else if token.text == ":" && !wrote_colon {
                        self.write(": ");
                        wrote_colon = true;
                    } else if wrote_colon {
                        // Type primitive
                        self.write_token(&token.text);
                    }
                }
                CstChild::Node(n) => {
                    if n.kind == NodeKind::ArgumentsParamBlock {
                        self.format_arguments_param_block(n);
                    } else if wrote_colon {
                        self.format_type_expr(n);
                    }
                }
                CstChild::Trivia(_) => {}
            }
        }

        self.write_newline();
    }

    pub(in crate::formatter) fn format_arguments_param_block(&mut self, node: &CstNode) {
        self.write(" {");
        self.write_newline();
        self.current_indent += 1;

        // Collect attrs and find max key length for alignment
        let attrs: Vec<&CstNode> = node
            .children
            .iter()
            .filter_map(|child| {
                if let CstChild::Node(n) = child
                    && n.kind == NodeKind::ArgumentsParamAttr
                {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();

        let max_key_len = if self.config.align_attributes {
            attrs
                .iter()
                .filter_map(|attr| self.get_arguments_param_attr_key(attr))
                .map(|k| k.len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };

        for attr in &attrs {
            self.format_arguments_param_attr(attr, max_key_len);
        }

        self.current_indent -= 1;
        self.write_indent();
        self.write("}");
    }

    fn get_arguments_param_attr_key(&self, node: &CstNode) -> Option<String> {
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
            {
                return Some(token.text.clone());
            }
        }
        None
    }

    pub(in crate::formatter) fn format_arguments_param_attr(
        &mut self,
        node: &CstNode,
        align_to: usize,
    ) {
        self.write_indent();

        let mut key_len: usize = 0;
        let mut wrote_key = false;
        let mut wrote_equals = false;

        for child in &node.children {
            match child {
                CstChild::Token(token) => {
                    if !wrote_key && self.is_identifier(&token.text) {
                        key_len = token.text.len();
                        self.write_token(&token.text);
                        wrote_key = true;
                    } else if token.text == "=" && !wrote_equals {
                        if align_to > 0 && key_len < align_to {
                            let padding = align_to - key_len;
                            self.write(&" ".repeat(padding));
                        }
                        self.write(" = ");
                        wrote_equals = true;
                    } else if wrote_equals {
                        // Value token (string, number, etc.)
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

    pub(in crate::formatter) fn format_attributes_param(
        &mut self,
        node: &CstNode,
        align_to: usize,
    ) {
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
                        self.write_token(&token.text);
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
                        self.write_token(&token.text);
                    } else if wrote_equals {
                        // Value
                        self.write_token(&token.text);
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

    pub(in crate::formatter) fn format_nested_block(&mut self, node: &CstNode) {
        self.write_indent();

        // Find and write block name (identifier)
        for child in &node.children {
            if let CstChild::Token(token) = child
                && self.is_identifier(&token.text)
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

    pub(in crate::formatter) fn block_has_content(&self, node: &CstNode) -> bool {
        node.children.iter().any(|child| {
            matches!(child, CstChild::Node(n) if n.kind == NodeKind::Attribute || n.kind == NodeKind::NestedBlock || n.kind == NodeKind::LocalBinding)
                || matches!(
                    child,
                    CstChild::Trivia(Trivia::LineComment(_) | Trivia::BlockComment(_))
                )
        })
    }

    /// Emit the ` { ... }` tail (or ` {}` when empty) for block-shaped
    /// expressions. Caller is responsible for writing the keyword or
    /// type prefix; this fn only formats the brace block and its
    /// attributes.
    pub(in crate::formatter) fn format_block_body_tail(&mut self, node: &CstNode) {
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

    pub(in crate::formatter) fn format_block_attributes(&mut self, node: &CstNode) {
        // Collect attributes into groups separated by blank lines.
        // Comments are attached to the next attribute (leading_comments map).
        let mut groups: Vec<Vec<&CstNode>> = Vec::new();
        let mut current_group: Vec<&CstNode> = Vec::new();
        let mut inline_comments: std::collections::HashMap<usize, &Trivia> =
            std::collections::HashMap::new();
        let mut leading_comments: std::collections::HashMap<usize, Vec<&Trivia>> =
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
                    // Attach pending comments to this attribute
                    if !pending_comments.is_empty() {
                        leading_comments.insert(attr_index, std::mem::take(&mut pending_comments));
                    }
                    // Start a new group if there was a blank line
                    if newline_count > 1 && !current_group.is_empty() {
                        groups.push(std::mem::take(&mut current_group));
                    }
                    current_group.push(n);
                    attr_index += 1;
                    newline_count = 0;
                }
                CstChild::Trivia(Trivia::LineComment(s) | Trivia::BlockComment(s)) => {
                    // Check if this is an inline comment (on same line as previous attribute)
                    if attr_index > 0 && !s.is_empty() && newline_count == 0 {
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

        // Post-process groups: split around attributes with map values so they
        // get their own group (with blank lines before/after).
        let groups = self.split_groups_around_map_attributes(groups);

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
                // Write leading comments attached to this attribute
                if let Some(comments) = leading_comments.get(&global_attr_index) {
                    for comment in comments {
                        self.write_indent();
                        self.write_trivia(comment);
                        self.write_newline();
                    }
                }
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

    /// Unwrap transparent expression wrappers (PipeExpr, ComposeExpr) to find the inner node
    pub(in crate::formatter) fn unwrap_expr_wrappers<'a>(&self, node: &'a CstNode) -> &'a CstNode {
        match node.kind {
            NodeKind::PipeExpr | NodeKind::ComposeExpr => {
                for child in &node.children {
                    if let CstChild::Node(n) = child {
                        return self.unwrap_expr_wrappers(n);
                    }
                }
                node
            }
            _ => node,
        }
    }

    /// Check if an attribute has a non-empty map value (block form `{ ... }`)
    fn attribute_has_map_value(&self, node: &CstNode) -> bool {
        if node.kind != NodeKind::Attribute {
            return false;
        }
        if let Some(value_node) = self.get_value_after_equals(node) {
            let unwrapped = self.unwrap_expr_wrappers(value_node);
            let map_node = if unwrapped.kind == NodeKind::Map {
                Some(unwrapped)
            } else {
                None
            };
            // Only return true for non-empty maps (maps with at least one entry or nested block)
            if let Some(map) = map_node {
                return map.children.iter().any(|child| {
                    matches!(child, CstChild::Node(n) if n.kind == NodeKind::MapEntry || n.kind == NodeKind::NestedBlock)
                });
            }
        }
        false
    }

    /// Split groups so that attributes with map values are isolated into their own groups.
    /// This ensures blank lines are inserted before and after map block attributes.
    fn split_groups_around_map_attributes<'a>(
        &self,
        groups: Vec<Vec<&'a CstNode>>,
    ) -> Vec<Vec<&'a CstNode>> {
        let mut result: Vec<Vec<&'a CstNode>> = Vec::new();
        for group in groups {
            let mut current: Vec<&'a CstNode> = Vec::new();
            for attr in group {
                if self.attribute_has_map_value(attr) {
                    // Push any accumulated non-map attributes as their own group
                    if !current.is_empty() {
                        result.push(std::mem::take(&mut current));
                    }
                    // Map attribute gets its own group
                    result.push(vec![attr]);
                } else {
                    current.push(attr);
                }
            }
            if !current.is_empty() {
                result.push(current);
            }
        }
        result
    }

    /// Get the first child node after `=` in any node (Attribute, MapEntry, etc.)
    pub(in crate::formatter) fn get_value_after_equals<'a>(
        &self,
        node: &'a CstNode,
    ) -> Option<&'a CstNode> {
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

    /// Check if a node is a Map, possibly wrapped in PipeExpr/ComposeExpr
    fn is_map_node(&self, node: &CstNode) -> bool {
        self.unwrap_expr_wrappers(node).kind == NodeKind::Map
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

    /// Unwrap a node to get the inner Map, handling PipeExpr/ComposeExpr wrappers
    fn unwrap_to_map<'a>(&self, node: &'a CstNode) -> Option<&'a CstNode> {
        let unwrapped = self.unwrap_expr_wrappers(node);
        if unwrapped.kind == NodeKind::Map {
            Some(unwrapped)
        } else {
            None
        }
    }

    /// Unwrap a node to get the inner List, handling PipeExpr/ComposeExpr wrappers
    fn unwrap_to_list<'a>(&self, node: &'a CstNode) -> Option<&'a CstNode> {
        let unwrapped = self.unwrap_expr_wrappers(node);
        if unwrapped.kind == NodeKind::List {
            Some(unwrapped)
        } else {
            None
        }
    }

    /// Check if an attribute should be converted to block syntax
    pub(in crate::formatter) fn should_convert_to_blocks(&self, node: &CstNode) -> Option<String> {
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
    pub(in crate::formatter) fn should_convert_map_entry_to_blocks(
        &self,
        node: &CstNode,
    ) -> Option<String> {
        self.should_convert_to_blocks_generic(node, |s, n| s.get_map_entry_key(n))
    }

    /// Emit a node's `= [{...}, {...}]` value as multiple `block_name { ... }` blocks.
    /// Works for both Attribute and MapEntry nodes.
    pub(in crate::formatter) fn emit_list_as_blocks(&mut self, node: &CstNode, block_name: &str) {
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

    pub(in crate::formatter) fn format_attribute(&mut self, node: &CstNode, align_to: usize) {
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

        // Write inline comment if present
        if let Some(comment) = inline_comment {
            self.write("  ");
            self.write_trivia(comment);
        }

        self.write_newline();
    }
}
