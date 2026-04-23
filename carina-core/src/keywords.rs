//! Single source of truth for the Carina DSL's reserved keywords.
//!
//! The LSP (semantic tokens, completion) and the TextMate grammars each
//! maintain their own representation; parity tests keep them from drifting
//! apart.

/// Semantic role of a keyword, aligned with the TextMate scope split used by
/// `editors/vscode/syntaxes/carina.tmLanguage.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeywordKind {
    /// Binding / function introducer: `let`, `fn`.
    Storage,
    /// Top-level / structural block declarations: `provider`, `backend`,
    /// `upstream_state`, `exports`, `attributes`, `arguments`, `validation`,
    /// `moved`, `removed`.
    Declaration,
    /// Control flow: `for`, `in`, `if`, `else`.
    Control,
    /// Prefix / non-block keywords: `use`, `import`, `require`, `read`.
    Other,
    /// Null literal.
    NullLiteral,
}

pub const KEYWORDS: &[(&str, KeywordKind)] = &[
    ("fn", KeywordKind::Storage),
    ("let", KeywordKind::Storage),
    ("arguments", KeywordKind::Declaration),
    ("attributes", KeywordKind::Declaration),
    ("backend", KeywordKind::Declaration),
    ("exports", KeywordKind::Declaration),
    ("moved", KeywordKind::Declaration),
    ("provider", KeywordKind::Declaration),
    ("removed", KeywordKind::Declaration),
    ("upstream_state", KeywordKind::Declaration),
    ("validation", KeywordKind::Declaration),
    ("else", KeywordKind::Control),
    ("for", KeywordKind::Control),
    ("if", KeywordKind::Control),
    ("in", KeywordKind::Control),
    ("import", KeywordKind::Other),
    ("read", KeywordKind::Other),
    ("require", KeywordKind::Other),
    ("use", KeywordKind::Other),
    ("null", KeywordKind::NullLiteral),
];

/// Keywords filtered by kind.
pub fn by_kind(kind: KeywordKind) -> impl Iterator<Item = &'static str> {
    KEYWORDS
        .iter()
        .filter(move |(_, k)| *k == kind)
        .map(|(name, _)| *name)
}

/// Return true when `word` is a reserved keyword.
pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|(name, _)| *name == word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_are_unique() {
        let mut names: Vec<&str> = KEYWORDS.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "Duplicate keyword entry in KEYWORDS"
        );
    }

    #[test]
    fn pest_grammar_contains_every_keyword() {
        let pest_grammar = include_str!("parser/carina.pest");
        for (name, _) in KEYWORDS {
            let literal = format!("\"{}\"", name);
            assert!(
                pest_grammar.contains(&literal),
                "KEYWORDS entry `{}` is not referenced as a string literal in carina.pest. \
                 Either the pest grammar lost a keyword or KEYWORDS has a stale entry.",
                name
            );
        }
    }

    #[test]
    fn is_keyword_matches_list() {
        assert!(is_keyword("let"));
        assert!(is_keyword("upstream_state"));
        assert!(!is_keyword("not_a_keyword"));
        assert!(!is_keyword(""));
    }

    #[test]
    fn by_kind_partitions_correctly() {
        let storage: Vec<&str> = by_kind(KeywordKind::Storage).collect();
        assert_eq!(storage, vec!["fn", "let"]);
        let null: Vec<&str> = by_kind(KeywordKind::NullLiteral).collect();
        assert_eq!(null, vec!["null"]);
    }
}
