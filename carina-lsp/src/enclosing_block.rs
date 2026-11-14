use carina_core::parser::ResourceTypePath;
use carina_core::schema::{OwnerBoundResourceSchema, SchemaKind, SchemaRegistry};
use tower_lsp::lsp_types::Position;

use crate::lex::{LexKind, LexedDocument, lex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchemaSelector {
    path: ResourceTypePath,
    kind: SelectorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SelectorKind {
    Syntax(SchemaKind),
    Kindless,
}

impl SchemaSelector {
    /// Handles and Ref/annotation types carry a path but no syntax kind.
    pub(crate) fn kindless(path: ResourceTypePath) -> Self {
        Self {
            path,
            kind: SelectorKind::Kindless,
        }
    }

    pub(crate) fn path(&self) -> &ResourceTypePath {
        &self.path
    }

    pub(crate) fn lookup<'a>(
        &self,
        schemas: &'a SchemaRegistry,
    ) -> Option<OwnerBoundResourceSchema<'a>> {
        if self.path.provider.is_empty() {
            return None;
        }
        match self.kind {
            SelectorKind::Syntax(kind) => {
                schemas.get(&self.path.provider, &self.path.resource_type, kind)
            }
            SelectorKind::Kindless => {
                schemas.get_kindless(&self.path.provider, &self.path.resource_type)
            }
        }
    }
}

pub(crate) struct ResourceHeader {
    pub(crate) selector: SchemaSelector,
    binding: Option<String>,
    is_block_header: bool,
}

fn walk_back_prefix(lexed: &LexedDocument, mut line: usize, prefix: &str) -> (String, usize) {
    let prefix = prefix.trim_end();
    if !prefix.is_empty() {
        return (prefix.to_string(), line);
    }
    let expression_line = line;
    while line > 0 {
        line -= 1;
        let preceding = &lexed.lines()[line];
        let last_significant = preceding
            .text()
            .char_indices()
            .zip(preceding.chars())
            .filter(|((_, ch), classified)| {
                !ch.is_whitespace() && classified.kind() != LexKind::Comment
            })
            .last();
        // Skip only raw blank/comment-only lines. Masked strings and heredocs
        // are statement boundaries, including a literal after `let x =`.
        let Some(((byte, ch), classified)) = last_significant else {
            continue;
        };
        let code = preceding.masked_code();
        let code = code.as_str().trim_end();
        if classified.kind() == LexKind::Code
            && crate::position::char_len(&preceding.text()[..byte + ch.len_utf8()])
                == crate::position::char_len(code)
            && (code.ends_with(['=', ':']) || strip_read(code).is_some())
        {
            return (code.to_string(), line);
        }
        break;
    }
    (String::new(), expression_line)
}

fn strip_read(prefix: &str) -> Option<&str> {
    prefix.strip_suffix("read").filter(|prefix| {
        !prefix.ends_with(|ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.'))
    })
}

pub(crate) struct ResourceExpressionPrefix {
    pub(crate) before_expression: String,
    pub(crate) read_start: Option<Position>,
}

/// Snippet insertion and header selection share the same continuation rule.
pub(crate) fn resource_expression_prefix(
    lexed: &LexedDocument,
    position: Position,
) -> Option<ResourceExpressionPrefix> {
    let line = position.line as usize;
    let code = lexed.lines().get(line)?.masked_code();
    let (prefix, line) = walk_back_prefix(
        lexed,
        line,
        crate::position::line_prefix(code.as_str(), position.character),
    );
    let (before_expression, read_start) = match strip_read(&prefix) {
        Some(before_read) => {
            let read_start = Position::new(line as u32, crate::position::char_len(&prefix) - 4);
            let (prefix, _) = walk_back_prefix(lexed, line, before_read);
            (prefix, Some(read_start))
        }
        None => (prefix, None),
    };
    Some(ResourceExpressionPrefix {
        before_expression,
        read_start,
    })
}

/// Parse the complete type-path token at the cursor (or before a block opener).
/// Only the preceding code token `read` selects a data source. Across physical
/// lines, only raw trivia and explicit continuation tokens may join a header.
/// Schema registration never affects this syntax decision.
pub(crate) fn resource_header(lexed: &LexedDocument, position: Position) -> Option<ResourceHeader> {
    parse_resource_header(lexed, position, false)
}

fn parse_resource_header(
    lexed: &LexedDocument,
    position: Position,
    resource_value_position: bool,
) -> Option<ResourceHeader> {
    let line_index = position.line as usize;
    let code = lexed.lines().get(line_index)?.masked_code();
    let code = code.as_str();
    let is_path_char = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.');
    let offset = crate::position::line_prefix(code, position.character).len();
    let end = offset
        + code[offset..]
            .find(|ch| !is_path_char(ch))
            .unwrap_or(code.len() - offset);
    let before_end = code[..end].trim_end();
    let start = before_end
        .rmatch_indices(|ch| !is_path_char(ch))
        .next()
        .map_or(0, |(index, ch)| index + ch.len());
    let token = &before_end[start..];
    if !token.split('.').all(is_module_identifier) {
        return None;
    }
    let resource_type = ResourceTypePath::parse(token)?;
    let expression_prefix = resource_expression_prefix(
        lexed,
        Position::new(position.line, crate::position::char_len(&code[..start])),
    )?;
    let kind = if expression_prefix.read_start.is_some() {
        SchemaKind::DataSource
    } else {
        SchemaKind::Resource
    };
    let prefix = expression_prefix
        .before_expression
        .rsplit(['{', '}', ','])
        .next()?
        .trim();
    let assignment = parse_let_assignment(prefix).filter(|a| a.expression.is_empty());
    let is_block_header = resource_value_position
        || (prefix.is_empty() || assignment.is_some()) && code[end..].trim_start().starts_with('{');
    // Outside a resource header, a path (including nested annotation types)
    // carries no kind. Only `read` or a managed header supplies a syntax kind.
    let selector = if !is_block_header && kind == SchemaKind::Resource {
        SchemaSelector::kindless(resource_type)
    } else {
        SchemaSelector {
            path: resource_type,
            kind: SelectorKind::Syntax(kind),
        }
    };
    Some(ResourceHeader {
        selector,
        binding: assignment.as_ref().map(|a| a.binding.to_string()),
        is_block_header,
    })
}

/// Scan bindings using the same header seam as hover and enclosing blocks.
/// Only direct resource/read declarations and conditionals whose branch tails
/// are direct declarations supply a selector. Every tail must have the same
/// exact path and kind, regardless of which branch core evaluates. Parentheses,
/// coalescing and nested conditional tails do not preserve a resource selector.
/// Associate selectors with declaration lines before deduplicating names
/// buffer-first. An unsaved scalar binding therefore keeps its `None` selector
/// instead of receiving the selector of a later on-disk declaration.
pub(crate) fn resource_binding_selectors(text: &str) -> Vec<(String, Option<SchemaSelector>)> {
    let lexed = lex(text);
    let tokens = binding_expression_tokens(&lexed);
    let mut seen = std::collections::HashSet::new();
    lexed
        .lines()
        .iter()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let code = line.masked_code();
            let (name, _) = crate::let_parse::parse_let_header(code.as_str())?;
            if !seen.insert(name.to_string()) {
                return None;
            }
            let start = Position::new(
                line_index as u32,
                crate::position::char_len(&code.as_str()[..code.as_str().find('=')? + 1]),
            );
            let mut parser = BindingSelectorParser {
                lexed: &lexed,
                tokens: &tokens,
                cursor: tokens.partition_point(|token| token.position < start),
            };
            let selector = match parser.expression(0, true) {
                Ok(BindingSelector::Resource(selector)) => Some(selector),
                _ => None,
            };
            Some((name.to_string(), selector))
        })
        .collect()
}

struct BindingExpressionToken {
    text: String,
    position: Position,
}

/// Keep literals as opaque operands rather than erasing them into trivia.
/// Comments, interpolation punctuation and heredoc bodies cannot supply headers.
fn binding_expression_tokens(lexed: &LexedDocument) -> Vec<BindingExpressionToken> {
    let mut tokens = Vec::new();
    let mut in_literal = false;
    for (line_index, line) in lexed.lines().iter().enumerate() {
        let position = |column| Position::new(line_index as u32, column);
        let code = line.masked_code();
        let code = code.as_str();
        let mut chars = code.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            if ch.is_whitespace() {
                continue;
            }
            let mut end = start + ch.len_utf8();
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.') {
                while let Some(&(offset, next)) = chars.peek() {
                    if !next.is_ascii_alphanumeric() && !matches!(next, '_' | '.') {
                        break;
                    }
                    chars.next();
                    end = offset + next.len_utf8();
                }
            } else if matches!(
                (ch, chars.peek().map(|(_, ch)| *ch)),
                ('?', Some('?')) | ('|', Some('>')) | ('>', Some('>'))
            ) {
                end = chars.next().unwrap().0 + 1;
            }
            tokens.push(BindingExpressionToken {
                text: code[start..end].to_string(),
                position: position(crate::position::char_len(&code[..start])),
            });
        }
        let heredoc = carina_core::heredoc::find_heredoc_opener_range(line.text());
        if let Some(range) = heredoc {
            tokens.push(BindingExpressionToken {
                text: String::new(),
                position: position(crate::position::char_len(&line.text()[..range.start])),
            });
        }
        let mut column = 0;
        for (ch, classified) in line.text().chars().zip(line.chars()) {
            let literal = !matches!(classified.kind(), LexKind::Code | LexKind::Comment);
            if literal && !in_literal && !line.is_heredoc_body() {
                tokens.push(BindingExpressionToken {
                    text: String::new(),
                    position: position(column),
                });
            }
            in_literal = literal;
            column += ch.len_utf16() as u32;
        }
    }
    tokens.sort_by_key(|token| token.position);
    tokens
}

/// A small syntax walk over the resource-valued RHS forms. Other primaries
/// remain opaque: a resource inside a list, call or map does not type the
/// enclosing value. Incomplete alternatives and conflicting selectors fail
/// closed, independently of whether the rest of the edited file parses.
struct BindingSelectorParser<'a> {
    lexed: &'a LexedDocument,
    tokens: &'a [BindingExpressionToken],
    cursor: usize,
}

#[derive(PartialEq, Eq)]
enum BindingSelector {
    Resource(SchemaSelector),
    NonResource,
    Conflict,
}

impl BindingSelectorParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens
            .get(self.cursor)
            .map(|token| token.text.as_str())
    }

    fn expect(&mut self, text: &str) -> Result<(), ()> {
        if self.peek() != Some(text) {
            return Err(());
        }
        self.cursor += 1;
        Ok(())
    }

    fn expression(&mut self, depth: usize, resource_position: bool) -> Result<BindingSelector, ()> {
        // Edited source must not be able to exhaust the LSP's stack.
        if depth > 128 {
            return Err(());
        }
        let mut selector = self.operand(depth, resource_position)?;
        while self.peek() == Some("??") {
            self.cursor += 1;
            self.operand(depth + 1, false)?;
            selector = BindingSelector::NonResource;
        }
        Ok(selector)
    }

    fn operand(&mut self, depth: usize, resource_position: bool) -> Result<BindingSelector, ()> {
        let mut selector = self.primary(depth, resource_position)?;
        loop {
            match self.peek() {
                Some("+") | Some("-") | Some("*") | Some("/") | Some("%") | Some("|>")
                | Some(">>") => {
                    self.cursor += 1;
                    self.primary(depth + 1, false)?;
                }
                Some("[") => self.group()?,
                Some(field) if field.starts_with('.') => self.cursor += 1,
                _ => break,
            }
            selector = BindingSelector::NonResource;
        }
        Ok(selector)
    }

    fn primary(&mut self, depth: usize, resource_position: bool) -> Result<BindingSelector, ()> {
        let token = self.tokens.get(self.cursor).ok_or(())?;
        match token.text.as_str() {
            "(" => {
                self.cursor += 1;
                self.expression(depth + 1, false)?;
                self.expect(")")?;
                Ok(BindingSelector::NonResource)
            }
            "if" => self.conditional(depth + 1, resource_position),
            "[" | "{" => {
                self.group()?;
                Ok(BindingSelector::NonResource)
            }
            "for" => {
                if !resource_position {
                    return Err(());
                }
                self.body_start()?;
                self.group()?;
                Ok(BindingSelector::NonResource)
            }
            "}" | ")" | "]" | "??" | "let" | "else" => Err(()),
            _ => {
                self.cursor += 1;
                let mut path_position = token.position;
                if token.text == "read" {
                    path_position = self.tokens.get(self.cursor).ok_or(())?.position;
                    self.cursor += 1;
                }
                match self.peek() {
                    Some("{") => {
                        // Conditions and branch-local lets are value positions
                        // in core, so declarations cannot appear there.
                        if !resource_position {
                            return Err(());
                        }
                        let header = parse_resource_header(self.lexed, path_position, true);
                        // Preserve completion for a direct resource declaration
                        // whose body is still being edited. Alternatives must
                        // always have complete bodies.
                        if self.group().is_err() && depth != 0 {
                            return Err(());
                        }
                        Ok(header.map_or(BindingSelector::NonResource, |header| {
                            BindingSelector::Resource(header.selector)
                        }))
                    }
                    Some("(") => {
                        self.group()?;
                        Ok(BindingSelector::NonResource)
                    }
                    _ => Ok(BindingSelector::NonResource),
                }
            }
        }
    }

    fn conditional(
        &mut self,
        depth: usize,
        resource_position: bool,
    ) -> Result<BindingSelector, ()> {
        let condition_start = self.cursor + 1;
        self.body_start()?;
        let mut condition = BindingSelectorParser {
            lexed: self.lexed,
            tokens: &self.tokens[condition_start..self.cursor],
            cursor: 0,
        };
        condition.expression(depth + 1, false)?;
        if condition.cursor != condition.tokens.len() {
            return Err(());
        }
        let selector = self.branch(depth, resource_position)?;
        if self.peek() == Some("else") {
            self.cursor += 1;
            if self.branch(depth, resource_position)? != selector {
                return Ok(BindingSelector::Conflict);
            }
        }
        Ok(selector)
    }

    fn branch(&mut self, depth: usize, resource_position: bool) -> Result<BindingSelector, ()> {
        self.expect("{")?;
        while self.peek() == Some("let") {
            self.cursor += 2; // keyword and local binding name
            self.expect("=")?;
            self.expression(depth + 1, false)?;
        }
        // Core lowers only direct declarations as structural branch tails.
        // Nested value conditionals remain usable inside local value bindings.
        if resource_position && self.peek() == Some("if") {
            return Err(());
        }
        let selector = self.expression(depth + 1, resource_position)?;
        self.expect("}")?;
        Ok(selector)
    }

    fn body_start(&mut self) -> Result<(), ()> {
        self.cursor += 1; // if/for
        let condition_start = self.cursor;
        loop {
            match self.peek().ok_or(())? {
                // Parenthesized conditions, calls and iterable lists can
                // contain braces that do not open the control-flow body.
                "(" | "[" => self.group()?,
                "{" if self.cursor > condition_start => {
                    let opener = self.cursor;
                    self.group()?;
                    // A resource/module-valued condition consumes its own
                    // block before the brace that opens the branch body.
                    if self.peek() != Some("{") {
                        self.cursor = opener;
                        return Ok(());
                    }
                }
                "{" => self.group()?,
                "}" | ")" | "]" | "else" | "let" => return Err(()),
                _ => self.cursor += 1,
            }
        }
    }

    fn group(&mut self) -> Result<(), ()> {
        let mut closers = Vec::new();
        loop {
            let token = self.peek().ok_or(())?;
            match token {
                "(" => closers.push(")"),
                "[" => closers.push("]"),
                "{" => closers.push("}"),
                ")" | "]" | "}" if closers.pop() != Some(token) => return Err(()),
                _ => {}
            }
            self.cursor += 1;
            if closers.is_empty() {
                return Ok(());
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnclosingBlock {
    Resource {
        selector: SchemaSelector,
        struct_path: StructPath,
        binding: Option<String>,
    },
    ModuleCall {
        module_name: String,
        arg_path: Vec<String>,
    },
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StructPath {
    Known(Vec<String>),
    Unresolvable,
}

#[derive(Clone, Copy)]
enum SegmentSyntax {
    Assignment,
    Bare,
    NamedModule,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineOpener {
    Brace,
    Bracket,
    Parenthesis,
}

#[derive(Clone, Copy)]
enum SegmentScanMode {
    BeforeOpener,
    AtCursor,
}

struct LineSegment {
    raw: String,
    code: String,
    block_code: String,
}

enum BraceOpener {
    Resource {
        selector: SchemaSelector,
        binding: Option<String>,
    },
    Path(PathSegment),
    Else,
    ControlFlow,
    Anonymous,
    Other,
}

#[derive(Clone, Copy)]
enum ElseState {
    None,
    AwaitingBranchCloser,
    ReclassifyBranchOpener,
}

#[derive(Clone, Copy)]
enum PendingElement {
    None,
    Unnamed,
}

struct LetAssignment<'a> {
    binding: &'a str,
    expression: &'a str,
}

struct PathSegment {
    name: String,
    syntax: SegmentSyntax,
}

/// Whether the innermost open code brace is a for/if/else expression body.
pub(crate) fn in_control_flow_body(lexed: &LexedDocument, position: Position) -> bool {
    let mut depth = 0;
    for (line_index, line) in lexed
        .lines()
        .iter()
        .enumerate()
        .take(position.line as usize + 1)
        .rev()
    {
        let chars: Vec<_> = line.text().chars().collect();
        let limit = if line_index == position.line as usize {
            crate::position::utf16_offset_to_char_offset(line.text(), position.character)
                .unwrap_or(chars.len())
        } else {
            chars.len()
        };
        for index in (0..limit).rev() {
            if line.chars()[index].kind() != LexKind::Code {
                continue;
            }
            match chars[index] {
                '}' => depth += 1,
                '{' if depth > 0 => depth -= 1,
                '{' => {
                    let segment = scan_line_segment(
                        &chars,
                        line.chars(),
                        index,
                        SegmentScanMode::BeforeOpener,
                    );
                    return is_control_flow_segment(&segment.block_code)
                        || is_else_segment(&segment.code);
                }
                _ => {}
            }
        }
    }
    false
}

/// Classify the block enclosing `position` with one lexer-aware backwards walk.
/// Only ordinary code punctuation is structural: interpolation expressions,
/// strings, comments, and heredoc bodies are deliberately invisible here.
pub(crate) fn enclosing_block(text: &str, position: Position) -> EnclosingBlock {
    let lexed = lex(text);
    let source_lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let current_line = position.line as usize;
    if current_line >= source_lines.len() || current_line >= lexed.lines().len() {
        return EnclosingBlock::Other;
    }

    let mut brace_depth = 0_usize;
    let mut bracket_depth = 0_usize;
    let mut reverse_path = Vec::new();
    let mut pending_element = PendingElement::None;
    let mut else_state = ElseState::None;

    for line_index in (0..=current_line).rev() {
        let source_chars: Vec<char> = source_lines[line_index].chars().collect();
        let classified = lexed.lines()[line_index].chars();
        debug_assert_eq!(source_chars.len(), classified.len());
        let limit = if line_index == current_line {
            crate::position::utf16_offset_to_char_offset(
                source_lines[line_index],
                position.character,
            )
            .unwrap_or(source_chars.len())
        } else {
            source_chars.len()
        };

        for char_index in (0..limit).rev() {
            if classified[char_index].kind() != LexKind::Code {
                continue;
            }

            match source_chars[char_index] {
                '}' => {
                    brace_depth += 1;
                    match else_state {
                        ElseState::None => {}
                        ElseState::AwaitingBranchCloser => {
                            else_state = ElseState::ReclassifyBranchOpener;
                        }
                        ElseState::ReclassifyBranchOpener => {}
                    }
                }
                '{' => {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                        let should_reclassify = match else_state {
                            ElseState::None | ElseState::AwaitingBranchCloser => false,
                            ElseState::ReclassifyBranchOpener => brace_depth == 0,
                        };
                        if !should_reclassify {
                            continue;
                        }
                        else_state = ElseState::None;
                    }

                    let segment = scan_line_segment(
                        &source_chars,
                        classified,
                        char_index,
                        SegmentScanMode::BeforeOpener,
                    );
                    let header = resource_header(
                        &lexed,
                        Position::new(
                            line_index as u32,
                            crate::position::char_offset_to_utf16_offset(
                                source_lines[line_index],
                                char_index,
                            ),
                        ),
                    );
                    match pending_element {
                        PendingElement::None => {}
                        PendingElement::Unnamed => {
                            match classify_brace_opener(&segment, header.as_ref()) {
                                BraceOpener::Path(PathSegment {
                                    name,
                                    syntax: SegmentSyntax::Assignment,
                                }) => {
                                    reverse_path.push(PathSegment {
                                        name,
                                        syntax: SegmentSyntax::Assignment,
                                    });
                                    pending_element = PendingElement::None;
                                    continue;
                                }
                                BraceOpener::Else => {
                                    else_state = ElseState::AwaitingBranchCloser;
                                    continue;
                                }
                                BraceOpener::ControlFlow => continue,
                                BraceOpener::Resource { selector, binding } => {
                                    return EnclosingBlock::Resource {
                                        selector,
                                        struct_path: StructPath::Unresolvable,
                                        binding,
                                    };
                                }
                                BraceOpener::Path(PathSegment {
                                    name: _,
                                    syntax: SegmentSyntax::Bare,
                                })
                                | BraceOpener::Path(PathSegment {
                                    name: _,
                                    syntax: SegmentSyntax::NamedModule,
                                })
                                | BraceOpener::Anonymous
                                | BraceOpener::Other => return EnclosingBlock::Other,
                            }
                        }
                    }
                    match classify_brace_opener(&segment, header.as_ref()) {
                        BraceOpener::Resource { selector, binding } => {
                            return EnclosingBlock::Resource {
                                selector,
                                struct_path: resource_struct_path(reverse_path),
                                binding,
                            };
                        }
                        BraceOpener::Path(path_segment) => {
                            reverse_path.push(path_segment);
                        }
                        BraceOpener::Else => {
                            else_state = ElseState::AwaitingBranchCloser;
                        }
                        BraceOpener::ControlFlow => {}
                        BraceOpener::Anonymous => {
                            // A nameless `{` after `[` or `,` is a transparent
                            // element wrapper. Remember it until the matching
                            // list opener supplies the attribute name.
                            pending_element = PendingElement::Unnamed;
                        }
                        BraceOpener::Other => return EnclosingBlock::Other,
                    }
                }
                ']' => bracket_depth += 1,
                '[' if bracket_depth > 0 => bracket_depth -= 1,
                '[' if brace_depth > 0 => {}
                '[' => match pending_element {
                    PendingElement::None => {}
                    PendingElement::Unnamed => {
                        // A list name is part of a schema path only while the
                        // cursor is inside one of its struct elements. Directly
                        // inside `[value, |]`, the list is a value expression and
                        // must not masquerade as a struct block.
                        let segment = scan_line_segment(
                            &source_chars,
                            classified,
                            char_index,
                            SegmentScanMode::BeforeOpener,
                        );
                        let Some(path_segment) = parse_list_attribute_segment(&segment) else {
                            return EnclosingBlock::Other;
                        };
                        reverse_path.push(path_segment);
                        pending_element = PendingElement::None;
                    }
                },
                _ => {}
            }
        }
    }

    match pending_element {
        PendingElement::None => classify_non_resource(reverse_path),
        PendingElement::Unnamed => EnclosingBlock::Other,
    }
}

fn scan_line_segment(
    source_chars: &[char],
    classified: &[crate::lex::ClassifiedChar],
    limit: usize,
    mode: SegmentScanMode,
) -> LineSegment {
    let mut segment_start = 0;
    let mut block_start = 0;
    let mut openers = Vec::new();

    for index in 0..limit {
        if classified[index].kind() != LexKind::Code {
            continue;
        }
        match source_chars[index] {
            '{' => {
                openers.push(LineOpener::Brace);
                segment_start = index + 1;
                block_start = index + 1;
            }
            '}' => {
                pop_line_opener(&mut openers, LineOpener::Brace);
                segment_start = index + 1;
                block_start = index + 1;
            }
            '[' => {
                openers.push(LineOpener::Bracket);
                match mode {
                    SegmentScanMode::BeforeOpener => segment_start = index + 1,
                    SegmentScanMode::AtCursor => {}
                }
            }
            ']' => pop_line_opener(&mut openers, LineOpener::Bracket),
            '(' => openers.push(LineOpener::Parenthesis),
            ')' => pop_line_opener(&mut openers, LineOpener::Parenthesis),
            ',' => {
                let starts_segment = match (mode, openers.last()) {
                    (SegmentScanMode::BeforeOpener, Some(LineOpener::Parenthesis)) => false,
                    (
                        SegmentScanMode::BeforeOpener,
                        Some(LineOpener::Brace) | Some(LineOpener::Bracket) | None,
                    ) => true,
                    (
                        SegmentScanMode::AtCursor,
                        Some(LineOpener::Bracket) | Some(LineOpener::Parenthesis),
                    ) => false,
                    (SegmentScanMode::AtCursor, Some(LineOpener::Brace) | None) => true,
                };
                if starts_segment {
                    segment_start = index + 1;
                }
            }
            _ => {}
        }
    }

    let raw = source_chars[segment_start..limit].iter().collect();
    let code = masked_code(source_chars, classified, segment_start, limit);
    let block_code = masked_code(source_chars, classified, block_start, limit);
    LineSegment {
        raw,
        code,
        block_code,
    }
}

fn masked_code(
    source_chars: &[char],
    classified: &[crate::lex::ClassifiedChar],
    start: usize,
    limit: usize,
) -> String {
    source_chars[start..limit]
        .iter()
        .zip(&classified[start..limit])
        .map(|(character, classified)| {
            if classified.kind() == LexKind::Code {
                *character
            } else {
                ' '
            }
        })
        .collect()
}

/// Return the current field/value segment on the cursor line.
///
/// Delimiters inside strings, comments, heredocs, or interpolations are
/// ignored. A comma is a field boundary at line/map scope, but remains part
/// of the value while nested in a function call or list literal.
pub(crate) fn cursor_line_segment(text: &str, position: Position) -> String {
    let lexed = lex(text);
    let source_lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let line_index = position.line as usize;
    let Some(source_line) = source_lines.get(line_index) else {
        return String::new();
    };
    let Some(lexed_line) = lexed.lines().get(line_index) else {
        return String::new();
    };
    let source_chars: Vec<char> = source_line.chars().collect();
    let classified = lexed_line.chars();
    debug_assert_eq!(source_chars.len(), classified.len());
    let limit = crate::position::utf16_offset_to_char_offset(source_line, position.character)
        .unwrap_or(source_chars.len());
    scan_line_segment(&source_chars, classified, limit, SegmentScanMode::AtCursor).raw
}

fn pop_line_opener(openers: &mut Vec<LineOpener>, expected: LineOpener) {
    if openers.last() == Some(&expected) {
        openers.pop();
    }
}

fn parse_path_segment(segment: &LineSegment, allow_bare: bool) -> Option<PathSegment> {
    let equals_index = segment.code.chars().position(|character| character == '=');
    let (name, syntax) = match equals_index {
        Some(index)
            if segment
                .code
                .chars()
                .skip(index + 1)
                .collect::<String>()
                .trim_start()
                .starts_with('=') =>
        {
            return None;
        }
        Some(index) => {
            let code_candidate: String = segment.code.chars().take(index).collect();
            let raw_candidate: String = segment.raw.chars().take(index).collect();
            let code_candidate = code_candidate.trim();
            let raw_candidate = raw_candidate.trim();
            let name = if is_identifier(code_candidate) {
                code_candidate.to_string()
            } else if code_candidate.is_empty() && is_string_literal(raw_candidate) {
                raw_candidate.to_string()
            } else {
                return None;
            };
            (name, SegmentSyntax::Assignment)
        }
        None if allow_bare => {
            let candidate = segment.code.trim();
            if !is_identifier(candidate) {
                return None;
            }
            (candidate.to_string(), SegmentSyntax::Bare)
        }
        None => return None,
    };

    Some(PathSegment { name, syntax })
}

fn parse_list_attribute_segment(segment: &LineSegment) -> Option<PathSegment> {
    let (name, value_prefix) = segment.code.split_once('=')?;
    let name = name.trim();
    if !is_identifier(name) || !value_prefix.trim().is_empty() {
        return None;
    }
    Some(PathSegment {
        name: name.to_string(),
        syntax: SegmentSyntax::Assignment,
    })
}

fn classify_brace_opener(segment: &LineSegment, header: Option<&ResourceHeader>) -> BraceOpener {
    if let Some(header) = header.filter(|header| header.is_block_header) {
        BraceOpener::Resource {
            selector: header.selector.clone(),
            binding: header.binding.clone(),
        }
    } else if is_else_segment(&segment.code) {
        BraceOpener::Else
    } else if is_control_flow_segment(&segment.block_code) {
        BraceOpener::ControlFlow
    } else if let Some(module_name) = named_module_call_name(&segment.code) {
        BraceOpener::Path(PathSegment {
            name: module_name.to_string(),
            syntax: SegmentSyntax::NamedModule,
        })
    } else if let Some(path_segment) = parse_path_segment(segment, true) {
        BraceOpener::Path(path_segment)
    } else if segment.code.trim().is_empty() {
        BraceOpener::Anonymous
    } else {
        BraceOpener::Other
    }
}

fn is_else_segment(segment: &str) -> bool {
    let trimmed = segment.trim();
    trimmed == "else" || trimmed.starts_with("else ")
}

fn is_control_flow_segment(segment: &str) -> bool {
    let trimmed = segment.trim();
    let expression = parse_let_assignment(trimmed)
        .map(|assignment| assignment.expression)
        .unwrap_or(trimmed);
    expression == "if"
        || expression.starts_with("if ")
        || expression == "for"
        || expression.starts_with("for ") && expression.contains(" in ")
}

fn parse_let_assignment(segment: &str) -> Option<LetAssignment<'_>> {
    let after_let = strip_keyword(segment, "let")?;
    let (binding, expression) = after_let.split_once('=')?;
    let binding = binding.trim();
    is_identifier(binding).then(|| LetAssignment {
        binding,
        expression: expression.trim_start(),
    })
}

pub(crate) fn named_module_call_name(header: &str) -> Option<&str> {
    let trimmed = header.trim();
    let without_opener = trimmed
        .strip_suffix('{')
        .map(str::trim_end)
        .unwrap_or(trimmed);
    let assignment = parse_let_assignment(without_opener)?;
    let module_name = assignment.expression.trim();
    if matches!(module_name, "use" | "upstream_state" | "read" | "provider") {
        return None;
    }
    is_module_identifier(module_name).then_some(module_name)
}

fn strip_keyword<'a>(segment: &'a str, keyword: &str) -> Option<&'a str> {
    let remainder = segment.strip_prefix(keyword)?;
    remainder
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| remainder.trim_start())
}

fn is_string_literal(candidate: &str) -> bool {
    let mut characters = candidate.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let Some(last) = characters.next_back() else {
        return false;
    };
    match first {
        '\'' => last == '\'',
        '"' => last == '"',
        _ => false,
    }
}

fn is_identifier(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_module_identifier(candidate: &str) -> bool {
    let mut characters = candidate.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn resource_struct_path(mut reverse_path: Vec<PathSegment>) -> StructPath {
    let mut struct_path = Vec::with_capacity(reverse_path.len());
    for segment in reverse_path.drain(..) {
        match segment.syntax {
            SegmentSyntax::Assignment | SegmentSyntax::Bare => struct_path.push(segment.name),
            SegmentSyntax::NamedModule => return StructPath::Unresolvable,
        }
    }
    struct_path.reverse();
    StructPath::Known(struct_path)
}

fn classify_non_resource(mut reverse_path: Vec<PathSegment>) -> EnclosingBlock {
    let Some(outermost) = reverse_path.pop() else {
        return EnclosingBlock::Other;
    };
    let module_name = match outermost.syntax {
        SegmentSyntax::Bare | SegmentSyntax::NamedModule => outermost.name,
        SegmentSyntax::Assignment => return EnclosingBlock::Other,
    };
    if matches!(
        module_name.as_str(),
        "arguments" | "attributes" | "directives" | "exports"
    ) {
        return EnclosingBlock::Other;
    }

    let mut arg_path = Vec::with_capacity(reverse_path.len());
    for segment in reverse_path {
        match segment.syntax {
            SegmentSyntax::Assignment | SegmentSyntax::Bare => arg_path.push(segment.name),
            SegmentSyntax::NamedModule => return EnclosingBlock::Other,
        }
    }
    arg_path.reverse();
    EnclosingBlock::ModuleCall {
        module_name,
        arg_path,
    }
}
