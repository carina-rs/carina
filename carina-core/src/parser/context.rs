//! `ParseContext` (per-parse mutable scope) and the small pest helper
//! functions (`next_pair`, `first_inner`, `extract_key_string`) every
//! block parser needs.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{
    DeferredForExpression, PendingFunctionTypeCheck, ProviderConfig, UpstreamState, UserFunction,
    WaitBinding,
};
use super::error::{ParseError, ParseWarning};
use super::{ProviderContext, Rule};
use crate::eval_value::{EvalValue, StructuralHandle, StructuralHandleKind};
use crate::resource::{Resource, Value};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// Parser-local lexical binding kind.
///
/// Structural bindings (resources, module calls, `for` results, waits,
/// upstream state, and imports) are distinct from ordinary values. Their
/// parser-only handle records reference behavior without entering the public
/// value model, preventing access parsing from mistaking a structural address
/// such as `roles[0]` for value indexing.
#[derive(Clone)]
pub(super) enum ParseBinding {
    Value {
        value: EvalValue,
        source_line: usize,
    },
    Structural {
        handle: StructuralHandle,
    },
}

#[derive(Clone, Copy)]
pub(super) enum ParseBindingRef<'a> {
    Value { value: &'a EvalValue },
    Structural(&'a StructuralHandle),
    Unbound,
}

/// Parse context (variable scope)
#[derive(Clone)]
pub(crate) struct ParseContext<'cfg> {
    /// All lexical bindings, with value and structural bindings represented
    /// as disjoint variants. Value bindings carry `EvalValue` because partial
    /// applications can live until a later pipe finishes the application.
    pub(super) bindings: IndexMap<String, ParseBinding>,
    /// Resource bindings (binding_name -> Resource)
    pub(super) resource_bindings: HashMap<String, Resource>,
    /// Imported modules (alias -> path)
    pub(super) imported_modules: HashMap<String, String>,
    /// User-defined functions
    pub(super) user_functions: Arc<HashMap<String, UserFunction>>,
    /// Interned user-function definitions used by deferred call nodes.
    /// Invalidated together with `user_function_scope` whenever the mutable
    /// parser table changes.
    resolved_user_functions: RefCell<HashMap<String, Arc<UserFunction>>>,
    /// Function names injected from sibling files for this directory parse.
    /// A local definition may replace its seed once; a second local
    /// definition remains a duplicate.
    pub(super) seeded_user_functions: HashSet<String>,
    /// Functions currently being evaluated (for recursion detection)
    pub(super) evaluating_functions: Vec<String>,
    /// Parser configuration (decryptor, custom validators)
    pub(super) config: &'cfg ProviderContext,
    /// Binding names from structurally-required expressions (if/for/read)
    pub(super) structural_bindings: HashSet<String>,
    /// Upstream state bindings (binding_name -> UpstreamState)
    pub(super) upstream_states: HashMap<String, UpstreamState>,
    /// `wait` bindings declared via `let <name> = wait <target> { ... }`
    /// (binding_name -> WaitBinding).
    pub(super) wait_bindings: HashMap<String, WaitBinding>,
    /// Named provider instances declared via
    /// `let <name> = provider <kind> { ... }`. Drained into
    /// `ParsedFile.providers` by the top-level parse loop.
    pub(super) named_provider_instances: Vec<ProviderConfig>,
    /// Non-fatal warnings collected during parsing
    pub(super) warnings: Vec<ParseWarning>,
    /// Deferred for-expressions collected during parsing
    pub(super) deferred_for_expressions: Vec<DeferredForExpression>,
    /// Names pre-registered via the directory-aware seed pass
    /// (`parse_with_seeded_bindings`). Excluded from local duplicate-
    /// binding checks, so a `let role = ...` in this file does not
    /// collide with the seed installed because the same name is
    /// declared in a sibling `.crn`. See #2817.
    pub(super) seeded_bindings: HashSet<String>,
    /// Bootstrap-evaluated calls whose provider-backed annotations must be
    /// checked once schemas are available. Shared by cloned lexical contexts
    /// so calls evaluated inside function bodies and loops reach the file.
    pub(super) pending_function_type_checks: Rc<RefCell<Vec<PendingFunctionTypeCheck>>>,
    /// Directory bootstrap parses may encounter a sibling name before the
    /// fixed point has classified it as a value or structural binding. Static
    /// control-flow expressions defer only that provisional case; the final
    /// parse runs with this disabled and reports the classified error.
    pub(super) defer_provisional_static_conditions: bool,
    /// Preserve bare lexical bindings until an expected value type is known.
    /// Resource attributes and nested module-call expressions enable this;
    /// untyped `let` expressions retain their original eager behavior.
    preserve_typed_value_candidates: bool,
    preserve_typed_structural_candidates: bool,
}

impl<'cfg> ParseContext<'cfg> {
    pub(super) fn new(config: &'cfg ProviderContext) -> Self {
        Self {
            bindings: IndexMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
            user_functions: Arc::new(HashMap::new()),
            resolved_user_functions: RefCell::new(HashMap::new()),
            seeded_user_functions: HashSet::new(),
            evaluating_functions: Vec::new(),
            config,
            structural_bindings: HashSet::new(),
            upstream_states: HashMap::new(),
            wait_bindings: HashMap::new(),
            named_provider_instances: Vec::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
            seeded_bindings: HashSet::new(),
            pending_function_type_checks: Rc::new(RefCell::new(Vec::new())),
            defer_provisional_static_conditions: false,
            preserve_typed_value_candidates: false,
            preserve_typed_structural_candidates: false,
        }
    }

    pub(super) fn record_pending_function_type_check(&self, check: PendingFunctionTypeCheck) {
        let mut checks = self.pending_function_type_checks.borrow_mut();
        if !checks.contains(&check) {
            checks.push(check);
        }
    }

    fn invalidate_user_function_cache(&self) {
        self.resolved_user_functions.borrow_mut().clear();
    }

    pub(super) fn replace_user_functions(&mut self, functions: HashMap<String, UserFunction>) {
        self.user_functions = Arc::new(functions);
        self.invalidate_user_function_cache();
    }

    pub(super) fn replace_shared_user_functions(
        &mut self,
        functions: Arc<HashMap<String, UserFunction>>,
    ) {
        self.user_functions = functions;
        self.invalidate_user_function_cache();
    }

    pub(super) fn extend_user_functions(&mut self, functions: HashMap<String, UserFunction>) {
        Arc::make_mut(&mut self.user_functions).extend(functions);
        self.invalidate_user_function_cache();
    }

    pub(super) fn insert_user_function(&mut self, name: String, function: UserFunction) {
        Arc::make_mut(&mut self.user_functions).insert(name, function);
        self.invalidate_user_function_cache();
    }

    pub(super) fn user_functions_mut(&mut self) -> &mut HashMap<String, UserFunction> {
        self.invalidate_user_function_cache();
        Arc::make_mut(&mut self.user_functions)
    }

    pub(super) fn resolved_user_function(&self, name: &str) -> Option<Arc<UserFunction>> {
        if let Some(function) = self.resolved_user_functions.borrow().get(name) {
            return Some(Arc::clone(function));
        }
        let function = Arc::new(self.user_functions.get(name)?.clone());
        self.resolved_user_functions
            .borrow_mut()
            .insert(name.to_string(), Arc::clone(&function));
        Some(function)
    }

    pub(super) fn shared_user_function_scope(&self) -> Arc<HashMap<String, UserFunction>> {
        Arc::clone(&self.user_functions)
    }

    /// True if `name` was pre-registered via the directory-aware seed
    /// pass and not yet shadowed by a local declaration. Used by entry
    /// points that perform duplicate-binding checks (`let`, `arguments`)
    /// to allow a local declaration to overwrite the seeded placeholder
    /// without falsely flagging a duplicate. See #2817.
    pub(in crate::parser) fn is_seeded_binding(&self, name: &str) -> bool {
        self.seeded_bindings.contains(name)
    }

    /// Drop `name` from the seeded set. Called as soon as a local
    /// declaration overwrites the seeded placeholder, so subsequent
    /// `let foo = ...` redeclarations within the same file *are* still
    /// flagged as duplicates.
    pub(in crate::parser) fn unmark_seeded(&mut self, name: &str) {
        self.seeded_bindings.remove(name);
    }

    pub(super) fn set_variable(&mut self, name: String, value: impl Into<EvalValue>) {
        self.set_variable_at_line(name, value, 0);
    }

    pub(super) fn set_variable_at_line(
        &mut self,
        name: String,
        value: impl Into<EvalValue>,
        source_line: usize,
    ) {
        self.bindings.insert(
            name,
            ParseBinding::Value {
                value: value.into(),
                source_line,
            },
        );
    }

    pub(super) fn for_typed_value_sink(&self) -> Self {
        let mut typed = self.clone();
        typed.preserve_typed_value_candidates = true;
        typed.preserve_typed_structural_candidates = true;
        typed
    }

    pub(super) fn for_typed_module_value_sink(&self) -> Self {
        let mut typed = self.clone();
        typed.preserve_typed_value_candidates = true;
        typed.preserve_typed_structural_candidates = false;
        typed
    }

    pub(super) fn without_typed_value_sink(&self) -> Self {
        let mut untyped = self.clone();
        untyped.preserve_typed_value_candidates = false;
        untyped.preserve_typed_structural_candidates = false;
        untyped
    }

    pub(super) fn set_structural_binding(&mut self, name: String, handle: StructuralHandle) {
        self.bindings
            .insert(name, ParseBinding::Structural { handle });
    }

    pub(super) fn set_structural_alias(&mut self, name: String, handle: StructuralHandle) {
        self.bindings.insert(
            name.clone(),
            ParseBinding::Structural {
                handle: handle.with_alias(name),
            },
        );
    }

    pub(super) fn get_variable(&self, name: &str) -> Option<&EvalValue> {
        match self.bindings.get(name) {
            Some(ParseBinding::Value { value, .. }) => Some(value),
            Some(ParseBinding::Structural { .. }) | None => None,
        }
    }

    pub(super) fn lookup_binding(&self, name: &str) -> ParseBindingRef<'_> {
        match self.bindings.get(name) {
            Some(ParseBinding::Value { value, .. }) => ParseBindingRef::Value { value },
            Some(ParseBinding::Structural { handle, .. }) => ParseBindingRef::Structural(handle),
            None => ParseBindingRef::Unbound,
        }
    }

    pub(super) fn preserves_bare_binding(&self, name: &str) -> bool {
        self.preserve_typed_structural_candidates && self.is_structural_binding(name)
    }

    pub(super) fn bare_value_binding_candidate(&self, name: &str) -> Option<Value> {
        if !self.preserve_typed_value_candidates {
            return None;
        }
        let ParseBinding::Value { value, .. } = self.bindings.get(name)? else {
            return None;
        };
        value.clone().into_value().ok()
    }

    pub(super) fn is_structural_binding(&self, name: &str) -> bool {
        matches!(
            self.bindings.get(name),
            Some(ParseBinding::Structural { .. })
        )
    }

    pub(super) fn contains_binding(&self, name: &str) -> bool {
        self.bindings.contains_key(name)
    }

    pub(super) fn set_resource_binding(&mut self, name: String, resource: Resource) {
        if !matches!(
            self.bindings.get(&name),
            Some(ParseBinding::Structural { .. })
        ) {
            self.set_structural_binding(
                name.clone(),
                StructuralHandle::new(
                    StructuralHandleKind::Resource(super::ast::ResourceTypePath::new(
                        resource.id.provider.clone(),
                        resource.id.resource_type.clone(),
                    )),
                    name.clone(),
                ),
            );
        }
        self.resource_bindings.insert(name, resource);
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
