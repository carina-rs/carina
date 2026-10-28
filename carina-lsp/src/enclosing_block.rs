use carina_core::schema::SchemaRegistry;
use tower_lsp::lsp_types::Position;

use crate::lex::{LexKind, lex};

/// Schema keys in the only ordering that source matching may use.
///
/// Keeping the sorted, deduplicated list behind this type prevents hover and
/// completion from rebuilding subtly different resource-header matchers.
pub(crate) struct ResourceTypePatterns(Vec<String>);

struct ResourceHeader<'a> {
    resource_type: &'a str,
    binding: Option<String>,
}

impl ResourceTypePatterns {
    pub(crate) fn from_schemas(schemas: &SchemaRegistry) -> Self {
        let mut patterns: Vec<String> = schemas
            .iter()
            .map(|(provider, resource_type, _kind, _schema)| {
                if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{provider}.{resource_type}")
                }
            })
            .collect();
        patterns.sort();
        patterns.dedup();
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
        Self(patterns)
    }

    pub(crate) fn find_in<'a>(&'a self, segment: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|pattern| segment.contains(pattern.as_str()))
            .map(String::as_str)
    }

    fn resource_header<'a>(&'a self, segment: &str) -> Option<ResourceHeader<'a>> {
        let trimmed = segment.trim();
        let assignment = parse_let_assignment(trimmed);
        let (binding, after_binding) = match assignment {
            Some(assignment) => (Some(assignment.binding.to_string()), assignment.expression),
            None => (None, trimmed),
        };
        let candidate = strip_keyword(after_binding, "read")
            .unwrap_or(after_binding)
            .trim();

        self.0
            .iter()
            .find(|pattern| candidate == pattern.as_str())
            .map(|pattern| ResourceHeader {
                resource_type: pattern,
                binding,
            })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnclosingBlock {
    Resource {
        resource_type: String,
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
        resource_type: String,
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

/// Classify the block enclosing `position` with one lexer-aware backwards walk.
/// Only ordinary code punctuation is structural: interpolation expressions,
/// strings, comments, and heredoc bodies are deliberately invisible here.
pub(crate) fn enclosing_block(
    text: &str,
    position: Position,
    resource_types: &ResourceTypePatterns,
) -> EnclosingBlock {
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
            (position.character as usize).min(source_chars.len())
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
                    match pending_element {
                        PendingElement::None => {}
                        PendingElement::Unnamed => {
                            match classify_brace_opener(&segment, resource_types) {
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
                                BraceOpener::Resource {
                                    resource_type,
                                    binding,
                                } => {
                                    return EnclosingBlock::Resource {
                                        resource_type,
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
                    match classify_brace_opener(&segment, resource_types) {
                        BraceOpener::Resource {
                            resource_type,
                            binding,
                        } => {
                            return EnclosingBlock::Resource {
                                resource_type,
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
    let limit = (position.character as usize).min(source_chars.len());
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

fn classify_brace_opener(
    segment: &LineSegment,
    resource_types: &ResourceTypePatterns,
) -> BraceOpener {
    if let Some(header) = resource_types.resource_header(&segment.code) {
        BraceOpener::Resource {
            resource_type: header.resource_type.to_string(),
            binding: header.binding,
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
