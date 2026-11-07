//! Regexes admitted by schema validation, never compiled during value validation.
//!
//! A refined string cannot declare a bare-string pattern.
//!
//! ```compile_fail,E0308
//! use carina_core::schema::AttributeType;
//! let _ = AttributeType::refined_string(None, Some(String::from("abc")), None, None);
//! ```

use std::{
    fmt,
    hash::{Hash, Hasher},
};

use super::{PatternMatch, PatternSpec, PatternSpecDisplay};

use crate::provider_text::{ProviderText, ProviderTextBuilder};
use regex_syntax::ast::{self, Ast};

// Match the regex crate's authored-source budget, checked before any rewrite.
const SOURCE_NEST_LIMIT: u32 = 250;
// ASCII classes can add a bracketed node (Ast::ClassBracketed or
// ClassSetItem::Bracketed) and ClassSetItem::Union. Word-boundary rewrites
// add only one Group on a separate assertion branch, so they do not stack.
const MAX_DIALECT_NEST_OVERHEAD: u32 = 1 + 1;
// The Full envelope \A(?:p)\z adds Group and Concat; its assertions are leaves.
const FULL_ENVELOPE_NEST_OVERHEAD: u32 = 1 + 1;
// Only internal compilation and inspection receive the combined slack.
const COMPILED_NEST_LIMIT: u32 =
    SOURCE_NEST_LIMIT + MAX_DIALECT_NEST_OVERHEAD + FULL_ENVELOPE_NEST_OVERHEAD;

mod sealed {
    pub trait Sealed {}
}

/// Pattern representation carried by a draft or admitted schema tree.
pub trait PatternForm: sealed::Sealed + Copy + fmt::Debug + Eq + Send + Sync + 'static {
    type Pattern: Clone + fmt::Debug + Eq + AsRef<PatternSpec> + Send + Sync;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draft;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compiled;

impl sealed::Sealed for Draft {}
impl sealed::Sealed for Compiled {}

impl PatternForm for Draft {
    type Pattern = PatternSpec;
}

impl PatternForm for Compiled {
    type Pattern = CompiledPattern;
}

/// A regex in Carina's schema-pattern dialect, constructed only by [`Self::try_new`].
///
/// CloudFormation supplies ECMAScript patterns, so `\d`, `\D`, `\w`, `\W`,
/// `\b`, and `\B` have ASCII meaning, including escapes inside bracket classes.
/// Other characters and Unicode properties retain the regex crate's Unicode
/// semantics. `\s` and `\S` retain Rust's Unicode whitespace set, which is
/// approximately ECMAScript's whitespace set; they differ at U+FEFF and U+0085.
///
/// Explicit Unicode mode flags are rejected. Case-insensitive word classes are
/// also rejected, since Unicode case folding could widen their ASCII ranges.
/// Source text is preserved for diagnostics, transport and fingerprints.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    spec: PatternSpec,
    regex: regex::Regex,
}

impl CompiledPattern {
    pub fn try_new(spec: &PatternSpec) -> Result<Self, PatternCompileError> {
        let source = spec.as_str();
        let compile = || {
            let mut ast = ast::parse::ParserBuilder::new()
                .nest_limit(SOURCE_NEST_LIMIT)
                .build()
                .parse(source)
                .map_err(|error| PatternCompileErrorSource::Parse(Box::new(error)))?;
            // Keep compiler diagnostics and their carets tied to the original spelling.
            regex_syntax::ParserBuilder::new()
                .nest_limit(SOURCE_NEST_LIMIT)
                .build()
                .parse(source)
                .map_err(|error| {
                    PatternCompileErrorSource::Regex(regex::Error::Syntax(error.to_string()))
                })?;
            translate_ascii_escapes(&mut ast)?;
            let mut translated = String::new();
            ast::print::Printer::new()
                .print(&ast, &mut translated)
                .expect("printing a regex AST into a String cannot fail");
            let expression = match spec.mode() {
                PatternMatch::Full => format!(r"\A(?:{translated})\z"),
                PatternMatch::Substring { .. } => translated,
            };
            regex::RegexBuilder::new(&expression)
                .nest_limit(COMPILED_NEST_LIMIT)
                .build()
                .map_err(PatternCompileErrorSource::Regex)
        };
        compile()
            .map(|regex| Self {
                spec: spec.clone(),
                regex,
            })
            .map_err(|error| {
                let mut diagnostic = ProviderTextBuilder::default();
                diagnostic.push_str("invalid string pattern ");
                PatternSpecDisplay::new(spec)
                    .quoted()
                    .push_to(&mut diagnostic);
                diagnostic.push_str(&format!(": {error}"));
                PatternCompileError {
                    diagnostic: diagnostic.finish(),
                    spec: spec.clone(),
                    reason: error,
                }
            })
    }

    pub fn as_str(&self) -> &str {
        self.spec.as_str()
    }

    pub fn is_match(&self, value: &str) -> bool {
        self.regex.is_match(value)
    }

    /// Recognize only an absolute whole-string character class repetition.
    /// Inspect the compiler's translated dialect so `\d` and `\w` retain
    /// their ASCII meaning. Substring patterns need authored absolute boundaries;
    /// multiline assertions alone prove nothing.
    pub(crate) fn whole_string_class(
        &self,
    ) -> Option<(super::CharSet, super::length_interval::LengthInterval)> {
        use regex_syntax::hir::{Class, HirKind, Look};
        let hir = regex_syntax::ParserBuilder::new()
            .nest_limit(COMPILED_NEST_LIMIT)
            .build()
            .parse(self.regex.as_str())
            .ok()?;
        let HirKind::Concat(parts) = hir.kind() else {
            return None;
        };
        let mut parts = parts.as_slice();
        // Full adds absolute assertions around authored anchors. Repeated
        // absolute assertions are idempotent; retain all other assertions.
        let original_len = parts.len();
        while matches!(
            parts.first().map(|part| part.kind()),
            Some(HirKind::Look(Look::Start))
        ) {
            parts = &parts[1..];
        }
        let starts_stripped = parts.len() < original_len;
        let before_end_len = parts.len();
        while matches!(
            parts.last().map(|part| part.kind()),
            Some(HirKind::Look(Look::End))
        ) {
            parts = &parts[..parts.len() - 1];
        }
        if !starts_stripped || parts.len() == before_end_len {
            return None;
        }
        let [middle] = parts else {
            return None;
        };
        let (class, min, max) = match middle.kind() {
            HirKind::Repetition(rep) => (&*rep.sub, u64::from(rep.min), rep.max.map(u64::from)),
            _ => (middle, 1, Some(1)),
        };
        let ranges = match class.kind() {
            HirKind::Class(Class::Unicode(class)) => class
                .ranges()
                .iter()
                .map(|r| (r.start(), r.end()))
                .collect(),
            HirKind::Class(Class::Bytes(class)) => class
                .ranges()
                .iter()
                .map(|r| (char::from(r.start()), char::from(r.end())))
                .collect(),
            HirKind::Literal(literal) => {
                let text = std::str::from_utf8(&literal.0).ok()?;
                let mut chars = text.chars();
                let ch = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                vec![(ch, ch)]
            }
            _ => return None,
        };
        Some((
            super::CharSet::new(ranges).ok()?,
            super::length_interval::LengthInterval { min, max },
        ))
    }
}

impl AsRef<PatternSpec> for CompiledPattern {
    fn as_ref(&self) -> &PatternSpec {
        &self.spec
    }
}

impl Hash for CompiledPattern {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.spec.hash(state);
    }
}
impl PartialOrd for CompiledPattern {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CompiledPattern {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.spec.cmp(&other.spec)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{diagnostic}")]
pub struct PatternCompileError {
    spec: PatternSpec,
    /// Typed parser, dialect or regex compiler diagnostic rendered by this error.
    pub reason: PatternCompileErrorSource,
    diagnostic: ProviderText,
}

impl Eq for PatternCompileError {}

impl AsRef<PatternSpec> for PatternCompileError {
    fn as_ref(&self) -> &PatternSpec {
        &self.spec
    }
}

/// The underlying parser, dialect or regex compiler failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PatternCompileErrorSource {
    #[error(transparent)]
    Parse(Box<ast::Error>),
    #[error(transparent)]
    Regex(regex::Error),
    #[error("explicit Unicode mode flags are not supported (line {line}, column {column})")]
    UnicodeFlag { line: usize, column: usize },
    #[error("case-insensitive word classes can widen ASCII ranges (line {line}, column {column})")]
    CaseInsensitiveWordClass { line: usize, column: usize },
}

// Use a heap stack for both the regex and its class-set trees. Flat sequences
// and class-set binary operations can be large without deeply nested groups.
fn translate_ascii_escapes(ast: &mut Ast) -> Result<(), PatternCompileErrorSource> {
    enum Work<'a> {
        Ast(&'a mut Ast),
        Set(&'a mut ast::ClassSet),
        Item(&'a mut ast::ClassSetItem),
        RestoreCase(bool),
    }
    let mut stack = vec![Work::Ast(ast)];
    let mut case_insensitive = false;
    while let Some(work) = stack.pop() {
        match work {
            Work::RestoreCase(previous) => case_insensitive = previous,
            Work::Ast(ast) => match ast {
                Ast::Flags(flags) => update_flags(&flags.flags, &mut case_insensitive)?,
                Ast::ClassPerl(class) => {
                    if let Some(class) = ascii_class(class, case_insensitive)? {
                        *ast = Ast::class_bracketed(class);
                    }
                }
                Ast::Assertion(assertion) => {
                    let word_boundary = match assertion.kind {
                        ast::AssertionKind::StartLine
                        | ast::AssertionKind::EndLine
                        | ast::AssertionKind::StartText
                        | ast::AssertionKind::EndText => false,
                        ast::AssertionKind::WordBoundary
                        | ast::AssertionKind::NotWordBoundary
                        | ast::AssertionKind::WordBoundaryStart
                        | ast::AssertionKind::WordBoundaryEnd
                        | ast::AssertionKind::WordBoundaryStartAngle
                        | ast::AssertionKind::WordBoundaryEndAngle
                        | ast::AssertionKind::WordBoundaryStartHalf
                        | ast::AssertionKind::WordBoundaryEndHalf => true,
                    };
                    if word_boundary {
                        let span = assertion.span;
                        *ast = Ast::group(ast::Group {
                            span,
                            kind: ast::GroupKind::NonCapturing(ast::Flags {
                                span,
                                items: vec![
                                    ast::FlagsItem {
                                        span,
                                        kind: ast::FlagsItemKind::Negation,
                                    },
                                    ast::FlagsItem {
                                        span,
                                        kind: ast::FlagsItemKind::Flag(ast::Flag::Unicode),
                                    },
                                ],
                            }),
                            ast: Box::new(Ast::assertion((**assertion).clone())),
                        });
                    }
                }
                Ast::ClassBracketed(class) => stack.push(Work::Set(&mut class.kind)),
                Ast::Repetition(repetition) => stack.push(Work::Ast(&mut repetition.ast)),
                Ast::Group(group) => {
                    stack.push(Work::RestoreCase(case_insensitive));
                    if let Some(flags) = group.flags() {
                        update_flags(flags, &mut case_insensitive)?;
                    }
                    stack.push(Work::Ast(&mut group.ast));
                }
                Ast::Alternation(alternation) => {
                    stack.extend(alternation.asts.iter_mut().rev().map(Work::Ast))
                }
                Ast::Concat(concat) => stack.extend(concat.asts.iter_mut().rev().map(Work::Ast)),
                Ast::Empty(_) | Ast::Literal(_) | Ast::Dot(_) | Ast::ClassUnicode(_) => {}
            },
            Work::Set(set) => match set {
                ast::ClassSet::Item(item) => stack.push(Work::Item(item)),
                ast::ClassSet::BinaryOp(op) => {
                    stack.push(Work::Set(&mut op.rhs));
                    stack.push(Work::Set(&mut op.lhs));
                }
            },
            Work::Item(item) => match item {
                ast::ClassSetItem::Perl(class) => {
                    if let Some(class) = ascii_class(class, case_insensitive)? {
                        *item = ast::ClassSetItem::Bracketed(Box::new(class));
                    }
                }
                ast::ClassSetItem::Bracketed(class) => stack.push(Work::Set(&mut class.kind)),
                ast::ClassSetItem::Union(union) => {
                    stack.extend(union.items.iter_mut().rev().map(Work::Item))
                }
                ast::ClassSetItem::Empty(_)
                | ast::ClassSetItem::Literal(_)
                | ast::ClassSetItem::Range(_)
                | ast::ClassSetItem::Ascii(_)
                | ast::ClassSetItem::Unicode(_) => {}
            },
        }
    }
    Ok(())
}

fn update_flags(
    flags: &ast::Flags,
    case_insensitive: &mut bool,
) -> Result<(), PatternCompileErrorSource> {
    if flags.flag_state(ast::Flag::Unicode).is_some() {
        return Err(PatternCompileErrorSource::UnicodeFlag {
            line: flags.span.start.line,
            column: flags.span.start.column,
        });
    }
    if let Some(enabled) = flags.flag_state(ast::Flag::CaseInsensitive) {
        *case_insensitive = enabled;
    }
    Ok(())
}

fn ascii_class(
    class: &ast::ClassPerl,
    case_insensitive: bool,
) -> Result<Option<ast::ClassBracketed>, PatternCompileErrorSource> {
    let ranges: &[(char, char)] = match class.kind {
        ast::ClassPerlKind::Digit => &[('0', '9')],
        ast::ClassPerlKind::Word if case_insensitive => {
            return Err(PatternCompileErrorSource::CaseInsensitiveWordClass {
                line: class.span.start.line,
                column: class.span.start.column,
            });
        }
        ast::ClassPerlKind::Word => &[('0', '9'), ('A', 'Z'), ('a', 'z'), ('_', '_')],
        // Retain Rust's Unicode \s/\S, approximately ECMAScript's whitespace set.
        ast::ClassPerlKind::Space => return Ok(None),
    };
    let span = class.span;
    let literal = |c| ast::Literal {
        span,
        kind: ast::LiteralKind::Verbatim,
        c,
    };
    let items = ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                ast::ClassSetItem::Literal(literal(start))
            } else {
                ast::ClassSetItem::Range(ast::ClassSetRange {
                    span,
                    start: literal(start),
                    end: literal(end),
                })
            }
        })
        .collect();
    Ok(Some(ast::ClassBracketed {
        span,
        negated: class.negated,
        kind: ast::ClassSet::union(ast::ClassSetUnion { span, items }),
    }))
}

impl fmt::Display for CompiledPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&PatternSpecDisplay::new(&self.spec), formatter)
    }
}

impl PartialEq for CompiledPattern {
    fn eq(&self, other: &Self) -> bool {
        self.spec == other.spec
    }
}

impl Eq for CompiledPattern {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_match_is_absolute_and_preserves_alternation() {
        for (source, accepted, rejected) in [
            ("a|ab", "ab", "xab"),
            ("a|b$", "a", "ax"),
            ("", "", "x"),
            ("(?m)^a$", "a", "a\n"),
            ("a", "a", "a\n"),
            ("a", "a", "a\r\n"),
        ] {
            let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap();
            assert!(pattern.is_match(accepted), "{source:?}: {accepted:?}");
            assert!(!pattern.is_match(rejected), "{source:?}: {rejected:?}");
        }
    }

    #[test]
    fn ascii_digit_and_word_escapes_reject_unicode() {
        for (source, ascii, unicode) in [
            (r"^\d+$", "0123", "١٢"),
            (r"^[\d]+$", "0123", "١٢"),
            (r"^[^\D]+$", "0123", "١٢"),
            (r"^[\d.]+$", "12.3", "١.٢"),
            (r"^\w+$", "Az_09", "é"),
            (r"^[\w]+$", "Az_09", "中"),
            (r"^[^\W]+$", "Az_09", "١"),
            (r"^[^\W-]+$", "Az_09", "\u{301}"),
            (r"^[\w&&[^_]]+$", "Az09", "é"),
            (r"^[\w--[0-9]]+$", "Az_", "é"),
        ] {
            let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap();
            assert!(pattern.is_match(ascii), "{source:?}: {ascii:?}");
            assert!(!pattern.is_match(unicode), "{source:?}: {unicode:?}");
        }
    }

    #[test]
    fn ascii_complement_classes_accept_unicode() {
        for (source, ascii, excluded, unicode) in [
            (r"^\D+$", "abc", "123", "١٢"),
            (r"^[\D]+$", "abc", "123", "١٢"),
            (r"^[^\d]+$", "abc", "123", "١٢"),
            (r"^\W+$", "-!", "Az_09", "é中١"),
            (r"^[\W]+$", "-!", "Az_09", "é中١"),
            (r"^[^\w-]+$", ".!", "Az_09-", "é中١"),
        ] {
            let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap();
            assert!(pattern.is_match(ascii), "{source:?}: {ascii:?}");
            assert!(!pattern.is_match(excluded), "{source:?}: {excluded:?}");
            assert!(pattern.is_match(unicode), "{source:?}: {unicode:?}");
        }
    }

    #[test]
    fn word_boundaries_use_ascii_word_characters() {
        for (source, input, expected) in [
            (r"\bA\b", "éAé", true),
            (r"\bA\b", " A ", true),
            (r"\bé\b", "é", false),
            (r"\Bé\B", "é", true),
            (r"A\Bé", "Aé", false),
            (r"^\b\w+\b$", "Az_09", true),
            (r"^\b\w+\b$", "é", false),
        ] {
            let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Dialect equivalence test intentionally uses substring semantics",
                    )
                    .unwrap(),
                },
            ))
            .unwrap();
            assert_eq!(pattern.is_match(input), expected, "{source:?}: {input:?}");
        }
    }

    #[test]
    fn unicode_text_properties_and_whitespace_are_preserved() {
        for (source, input) in [
            (r"^.+$", "é١中"),
            (r"^[^a]+$", "é١中"),
            (r"^é中$", "é中"),
            (r"^\p{Greek}+$", "αβ"),
            (r"^\s+$", "\u{a0}\u{2003}"),
            (r"^[\s]+$", "\u{a0}\u{2003}"),
            (r"^\S+$", "é١中"),
            (r"^\\d$", r"\d"),
            (r"(?x)^\d+ \. \w+$ # preserved semantics", "12.a"),
            (r"(?i:a)\w", "Az"),
            (r"(?i:\d)", "1"),
        ] {
            assert!(
                CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                    source,
                    crate::schema::PatternMatch::Full
                ))
                .unwrap()
                .is_match(input),
                "{source:?}"
            );
        }
        assert!(
            !CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                r"(?i:a)\w",
                crate::schema::PatternMatch::Full
            ))
            .unwrap()
            .is_match("Aé")
        );
    }

    #[test]
    fn pattern_source_text_is_preserved() {
        let source = r"^\d{2}[\w.-]\b$";
        let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            source,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap();
        assert_eq!(pattern.as_str(), source);
        assert_eq!(pattern.to_string(), format!("/{source}/ (Full)"));
        assert_eq!(pattern.as_ref().as_str(), source);
        assert_eq!(pattern, pattern.clone());
        assert!(pattern.is_match("12a"));
    }

    #[test]
    fn provider_text_pattern_error_diagnostic_is_sanitized_and_bounded() {
        let raw = format!("\x1b[2J{}[", "x".repeat(20 * 1024));
        let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            &raw,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap_err();
        assert!(!error.to_string().contains('\x1b'));
        assert!(error.to_string().contains("truncated,"));
        assert!(error.to_string().len() <= crate::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn pattern_errors_preserve_the_original_source() {
        let source = r"^\d{2}[$";
        let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            source,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap_err();
        assert_eq!(error.as_ref().as_str(), source);
        assert!(
            error
                .reason
                .to_string()
                .contains("unclosed character class")
        );
        assert!(
            matches!(error.reason, PatternCompileErrorSource::Parse(ref error) if error.pattern() == source)
        );
        let source = r"^\d\p{NotAProperty}$";
        let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            source,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap_err();
        assert_eq!(error.as_ref().as_str(), source);
        assert!(matches!(error.reason, PatternCompileErrorSource::Regex(_)));
        assert!(error.reason.to_string().contains("Unicode property"));
    }

    #[test]
    fn pattern_compile_diagnostic_uses_original_spelling() {
        let source = r"^\d\p{NotAProperty}$";
        let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            source,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap_err();
        let expected = regex::Regex::new(source).unwrap_err();
        assert_eq!(error.as_ref().as_str(), source);
        assert_eq!(error.reason.to_string(), expected.to_string());
    }

    #[test]
    fn pattern_errors_are_terminal_diagnostics() {
        for pattern in [r"(?=x)", r"^\d\p{NotAProperty}$", r"(?u:\d)"] {
            let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                pattern,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap_err();
            let rendered = error.to_string();
            let diagnostic = error.reason.to_string();
            assert_eq!(rendered.matches(&diagnostic).count(), 1, "{rendered}");
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn interacting_flags_are_rejected_explicitly() {
        for source in [r"(?u)\d", r"(?-u:\w)", r"(?u:é)"] {
            let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap_err();
            assert_eq!(error.as_ref().as_str(), source);
            assert!(error.reason.to_string().contains("Unicode"));
        }
        for source in [r"(?i:\w)", r"(?i)[^\w-]", r"(?i)(a|\W)"] {
            let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Full,
            ))
            .unwrap_err();
            assert_eq!(error.as_ref().as_str(), source);
            assert!(error.reason.to_string().contains("case-insensitive"));
        }
        assert!(
            CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                r"(?i:(?-i:\w))",
                crate::schema::PatternMatch::Full
            ))
            .unwrap()
            .is_match("A")
        );
    }

    #[test]
    fn handwritten_core_patterns_preserve_behavior() {
        // Existing valid declaration/test patterns inventoried before this dialect
        // change. Compare Unicode probes too: their explicit ASCII ranges and
        // Unicode whitespace must keep their established behavior.
        let patterns = [
            (r#"^.*$"#, crate::schema::PatternMatch::Full),
            (r#"^.+$"#, crate::schema::PatternMatch::Full),
            (
                r#"^AWSReservedSSO_AdministratorAccess_[0-9a-f]{16}$"#,
                crate::schema::PatternMatch::Full,
            ),
            (r#"^Z+$"#, crate::schema::PatternMatch::Full),
            (r#"^Z[A-Z0-9]+$"#, crate::schema::PatternMatch::Full),
            (r#"^[0-9]+$"#, crate::schema::PatternMatch::Full),
            (r#"^[A-Za-z0-9.-]+$"#, crate::schema::PatternMatch::Full),
            (
                r#"^[a-zA-Z0-9=:#@/\-,.][a-zA-Z0-9+=:#@/\-,.\s]+[a-zA-Z0-9+=:#@/\-,.]{1,256}$"#,
                crate::schema::PatternMatch::Full,
            ),
            (r#"^[a-z]+$"#, crate::schema::PatternMatch::Full),
            (r#"^a*$"#, crate::schema::PatternMatch::Full),
            (r#"^a+$"#, crate::schema::PatternMatch::Full),
            (r#"^accepted$"#, crate::schema::PatternMatch::Full),
            (r#"^allowed$"#, crate::schema::PatternMatch::Full),
            (
                r#"^arn:(aws|aws-cn|aws-us-gov):[^:]+:.+$"#,
                crate::schema::PatternMatch::Full,
            ),
            (
                r#"^arn:(aws|aws-cn|aws-us-gov):s3:::.+$"#,
                crate::schema::PatternMatch::Full,
            ),
            (r#"^base$"#, crate::schema::PatternMatch::Full),
            (r#"^base-[a-z]+$"#, crate::schema::PatternMatch::Full),
            (r#"^foo$"#, crate::schema::PatternMatch::Full),
            (
                r#"^good-"#,
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Legacy fixture deliberately constrains only a prefix",
                    )
                    .unwrap(),
                },
            ),
            (r#"^good-[a-z]+$"#, crate::schema::PatternMatch::Full),
            (r#"^module-target$"#, crate::schema::PatternMatch::Full),
            (r#"^outer$"#, crate::schema::PatternMatch::Full),
            (r#"^own$"#, crate::schema::PatternMatch::Full),
            (r#"^raw_dsl$"#, crate::schema::PatternMatch::Full),
            (r#"^sibling$"#, crate::schema::PatternMatch::Full),
            (r#"^valid$"#, crate::schema::PatternMatch::Full),
            (r#"^vpc-[a-f0-9]+$"#, crate::schema::PatternMatch::Full),
            (r#"^x$"#, crate::schema::PatternMatch::Full),
            (r#"^x+$"#, crate::schema::PatternMatch::Full),
            (r#"^y$"#, crate::schema::PatternMatch::Full),
        ];
        let probes = [
            "",
            "a",
            "ABC",
            "123",
            "-",
            "foo",
            "vpc-12abc",
            "ZABC1",
            "arn:aws:s3:::bucket",
            "arn:aws-cn:sns:region:123:a",
            "good-item",
            "AWSReservedSSO_AdministratorAccess_0123456789abcdef",
            "allowed",
            "raw_dsl",
            "α",
            "é",
            "١",
            "\u{a0}",
            "a\u{2003}b",
            "(bad)",
        ];
        for (source, mode) in patterns {
            let original = regex::Regex::new(source).unwrap();
            let translated =
                CompiledPattern::try_new(&crate::schema::PatternSpec::new(source, mode)).unwrap();
            assert_eq!(translated.as_str(), source);
            for input in probes {
                assert_eq!(
                    original.is_match(input),
                    translated.is_match(input),
                    "{source:?}: {input:?}"
                );
            }
        }
    }

    #[test]
    fn builtin_type_helpers_keep_their_validation() {
        use crate::resource::{ConcreteValue, Value};
        use crate::schema::{AttributeType, RawShape, ValidatedConfigSchema, types};
        use std::collections::BTreeMap;

        let string = |s: &str| -> Value { Value::Concrete(ConcreteValue::String(s.into())) };
        let cases = [
            (
                types::positive_int as fn() -> AttributeType,
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(0)),
            ),
            (
                types::ipv4_cidr,
                string("10.0.0.0/16"),
                string("10.0.0.0/33"),
            ),
            (
                types::ipv4_address,
                string("192.168.0.1"),
                string("256.0.0.1"),
            ),
            (
                types::ipv6_address,
                string("2001:db8::1"),
                string("2001:gggg::1"),
            ),
            (
                types::ipv6_cidr,
                string("2001:db8::/32"),
                string("2001:db8::/129"),
            ),
            (types::cidr, string("2001:db8::/32"), string("not-a-cidr")),
            (
                types::email,
                string("user@example.com"),
                string("no-at-sign.com"),
            ),
            (
                types::http_response_status_code,
                string("200"),
                string("301"),
            ),
            (types::redirect_protocol, string("HTTPS"), string("FTP")),
            (
                types::redirect_host,
                string("example.com"),
                string("#{path}"),
            ),
            (types::redirect_port, string("443"), string("0")),
            (types::redirect_path, string("/path"), string("relative")),
            (
                types::redirect_query,
                string("q=#{query}"),
                string("#{bad}"),
            ),
        ];
        for (constructor, valid, invalid) in cases {
            let draft = constructor();
            let mut nodes = vec![&draft];
            while let Some(node) = nodes.pop() {
                match node.raw_shape() {
                    RawShape::String { pattern, .. } => assert!(pattern.is_none()),
                    RawShape::Union(members) => nodes.extend(members),
                    RawShape::Int { .. } => {}
                    other => panic!("unexpected builtin shape: {other:?}"),
                }
            }
            let admitted =
                ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), draft)])).unwrap();
            let ty = &admitted.parts()["value"];
            assert!(ty.validate(&valid).is_ok(), "{}: {valid:?}", ty.type_name());
            assert!(
                ty.validate(&invalid).is_err(),
                "{}: {invalid:?}",
                ty.type_name()
            );
        }
    }

    #[test]
    fn ascii_inputs_match_plain_regex_equivalently() {
        let patterns = [
            r"\d",
            r"\D",
            r"\w",
            r"\W",
            r"\b",
            r"\B",
            r"\b{start}",
            r"\b{end}",
            r"\<",
            r"\>",
            r"\b{start-half}",
            r"\b{end-half}",
            r"^\d{1,3}$",
            r"^[\d.]+$",
            r"^[^\D]+$",
            r"^[^\d]+$",
            r"^[^\w-]+$",
            r"^[\w.-]+$",
            r"^[^\W]+$",
            r"[\D\w]",
            r"\b\w+\b",
            r"\B\w{1,2}\B",
            r"(?:\d+|\w?)-\D*",
            r"^(\w|\d){0,2}$",
            r"^a\b|\B_+$",
            r"[\w&&[^_]]",
            r"[\w--[0-9]]",
            r"\s+\S?",
            r"(?m)^\d+$",
            r"(?i:a)\d",
            r"(?x)^\w+ \. \d+$ # comment",
            r"^\\d$",
        ];
        let mut inputs = (0u8..=127)
            .map(|c| char::from(c).to_string())
            .collect::<Vec<_>>();
        inputs.push(String::new());
        let alphabet = b"aZ07_-. \n\t/";
        for &a in alphabet {
            for &b in alphabet {
                inputs.push(String::from_utf8(vec![a, b]).unwrap());
                for &c in alphabet {
                    inputs.push(String::from_utf8(vec![a, b, c]).unwrap());
                }
            }
        }
        for source in patterns {
            let original = regex::Regex::new(source).unwrap();
            let translated = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                source,
                crate::schema::PatternMatch::Substring {
                    reason: crate::schema::PatternReason::new(
                        "Dialect equivalence test intentionally uses substring semantics",
                    )
                    .unwrap(),
                },
            ))
            .unwrap();
            for input in &inputs {
                assert_eq!(
                    original.is_match(input),
                    translated.is_match(input),
                    "{source:?}: {input:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod explicit_mode_tests {
    use super::*;

    #[test]
    fn substring_preserves_source_and_deliberately_accepts_containing_values() {
        let spec = PatternSpec::new(
            "a|ab",
            PatternMatch::Substring {
                reason: super::super::PatternReason::new(
                    "Upstream explicitly uses substring semantics",
                )
                .unwrap(),
            },
        );
        let pattern = CompiledPattern::try_new(&spec).unwrap();
        assert_eq!(pattern.as_str(), "a|ab");
        assert_eq!(pattern.as_ref(), &spec);
        assert!(pattern.is_match("xabx"));
        assert!(pattern.to_string().contains("Substring"));
        assert!(
            pattern
                .to_string()
                .contains("Upstream explicitly uses substring semantics")
        );
    }

    #[test]
    fn full_extended_mode_trailing_comments_and_flags_stay_inside_boundaries() {
        for source in [
            "(?x)a # trailing comment",
            "(?x)a # trailing comment\n",
            "(?m)a$",
            "(?m)a|b$",
        ] {
            let pattern =
                CompiledPattern::try_new(&PatternSpec::new(source, PatternMatch::Full)).unwrap();
            assert!(pattern.is_match("a"), "{source}");
            assert!(!pattern.is_match("a\n"), "{source}");
            assert!(!pattern.is_match("xa"), "{source}");
            assert_eq!(pattern.as_str(), source);
        }
    }
}

#[cfg(test)]
mod wrapper_budget_tests {
    use super::*;
    use crate::schema::PatternReason;

    #[test]
    fn full_wrapper_preserves_the_authored_nesting_budget() {
        for (leaf, depth, value) in [
            ("a", 250, "a"),
            (r"\bA\b", 248, "A"),
            (r"\bA\b", 249, "A"),
            (r"\B-\B", 249, "-"),
            (r"\w", 250, "A"),
            (r"\W", 250, "-"),
            (r"\d", 250, "7"),
            (r"\D", 250, "A"),
            (r"[\w]", 249, "A"),
            (r"[\W]", 249, "-"),
            (r"[\d]", 249, "7"),
            (r"[\D]", 249, "A"),
            (r"[a\w]", 248, "A"),
            (r"[a\W]", 248, "-"),
            (r"[\w&&\w]", 248, "A"),
            (r"\b\w\b", 249, "A"),
        ] {
            let source = format!("{}{leaf}{}", "(".repeat(depth), ")".repeat(depth));
            regex::Regex::new(&source).expect("authored source fits the regex nesting budget");
            let deeper_source = format!("({source})");
            let deeper_source_is_valid = regex::Regex::new(&deeper_source).is_ok();
            for mode in [
                PatternMatch::Full,
                PatternMatch::Substring {
                    reason: PatternReason::new("Nesting-budget parity probe").unwrap(),
                },
            ] {
                let pattern = CompiledPattern::try_new(&PatternSpec::new(&source, mode.clone()))
                    .unwrap_or_else(|error| {
                        panic!("{leaf:?}, {depth} groups: internal rewrites must not consume the authored budget: {error}")
                    });
                assert!(pattern.is_match(value));
                assert_eq!(pattern.as_str(), source);
                let deeper = CompiledPattern::try_new(&PatternSpec::new(&deeper_source, mode));
                assert_eq!(
                    deeper.is_ok(),
                    deeper_source_is_valid,
                    "{leaf:?}, {} groups: both modes must obey the authored-source budget",
                    depth + 1
                );
                if !deeper_source_is_valid {
                    let error = deeper.unwrap_err();
                    assert_eq!(error.as_ref().as_str(), deeper_source);
                    assert!(matches!(
                        &error.reason,
                        PatternCompileErrorSource::Parse(parse)
                            if matches!(parse.kind(), ast::ErrorKind::NestLimitExceeded(SOURCE_NEST_LIMIT))
                    ));
                }
            }
        }
    }

    #[test]
    fn both_modes_keep_compiler_size_limits_and_source_diagnostics() {
        let source = "[a-z]{1000000}";
        for mode in [
            PatternMatch::Full,
            PatternMatch::Substring {
                reason: PatternReason::new("Compiler-budget parity probe").unwrap(),
            },
        ] {
            let error = CompiledPattern::try_new(&PatternSpec::new(source, mode)).unwrap_err();
            assert_eq!(error.as_ref().as_str(), source);
            assert!(matches!(
                error.reason,
                PatternCompileErrorSource::Regex(regex::Error::CompiledTooBig(_))
            ));
        }
    }
}

#[cfg(test)]
mod compile_error_spec_tests {
    use super::*;
    use crate::schema::PatternReason;

    #[test]
    fn compile_errors_preserve_mode_and_reason_in_identity_and_diagnostics() {
        let specs = [
            PatternSpec::new("[", PatternMatch::Full),
            PatternSpec::new(
                "[",
                PatternMatch::Substring {
                    reason: PatternReason::new("first substring contract").unwrap(),
                },
            ),
            PatternSpec::new(
                "[",
                PatternMatch::Substring {
                    reason: PatternReason::new("second substring contract").unwrap(),
                },
            ),
        ];
        let errors: Vec<_> = specs
            .iter()
            .map(|spec| CompiledPattern::try_new(spec).unwrap_err())
            .collect();
        for (i, error) in errors.iter().enumerate() {
            assert_eq!(error.as_ref(), &specs[i]);
            assert_eq!(error.as_ref().as_str(), "[");
            assert!(
                error
                    .to_string()
                    .contains(&PatternSpecDisplay::new(&specs[i]).mode_only().to_string())
            );
            for (j, other) in errors.iter().enumerate() {
                assert_eq!(error == other, i == j);
            }
        }
    }
}
