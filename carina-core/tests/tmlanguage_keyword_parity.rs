//! Drift-detection tests: assert that every `KEYWORDS` entry appears in each
//! `carina.tmLanguage.json` under its expected scope, and that the grammar
//! lists nothing beyond `KEYWORDS`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use carina_core::keywords::{KEYWORDS, KeywordKind};

const VSCODE_GRAMMAR: &str = "../editors/vscode/syntaxes/carina.tmLanguage.json";
const TMBUNDLE_GRAMMAR: &str = "../editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json";

fn kind_for_scope(scope: &str) -> Option<KeywordKind> {
    match scope {
        "storage.type.carina" => Some(KeywordKind::Storage),
        "keyword.declaration.carina" => Some(KeywordKind::Declaration),
        "keyword.control.carina" => Some(KeywordKind::Control),
        "keyword.other.carina" => Some(KeywordKind::Other),
        "constant.language.null.carina" => Some(KeywordKind::NullLiteral),
        _ => None,
    }
}

fn parse_grammar_keywords(path: &str) -> BTreeSet<(KeywordKind, String)> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let full_path = manifest_dir.join(path);
    let content = fs::read_to_string(&full_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", full_path.display(), e));
    let json: serde_json::Value =
        serde_json::from_str(&content).expect("tmLanguage file must be valid JSON");

    let patterns = json
        .pointer("/repository/keywords/patterns")
        .and_then(|v| v.as_array())
        .expect("expected repository.keywords.patterns to be an array");

    let mut pairs = BTreeSet::new();
    for pattern in patterns {
        let scope = pattern
            .get("name")
            .and_then(|v| v.as_str())
            .expect("pattern missing `name`");
        let match_re = pattern
            .get("match")
            .and_then(|v| v.as_str())
            .expect("pattern missing `match`");
        let kind =
            kind_for_scope(scope).unwrap_or_else(|| panic!("unknown scope `{scope}` in {path}"));
        for word in extract_words(match_re) {
            pairs.insert((kind, word));
        }
    }
    pairs
}

/// Parse `\b(a|b|c)\b` or `\bword\b` into the alternation members. Panics on
/// any other shape — this is a linter, so malformed input should fail loudly.
fn extract_words(regex: &str) -> Vec<String> {
    let inner = regex
        .strip_prefix("\\b")
        .and_then(|s| s.strip_suffix("\\b"))
        .unwrap_or_else(|| panic!("regex `{regex}` must be bracketed by \\b"));
    let has_open = inner.starts_with('(');
    let has_close = inner.ends_with(')');
    let inner = match (has_open, has_close) {
        (true, true) => &inner[1..inner.len() - 1],
        (false, false) => inner,
        _ => panic!("regex `{regex}` has unbalanced parentheses"),
    };
    inner.split('|').map(|s| s.trim().to_string()).collect()
}

fn expected_pairs() -> BTreeSet<(KeywordKind, String)> {
    KEYWORDS
        .iter()
        .map(|(name, kind)| (*kind, name.to_string()))
        .collect()
}

fn assert_grammar_matches(path: &str) {
    let actual = parse_grammar_keywords(path);
    let expected = expected_pairs();

    let missing: Vec<_> = expected.difference(&actual).collect();
    let extra: Vec<_> = actual.difference(&expected).collect();

    assert!(
        missing.is_empty() && extra.is_empty(),
        "TextMate grammar {} drifted from carina_core::keywords::KEYWORDS.\n  \
         Missing (in KEYWORDS but not in grammar): {:?}\n  \
         Extra (in grammar but not in KEYWORDS): {:?}",
        path,
        missing,
        extra
    );
}

#[test]
fn vscode_grammar_matches_keywords() {
    assert_grammar_matches(VSCODE_GRAMMAR);
}

#[test]
fn tmbundle_grammar_matches_keywords() {
    assert_grammar_matches(TMBUNDLE_GRAMMAR);
}
