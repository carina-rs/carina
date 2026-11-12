//! The single classifier for source owned by intrinsic secrets.

use super::Rule;
use pest::iterators::Pair;
use std::ops::Range;

/// A protected subtree and the only token a diagnostic may use as its anchor.
#[derive(Debug, Clone)]
pub(crate) struct SecretOwnedRange {
    owned: Range<usize>,
    anchor: Range<usize>,
}

impl SecretOwnedRange {
    pub(crate) fn owned(&self) -> Range<usize> {
        self.owned.clone()
    }

    pub(crate) fn anchor(&self) -> Range<usize> {
        self.anchor.clone()
    }
}

#[derive(Debug, Clone, Default)]
/// Ordered, disjoint outermost secret-owned ranges in a parsed file.
pub(crate) struct SecretOwnedRanges(Vec<SecretOwnedRange>);

impl SecretOwnedRanges {
    pub(super) fn from_pairs(pairs: pest::iterators::Pairs<'_, Rule>) -> Self {
        Self(pairs.flat_map(|pair| Self::from_pair(pair).0).collect())
    }

    pub(crate) fn from_pair(pair: Pair<'_, Rule>) -> Self {
        fn anchor(pair: &Pair<'_, Rule>) -> Option<Range<usize>> {
            match pair.as_rule() {
                Rule::function_call | Rule::validate_function_call => pair
                    .clone()
                    .into_inner()
                    .next()
                    .filter(|name| super::grammar::is_secret_identifier(name.as_str()))
                    .map(|name| name.as_span().start()..name.as_span().end()),
                _ => None,
            }
        }

        fn end(pair: &Pair<'_, Rule>) -> usize {
            pair.as_span().start() + pair.as_str().trim_end().len()
        }

        fn collect(pair: Pair<'_, Rule>, regions: &mut Vec<SecretOwnedRange>) {
            if let Some(anchor) = anchor(&pair) {
                regions.push(SecretOwnedRange {
                    owned: pair.as_span().start()..end(&pair),
                    anchor,
                });
                return;
            }
            if pair.as_rule() == Rule::pipe_expr {
                let parts = pair.clone().into_inner().collect::<Vec<_>>();
                if let Some((index, anchor)) = parts
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, part)| anchor(part).map(|anchor| (index, anchor)))
                {
                    regions.push(SecretOwnedRange {
                        owned: pair.as_span().start()..end(&parts[index]),
                        anchor,
                    });
                    for child in parts.into_iter().skip(index + 1) {
                        collect(child, regions);
                    }
                    return;
                }
            }
            for child in pair.into_inner() {
                collect(child, regions);
            }
        }

        let mut regions = Vec::new();
        collect(pair, &mut regions);
        Self(regions)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &SecretOwnedRange> {
        self.0.iter()
    }

    pub(super) fn owns(&self, candidate: Range<usize>) -> bool {
        self.owner(candidate.clone()).is_some_and(|region| {
            region.owned.start <= candidate.start && candidate.end <= region.owned.end
        })
    }

    pub(crate) fn owner(&self, candidate: Range<usize>) -> Option<&SecretOwnedRange> {
        let index = self
            .0
            .partition_point(|region| region.owned.end <= candidate.start);
        self.0
            .get(index)
            .filter(|region| region.owned.start < candidate.end)
    }

    pub(super) fn original_owner(
        &self,
        candidate: Range<usize>,
        file: &super::PreprocessedFile,
    ) -> Option<&SecretOwnedRange> {
        let index = self
            .0
            .partition_point(|region| file.original_byte(region.owned.end) <= candidate.start);
        self.0.get(index).filter(|region| {
            file.original_byte(region.owned.start) <= candidate.start
                && candidate.end <= file.original_byte(region.owned.end)
        })
    }
}

/// Syntax-attested text. Failed protected files have no text-bearing arm.
pub struct DiagnosticFileText<'a> {
    kind: DiagnosticTextKind<'a>,
}
enum DiagnosticTextKind<'a> {
    Parsed {
        source: &'a str,
        protected: SecretOwnedRanges,
    },
    Failed(super::FailedFileText<'a>),
}
impl<'a> DiagnosticFileText<'a> {
    pub fn new(source: &'a str) -> Self {
        let parsed = crate::heredoc::preprocess_heredocs_raw(source)
            .ok()
            .map(super::PreprocessedFile::from_preprocessed)
            .and_then(|file| {
                let pairs = file.syntax_for_secret_classification()?;
                let mut protected = SecretOwnedRanges(
                    pairs
                        .flat_map(|pair| SecretOwnedRanges::from_pair(pair).0)
                        .collect(),
                );
                for region in &mut protected.0 {
                    region.owned = file.original_byte(region.owned.start)
                        ..file.original_byte(region.owned.end);
                    region.anchor = file.original_byte(region.anchor.start)
                        ..file.original_byte(region.anchor.end);
                }
                Some(protected)
            });
        Self {
            kind: match parsed {
                Some(protected) => DiagnosticTextKind::Parsed { source, protected },
                None => DiagnosticTextKind::Failed(super::failed_file_text(source)),
            },
        }
    }
    pub fn is_protected_failure(&self) -> bool {
        matches!(
            self.kind,
            DiagnosticTextKind::Failed(super::FailedFileText::Protected)
        )
    }
    pub fn public_text(&self) -> Option<&'a str> {
        match &self.kind {
            DiagnosticTextKind::Parsed { source, .. } => Some(source),
            DiagnosticTextKind::Failed(super::FailedFileText::Public(text)) => Some(text.text()),
            DiagnosticTextKind::Failed(super::FailedFileText::Protected) => None,
        }
    }
}

/// Lexical lints receive only this byte/line-preserving public projection.
/// Parsed keyed findings use the same ownership value for typed admission.
pub(crate) struct LintSource<'a> {
    original: &'a str,
    public: String,
    protected: SecretOwnedRanges,
    positions: SourcePositions,
    legacy: crate::lint_text::FirstOccurrences,
    public_names: crate::lint_text::FirstOccurrences,
}

use crate::lint_text::SourcePositions;

pub(crate) struct AdmittedLintKey {
    pub(crate) name: AdmittedLintName,
    pub(crate) range: Range<usize>,
    pub(crate) first: usize,
}

pub(crate) enum AdmittedLintName {
    Public(String),
    InsideSecret,
}

impl<'a> LintSource<'a> {
    pub(crate) fn new(source: &DiagnosticFileText<'a>) -> Option<Self> {
        let original = source.public_text()?;
        let protected = match &source.kind {
            DiagnosticTextKind::Parsed { protected, .. } => protected.clone(),
            DiagnosticTextKind::Failed(_) => SecretOwnedRanges::default(),
        };
        let source = original;
        let mut public = source.as_bytes().to_vec();
        for owner in protected.iter() {
            for byte in &mut public[owner.owned()] {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        }
        Some(Self {
            original: source,
            public: String::from_utf8(public)
                .expect("masked UTF-8 contains only public scalars and spaces"),
            protected,
            positions: SourcePositions::new(source),
            legacy: Default::default(),
            public_names: Default::default(),
        })
    }

    pub(crate) fn text(&self) -> &str {
        &self.public
    }

    pub(crate) fn original_text(&self) -> &str {
        self.original
    }

    pub(crate) fn collect_names<T, R>(
        &self,
        candidates: impl IntoIterator<Item = (Range<usize>, T)>,
        mut build: impl FnMut(String, T, Range<usize>) -> R,
    ) -> Vec<R> {
        candidates
            .into_iter()
            .filter_map(|(range, detail)| {
                // A candidate truncated at the mask boundary is not a public name.
                self.protected
                    .owner(range.start..range.end.saturating_add(1))
                    .is_none()
                    .then(|| build(self.original[range.clone()].to_owned(), detail, range))
            })
            .collect()
    }

    pub(crate) fn position(&self, byte: usize) -> (usize, usize) {
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::PositionQuery, 1);
        let position = self.positions.bytes[byte];
        (position.line + 1, position.scalar + 1)
    }

    pub(crate) fn lint_ranges(
        &self,
        range: Range<usize>,
        legacy_pattern: &str,
        legacy_width: usize,
    ) -> (super::ParseWarningSpan, (usize, usize, usize)) {
        let start = self.position(range.start);
        let end = self.position(range.end);
        let span = super::ParseWarningSpan {
            start_line: start.0,
            start_column: start.1,
            end_line: end.0,
            end_column: end.1,
        };
        let line_range = &self.positions.lines[start.0 - 1];
        let line_start = line_range.start;
        let line_end = line_range.end;
        let line = &self.original[line_start..line_end];
        let legacy_byte = self
            .legacy
            .first(start.0 - 1, line, legacy_pattern)
            .unwrap_or(range.start - line_start);
        let column = self.positions.bytes[line_start + legacy_byte].utf16;
        let end_column = column + legacy_width;
        (span, (start.0 - 1, column, end_column))
    }

    /// Preserve the legacy first-substring anchor without rescanning a line
    /// for each authored name. Extraction already supplies the occurrence.
    pub(crate) fn first_public_occurrence(&self, occurrence: Range<usize>) -> Range<usize> {
        let line = self.positions.bytes[occurrence.start].line;
        let bounds = &self.positions.lines[line];
        let pattern = &self.public[occurrence.clone()];
        let first = self
            .public_names
            .first(line, &self.public[bounds.clone()], pattern)
            .unwrap_or(occurrence.start - bounds.start);
        bounds.start + first..bounds.start + first + pattern.len()
    }

    pub(crate) fn admit_key(
        &self,
        name: String,
        range: Range<usize>,
        first: usize,
    ) -> AdmittedLintKey {
        match self.protected.owner(range.clone()) {
            Some(owner) => AdmittedLintKey {
                name: AdmittedLintName::InsideSecret,
                range: owner.anchor(),
                first: owner.anchor.start,
            },
            None => AdmittedLintKey {
                name: AdmittedLintName::Public(name),
                range,
                first,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round7_indexed_positions_and_secret_intervals_match_reference() {
        let source = "let Bad = '😀é'\nlet hidden = secret('CANARY')\nlet next = secret('OTHER')\n";
        let text = DiagnosticFileText::new(source);
        let admitted = LintSource::new(&text).unwrap();
        let boundaries = source
            .char_indices()
            .map(|(byte, _)| byte)
            .chain(std::iter::once(source.len()))
            .collect::<Vec<_>>();
        for &byte in &boundaries {
            let prefix = &source[..byte];
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let line_start = prefix.rfind('\n').map_or(0, |byte| byte + 1);
            assert_eq!(
                admitted.position(byte),
                (line, prefix[line_start..].chars().count() + 1)
            );
            assert_eq!(
                admitted.positions.bytes[byte].utf16,
                prefix[line_start..].encode_utf16().count()
            );
            for &end in boundaries.iter().filter(|end| **end >= byte) {
                let expected = admitted
                    .protected
                    .0
                    .iter()
                    .find(|region| region.owned.start < end && byte < region.owned.end);
                assert_eq!(
                    admitted
                        .protected
                        .owner(byte..end)
                        .map(SecretOwnedRange::owned),
                    expected.map(SecretOwnedRange::owned)
                );
            }
        }
    }

    #[test]
    fn round9_lint_position_projection_counts_linear_work() {
        use crate::test_work::{Operation, measure};
        let run = |count| {
            let source = format!(
                "???\n{}",
                (0..count)
                    .map(|i| format!("let Bad{i} = 1\n"))
                    .collect::<String>()
            );
            let (findings, work) =
                measure(|| crate::lint::non_snake_case_binding_findings(&source));
            assert_eq!(findings.len(), count);
            assert_eq!(work.get(Operation::ColumnBytes), 0);
            assert_eq!(work.get(Operation::PositionIndexBytes), source.len());
            assert!(work.get(Operation::PositionQuery) > 0);
            work
        };
        let small = run(256);
        let large = run(512);
        assert_eq!(
            large.get(Operation::PositionQuery),
            2 * small.get(Operation::PositionQuery)
        );
        assert!(
            large.get(Operation::PositionIndexBytes)
                <= 3 * small.get(Operation::PositionIndexBytes)
        );
    }
}
