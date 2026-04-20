//! Drift-detection test for the two pest grammar files.
//!
//! The main parser (`carina-core/src/parser/carina.pest`) and the
//! formatter's trivia-preserving parser (`carina-core/src/formatter/
//! carina_fmt.pest`) define overlapping but not identical rule sets —
//! the formatter carries extra trivia productions, and the main parser
//! has some constructs the formatter doesn't surface. The two grammars
//! have drifted silently before (#2117: missing `upstream_state_expr`,
//! missing discard pattern in for-bindings), so this test flags every
//! main-grammar rule whose name isn't also present in the formatter's
//! grammar.
//!
//! The check is one-directional: every rule defined in `carina.pest`
//! must have a same-named production in `carina_fmt.pest`. The reverse
//! direction is not required — the formatter legitimately introduces
//! `trivia`, `ws`, `comment`, `newline`, keyword (`kw_*`), and various
//! delimiter rules that the canonical parser folds into implicit
//! whitespace.
//!
//! `ALLOWED_MISSING` is an explicit escape hatch for rules that exist
//! only as an implementation detail in one grammar. Add entries there
//! deliberately, with a comment justifying each — the default is to add
//! the production to the formatter grammar.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

const MAIN_GRAMMAR: &str = "src/parser/carina.pest";
const FMT_GRAMMAR: &str = "src/formatter/carina_fmt.pest";

/// Main-grammar rule names that deliberately have no formatter
/// counterpart. Keep this list short and justify each entry in a
/// comment — the default response to drift should be to add the rule
/// to the formatter, not to allow-list it here.
const ALLOWED_MISSING: &[&str] = &[
    // pest built-ins with matching semantics but different spellings —
    // the formatter uses explicit `trivia` / `ws` / `newline` / `comment`
    // productions instead of the implicit `WHITESPACE` / `COMMENT` hooks.
    "WHITESPACE",
    "COMMENT",
    // Lexer-level string/comment helpers. The formatter consumes these
    // inline via its own string/comment productions.
    "string_char",
    "string_literal",
    "string_part",
    "single_quoted_content",
    "interpolation",
    "line_comment",
    "block_comment",
    // Operator/precedence intermediate rules — folded into the
    // formatter's pipe/compose chain and primary.
    "coalesce_expr",
    // Body wrappers for `fn`, `for`, `if` — the formatter uses
    // `fn_body_content`, `for_body_content`, `if_body_content` silent
    // rules instead.
    "fn_body",
    "fn_params",
    "for_body",
    "if_body",
    // Module call argument — the formatter uses the generic
    // `block_content*` inside `module_call` rather than a dedicated
    // per-argument rule.
    "module_call_arg",
    // Type expression sub-rules — the formatter exposes a single
    // `type_expr` / `type_list` / `type_map` surface; the main
    // parser's `type_simple` / `type_generic` / `resource_type_path`
    // collapse into those.
    "type_simple",
    "type_generic",
    "resource_type_path",
    // State-block attributes — the formatter names them more
    // granularly (`import_to_attr`, `import_id_attr`,
    // `removed_from_attr`, `moved_from_attr`, `moved_to_attr`) rather
    // than the main parser's umbrella `import_state_attr`,
    // `removed_attr`, `moved_attr`. Different surface, same content.
    "import_state_attr",
    "removed_attr",
    "moved_attr",
    // Arguments-block sub-rules — the formatter uses
    // `arguments_param_attr` with inline keyword branches instead of
    // distinct `arg_description_attr` / `arg_default_attr` /
    // `arg_validation_block` productions.
    "arg_description_attr",
    "arg_default_attr",
    "arg_validation_block",
    "validation_condition_attr",
    "validation_error_message_attr",
];

fn collect_rule_names(pest_path: &str) -> BTreeSet<String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let full_path = manifest_dir.join(pest_path);
    let content = fs::read_to_string(&full_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", full_path.display(), e));

    let mut names = BTreeSet::new();
    for raw_line in content.lines() {
        // Strip line comments so `//` inside a rule body doesn't count.
        let line = raw_line.split("//").next().unwrap_or("").trim_start();
        if line.is_empty() {
            continue;
        }
        // A pest rule looks like `name = { ... }` or `name = _{ ... }`
        // or `name = @{ ... }`. The name is always the first token on
        // the line and uses only ASCII identifier characters.
        let Some((ident, rest)) = line.split_once('=') else {
            continue;
        };
        let ident = ident.trim();
        if ident.is_empty() || !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // Confirm what follows `=` is a pest rule body: `{`, `_{`
        // (silent), `@{` (atomic), or `${` (compound-atomic). Otherwise
        // this is a stray `=` sign we don't care about.
        let rest = rest.trim_start();
        if rest.starts_with('{')
            || rest.starts_with("_{")
            || rest.starts_with("@{")
            || rest.starts_with("${")
        {
            names.insert(ident.to_string());
        }
    }
    names
}

#[test]
fn every_main_rule_has_a_formatter_counterpart() {
    let main_rules = collect_rule_names(MAIN_GRAMMAR);
    let fmt_rules = collect_rule_names(FMT_GRAMMAR);
    assert!(
        !main_rules.is_empty(),
        "parsed zero rules from {} — check the rule-extraction regex",
        MAIN_GRAMMAR
    );
    assert!(
        !fmt_rules.is_empty(),
        "parsed zero rules from {} — check the rule-extraction regex",
        FMT_GRAMMAR
    );
    let allowed: BTreeSet<&str> = ALLOWED_MISSING.iter().copied().collect();
    let missing: Vec<&String> = main_rules
        .iter()
        .filter(|r| !fmt_rules.contains(*r) && !allowed.contains(r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "Main-grammar rules are missing from `{}`:\n  {}\n\nAdd the rule to the formatter grammar, or extend `ALLOWED_MISSING` with a justification.",
        FMT_GRAMMAR,
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
