//! Single source of truth for binding-name lookups (#2231 / #2251).
//!
//! Three sibling views live here, each answering a different question
//! about the same set of names declared in a parsed Carina configuration:
//!
//! - [`BindingNameSet`] — "is this identifier in scope at all?" View
//!   for the parser and LSP scope-check pass. Records all eight
//!   declaration kinds the parser tracks (resource, argument, module
//!   call, upstream state, import alias, user function, variable,
//!   structural binding from let-of-if/for/read).
//! - [`BindingIndex`] — "what statically known attribute surface does this
//!   binding point at?" Schema/composition-aware view. Used by validation
//!   and the LSP.
//! - [`ResolvedBindings`] — "what attribute values does this binding
//!   actually carry?" Value-aware view. Used by the resolver and
//!   executor. Includes upstream-state bindings that have no `Resource`
//!   or `ResourceSchema`.
//!
//! The three are separate types rather than fields on one because their
//! invariants differ — `BindingIndex` requires a typed validation target,
//! `ResolvedBindings` requires only attribute values (no schema),
//! `BindingNameSet` requires only the name and an origin tag. Callers that
//! need more than one view hold them side by side.
//!
//! Built once at the parse → validate boundary, then borrowed. Resource
//! discovery is **top-level only** (`iter_top_level_resources`) on purpose:
//! for-body template resources carry a parser-synthesised binding name
//! used for address derivation, but those names are an internal detail
//! and were never visible to validation's `binding_map` pre-#2231.
//! Surfacing them through `BindingIndex` would let ResourceRefs name
//! them, which is a behaviour change neither validation nor the LSP
//! wants. Wait bindings are then layered on top as aliases of their
//! target validation surface because `<wait>.<attr>` is addressable DSL.
//! The LSP still walks `iter_all_resources` separately for its own
//! checks (so for-body type/enum diagnostics fire); only the
//! binding-name table is scoped.
//!
//! ```ignore
//! let index = BindingIndex::from_parsed(&parsed, &registry);
//! if let Some(entry) = index.get("vpc") {
//!     // entry.target is the statically known validation surface
//! }
//! ```

use crate::data_source_value::{DataSourceValue, DataSourceValues};
use crate::parser::{BindingName, ResourceRef};
use crate::resource::{
    AccessPath, Composition, PathSegment, PlanInputCurrent, ReferencePathRef, Resource, ResourceId,
    State, Subscript, Value,
};
use crate::schema::{
    AttributeType, ResourceSchema, SchemaRegistry, Shape, TypeInSchema, suggest_similar_name,
};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Statically known validation surface for a binding.
#[derive(Debug, Clone, Copy)]
pub enum BindingTarget<'a> {
    Schema(&'a ResourceSchema),
    Composition(&'a Composition),
    ModuleCall(&'a crate::module_resolver::ResolvedModuleSignature),
    Argument(&'a crate::parser::TypeExpr),
    UpstreamState(&'a crate::upstream_exports::UpstreamExportEntries),
}

/// One entry in the binding index. The target is non-`Option`: managed/data
/// bindings whose schemas cannot be resolved are skipped, while compositions
/// always carry their declared attribute map directly.
#[derive(Debug)]
pub struct BindingEntry<'a> {
    pub target: BindingTarget<'a>,
}

/// Whether a path-shape mismatch is owned by this lookup or by a dedicated
/// diagnostic pass. Ordinary reference compatibility preserves the historic
/// upstream shape diagnostics; postfix validation requests a strict result
/// for schema/value-backed bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceTypeCheck {
    Compatibility,
    PostfixAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidAccessPolicy {
    Unchecked,
    Error,
}

impl ReferenceTypeCheck {
    fn invalid_access_policy(self, target: BindingTarget<'_>) -> InvalidAccessPolicy {
        match (self, target) {
            // Upstream shape errors carry purpose-built guidance such as
            // "iterate the list" and must remain authoritative.
            (Self::PostfixAccess, BindingTarget::UpstreamState(_)) | (Self::Compatibility, _) => {
                InvalidAccessPolicy::Unchecked
            }
            (
                Self::PostfixAccess,
                BindingTarget::Schema(_)
                | BindingTarget::Composition(_)
                | BindingTarget::ModuleCall(_)
                | BindingTarget::Argument(_),
            ) => InvalidAccessPolicy::Error,
        }
    }
}

/// One type constraint on a reference path, paired with the definition
/// context required to interpret any nested schema `Ref`s.
#[derive(Debug, Clone)]
struct ResolvedTypeConstraint {
    attr_type: AttributeType,
    defs: BTreeMap<String, AttributeType>,
}

impl ResolvedTypeConstraint {
    fn new(attr_type: AttributeType, defs: BTreeMap<String, AttributeType>) -> Self {
        Self { attr_type, defs }
    }

    fn type_in_schema(&self) -> TypeInSchema<'_> {
        self.attr_type.in_schema(&self.defs)
    }

    fn narrow(
        self,
        segments: &[PathSegment],
        path: &AccessPath,
        invalid_access: InvalidAccessPolicy,
    ) -> Result<Option<Self>, RefTypeError> {
        let rendered_path = path.to_dot_string();
        let narrowed = narrow_type(
            &self.attr_type,
            segments,
            &self.defs,
            &rendered_path,
            invalid_access,
        )?
        .cloned();
        Ok(narrowed.map(|attr_type| Self::new(attr_type, self.defs)))
    }
}

/// How the type exposed at the current reference boundary was established.
///
/// The variant is part of the constraint itself so a declared composition
/// boundary cannot lose (or accidentally gain) that provenance independently
/// of its type.
#[derive(Debug, Clone)]
enum AuthoritativeTypeConstraint {
    Direct(ResolvedTypeConstraint),
    DeclaredCompositionAttribute(ResolvedTypeConstraint),
}

impl AuthoritativeTypeConstraint {
    fn constraint(&self) -> &ResolvedTypeConstraint {
        match self {
            Self::Direct(constraint) | Self::DeclaredCompositionAttribute(constraint) => constraint,
        }
    }

    fn into_constraint(self) -> ResolvedTypeConstraint {
        match self {
            Self::Direct(constraint) | Self::DeclaredCompositionAttribute(constraint) => constraint,
        }
    }

    fn narrow(
        self,
        segments: &[PathSegment],
        path: &AccessPath,
        invalid_access: InvalidAccessPolicy,
    ) -> Result<Option<Self>, RefTypeError> {
        match self {
            Self::Direct(constraint) => Ok(constraint
                .narrow(segments, path, invalid_access)?
                .map(AuthoritativeTypeConstraint::Direct)),
            Self::DeclaredCompositionAttribute(constraint) => Ok(constraint
                .narrow(segments, path, invalid_access)?
                .map(AuthoritativeTypeConstraint::DeclaredCompositionAttribute)),
        }
    }

    fn is_declared_composition_attribute(&self) -> bool {
        matches!(self, Self::DeclaredCompositionAttribute(_))
    }
}

/// A reference-path type together with every constraint preserved across
/// composition boundaries.
///
/// `authoritative` is the source type exposed at the current boundary. For a
/// declared composition attribute it is the lifted declaration; the forwarded
/// source and all constraints accumulated by earlier composition attributes
/// live in `evidence`. Assignability must check both. This prevents an
/// unrefined declaration such as `String` from erasing incompatible refinement
/// evidence while keeping the declaration authoritative for consumers.
#[derive(Debug, Clone)]
pub struct ResolvedRefType {
    authoritative: AuthoritativeTypeConstraint,
    evidence: Vec<ResolvedTypeConstraint>,
}

/// The first source constraint that failed assignment to a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTypeMismatch {
    actual: String,
    declared: Option<String>,
}

impl RefTypeMismatch {
    pub fn actual_type_name(&self) -> &str {
        &self.actual
    }

    /// Present only when forwarded/inferred evidence failed after the
    /// authoritative declared type had already passed.
    pub fn declared_type_name(&self) -> Option<&str> {
        self.declared.as_deref()
    }

    pub fn origin_description(&self, path: ReferencePathRef<'_>) -> String {
        match &self.declared {
            Some(declared) => format!("from {}, declared {}", path.to_dot_string(), declared),
            None => format!("from {}", path.to_dot_string()),
        }
    }
}

impl ResolvedRefType {
    fn new(attr_type: AttributeType, defs: BTreeMap<String, AttributeType>) -> Self {
        Self {
            authoritative: AuthoritativeTypeConstraint::Direct(ResolvedTypeConstraint::new(
                attr_type, defs,
            )),
            evidence: Vec::new(),
        }
    }

    fn with_declared_type(declared: ResolvedTypeConstraint, inferred: Option<Self>) -> Self {
        let mut evidence = Vec::new();
        if let Some(inferred) = inferred {
            evidence.push(inferred.authoritative.into_constraint());
            evidence.extend(inferred.evidence);
        }
        Self {
            authoritative: AuthoritativeTypeConstraint::DeclaredCompositionAttribute(declared),
            evidence,
        }
    }

    fn narrow(
        self,
        segments: &[PathSegment],
        path: &AccessPath,
        invalid_access: InvalidAccessPolicy,
    ) -> Result<Option<Self>, RefTypeError> {
        let Some(authoritative) = self.authoritative.narrow(segments, path, invalid_access)? else {
            return Ok(None);
        };
        let evidence = self
            .evidence
            .into_iter()
            .map(|constraint| constraint.narrow(segments, path, invalid_access))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(Some(Self {
            authoritative,
            evidence,
        }))
    }

    /// Human-readable name of the authoritative boundary type.
    ///
    /// This is for diagnostics only. Call [`Self::is_assignable_to_sink`] for
    /// type compatibility so inferred evidence is not bypassed.
    pub fn display_type_name(&self) -> String {
        self.authoritative
            .constraint()
            .type_in_schema()
            .resolved_type_name()
    }

    /// Require the authoritative boundary type and every inferred constraint
    /// accumulated through forwarded composition attributes to satisfy
    /// `sink`. The returned mismatch identifies the first failing constraint;
    /// inferred failures also retain the current boundary's declaration for
    /// diagnostics.
    pub fn is_assignable_to_sink(&self, sink: TypeInSchema<'_>) -> Result<(), RefTypeMismatch> {
        let authoritative = self.authoritative.constraint().type_in_schema();
        if !authoritative.is_assignable_to(sink) {
            return Err(RefTypeMismatch {
                actual: authoritative.resolved_type_name(),
                declared: None,
            });
        }

        for evidence in &self.evidence {
            let evidence = evidence.type_in_schema();
            if !evidence.is_assignable_to(sink) {
                return Err(RefTypeMismatch {
                    actual: evidence.resolved_type_name(),
                    declared: self
                        .authoritative
                        .is_declared_composition_attribute()
                        .then(|| authoritative.resolved_type_name()),
                });
            }
        }
        Ok(())
    }
}

/// Failure to resolve a statically known attribute path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefTypeError {
    UnknownAttribute {
        binding: String,
        attribute: String,
        known_attributes: Vec<String>,
        target: RefTargetKind,
    },
    UnknownStructField {
        path: String,
        field: String,
        struct_name: String,
        known_fields: Vec<String>,
    },
    UnknownUpstreamExport {
        binding: String,
        export: String,
        known_exports: Vec<String>,
    },
    InvalidAccess {
        path: String,
        step: PathSegment,
        actual: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefTargetKind {
    Schema { resource_type: String },
    Composition,
}

impl std::fmt::Display for RefTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAttribute {
                binding,
                attribute,
                known_attributes,
                target: _,
            } => {
                let known: Vec<&str> = known_attributes.iter().map(String::as_str).collect();
                write!(
                    f,
                    "unknown attribute '{}' on '{}' in reference {}.{}{}",
                    attribute,
                    binding,
                    binding,
                    attribute,
                    suggestion_suffix(attribute, &known),
                )
            }
            Self::UnknownStructField {
                path,
                field,
                struct_name,
                known_fields,
            } => {
                let known: Vec<&str> = known_fields.iter().map(String::as_str).collect();
                write!(
                    f,
                    "unknown field '{}' on struct '{}' in reference {}; known fields: {}.{}",
                    field,
                    struct_name,
                    path,
                    known.join(", "),
                    suggestion_suffix(field, &known),
                )
            }
            Self::UnknownUpstreamExport {
                binding,
                export,
                known_exports,
            } => {
                let known: Vec<&str> = known_exports.iter().map(String::as_str).collect();
                let suggestion = suggest_similar_name(export, &known)
                    .map(|suggestion| format!(" Did you mean `{}`?", suggestion))
                    .unwrap_or_default();
                write!(
                    f,
                    "upstream_state `{}` does not export `{}`.{}",
                    binding, export, suggestion
                )
            }
            Self::InvalidAccess { path, step, actual } => {
                write!(
                    f,
                    "cannot apply access {step} to {actual} in reference {path}"
                )
            }
        }
    }
}

/// Static result for one complete resource-reference path.
#[derive(Debug, Clone)]
pub enum RefType {
    Typed(ResolvedRefType),
    /// The binding exists but no sound schema type can be represented. This
    /// includes derived composition values and `TypeExpr::Unknown`.
    Unchecked,
    UnknownBinding {
        binding: String,
        attribute: String,
    },
    UnknownAttribute(RefTypeError),
}

fn suggestion_suffix(unknown: &str, known: &[&str]) -> String {
    suggest_similar_name(unknown, known)
        .map(|suggestion| format!(" Did you mean '{}'?", suggestion))
        .unwrap_or_default()
}

/// Index of `binding_name → validation target` for every named binding
/// declared in `parsed`. Lifetime `'a` ties the index to its inputs so callers
/// can keep it borrowed without cloning.
///
/// `entries` contains typed arguments, upstream exports, compositions,
/// pre-expansion module calls with loaded signatures, and managed/data
/// bindings whose schemas resolved successfully. `known_names` records every
/// named addressable binding even when its type surface is unavailable, so
/// callers can distinguish "unknown binding" from "known but unchecked".
#[derive(Debug)]
pub struct BindingIndex<'a> {
    entries: HashMap<String, BindingEntry<'a>>,
    known_names: std::collections::HashSet<String>,
    resource_handles: HashMap<String, AttributeType>,
    user_functions: &'a HashMap<String, crate::parser::UserFunction>,
}

impl<'a> BindingIndex<'a> {
    /// Build the index from a parsed file and a [`SchemaRegistry`]. The
    /// registry handles the `(provider, resource_type, kind) -> schema`
    /// lookup, including the kind-aware split between managed resources
    /// and data sources.
    ///
    /// Managed/data bindings whose schema is missing from the registry are
    /// silently skipped — callers (validation / LSP) treat unknown resource
    /// types as a separate diagnostic, so reporting them again here would
    /// double-count. Composition bindings do not need a registry lookup.
    pub fn from_parsed<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
    ) -> Self {
        Self::build(parsed, registry, None, None)
    }

    /// Build the index with the declared output surfaces of imported modules.
    /// This is the pre-expansion authority for references such as `web.url`:
    /// the module call is not a [`Composition`] yet, but its imported
    /// signature already declares the output type.
    pub fn from_parsed_with_module_signatures<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self {
        Self::build(parsed, registry, None, Some(module_signatures))
    }

    /// Build the index with statically resolved `upstream_state` exports.
    /// Bindings whose source could not be loaded are still recorded as known
    /// and therefore resolve to [`RefType::Unchecked`].
    pub fn from_parsed_with_upstream_exports<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
        upstream_exports: &'a crate::upstream_exports::UpstreamExports,
    ) -> Self {
        Self::build(parsed, registry, Some(upstream_exports), None)
    }

    /// Build the complete pre-expansion validation index used by the LSP.
    pub fn from_parsed_with_upstream_exports_and_module_signatures<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
        upstream_exports: &'a crate::upstream_exports::UpstreamExports,
        module_signatures: &'a crate::module_resolver::ResolvedModuleSignatures,
    ) -> Self {
        Self::build(
            parsed,
            registry,
            Some(upstream_exports),
            Some(module_signatures),
        )
    }

    fn build<E>(
        parsed: &'a crate::parser::File<E>,
        registry: &'a SchemaRegistry,
        upstream_exports: Option<&'a crate::upstream_exports::UpstreamExports>,
        module_signatures: Option<&'a crate::module_resolver::ResolvedModuleSignatures>,
    ) -> Self {
        let mut entries = HashMap::new();
        let mut known_names = std::collections::HashSet::new();
        let mut resource_handles = HashMap::new();
        // Walk top-level resources only — managed resources, data
        // sources, and compositions (carina#3181: `iter_top_level_resources`
        // chains the typed slices and excludes deferred for-body
        // templates). The parser auto-generates a synthetic `binding` for
        // anonymous for-body templates (used for resource address
        // derivation), but those names are an internal detail — they were
        // never visible to validation's binding map pre-#2231 and
        // surfacing them here would be an unintended behaviour change for
        // ResourceRef lookups. The LSP and validation both still walk
        // `iter_all_resources` *separately* for their own checks; only
        // the binding-name table is scoped to top-level here.
        for rref in parsed.iter_top_level_resources() {
            let Some(binding_name) = rref.binding() else {
                continue;
            };
            known_names.insert(binding_name.to_string());
            if !matches!(rref, ResourceRef::Composition(_)) {
                let id = rref.id();
                resource_handles.insert(
                    binding_name.to_string(),
                    AttributeType::resource_handle(id.provider.clone(), id.resource_type.clone()),
                );
            }
            // Schema lookup routes by the typestate arm: managed resources
            // via `get_for`, data sources via `get_for_data_source`.
            // Compositions carry their own statically known attribute map.
            let target = match rref {
                ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => registry
                    .get_for(m)
                    .map(|schema| BindingTarget::Schema(schema.as_schema())),
                ResourceRef::DataSource(d) => registry
                    .get_for_data_source(d)
                    .map(|schema| BindingTarget::Schema(schema.as_schema())),
                ResourceRef::Composition(c) => Some(BindingTarget::Composition(c)),
            };
            let Some(target) = target else {
                continue;
            };
            entries.insert(binding_name.to_string(), BindingEntry { target });
        }
        // Module-call bindings expose the imported module's declared output
        // surface before expansion. Record the binding even when its import
        // could not be loaded, preserving the known-but-untyped distinction.
        // Resource/composition entries above retain precedence on malformed
        // duplicate names.
        for call in &parsed.module_calls {
            let Some(binding_name) = call.binding_name.as_deref() else {
                continue;
            };
            known_names.insert(binding_name.to_string());
            if let Some(signature) =
                module_signatures.and_then(|signatures| signatures.get(&call.module_name))
            {
                entries
                    .entry(binding_name.to_string())
                    .or_insert(BindingEntry {
                        target: BindingTarget::ModuleCall(signature),
                    });
            }
        }

        // Wait bindings are addressable aliases for their target
        // resource (`let ready = wait cert { ... }` means
        // `ready.certificate_arn` has the same validation surface as
        // `cert.certificate_arn`). The target binding has already been
        // processed above for managed resources and data sources. If the
        // target lacks an entry (unknown resource type or future parser
        // expansion), keep the wait name in `known_names` so callers preserve
        // the "known binding, validation target absent" diagnostic split.
        for wb in &parsed.wait_bindings {
            let wait_name = wb.binding.as_str();
            known_names.insert(wait_name.to_string());
            if let Some(target) = entries.get(wb.target.as_str()).map(|entry| entry.target) {
                entries.insert(wait_name.to_string(), BindingEntry { target });
            }
            if let Some(handle) = resource_handles.get(wb.target.as_str()).cloned() {
                resource_handles.insert(wait_name.to_string(), handle);
            }
        }

        // Module arguments are typed values in their own module scope. They
        // come after addressable resource bindings so malformed duplicate-name
        // input preserves BindingNameSet's resource-first precedence.
        for argument in &parsed.arguments {
            known_names.insert(argument.name.clone());
            entries
                .entry(argument.name.clone())
                .or_insert(BindingEntry {
                    target: BindingTarget::Argument(&argument.type_expr),
                });
        }

        // An upstream binding is addressable only when its source directory
        // resolved. Missing/unloadable sources remain known-but-untyped, while
        // a successfully loaded empty export map is a typed surface on which
        // every field access produces the canonical "does not export" error.
        for upstream in &parsed.upstream_states {
            known_names.insert(upstream.binding.clone());
            if let Some(exports) =
                upstream_exports.and_then(|all_exports| all_exports.get(&upstream.binding))
            {
                entries
                    .entry(upstream.binding.clone())
                    .or_insert(BindingEntry {
                        target: BindingTarget::UpstreamState(exports),
                    });
            }
        }
        Self {
            entries,
            known_names,
            resource_handles,
            user_functions: &parsed.user_functions,
        }
    }

    pub fn get(&self, name: &str) -> Option<&BindingEntry<'a>> {
        self.entries.get(name)
    }

    pub(crate) fn user_function(&self, name: &str) -> Option<&'a crate::parser::UserFunction> {
        self.user_functions.get(name)
    }

    /// Resolve the static type of one complete reference path.
    ///
    /// Schema bindings are narrowed through the path directly. Composition
    /// bindings preserve a liftable declared boundary type together with all
    /// inferable forwarded-source constraints; otherwise a forwarded output
    /// recursively inherits the target path's type. Derived values and
    /// `TypeExpr::Unknown` remains unchecked. The active path set
    /// bounds malformed/cyclic forwarded chains.
    pub fn ref_type<'p>(&self, path: impl Into<ReferencePathRef<'p>>) -> RefType {
        self.ref_type_inner(
            path.into(),
            &mut HashSet::new(),
            ReferenceTypeCheck::Compatibility,
        )
    }

    /// Resolve a reference for postfix type checking. Schema-backed bindings
    /// (including wait aliases) report wrong-shape suffixes, while upstream
    /// bindings retain their dedicated shape-diagnostic authority.
    pub(crate) fn postfix_ref_type<'p>(&self, path: impl Into<ReferencePathRef<'p>>) -> RefType {
        self.ref_type_inner(
            path.into(),
            &mut HashSet::new(),
            ReferenceTypeCheck::PostfixAccess,
        )
    }

    fn ref_type_inner(
        &self,
        reference: ReferencePathRef<'_>,
        visiting: &mut HashSet<String>,
        check: ReferenceTypeCheck,
    ) -> RefType {
        let key = reference.to_plaintext_dot_string();
        if !visiting.insert(key.clone()) {
            return RefType::Unchecked;
        }

        if let ReferencePathRef::Binding(binding) = reference
            && let Some(handle) = self.resource_handles.get(binding)
        {
            visiting.remove(&key);
            return RefType::Typed(ResolvedRefType::new(
                handle.clone(),
                crate::schema::empty_defs_for_schema_walks().clone(),
            ));
        }

        let result = match self.entries.get(reference.binding()) {
            None if self.known_names.contains(reference.binding()) => RefType::Unchecked,
            None => RefType::UnknownBinding {
                binding: reference.binding().to_string(),
                attribute: match reference {
                    ReferencePathRef::Access(path) => path.attribute().to_string(),
                    ReferencePathRef::Binding(_) | ReferencePathRef::BindingAccess { .. } => {
                        String::new()
                    }
                },
            },
            Some(entry) => match entry.target {
                BindingTarget::Schema(schema) => {
                    let invalid_access = check.invalid_access_policy(entry.target);
                    let ReferencePathRef::Access(path) = reference else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let Some(attribute) = schema.attributes.get(path.attribute()) else {
                        visiting.remove(&key);
                        return RefType::UnknownAttribute(unknown_attribute(
                            path,
                            schema.attributes.keys(),
                            RefTargetKind::Schema {
                                resource_type: schema.resource_type.clone(),
                            },
                        ));
                    };
                    let rendered_path = path.to_dot_string();
                    match narrow_type(
                        &attribute.attr_type,
                        path.segments(),
                        &schema.defs,
                        &rendered_path,
                        invalid_access,
                    ) {
                        Ok(Some(attr_type)) => RefType::Typed(ResolvedRefType::new(
                            attr_type.clone(),
                            schema.defs.clone(),
                        )),
                        Ok(None) => RefType::Unchecked,
                        Err(error) => RefType::UnknownAttribute(error),
                    }
                }
                BindingTarget::Composition(composition) => {
                    let invalid_access = check.invalid_access_policy(entry.target);
                    let ReferencePathRef::Access(path) = reference else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let Some(attribute) = composition.signature.attributes.get(path.attribute())
                    else {
                        visiting.remove(&key);
                        return RefType::UnknownAttribute(unknown_attribute(
                            path,
                            composition.signature.attributes.keys(),
                            RefTargetKind::Composition,
                        ));
                    };

                    if let Some(declared) = attribute
                        .declared_type()
                        .and_then(crate::validation::lift_type_expr)
                    {
                        let defs = crate::schema::empty_defs_for_schema_walks();
                        let rendered_path = path.to_dot_string();
                        match narrow_type(
                            &declared,
                            path.segments(),
                            defs,
                            &rendered_path,
                            invalid_access,
                        ) {
                            Ok(Some(attr_type)) => {
                                let inferred = attribute.forwarded_path().and_then(|forwarded| {
                                    match self.ref_type_inner(
                                        ReferencePathRef::Access(forwarded),
                                        visiting,
                                        check,
                                    ) {
                                        RefType::Typed(resolved) => resolved
                                            .narrow(path.segments(), path, invalid_access)
                                            .ok()
                                            .flatten(),
                                        RefType::Unchecked
                                        | RefType::UnknownBinding { .. }
                                        | RefType::UnknownAttribute(_) => None,
                                    }
                                });
                                RefType::Typed(ResolvedRefType::with_declared_type(
                                    ResolvedTypeConstraint::new(attr_type.clone(), defs.clone()),
                                    inferred,
                                ))
                            }
                            Ok(None) => RefType::Unchecked,
                            Err(error) => RefType::UnknownAttribute(error),
                        }
                    } else if attribute.source_argument().is_some()
                        && attribute.declared_type().is_some()
                    {
                        // The declaration is the source of truth for a directly
                        // forwarded module argument. An unknown declaration
                        // stays unchecked rather than inheriting the caller's
                        // substituted value type.
                        RefType::Unchecked
                    } else if let Some(forwarded) = attribute.forwarded_path() {
                        match self.ref_type_inner(
                            ReferencePathRef::Access(forwarded),
                            visiting,
                            check,
                        ) {
                            RefType::Typed(resolved) => {
                                match resolved.narrow(path.segments(), path, invalid_access) {
                                    Ok(Some(resolved)) => RefType::Typed(resolved),
                                    Ok(None) => RefType::Unchecked,
                                    Err(error) => RefType::UnknownAttribute(error),
                                }
                            }
                            other => other,
                        }
                    } else {
                        RefType::Unchecked
                    }
                }
                BindingTarget::ModuleCall(signature) => {
                    let invalid_access = check.invalid_access_policy(entry.target);
                    let ReferencePathRef::Access(path) = reference else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let Some(declared) = signature.attributes.get(path.attribute()) else {
                        visiting.remove(&key);
                        return RefType::UnknownAttribute(unknown_attribute(
                            path,
                            signature.attributes.keys(),
                            RefTargetKind::Composition,
                        ));
                    };
                    let Some(declared) = declared
                        .as_ref()
                        .and_then(crate::validation::lift_type_expr)
                    else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let defs = crate::schema::empty_defs_for_schema_walks();
                    let rendered_path = path.to_dot_string();
                    match narrow_type(
                        &declared,
                        path.segments(),
                        defs,
                        &rendered_path,
                        invalid_access,
                    ) {
                        Ok(Some(attr_type)) => {
                            RefType::Typed(ResolvedRefType::new(attr_type.clone(), defs.clone()))
                        }
                        Ok(None) => RefType::Unchecked,
                        Err(error) => RefType::UnknownAttribute(error),
                    }
                }
                BindingTarget::Argument(type_expr) => {
                    let invalid_access = check.invalid_access_policy(entry.target);
                    // A resource-handle argument is substituted with the
                    // caller's concrete resource binding during module
                    // expansion. Its `.attribute` suffix therefore belongs
                    // to that resource schema, not to the string-backed
                    // transport type used for the module boundary. Keep the
                    // pre-expansion source-local check neutral; the expanded
                    // ResourceRef is checked against the caller schema.
                    if matches!(
                        type_expr,
                        crate::parser::TypeExpr::Ref(_)
                            | crate::parser::TypeExpr::DottedUnresolved(_)
                    ) && !matches!(reference, ReferencePathRef::Binding(_))
                    {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    }
                    let Some(declared) = crate::validation::lift_type_expr(type_expr) else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let defs = crate::schema::empty_defs_for_schema_walks();
                    match reference {
                        ReferencePathRef::Binding(_) => {
                            RefType::Typed(ResolvedRefType::new(declared, defs.clone()))
                        }
                        ReferencePathRef::Access(path) => {
                            let mut segments = Vec::with_capacity(path.segments().len() + 1);
                            segments.push(PathSegment::Field {
                                name: path.attribute().to_string(),
                            });
                            segments.extend_from_slice(path.segments());
                            let rendered_path = path.to_dot_string();
                            match narrow_type(
                                &declared,
                                &segments,
                                defs,
                                &rendered_path,
                                invalid_access,
                            ) {
                                Ok(Some(attr_type)) => RefType::Typed(ResolvedRefType::new(
                                    attr_type.clone(),
                                    defs.clone(),
                                )),
                                Ok(None) => RefType::Unchecked,
                                Err(error) => RefType::UnknownAttribute(error),
                            }
                        }
                        ReferencePathRef::BindingAccess { segments, .. } => {
                            let rendered_path = reference.to_dot_string();
                            match narrow_type(
                                &declared,
                                segments,
                                defs,
                                &rendered_path,
                                invalid_access,
                            ) {
                                Ok(Some(attr_type)) => RefType::Typed(ResolvedRefType::new(
                                    attr_type.clone(),
                                    defs.clone(),
                                )),
                                Ok(None) => RefType::Unchecked,
                                Err(error) => RefType::UnknownAttribute(error),
                            }
                        }
                    }
                }
                BindingTarget::UpstreamState(exports) => {
                    let invalid_access = check.invalid_access_policy(entry.target);
                    let ReferencePathRef::Access(path) = reference else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let Some(export) = exports.get(path.attribute()) else {
                        let mut known_exports: Vec<String> = exports.keys().cloned().collect();
                        known_exports.sort();
                        visiting.remove(&key);
                        return RefType::UnknownAttribute(RefTypeError::UnknownUpstreamExport {
                            binding: path.binding().to_string(),
                            export: path.attribute().to_string(),
                            known_exports,
                        });
                    };
                    if !export.has_declared_type {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    }
                    let Some(declared) = export
                        .type_expr
                        .as_ref()
                        .and_then(crate::validation::lift_type_expr)
                    else {
                        visiting.remove(&key);
                        return RefType::Unchecked;
                    };
                    let defs = crate::schema::empty_defs_for_schema_walks();
                    let rendered_path = path.to_dot_string();
                    match narrow_type(
                        &declared,
                        path.segments(),
                        defs,
                        &rendered_path,
                        invalid_access,
                    ) {
                        Ok(Some(attr_type)) => {
                            RefType::Typed(ResolvedRefType::new(attr_type.clone(), defs.clone()))
                        }
                        Ok(None) => RefType::Unchecked,
                        Err(error) => RefType::UnknownAttribute(error),
                    }
                }
            },
        };

        visiting.remove(&key);
        result
    }

    /// True iff a binding by this name was declared anywhere in the parsed
    /// file, *even if its schema could not be resolved*. Used to
    /// distinguish "unknown binding" diagnostics from "known binding but
    /// schema missing" (which is a different diagnostic surface).
    pub fn is_declared(&self, name: &str) -> bool {
        self.known_names.contains(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrow-valued projection — `binding_name → &ResourceSchema`. Lets
    /// callers that previously took a `HashMap<String, ResourceSchema>` of
    /// schema clones switch to a borrow-only map without changing their
    /// signatures' shape (just the value type). Saves a per-binding,
    /// per-keystroke `ResourceSchema::clone()` on the LSP path.
    pub fn schemas_by_name(&self) -> HashMap<&str, &'a ResourceSchema> {
        self.entries
            .iter()
            .filter_map(|(name, entry)| match entry.target {
                BindingTarget::Schema(schema) => Some((name.as_str(), schema)),
                BindingTarget::Composition(_)
                | BindingTarget::ModuleCall(_)
                | BindingTarget::Argument(_)
                | BindingTarget::UpstreamState(_) => None,
            })
            .collect()
    }
}

fn unknown_attribute<'a>(
    path: &AccessPath,
    known: impl IntoIterator<Item = &'a String>,
    target: RefTargetKind,
) -> RefTypeError {
    RefTypeError::UnknownAttribute {
        binding: path.binding().to_string(),
        attribute: path.attribute().to_string(),
        known_attributes: known.into_iter().cloned().collect(),
        target,
    }
}

fn narrow_type<'a>(
    start: &'a AttributeType,
    segments: &[PathSegment],
    defs: &'a BTreeMap<String, AttributeType>,
    full_path: &str,
    invalid_access: InvalidAccessPolicy,
) -> Result<Option<&'a AttributeType>, RefTypeError> {
    let mut current = start;
    for segment in segments {
        current = match (segment, current.shape_with_defs(defs)) {
            (PathSegment::Field { name }, Shape::Struct { name: struct_name }) => {
                let fields = crate::schema::struct_fields_with_defs(current, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                let Some(field) = fields.iter().find(|field| field.name == *name) else {
                    return Err(RefTypeError::UnknownStructField {
                        path: full_path.to_string(),
                        field: name.clone(),
                        struct_name: struct_name.to_string(),
                        known_fields: fields.iter().map(|field| field.name.clone()).collect(),
                    });
                };
                &field.field_type
            }
            (PathSegment::Field { .. }, Shape::Map { value, .. }) => value,
            (
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
                Shape::List { element_type, .. },
            ) => element_type,
            (
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
                Shape::Map { value, .. },
            ) => value,
            (_, Shape::Union) => return Ok(None),
            _ => match invalid_access {
                // Existing upstream reference paths have dedicated shape
                // diagnostics. Preserve that richer message by leaving the
                // compatibility lookup unchecked.
                InvalidAccessPolicy::Unchecked => return Ok(None),
                InvalidAccessPolicy::Error => {
                    return Err(RefTypeError::InvalidAccess {
                        path: full_path.to_string(),
                        step: segment.clone(),
                        actual: current.in_schema(defs).resolved_type_name(),
                    });
                }
            },
        };
    }
    Ok(Some(current))
}

/// Origin of a binding name in [`BindingNameSet`]. Eight kinds, one per
/// declaration form a parsed Carina configuration can introduce.
///
/// Lives on `BindingNameSet` (the scope-only view) rather than on
/// [`BindingValueSource`] (the value-aware view) because not every kind
/// has a value: `Use` (import alias) and `UserFunction` are pure
/// identifiers that the resolver/executor never need to look up
/// attributes on.
///
/// `#[non_exhaustive]` so adding a new declaration form (a future
/// `data` block, etc.) does not break downstream `match` arms.
///
/// Not `Copy`: `Wait` carries an owned `BindingName` (the wait→target
/// edge is a typed value, carina#3085 — not a string convention). The
/// enum is read out by reference (`kind()` / `iter()`); the only
/// in-crate consumers are within this module, so dropping `Copy` is
/// contained.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BindingNameKind {
    /// `let <name> = ...` resource binding.
    Resource,
    /// `argument <name> { ... }` module argument.
    Argument,
    /// `module <name> "..." { ... }` call site.
    ModuleCall,
    /// `upstream_state <name> "..." { ... }` data source.
    UpstreamState,
    /// `use <alias> from "..."` import.
    Use,
    /// `function <name>(...) { ... }` user-defined function.
    UserFunction,
    /// `variable <name> { ... }` config variable.
    Variable,
    /// Iteration / condition binding from `for <name> in ...` or
    /// equivalent structural form. Recorded for scope checks but not
    /// addressable by `ResourceRef` (see the module doc for the
    /// pre-existing invariant).
    Structural,
    /// `let <name> = wait <target> { ... }` wait binding (carina#3085).
    /// Unlike `Structural`, a wait binding **is** addressable by
    /// `ResourceRef`: `<name>.<attr>` is a passthrough to
    /// `<target>.<attr>` (see `notes/specs/2026-05-09-wait-construct-design.md`
    /// value semantics). The `target` is the binding name of the
    /// resource the wait observes; carried here as a typed
    /// [`BindingName`] so the wait→target edge cannot be confused with
    /// an arbitrary string or point at a non-existent name undetected.
    Wait { target: BindingName },
}

/// Name-only scope view: every identifier a parsed Carina configuration
/// has brought into scope, tagged with its declaration kind.
///
/// This is the canonical replacement for the parser's
/// `collect_known_bindings_merged` helper (#2104 / #2301). The two
/// views ([`BindingIndex`] and [`ResolvedBindings`]) carry richer data
/// (schema; resolved values) but are restricted to bindings that
/// actually carry such data. `BindingNameSet` is the broadest of the
/// three — it answers "is this identifier in scope at all?" for
/// parser- and LSP-side identifier-scope diagnostics.
///
/// The set is built once from a [`ParsedFile`] (the merged
/// directory-level view) and borrowed thereafter; names are owned
/// `String`s so the set survives independent of the source.
#[derive(Debug, Default, Clone)]
pub struct BindingNameSet {
    by_name: HashMap<String, BindingNameKind>,
}

impl BindingNameSet {
    /// Build the set from a merged [`ParsedFile`], populating every
    /// declaration form the parser tracks.
    ///
    /// **Kind precedence on collisions**: a single name can appear in
    /// more than one parser-side map by design — for example, a `let
    /// vpc = aws.ec2.Vpc { ... }` registers `vpc` in both
    /// `parsed.resources` (as a resource binding) *and*
    /// `parsed.variables` (as a let-value binding, holding a
    /// placeholder ref). To keep the kind label meaningful, sources are
    /// inserted in **most-specific-first** order and the first
    /// `insert` wins (later sources call `entry().or_insert`).
    ///
    /// The order — `Resource` → `ModuleCall` → `UpstreamState` →
    /// `Argument` → `Use` → `UserFunction` → `Structural` → `Variable`
    /// — keeps `Variable` last because `parsed.variables` is the
    /// catch-all "any `let`-RHS placeholder lives here" map, while the
    /// preceding sources are narrower declaration forms.
    pub fn from_parsed<E>(parsed: &crate::parser::File<E>) -> Self {
        let mut by_name: HashMap<String, BindingNameKind> = HashMap::new();

        // carina#3181: walk the typed top-level slices — managed
        // resources, data sources, and compositions all register a
        // `Resource`-kind binding name (a data source declared as
        // `let x = read ...` is addressable by `ResourceRef`, and a
        // composition binding was a `Resource`-kind name before the
        // typestate split too).
        for rref in parsed.iter_top_level_resources() {
            if let Some(name) = rref.binding() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::Resource);
            }
        }
        // Wait bindings register right after resources: a wait binding
        // is addressable by `ResourceRef` (passthrough to its target),
        // so it belongs with the addressable forms, not with
        // `Structural`. The parser enforces distinct binding names, so
        // a name is a wait binding xor a resource binding — ordering
        // here is documentation, not correctness-critical (carina#3085).
        for wb in &parsed.wait_bindings {
            by_name
                .entry(wb.binding.as_str().to_string())
                .or_insert(BindingNameKind::Wait {
                    target: wb.target.clone(),
                });
        }
        for call in &parsed.module_calls {
            if let Some(name) = call.binding_name.as_deref() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::ModuleCall);
            }
        }
        for upstream in &parsed.upstream_states {
            by_name
                .entry(upstream.binding.clone())
                .or_insert(BindingNameKind::UpstreamState);
        }
        for arg in &parsed.arguments {
            by_name
                .entry(arg.name.clone())
                .or_insert(BindingNameKind::Argument);
        }
        for use_decl in &parsed.uses {
            by_name
                .entry(use_decl.alias.clone())
                .or_insert(BindingNameKind::Use);
        }
        for fn_name in parsed.user_functions.keys() {
            by_name
                .entry(fn_name.clone())
                .or_insert(BindingNameKind::UserFunction);
        }
        for structural in &parsed.structural_bindings {
            by_name
                .entry(structural.clone())
                .or_insert(BindingNameKind::Structural);
        }
        // Parser-classified structural handles include aliases such as
        // `let network = vpc`. Aliases do not have their own resource row and
        // deliberately stay out of `parsed.variables`, so the parser metadata
        // is their declaration authority. This metadata is merged across all
        // sibling files by the directory loader.
        for structural in parsed.parser_binding_metadata.structural_handles().keys() {
            by_name
                .entry(structural.clone())
                .or_insert(BindingNameKind::Structural);
        }
        for var_name in parsed.variables.keys() {
            by_name
                .entry(var_name.clone())
                .or_insert(BindingNameKind::Variable);
        }

        Self { by_name }
    }

    /// `true` iff `name` is declared somewhere in the parsed file.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Declaration kind for `name`, or `None` if the name is not
    /// in scope.
    pub fn kind(&self, name: &str) -> Option<&BindingNameKind> {
        self.by_name.get(name)
    }

    /// Iterate every in-scope name. Order is unspecified.
    pub fn iter_names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Iterate every (name, kind) pair. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &BindingNameKind)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// A `wait` binding's value-layer contract: `binding`'s
/// `<binding>.<attr>` is a passthrough to `<target>.<attr>`
/// (carina#3085). Both sides are the typed [`BindingName`] newtype so
/// the wait→target edge cannot be confused with an arbitrary string.
///
/// This is the *only* thing `ResolvedBindings` needs from a wait
/// declaration — deliberately **not** the full parser
/// [`crate::parser::WaitBinding`] (which also carries the `until`
/// predicate, timeout, `depends_on`, source line — all effect-layer /
/// diagnostics concerns irrelevant to value resolution). Decoupling
/// here means: (a) carina-core resolution does not depend on the
/// parser AST shape, and (b) the apply-from-plan-file path — which
/// reconstructs from a serialized `PlanFile` and has no `WaitBinding`
/// — can supply the same typed spec without resurrecting the AST.
/// `WaitBinding` provides a `From` conversion (the plan path); the
/// plan-file path builds it from its serialized `(binding, target)`
/// pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitAliasSpec {
    pub binding: BindingName,
    pub target: BindingName,
}

impl From<&crate::parser::WaitBinding> for WaitAliasSpec {
    fn from(wb: &crate::parser::WaitBinding) -> Self {
        Self {
            binding: wb.binding.clone(),
            target: wb.target.clone(),
        }
    }
}

/// Where the values for a binding came from. `Local` means a `let` binding
/// in the current configuration; `Upstream` means an `upstream_state` data
/// source bringing values in from another state file.
///
/// Marked `#[non_exhaustive]` because #2301 will introduce structural
/// sources (`for` / `if` / module) — flagging the intent now keeps
/// downstream `match` arms from becoming exhaustive against the current
/// two variants.
///
/// Not `Copy`: `WaitAlias` carries an owned `BindingName` (the
/// wait→target edge is a typed value, carina#3085). `source()` returns
/// a borrow; the only in-crate value-position use is a `matches!`
/// (`Copy`-independent), so dropping `Copy` is contained.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingValueSource {
    Local,
    Upstream,
    /// The binding is a `wait` binding whose `<name>.<attr>` is a
    /// passthrough to `target`'s attributes (carina#3085). The
    /// attributes stored alongside this source are a snapshot of the
    /// target's resolved attributes at construction time (see
    /// `notes/specs/2026-05-09-wait-construct-design.md` value
    /// semantics — "snapshot of the target captured by the read() that
    /// satisfied until"). Retained as a distinct source (not flattened
    /// to `Local`) so "did this value come *through* a wait?" stays
    /// observable — the dependency edge is handled separately by
    /// `Effect::Wait` lowering and is not affected by this value-layer
    /// alias.
    WaitAlias {
        target: BindingName,
    },
}

/// One entry in [`ResolvedBindings`]: the merged attribute map and the
/// source it came from. Stored as a single struct (rather than two
/// parallel maps) so the type system enforces "every name has attributes,
/// provenance, and an explicit completeness state".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingCompleteness {
    /// The snapshot was successfully loaded/read/applied. A missing key is
    /// definitive absence and may select a coalesce default.
    Complete,
    /// The snapshot is provisional or unavailable. A missing key is unknown
    /// and must not select a coalesce default.
    Incomplete,
}

/// Plan-derived knowledge about a managed resource's refreshed attributes.
///
/// A successful refresh is a complete snapshot when the differ schedules no
/// mutation. During an update, directly changed attributes identify where the
/// desired view must replace (or remove) the refreshed value. Provider-
/// populated attributes retain a present refreshed value but do not make a
/// missing value definitive. Every other refreshed value and absence remains
/// authoritative. Creates, replacements, and unavailable current rows use
/// [`Self::Unavailable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedBindingStability {
    NoPlannedEffect,
    Updating {
        /// Attributes directly written or removed by the planned update.
        /// Their authored desired values/absence override refreshed state;
        /// unresolved authored values remain deferred through ordinary value
        /// resolution.
        changed_attributes: HashSet<String>,
        /// Provider-populated attributes which may be refreshed as a side
        /// effect of the update. A value present in refreshed prior state is
        /// still usable (matching ordinary reference resolution), while a
        /// missing value is not definitive absence.
        refreshable_attributes: HashSet<String>,
    },
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub attributes: HashMap<String, Value>,
    pub source: BindingValueSource,
    pub completeness: BindingCompleteness,
    /// Attributes whose present values are authoritative even when the
    /// surrounding snapshot is incomplete. For managed resources these are
    /// desired/configured attributes; provider outputs merged from an old or
    /// in-flight snapshot are deliberately excluded.
    definitive_attributes: HashSet<String>,
    /// Attributes directly changed by a planned update. This records which
    /// refreshed keys were replaced/removed by the authored desired view.
    changed_attributes: HashSet<String>,
    /// Provider-populated attributes which may be refreshed by a planned
    /// update. Present prior-state values remain usable; only their absence
    /// is unknown until refresh/apply completes.
    refreshable_attributes: HashSet<String>,
    /// Secret-bearing refreshed attributes that this update explicitly
    /// removes. They are absent from the effective desired view, but callers
    /// which validate required pre-apply references must retain the taint so
    /// an unresolved reference cannot make a secret export appear public.
    removed_sensitive_attributes: HashSet<String>,
}

/// Attribute values whose provider plaintext has been reconciled with the
/// fully resolved desired secret shape.
///
/// The inner map is deliberately private. Executor binding updates accept
/// this witness instead of an arbitrary `HashMap`, so provider-ready values
/// (whose `Secret` wrappers have already been removed) cannot accidentally be
/// installed as the source for downstream references.
#[derive(Debug, Clone, PartialEq)]
pub struct SecretPreservingAttributes {
    attributes: HashMap<String, Value>,
}

impl SecretPreservingAttributes {
    /// Combine resolved desired values with the provider-facing values made
    /// from them. Desired-only keys are retained (notably write-only inputs),
    /// while provider values win after inheriting every secret marker.
    pub(crate) fn from_provider_values(
        resolved_shapes: &HashMap<String, Value>,
        provider_values: &HashMap<String, Value>,
    ) -> Self {
        let mut attributes = resolved_shapes.clone();
        for (key, value) in provider_values {
            let protected = resolved_shapes.get(key).map_or_else(
                || value.clone(),
                |shape| crate::value::mask_plaintext_using_secret_shape_fail_closed(value, shape),
            );
            attributes.insert(key.clone(), protected);
        }
        Self { attributes }
    }

    /// Read-only access for state writeback. Construction remains confined to
    /// the secret-preserving resolution paths above.
    pub fn values(&self) -> &HashMap<String, Value> {
        &self.attributes
    }
}

/// Value-aware sibling of [`BindingIndex`]. See the module-level doc for
/// why this is a separate type.
///
/// Owned, not borrowed: building the merged map requires combining
/// `Resource.attributes` with `State.attributes`, and there is no single
/// upstream `HashMap<String, HashMap<String, Value>>` already on hand to
/// borrow from. Owning the merged map avoids a self-referential
/// structure.
#[derive(Debug, Default, Clone)]
pub struct ResolvedBindings {
    by_name: HashMap<String, ResolvedBinding>,
}

/// Required inputs for [`ResolvedBindings::pre_apply`] (carina#3248).
///
/// All fields are mandatory; no `Default`, no `Option`. This shape
/// turns "forgot compositions at a new call site" into a compile error —
/// a struct-literal with a missing field fails to compile, so the
/// pre-apply path cannot be silently constructed without the kind
/// of binding sources it needs (carina#3246).
///
/// Slices and maps are borrowed: the constructor reads them once and
/// stores owned copies internally, so the caller retains ownership
/// without forcing a clone at the API boundary.
pub struct PreApplyInputs<'a> {
    pub managed: &'a [Resource],
    pub compositions: &'a [Composition],
    pub managed_current: &'a HashMap<ResourceId, PlanInputCurrent>,
    pub data_source_values: &'a DataSourceValues,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [WaitAliasSpec],
}

/// Inputs available while data-source queries are still being resolved.
///
/// This is deliberately distinct from [`PreApplyInputs`]: data sources in
/// this view contribute only their authored input attributes and are always
/// incomplete. Once provider reads have been classified, all plan/apply
/// binding construction must cross the total [`DataSourceValues`] boundary.
pub struct PreDataSourceReadInputs<'a> {
    pub managed: &'a [Resource],
    pub compositions: &'a [Composition],
    pub data_sources: &'a [crate::resource::DataSource],
    pub managed_current: &'a HashMap<ResourceId, PlanInputCurrent>,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [WaitAliasSpec],
}

impl ResolvedBindings {
    /// Build the value-level binding view used by validation and editor
    /// diagnostics.
    ///
    /// Unlike plan/apply there is no provider state to layer in: the DSL's
    /// managed resources, data sources, expanded compositions, and wait
    /// aliases are the complete value authority available at this phase.
    /// Keeping this constructor next to [`Self::pre_apply`] prevents CLI and
    /// LSP validation from assembling subtly different binding maps.
    pub fn for_validation<E>(parsed: &crate::parser::File<E>) -> Self {
        let wait_aliases = parsed
            .wait_bindings
            .iter()
            .map(WaitAliasSpec::from)
            .collect::<Vec<_>>();
        Self::pre_data_source_read(PreDataSourceReadInputs {
            managed: &parsed.resources,
            compositions: &parsed.compositions,
            data_sources: &parsed.data_sources,
            managed_current: &HashMap::new(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &wait_aliases,
        })
    }

    /// Build the explicitly incomplete value view used to resolve query
    /// inputs before provider reads run.
    pub fn pre_data_source_read(inputs: PreDataSourceReadInputs<'_>) -> Self {
        let mut bindings = Self::build_managed_core(
            inputs.managed,
            inputs.managed_current,
            inputs.remote_bindings,
            &HashMap::new(),
        );
        bindings.layer_unread_data_source_bindings(inputs.data_sources);
        bindings.layer_compositions(inputs.compositions);
        bindings.layer_wait_aliases(inputs.wait_aliases);
        bindings
    }

    /// Borrowing variant for a managed view assembled from multiple owners.
    pub fn pre_data_source_read_from_managed<'a>(
        managed: impl IntoIterator<Item = &'a Resource>,
        compositions: &[Composition],
        data_sources: &[crate::resource::DataSource],
        managed_current: &HashMap<ResourceId, PlanInputCurrent>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[WaitAliasSpec],
    ) -> Self {
        let mut bindings = Self::build_managed_core_from_iter(
            managed,
            managed_current,
            remote_bindings,
            &HashMap::new(),
        );
        bindings.layer_unread_data_source_bindings(data_sources);
        bindings.layer_compositions(compositions);
        bindings.layer_wait_aliases(wait_aliases);
        bindings
    }

    /// Borrowing pre-apply variant for a managed view assembled from multiple
    /// owners. Unlike the unread-query constructor, this requires the total
    /// typed data-source outcomes and therefore exposes completed read values.
    pub fn pre_apply_from_managed<'a>(
        managed: impl IntoIterator<Item = &'a Resource>,
        compositions: &[Composition],
        managed_current: &HashMap<ResourceId, PlanInputCurrent>,
        data_source_values: &DataSourceValues,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        wait_aliases: &[WaitAliasSpec],
    ) -> Self {
        let mut bindings = Self::build_managed_core_from_iter(
            managed,
            managed_current,
            remote_bindings,
            &HashMap::new(),
        );
        bindings.layer_data_source_bindings(data_source_values);
        bindings.layer_compositions(compositions);
        bindings.layer_wait_aliases(wait_aliases);
        bindings
    }

    /// Single typed pre-apply constructor (carina#3248).
    ///
    /// Builds the bindings view from every kind of binding that
    /// reference resolution can name — managed resources, composition
    /// resources (module-call attribute containers), and data
    /// sources — plus the managed/data-source state maps, `remote_bindings`,
    /// and `wait_aliases` inputs the legacy entries took.
    ///
    /// The required-fields `PreApplyInputs` struct turns "forgot to
    /// include compositions at a new call site" into a compile error
    /// rather than a runtime symptom: any missing field on the
    /// struct-literal fails to compile, so a new pre-apply call site
    /// cannot accidentally lose the composition / data-source layer the
    /// way the previous managed-only constructor + opt-in
    /// `add_compositions` shape allowed (carina#3246).
    ///
    /// Layering order: managed first (merged with `managed_current`,
    /// DSL-wins-on-collision), then data sources, then compositions,
    /// then wait aliases. The post-apply layering in
    /// `state_writeback.rs` uses the same order, so a same-stack
    /// collision resolves identically on the pre-apply and post-apply
    /// sides. Same-file collisions are rejected by the parser and
    /// cross-file collisions by the directory loader's duplicate-
    /// declaration validation, so the order is observable only in test
    /// code that constructs colliding inputs by hand.
    pub fn pre_apply(inputs: PreApplyInputs<'_>) -> Self {
        Self::pre_apply_with_stability(inputs, &HashMap::new())
    }

    /// Build the pre-apply view with plan-derived per-resource stability.
    ///
    /// An omitted entry means that no mutating effect is planned for a
    /// successfully refreshed resource. Callers which already have a plan (or
    /// the differ's pre-expansion projection) pass every mutating resource in
    /// `managed_stability`.
    pub fn pre_apply_with_stability(
        inputs: PreApplyInputs<'_>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
    ) -> Self {
        let mut bindings = Self::build_managed_core(
            inputs.managed,
            inputs.managed_current,
            inputs.remote_bindings,
            managed_stability,
        );
        bindings.layer_data_source_bindings(inputs.data_source_values);
        bindings.layer_compositions(inputs.compositions);
        bindings.layer_wait_aliases(inputs.wait_aliases);
        bindings
    }

    /// Shared managed + remote-bindings layering used by both the
    /// pre-apply constructor and the legacy entries during the
    /// migration window. Wait-aliases are layered separately by the
    /// caller after data sources / compositions so a wait whose target is
    /// a data source or composition still snapshots the resolved attrs.
    fn build_managed_core(
        managed: &[Resource],
        managed_current: &HashMap<ResourceId, PlanInputCurrent>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
    ) -> Self {
        Self::build_managed_core_from_iter(
            managed.iter(),
            managed_current,
            remote_bindings,
            managed_stability,
        )
    }

    fn build_managed_core_from_iter<'a>(
        managed: impl IntoIterator<Item = &'a Resource>,
        managed_current: &HashMap<ResourceId, PlanInputCurrent>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
    ) -> Self {
        let mut by_name: HashMap<String, ResolvedBinding> = HashMap::new();

        for resource in managed {
            let Some(binding_name) = resource.binding.as_ref() else {
                continue;
            };
            let mut merged: HashMap<String, Value> = resource.resolved_attributes();
            let definitive_attributes: HashSet<String> = merged.keys().cloned().collect();
            let (changed_attributes, refreshable_attributes) = match managed_stability
                .get(&resource.id)
            {
                Some(ManagedBindingStability::Updating {
                    changed_attributes,
                    refreshable_attributes,
                }) => (changed_attributes.clone(), refreshable_attributes.clone()),
                None
                | Some(
                    ManagedBindingStability::NoPlannedEffect | ManagedBindingStability::Unavailable,
                ) => (HashSet::new(), HashSet::new()),
            };
            let mut completeness = BindingCompleteness::Incomplete;
            let mut removed_sensitive_attributes = HashSet::new();
            if let Some(current) = managed_current.get(&resource.id) {
                match current.current() {
                    crate::resource::Current::Absent(_absent) => {}
                    crate::resource::Current::Present(present) => {
                        completeness = BindingCompleteness::Complete;
                        for (k, v) in &present.state().attributes {
                            if changed_attributes.contains(k) && !definitive_attributes.contains(k)
                            {
                                // The update removes this authored key. Do
                                // not resurrect its refreshed prior value.
                                if crate::value::contains_secret(v) {
                                    removed_sensitive_attributes.insert(k.clone());
                                }
                                continue;
                            }
                            merged.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                        match managed_stability.get(&resource.id) {
                            None | Some(ManagedBindingStability::NoPlannedEffect) => {}
                            Some(ManagedBindingStability::Updating { .. }) => {}
                            Some(ManagedBindingStability::Unavailable) => {
                                completeness = BindingCompleteness::Incomplete;
                            }
                        }
                    }
                    crate::resource::Current::CreateInFlight(_pending) => {}
                    crate::resource::Current::MissingIdentifier(unidentified) => {
                        for (k, v) in &unidentified.state().attributes {
                            merged.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
            }
            by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: merged,
                    source: BindingValueSource::Local,
                    completeness,
                    definitive_attributes,
                    changed_attributes,
                    refreshable_attributes,
                    removed_sensitive_attributes,
                },
            );
        }

        for (remote_binding, remote_attrs) in remote_bindings {
            by_name.insert(
                remote_binding.clone(),
                ResolvedBinding {
                    attributes: remote_attrs.clone(),
                    source: BindingValueSource::Upstream,
                    completeness: BindingCompleteness::Complete,
                    definitive_attributes: HashSet::new(),
                    changed_attributes: HashSet::new(),
                    refreshable_attributes: HashSet::new(),
                    removed_sensitive_attributes: HashSet::new(),
                },
            );
        }

        Self { by_name }
    }

    /// Layer wait aliases on the view. Extracted from
    /// `pre_apply` so `pre_apply` can sequence the
    /// layering (managed → data sources → compositions → wait aliases)
    /// without duplicating the alias logic.
    fn layer_wait_aliases(&mut self, wait_aliases: &[WaitAliasSpec]) {
        for spec in wait_aliases {
            let Some(target_entry) = self.by_name.get(spec.target.as_str()) else {
                continue;
            };
            let snapshot = target_entry.attributes.clone();
            let completeness = target_entry.completeness;
            let definitive_attributes = target_entry.definitive_attributes.clone();
            let changed_attributes = target_entry.changed_attributes.clone();
            let refreshable_attributes = target_entry.refreshable_attributes.clone();
            let removed_sensitive_attributes = target_entry.removed_sensitive_attributes.clone();
            self.by_name.insert(
                spec.binding.as_str().to_string(),
                ResolvedBinding {
                    attributes: snapshot,
                    source: BindingValueSource::WaitAlias {
                        target: spec.target.clone(),
                    },
                    completeness,
                    definitive_attributes,
                    changed_attributes,
                    refreshable_attributes,
                    removed_sensitive_attributes,
                },
            );
        }
    }

    /// Typed post-apply layering (#3176): add composition
    /// bindings onto an existing view.
    ///
    /// **Scope:** post-apply increment layering (carina#3248). The
    /// canonical caller is `state_writeback.rs`, which re-resolves
    /// compositions against post-apply state via
    /// `resolve_virtual_refs_post_apply` and then layers them on top
    /// of the pre-apply binding view for export resolution. Pre-apply
    /// call sites must use [`ResolvedBindings::pre_apply`] instead —
    /// that constructor lays compositions in once at the start and makes
    /// "forgot compositions" a compile error.
    ///
    /// **Ordering contract**: must be called *after* the managed-side
    /// bindings have been constructed, so any composition whose
    /// attributes contain a `ResourceRef` to a managed sibling
    /// resolves against the up-to-date managed view. The caller is
    /// responsible for that ordering; the function itself just
    /// appends.
    ///
    /// Unlike `pre_apply`, this entry does **not** merge
    /// `managed_current`: compositions have no provider-side state to
    /// merge. The composition's own attribute map (as authored, after
    /// `resolve_virtual_refs_post_apply` materialised any refs) is
    /// recorded verbatim.
    ///
    /// Same-name collisions favour the composition entry (it is inserted
    /// last) — consistent with the design-doc rule that the
    /// post-apply view is layered *on top of* the pre-apply view.
    ///
    /// Returns `Result<(), String>` to match the design-doc signature;
    /// today the only failure mode (an ordering-precondition check)
    /// is left to the caller, so this currently always returns
    /// `Ok(())`. The Result shape lets future validation (e.g. a
    /// hard error on a same-name collision when the post-apply layer
    /// expects to merge rather than overwrite) be added without
    /// breaking the signature.
    pub fn layer_compositions_post_apply(
        &mut self,
        compositions: &[Composition],
    ) -> Result<(), String> {
        self.layer_compositions(compositions);
        Ok(())
    }

    fn layer_compositions(&mut self, compositions: &[Composition]) {
        for v in compositions.iter() {
            let Some(binding_name) = v.binding.as_ref() else {
                continue;
            };
            let value_attrs: indexmap::IndexMap<String, crate::resource::Value> = v
                .signature
                .attributes
                .iter()
                .map(|(k, attr)| (k.clone(), attr.to_value()))
                .collect();
            let definitive_attributes = value_attrs.keys().cloned().collect();
            self.by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: crate::resource::attrs_to_hashmap(&value_attrs),
                    source: BindingValueSource::Local,
                    completeness: BindingCompleteness::Complete,
                    definitive_attributes,
                    changed_attributes: HashSet::new(),
                    refreshable_attributes: HashSet::new(),
                    removed_sensitive_attributes: HashSet::new(),
                },
            );
        }
    }

    /// Layer data-source bindings onto the view. carina#3181: a
    /// `let x = read aws.iam.user { ... }` binding is addressable by a
    /// `ResourceRef` (`x.user_id`), and an `exports { y = x.user_id }`
    /// must see the data source's resolved attribute map. Data sources
    /// are a distinct typestate from managed resources, so they are
    /// layered in explicitly rather than collapsed into the managed
    /// slice.
    ///
    /// # State merging (carina#3252)
    ///
    /// Each data-source binding carries two layers, in this order:
    ///
    /// 1. `DataSource.attributes` — the parsed DSL attributes (input
    ///    filters like `path_prefix` / `name_regex`). Resource type identity
    ///    lives on `DataSource.id`, outside the provider-facing map.
    /// 2. `data_source_states[ds.id].attributes` — the **read result** the
    ///    provider returned: `arns`, `user_id`, `account_id`, etc.
    ///    Populated by `read_data_source_with_retry` (apply path) or
    ///    `resolve_data_source_refs_for_refresh` (plan path). Without
    ///    this layer the read attributes are missing from the binding
    ///    entirely; a downstream `ResourceRef` like
    ///    `admin_access_roles.arns` resolves to nothing and the
    ///    executor's `assert_fully_resolved` rejects it with the
    ///    misleading "add a `wait` block" message that does not apply
    ///    to data sources.
    ///
    /// Layer 2 wins on key collision — same direction as
    /// `record_applied` (post-apply, provider value is truth), but
    /// gated on `state.exists`; managed state uses its typed `Current`
    /// variant for the equivalent decision (an absent row must not contribute
    /// attributes). `record_applied` itself has no such
    /// gate because its caller only fires it on a successful apply
    /// result. The merge direction deliberately diverges from
    /// [`Self::build_managed_core`] (which is DSL-wins because the
    /// managed-resource DSL attributes *are* the user-authored desired
    /// state): a data source has no desired-state authoring, only a
    /// filter input and a provider-returned result, so the provider's
    /// value is the source of truth. The two layers don't typically
    /// share keys anyway (inputs are filters, outputs are produced
    /// values).
    ///
    /// The shared structural pattern with [`Self::build_managed_core`]
    /// is that *both* merge their provider-read value into the binding —
    /// the carina#3252 gap was a data-source binding that dropped state
    /// entirely. The merge-direction divergence above is intentional.
    ///
    /// **Scope:** the only documented caller is
    /// [`ResolvedBindings::pre_apply`]. The function is `pub(crate)`
    /// so a new call site cannot recreate the carina#3252 gap by bypassing
    /// the total [`DataSourceValues`] collection.
    pub(crate) fn layer_data_source_bindings(&mut self, data_source_values: &DataSourceValues) {
        for (d, value) in data_source_values.iter() {
            let Some(binding_name) = d.binding.as_ref() else {
                continue;
            };
            let mut merged = crate::resource::attrs_to_hashmap(&d.attributes);
            let definitive_attributes = merged.keys().cloned().collect();
            let mut completeness = BindingCompleteness::Incomplete;
            match value {
                DataSourceValue::Read(state) => {
                    // A completed not-found read contributes no attributes,
                    // but its missing outputs are still definitive absence.
                    completeness = BindingCompleteness::Complete;
                    if state.exists {
                        for (k, v) in &state.attributes {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                }
                DataSourceValue::DeferredToApply(_) | DataSourceValue::BlockedByPlanError => {}
            }
            self.by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: merged,
                    source: BindingValueSource::Local,
                    completeness,
                    definitive_attributes,
                    changed_attributes: HashSet::new(),
                    refreshable_attributes: HashSet::new(),
                    removed_sensitive_attributes: HashSet::new(),
                },
            );
        }
    }

    fn layer_unread_data_source_bindings(&mut self, data_sources: &[crate::resource::DataSource]) {
        for data_source in data_sources {
            let Some(binding_name) = data_source.binding.as_ref() else {
                continue;
            };
            let attributes = crate::resource::attrs_to_hashmap(&data_source.attributes);
            let definitive_attributes = attributes.keys().cloned().collect();
            self.by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes,
                    source: BindingValueSource::Local,
                    completeness: BindingCompleteness::Incomplete,
                    definitive_attributes,
                    changed_attributes: HashSet::new(),
                    refreshable_attributes: HashSet::new(),
                    removed_sensitive_attributes: HashSet::new(),
                },
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<&HashMap<String, Value>> {
        self.by_name.get(name).map(|b| &b.attributes)
    }

    pub fn source(&self, name: &str) -> Option<&BindingValueSource> {
        self.by_name.get(name).map(|b| &b.source)
    }

    pub fn completeness(&self, name: &str) -> Option<BindingCompleteness> {
        self.by_name.get(name).map(|b| b.completeness)
    }

    /// Whether a present attribute value can be consumed before apply.
    pub fn attribute_value_is_definitive(&self, name: &str, attribute: &str) -> bool {
        self.by_name.get(name).is_some_and(|binding| {
            binding.definitive_attributes.contains(attribute)
                || (!binding.changed_attributes.contains(attribute)
                    && binding.completeness == BindingCompleteness::Complete)
        })
    }

    /// Whether a missing attribute is definitive absence. Provider-populated
    /// attributes may appear during an update, so their prior absence remains
    /// unknown even though an already-present prior value is usable.
    pub fn attribute_absence_is_definitive(&self, name: &str, attribute: &str) -> bool {
        self.by_name.get(name).is_some_and(|binding| {
            (binding.changed_attributes.contains(attribute)
                && !binding.definitive_attributes.contains(attribute))
                || (self.attribute_value_is_definitive(name, attribute)
                    && !binding.refreshable_attributes.contains(attribute))
        })
    }

    /// Whether a required reference targets a secret-bearing refreshed value
    /// that this plan removes from the effective desired view.
    ///
    /// Resolution still treats the key as absent (so a direct coalesce may
    /// select its fallback), but validation must not lose the sensitivity of
    /// a bare unresolved reference before the removal has run.
    pub fn removed_attribute_was_sensitive(&self, name: &str, attribute: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|binding| binding.removed_sensitive_attributes.contains(attribute))
    }

    pub fn binding_is_fully_definitive(&self, name: &str) -> bool {
        self.by_name.get(name).is_some_and(|binding| {
            binding.completeness == BindingCompleteness::Complete
                && binding.refreshable_attributes.is_empty()
        })
    }

    /// Apply the final plan's managed-resource effects to an existing binding
    /// view. This is the authoritative post-diff refinement used by export
    /// checks and apply scheduling; it also includes cascade-generated updates
    /// that a pre-expansion differ projection cannot know about.
    pub fn apply_plan_stability(
        &mut self,
        managed: &[Resource],
        managed_current: &HashMap<ResourceId, PlanInputCurrent>,
        plan: &crate::plan::Plan,
        schemas: &crate::schema::SchemaRegistry,
    ) {
        let stability = managed_binding_stability_from_plan(managed, plan, schemas);
        for resource in managed {
            let Some(binding_name) = resource.binding.as_deref() else {
                continue;
            };
            let Some(binding) = self.by_name.get_mut(binding_name) else {
                continue;
            };
            match stability.get(&resource.id) {
                None | Some(ManagedBindingStability::NoPlannedEffect) => {
                    binding.changed_attributes.clear();
                    binding.refreshable_attributes.clear();
                    binding.removed_sensitive_attributes.clear();
                    if managed_current.get(&resource.id).is_some_and(|current| {
                        matches!(current.current(), crate::resource::Current::Present(_))
                    }) {
                        binding.completeness = BindingCompleteness::Complete;
                    }
                }
                Some(ManagedBindingStability::Updating {
                    changed_attributes,
                    refreshable_attributes,
                }) => {
                    binding.completeness = BindingCompleteness::Complete;
                    binding.changed_attributes = changed_attributes.clone();
                    binding.refreshable_attributes = refreshable_attributes.clone();
                    let desired = resource.resolved_attributes();
                    binding.definitive_attributes = desired.keys().cloned().collect();
                    binding.removed_sensitive_attributes.clear();
                    for attribute in changed_attributes {
                        if let Some(value) = desired.get(attribute) {
                            binding.attributes.insert(attribute.clone(), value.clone());
                        } else {
                            if binding
                                .attributes
                                .get(attribute)
                                .is_some_and(crate::value::contains_secret)
                            {
                                binding
                                    .removed_sensitive_attributes
                                    .insert(attribute.clone());
                            }
                            binding.attributes.remove(attribute);
                        }
                    }
                }
                Some(ManagedBindingStability::Unavailable) => {
                    binding.completeness = BindingCompleteness::Incomplete;
                    binding.changed_attributes.clear();
                    binding.refreshable_attributes.clear();
                    binding.removed_sensitive_attributes.clear();
                }
            }
        }
    }

    pub fn access_miss_policy_for_value(&self, value: &Value) -> crate::resource::AccessMissPolicy {
        let complete = value.root_bindings().iter().all(|name| {
            self.by_name.get(name).is_some_and(|binding| {
                binding.completeness == BindingCompleteness::Complete
                    && binding.refreshable_attributes.is_empty()
            })
        });
        if complete {
            crate::resource::AccessMissPolicy::Error
        } else {
            crate::resource::AccessMissPolicy::Defer
        }
    }

    /// Record (or refresh) a binding after a `Create` / `Update` effect
    /// returns its post-apply state.
    ///
    /// `resource_attrs` is the resolved DSL attribute map; `state` is
    /// what the provider just reported. **State wins on key collision** —
    /// the provider's freshly-returned values (e.g. an auto-assigned
    /// `id`) are by definition the source of truth for the binding's
    /// downstream consumers. This is the inverse of `pre_apply`'s
    /// "DSL wins" pre-apply rule: pre-apply we trust the DSL, post-apply
    /// we trust the provider.
    ///
    /// Replaces the executor's `update_binding_map` helper (#2300).
    pub fn record_applied(
        &mut self,
        binding: Option<&str>,
        resource_attrs: &SecretPreservingAttributes,
        state: &State,
    ) {
        let Some(name) = binding else {
            return;
        };
        let mut merged = resource_attrs.values().clone();
        for (k, v) in &state.attributes {
            let value = resource_attrs.values().get(k).map_or_else(
                || v.clone(),
                |shape| crate::value::mask_plaintext_using_secret_shape_fail_closed(v, shape),
            );
            merged.insert(k.clone(), value);
        }
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: merged,
                source: BindingValueSource::Local,
                completeness: BindingCompleteness::Complete,
                definitive_attributes: HashSet::new(),
                changed_attributes: HashSet::new(),
                refreshable_attributes: HashSet::new(),
                removed_sensitive_attributes: HashSet::new(),
            },
        );
    }

    /// Capture a successful wait read using the wait target's current binding
    /// as the secret-shape authority. The caller resolves the wait alias to
    /// its target before dispatch, then this lookup happens again at completion
    /// so a just-created or just-updated target supplies the current shape.
    pub(crate) fn secret_preserving_wait_snapshot(
        &self,
        target_binding: &str,
        state: &State,
    ) -> Result<SecretPreservingAttributes, String> {
        let target = self.by_name.get(target_binding).ok_or_else(|| {
            format!("wait target binding `{target_binding}` is unavailable after completion")
        })?;
        Ok(SecretPreservingAttributes::from_provider_values(
            &target.attributes,
            &state.attributes,
        ))
    }

    /// Set a binding directly to the given attribute map and source.
    /// Replaces any existing entry with the same name.
    ///
    /// Used by call sites that already hold a fully-resolved attribute
    /// map and don't need the `record_applied` state-merge path — for
    /// example, post-apply state writeback that hydrates bindings
    /// straight out of `StateFile.resources[].attributes`.
    ///
    /// The name is `set` rather than `insert` to avoid sounding like
    /// `HashMap::insert` (which returns the previous value); this method
    /// is fire-and-forget.
    pub fn set(&mut self, name: &str, attrs: HashMap<String, Value>, source: BindingValueSource) {
        let completeness = match source {
            BindingValueSource::Upstream => BindingCompleteness::Complete,
            BindingValueSource::Local | BindingValueSource::WaitAlias { .. } => {
                BindingCompleteness::Incomplete
            }
        };
        self.set_with_completeness(name, attrs, source, completeness);
    }

    pub fn set_with_completeness(
        &mut self,
        name: &str,
        attrs: HashMap<String, Value>,
        source: BindingValueSource,
        completeness: BindingCompleteness,
    ) {
        let definitive_attributes = attrs.keys().cloned().collect();
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: attrs,
                source,
                completeness,
                definitive_attributes,
                changed_attributes: HashSet::new(),
                refreshable_attributes: HashSet::new(),
                removed_sensitive_attributes: HashSet::new(),
            },
        );
    }

    /// Project this resolved view into the [`IterableBindings`] a
    /// deferred-for expansion consumes.
    ///
    /// Every entry's merged attribute map (local DSL ⊕ refreshed state,
    /// with upstream and wait-alias entries already folded in by
    /// [`Self::pre_apply`]) is exposed under its binding
    /// name. This is the *only* same-config-aware constructor of
    /// `IterableBindings`: a deferred-for iterable resolves against the
    /// exact post-refresh view every non-loop `ResourceRef` resolves
    /// against (carina#3132 — one resolution timing, no upstream-only
    /// carve-out). The projection borrows the already-built merged view; no
    /// per-call clone or second join exists.
    pub fn project_iterable_bindings(&self) -> IterableBindings<'_> {
        IterableBindings {
            bindings: std::borrow::Cow::Borrowed(self),
        }
    }
}

pub fn managed_binding_stability_from_plan(
    managed: &[Resource],
    plan: &crate::plan::Plan,
    schemas: &crate::schema::SchemaRegistry,
) -> HashMap<ResourceId, ManagedBindingStability> {
    use crate::effect::Effect;
    use crate::schema::SchemaKind;

    let managed_ids: HashSet<&ResourceId> = managed.iter().map(|resource| &resource.id).collect();
    let mut result: HashMap<ResourceId, ManagedBindingStability> = managed
        .iter()
        .map(|resource| {
            (
                resource.id.clone(),
                ManagedBindingStability::NoPlannedEffect,
            )
        })
        .collect();
    for effect in plan.effects() {
        match effect {
            Effect::Update {
                to,
                changed_attributes,
                ..
            } if managed_ids.contains(&to.id) => {
                let changed_attributes = changed_attributes.iter().cloned().collect();
                let refreshable_attributes = schemas
                    .get(&to.id.provider, &to.id.resource_type, SchemaKind::Resource)
                    .map(|schema| schema.as_schema().read_only_attributes())
                    .unwrap_or_default()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                result.insert(
                    to.id.clone(),
                    ManagedBindingStability::Updating {
                        changed_attributes,
                        refreshable_attributes,
                    },
                );
            }
            Effect::Create(resource) if managed_ids.contains(&resource.id) => {
                result.insert(resource.id.clone(), ManagedBindingStability::Unavailable);
            }
            Effect::ResumeCreate { resource, .. } if managed_ids.contains(&resource.id) => {
                result.insert(resource.id.clone(), ManagedBindingStability::Unavailable);
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => {}
        }
    }
    result
}

/// The set of every binding a deferred-for iterable may legally
/// reference: same-config `let` resources (post-refresh), `upstream_state`
/// data, and `wait` aliases — all merged.
///
/// Exists as a distinct newtype, rather than a bare
/// `HashMap<String, HashMap<String, Value>>`, so the input to
/// [`File::expand_deferred_for_expressions`] *names* its contract. Before
/// carina#3132 the expander took the raw map and the only caller passed
/// the upstream-only `remote_bindings`; a same-config `let cert`'s
/// refreshed `domain_validation_options` was therefore never in scope and
/// the loop stayed deferred forever. Routing construction through
/// [`ResolvedBindings::project_iterable_bindings`] (the merged view) —
/// or the explicitly-named [`Self::from_upstream_only`] for the
/// upstream-only unit tests — makes "iterable resolved against the wrong
/// map" unrepresentable.
#[derive(Debug, Clone)]
pub struct IterableBindings<'a> {
    bindings: std::borrow::Cow<'a, ResolvedBindings>,
}

impl Default for IterableBindings<'_> {
    fn default() -> Self {
        Self {
            bindings: std::borrow::Cow::Owned(ResolvedBindings::default()),
        }
    }
}

impl<'a> IterableBindings<'a> {
    /// Construct from an upstream-only binding map.
    ///
    /// Named to make the upstream-only nature explicit at the call site:
    /// the runtime plan/apply paths must use
    /// [`ResolvedBindings::project_iterable_bindings`] so same-config
    /// `let` reads are in scope. This constructor exists for the
    /// carina-core parser tests that exercise the `upstream_state`
    /// iterable path with hand-built bindings.
    pub fn from_upstream_only(remote_bindings: HashMap<String, HashMap<String, Value>>) -> Self {
        Self {
            bindings: std::borrow::Cow::Owned(ResolvedBindings {
                by_name: remote_bindings
                    .into_iter()
                    .map(|(name, attributes)| {
                        (
                            name,
                            ResolvedBinding {
                                attributes,
                                source: BindingValueSource::Upstream,
                                completeness: BindingCompleteness::Complete,
                                definitive_attributes: HashSet::new(),
                                changed_attributes: HashSet::new(),
                                refreshable_attributes: HashSet::new(),
                                removed_sensitive_attributes: HashSet::new(),
                            },
                        )
                    })
                    .collect(),
            }),
        }
    }

    /// Resolve a binding's attribute map (the iterable's source).
    pub fn get(&self, binding: &str) -> Option<&HashMap<String, Value>> {
        self.bindings.get(binding)
    }

    /// Provenance of the iterable container. Expansion uses this to retain
    /// missing local desired-side fields while treating loaded upstream data
    /// as final.
    pub fn source(&self, binding: &str) -> Option<&BindingValueSource> {
        self.bindings.source(binding)
    }

    /// Resolve a complete deferred iterable through the same three-state
    /// evaluator used by ordinary resource attributes.
    pub fn resolve_value(&self, value: &Value) -> Result<crate::resolver::ValueResolution, String> {
        crate::resolver::resolve_ref_value_status(value, self.bindings.as_ref())
    }

    /// Missing selections may be treated as final only when every binding
    /// reachable from the iterable is complete.
    pub fn access_miss_policy(&self, value: &Value) -> crate::resource::AccessMissPolicy {
        self.bindings.as_ref().access_miss_policy_for_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{
        ArgumentParameter, BindingName, ResourceTypePath, TypeExpr, UntilPredicateAst,
        UpstreamState, WaitBinding, parse,
    };
    use crate::resource::{CompositionAttribute, Signature};
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, TypeInSchema,
    };

    fn vpc_schema() -> ResourceSchema {
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
    }

    fn registry_with_vpc() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.insert(
            "aws",
            (vpc_schema())
                .validate()
                .expect("schema fixture must be valid"),
        );
        r
    }

    fn composition(binding: &'static str) -> Composition {
        Composition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", binding,
            )),
            signature: Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: indexmap::indexmap! {
                    "target_group_arn".to_string() => CompositionAttribute::from_value(
                        Value::Concrete(crate::resource::ConcreteValue::String(
                            "arn:example".into(),
                        )),
                        None,
                    ),
                },
                pending_constraints: Vec::new(),
            },
            binding: Some(binding.to_string()),
            dependency_bindings: Default::default(),
            module_name: "test_module".to_string(),
            instance: binding.to_string(),
            provenance: Default::default(),
            quoted_string_attrs: Default::default(),
        }
    }

    #[test]
    fn build_indexes_named_let_binding() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        let entry = index.get("vpc").expect("vpc binding present");
        let BindingTarget::Schema(schema) = entry.target else {
            panic!("managed binding must carry a schema target");
        };
        assert_eq!(schema.resource_type, "ec2.Vpc");
        assert!(index.is_declared("vpc"));
        assert_eq!(index.len(), 1);
    }

    /// carina#3181 PR C: `read` (data-source) bindings must appear in the
    /// `BindingIndex` — `from_parsed` walks the typed `data_sources`
    /// slice, not just managed `resources`.
    #[test]
    fn build_indexes_data_source_binding() {
        let src = r#"
let existing = read aws.ec2.Vpc {
    name = "v"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (vpc_schema().as_data_source())
                .validate()
                .expect("schema fixture must be valid"),
        );

        let index = BindingIndex::from_parsed(&parsed, &registry);
        let entry = index
            .get("existing")
            .expect("data-source binding present in index");
        let BindingTarget::Schema(schema) = entry.target else {
            panic!("data-source binding must carry a schema target");
        };
        assert_eq!(schema.resource_type, "ec2.Vpc");
        assert!(index.is_declared("existing"));
    }

    #[test]
    fn build_indexes_composition_but_excludes_it_from_schema_projection() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.compositions.push(composition("instance"));

        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let entry = index
            .get("instance")
            .expect("composition binding must have a typed BindingIndex entry");
        let BindingTarget::Composition(indexed) = entry.target else {
            panic!("composition binding must carry a composition target");
        };
        assert!(std::ptr::eq(indexed, &parsed.compositions[0]));
        assert!(index.is_declared("instance"));
        assert!(
            !index.schemas_by_name().contains_key("instance"),
            "schema-only projection must exclude composition entries",
        );
    }

    #[test]
    fn pre_expansion_module_call_binding_exposes_declared_attribute_types() {
        let parsed = parse(
            "let m = use { source = './m' }\nlet x = m {}\n",
            &Default::default(),
        )
        .expect("module-call fixture must parse");
        let signatures = HashMap::from([(
            "m".to_string(),
            crate::module_resolver::ResolvedModuleSignature {
                arguments: Vec::new(),
                attributes: indexmap::indexmap! {
                    "count".to_string() => Some(TypeExpr::Int),
                    "name".to_string() => Some(TypeExpr::String),
                },
                requires: Vec::new(),
            },
        )]);
        let registry = SchemaRegistry::new();
        let index =
            BindingIndex::from_parsed_with_module_signatures(&parsed, &registry, &signatures);

        let RefType::Typed(count) = index.ref_type(&AccessPath::new("x", "count")) else {
            panic!("module output must be typed before composition expansion");
        };
        assert_eq!(count.display_type_name(), "Int");
        assert!(
            count
                .is_assignable_to_sink(TypeInSchema::schemaless(&AttributeType::int()))
                .is_ok()
        );
        assert!(
            count
                .is_assignable_to_sink(TypeInSchema::schemaless(&AttributeType::string()))
                .is_err()
        );

        assert!(matches!(
            index.ref_type(&AccessPath::new("x", "missing")),
            RefType::UnknownAttribute(RefTypeError::UnknownAttribute { .. })
        ));
        assert!(
            !index.schemas_by_name().contains_key("x"),
            "module signatures are typed surfaces, not provider schemas",
        );
    }

    #[test]
    fn wait_binding_aliases_composition_target() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.compositions.push(composition("instance"));
        parsed.wait_bindings.push(WaitBinding {
            binding: BindingName::from("ready"),
            target: BindingName::from("instance"),
            until_raw: "instance.target_group_arn == 'ready'".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["instance".to_string(), "target_group_arn".to_string()],
                rhs: Value::Concrete(crate::resource::ConcreteValue::String("ready".into())),
            },
            timeout_secs: None,
            depends_on: Vec::new(),
            line: 1,
        });

        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let entry = index
            .get("ready")
            .expect("wait binding must alias a composition BindingTarget");
        let BindingTarget::Composition(indexed) = entry.target else {
            panic!("wait binding must preserve the composition target arm");
        };
        assert!(std::ptr::eq(indexed, &parsed.compositions[0]));
        assert!(!index.schemas_by_name().contains_key("ready"));
    }

    #[test]
    fn build_skips_anonymous_resources() {
        // No `let` binding — anonymous resources never appear in the index
        // because they cannot be referenced by name.
        let src = r#"
aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.is_empty());
    }

    #[test]
    fn build_skips_bindings_with_unknown_schema() {
        // The Vpc binding exists but its schema is not registered. Callers
        // (validation / LSP) raise a separate "unknown resource type"
        // diagnostic, so `get` returns None — but `contains_name` still
        // says yes, so a "unknown binding" diagnostic is not double-fired
        // on top of the "unknown resource type" one.
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = SchemaRegistry::new();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("vpc").is_none());
        assert!(
            index.is_declared("vpc"),
            "binding declared in source must show up in `known_names`",
        );
    }

    #[test]
    fn build_records_wait_binding_name_when_target_schema_is_unknown() {
        let src = r#"
let cert = aws.acm.Certificate {
    name = "cert"
}

let cert_issued = wait cert {
    until = cert.status == "issued"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = SchemaRegistry::new();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("cert").is_none());
        assert!(index.get("cert_issued").is_none());
        assert!(index.is_declared("cert"));
        assert!(
            index.is_declared("cert_issued"),
            "wait binding declared in source must show up in `known_names`",
        );
    }

    #[test]
    fn build_includes_only_named_top_level_bindings_not_for_body_templates() {
        // For-body template resources never carry a `binding`, so they
        // must not appear in the index — the iter walks all resources
        // (top-level and for-body) for parity with `iter_all_resources`,
        // but the binding filter weeds out the unnamed ones.
        let src = r#"
let net = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

for _, n in some_iterable {
    aws.ec2.Vpc {
        name = n
        cidr_block = "10.0.0.0/16"
    }
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("net").is_some(), "named let binding indexed");
        assert!(
            !index.is_declared("n"),
            "for-body iteration variable is not a binding the index should know about",
        );
        // For-body templates carry parser-synthesised internal bindings
        // (used for address derivation) — those names never surfaced in
        // the pre-#2231 validation map and `BindingIndex::from_parsed`
        // preserves that contract by walking top-level only.
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn schemas_by_name_returns_borrowed_schemas() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let by_name = index.schemas_by_name();
        let schema = by_name.get("vpc").expect("vpc projection present");
        assert_eq!(schema.resource_type, "ec2.Vpc");
        // The projection borrows the schema held by the matching target.
        let BindingTarget::Schema(indexed) = index.get("vpc").unwrap().target else {
            panic!("managed binding must carry a schema target");
        };
        assert!(std::ptr::eq(*schema, indexed));
    }

    #[test]
    fn get_returns_none_for_unknown_name() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);
        assert!(index.get("missing").is_none());
    }

    #[test]
    fn ref_type_infers_unannotated_forwarded_composition_attribute() {
        let mut parsed = parse(
            r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#,
            &Default::default(),
        )
        .expect("parse");
        let mut output = composition("instance");
        output.signature.attributes.insert(
            "cidr".to_string(),
            CompositionAttribute::from_value(
                Value::resource_ref("vpc".to_string(), "cidr_block", vec![]),
                None,
            ),
        );
        parsed.compositions.push(output);
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let RefType::Typed(resolved) = index.ref_type(&AccessPath::new("instance", "cidr")) else {
            panic!("forwarded composition attribute must inherit its source type");
        };
        assert_eq!(resolved.display_type_name(), "String");
    }

    #[derive(Clone)]
    struct ChainType {
        name: &'static str,
        attr: AttributeType,
        declaration: Option<TypeExpr>,
    }

    fn composition_chain_type_universe() -> Vec<ChainType> {
        let string = AttributeType::string();
        let bool_ = AttributeType::bool();
        let vpc_id = AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type("aws", "ec2.Vpc", "Id")),
            None,
            None,
            None,
        );
        let security_group_id = AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type(
                "aws",
                "ec2.SecurityGroup",
                "Id",
            )),
            None,
            None,
            None,
        );

        vec![
            ChainType {
                name: "String",
                attr: string.clone(),
                declaration: Some(TypeExpr::String),
            },
            ChainType {
                name: "Bool",
                attr: bool_.clone(),
                declaration: Some(TypeExpr::Bool),
            },
            ChainType {
                name: "String(pattern=alpha)",
                attr: AttributeType::refined_string(None, Some("^[a-z]+$".to_string()), None, None),
                declaration: None,
            },
            ChainType {
                name: "String(pattern=digits)",
                attr: AttributeType::refined_string(None, Some("^[0-9]+$".to_string()), None, None),
                declaration: None,
            },
            ChainType {
                name: "String(length=..1024)",
                attr: AttributeType::refined_string(None, None, Some((None, Some(1024))), None),
                declaration: None,
            },
            ChainType {
                name: "aws.ec2.Vpc.Id",
                attr: vpc_id.clone(),
                declaration: Some(TypeExpr::SchemaType {
                    provider: "aws".to_string(),
                    path: "ec2.Vpc".to_string(),
                    type_name: "Id".to_string(),
                }),
            },
            ChainType {
                name: "aws.ec2.SecurityGroup.Id",
                attr: security_group_id.clone(),
                declaration: Some(TypeExpr::SchemaType {
                    provider: "aws".to_string(),
                    path: "ec2.SecurityGroup".to_string(),
                    type_name: "Id".to_string(),
                }),
            },
            ChainType {
                name: "List<String>",
                attr: AttributeType::list(string.clone()),
                declaration: Some(TypeExpr::List(Box::new(TypeExpr::String))),
            },
            ChainType {
                name: "List<String(pattern=alpha)>",
                attr: AttributeType::list(AttributeType::refined_string(
                    None,
                    Some("^[a-z]+$".to_string()),
                    None,
                    None,
                )),
                declaration: None,
            },
            ChainType {
                name: "Map<String>",
                attr: AttributeType::map(string.clone()),
                declaration: Some(TypeExpr::Map(Box::new(TypeExpr::String))),
            },
            ChainType {
                name: "Union<String | Bool>",
                attr: AttributeType::union(vec![string, bool_]),
                declaration: Some(TypeExpr::Union(vec![TypeExpr::String, TypeExpr::Bool])),
            },
        ]
    }

    /// A composition declaration is a contract, not a cast. For every actual
    /// source A, liftable declared boundary B, and sink C in this finite
    /// universe, the checker may accept A ->(declared B)-> C only when both
    /// A -> C and B -> C hold in the directional relation.
    #[test]
    fn composition_attribute_chain_requires_declared_and_inferred_types() {
        let universe = composition_chain_type_universe();

        for actual in &universe {
            for declared in &universe {
                let Some(declaration) = &declared.declaration else {
                    continue;
                };
                let mut parsed = parse("let source = test.source.Node { }", &Default::default())
                    .expect("source fixture must parse");
                let mut middle = composition("middle");
                middle.signature.attributes.clear();
                middle.signature.attributes.insert(
                    "value".to_string(),
                    CompositionAttribute::from_value(
                        Value::resource_ref("source".to_string(), "value", vec![]),
                        Some(declaration.clone()),
                    ),
                );
                parsed.compositions.push(middle);

                let mut registry = SchemaRegistry::new();
                registry.insert(
                    "test",
                    (ResourceSchema::new("source.Node").attribute(
                        AttributeSchema::optional("value", actual.attr.clone()).build(),
                    ))
                    .validate()
                    .expect("schema fixture must be valid"),
                );
                let index = BindingIndex::from_parsed(&parsed, &registry);
                let RefType::Typed(chain) = index.ref_type(&AccessPath::new("middle", "value"))
                else {
                    panic!(
                        "composition chain must be typed: {} ->(declared {})",
                        actual.name, declared.name,
                    );
                };

                for sink in &universe {
                    let accepted = chain
                        .is_assignable_to_sink(TypeInSchema::schemaless(&sink.attr))
                        .is_ok();
                    let actual_accepts = TypeInSchema::schemaless(&actual.attr)
                        .is_assignable_to(TypeInSchema::schemaless(&sink.attr));
                    let declared_accepts = TypeInSchema::schemaless(&declared.attr)
                        .is_assignable_to(TypeInSchema::schemaless(&sink.attr));
                    assert!(
                        !accepted || (actual_accepts && declared_accepts),
                        "checker accepted {} ->(declared {})→ {}, but direct constraints were actual={} declared={}",
                        actual.name,
                        declared.name,
                        sink.name,
                        actual_accepts,
                        declared_accepts,
                    );
                }
            }
        }
    }

    #[test]
    fn ref_type_resolves_bare_and_struct_field_module_arguments() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.arguments.extend([
            ArgumentParameter {
                name: "name".to_string(),
                type_expr: TypeExpr::String,
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "cfg".to_string(),
                type_expr: TypeExpr::Struct {
                    fields: vec![("enabled".to_string(), TypeExpr::Bool)],
                },
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ]);
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let RefType::Typed(name) = index.ref_type("name") else {
            panic!("bare module argument must resolve to its declared type");
        };
        assert_eq!(name.display_type_name(), "String");

        let RefType::Typed(enabled) = index.ref_type(&AccessPath::new("cfg", "enabled")) else {
            panic!("field access into a struct argument must narrow its declared type");
        };
        assert_eq!(enabled.display_type_name(), "Bool");
    }

    #[test]
    fn ref_type_lifts_resource_handle_argument_and_enforces_boundary_matrix() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.arguments.push(ArgumentParameter {
            name: "vpc".to_string(),
            type_expr: TypeExpr::Ref(ResourceTypePath::new("aws", "ec2.Vpc")),
            default: None,
            description: None,
            validations: Vec::new(),
        });
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let RefType::Typed(vpc) = index.ref_type("vpc") else {
            panic!("Ref-typed module argument must resolve to a resource handle");
        };
        let same = crate::validation::lift_type_expr(&TypeExpr::Ref(ResourceTypePath::new(
            "aws", "ec2.Vpc",
        )))
        .expect("Ref must be liftable");
        let different = crate::validation::lift_type_expr(&TypeExpr::Ref(ResourceTypePath::new(
            "aws",
            "ec2.Subnet",
        )))
        .expect("Ref must be liftable");
        let literal_union = crate::validation::lift_type_expr(&TypeExpr::Union(vec![
            TypeExpr::StringLiteral("dev".to_string()),
            TypeExpr::StringLiteral("prod".to_string()),
        ]))
        .expect("literal union must be liftable");

        for accepted in [AttributeType::string(), same] {
            assert!(
                vpc.is_assignable_to_sink(TypeInSchema::schemaless(&accepted))
                    .is_ok(),
                "resource handles retain main's string-shaped compatibility"
            );
        }
        for rejected in [AttributeType::int(), different, literal_union] {
            assert!(
                vpc.is_assignable_to_sink(TypeInSchema::schemaless(&rejected))
                    .is_err(),
                "resource handles must not flow into non-string, closed-literal, or different-handle sinks"
            );
        }
    }

    #[test]
    fn ref_type_lifts_bare_resource_binding_as_a_handle() {
        let parsed = parse(
            r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#,
            &Default::default(),
        )
        .expect("parse");
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let RefType::Typed(vpc) = index.ref_type("vpc") else {
            panic!("a bare resource binding must resolve to its resource-handle type");
        };
        assert_eq!(vpc.display_type_name(), "aws.ec2.Vpc");
    }

    #[test]
    fn ref_type_resolves_upstream_export_and_reports_missing_export() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.upstream_states.push(UpstreamState {
            binding: "up".to_string(),
            source: "../upstream".into(),
        });
        let exports = HashMap::from([(
            "up".to_string(),
            HashMap::from([(
                "vpc".to_string(),
                crate::upstream_exports::UpstreamExportEntry {
                    type_expr: Some(TypeExpr::SchemaType {
                        provider: "aws".to_string(),
                        path: "ec2.Vpc".to_string(),
                        type_name: "Id".to_string(),
                    }),
                    has_declared_type: true,
                    value: None,
                },
            )]),
        )]);
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed_with_upstream_exports(&parsed, &registry, &exports);

        let RefType::Typed(vpc) = index.ref_type(&AccessPath::new("up", "vpc")) else {
            panic!("annotated upstream export must resolve to its declared type");
        };
        assert_eq!(vpc.display_type_name(), "aws.ec2.Vpc.Id");

        assert!(matches!(
            index.ref_type(&AccessPath::new("up", "missing")),
            RefType::UnknownAttribute(RefTypeError::UnknownUpstreamExport {
                binding,
                export,
                ..
            }) if binding == "up" && export == "missing"
        ));
    }

    #[test]
    fn ref_type_leaves_inferred_but_unannotated_upstream_export_unchecked() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.upstream_states.push(UpstreamState {
            binding: "up".to_string(),
            source: "../upstream".into(),
        });
        let exports = HashMap::from([(
            "up".to_string(),
            HashMap::from([(
                "opaque".to_string(),
                crate::upstream_exports::UpstreamExportEntry {
                    type_expr: Some(TypeExpr::String),
                    has_declared_type: false,
                    value: None,
                },
            )]),
        )]);
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed_with_upstream_exports(&parsed, &registry, &exports);

        assert!(matches!(
            index.ref_type(&AccessPath::new("up", "opaque")),
            RefType::Unchecked
        ));
    }

    #[test]
    fn ref_type_leaves_unloadable_upstream_binding_unchecked() {
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.upstream_states.push(UpstreamState {
            binding: "up".to_string(),
            source: "../missing".into(),
        });
        let exports = HashMap::new();
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed_with_upstream_exports(&parsed, &registry, &exports);

        assert!(matches!(
            index.ref_type(&AccessPath::new("up", "anything")),
            RefType::Unchecked
        ));
    }

    #[test]
    fn ref_type_forwarded_composition_cycle_is_unchecked() {
        let make_forwarder = |binding: &'static str, target: &str| Composition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", binding,
            )),
            signature: Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: indexmap::indexmap! {
                    "out".to_string() => CompositionAttribute::from_value(
                        Value::resource_ref(target.to_string(), "out", vec![]),
                        None,
                    ),
                },
                pending_constraints: Vec::new(),
            },
            binding: Some(binding.to_string()),
            dependency_bindings: Default::default(),
            module_name: "cycle".to_string(),
            instance: binding.to_string(),
            provenance: Default::default(),
            quoted_string_attrs: Default::default(),
        };
        let mut parsed = parse("", &Default::default()).expect("parse");
        parsed.compositions.push(make_forwarder("a", "b"));
        parsed.compositions.push(make_forwarder("b", "a"));
        let registry = SchemaRegistry::new();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        assert!(matches!(
            index.ref_type(&AccessPath::new("a", "out")),
            RefType::Unchecked
        ));
    }
}

#[cfg(test)]
mod resolved_bindings_tests {
    use super::*;
    use crate::data_source_value::{DataSourceValue, DataSourceValues};
    use crate::resource::{
        ConcreteValue, DataSource, DeferredValue, PartialReadMarker, Resource, ResourceId, State,
        UnknownReason, Value,
    };
    use std::collections::BTreeSet;

    fn secret_preserving(attributes: &HashMap<String, Value>) -> SecretPreservingAttributes {
        SecretPreservingAttributes::from_provider_values(attributes, attributes)
    }

    fn test_data_source_values(
        data_sources: &[DataSource],
        states: &HashMap<ResourceId, State>,
    ) -> DataSourceValues {
        DataSourceValues::from_data_sources(data_sources, |data_source| {
            states.get(&data_source.id).map_or_else(
                || DataSourceValue::deferred_to_apply(Vec::new()),
                |state| DataSourceValue::read(state.clone()),
            )
        })
    }

    fn make_resource(
        name: &'static str,
        binding: Option<&str>,
        attrs: Vec<(&str, Value)>,
    ) -> Resource {
        let mut r = Resource::new("test.resource", name);
        r.attributes = attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        r.binding = binding.map(|b| b.to_string());
        r
    }

    #[test]
    fn local_binding_carries_dsl_attributes() {
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )],
        )];
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".into()
            )))
        );
        assert_eq!(resolved.source("vpc"), Some(&BindingValueSource::Local));
    }

    #[test]
    fn local_binding_merges_state_attributes_when_dsl_missing_them() {
        let rid = ResourceId::with_identity("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: true,
                attributes: vec![
                    (
                        "id".to_string(),
                        Value::Concrete(ConcreteValue::String("vpc-abc".into())),
                    ),
                    // conflicting key — DSL value should win
                    (
                        "cidr_block".to_string(),
                        Value::Concrete(ConcreteValue::String("WRONG".into())),
                    ),
                ]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),

                partial_read: None,
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-abc".into()))),
            "state-only attribute must be merged in",
        );
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".into()
            ))),
            "DSL value must win when both sides define a key",
        );
    }

    #[test]
    fn pre_apply_materializes_partial_read_unknown_for_sibling_ref() {
        let id_a = ResourceId::with_identity("test.resource", "a");
        let id_b = ResourceId::with_identity("test.resource", "b");
        let a = make_resource("a", Some("a"), vec![]);
        let b = make_resource(
            "b",
            Some("b"),
            vec![(
                "target",
                Value::resource_ref("a".to_string(), "dns_name".to_string(), vec![]),
            )],
        );
        let mut partial_a =
            State::existing(id_a.clone(), HashMap::new()).with_identifier("aws-id-A");
        partial_a.partial_read = Some(PartialReadMarker {
            detail: "read after create did not return dns_name".to_string(),
            missing_attributes: ["dns_name".to_string()].into_iter().collect(),
        });
        let states: HashMap<ResourceId, State> = [
            (id_a, partial_a),
            (id_b.clone(), State::existing(id_b, HashMap::new())),
        ]
        .into_iter()
        .collect();
        let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
            states,
            &crate::schema::SchemaRegistry::new(),
            &[],
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut resources = vec![a, b];

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &plan_input_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let a_dns_name = resolved
            .get("a")
            .and_then(|attrs| attrs.get("dns_name"))
            .expect("partial-read missing attribute must be materialized");
        match a_dns_name {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::PostCreateReadIncomplete {
                detail,
            })) => assert_eq!(detail, "read after create did not return dns_name"),
            other => panic!("expected PostCreateReadIncomplete unknown, got {other:?}"),
        }

        crate::resolver::resolve_refs_with_state_and_remote(&mut resources, &resolved)
            .expect("sibling ref should resolve through the binding view");
        let target = resources[1]
            .attributes
            .get("target")
            .expect("target attribute should remain present");
        match target {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::PostCreateReadIncomplete {
                detail,
            })) => assert_eq!(detail, "read after create did not return dns_name"),
            other => {
                panic!("expected sibling ref to resolve to partial-read unknown, got {other:?}")
            }
        }
    }

    #[test]
    fn upstream_state_binding_is_first_class() {
        // Upstream-state bindings have no `Resource` and no `ResourceSchema`,
        // which is the case `BindingIndex` cannot represent.
        let resources: Vec<Resource> = Vec::new();
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut network_attrs = HashMap::new();
        network_attrs.insert(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );
        remote.insert("network".to_string(), network_attrs);

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let attrs = resolved.get("network").expect("upstream binding present");
        assert_eq!(
            attrs.get("vpc_id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-123".into())))
        );
        assert_eq!(
            resolved.source("network"),
            Some(&BindingValueSource::Upstream)
        );
    }

    #[test]
    fn unbound_resources_are_excluded() {
        let resources = vec![make_resource(
            "anonymous",
            None,
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        assert!(resolved.get("anonymous").is_none());
    }

    #[test]
    fn upstream_overrides_local_on_name_collision() {
        // The pre-#2299 resolver inserted remote bindings *after* local
        // ones, so upstream wins when both define the same name. This is
        // load-bearing behaviour — preserving it makes #2299 a no-op for
        // configurations that already rely on it.
        let resources = vec![make_resource(
            "my-local",
            Some("shared"),
            vec![(
                "kind",
                Value::Concrete(ConcreteValue::String("local".into())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote.insert(
            "shared".to_string(),
            vec![(
                "kind".to_string(),
                Value::Concrete(ConcreteValue::String("upstream".into())),
            )]
            .into_iter()
            .collect(),
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        let attrs = resolved.get("shared").expect("shared binding present");
        assert_eq!(
            attrs.get("kind"),
            Some(&Value::Concrete(ConcreteValue::String("upstream".into()))),
            "upstream binding must override local one with the same name",
        );
        assert_eq!(
            resolved.source("shared"),
            Some(&BindingValueSource::Upstream),
        );
    }

    #[test]
    fn destroyed_state_does_not_contribute_attributes() {
        // `State.exists = false` represents a tombstone (resource was
        // destroyed since the last apply). The resolver must not merge
        // its attributes — they describe a no-longer-real resource.
        let rid = ResourceId::with_identity("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: false,
                attributes: vec![(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-stale".into())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),

                partial_read: None,
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });
        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert!(
            attrs.get("id").is_none(),
            "tombstoned state must not contribute attributes",
        );
    }

    #[test]
    fn record_applied_inserts_state_winning_merge() {
        // Post-apply semantics: the freshly returned `State.attributes`
        // must override the DSL `resource_attrs` on conflict — the
        // provider just told us the real value (e.g. an auto-assigned
        // `id` differs from the placeholder, or `arn` materialises). The
        // executor relied on this in `update_binding_map`, so the
        // `ResolvedBindings` API has to preserve it byte-for-byte.
        let mut resolved = ResolvedBindings::default();
        let resource_attrs: HashMap<String, Value> = vec![
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-dsl".into())),
            ),
            (
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            ),
        ]
        .into_iter()
        .collect();
        let state = State {
            id: ResourceId::with_identity("test.resource", "my-vpc"),
            identifier: None,
            exists: true,
            attributes: vec![
                // overrides DSL value
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-applied".into())),
                ),
                // adds a state-only key
                (
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-abc".into())),
                ),
            ]
            .into_iter()
            .collect(),
            dependency_bindings: BTreeSet::new(),

            partial_read: None,
        };

        resolved.record_applied(Some("vpc"), &secret_preserving(&resource_attrs), &state);

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("name"),
            Some(&Value::Concrete(ConcreteValue::String(
                "vpc-applied".into()
            ))),
            "state must win over resource_attrs on conflict",
        );
        assert_eq!(
            attrs.get("id"),
            Some(&Value::Concrete(ConcreteValue::String("vpc-abc".into())))
        );
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::Concrete(ConcreteValue::String(
                "10.0.0.0/16".into()
            ))),
        );
        assert_eq!(resolved.source("vpc"), Some(&BindingValueSource::Local));
    }

    #[test]
    fn record_applied_keeps_secret_sensitivity_on_provider_values() {
        let mut resolved = ResolvedBindings::default();
        let plaintext = Value::Concrete(ConcreteValue::String("sensitive".into()));
        let resource_attrs = HashMap::from([(
            "comment".to_string(),
            Value::Deferred(crate::resource::DeferredValue::Secret(Box::new(
                plaintext.clone(),
            ))),
        )]);
        let state = State {
            id: ResourceId::with_identity("test.resource", "secret"),
            identifier: None,
            exists: true,
            attributes: HashMap::from([("comment".to_string(), plaintext.clone())]),
            dependency_bindings: BTreeSet::new(),
            partial_read: None,
        };

        resolved.record_applied(Some("secret"), &secret_preserving(&resource_attrs), &state);

        assert_eq!(
            resolved
                .get("secret")
                .and_then(|attrs| attrs.get("comment")),
            Some(&Value::Deferred(crate::resource::DeferredValue::Secret(
                Box::new(plaintext)
            )))
        );
    }

    #[test]
    fn record_applied_with_no_binding_is_a_noop() {
        let mut resolved = ResolvedBindings::default();
        let attrs: HashMap<String, Value> = HashMap::new();
        let state = State {
            id: ResourceId::with_identity("test.resource", "anon"),
            identifier: None,
            exists: true,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),

            partial_read: None,
        };

        resolved.record_applied(None, &secret_preserving(&attrs), &state);
        assert!(resolved.get("anon").is_none());
    }

    #[test]
    fn cloned_view_does_not_share_storage_with_original() {
        // The phased / replace executor paths snapshot the binding map
        // for cascade frames: `local_binding_map = binding_snapshot.clone()`.
        // After #2300 the same pattern relies on `ResolvedBindings: Clone`,
        // and the clone must be independent so frame-local mutations
        // don't leak back into the parent snapshot.
        let resources = vec![make_resource(
            "vpc",
            Some("vpc"),
            vec![(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let parent = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &resources,
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map_from_states(
                states.clone(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[],
        });

        let mut child = parent.clone();
        let extra_state = State {
            id: ResourceId::with_identity("test.resource", "subnet"),
            identifier: None,
            exists: true,
            attributes: vec![(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("subnet-1".into())),
            )]
            .into_iter()
            .collect(),
            dependency_bindings: BTreeSet::new(),

            partial_read: None,
        };
        child.record_applied(
            Some("subnet"),
            &secret_preserving(&HashMap::new()),
            &extra_state,
        );

        assert!(child.get("subnet").is_some());
        assert!(
            parent.get("subnet").is_none(),
            "child mutation must not leak into the parent snapshot",
        );
    }

    #[test]
    fn set_replaces_entry_and_records_source() {
        // `set` is the no-merge counterpart to `record_applied` — used
        // when the caller already holds a fully-resolved attribute map
        // (e.g. post-apply state-writeback hydrating exports). Verify
        // it overwrites any existing entry and records the given source.
        let mut resolved = ResolvedBindings::default();
        let initial: HashMap<String, Value> = vec![(
            "kind".to_string(),
            Value::Concrete(ConcreteValue::String("first".into())),
        )]
        .into_iter()
        .collect();
        resolved.set("registry", initial, BindingValueSource::Upstream);
        assert_eq!(
            resolved.source("registry"),
            Some(&BindingValueSource::Upstream)
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::Concrete(ConcreteValue::String("first".into())))
        );

        let replacement: HashMap<String, Value> = vec![(
            "kind".to_string(),
            Value::Concrete(ConcreteValue::String("second".into())),
        )]
        .into_iter()
        .collect();
        resolved.set("registry", replacement, BindingValueSource::Local);
        assert_eq!(
            resolved.source("registry"),
            Some(&BindingValueSource::Local),
            "set must replace the source as well as the attributes",
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::Concrete(ConcreteValue::String("second".into())))
        );
    }

    // ---- carina#3085: wait-binding passthrough alias ----

    fn wait_spec(binding: &str, target: &str) -> WaitAliasSpec {
        WaitAliasSpec {
            binding: BindingName::new(binding),
            target: BindingName::new(target),
        }
    }

    /// Test plan item 1: a wait binding resolves to its target's
    /// attribute map, sourced as `WaitAlias { target }`.
    #[test]
    fn wait_alias_resolves_to_target_attributes() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:1:certificate/abc".into(),
                )),
            )],
        );
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        assert_eq!(
            resolved
                .get("cert_issued")
                .and_then(|a| a.get("certificate_arn")),
            Some(&Value::Concrete(ConcreteValue::String(
                "arn:aws:acm:us-east-1:1:certificate/abc".into()
            ))),
            "cert_issued.certificate_arn must passthrough to cert's value"
        );
        assert_eq!(
            resolved.source("cert_issued"),
            Some(&BindingValueSource::WaitAlias {
                target: BindingName::new("cert")
            }),
            "source must record the wait→target edge, not flatten to Local"
        );
    }

    /// Test plan item 1 (negative): a wait whose target has no entry
    /// (typo / scoped-out) creates no alias — the ref stays unresolved
    /// and the existing PlanError path (not a panic) surfaces it.
    #[test]
    fn wait_alias_absent_target_creates_no_entry() {
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "nonexistent")],
        });
        assert!(
            resolved.get("cert_issued").is_none(),
            "no alias when target is absent (ref stays unresolved → existing PlanError)"
        );
    }

    /// The target may be an upstream binding, not just a resource —
    /// the alias must still mirror it (aliases materialise after both
    /// local and upstream entries exist).
    #[test]
    fn wait_alias_target_can_be_upstream() {
        let mut remote = HashMap::new();
        let mut attrs = HashMap::new();
        attrs.insert(
            "id".to_string(),
            Value::Concrete(ConcreteValue::String("up-1".into())),
        );
        remote.insert("up".to_string(), attrs);
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote,
            wait_aliases: &[wait_spec("waited", "up")],
        });
        assert_eq!(
            resolved.get("waited").and_then(|a| a.get("id")),
            Some(&Value::Concrete(ConcreteValue::String("up-1".into())))
        );
    }

    /// Test plan item 5: the alias is an independent snapshot — a
    /// later `set("cert", …)` write-back must not mutate the alias,
    /// and vice versa (design: read-time snapshot).
    #[test]
    fn wait_alias_is_independent_snapshot() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String("arn:old".into())),
            )],
        );
        let mut resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        let mut new_attrs = HashMap::new();
        new_attrs.insert(
            "certificate_arn".to_string(),
            Value::Concrete(ConcreteValue::String("arn:new".into())),
        );
        resolved.set("cert", new_attrs, BindingValueSource::Local);
        assert_eq!(
            resolved
                .get("cert_issued")
                .and_then(|a| a.get("certificate_arn")),
            Some(&Value::Concrete(ConcreteValue::String("arn:old".into()))),
            "wait alias is a snapshot: a later set('cert', …) must not mutate it"
        );
    }

    /// Design Risks (multiple downstream consumers): the single wait
    /// alias entry resolves consistently for every consumer that reads
    /// it — there is one `cert_issued` entry, so N downstream resources
    /// referencing `cert_issued.*` all see the same value. (The single
    /// `Effect::Wait` fan-out to all consumers is the dependency-graph
    /// machinery's job, unchanged by this value-layer alias and
    /// covered by the wait_downstream_apply E2E.)
    #[test]
    fn wait_alias_resolves_consistently_for_repeated_lookups() {
        let cert = make_resource(
            "cert",
            Some("cert"),
            vec![(
                "certificate_arn",
                Value::Concrete(ConcreteValue::String("arn:shared".into())),
            )],
        );
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[cert],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[wait_spec("cert_issued", "cert")],
        });
        // Two independent consumers both resolve the same value.
        let a = resolved
            .get("cert_issued")
            .and_then(|m| m.get("certificate_arn"))
            .cloned();
        let b = resolved
            .get("cert_issued")
            .and_then(|m| m.get("certificate_arn"))
            .cloned();
        assert_eq!(a, b);
        assert_eq!(
            a,
            Some(Value::Concrete(ConcreteValue::String("arn:shared".into())))
        );
    }

    /// carina#3252: a `read aws.X` data-source binding must expose the
    /// attributes that `read_data_source_with_retry` wrote into
    /// `data_source_states[ds.id]`, not just the DSL-side input filters.
    ///
    /// Pre-fix: `layer_data_sources_post_apply` recorded only
    /// `DataSource.attributes` (the `path_prefix` / `name_regex` filters),
    /// so a downstream managed resource referencing
    /// `admin_access_roles.arns` saw nothing in the binding map and the
    /// executor's `assert_fully_resolved` rejected the unresolved
    /// `ResourceRef` with the misleading "add a `wait` block" message.
    ///
    /// The fix mirrors `build_managed_core`: pre-apply layering for a
    /// data-source binding must merge the read-result `State.attributes`
    /// (the actual output: `arns`, `user_id`, ...) on top of the DSL
    /// input map. Read-result wins on key collision — the input map is
    /// just the filter set the provider was *given*; the state map is
    /// what the provider *returned*.
    #[test]
    fn data_source_binding_merges_state_attributes() {
        let ds_id = ResourceId::with_identity("aws.iam.Roles", "admin_access_roles");
        let mut ds = DataSource::new("aws.iam.Roles", "admin_access_roles");
        ds.id = ds_id.clone();
        ds.binding = Some("admin_access_roles".to_string());
        ds.attributes.insert(
            "path_prefix".to_string(),
            Value::Concrete(ConcreteValue::String(
                "/aws-reserved/sso.amazonaws.com/".into(),
            )),
        );
        ds.attributes.insert(
            "name_regex".to_string(),
            Value::Concrete(ConcreteValue::String(
                "^AWSReservedSSO_AdministratorAccess_[0-9a-f]{16}$"
                    .to_string()
                    .into(),
            )),
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State {
                id: ds_id,
                identifier: None,
                exists: true,
                attributes: vec![(
                    "arns".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::String(
                            "arn:aws:iam::111111111111:role/aws-reserved/sso.amazonaws.com/AWSReservedSSO_AdministratorAccess_abcdef0123456789".into(),
                        )),
                    ])),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),

                partial_read: None,

                },
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &test_data_source_values(&[ds], &states),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });

        let attrs = resolved
            .get("admin_access_roles")
            .expect("data-source binding present");
        // The DSL-input filter survives — useful for plan-display refs
        // that name the input field.
        assert!(
            attrs.get("path_prefix").is_some(),
            "DSL input filter must remain visible in the binding",
        );
        // The read result must be visible so downstream resource refs
        // (e.g. `role.assume_role_policy_document = admin_access_roles.arns`)
        // resolve to a concrete `List<String>` at apply time.
        let arns = attrs
            .get("arns")
            .expect("read-state attribute `arns` must be visible on the binding");
        match arns {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items.len(), 1, "one role arn returned");
            }
            other => panic!("expected concrete List, got {:?}", other),
        }
        assert_eq!(
            resolved.source("admin_access_roles"),
            Some(&BindingValueSource::Local)
        );
    }

    #[test]
    fn completed_not_found_data_source_makes_missing_output_absent() {
        let id = ResourceId::with_identity("test.Lookup", "lookup");
        let mut data_source = DataSource::new("test.Lookup", "lookup");
        data_source.id = id.clone();
        data_source.binding = Some("lookup".to_string());
        let states = HashMap::from([(id.clone(), State::not_found(id))]);

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &test_data_source_values(&[data_source], &states),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        assert_eq!(
            resolved.completeness("lookup"),
            Some(BindingCompleteness::Complete),
        );

        let fallback = Value::Concrete(ConcreteValue::String("fallback".into()));
        let expression = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("lookup", "result", Vec::new())),
            default: Box::new(fallback.clone()),
        });
        assert_eq!(
            crate::resolver::resolve_ref_value_status(&expression, &resolved),
            Ok(crate::resolver::ValueResolution::Present(fallback)),
        );
    }

    #[test]
    fn identifierless_data_source_remains_valid_in_separate_map() {
        let id = ResourceId::with_identity("aws.sts.CallerIdentity", "caller");
        let mut data_source = DataSource::new("aws.sts.CallerIdentity", "caller");
        data_source.id = id.clone();
        data_source.binding = Some("caller".to_string());
        let data_source_states = HashMap::from([(
            id.clone(),
            State::existing(
                id,
                HashMap::from([(
                    "account_id".to_string(),
                    Value::Concrete(ConcreteValue::String("111111111111".into())),
                )]),
            ),
        )]);

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &crate::resource::into_plan_input_current_map(
                HashMap::new(),
                &crate::schema::SchemaRegistry::new(),
                &[],
            ),
            data_source_values: &test_data_source_values(&[data_source], &data_source_states),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });

        assert_eq!(
            resolved
                .get("caller")
                .and_then(|attributes| attributes.get("account_id")),
            Some(&Value::Concrete(ConcreteValue::String(
                "111111111111".into()
            )))
        );
    }

    /// carina#3252 follow-up: when the data-source DSL input map and the
    /// read state share a key, the read state must win — it is the value
    /// the provider returned. Mirrors `record_applied`'s "state wins"
    /// rule for managed resources. The two maps don't normally collide
    /// (inputs are filters, outputs are produced values), but a provider
    /// is allowed to echo an input back into its state and the binding
    /// view must surface the post-read value, not the pre-read filter.
    #[test]
    fn data_source_binding_state_wins_on_key_collision() {
        let ds_id = ResourceId::with_identity("aws.example.Echo", "echo");
        let mut ds = DataSource::new("aws.example.Echo", "echo");
        ds.id = ds_id.clone();
        ds.binding = Some("echo".to_string());
        ds.attributes.insert(
            "filter".to_string(),
            Value::Concrete(ConcreteValue::String("dsl-input".into())),
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State {
                id: ds_id,
                identifier: None,
                exists: true,
                attributes: vec![(
                    "filter".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-result".into())),
                )]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),

                partial_read: None,
            },
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &test_data_source_values(&[ds], &states),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        let attrs = resolved.get("echo").expect("echo binding present");
        assert_eq!(
            attrs.get("filter"),
            Some(&Value::Concrete(ConcreteValue::String(
                "provider-result".into()
            ))),
            "read-state attribute must override DSL input on collision",
        );
    }

    /// carina#3252: cover the no-DSL-input shape (e.g.
    /// `read aws.sts.CallerIdentity {}` — failure path (b)/(c) in the
    /// issue comment). The DSL attribute map is empty; the binding's
    /// entire visible content comes from `data_source_states[ds.id]`. A
    /// regression that re-broke state merging would still
    /// pass [`data_source_binding_merges_state_attributes`] because that
    /// test also asserts the DSL filter survives — this test does not,
    /// so it pins the state-only variant.
    #[test]
    fn data_source_binding_with_no_dsl_inputs_exposes_state_only() {
        let ds_id = ResourceId::with_provider_identity("aws", "sts.CallerIdentity", "caller", None);
        let mut ds = DataSource::with_provider("aws", "sts.CallerIdentity", "caller", None);
        ds.id = ds_id.clone();
        ds.binding = Some("caller".to_string());
        assert!(
            ds.attributes.is_empty(),
            "this test covers the no-DSL-input shape",
        );

        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            ds_id.clone(),
            State::existing(
                ds_id,
                vec![(
                    "account_id".to_string(),
                    Value::Concrete(ConcreteValue::String("111111111111".into())),
                )]
                .into_iter()
                .collect(),
            ),
        );

        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &[],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &test_data_source_values(&[ds], &states),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        let attrs = resolved.get("caller").expect("caller binding present");
        assert_eq!(
            attrs.get("account_id"),
            Some(&Value::Concrete(ConcreteValue::String(
                "111111111111".into()
            ))),
            "with no DSL inputs, the binding's content is entirely the \
             read state's attributes",
        );
    }
}

#[cfg(test)]
mod binding_name_set_tests {
    use super::*;
    use crate::parser::{ParsedFile, parse};

    fn parsed_with(src: &str) -> ParsedFile {
        parse(src, &Default::default()).expect("parse")
    }

    #[test]
    fn records_resource_let_bindings_as_resource_kind() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("vpc"));
        assert_eq!(names.kind("vpc"), Some(&BindingNameKind::Resource));
    }

    #[test]
    fn records_upstream_state_with_upstream_kind() {
        let src = r#"
let network = upstream_state {
    source = "../network"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("network"));
        assert_eq!(names.kind("network"), Some(&BindingNameKind::UpstreamState));
    }

    #[test]
    fn records_top_level_let_value_with_variable_kind() {
        // `let <name> = <Value>` (non-resource RHS) is captured under
        // `parsed.variables` and surfaces as `Variable` in the name set.
        let src = r#"
let region = "ap-northeast-1"
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("region"));
        assert_eq!(names.kind("region"), Some(&BindingNameKind::Variable));
    }

    #[test]
    fn records_arguments_block_entries_with_argument_kind() {
        let src = r#"
arguments {
    env: String
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("env"));
        assert_eq!(names.kind("env"), Some(&BindingNameKind::Argument));
    }

    #[test]
    fn records_fn_def_with_user_function_kind() {
        let src = r#"
fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("double"));
        assert_eq!(names.kind("double"), Some(&BindingNameKind::UserFunction));
    }

    #[test]
    fn records_module_call_with_module_call_kind() {
        let src = r#"
let cluster = module {
    source = "../cluster"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(
            names.contains("cluster"),
            "module-call binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("cluster"), Some(&BindingNameKind::ModuleCall));
    }

    #[test]
    fn matches_collect_known_bindings_merged_byte_for_byte() {
        // BindingNameSet is the canonical replacement for
        // `collect_known_bindings_merged`. The set of names must be
        // identical so the replacement is a pure refactor.
        let src = r#"
arguments {
    env: String
}

let region = "ap-northeast-1"

let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

let network = upstream_state {
    source = "../network"
}

fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);
        let legacy = crate::parser::collect_known_bindings_merged(&parsed);

        let from_set: std::collections::HashSet<&str> = names.iter_names().collect();
        let legacy_set: std::collections::HashSet<&str> = legacy.into_iter().collect();
        assert_eq!(from_set, legacy_set);
    }

    #[test]
    fn structural_binding_is_in_scope_but_not_addressable_by_resource_ref() {
        // The pre-#2301 invariant: when a `let` binds the result of an
        // `if` / `for` / `read` expression, the parser flags the
        // binding `is_structural` and records the name in
        // `parsed.structural_bindings` (so unused-binding warnings
        // don't fire). Such names are visible to scope checks but not
        // surfaced through `ResolvedBindings` — the executor /
        // resolver address bindings by their attribute map, and a
        // structural binding's "value" is the entire if/for branch,
        // not an attribute set.
        let src = r#"
let chosen = if true { "primary" } else { "fallback" }
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        // Scope-side: `chosen` is in scope as a Structural kind.
        assert!(
            names.contains("chosen"),
            "structural binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("chosen"), Some(&BindingNameKind::Structural));

        // Value-side: `ResolvedBindings` does NOT carry an entry for
        // `chosen`, so a `ResourceRef` to `chosen.foo` cannot resolve.
        let resolved = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &parsed.resources,
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        });
        assert!(
            resolved.get("chosen").is_none(),
            "structural bindings must stay invisible to ResourceRef resolution",
        );
    }

    /// carina#3085 Test plan item 2: a `wait` binding registers as
    /// `BindingNameKind::Wait { target }` and — unlike `Structural` —
    /// **is** addressable (`contains` is true), because
    /// `<wait-binding>.<attr>` is a passthrough to its target.
    #[test]
    fn wait_binding_registers_as_wait_kind_and_is_addressable() {
        let src = r#"
let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}

let cert_issued = wait cert {
    until = cert.status == aws.acm.Certificate.Status.Issued
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(
            names.contains("cert_issued"),
            "a wait binding is addressable (passthrough), unlike Structural; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(
            names.kind("cert_issued"),
            Some(&BindingNameKind::Wait {
                target: BindingName::new("cert")
            }),
            "wait binding must carry its typed target edge"
        );
        // The target itself is still a plain resource binding.
        assert_eq!(names.kind("cert"), Some(&BindingNameKind::Resource));
    }
}
