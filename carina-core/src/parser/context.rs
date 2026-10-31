//! `ParseContext` (per-parse mutable scope) and the small pest helper
//! functions (`next_pair`, `first_inner`, `extract_key_string`) every
//! block parser needs.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{
    AttributePath, AttributePathSegment, BareBindingKind, BareBindingSource, BareBindingUse,
    BindingScopeId, BindingUseOwner, DeferredForExpression, ModuleCallId, ParsedBindingUses,
    PendingFunctionTypeCheck, PendingValueSlot, ProviderConfig, ResourceAddress, SourceFileId,
    SourceSpan, TypeExpr, UpstreamState, UserFunction, WaitBinding,
};
use super::error::{ParseError, ParseWarning};
use super::{ProviderContext, Rule};
use crate::eval_value::{
    AuthoredEvalValue as EvalValue, AuthoredStructuralHandle as StructuralHandle,
    StructuralHandleKind,
};
use crate::resource::{AuthoredResource as Resource, AuthoredValue as Value, ResourceId};
use indexmap::IndexMap;
use std::cell::{Cell, OnceCell, RefCell};
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
        value: Box<EvalValue>,
        shared_value: OnceCell<Arc<Value>>,
        source_line: usize,
        declaration: crate::schema::BindingKind,
        declared_type: Option<Arc<TypeExpr>>,
        scope: BindingScopeId,
    },
    Structural {
        handle: StructuralHandle,
        scope: BindingScopeId,
    },
}

#[derive(Clone, Copy)]
pub(super) enum ParseBindingRef<'a> {
    Value {
        value: &'a EvalValue,
        declaration: crate::schema::BindingKind,
        declared_type: Option<&'a Arc<TypeExpr>>,
    },
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
    /// Shared parser-side provenance recorder. Real lexical child contexts
    /// share it; expression parsing never clones the context merely to change
    /// sink behavior.
    pub(super) parsed_binding_uses: Rc<RefCell<ParsedBindingUses>>,
    pub(super) binding_scope: BindingScopeId,
    binding_scope_source_file: std::path::PathBuf,
    next_binding_scope: Rc<Cell<u64>>,
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
            parsed_binding_uses: Rc::new(RefCell::new(ParsedBindingUses::default())),
            binding_scope: BindingScopeId::root(std::path::Path::new("test.crn")),
            binding_scope_source_file: std::path::PathBuf::from("test.crn"),
            next_binding_scope: Rc::new(Cell::new(1)),
        }
    }

    pub(super) fn set_source_scope(&mut self, source_file: &std::path::Path) {
        self.binding_scope = BindingScopeId::root(source_file);
        self.binding_scope_source_file = source_file.to_path_buf();
    }

    pub(super) fn child_scope(&self) -> Self {
        let ordinal = self.next_binding_scope.get();
        self.next_binding_scope.set(ordinal + 1);
        let mut child = self.clone();
        child.binding_scope = self
            .binding_scope
            .child(&self.binding_scope_source_file, ordinal);
        child
    }

    pub(super) fn take_parsed_binding_uses(&self) -> ParsedBindingUses {
        std::mem::take(&mut *self.parsed_binding_uses.borrow_mut())
    }

    pub(super) fn remap_binding_use_resource(&self, from: &ResourceId, to: &ResourceId) {
        self.parsed_binding_uses
            .borrow_mut()
            .remap_resource_owner(from, to);
    }

    pub(super) fn remap_binding_use_module_call(&self, from: &ModuleCallId, to: &ModuleCallId) {
        self.parsed_binding_uses
            .borrow_mut()
            .remap_module_call_owner(from, to);
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
                value: Box::new(value.into()),
                shared_value: OnceCell::new(),
                source_line,
                declaration: crate::schema::BindingKind::Let,
                declared_type: None,
                scope: self.binding_scope.clone(),
            },
        );
    }

    pub(super) fn set_argument_variable(
        &mut self,
        name: String,
        value: impl Into<EvalValue>,
        declared_type: Option<TypeExpr>,
    ) {
        self.bindings.insert(
            name,
            ParseBinding::Value {
                value: Box::new(value.into()),
                shared_value: OnceCell::new(),
                source_line: 0,
                declaration: crate::schema::BindingKind::Argument,
                declared_type: declared_type.map(Arc::new),
                scope: self.binding_scope.clone(),
            },
        );
    }

    pub(super) fn set_structural_binding(&mut self, name: String, handle: StructuralHandle) {
        self.bindings.insert(
            name,
            ParseBinding::Structural {
                handle,
                scope: self.binding_scope.clone(),
            },
        );
    }

    pub(super) fn set_structural_alias(&mut self, name: String, handle: StructuralHandle) {
        self.bindings.insert(
            name.clone(),
            ParseBinding::Structural {
                handle: handle.with_alias(name),
                scope: self.binding_scope.clone(),
            },
        );
    }

    pub(super) fn get_variable(&self, name: &str) -> Option<&EvalValue> {
        match self.bindings.get(name) {
            Some(ParseBinding::Value { value, .. }) => Some(value.as_ref()),
            Some(ParseBinding::Structural { .. }) | None => None,
        }
    }

    pub(super) fn lookup_binding(&self, name: &str) -> ParseBindingRef<'_> {
        match self.bindings.get(name) {
            Some(ParseBinding::Value {
                value,
                declaration,
                declared_type,
                ..
            }) => ParseBindingRef::Value {
                value: value.as_ref(),
                declaration: *declaration,
                declared_type: declared_type.as_ref(),
            },
            Some(ParseBinding::Structural { handle, .. }) => ParseBindingRef::Structural(handle),
            None => ParseBindingRef::Unbound,
        }
    }

    fn binding_scope(&self, name: &str) -> Option<&BindingScopeId> {
        match self.bindings.get(name) {
            Some(ParseBinding::Value { scope, .. } | ParseBinding::Structural { scope, .. }) => {
                Some(scope)
            }
            None => None,
        }
    }

    fn shared_binding_value(&self, name: &str) -> Option<Arc<Value>> {
        let ParseBinding::Value {
            value,
            shared_value,
            ..
        } = self.bindings.get(name)?
        else {
            return None;
        };
        if let Some(value) = shared_value.get() {
            return Some(Arc::clone(value));
        }
        let value = Arc::new(value.as_ref().clone().into_value().ok()?);
        let _ = shared_value.set(Arc::clone(&value));
        Some(value)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StructuralSinkMode {
    Pending,
    DirectHandle,
}

#[derive(Clone)]
pub(crate) struct ExpressionMode<'a> {
    sink: Option<ExpressionSink<'a>>,
}

#[derive(Clone)]
struct ExpressionSink<'a> {
    owner: BindingUseOwner,
    root_path: AttributePath,
    path: AttributePath,
    source_file: &'a std::path::Path,
    preprocessed_file: &'a super::PreprocessedFile,
    structural: StructuralSinkMode,
}

impl ExpressionMode<'_> {
    pub(super) const fn untyped() -> Self {
        Self { sink: None }
    }
}

impl<'a> ExpressionMode<'a> {
    pub(super) fn resource(
        id: ResourceId,
        source_file: &'a std::path::Path,
        preprocessed_file: &'a super::PreprocessedFile,
    ) -> Self {
        Self {
            sink: Some(ExpressionSink {
                owner: BindingUseOwner::Resource(ResourceAddress::new(id)),
                root_path: AttributePath::root("<resource>"),
                path: AttributePath::root("<resource>"),
                source_file,
                preprocessed_file,
                structural: StructuralSinkMode::Pending,
            }),
        }
    }

    pub(super) fn module_argument(
        call: super::ModuleCallId,
        argument: String,
        value_span: super::SourceSpan,
        source_file: &'a std::path::Path,
        preprocessed_file: &'a super::PreprocessedFile,
        structural: StructuralSinkMode,
    ) -> Self {
        let path = AttributePath::root(argument.clone());
        Self {
            sink: Some(ExpressionSink {
                owner: BindingUseOwner::ModuleArgument {
                    call,
                    argument,
                    value_span,
                },
                root_path: path.clone(),
                path,
                source_file,
                preprocessed_file,
                structural,
            }),
        }
    }

    pub(super) fn at_attribute(&self, name: impl Into<String>) -> Self {
        let Some(sink) = &self.sink else {
            return Self::untyped();
        };
        let path = AttributePath::root(name);
        Self {
            sink: Some(ExpressionSink {
                root_path: path.clone(),
                path,
                ..sink.clone()
            }),
        }
    }

    pub(super) fn at_named_child(&self, name: impl Into<String>) -> Self {
        let name = name.into();
        match self.sink.as_ref() {
            Some(sink) if sink.root_path.root_name() == "<resource>" => self.at_attribute(name),
            Some(_) => self.at_struct_field(name),
            None => Self::untyped(),
        }
    }

    pub(super) fn at_list_index(&self, index: usize) -> Self {
        self.with_segment(AttributePathSegment::ListIndex(index))
    }

    pub(super) fn at_key(&self, key: impl Into<String>) -> Self {
        self.with_segment(AttributePathSegment::Key(key.into()))
    }

    pub(super) fn at_struct_field(&self, name: impl Into<String>) -> Self {
        self.with_segment(AttributePathSegment::StructField(name.into()))
    }

    fn with_segment(&self, segment: AttributePathSegment) -> Self {
        let Some(sink) = &self.sink else {
            return Self::untyped();
        };
        Self {
            sink: Some(ExpressionSink {
                path: sink.path.with_segment(segment),
                // A structural handle is directly transportable only when it
                // is the complete module argument. Once parsing descends into
                // a list/map/struct leaf there is no runtime Value placeholder
                // to manufacture, so reserve the owning argument instead.
                structural: StructuralSinkMode::Pending,
                ..sink.clone()
            }),
        }
    }

    pub(super) fn pending_slot(&self) -> Option<PendingValueSlot> {
        self.sink.as_ref().map(|sink| PendingValueSlot {
            owner: sink.owner.clone(),
            path: sink.root_path.clone(),
        })
    }

    pub(super) fn record_bare_binding(
        &self,
        ctx: &ParseContext<'_>,
        name: &str,
        token: &pest::iterators::Pair<'_, Rule>,
    ) -> Option<PendingValueSlot> {
        let sink = self.sink.as_ref()?;
        let (binding, source) = match ctx.lookup_binding(name) {
            ParseBindingRef::Value {
                declaration,
                declared_type,
                ..
            } => (
                BareBindingKind::Value { declaration },
                BareBindingSource::Value {
                    value: ctx.shared_binding_value(name)?,
                    declared_type: declared_type.cloned(),
                },
            ),
            ParseBindingRef::Structural(handle) => {
                let resource_type = match handle.kind() {
                    StructuralHandleKind::Resource(resource_type) => Some(resource_type.clone()),
                    StructuralHandleKind::For
                    | StructuralHandleKind::If
                    | StructuralHandleKind::Use
                    | StructuralHandleKind::Module
                    | StructuralHandleKind::Wait
                    | StructuralHandleKind::UpstreamState
                    | StructuralHandleKind::Provider
                    | StructuralHandleKind::ModuleParameter
                    | StructuralHandleKind::FunctionParameter => None,
                };
                (
                    BareBindingKind::Structural {
                        kind: handle.kind().clone(),
                        resource_type,
                    },
                    BareBindingSource::Structural(handle.clone()),
                )
            }
            ParseBindingRef::Unbound => return None,
        };
        let authored = BareBindingUse {
            source_file: SourceFileId::new(sink.source_file.to_path_buf()),
            scope: ctx
                .binding_scope(name)
                .expect("a recorded binding use has a declaration scope")
                .clone(),
            owner: sink.owner.clone(),
            path: sink.path.clone(),
            name: name.to_string(),
            span: SourceSpan::from_pest(token, sink.preprocessed_file),
            binding,
        };
        let pending_slot = (matches!(&source, BareBindingSource::Structural(_))
            && sink.structural == StructuralSinkMode::Pending)
            .then(|| PendingValueSlot {
                owner: sink.owner.clone(),
                path: sink.root_path.clone(),
            });
        ctx.parsed_binding_uses
            .borrow_mut()
            .record(authored, source, pending_slot.clone());
        pending_slot
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
