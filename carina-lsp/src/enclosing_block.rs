use carina_core::schema::SchemaRegistry;
use tower_lsp::lsp_types::Position;

use crate::lex::{LexKind, lex};

/// Schema keys in the only ordering that source matching may use.
///
/// Keeping the sorted, deduplicated list behind this type prevents hover and
/// completion from rebuilding subtly different resource-header matchers.
pub(crate) struct ResourceTypePatterns(Vec<String>);

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
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnclosingBlock {
    Resource {
        resource_type: String,
        struct_path: Vec<String>,
    },
    ModuleCall {
        module_name: String,
        arg_path: Vec<String>,
    },
    Other,
}

#[derive(Clone, Copy)]
enum SegmentSyntax {
    Assignment,
    Bare,
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
}

enum BraceOpener {
    Resource(String),
    Path(PathSegment),
    Else,
    ControlFlow,
    Anonymous,
    Other,
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
    let mut crossed_list_element = false;
    let mut awaiting_previous_branch_closer = false;
    let mut reclassify_previous_branch_opener = false;

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
                    if awaiting_previous_branch_closer {
                        awaiting_previous_branch_closer = false;
                        reclassify_previous_branch_opener = true;
                    }
                }
                '{' => {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                        if !reclassify_previous_branch_opener || brace_depth != 0 {
                            continue;
                        }
                        reclassify_previous_branch_opener = false;
                    }

                    let segment = scan_line_segment(
                        &source_chars,
                        classified,
                        char_index,
                        SegmentScanMode::BeforeOpener,
                    );
                    match classify_brace_opener(&segment, resource_types) {
                        BraceOpener::Resource(resource_type) => {
                            let mut struct_path: Vec<String> = reverse_path
                                .into_iter()
                                .map(|segment: PathSegment| segment.name)
                                .collect();
                            struct_path.reverse();
                            return EnclosingBlock::Resource {
                                resource_type,
                                struct_path,
                            };
                        }
                        BraceOpener::Path(path_segment) => {
                            reverse_path.push(path_segment);
                            crossed_list_element = false;
                        }
                        BraceOpener::Else => awaiting_previous_branch_closer = true,
                        BraceOpener::ControlFlow => {}
                        BraceOpener::Anonymous => {
                            // A nameless `{` after `[` or `,` is a transparent
                            // element wrapper. Remember it until the matching
                            // list opener supplies the attribute name.
                            crossed_list_element = true;
                        }
                        BraceOpener::Other => return EnclosingBlock::Other,
                    }
                }
                ']' => bracket_depth += 1,
                '[' if bracket_depth > 0 => bracket_depth -= 1,
                '[' if brace_depth > 0 => {}
                '[' if crossed_list_element => {
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
                    if let Some(path_segment) = parse_path_segment(&segment, false) {
                        reverse_path.push(path_segment);
                        crossed_list_element = false;
                    }
                }
                _ => {}
            }
        }
    }

    classify_non_resource(reverse_path)
}

fn scan_line_segment(
    source_chars: &[char],
    classified: &[crate::lex::ClassifiedChar],
    limit: usize,
    mode: SegmentScanMode,
) -> LineSegment {
    let mut segment_start = 0;
    let mut openers = Vec::new();

    for index in 0..limit {
        if classified[index].kind() != LexKind::Code {
            continue;
        }
        match source_chars[index] {
            '{' => {
                openers.push(LineOpener::Brace);
                segment_start = index + 1;
            }
            '}' => {
                pop_line_opener(&mut openers, LineOpener::Brace);
                segment_start = index + 1;
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
    let code = source_chars[segment_start..limit]
        .iter()
        .zip(&classified[segment_start..limit])
        .map(|(character, classified)| {
            if classified.kind() == LexKind::Code {
                *character
            } else {
                ' '
            }
        })
        .collect();
    LineSegment { raw, code }
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

fn classify_brace_opener(
    segment: &LineSegment,
    resource_types: &ResourceTypePatterns,
) -> BraceOpener {
    if let Some(resource_type) = resource_types.find_in(&segment.code) {
        BraceOpener::Resource(resource_type.to_string())
    } else if is_else_segment(&segment.code) {
        BraceOpener::Else
    } else if is_control_flow_segment(&segment.code) {
        BraceOpener::ControlFlow
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
    trimmed == "if"
        || trimmed.starts_with("if ")
        || trimmed == "for"
        || trimmed.starts_with("for ") && trimmed.contains(" in ")
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

fn classify_non_resource(mut reverse_path: Vec<PathSegment>) -> EnclosingBlock {
    let Some(outermost) = reverse_path.pop() else {
        return EnclosingBlock::Other;
    };
    let SegmentSyntax::Bare = outermost.syntax else {
        return EnclosingBlock::Other;
    };
    if matches!(
        outermost.name.as_str(),
        "arguments" | "attributes" | "directives" | "exports"
    ) {
        return EnclosingBlock::Other;
    }

    let mut arg_path: Vec<String> = reverse_path
        .into_iter()
        .map(|segment| segment.name)
        .collect();
    arg_path.reverse();
    EnclosingBlock::ModuleCall {
        module_name: outermost.name,
        arg_path,
    }
}
