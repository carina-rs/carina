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
                '}' => brace_depth += 1,
                '{' if brace_depth > 0 => brace_depth -= 1,
                '{' => {
                    let segment = code_segment_before(&source_chars, classified, char_index);
                    if let Some(resource_type) = resource_types.find_in(&segment) {
                        let mut struct_path: Vec<String> = reverse_path
                            .into_iter()
                            .map(|segment: PathSegment| segment.name)
                            .collect();
                        struct_path.reverse();
                        return EnclosingBlock::Resource {
                            resource_type: resource_type.to_string(),
                            struct_path,
                        };
                    }

                    if let Some(path_segment) = parse_path_segment(&segment, true) {
                        reverse_path.push(path_segment);
                    } else if !segment.trim().is_empty() {
                        return EnclosingBlock::Other;
                    }
                }
                ']' => bracket_depth += 1,
                '[' if bracket_depth > 0 => bracket_depth -= 1,
                '[' if brace_depth > 0 => {}
                '[' => {
                    let segment = code_segment_before(&source_chars, classified, char_index);
                    let Some(path_segment) = parse_path_segment(&segment, false) else {
                        return EnclosingBlock::Other;
                    };
                    reverse_path.push(path_segment);
                }
                _ => {}
            }
        }
    }

    classify_non_resource(reverse_path)
}

fn code_segment_before(
    source_chars: &[char],
    classified: &[crate::lex::ClassifiedChar],
    opener_index: usize,
) -> String {
    let segment_start = (0..opener_index)
        .rev()
        .find(|index| {
            classified[*index].kind() == LexKind::Code
                && matches!(source_chars[*index], '{' | '}' | '[' | ',')
        })
        .map_or(0, |index| index + 1);

    source_chars[segment_start..opener_index]
        .iter()
        .zip(&classified[segment_start..opener_index])
        .map(|(character, classified)| {
            if classified.kind() == LexKind::Code {
                *character
            } else {
                ' '
            }
        })
        .collect()
}

fn parse_path_segment(segment: &str, allow_bare: bool) -> Option<PathSegment> {
    let trimmed = segment.trim();
    let (candidate, syntax) = match trimmed.strip_suffix('=') {
        Some(before_equals) => (before_equals.trim(), SegmentSyntax::Assignment),
        None if allow_bare => (trimmed, SegmentSyntax::Bare),
        None => return None,
    };

    is_identifier(candidate).then(|| PathSegment {
        name: candidate.to_string(),
        syntax,
    })
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
