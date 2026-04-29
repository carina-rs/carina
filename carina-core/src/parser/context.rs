//! `ParseContext` (per-parse mutable scope) and the small pest helper
//! functions (`next_pair`, `first_inner`, `extract_key_string`) every
//! block parser needs.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{DeferredForExpression, UpstreamState, UserFunction};
use super::error::{ParseError, ParseWarning};
use super::{ProviderContext, Rule};
use crate::eval_value::EvalValue;
use crate::resource::Resource;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Parse context (variable scope)
#[derive(Clone)]
pub(crate) struct ParseContext<'cfg> {
    /// Variables bound by `let` statements during parsing. Carries
    /// `EvalValue` rather than `Value` because partial applications
    /// (e.g. `let f = join("-")`) produce a closure that lives until a
    /// later pipe finishes the application. Lowered to `Value` at the
    /// end of `parse(...)`; an unfinished closure surfaces as a
    /// parse-time error.
    pub(super) variables: IndexMap<String, EvalValue>,
    /// Resource bindings (binding_name -> Resource)
    pub(super) resource_bindings: HashMap<String, Resource>,
    /// Imported modules (alias -> path)
    pub(super) imported_modules: HashMap<String, String>,
    /// User-defined functions
    pub(super) user_functions: HashMap<String, UserFunction>,
    /// Functions currently being evaluated (for recursion detection)
    pub(super) evaluating_functions: Vec<String>,
    /// Parser configuration (decryptor, custom validators)
    pub(super) config: &'cfg ProviderContext,
    /// Binding names from structurally-required expressions (if/for/read)
    pub(super) structural_bindings: HashSet<String>,
    /// Upstream state bindings (binding_name -> UpstreamState)
    pub(super) upstream_states: HashMap<String, UpstreamState>,
    /// Non-fatal warnings collected during parsing
    pub(super) warnings: Vec<ParseWarning>,
    /// Deferred for-expressions collected during parsing
    pub(super) deferred_for_expressions: Vec<DeferredForExpression>,
}

impl<'cfg> ParseContext<'cfg> {
    pub(super) fn new(config: &'cfg ProviderContext) -> Self {
        Self {
            variables: IndexMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
            user_functions: HashMap::new(),
            evaluating_functions: Vec::new(),
            config,
            structural_bindings: HashSet::new(),
            upstream_states: HashMap::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
        }
    }

    pub(super) fn set_variable(&mut self, name: String, value: impl Into<EvalValue>) {
        self.variables.insert(name, value.into());
    }

    pub(super) fn get_variable(&self, name: &str) -> Option<&EvalValue> {
        self.variables.get(name)
    }

    pub(super) fn set_resource_binding(&mut self, name: String, resource: Resource) {
        self.resource_bindings.insert(name, resource);
    }

    pub(super) fn is_resource_binding(&self, name: &str) -> bool {
        self.resource_bindings.contains_key(name)
    }
}

/// Helper to get the next element from a pest iterator, returning a ParseError on failure
pub(crate) fn next_pair<'a>(
    iter: &mut pest::iterators::Pairs<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    iter.next().ok_or_else(|| ParseError::InternalError {
        expected: expected.to_string(),
        context: context.to_string(),
    })
}

/// Extract a key string from either an identifier or a quoted string pair.
/// For identifiers, returns the raw text. For strings, extracts the content
/// without quotes (supports both single-quoted and double-quoted strings).
pub(crate) fn extract_key_string(
    pair: pest::iterators::Pair<'_, Rule>,
) -> Result<String, ParseError> {
    match pair.as_rule() {
        Rule::identifier => Ok(pair.as_str().to_string()),
        Rule::string => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| ParseError::InternalError {
                    expected: "string content".to_string(),
                    context: "map/attribute key".to_string(),
                })?;
            match inner.as_rule() {
                Rule::single_quoted_string => {
                    // Extract content between quotes
                    let content = inner
                        .into_inner()
                        .next()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default();
                    Ok(content)
                }
                Rule::double_quoted_string => {
                    let mut result = String::new();
                    for part in inner.into_inner() {
                        match part.as_rule() {
                            Rule::string_part => {
                                let inner_part = part.into_inner().next().unwrap();
                                match inner_part.as_rule() {
                                    Rule::string_literal => result.push_str(inner_part.as_str()),
                                    Rule::interpolation => {
                                        return Err(ParseError::InternalError {
                                            expected: "literal string".to_string(),
                                            context: "interpolation not supported in map keys"
                                                .to_string(),
                                        });
                                    }
                                    _ => result.push_str(inner_part.as_str()),
                                }
                            }
                            _ => result.push_str(part.as_str()),
                        }
                    }
                    Ok(result)
                }
                _ => Ok(inner.as_str().to_string()),
            }
        }
        _ => Ok(pair.as_str().to_string()),
    }
}

/// Helper to get the first inner pair from a pest pair
pub(crate) fn first_inner<'a>(
    pair: pest::iterators::Pair<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    pair.into_inner()
        .next()
        .ok_or_else(|| ParseError::InternalError {
            expected: expected.to_string(),
            context: context.to_string(),
        })
}
