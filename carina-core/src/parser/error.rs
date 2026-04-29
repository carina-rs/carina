//! Parse-time error and warning types.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::Rule;

/// Parse error
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Syntax error: {0}")]
    Syntax(#[from] pest::error::Error<Rule>),

    #[error("Invalid expression at line {line}: {message}")]
    InvalidExpression { line: usize, message: String },

    #[error("Undefined variable: {0}")]
    UndefinedVariable(String),

    #[error("Invalid resource type: {0}")]
    InvalidResourceType(String),

    #[error("Duplicate module definition: {0}")]
    DuplicateModule(String),

    #[error("Duplicate binding at line {line}: {name}")]
    DuplicateBinding { name: String, line: usize },

    #[error("{}", format_undefined_identifier(name, suggestion.as_deref(), in_scope))]
    UndefinedIdentifier {
        name: String,
        line: usize,
        /// Edit-distance close match among in-scope bindings, if any.
        /// Filled by the check site when a `suggest_similar_name` result is
        /// available; None for hand-constructed errors.
        suggestion: Option<String>,
        /// Concrete binding names in scope at the check site, sorted for
        /// deterministic rendering. Empty when no bindings have been
        /// declared at all.
        in_scope: Vec<String>,
    },

    #[error("Module not found: {0}")]
    ModuleNotFound(String),

    #[error("Internal parser error: expected {expected} in {context}")]
    InternalError { expected: String, context: String },

    #[error("Recursive function call detected: {0}")]
    RecursiveFunction(String),

    #[error("User-defined function error: {0}")]
    UserFunctionError(String),
}

/// Render the `UndefinedIdentifier` message. When a close match exists
/// (`suggestion`), lead with `Did you mean ...?`. Otherwise list the
/// concrete in-scope names so the reader learns what is available,
/// followed by the abstract list of binding kinds as a trailing aside.
/// See #2038.
fn format_undefined_identifier(
    name: &str,
    suggestion: Option<&str>,
    in_scope: &[String],
) -> String {
    if let Some(s) = suggestion {
        return format!("Undefined identifier `{}`. Did you mean `{}`?", name, s);
    }
    let kinds = "let / upstream_state / read / module / function / for / fn / arguments";
    if in_scope.is_empty() {
        format!(
            "Undefined identifier `{}`: no bindings are in scope ({})",
            name, kinds,
        )
    } else {
        format!(
            "Undefined identifier `{}`. In-scope names: {} ({})",
            name,
            in_scope.join(", "),
            kinds,
        )
    }
}

/// A structured warning emitted during parsing (non-fatal).
#[derive(Debug, Clone, PartialEq)]
pub struct ParseWarning {
    /// Full source path of the file this warning originated from
    /// (stamped by `config_loader` after parsing). `None` at parse time;
    /// always `Some` once the warning reaches CLI/LSP callers.
    pub file: Option<String>,
    pub line: usize,
    pub message: String,
}

impl ParseError {
    /// Construct an `UndefinedIdentifier` error with did-you-mean suggestion
    /// and sorted in-scope binding list. `known_bindings` is the set of
    /// names that are in scope at the check site — they will be sorted
    /// and used to compute the close-match suggestion. See #2038.
    pub fn undefined_identifier(
        name: String,
        line: usize,
        known_bindings: Vec<String>,
    ) -> ParseError {
        let known_refs: Vec<&str> = known_bindings.iter().map(String::as_str).collect();
        let suggestion = crate::schema::suggest_similar_name(&name, &known_refs);
        let mut in_scope = known_bindings;
        in_scope.sort();
        ParseError::UndefinedIdentifier {
            name,
            line,
            suggestion,
            in_scope,
        }
    }
}

pub(super) fn undefined_identifier_error(
    known: &std::collections::HashSet<&str>,
    name: String,
    line: usize,
) -> ParseError {
    ParseError::undefined_identifier(name, line, known.iter().map(|s| s.to_string()).collect())
}
