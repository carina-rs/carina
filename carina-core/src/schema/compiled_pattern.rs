//! Regexes admitted by schema validation, never compiled during value validation.

use std::{fmt, ops::Deref};

use regex_syntax::ast::{self, Ast};

mod sealed {
    pub trait Sealed {}
}

/// Pattern representation carried by a draft or admitted schema tree.
pub trait PatternForm: sealed::Sealed + Copy + fmt::Debug + Eq + Send + Sync + 'static {
    type Pattern: Clone + fmt::Debug + Eq + Deref<Target = str> + Send + Sync;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Draft;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compiled;

impl sealed::Sealed for Draft {}
impl sealed::Sealed for Compiled {}

impl PatternForm for Draft {
    type Pattern = String;
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
    source: String,
    regex: regex::Regex,
}

impl CompiledPattern {
    pub fn try_new(source: &str) -> Result<Self, PatternCompileError> {
        let compile = || {
            let mut ast = ast::parse::Parser::new()
                .parse(source)
                .map_err(|error| PatternCompileErrorSource::Parse(Box::new(error)))?;
            // Keep compiler diagnostics and their carets tied to the original spelling.
            regex::Regex::new(source).map_err(PatternCompileErrorSource::Regex)?;
            translate_ascii_escapes(&mut ast)?;
            let mut translated = String::new();
            ast::print::Printer::new()
                .print(&ast, &mut translated)
                .expect("printing a regex AST into a String cannot fail");
            regex::Regex::new(&translated).map_err(PatternCompileErrorSource::Regex)
        };
        compile()
            .map(|regex| Self {
                source: source.to_string(),
                regex,
            })
            .map_err(|error| PatternCompileError {
                pattern: source.to_string(),
                source: error,
            })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn is_match(&self, value: &str) -> bool {
        self.regex.is_match(value)
    }
}

impl Deref for CompiledPattern {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("invalid string pattern {pattern:?}")]
pub struct PatternCompileError {
    pub pattern: String,
    #[source]
    pub source: PatternCompileErrorSource,
}

impl Eq for PatternCompileError {}

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
        formatter.write_str(self.as_str())
    }
}

impl PartialEq for CompiledPattern {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CompiledPattern {}

#[cfg(test)]
mod tests {
    use super::*;

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
            let pattern = CompiledPattern::try_new(source).unwrap();
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
            let pattern = CompiledPattern::try_new(source).unwrap();
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
            let pattern = CompiledPattern::try_new(source).unwrap();
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
                CompiledPattern::try_new(source).unwrap().is_match(input),
                "{source:?}"
            );
        }
        assert!(
            !CompiledPattern::try_new(r"(?i:a)\w")
                .unwrap()
                .is_match("Aé")
        );
    }

    #[test]
    fn pattern_source_text_is_preserved() {
        let source = r"^\d{2}[\w.-]\b$";
        let pattern = CompiledPattern::try_new(source).unwrap();
        assert_eq!(pattern.as_str(), source);
        assert_eq!(pattern.to_string(), source);
        assert_eq!(&*pattern, source);
        assert_eq!(pattern, pattern.clone());
        assert!(pattern.is_match("12a"));
    }

    #[test]
    fn pattern_errors_preserve_the_original_source() {
        let source = r"^\d{2}[$";
        let error = CompiledPattern::try_new(source).unwrap_err();
        assert_eq!(error.pattern, source);
        assert!(
            error
                .source
                .to_string()
                .contains("unclosed character class")
        );
        assert!(
            matches!(error.source, PatternCompileErrorSource::Parse(ref error) if error.pattern() == source)
        );
        let source = r"^\d\p{NotAProperty}$";
        let error = CompiledPattern::try_new(source).unwrap_err();
        assert_eq!(error.pattern, source);
        assert!(matches!(error.source, PatternCompileErrorSource::Regex(_)));
        assert!(error.source.to_string().contains("Unicode property"));
    }

    #[test]
    fn pattern_compile_diagnostic_uses_original_spelling() {
        let source = r"^\d\p{NotAProperty}$";
        let error = CompiledPattern::try_new(source).unwrap_err();
        let expected = regex::Regex::new(source).unwrap_err();
        assert_eq!(error.pattern, source);
        assert_eq!(error.source.to_string(), expected.to_string());
    }

    #[test]
    fn interacting_flags_are_rejected_explicitly() {
        for source in [r"(?u)\d", r"(?-u:\w)", r"(?u:é)"] {
            let error = CompiledPattern::try_new(source).unwrap_err();
            assert_eq!(error.pattern, source);
            assert!(error.source.to_string().contains("Unicode"));
        }
        for source in [r"(?i:\w)", r"(?i)[^\w-]", r"(?i)(a|\W)"] {
            let error = CompiledPattern::try_new(source).unwrap_err();
            assert_eq!(error.pattern, source);
            assert!(error.source.to_string().contains("case-insensitive"));
        }
        assert!(
            CompiledPattern::try_new(r"(?i:(?-i:\w))")
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
            r#"^.*$"#,
            r#"^.+$"#,
            r#"^AWSReservedSSO_AdministratorAccess_[0-9a-f]{16}$"#,
            r#"^Z+$"#,
            r#"^Z[A-Z0-9]+$"#,
            r#"^[0-9]+$"#,
            r#"^[A-Za-z0-9.-]+$"#,
            r#"^[a-zA-Z0-9=:#@/\-,.][a-zA-Z0-9+=:#@/\-,.\s]+[a-zA-Z0-9+=:#@/\-,.]{1,256}$"#,
            r#"^[a-z]+$"#,
            r#"^a*$"#,
            r#"^a+$"#,
            r#"^accepted$"#,
            r#"^allowed$"#,
            r#"^arn:(aws|aws-cn|aws-us-gov):[^:]+:.+$"#,
            r#"^arn:(aws|aws-cn|aws-us-gov):s3:::.+$"#,
            r#"^base$"#,
            r#"^base-[a-z]+$"#,
            r#"^foo$"#,
            r#"^good-"#,
            r#"^good-[a-z]+$"#,
            r#"^module-target$"#,
            r#"^outer$"#,
            r#"^own$"#,
            r#"^raw_dsl$"#,
            r#"^sibling$"#,
            r#"^valid$"#,
            r#"^vpc-[a-f0-9]+$"#,
            r#"^x$"#,
            r#"^x+$"#,
            r#"^y$"#,
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
        for source in patterns {
            let original = regex::Regex::new(source).unwrap();
            let translated = CompiledPattern::try_new(source).unwrap();
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
            let translated = CompiledPattern::try_new(source).unwrap();
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
