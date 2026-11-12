//! Indexed lint coordinates and legacy first-substring positions. Identity is
//! never reconstructed from diagnostic text. No index implements Debug.

use std::{collections::HashMap, ops::Range, sync::Mutex};

#[inline]
fn step() {
    #[cfg(any(test, feature = "test-utils"))]
    crate::test_work::record(crate::test_work::Operation::LintIndexStep, 1);
}

#[derive(Clone, Default)]
struct State {
    length: usize,
    suffix: Option<usize>,
    first_end: usize,
    edges: HashMap<u8, usize>,
}

/// A suffix automaton indexes each line once. Every substring query costs only
/// its own length, including overlapping matches and matches inside identifiers.
/// Clones retain the earliest end position, preserving str::find exactly.
struct FirstOccurrenceIndex(Vec<State>);

impl FirstOccurrenceIndex {
    fn new(source: &str) -> Self {
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::LintScanBytes, source.len());
        let mut nodes = vec![State::default()];
        let mut last = 0;
        for (position, byte) in source.bytes().enumerate() {
            step();
            let current = nodes.len();
            nodes.push(State {
                length: nodes[last].length + 1,
                first_end: position,
                ..State::default()
            });
            let mut parent = Some(last);
            while let Some(p) = parent {
                step();
                if let std::collections::hash_map::Entry::Vacant(entry) = nodes[p].edges.entry(byte)
                {
                    entry.insert(current);
                } else {
                    break;
                }
                parent = nodes[p].suffix;
            }
            if let Some(p) = parent {
                let target = nodes[p].edges[&byte];
                if nodes[p].length + 1 == nodes[target].length {
                    nodes[current].suffix = Some(target);
                } else {
                    let clone = nodes.len();
                    let mut cloned = nodes[target].clone();
                    #[cfg(any(test, feature = "test-utils"))]
                    crate::test_work::record(
                        crate::test_work::Operation::LintIndexStep,
                        cloned.edges.len(),
                    );
                    cloned.length = nodes[p].length + 1;
                    nodes.push(cloned);
                    while let Some(p) = parent {
                        step();
                        if nodes[p].edges.get(&byte) != Some(&target) {
                            break;
                        }
                        nodes[p].edges.insert(byte, clone);
                        parent = nodes[p].suffix;
                    }
                    nodes[target].suffix = Some(clone);
                    nodes[current].suffix = Some(clone);
                }
            } else {
                nodes[current].suffix = Some(0);
            }
            last = current;
        }
        Self(nodes)
    }

    fn first(&self, pattern: &str) -> Option<usize> {
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::LintSearchBytes, pattern.len());
        if pattern.is_empty() {
            return Some(0);
        }
        let mut state = 0;
        for byte in pattern.bytes() {
            step();
            state = *self.0[state].edges.get(&byte)?;
        }
        Some(self.0[state].first_end + 1 - pattern.len())
    }
}

#[derive(Default)]
pub(crate) struct FirstOccurrences(Mutex<HashMap<usize, FirstOccurrenceIndex>>);
impl FirstOccurrences {
    pub(crate) fn first(&self, line: usize, text: &str, pattern: &str) -> Option<usize> {
        self.0
            .lock()
            .expect("lint index lock poisoned")
            .entry(line)
            .or_insert_with(|| FirstOccurrenceIndex::new(text))
            .first(pattern)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct IndexedPosition {
    pub(crate) line: usize,
    pub(crate) scalar: usize,
    pub(crate) utf16: usize,
}

pub(crate) struct SourcePositions {
    pub(crate) bytes: Vec<IndexedPosition>,
    pub(crate) lines: Vec<Range<usize>>,
    scalar_starts: Vec<usize>,
    scalar_utf16: Vec<usize>,
}
impl SourcePositions {
    pub(crate) fn new(source: &str) -> Self {
        #[cfg(any(test, feature = "test-utils"))]
        {
            crate::test_work::record(
                crate::test_work::Operation::PositionIndexBytes,
                source.len(),
            );
            crate::test_work::record(crate::test_work::Operation::LintScanBytes, source.len());
        }
        let mut bytes = vec![IndexedPosition::default(); source.len() + 1];
        let mut lines = Vec::new();
        let mut scalar_starts = vec![0];
        let mut scalar_utf16 = Vec::new();
        let mut position = IndexedPosition::default();
        let mut start = 0;
        for (byte, character) in source.char_indices() {
            scalar_utf16.push(position.utf16);
            bytes[byte..byte + character.len_utf8()].fill(position);
            if character == '\n' {
                lines.push(start..byte);
                scalar_starts.push(scalar_utf16.len());
                start = byte + 1;
                position = IndexedPosition {
                    line: position.line + 1,
                    scalar: 0,
                    utf16: 0,
                };
            } else {
                position.scalar += 1;
                position.utf16 += character.len_utf16();
            }
        }
        scalar_utf16.push(position.utf16);
        bytes[source.len()] = position;
        lines.push(start..source.len());
        Self {
            bytes,
            lines,
            scalar_starts,
            scalar_utf16,
        }
    }

    fn utf16_column(&self, line: usize, scalar: usize) -> usize {
        #[cfg(any(test, feature = "test-utils"))]
        crate::test_work::record(crate::test_work::Operation::LintPositionBytes, 1);
        let Some(range) = self.lines.get(line) else {
            return scalar;
        };
        let end = self.bytes[range.end];
        if scalar > end.scalar {
            scalar + end.utf16 - end.scalar
        } else {
            self.scalar_utf16[self.scalar_starts[line] + scalar]
        }
    }
}

/// One immutable source view, with coordinates and lazy per-line substring
/// indices. Rebuilt with a document revision; no per-finding source scans.
pub struct LintTextIndex {
    text: String,
    positions: SourcePositions,
    occurrences: FirstOccurrences,
}
impl LintTextIndex {
    pub fn new(source: &str) -> Self {
        Self {
            text: source.to_owned(),
            positions: SourcePositions::new(source),
            occurrences: FirstOccurrences::default(),
        }
    }

    pub fn utf16_column(&self, line: usize, scalar: usize) -> usize {
        self.positions.utf16_column(line, scalar)
    }

    /// Match Rope's line bounds, including the line terminator, for strict span admission.
    pub fn line_scalar_len(&self, line: usize) -> Option<usize> {
        let range = self.positions.lines.get(line)?;
        Some(self.positions.bytes[range.end].scalar + usize::from(range.end < self.text.len()))
    }

    pub fn first_on_line(&self, line: usize, pattern: &str) -> Option<usize> {
        let range = self.positions.lines.get(line)?;
        if range.start == self.text.len() {
            return None;
        }
        let text = &self.text[range.clone()];
        let text = text.strip_suffix('\r').unwrap_or(text);
        let found = self.occurrences.first(line, text, pattern)?;
        Some(self.positions.bytes[range.start + found].utf16)
    }
}

/// Prefix-only schema matches retain the caller's iteration order, even for
/// unusual overlapping schema names, without comparing every name per line.
pub(crate) struct PrefixNames<'a> {
    nodes: Vec<PrefixNode<'a>>,
}
#[derive(Default)]
struct PrefixNode<'a> {
    edges: HashMap<u8, usize>,
    terminal: Option<(usize, &'a str)>,
}
impl<'a> PrefixNames<'a> {
    pub(crate) fn new(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut nodes = vec![PrefixNode::default()];
        for (rank, name) in names.into_iter().enumerate() {
            let mut state = 0;
            for byte in name.bytes() {
                step();
                state = match nodes[state].edges.get(&byte) {
                    Some(&next) => next,
                    None => {
                        let next = nodes.len();
                        nodes.push(PrefixNode::default());
                        nodes[state].edges.insert(byte, next);
                        next
                    }
                };
            }
            nodes[state].terminal = Some((rank, name));
        }
        Self { nodes }
    }

    pub(crate) fn matches(&self, line: &str) -> Vec<&'a str> {
        let mut matches = self.nodes[0].terminal.into_iter().collect::<Vec<_>>();
        let mut state = 0;
        for byte in line.bytes() {
            step();
            let Some(&next) = self.nodes[state].edges.get(&byte) else {
                break;
            };
            state = next;
            if let Some(entry) = self.nodes[state].terminal {
                matches.push(entry);
            }
        }
        matches.sort_unstable_by_key(|(rank, _)| *rank);
        matches.into_iter().map(|(_, name)| name).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round11_occurrence_index_matches_find_for_overlaps_and_unicode() {
        for text in [
            "ababa BadName Name BadName",
            "'BadName' /* BadName */",
            "😀éé é日本",
            "secret('BadName') BadName",
            "",
        ] {
            let index = FirstOccurrenceIndex::new(text);
            let boundaries = text
                .char_indices()
                .map(|(byte, _)| byte)
                .chain(std::iter::once(text.len()))
                .collect::<Vec<_>>();
            for &start in &boundaries {
                for &end in boundaries.iter().filter(|end| **end >= start) {
                    let pattern = &text[start..end];
                    assert_eq!(
                        index.first(pattern),
                        text.find(pattern),
                        "{text:?} / {pattern:?}"
                    );
                }
            }
            assert_eq!(index.first("not-present"), None);
        }
    }

    #[test]
    fn round11_indexed_scalar_positions_match_prefix_conversion() {
        for source in ["😀aé\r\n日本 é\n", "\n", "", "α😀β"] {
            let index = LintTextIndex::new(source);
            for (line, text) in source.split('\n').enumerate() {
                for scalar in 0..text.chars().count() + 3 {
                    let expected = scalar
                        + text
                            .chars()
                            .take(scalar)
                            .map(|ch| ch.len_utf16() - 1)
                            .sum::<usize>();
                    assert_eq!(index.utf16_column(line, scalar), expected);
                }
            }
        }
    }
}
