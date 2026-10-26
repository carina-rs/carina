//! Shared source lexer for LSP features that need string and interpolation
//! boundaries even when the parser cannot produce an AST for an edited file.
//!
//! Heredoc discovery deliberately delegates to `carina_core::heredoc` on each
//! physical line. The parser's preprocessor is line-based, so a carried string
//! or interpolation state must not suppress a heredoc opener it recognizes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LexKind {
    Code,
    DoubleQuotedString,
    SingleQuotedString,
    InterpolationCode,
    Comment,
    HeredocBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClassifiedChar {
    kind: LexKind,
    interpolation_depth: usize,
    interpolation_delimiter: bool,
}

impl ClassifiedChar {
    pub(crate) fn kind(self) -> LexKind {
        self.kind
    }

    pub(crate) fn is_inside_interpolation(self) -> bool {
        self.interpolation_depth > 0
    }

    pub(crate) fn is_interpolation_delimiter(self) -> bool {
        self.interpolation_delimiter
    }

    pub(crate) fn is_code_visible(self) -> bool {
        matches!(self.kind, LexKind::Code | LexKind::InterpolationCode)
    }
}

#[derive(Debug)]
pub(crate) struct LexedLine {
    text: String,
    chars: Vec<ClassifiedChar>,
    heredoc_body: bool,
}

/// Code text with every lexer-classified non-code character replaced by one
/// space. The private field keeps semantic-token walkers from accepting raw
/// source text by accident.
pub(crate) struct MaskedCode(String);

impl MaskedCode {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl LexedLine {
    pub(crate) fn chars(&self) -> &[ClassifiedChar] {
        &self.chars
    }

    pub(crate) fn is_heredoc_body(&self) -> bool {
        self.heredoc_body
    }

    /// Replace every non-code character with one space. This preserves source
    /// character positions (the unit used by LSP ranges), though replacing a
    /// non-ASCII character intentionally does not preserve UTF-8 byte offsets.
    pub(crate) fn masked_code(&self) -> MaskedCode {
        MaskedCode(
            self.text
                .chars()
                .zip(&self.chars)
                .map(|(ch, classified)| {
                    if classified.is_code_visible() {
                        ch
                    } else {
                        ' '
                    }
                })
                .collect(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourcePosition {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InterpolationSpan {
    pub(crate) start: SourcePosition,
    pub(crate) end: Option<SourcePosition>,
    only_inline_whitespace: bool,
}

impl InterpolationSpan {
    pub(crate) fn is_inline_empty(self) -> bool {
        self.end.is_some() && self.only_inline_whitespace
    }
}

#[derive(Debug)]
pub(crate) struct LexedDocument {
    lines: Vec<LexedLine>,
    interpolations: Vec<InterpolationSpan>,
}

impl LexedDocument {
    pub(crate) fn lines(&self) -> &[LexedLine] {
        &self.lines
    }

    pub(crate) fn interpolations(&self) -> &[InterpolationSpan] {
        &self.interpolations
    }

    pub(crate) fn last_classified_char(&self) -> Option<ClassifiedChar> {
        self.lines.last()?.chars.last().copied()
    }
}

#[derive(Clone, Copy, Debug)]
enum Frame {
    DoubleQuotedString,
    SingleQuotedString,
    Interpolation {
        brace_depth: usize,
        span_index: usize,
        delimiter_kind: LexKind,
    },
    BlockComment {
        depth: usize,
    },
    HeredocBody,
}

#[derive(Default)]
struct LexState {
    frames: Vec<Frame>,
    interpolation_depth: usize,
}

impl LexState {
    fn heredoc_body() -> Self {
        let mut state = Self::default();
        state.push(Frame::HeredocBody);
        state
    }

    fn last(&self) -> Option<&Frame> {
        self.frames.last()
    }

    fn last_mut(&mut self) -> Option<&mut Frame> {
        self.frames.last_mut()
    }

    fn frames(&self) -> &[Frame] {
        &self.frames
    }

    fn push(&mut self, frame: Frame) {
        if matches!(frame, Frame::Interpolation { .. }) {
            self.interpolation_depth += 1;
        }
        self.frames.push(frame);
    }

    fn pop(&mut self) -> Option<Frame> {
        let frame = self.frames.pop()?;
        if matches!(frame, Frame::Interpolation { .. }) {
            self.interpolation_depth -= 1;
        }
        Some(frame)
    }

    /// Parser preprocessing turns each heredoc body newline into the two
    /// characters `\\n`, so an interpolation cannot continue onto the next
    /// physical body line. Discard any unfinished nested state at that edge.
    fn finish_heredoc_line(&mut self) {
        while self.frames.len() > 1 {
            self.pop();
        }
        debug_assert!(matches!(self.last(), Some(Frame::HeredocBody)));
        debug_assert_eq!(self.interpolation_depth, 0);
    }
}

struct HeredocState {
    marker: String,
    quoted: bool,
    state: LexState,
}

impl HeredocState {
    fn new(marker: String, quoted: bool) -> Self {
        Self {
            marker,
            quoted,
            state: LexState::heredoc_body(),
        }
    }
}

pub(crate) fn lex(text: &str) -> LexedDocument {
    let mut lines = Vec::new();
    let mut interpolations = Vec::new();
    let mut state = LexState::default();
    let mut heredoc: Option<HeredocState> = None;

    for (line_index, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if let Some(active) = heredoc.as_mut() {
            let is_terminator = line.trim() == active.marker;
            let lexed_line = if active.quoted || is_terminator {
                uniform_heredoc_line(line)
            } else {
                let lexed_line = scan_line(
                    line,
                    line_index as u32,
                    &mut active.state,
                    &mut interpolations,
                    true,
                );
                active.state.finish_heredoc_line();
                lexed_line
            };
            lines.push(lexed_line);

            if is_terminator {
                heredoc = None;
            }
            continue;
        }

        // This must run regardless of carried lexical state. It intentionally
        // mirrors the parser preprocessor's per-line discovery behavior.
        let heredoc_start = carina_core::heredoc::find_heredoc_marker_with_quoted(line);
        let lexed_line = scan_line(
            line,
            line_index as u32,
            &mut state,
            &mut interpolations,
            false,
        );
        lines.push(lexed_line);
        mark_open_interpolations_multiline(state.frames(), &mut interpolations);

        if let Some((marker, quoted)) = heredoc_start {
            heredoc = Some(HeredocState::new(marker, quoted));
        }
    }

    LexedDocument {
        lines,
        interpolations,
    }
}

fn uniform_heredoc_line(line: &str) -> LexedLine {
    LexedLine {
        text: line.to_string(),
        chars: line
            .chars()
            .map(|_| ClassifiedChar {
                kind: LexKind::HeredocBody,
                interpolation_depth: 0,
                interpolation_delimiter: false,
            })
            .collect(),
        heredoc_body: true,
    }
}

fn scan_line(
    line: &str,
    line_index: u32,
    state: &mut LexState,
    interpolations: &mut Vec<InterpolationSpan>,
    heredoc_body: bool,
) -> LexedLine {
    let source_chars: Vec<char> = line.chars().collect();
    let mut chars = vec![classified(LexKind::Code, state, false); source_chars.len()];
    let mut i = 0usize;

    while i < source_chars.len() {
        match state.last().copied() {
            None => {
                if is_line_comment_start(&source_chars, i) {
                    fill_from(&mut chars, i, LexKind::Comment, state);
                    break;
                }
                if starts_with(&source_chars, i, '/', '*') {
                    set_pair(&mut chars, i, LexKind::Comment, state);
                    state.push(Frame::BlockComment { depth: 1 });
                    i += 2;
                    continue;
                }
                if source_chars[i] == '"' {
                    chars[i] = classified(LexKind::DoubleQuotedString, state, false);
                    state.push(Frame::DoubleQuotedString);
                } else if source_chars[i] == '\'' {
                    chars[i] = classified(LexKind::SingleQuotedString, state, false);
                    state.push(Frame::SingleQuotedString);
                } else {
                    chars[i] = classified(LexKind::Code, state, false);
                }
                i += 1;
            }
            Some(Frame::DoubleQuotedString) => {
                let kind = LexKind::DoubleQuotedString;
                chars[i] = classified(kind, state, false);
                if source_chars[i] == '\\' && i + 1 < source_chars.len() {
                    chars[i + 1] = classified(kind, state, false);
                    i += 2;
                    continue;
                }
                if starts_with(&source_chars, i, '$', '{') {
                    start_interpolation(&mut chars, i, line_index, state, interpolations, kind);
                    i += 2;
                    continue;
                }
                if source_chars[i] == '"' {
                    state.pop();
                }
                i += 1;
            }
            Some(Frame::SingleQuotedString) => {
                let kind = LexKind::SingleQuotedString;
                chars[i] = classified(kind, state, false);
                if source_chars[i] == '\\' && i + 1 < source_chars.len() {
                    chars[i + 1] = classified(kind, state, false);
                    i += 2;
                    continue;
                }
                if source_chars[i] == '\'' {
                    state.pop();
                }
                i += 1;
            }
            Some(Frame::HeredocBody) => {
                let kind = LexKind::HeredocBody;
                chars[i] = classified(kind, state, false);
                if starts_with(&source_chars, i, '$', '{') {
                    start_interpolation(&mut chars, i, line_index, state, interpolations, kind);
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Some(Frame::Interpolation {
                brace_depth,
                span_index,
                delimiter_kind,
            }) => {
                if source_chars[i] == ' ' || source_chars[i] == '\t' {
                    chars[i] = classified(LexKind::InterpolationCode, state, false);
                    i += 1;
                    continue;
                }
                if is_line_comment_start(&source_chars, i) {
                    mark_interpolation_nonempty(span_index, interpolations);
                    fill_from(&mut chars, i, LexKind::Comment, state);
                    break;
                }
                if starts_with(&source_chars, i, '/', '*') {
                    mark_interpolation_nonempty(span_index, interpolations);
                    set_pair(&mut chars, i, LexKind::Comment, state);
                    state.push(Frame::BlockComment { depth: 1 });
                    i += 2;
                    continue;
                }
                if source_chars[i] == '"' {
                    mark_interpolation_nonempty(span_index, interpolations);
                    chars[i] = classified(LexKind::DoubleQuotedString, state, false);
                    state.push(Frame::DoubleQuotedString);
                    i += 1;
                    continue;
                }
                if source_chars[i] == '\'' {
                    mark_interpolation_nonempty(span_index, interpolations);
                    chars[i] = classified(LexKind::SingleQuotedString, state, false);
                    state.push(Frame::SingleQuotedString);
                    i += 1;
                    continue;
                }
                if source_chars[i] == '{' {
                    mark_interpolation_nonempty(span_index, interpolations);
                    chars[i] = classified(LexKind::InterpolationCode, state, false);
                    if let Some(Frame::Interpolation { brace_depth, .. }) = state.last_mut() {
                        *brace_depth += 1;
                    }
                    i += 1;
                    continue;
                }
                if source_chars[i] == '}' {
                    if brace_depth == 1 {
                        chars[i] = classified(delimiter_kind, state, true);
                        interpolations[span_index].end = Some(SourcePosition {
                            line: line_index,
                            character: i as u32 + 1,
                        });
                        state.pop();
                    } else {
                        mark_interpolation_nonempty(span_index, interpolations);
                        chars[i] = classified(LexKind::InterpolationCode, state, false);
                        if let Some(Frame::Interpolation { brace_depth, .. }) = state.last_mut() {
                            *brace_depth -= 1;
                        }
                    }
                    i += 1;
                    continue;
                }
                mark_interpolation_nonempty(span_index, interpolations);
                chars[i] = classified(LexKind::InterpolationCode, state, false);
                i += 1;
            }
            Some(Frame::BlockComment { depth }) => {
                chars[i] = classified(LexKind::Comment, state, false);
                if starts_with(&source_chars, i, '/', '*') {
                    chars[i + 1] = classified(LexKind::Comment, state, false);
                    if let Some(Frame::BlockComment { depth }) = state.last_mut() {
                        *depth += 1;
                    }
                    i += 2;
                    continue;
                }
                if starts_with(&source_chars, i, '*', '/') {
                    chars[i + 1] = classified(LexKind::Comment, state, false);
                    if depth == 1 {
                        state.pop();
                    } else if let Some(Frame::BlockComment { depth }) = state.last_mut() {
                        *depth -= 1;
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
        }
    }

    LexedLine {
        text: line.to_string(),
        chars,
        heredoc_body,
    }
}

fn start_interpolation(
    chars: &mut [ClassifiedChar],
    index: usize,
    line: u32,
    state: &mut LexState,
    interpolations: &mut Vec<InterpolationSpan>,
    delimiter_kind: LexKind,
) {
    let span_index = interpolations.len();
    interpolations.push(InterpolationSpan {
        start: SourcePosition {
            line,
            character: index as u32,
        },
        end: None,
        only_inline_whitespace: true,
    });
    state.push(Frame::Interpolation {
        brace_depth: 1,
        span_index,
        delimiter_kind,
    });
    chars[index] = classified(delimiter_kind, state, true);
    chars[index + 1] = classified(delimiter_kind, state, true);
}

fn classified(kind: LexKind, state: &LexState, interpolation_delimiter: bool) -> ClassifiedChar {
    #[cfg(test)]
    record_interpolation_depth_work(1);
    ClassifiedChar {
        kind,
        interpolation_depth: state.interpolation_depth,
        interpolation_delimiter,
    }
}

#[cfg(test)]
std::thread_local! {
    static INTERPOLATION_DEPTH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_interpolation_depth_work(units: usize) {
    INTERPOLATION_DEPTH_WORK.set(INTERPOLATION_DEPTH_WORK.get() + units);
}

#[cfg(test)]
fn reset_interpolation_depth_work() {
    INTERPOLATION_DEPTH_WORK.set(0);
}

#[cfg(test)]
fn interpolation_depth_work() -> usize {
    INTERPOLATION_DEPTH_WORK.get()
}

fn set_pair(chars: &mut [ClassifiedChar], index: usize, kind: LexKind, state: &LexState) {
    chars[index] = classified(kind, state, false);
    chars[index + 1] = classified(kind, state, false);
}

fn fill_from(chars: &mut [ClassifiedChar], index: usize, kind: LexKind, state: &LexState) {
    let classified = classified(kind, state, false);
    chars[index..].fill(classified);
}

fn starts_with(chars: &[char], index: usize, first: char, second: char) -> bool {
    chars.get(index) == Some(&first) && chars.get(index + 1) == Some(&second)
}

fn is_line_comment_start(chars: &[char], index: usize) -> bool {
    chars.get(index) == Some(&'#') || starts_with(chars, index, '/', '/')
}

fn mark_interpolation_nonempty(span_index: usize, interpolations: &mut [InterpolationSpan]) {
    interpolations[span_index].only_inline_whitespace = false;
}

fn mark_open_interpolations_multiline(frames: &[Frame], interpolations: &mut [InterpolationSpan]) {
    for frame in frames {
        if let Frame::Interpolation { span_index, .. } = frame {
            interpolations[*span_index].only_inline_whitespace = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heredoc_detection_matches_parser_preprocessor_across_carried_state() {
        for input in [
            "let value = \"first\nfake = <<EOT\n\"\nEOT\n\"",
            "let value = \"x-${join('-',\n  [<<EOT\nhello\nEOT\n  ])}\"",
        ] {
            let preprocessed = carina_core::heredoc::preprocess_heredocs(input)
                .expect("the parser preprocessor must recognize the heredoc");
            assert_eq!(preprocessed.heredocs.len(), 1, "input: {input:?}");

            let lexed = lex(input);
            assert!(
                lexed.lines().iter().any(LexedLine::is_heredoc_body),
                "the LSP lexer must recognize the same heredoc: {input:?}"
            );
        }
    }

    #[test]
    fn heredoc_body_punctuation_does_not_leak_lexical_state() {
        for body in ["it's", "rm -rf /tmp/*"] {
            let input = format!("let note = <<EOT\n{body}\nEOT\nlet value = \"${{}}\"");
            let lexed = lex(&input);
            let empty: Vec<_> = lexed
                .interpolations()
                .iter()
                .copied()
                .filter(|span| span.is_inline_empty())
                .collect();

            assert_eq!(empty.len(), 1, "body: {body:?}");
            assert_eq!(empty[0].start.line, 3, "body: {body:?}");
        }
    }

    #[test]
    fn deeply_nested_interpolation_depth_tracking_is_linear() {
        let depth = 20_000;
        let mut input = String::from("\"");
        input.push_str(&"${\"".repeat(depth));
        input.push('x');
        input.push_str(&"\"}".repeat(depth));
        input.push('"');

        reset_interpolation_depth_work();
        let lexed = lex(&input);
        let work = interpolation_depth_work();
        let character_count = input.chars().count();

        assert_eq!(lexed.lines().len(), 1);
        assert!(
            work <= character_count * 4,
            "interpolation-depth work must be linear: {work} units for {character_count} chars"
        );
    }
}
