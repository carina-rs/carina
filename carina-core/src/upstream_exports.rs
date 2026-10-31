//! Static validation of `upstream_state` field references.
//!
//! An `upstream_state { source = "..." }` binding exposes exactly the keys
//! declared by the upstream project's `exports { }` block. Those keys are
//! fixed at declaration time and visible by parsing the upstream's `.crn`
//! files — no state I/O.
//!
//! Two public entry points:
//! - [`resolve_upstream_exports`] parses each upstream's source directory
//!   and returns the declared key set per binding.
//! - [`check_upstream_state_field_references`] walks a parsed project and
//!   returns an error for every reference whose field isn't in the set.
//!
//! Both are pure functions so `validate`, LSP diagnostics, and any other
//! surface can share the same logic without duplicating traversal code.

use std::collections::HashMap;
use std::path::Path;

use crate::config_loader::{find_crn_files_in_dir, parse_directory};
use crate::parser::{
    LoadedProviderContext, ProviderContext, ResourceContext, ResourceRef, TypeExpr, UpstreamState,
};
use crate::resource::{BindingAddress, DeferredValue, Subscript, Value};
use crate::schema::suggest_similar_name;
use crate::static_type::ConsumerExportStaticType;
#[cfg(test)]
use crate::static_type::StaticType;

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamExportType {
    Static(ConsumerExportStaticType),
    InferenceNotRun,
}

impl UpstreamExportType {
    pub fn declared_known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Static(static_type) => static_type.declared_known(),
            Self::InferenceNotRun => None,
        }
    }
}

/// One export's static-type provenance and literal value, as carried through
/// [`UpstreamExports`]. `value` is currently always `Some` (the parser requires
/// `name = expr`) but is kept `Option` for forward compatibility with
/// future grammar widenings.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamExportEntry {
    pub static_type: UpstreamExportType,
    pub value: Option<Value>,
}

/// Exports for a single `upstream_state` binding: export name → entry.
pub type UpstreamExportEntries = HashMap<String, UpstreamExportEntry>;

/// Exports declared by each `upstream_state` binding: binding name →
/// per-export entry map. Carrying the literal `Value` alongside the
/// static-type provenance lets downstream consumers (LSP map-literal-key
/// completion, future doc tools) read the body without re-parsing
/// the upstream directory.
pub type UpstreamExports = HashMap<String, UpstreamExportEntries>;

/// A diagnostic about a `binding.field` reference whose downstream usage
/// doesn't fit the upstream's exports.
///
/// Four error types share this shape — name-not-exported
/// ([`UpstreamFieldError`]), `for`-iterable shape mismatch
/// ([`UpstreamForIterableShapeError`]), attribute-access shape mismatch
/// ([`UpstreamAttributeAccessShapeError`]), and subscript shape mismatch
/// ([`UpstreamSubscriptShapeError`]). They share their
/// CLI and LSP wirings through this trait so adding another check is
/// one `impl`, not three identical extends/loops.
///
/// Excludes [`UpstreamResolveError`] on purpose: a resolve failure
/// doesn't have a `(binding, field)` pair (the upstream source itself
/// failed to parse) so the LSP anchoring path can't render it the same
/// way.
///
/// `Display` is required so `to_string()` produces the canonical
/// `"location: message"` form for the CLI's combined-error path —
/// keeping the format in one place (the per-type `Display` impl)
/// instead of letting the CLI reimplement it inline.
pub trait UpstreamRefDiagnostic: std::fmt::Display {
    /// Where in the downstream project the bad reference appears
    /// (e.g. `"aws.s3.Bucket.main attribute `name`"`).
    fn location(&self) -> &str;
    /// Root binding name (e.g. `"orgs"`).
    fn binding(&self) -> &str;
    /// Top-level field on the upstream's exports (e.g. `"accounts"`).
    fn field(&self) -> &str;
    /// User-facing diagnostic body, location-free. The LSP diagnostic
    /// range carries the *where*; the message says *what*.
    fn diagnostic_message(&self) -> String;
}

/// An `upstream_state` binding whose source directory exists but couldn't
/// be parsed. Downstream field-reference checks against this binding are
/// skipped (we don't know what it exports), but the failure itself must be
/// surfaced — otherwise a broken upstream silently masks downstream typos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResolveError {
    pub binding: String,
    pub source: std::path::PathBuf,
    pub reason: String,
}

impl std::fmt::Display for UpstreamResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "upstream_state `{}`: failed to parse source `{}`: {}",
            self.binding,
            self.source.display(),
            self.reason
        )
    }
}

impl std::error::Error for UpstreamResolveError {}

/// A reference in the downstream project whose field isn't declared by the
/// upstream's `exports { }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamFieldError {
    /// Where in the downstream project the bad reference appears
    /// (e.g. `"aws.s3.Bucket.main attribute `name`"`).
    pub location: String,
    pub binding: String,
    pub field: String,
    pub suggestion: Option<String>,
}

impl UpstreamFieldError {
    /// Location-free phrasing shared by CLI and LSP — the LSP diagnostic
    /// range already tells the user *where*, so it only needs *what*.
    pub fn diagnostic_message(&self) -> String {
        let suggestion = self
            .suggestion
            .as_ref()
            .map(|s| format!(" Did you mean `{}`?", s))
            .unwrap_or_default();
        format!(
            "upstream_state `{}` does not export `{}`.{}",
            self.binding, self.field, suggestion
        )
    }
}

impl std::fmt::Display for UpstreamFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamFieldError {}

impl UpstreamRefDiagnostic for UpstreamFieldError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamFieldError::diagnostic_message(self)
    }
}

/// Statically resolve each `upstream_state` binding's declared exports.
///
/// Returns `(exports, resolve_errors)` — a **partial** result so one broken
/// upstream doesn't suppress checks against the others.
///
/// - Present in `exports` (possibly empty set): source exists and parses.
///   An empty set means "exports nothing", so every downstream reference
///   against it is invalid.
/// - Omitted from `exports` and listed in `resolve_errors`: source exists
///   but fails to parse — callers should surface this error separately.
/// - Absent from both: source directory is missing. The dedicated
///   `check_upstream_state_sources` diagnostic already handles that case.
///
/// An empty directory (no `.crn` files) is treated as "exports nothing"
/// rather than a parse failure; the user may be mid-setup.
pub fn resolve_upstream_exports<B: crate::resource::BindingAddress>(
    base_dir: &Path,
    upstream_states: &[UpstreamState<B>],
    config: &LoadedProviderContext,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    resolve_upstream_exports_with_schemas(base_dir, upstream_states, config, None)
}

/// Bootstrap/discovery variant of [`resolve_upstream_exports`]. Module value
/// checks are limited to provider-independent types.
pub fn resolve_upstream_exports_providerless<B: crate::resource::BindingAddress>(
    base_dir: &Path,
    upstream_states: &[UpstreamState<B>],
    config: &ProviderContext,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    resolve_upstream_exports_providerless_with_schemas(base_dir, upstream_states, config, None)
}

/// As [`resolve_upstream_exports`], but consult `schemas` to infer
/// missing type annotations from each export's rhs (#2361).
/// When `schemas` is `None` the behavior is identical to the legacy
/// entry point — the export's declared type is forwarded as-is. Production
/// callers (CLI validate, LSP diagnostics, LSP completion) thread the
/// real schema registry so unannotated exports become typed before
/// downstream type-checks see them.
pub fn resolve_upstream_exports_with_schemas<B: crate::resource::BindingAddress>(
    base_dir: &Path,
    upstream_states: &[UpstreamState<B>],
    config: &LoadedProviderContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    resolve_upstream_exports_with_context(
        base_dir,
        upstream_states,
        crate::validation::typecheck::TypecheckProviderContext::loaded(config),
        schemas,
    )
}

/// Bootstrap/discovery variant of
/// [`resolve_upstream_exports_with_schemas`].
pub fn resolve_upstream_exports_providerless_with_schemas<B: crate::resource::BindingAddress>(
    base_dir: &Path,
    upstream_states: &[UpstreamState<B>],
    config: &ProviderContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    resolve_upstream_exports_with_context(
        base_dir,
        upstream_states,
        crate::validation::typecheck::TypecheckProviderContext::providerless(config),
        schemas,
    )
}

fn resolve_upstream_exports_with_context<B: crate::resource::BindingAddress>(
    base_dir: &Path,
    upstream_states: &[UpstreamState<B>],
    provider_context: crate::validation::typecheck::TypecheckProviderContext<'_>,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    let config = provider_context.as_provider_context();
    let mut out: UpstreamExports = HashMap::new();
    let mut errors: Vec<UpstreamResolveError> = Vec::new();
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source);
        if !source_abs.is_dir() {
            continue;
        }
        if matches!(find_crn_files_in_dir(&source_abs), Ok(files) if files.is_empty()) {
            out.insert(us.binding.to_string(), HashMap::new());
            continue;
        }
        match parse_directory(&source_abs, config) {
            Ok(parsed) => {
                let mut parsed = parsed.into_parsed();
                // Normalize every consumer-visible declaration before module
                // expansion and export inference. Inference compares the
                // structural TypeExpr values it receives, so leaving one
                // declaration as DottedUnresolved while an equivalent schema
                // projection is already SchemaType creates a false mismatch.
                crate::validation::resolve_file_type_exprs_best_effort(&mut parsed, config);
                // Expand module calls before inference so that
                // `attributes { ... }`-typed `Virtual` resources land in
                // `parsed.resources` (#2493). Without this the inferer
                // sees module-call bindings as known-but-non-inferable
                // and any `exports` value referencing
                // `<module_call_binding>.<attr>` (e.g.
                // `oidc_provider_arn = bs.oidc_provider_arn` at
                // `infra/registry/dev/bootstrap/exports.crn`) remains
                // unchecked even when the type is fully derivable.
                // `resolve_modules_with_config` is the same expansion
                // pass `load_configuration` runs, so the inference reach
                // here matches what the CLI sees post-expansion.
                let empty_schemas = crate::schema::SchemaRegistry::new();
                let expansion_schemas = schemas.unwrap_or(&empty_schemas);
                let module_resolution = match provider_context.loaded_context() {
                    Some(loaded) => crate::module_resolver::resolve_upstream_modules_with_config(
                        parsed,
                        &source_abs,
                        loaded,
                        expansion_schemas,
                    ),
                    None => {
                        crate::module_resolver::resolve_upstream_modules_with_providerless_config(
                            parsed,
                            &source_abs,
                            config,
                            expansion_schemas,
                        )
                    }
                };
                let parsed = match module_resolution {
                    Ok(parsed) => parsed,
                    Err(reason) => {
                        errors.push(UpstreamResolveError {
                            binding: us.binding.to_string(),
                            source: us.source.clone(),
                            reason: reason.to_string(),
                        });
                        continue;
                    }
                };
                // Declarations were resolved before expansion so inference
                // compares canonical identities. Resolve once more afterward
                // to classify newly inferred names that the consumer cannot
                // name, preserving that distinction from inference failure.
                let entries: UpstreamExportEntries = match schemas {
                    Some(s) => {
                        let inferred = crate::validation::inference::apply_inference(parsed, s);
                        inferred
                            .export_params
                            .into_iter()
                            .map(|e| {
                                (
                                    e.name,
                                    UpstreamExportEntry {
                                        static_type: UpstreamExportType::Static(
                                            crate::validation::resolve_export_static_type_for_consumer(
                                                e.static_type,
                                                config,
                                            ),
                                        ),
                                        value: e.value,
                                    },
                                )
                            })
                            .collect()
                    }
                    // `schemas` is None (legacy entry point) — forward
                    // the declared type unchanged. Note that without
                    // schemas there is no way to run inference, so
                    // unannotated exports are marked `InferenceNotRun`.
                    None => parsed
                        .export_params
                        .into_iter()
                        .map(|e| {
                            let static_type = match e.static_type {
                                Some(static_type) => {
                                    UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                                        crate::validation::resolve_consumer_static_type(
                                            &static_type,
                                            config,
                                        ),
                                    ))
                                }
                                None => UpstreamExportType::InferenceNotRun,
                            };
                            (
                                e.name,
                                UpstreamExportEntry {
                                    static_type,
                                    value: e.value,
                                },
                            )
                        })
                        .collect(),
                };
                out.insert(us.binding.to_string(), entries);
            }
            Err(reason) => {
                errors.push(UpstreamResolveError {
                    binding: us.binding.to_string(),
                    source: us.source.clone(),
                    reason,
                });
            }
        }
    }
    (out, errors)
}

/// Format the location string for an attribute on a resource, with the
/// `for-body` prefix when the resource is a deferred-for template.
/// Three checks emit the same string; centralized here so a future
/// tweak to the wording lands in one place.
fn resource_attr_location<S, B, D>(rref: ResourceRef<'_, S, B, D>, attr_name: &str) -> String
where
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<B>,
{
    match rref.context() {
        ResourceContext::Direct => format!("{} attribute `{}`", rref.id(), attr_name),
        ResourceContext::Deferred(d) => {
            format!(
                "for-body `{}` {} attribute `{}`",
                D::deferred_header(d),
                rref.id(),
                attr_name
            )
        }
    }
}

/// Walk every project value covered by the upstream export checks, with its
/// user-facing diagnostic location. Parser-side deferred resource bodies are
/// matched explicitly; iterable expressions are handled separately because
/// they have their own shape semantics.
fn for_each_project_value<E, D, F>(parsed: &crate::parser::File<E, E::Binding, D>, mut f: F)
where
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
    F: FnMut(&Value<E::Binding>, &str),
{
    for rref in parsed.iter_all_resources() {
        for (attr_name, value) in rref.attributes().iter() {
            f(value, &resource_attr_location(rref, attr_name));
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value() {
            f(value, &format!("exports.{}", export.name()));
        }
    }
    for call in &parsed.module_calls {
        let caller = call
            .binding_name
            .as_ref()
            .map(BindingAddress::as_str)
            .unwrap_or(&call.module_name);
        for (arg_name, value) in call.arguments() {
            f(
                value.walker_value(),
                &format!("module `{}` argument `{}`", caller, arg_name),
            );
        }
    }
}

/// Walk a parsed project and return an error for every reference whose root
/// binding is in `exports` but whose field isn't in its declared key set.
/// Also covers deferred for-iterables (e.g. `for _ in orgs.accounts`), which
/// parse into `deferred_for_expressions` rather than `Value::Deferred(DeferredValue::ResourceRef)`.
///
/// Bindings absent from `exports` are skipped — the caller decides what to
/// do about unresolved upstreams.
pub fn check_upstream_state_field_references<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamFieldError> {
    check_upstream_state_field_references_inner(parsed, exports, true)
}

/// Check only scopes that do not have a typed consumer and therefore cannot
/// route existence through [`crate::binding_index::BindingIndex::ref_type`].
/// Production validation uses this alongside the unified typed-reference
/// passes; the broader legacy entry point above remains useful to callers that
/// run this check in isolation.
pub fn check_upstream_state_untyped_field_references<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamFieldError> {
    check_upstream_state_field_references_inner(parsed, exports, false)
}

fn check_upstream_state_field_references_inner<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
    include_typed_consumers: bool,
) -> Vec<UpstreamFieldError> {
    let mut errors: Vec<UpstreamFieldError> = Vec::new();

    // One &str slice per binding so a project with many bad refs doesn't
    // re-materialize the same Vec for every error.
    let known_by_binding: HashMap<&str, Vec<&str>> = exports
        .iter()
        .map(|(b, keys)| (b.as_str(), keys.keys().map(String::as_str).collect()))
        .collect();

    // Ref-checking closure lives in its own scope so the `&mut errors`
    // borrow it holds is released before we push deferred-iterable
    // errors directly below.
    {
        let mut check = |value: &Value<E::Binding>, location: &str| {
            value.visit_resource_refs_with_usage(&mut |path, usage| {
                if usage == crate::resource::ResourceRefUsage::CoalesceValue {
                    return;
                }
                let binding = path.binding();
                let field = path.attribute();
                let Some(keys) = exports.get(binding) else {
                    return;
                };
                if keys.contains_key(field) {
                    return;
                }
                let known = known_by_binding
                    .get(binding)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                errors.push(UpstreamFieldError {
                    location: location.to_string(),
                    binding: binding.to_string(),
                    field: field.to_string(),
                    suggestion: suggest_similar_name(field, known),
                });
            });
        };

        if include_typed_consumers {
            for_each_project_value(parsed, |value, location| check(value, location));
            for attr in &parsed.attribute_params {
                if let Some(value) = &attr.value {
                    check(value, &format!("attributes.{}", attr.name));
                }
            }
        }

        // `let` values have no sink-driven validation pass of their own.
        for (name, value) in &parsed.variables {
            check(value, &format!("let {name}"));
        }
    }

    // Deferred iterables are not part of the materialized resource tree yet;
    // validate their complete expression separately.
    for deferred in parsed.deferred_for_sites() {
        let location = format!("for-expression `{}`", deferred.header);
        deferred
            .iterable
            .visit_resource_refs_with_usage(&mut |path, usage| {
                if usage == crate::resource::ResourceRefUsage::CoalesceValue {
                    return;
                }
                let binding = path.binding();
                let field = path.attribute();
                let Some(keys) = exports.get(binding) else {
                    return;
                };
                if keys.contains_key(field) {
                    return;
                }
                let known = known_by_binding
                    .get(binding)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                errors.push(UpstreamFieldError {
                    location: location.clone(),
                    binding: binding.to_string(),
                    field: field.to_string(),
                    suggestion: suggest_similar_name(field, known),
                });
            });
    }

    // HashMap-backed walks (parsed.variables, resource.attributes,
    // call.arguments) visit entries in nondeterministic order. Sort the
    // final error list so CLI output and any snapshot-style assertions
    // stay stable across runs.
    errors.sort_by(|a, b| {
        (a.location.as_str(), a.binding.as_str(), a.field.as_str()).cmp(&(
            b.location.as_str(),
            b.binding.as_str(),
            b.field.as_str(),
        ))
    });
    errors
}

use crate::validation::walk_type_expr_fields;

/// A `for` expression iterates an `upstream_state` field whose declared
/// export type doesn't match the binding pattern's expected shape:
/// `for x in ...` requires a list, `for k, v in ...` requires a map.
///
/// This is the shape side of #1894 — surfacing pending upstream
/// `list ↔ map` migrations in the downstream's plan/validate output
/// instead of letting them blow up at apply time.
#[derive(Debug, Clone)]
pub struct UpstreamForIterableShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Declared export type — what the upstream's `exports.crn` says.
    pub export_type: TypeExpr,
    /// What kind of binding the downstream `for` introduces.
    pub binding_kind: ForIterableBindingKind,
}

/// Coarse classification of a `ForBinding` for cross-directory shape
/// compatibility. `Simple`/`Indexed` both require a list; `Map` requires
/// a map. The full `ForBinding` is intentionally not stored here — the
/// diagnostic only cares about the shape, not the variable names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForIterableBindingKind {
    /// `for x in ...` or `for (i, x) in ...` — both consume a list.
    List,
    /// `for k, v in ...` — consumes a map.
    Map,
}

impl ForIterableBindingKind {
    fn from_for_binding(binding: &crate::parser::ForBinding) -> Self {
        use crate::parser::ForBinding;
        match binding {
            ForBinding::Simple(_) | ForBinding::Indexed(_, _) => ForIterableBindingKind::List,
            ForBinding::Map(_, _) => ForIterableBindingKind::Map,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ForIterableBindingKind::List => "list",
            ForIterableBindingKind::Map => "map",
        }
    }
}

impl UpstreamForIterableShapeError {
    pub fn diagnostic_message(&self) -> String {
        // Suggestion text depends on what the upstream actually exports,
        // not just on what the binding expected: a `for x in scalar` has
        // no valid binding form to suggest at all.
        let suggested = match &self.export_type {
            TypeExpr::List(_) => match self.binding_kind {
                ForIterableBindingKind::List => "",
                ForIterableBindingKind::Map => "; use `for x in ...` to iterate the list",
            },
            TypeExpr::Map(_) => match self.binding_kind {
                ForIterableBindingKind::List => "; use `for k, v in ...` to iterate the map",
                ForIterableBindingKind::Map => "",
            },
            // Scalar exports can't be iterated at all; saying nothing is
            // honest. The upstream contract has to change first.
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => "",
        };
        format!(
            "upstream_state `{}.{}` is declared as `{}` but `{}` requires a {} iterable{}",
            self.binding,
            self.field,
            self.export_type,
            self.location,
            self.binding_kind.as_str(),
            suggested,
        )
    }
}

impl std::fmt::Display for UpstreamForIterableShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamForIterableShapeError {}

impl UpstreamRefDiagnostic for UpstreamForIterableShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamForIterableShapeError::diagnostic_message(self)
    }
}

/// Walk every `for` expression that iterates an `upstream_state` field
/// reference and emit an error when the export's declared type
/// (`list(...)` vs `map(...)`) doesn't match the binding pattern's
/// expected shape.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`accounts` without `: T`) — there's
///   no upstream type to compare against, and the field-name path
///   handles existence.
///
/// This is the type-level sibling of the runtime check in
/// `parser::ParsedFile::expand_deferred_for_expressions` (`parser/ast.rs`).
/// Both flag `(ForBinding × shape)` mismatches; this one fires at
/// validate time off the upstream's *declared* type, the parser-side
/// one fires at expansion time off the *resolved* `Value`.
pub fn check_upstream_state_for_iterable_shapes<E, D>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamForIterableShapeError>
where
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
{
    fn visit_possible_iterables<B: BindingAddress>(
        value: &Value<B>,
        f: &mut impl FnMut(&crate::resource::AccessPath<B>),
    ) {
        match value {
            Value::Deferred(DeferredValue::ResourceRef { path }) => f(path),
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                visit_possible_iterables(value, f);
                visit_possible_iterables(default, f);
            }
            Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
                visit_possible_iterables(expression.lhs(), f);
                visit_possible_iterables(expression.rhs(), f);
            }
            Value::Deferred(DeferredValue::Secret(inner)) => visit_possible_iterables(inner, f),
            // References inside a function argument constrain that argument,
            // not the function result used as the iterable.
            Value::Concrete(_)
            | Value::Deferred(
                DeferredValue::BindingRef { .. }
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Access(_)
                | DeferredValue::RedactedSecret { .. }
                | DeferredValue::Unknown(_),
            ) => {}
        }
    }

    let mut errors: Vec<UpstreamForIterableShapeError> = Vec::new();
    for deferred in parsed.deferred_for_sites() {
        let binding_kind = ForIterableBindingKind::from_for_binding(deferred.binding);
        visit_possible_iterables(deferred.iterable, &mut |path| {
            let Some(fields) = exports.get(path.binding()) else {
                return;
            };
            let Some(entry) = fields.get(path.attribute()) else {
                return;
            };
            let Some(export_type) = entry.static_type.declared_known() else {
                return;
            };
            let export_kind = match export_type {
                TypeExpr::List(_) => Some(ForIterableBindingKind::List),
                TypeExpr::Map(_) => Some(ForIterableBindingKind::Map),
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_) => None,
            };
            // Scalars feeding a `for` are a different kind of bug; the
            // shape-check fires on them too because no kind matches.
            if export_kind == Some(binding_kind) {
                return;
            }
            errors.push(UpstreamForIterableShapeError {
                location: format!("for-expression `{}`", deferred.header),
                binding: path.binding().to_string(),
                field: path.attribute().to_string(),
                export_type: export_type.clone(),
                binding_kind,
            });
        });
    }
    errors.sort_by(|a, b| {
        (a.location.as_str(), a.binding.as_str(), a.field.as_str()).cmp(&(
            b.location.as_str(),
            b.binding.as_str(),
            b.field.as_str(),
        ))
    });
    errors
}

/// A reference walks past the top-level upstream export with a `.field`
/// chain that doesn't match the export's declared `TypeExpr` — the
/// downstream wrote `orgs.account.foo` but `account`'s declared
/// `TypeExpr` is a `list(...)` / `map(...)` / scalar (no fields), or
/// a `Struct{...}` whose fields don't include `foo`.
///
/// Sibling of [`UpstreamForIterableShapeError`] from #2317 — same family
/// of "downstream's usage doesn't fit the upstream's declared shape",
/// different consumer (attribute access vs `for` iterable). The two
/// share the same detection-layer style: walk the parsed project,
/// look up the declared `TypeExpr` per `(binding, field)`, and emit a
/// structured diagnostic when the usage doesn't fit.
#[derive(Debug, Clone)]
pub struct UpstreamAttributeAccessShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Field-path segments after `binding.field`, exactly as the
    /// downstream wrote them. Kept in full for code actions that want
    /// to render the original access path.
    pub field_path: Vec<String>,
    /// What the upstream declared at the deepest segment that *did*
    /// resolve. For `orgs.account.network.bad_field` that's the
    /// `Struct{...}` for `network`; for `orgs.accounts.foo` against a
    /// `list(...)` it's the `list(...)` itself.
    pub mismatched_at: TypeExpr,
    /// The first segment in `field_path` that didn't fit
    /// `mismatched_at`. Stored directly rather than as an index because
    /// no consumer currently needs the position back into `field_path`.
    pub bad_segment: String,
}

impl UpstreamAttributeAccessShapeError {
    pub fn diagnostic_message(&self) -> String {
        match &self.mismatched_at {
            TypeExpr::Struct { fields } => {
                let known: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
                format!(
                    "upstream_state `{}.{}` has no field `{}`; declared fields are: {}",
                    self.binding,
                    self.field,
                    self.bad_segment,
                    known.join(", "),
                )
            }
            TypeExpr::List(_) => format!(
                "upstream_state `{}.{}` is declared as `{}` but `.{}` reads it as a struct; iterate the list with `for x in {}.{}` to access elements",
                self.binding,
                self.field,
                self.mismatched_at,
                self.bad_segment,
                self.binding,
                self.field,
            ),
            TypeExpr::Map(_) => format!(
                "upstream_state `{}.{}` is declared as `{}` but `.{}` reads it as a struct; iterate the map with `for k, v in {}.{}` to access entries",
                self.binding,
                self.field,
                self.mismatched_at,
                self.bad_segment,
                self.binding,
                self.field,
            ),
            other @ (TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_)) => format!(
                "upstream_state `{}.{}` is declared as `{}` (a scalar), but `.{}` reads it as a struct; scalars have no fields",
                self.binding, self.field, other, self.bad_segment,
            ),
        }
    }
}

impl std::fmt::Display for UpstreamAttributeAccessShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamAttributeAccessShapeError {}

impl UpstreamRefDiagnostic for UpstreamAttributeAccessShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamAttributeAccessShapeError::diagnostic_message(self)
    }
}

/// Walk every `Value::Deferred(DeferredValue::ResourceRef)` whose root is an `upstream_state`
/// binding and whose `field_path` is non-empty, and emit an error
/// whenever a path segment doesn't fit the declared upstream
/// `TypeExpr`.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`account` without `: T`) — there's
///   no upstream type to compare the path against;
/// - the `field_path` is empty — that's a top-level field access,
///   handled by the typed consumer through `BindingIndex::ref_type`.
///
/// Sibling of [`check_upstream_state_for_iterable_shapes`] from #2317;
/// both surface "downstream usage doesn't fit upstream's declared
/// shape" before apply.
pub fn check_upstream_state_attribute_access_shapes<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamAttributeAccessShapeError> {
    check_upstream_state_attribute_access_shapes_inner(parsed, exports, true)
}

/// Check only attribute-access shapes that
/// [`crate::binding_index::BindingIndex::ref_type`] cannot represent.
///
/// The unified resolver reports missing fields on declared structs itself.
/// List, map, and scalar values reached through field syntax resolve to
/// `Unchecked`, so this fallback retains their more specific upstream shape
/// diagnostics without reporting a struct-field typo twice.
pub fn check_upstream_state_attribute_access_shape_fallbacks<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamAttributeAccessShapeError> {
    check_upstream_state_attribute_access_shapes_inner(parsed, exports, false)
}

fn check_upstream_state_attribute_access_shapes_inner<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
    include_resolver_struct_errors: bool,
) -> Vec<UpstreamAttributeAccessShapeError> {
    let mut errors: Vec<UpstreamAttributeAccessShapeError> = Vec::new();
    for_each_project_value(parsed, |value, location| {
        visit_attribute_access(
            value,
            exports,
            location,
            include_resolver_struct_errors,
            &mut errors,
        );
    });
    for deferred in parsed.deferred_for_sites() {
        visit_attribute_access(
            deferred.iterable,
            exports,
            &format!("for-expression `{}`", deferred.header),
            include_resolver_struct_errors,
            &mut errors,
        );
    }
    errors.sort_by(|a, b| {
        (a.location.as_str(), a.binding.as_str(), a.field.as_str()).cmp(&(
            b.location.as_str(),
            b.binding.as_str(),
            b.field.as_str(),
        ))
    });
    errors
}

/// Walk a Value tree, find every `ResourceRef` with a non-empty
/// `field_path`, and check the path against the upstream's declared
/// `TypeExpr`.
fn visit_attribute_access<B: BindingAddress>(
    value: &Value<B>,
    exports: &UpstreamExports,
    location: &str,
    include_resolver_struct_errors: bool,
    errors: &mut Vec<UpstreamAttributeAccessShapeError>,
) {
    value.visit_resource_refs(&mut |path| {
        let leading: Vec<String> = path.leading_field_path();
        if leading.is_empty() {
            return;
        }
        let binding = path.binding();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(entry) = fields.get(attribute) else {
            return;
        };
        let Some(export_type) = entry.static_type.declared_known() else {
            return;
        };
        let walk_result = walk_type_expr_fields(export_type, &leading);
        if let Err((mismatched_at, bad_segment)) = walk_result {
            if !include_resolver_struct_errors && matches!(mismatched_at, TypeExpr::Struct { .. }) {
                return;
            }
            let mismatched_at = mismatched_at.clone();
            let bad_segment = bad_segment.to_string();
            errors.push(UpstreamAttributeAccessShapeError {
                location: location.to_string(),
                binding: binding.to_string(),
                field: attribute.to_string(),
                field_path: leading,
                mismatched_at,
                bad_segment,
            });
        }
    });
}

/// A `[index]` subscript reads an `upstream_state` export at a depth
/// where the declared `TypeExpr` doesn't permit that subscript shape:
/// integer subscript against `map(_)` / `Struct{...}` / scalar, or
/// string subscript against `list(_)` / `Struct{...}` / scalar.
#[derive(Debug, Clone)]
pub struct UpstreamSubscriptShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Field-chain segments between `binding.field` and the offending
    /// subscript. Empty when the subscript sits directly on the
    /// top-level export (`orgs.accounts[0]`).
    pub field_path: Vec<String>,
    /// What the upstream declared at the position the bad subscript
    /// reads. For `orgs.accounts[0]` against `accounts: map(_)` this
    /// is the `map(_)`; for `orgs.region[0]` it's the scalar.
    pub mismatched_at: TypeExpr,
    /// The literal subscript the user wrote — preserved so the
    /// diagnostic echoes their own syntax (`[0]` / `["alpha"]`)
    /// rather than a placeholder.
    pub bad_subscript: Subscript,
}

impl UpstreamSubscriptShapeError {
    pub fn diagnostic_message(&self) -> String {
        let mut access = format!("{}.{}", self.binding, self.field);
        for seg in &self.field_path {
            access.push('.');
            access.push_str(seg);
        }
        let mut bad = String::new();
        self.bad_subscript.append_to_dot_string(&mut bad);
        let suggestion = match &self.mismatched_at {
            TypeExpr::List(_) => match &self.bad_subscript {
                Subscript::Int { .. } => "",
                Subscript::Str { .. } => "; integer subscript `[i]` reads list elements",
            },
            TypeExpr::Map(_) => match &self.bad_subscript {
                Subscript::Int { .. } => "; string subscript `[\"k\"]` reads map entries",
                Subscript::Str { .. } => "",
            },
            // Struct/scalar don't host any subscript form, so the
            // "use the other syntax" hint would be misleading. Stay
            // silent and let the caller show the declared type.
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => "",
        };
        format!(
            "upstream_state `{}` is declared as `{}`; `{}{}` does not fit{}",
            access, self.mismatched_at, access, bad, suggestion,
        )
    }
}

impl std::fmt::Display for UpstreamSubscriptShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamSubscriptShapeError {}

impl UpstreamRefDiagnostic for UpstreamSubscriptShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamSubscriptShapeError::diagnostic_message(self)
    }
}

/// Walk every `Value::Deferred(DeferredValue::ResourceRef)` whose root is an `upstream_state`
/// binding and whose `subscripts` chain is non-empty, and emit an error
/// whenever a subscript's kind (integer vs string) doesn't fit the
/// declared upstream `TypeExpr` at that depth.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`accounts` without `: T`) — there's
///   no upstream type to compare the subscript against;
/// - the leading `field_path` already mismatches the declared shape
///   (already handled by `check_upstream_state_attribute_access_shapes`)
///   — in that case the field-walk diagnostic is enough.
pub fn check_upstream_state_subscript_shapes<
    E: crate::parser::ExportParamLike,
    D: crate::parser::DeferredForPhaseTraversal<E::Binding>,
>(
    parsed: &crate::parser::File<E, E::Binding, D>,
    exports: &UpstreamExports,
) -> Vec<UpstreamSubscriptShapeError> {
    let mut errors: Vec<UpstreamSubscriptShapeError> = Vec::new();
    for_each_project_value(parsed, |value, location| {
        visit_subscript_access(value, exports, location, &mut errors);
    });
    for deferred in parsed.deferred_for_sites() {
        visit_subscript_access(
            deferred.iterable,
            exports,
            &format!("for-expression `{}`", deferred.header),
            &mut errors,
        );
    }
    errors.sort_by(|a, b| {
        (a.location.as_str(), a.binding.as_str(), a.field.as_str()).cmp(&(
            b.location.as_str(),
            b.binding.as_str(),
            b.field.as_str(),
        ))
    });
    errors
}

/// Walk a Value tree, find every `ResourceRef` with a non-empty
/// `subscripts` chain, and check each subscript's kind against the
/// upstream's declared `TypeExpr` at that depth.
fn visit_subscript_access<B: BindingAddress>(
    value: &Value<B>,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamSubscriptShapeError>,
) {
    value.visit_resource_refs(&mut |path| {
        // Chained access (`cert.foo[0].bar`) is not yet exercised by
        // this checker — see #3025 for the planned segments-driven
        // walker. Skip the path here; the resolver still surfaces
        // typos at apply time and the simple-shape paths remain
        // covered.
        if !path.is_simple_shape() {
            return;
        }
        let subscripts = path.trailing_subscripts();
        if subscripts.is_empty() {
            return;
        }
        let binding = path.binding();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(entry) = fields.get(attribute) else {
            return;
        };
        let Some(export_type) = entry.static_type.declared_known() else {
            return;
        };
        // If the field path itself doesn't resolve, the
        // attribute-access check will already report it. Skip here so
        // we don't double-fire.
        let field_path: Vec<String> = path.leading_field_path();
        let Ok(at_field_end) = walk_type_expr_fields(export_type, &field_path) else {
            return;
        };
        // Step through each subscript, descending into the element
        // type when it fits and emitting at the first kind mismatch.
        let mut current = at_field_end;
        for sub in &subscripts {
            let next = match current {
                TypeExpr::List(inner) => match sub {
                    Subscript::Int { .. } => Some(inner.as_ref()),
                    Subscript::Str { .. } => None,
                },
                TypeExpr::Map(inner) => match sub {
                    Subscript::Int { .. } => None,
                    Subscript::Str { .. } => Some(inner.as_ref()),
                },
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_) => None,
            };
            let Some(next) = next else {
                errors.push(UpstreamSubscriptShapeError {
                    location: location.to_string(),
                    binding: binding.to_string(),
                    field: attribute.to_string(),
                    field_path: field_path.clone(),
                    mismatched_at: current.clone(),
                    bad_subscript: sub.clone(),
                });
                return;
            };
            current = next;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedFile;
    use crate::resource::DeferredValue;
    use crate::schema::SchemaRegistry;
    use crate::static_type::ConsumerExportStaticType as ExportStaticType;
    use std::fs;
    use std::path::PathBuf;

    fn write_crn(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn ctx() -> LoadedProviderContext {
        LoadedProviderContext::empty_for_test()
    }

    fn consumer_type(type_expr: TypeExpr) -> crate::static_type::ConsumerStaticType {
        crate::validation::resolve_consumer_static_type_for_signature(&type_expr, &ctx())
    }

    fn consumer_inferred_type(
        type_expr: TypeExpr,
    ) -> crate::static_type::ConsumerInferredStaticType {
        match consumer_type(type_expr) {
            StaticType::Known(resolved) => StaticType::Known(resolved),
            StaticType::Unchecked(reason) => StaticType::Unchecked(
                crate::static_type::ConsumerInferredUncheckedReason::UnnameableInConsumer(reason),
            ),
        }
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: binding.into(),
            source: PathBuf::from(source),
        }
    }

    fn parse_project(source: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.crn"), source).unwrap();
        parse_directory(tmp.path(), &ctx())
            .expect("parse_directory")
            .into_parsed()
    }

    fn inference_not_run_entry() -> UpstreamExportEntry {
        UpstreamExportEntry {
            static_type: UpstreamExportType::InferenceNotRun,
            value: None,
        }
    }

    fn declared_entry(type_expr: TypeExpr) -> UpstreamExportEntry {
        UpstreamExportEntry {
            static_type: UpstreamExportType::Static(ExportStaticType::Declared(consumer_type(
                type_expr,
            ))),
            value: None,
        }
    }

    #[test]
    fn resolve_upstream_exports_preserves_attached_inference_failure() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("upstream");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("main.crn"), "exports {\n  x = [1, 'a']\n}\n").unwrap();

        let (exports, errors) = resolve_upstream_exports_with_schemas(
            root.path(),
            &[upstream("up", "upstream")],
            &ctx(),
            Some(&SchemaRegistry::new()),
        );

        assert!(errors.is_empty(), "unexpected resolve errors: {errors:?}");
        let entry = &exports["up"]["x"];
        assert!(matches!(
            &entry.static_type,
            UpstreamExportType::Static(ExportStaticType::Inferred(StaticType::Unchecked(
                crate::static_type::ConsumerInferredUncheckedReason::InferenceFailed(
                    crate::static_type::InferenceError::HeterogeneousCollection { .. }
                )
            )))
        ));
    }

    fn mk_exports(pairs: &[(&str, &[&str])]) -> UpstreamExports {
        pairs
            .iter()
            .map(|(binding, keys)| {
                (
                    binding.to_string(),
                    keys.iter()
                        .map(|s| (s.to_string(), inference_not_run_entry()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn resolve_uses_base_dir_regardless_of_declaring_file() {
        // Issue #1997: the `source` path is resolved against the project's
        // base directory (the directory passed to validate/plan/apply), not
        // against the specific .crn file that happens to declare the
        // `upstream_state`. Two declarations in sibling files in the same
        // project must therefore resolve identically.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        // Two sibling .crn files living at different depths is not possible
        // within one base_dir (directory-scoped parse is flat), but the rule
        // is that file position inside the base dir is irrelevant: the same
        // relative source string resolves the same way for every declaration.
        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        assert!(got.get("orgs").unwrap().contains_key("accounts"));

        // Same call a second time with an identical UpstreamState produces
        // the same result — guards against any accidental dependence on
        // declaring-file state.
        let (got2, errs2) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs2.is_empty());
        assert_eq!(got, got2);
    }

    #[test]
    fn resolve_reads_exports_from_default_exports_file() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: String = "x"
                region: String = "ap-northeast-1"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());

        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(keys.contains_key("accounts"));
        assert!(keys.contains_key("region"));
    }

    #[test]
    fn resolve_reads_exports_from_any_crn_file() {
        // exports.crn is convention; the block can live in any .crn file.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(keys.contains_key("accounts"));
    }

    #[test]
    fn resolve_merges_exports_across_multiple_crn_files_in_upstream() {
        // Issue #1997: when the upstream project is multi-file, the resolver
        // must merge every .crn file's exports — not just read one privileged
        // file. A downstream that references a field declared in a sibling
        // of the upstream's exports file must still validate.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        // Two sibling files each contributing their own exports block.
        write_crn(
            &upstream_dir,
            "accounts.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        write_crn(
            &upstream_dir,
            "region.crn",
            r#"exports {
                region: String = "ap-northeast-1"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(
            keys.contains_key("accounts"),
            "export from accounts.crn must be merged, got {:?}",
            keys
        );
        assert!(
            keys.contains_key("region"),
            "export from region.crn must be merged, got {:?}",
            keys
        );
    }

    #[test]
    fn resolve_reads_struct_typed_export_from_multi_file_directory() {
        // Multi-file upstream: `exports.crn` carries the struct-typed export
        // while a sibling `backend.crn` is parsed in the same directory.
        // The resolver must surface the struct annotation on the export.
        use crate::parser::TypeExpr;

        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: struct {
                    registry_prod: String,
                    registry_dev: String,
                } = {
                    registry_prod = "111111111111"
                    registry_dev  = "222222222222"
                }
            }"#,
        );
        write_crn(
            &upstream_dir,
            "backend.crn",
            r#"backend local { path = "carina.state.json" }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        let entry = keys.get("accounts").expect("accounts export");
        let ty = entry
            .static_type
            .declared_known()
            .expect("type annotation present");
        assert!(
            matches!(ty, TypeExpr::Struct { fields } if fields.len() == 2),
            "expected struct with 2 fields, got {ty:?}"
        );
    }

    #[test]
    fn resolve_carries_map_literal_value_alongside_type() {
        // #2492: each resolved export entry must carry both its declared
        // `TypeExpr` and the literal `Value` from the upstream's
        // `exports { }` block. Without the value, downstream consumers
        // (LSP map-literal-key completion, future doc-extracting tools)
        // have to re-parse the upstream directory a second time per call.
        use crate::resource::{ConcreteValue, Value};

        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: map(String) = {
                    registry_prod = "111111111111"
                    registry_dev  = "222222222222"
                }
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let entries = got.get("orgs").expect("resolved");
        let entry = entries.get("accounts").expect("accounts export");
        assert!(
            entry.static_type.declared_known().is_some(),
            "type annotation must still be present"
        );
        let value = entry.value.as_ref().expect("export value must be carried");
        let Value::Concrete(ConcreteValue::Map(entries)) = value else {
            panic!(
                "expected Value::Concrete(ConcreteValue::Map) for map literal export, got {value:?}"
            );
        };
        let mut keys: Vec<&String> = entries.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["registry_dev", "registry_prod"],
            "map literal keys must be reachable from UpstreamExports without re-parsing"
        );
    }

    #[test]
    fn resolve_skips_missing_source_directory() {
        // Missing-directory is `check_upstream_state_sources`' job; this
        // resolver stays quiet about it.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../missing")], &ctx());
        assert!(got.is_empty());
        assert!(errs.is_empty());
    }

    #[test]
    fn resolve_returns_empty_set_when_source_has_no_exports_block() {
        // An upstream without `exports { }` exports nothing — valid static
        // answer. Downstream refs against it will then fail the checker.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"backend local { path = "carina.state.json" }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("binding should be resolved");
        assert!(keys.is_empty());
    }

    #[test]
    fn resolve_treats_empty_directory_as_empty_exports() {
        // Empty dir (user is mid-setup) — not a parse failure.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(errs.is_empty());
        assert!(got.get("orgs").expect("resolved").is_empty());
    }

    #[test]
    fn resolve_reports_parse_errors_for_broken_upstream() {
        // A readable upstream whose own main.crn fails to parse must surface as
        // a resolve error — otherwise downstream typos get silently masked.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(&upstream_dir, "main.crn", "not valid crn syntax {{{");
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(!got.contains_key("orgs"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].binding, "orgs");
        let rendered = errs[0].to_string();
        assert!(rendered.contains("orgs"), "{rendered}");
        assert!(rendered.contains("organizations"), "{rendered}");
        assert!(rendered.contains("Syntax error"), "{rendered}");
        assert!(rendered.contains("--> 1:"), "{rendered}");
        assert_eq!(
            rendered.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {rendered}"
        );
    }

    #[test]
    fn resolve_reports_parse_errors_for_broken_imported_module() {
        // A readable upstream whose imported module fails to parse must
        // preserve the ModuleError leaf through the String boundary.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        let broken_module = upstream_dir.join("broken");
        fs::create_dir(&broken_module).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "let broken = use { source = './broken' }\nlet instance = broken {}\n",
        );
        write_crn(&broken_module, "main.crn", "let =\n");
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            resolve_upstream_exports(&base, &[upstream("orgs", "../organizations")], &ctx());
        assert!(!got.contains_key("orgs"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].binding, "orgs");
        let rendered = errs[0].to_string();
        assert!(rendered.contains("orgs"), "{rendered}");
        assert!(rendered.contains("organizations"), "{rendered}");
        assert!(rendered.contains("Syntax error"), "{rendered}");
        assert!(rendered.contains("--> 1:"), "{rendered}");
        assert_eq!(
            rendered.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {rendered}"
        );
    }

    #[test]
    fn check_rejects_unexported_field_with_suggestion() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                bad: String = orgs.account
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "account");
        assert_eq!(errs[0].suggestion.as_deref(), Some("accounts"));
        assert!(errs[0].to_string().contains("exports.bad"));
        assert!(
            errs[0]
                .diagnostic_message()
                .contains("Did you mean `accounts`?")
        );
    }

    #[test]
    fn check_accepts_exported_field() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                good: String = orgs.accounts
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert!(errs.is_empty(), "got: {errs:?}");
    }

    #[test]
    fn coalesce_lhs_suppresses_only_missing_export_existence_diagnostic() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                selected: list(String) = orgs.optional ?? orgs.required
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(errs[0].field, "required");
    }

    #[test]
    fn coalesce_lhs_nested_references_remain_required() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                list_value: list(String) = [orgs.in_list] ?? []
                interpolated: String = "${orgs.in_interpolation}" ?? "fallback"
                function_arg: String = upper(orgs.in_function) ?? "fallback"
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        let fields = errs
            .iter()
            .map(|error| error.field.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            fields,
            std::collections::BTreeSet::from(["in_function", "in_interpolation", "in_list",]),
            "only a direct reference/access-chain on the left of ?? is optional: {errs:?}",
        );
    }

    #[test]
    fn deferred_for_coalesce_lhs_may_be_absent() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.accounts ?? []) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert!(errs.is_empty(), "got: {errs:?}");
    }

    #[test]
    fn deferred_for_coalesce_default_keeps_iterable_shape_validation() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.accounts ?? orgs.fallback) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                ("accounts", TypeExpr::List(Box::new(TypeExpr::String))),
                ("fallback", TypeExpr::String),
            ],
        )]);

        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(errs[0].field, "fallback");
    }

    #[test]
    fn deferred_for_coalesce_value_keeps_access_shape_validation() {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.account.missing ?? []) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);

        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(errs[0].bad_segment, "missing");
    }

    #[test]
    fn check_rejects_any_field_when_upstream_declares_no_exports() {
        // Empty key set → every ref is invalid. Provable statically.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                x: String = orgs.anything
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "anything");
        assert!(errs[0].suggestion.is_none());
    }

    #[test]
    fn check_skips_bindings_without_resolved_exports() {
        // Binding absent from exports map → checker ignores it.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }
            let other = upstream_state { source = "../other" }

            exports {
                x: String = other.foo
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        assert!(check_upstream_state_field_references(&parsed, &exports).is_empty());
    }

    #[test]
    fn check_rejects_unexported_field_in_let_binding() {
        // `let x = orgs.acc` stores the ref in `variables`, not in
        // `resources` — walker must cover that surface too.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            let x = orgs.acc
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(errs[0].field, "acc");
        assert!(errs[0].location.contains("let"));
    }

    #[test]
    fn upstream_state_sibling_reference_no_empty_export_error() {
        // Regression: #2847. `bootstrap` declared in `backend.crn`,
        // referenced from `main.crn`. Pre-fix the seeded placeholder
        // surfaced as a `ResourceRef { attribute: "" }` and tripped the
        // walker into emitting `does not export ``.`. Type-split makes
        // the offending shape unrepresentable: a sibling-only seed is a
        // `Value::Deferred(DeferredValue::BindingRef)`, which has no attribute slot for the
        // walker to read, so the empty-field diagnostic class is
        // statically eliminated.
        let tmp = tempfile::tempdir().unwrap();
        write_crn(
            tmp.path(),
            "backend.crn",
            r#"
            let bootstrap = upstream_state { source = "../bootstrap" }
            "#,
        );
        write_crn(
            tmp.path(),
            "main.crn",
            r#"
            let arn = bootstrap.oidc_provider_arn
            "#,
        );
        let parsed = parse_directory(tmp.path(), &ctx()).expect("parse_directory");
        // Whitebox: the bare-binding seed surfaces as `BindingRef`,
        // not `ResourceRef`. The type itself proves the seeded shape
        // can no longer impersonate an attribute reference.
        let bootstrap_var = parsed.variables.get("bootstrap");
        assert!(
            matches!(
                bootstrap_var,
                Some(Value::Deferred(DeferredValue::BindingRef { .. })) | None
            ),
            "sibling-only `bootstrap` must be either absent or a \
             BindingRef, got: {bootstrap_var:?}"
        );
        let exports = mk_exports(&[("bootstrap", &["oidc_provider_arn"])]);
        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "consumer-only sibling reference must not synthesize an \
             empty-field error, got: {errs:?}"
        );
    }

    #[test]
    fn check_rejects_unexported_field_in_for_expression_body() {
        // The iterable is valid (`orgs.accounts`) but a ref inside the body
        // points at a non-exported field. Must still be flagged — body refs
        // don't reach `parsed.resources` until plan-time expansion.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for name, _ in orgs.accounts {
                awscc.ec2.Vpc {
                    name = name
                    cidr_block = orgs.NONEXISTENT
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert!(
            errs.iter().any(|e| e.field == "NONEXISTENT"),
            "for-body ref to non-exported field must be flagged, got: {errs:?}"
        );
    }

    #[test]
    fn for_body_field_error_reported_once_not_twice() {
        // Regression: the old code walked both `deferred.template_resource.attributes`
        // and `deferred.attributes`, potentially reporting the same ref twice.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }
            for name, _ in orgs.accounts {
                aws.s3.Bucket {
                    name = orgs.missing
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs = check_upstream_state_field_references(&parsed, &exports);
        let missing: Vec<_> = errs.iter().filter(|e| e.field == "missing").collect();
        assert_eq!(missing.len(), 1, "must not double-report, got: {missing:?}");
    }

    #[test]
    fn check_rejects_unexported_field_in_for_expression_iterable() {
        // `for name, _ in orgs.account` — issue #1990's canonical repro.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for name, _ in orgs.account {
                awscc.ec2.Vpc {
                    name = name
                    cidr_block = '10.0.0.0/16'
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);

        let errs = check_upstream_state_field_references(&parsed, &exports);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "account");
        assert_eq!(errs[0].suggestion.as_deref(), Some("accounts"));
        assert!(errs[0].location.contains("for"));
    }

    // ================================================================
    // Phase 2 of #1992: type compatibility through the unified binding resolver
    // ================================================================

    /// Build an `UpstreamExports` with typed entries.
    fn mk_typed_exports(pairs: &[(&str, &[(&str, TypeExpr)])]) -> UpstreamExports {
        pairs
            .iter()
            .map(|(binding, fields)| {
                (
                    binding.to_string(),
                    fields
                        .iter()
                        .map(|(name, ty)| (name.to_string(), declared_entry(ty.clone())))
                        .collect(),
                )
            })
            .collect()
    }

    /// Minimal schema registry for test resources: `test.r.res` with a single
    /// typed attribute, registered under provider `test`.
    fn schema_with_attr(
        attr_name: &str,
        attr_type: crate::schema::AttributeType,
    ) -> SchemaRegistry {
        use crate::schema::{AttributeSchema, ResourceSchema};
        let schema = ResourceSchema::new("r.res")
            .attribute(AttributeSchema::optional(attr_name, attr_type).build());
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        registry
    }

    fn parse_project_with_provider(source: &str, provider_name: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        let full = format!(
            "provider {} {{\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}}\n{}",
            provider_name, source
        );
        fs::write(tmp.path().join("main.crn"), full).unwrap();
        parse_directory(tmp.path(), &ctx())
            .expect("parse_directory")
            .into_parsed()
    }

    /// Preserve the historical positional-walker coverage while exercising
    /// the unified binding resolver used by production validation.
    fn check_upstream_ref_types_via_binding_index(
        parsed: &ParsedFile,
        exports: &UpstreamExports,
        schemas: &SchemaRegistry,
    ) -> Vec<String> {
        let bindings = crate::binding_index::BindingIndex::from_parsed_with_upstream_exports(
            parsed, schemas, exports,
        );
        crate::validation::validate_resource_ref_types(
            parsed,
            schemas,
            &std::collections::HashSet::new(),
            &bindings,
        )
        .err()
        .map(|joined| joined.lines().map(str::to_string).collect())
        .unwrap_or_default()
    }

    #[test]
    fn type_check_flags_string_consumer_with_int_export() {
        // Export is `int`, consumer expects string-compatible — mismatch.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.count
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
        assert!(errs[0].contains("orgs.count"), "unexpected: {errs:?}");
        assert!(errs[0].contains("expected String"), "unexpected: {errs:?}");
        assert!(errs[0].contains("got Int"), "unexpected: {errs:?}");
    }

    #[test]
    fn type_check_passes_when_export_type_matches_consumer() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn type_check_skips_exports_without_type_annotation() {
        // Phase 2 requires an annotation on the export to compare against.
        // Without one (export parsed with no `: T`), skip — don't false-flag.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.count
                }
            "#,
            "test",
        );
        let mut exports: UpstreamExports = HashMap::new();
        let mut fields = HashMap::new();
        fields.insert("count".to_string(), inference_not_run_entry());
        exports.insert("orgs".to_string(), fields);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn unified_ref_check_reports_unknown_field() {
        // Existence and type now share `BindingIndex::ref_type`, so the same
        // pass emits the canonical missing-export diagnostic exactly once.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.missing
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected errors: {errs:?}");
        assert!(
            errs[0].contains("upstream_state `orgs` does not export `missing`"),
            "unexpected errors: {errs:?}",
        );
    }

    #[test]
    fn type_check_list_element_mismatch() {
        // Export is `list(int)`, consumer expects `list(string)`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.counts
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::List(Box::new(TypeExpr::Int)))],
        )]);
        let schemas = schema_with_attr(
            "name",
            crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
        );
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
    }

    #[test]
    fn type_check_does_not_apply_function_result_sink_to_function_arguments() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = join(",", orgs.counts)
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::List(Box::new(TypeExpr::Int)))],
        )]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert!(
            errs.is_empty(),
            "a function argument must not be checked against the function result's String sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_list_member_of_union_for_nested_refs() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = [orgs.count]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            crate::schema::AttributeType::union(vec![
                crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
                crate::schema::AttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the list value must select the union's List member and check its String element sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_map_member_of_union_for_nested_refs() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            crate::schema::AttributeType::union(vec![
                crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
                crate::schema::AttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the map value must select the union's Map member and check its String value sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_struct_member_of_union_for_map_value() {
        use crate::schema::StructField;

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            crate::schema::AttributeType::union(vec![
                crate::schema::AttributeType::struct_(
                    "Count".to_string(),
                    vec![
                        StructField::optional("count", crate::schema::AttributeType::string())
                            .build(),
                    ],
                ),
                crate::schema::AttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the map value must select the union's Struct member and check its String field sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_leaves_ambiguous_map_shaped_union_without_a_nested_sink() {
        use crate::schema::StructField;

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            crate::schema::AttributeType::union(vec![
                crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
                crate::schema::AttributeType::struct_(
                    "Count".to_string(),
                    vec![
                        StructField::optional("count", crate::schema::AttributeType::string())
                            .build(),
                    ],
                ),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert!(
            errs.is_empty(),
            "Map and Struct both match the value shape, so no unique nested sink may be guessed: {errs:?}",
        );
    }

    #[test]
    fn type_check_flags_for_body_attribute() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                for _, account_id in orgs.counts {
                    test.r.res {
                        name = orgs.count
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                ("count", TypeExpr::Int),
                ("counts", TypeExpr::List(Box::new(TypeExpr::Int))),
            ],
        )]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "expected one error inside for body, got {errs:?}"
        );
        assert!(errs[0].contains("for-body"), "unexpected: {errs:?}");
    }

    // Issue #2663: a semantic-ARN refinement (e.g. `IamOidcProviderArn`)
    // declared on an upstream export must be assignable to a
    // `Union<Struct(Principal), String>` receiver — the canonical IAM
    // policy `principal` slot. Before the fix, the Struct member made
    // the union fall through to the catch-all reject so the arn was
    // refused at the very position it was designed for.
    #[test]
    fn type_check_accepts_arn_refinement_into_principal_union() {
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let bootstrap = upstream_state { source = "../bootstrap" }
                test.r.res {
                    name = { federated = bootstrap.oidc_provider_arn }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "bootstrap",
            &[(
                "oidc_provider_arn",
                TypeExpr::Simple("iam_oidc_provider_arn".to_string()),
            )],
        )]);
        // Mirrors the canonical IAM `principal` slot shape:
        // `Union<Struct(IamPolicyPrincipal), String>`. The map walker
        // hits the Union catch-all and walks each value against the
        // whole union, so the leaf `Simple(iam_oidc_provider_arn)`
        // must be assignable to the union receiver directly.
        let principal_union = AttributeType::union(vec![
            AttributeType::struct_(
                "IamPolicyPrincipal".to_string(),
                vec![StructField::optional("federated", AttributeType::string()).build()],
            ),
            AttributeType::string(),
        ]);
        let schemas = schema_with_attr("name", principal_union);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "IamOidcProviderArn must be assignable into Union<Struct, String>, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_rejects_distinct_bare_refinement_kinds() {
        // `KmsKeyArn` and `Arn` are distinct bare identity kinds; the shared
        // relation cannot infer a subtype edge from their spelling. #1874's
        // safe widening is represented by structured identities that retain
        // kind `Arn` and place `kms.Key` on the segments axis (covered by the
        // core directional-relation tests).
        use crate::schema::{AttributeType, TypeIdentity};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.key_arn
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("key_arn", TypeExpr::Simple("kms_key_arn".to_string()))],
        )]);
        let arn = AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Arn"),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        );
        let schemas = schema_with_attr("name", arn);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "distinct bare identity kinds must not be treated as a subtype chain: {errs:?}"
        );
    }

    // ================================================================
    // #2447: subscript narrows the export type before the top-level
    // type-compatibility check
    // ================================================================

    #[test]
    fn type_check_map_subscript_narrows_to_value_type() {
        // `orgs.accounts['k']` against export `map(AwsAccountId)` narrows to
        // the value type before being compared to the receiver. Without the
        // narrowing the comparison fires `map(AwsAccountId)` against the
        // receiver and (when the receiver is anything but a compatible map)
        // false-flags the position. Issue #2447.
        use crate::schema::{AttributeType, TypeIdentity};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts['k']
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        // Receiver is `String`. Without narrowing, `map(AwsAccountId)`
        // compares against `String` and fails. With narrowing,
        // `AwsAccountId` (Custom over String) is accepted.
        let aws_account_id = AttributeType::refined_string_with_validator(
            TypeIdentity::bare("AwsAccountId"),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        );
        let schemas = schema_with_attr("name", aws_account_id);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "subscript narrows map(T) to T before compare, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_subscript_inside_struct_position_does_not_false_flag() {
        // The bucket-policy repro from #2447: a deeply-nested ref inside a
        // struct attribute (`policy: Struct(IamPolicyDocument)`) where
        // narrowing through a subscript produces a scalar that *could*
        // legitimately occupy a leaf string position inside the struct.
        // The top-level check must not reject the narrowed scalar against
        // the outer struct type — positional walking is the precise tool
        // for that, and the existing top-level check is too coarse.
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = "arn:aws:iam::${orgs.accounts['registry_dev']}:root"
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let policy_struct = AttributeType::struct_(
            "IamPolicyDocument".to_string(),
            vec![StructField::required("statement", AttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "subscript-narrowed scalar must not be rejected against outer \
             struct attr, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_dot_form_chains_through_map_into_struct_field() {
        // `orgs.accounts.k.account_id` where `accounts: map(struct{
        // account_id: aws_account_id })`. The dot walk descends
        // map(T).k → T (struct), then struct.account_id → aws_account_id.
        // Both legs of `walk_type_expr_path` exercised by one path.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.alpha.account_id
                }
            "#,
            "test",
        );
        let account_struct = TypeExpr::Struct {
            fields: vec![(
                "account_id".to_string(),
                TypeExpr::Simple("aws_account_id".to_string()),
            )],
        };
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(account_struct)))],
        )]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "dot-walk through map into struct field must resolve to leaf type, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_list_subscript_narrows_to_element_type() {
        // Same shape as the map case for `list(T)[idx]`: subscript narrows
        // to T before the top-level check.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.regions[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("regions", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "list subscript narrows list(T) to T before compare, got: {errs:?}"
        );
    }

    // ================================================================
    // #2475: positional walker — narrowed scalar is compared against the
    // *position* type, not the outer attribute type.
    // ================================================================

    #[test]
    fn type_check_walker_resolves_struct_block_name_alias() {
        // The walker's `Struct` arm resolves field keys via
        // `struct_field_for_key`, which honours `block_name`
        // aliases. Without that resolution a ref written under the
        // alias would silently skip the type check (the alias key
        // wouldn't match any `field.name`). Pin the path: a field
        // declared as `name="statements", block_name="statement"`
        // accessed via the alias must still get its leaf ref checked
        // — here the leaf is a narrowed `Int` against a `String`
        // declaration, so the walker must flag the mismatch.
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = orgs.counts['k']
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::Map(Box::new(TypeExpr::Int)))],
        )]);
        let policy_struct = AttributeType::struct_(
            "PolicyDoc".to_string(),
            vec![
                StructField::required("statements", AttributeType::string())
                    .with_block_name("statement")
                    .build(),
            ],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "ref under block_name alias `statement` must be type-checked, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_int_in_int_field_passes_without_string_shape_leniency() {
        // Counterpart to the mismatched-leaf test below: narrowed Int
        // against an Int-typed struct field must pass via the strict
        // compatibility check, not via the string-shape leniency arm
        // (which only fires for `is_string_compatible_type` receivers).
        // Pin the path so a regression that wrongly broadened leniency
        // would still surface.
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    config = {
                        port = orgs.ports['k']
                    }
                }
            "#,
            "test",
        );
        let exports =
            mk_typed_exports(&[("orgs", &[("ports", TypeExpr::Map(Box::new(TypeExpr::Int)))])]);
        let cfg_struct = AttributeType::struct_(
            "Cfg".to_string(),
            vec![StructField::required("port", AttributeType::int()).build()],
        );
        let schemas = schema_with_attr("config", cfg_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "narrowed Int in Int-typed struct field must pass strict compat, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_scalar_in_struct_field_with_mismatched_leaf_flags() {
        // Acceptance criterion 1 from #2475: a narrowed `Int` (from a
        // subscript on `map(Int)`) sitting in a struct field whose
        // declared type is `String` must be flagged. Pre-#2475 the
        // `is_scalar_type_expr` short-circuit silently accepted this.
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = orgs.counts['k']
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::Map(Box::new(TypeExpr::Int)))],
        )]);
        let policy_struct = AttributeType::struct_(
            "PolicyDoc".to_string(),
            vec![StructField::required("statement", AttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "narrowed Int in String-typed struct field must flag, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_scalar_in_struct_field_with_matching_leaf_passes() {
        // Acceptance criterion 2 from #2475: the same shape but the
        // struct field's declared type matches the narrowed scalar —
        // the bucket-policy repro from #2447 — must continue to pass.
        // (Pre-fix this passed via the `is_scalar_type_expr`
        // short-circuit; post-fix it must pass via the positional
        // walker comparing AwsAccountId against the field's String.)
        use crate::schema::{AttributeType, StructField};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = "arn:aws:iam::${orgs.accounts['registry_dev']}:root"
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let policy_struct = AttributeType::struct_(
            "IamPolicyDocument".to_string(),
            vec![StructField::required("statement", AttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "narrowed AwsAccountId in String-shaped position must pass, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_map_inside_interpolation_flags() {
        // Acceptance criterion 3 from #2475: a `map(_)` ref embedded
        // inside a string interpolation cannot satisfy the String
        // position the interpolation forces. The outer attribute is
        // `tags: Map(String, String)`, so the *outer* compare
        // `map(...)` vs `map(String)` would happily succeed (inner
        // String ⊑ String). Only a positional walker, descending into
        // the map value's interpolation, can spot that the
        // interpolation position is String and the embedded ref is a
        // `map(...)`. Pre-#2475 this regression slipped through.
        use crate::schema::AttributeType;
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    tags = { Foo = "all accounts: ${orgs.accounts}" }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let schemas = schema_with_attr("tags", AttributeType::map(AttributeType::string()));
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "map embedded in tag-value interpolation must flag, got: {errs:?}"
        );
    }

    // ================================================================
    // #1894: for-iterable shape compatibility
    // (`check_upstream_state_for_iterable_shapes`)
    // ================================================================

    #[test]
    fn for_iterable_simple_binding_against_list_export_is_ok() {
        // `for x in orgs.accounts` over a `list(...)` export — fine.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "list export + simple binding must pass, got: {errs:?}"
        );
    }

    #[test]
    fn for_iterable_map_binding_against_map_export_is_ok() {
        // `for k, v in orgs.accounts` over a `map(...)` export — fine.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for name, account_id in orgs.accounts {
                    test.r.res {
                        name = name
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "map export + map binding must pass, got: {errs:?}"
        );
    }

    #[test]
    fn for_iterable_simple_binding_against_map_export_flags_mismatch() {
        // Upstream changed `accounts: list(_)` to `map(_)`. Downstream
        // still iterates as `for x in ...` — old shape. The check must
        // flag this so the cross-directory refactor surfaces in the
        // downstream plan/validate output rather than blowing up at
        // apply time. This is the canonical #1894 repro.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "map export + simple binding must fail, got: {errs:?}"
        );
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "accounts");
        assert_eq!(errs[0].binding_kind, ForIterableBindingKind::List);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("map(AwsAccountId)") && msg.contains("requires a list"),
            "message must show map export and list-iterable expectation: {msg}"
        );
        assert!(
            msg.contains("for k, v in"),
            "message must suggest the map binding form: {msg}"
        );
    }

    #[test]
    fn for_iterable_map_binding_against_list_export_flags_mismatch() {
        // Inverse of the above: downstream uses `for k, v in ...` but
        // upstream still exports a list. Same class of bug.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for name, account_id in orgs.accounts {
                    test.r.res {
                        name = name
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "list export + map binding must fail, got: {errs:?}"
        );
        assert_eq!(errs[0].field, "accounts");
        assert_eq!(errs[0].binding_kind, ForIterableBindingKind::Map);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list(AwsAccountId)") && msg.contains("requires a map"),
            "message must show list export and map-iterable expectation: {msg}"
        );
        assert!(
            msg.contains("for x in"),
            "message must suggest the list binding form: {msg}"
        );
    }

    #[test]
    fn for_iterable_skipped_when_export_has_no_declared_type() {
        // `accounts` declared without `: T` annotation — nothing to
        // compare against, the field-name check still runs but the
        // shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("accounts".to_string(), inference_not_run_entry());
        exports.insert("orgs".to_string(), fields);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
    }

    #[test]
    fn for_iterable_skipped_when_binding_unknown() {
        // The binding isn't in `exports` at all (resolve failed
        // upstream). Field-name check handles that case via
        // UpstreamFieldError; the shape check stays silent so the
        // user only sees one diagnostic.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
    }

    #[test]
    fn for_iterable_shape_check_against_real_directory_fixture() {
        // Directory-scoped acceptance: upstream's `exports.crn` lives in
        // a sibling directory and `resolve_upstream_exports` parses it
        // off disk, then the shape check fires when downstream's
        // `for` binding doesn't match the exported `list ↔ map`. This
        // is the realistic shape of #1894 (cross-directory refactor).
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: map(String) = {
                    alpha = "111111111111"
                }
            }"#,
        );
        write_crn(
            &upstream_dir,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        write_crn(
            &base,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        write_crn(
            &base,
            "main.crn",
            r#"
                let orgs = upstream_state { source = "../organizations" }
                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
        );
        let parsed = parse_directory(&base, &ctx()).expect("parse_directory");
        let (exports, resolve_errs) =
            resolve_upstream_exports(&base, &parsed.upstream_states, &ctx());
        assert!(
            resolve_errs.is_empty(),
            "unexpected resolve errors: {resolve_errs:?}"
        );
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "expected one shape mismatch from list↔map upstream change, got: {errs:?}"
        );
        assert_eq!(errs[0].field, "accounts");
        assert_eq!(errs[0].binding_kind, ForIterableBindingKind::List);
    }

    #[test]
    fn for_iterable_against_scalar_export_flags_mismatch() {
        // Upstream exports a scalar (e.g. `accounts: String`); downstream
        // tries to iterate. There is no valid binding form to suggest —
        // the upstream contract has to change first. The check fires
        // and the message stays honest by omitting the suggestion.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("accounts", TypeExpr::String)])]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "scalar export + for must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
        assert!(
            msg.contains("requires a list"),
            "message must say list expected: {msg}"
        );
        assert!(
            !msg.contains("for k, v in") && !msg.contains("for x in"),
            "scalar export must not suggest a binding form: {msg}"
        );
    }

    #[test]
    fn for_iterable_indexed_binding_classified_as_list() {
        // `for (i, x) in orgs.accounts` is a *list* iterable — the
        // 2-name binding pattern is `(index, value)`, not `(key, value)`.
        // The kind classifier must collapse Simple+Indexed into List.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for (i, account_id) in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        // List export — indexed binding is fine.
        let list_exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        assert!(
            check_upstream_state_for_iterable_shapes(&parsed, &list_exports).is_empty(),
            "indexed binding against list export must pass"
        );
        // Map export — indexed binding is the wrong shape.
        let map_exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &map_exports);
        assert_eq!(
            errs.len(),
            1,
            "indexed binding + map export must fail, got: {errs:?}"
        );
        assert_eq!(errs[0].binding_kind, ForIterableBindingKind::List);
    }

    #[test]
    fn for_iterable_errors_sort_stably() {
        // Two `for` expressions in a single project produce errors in
        // a deterministic order regardless of the underlying parse order.
        // Mirrors the existing sort guarantee on `UpstreamFieldError`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.alpha_accounts {
                    test.r.res {
                        name = account_id
                    }
                }

                for account_id in orgs.beta_accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                (
                    "alpha_accounts",
                    TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
                ),
                (
                    "beta_accounts",
                    TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
                ),
            ],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 2, "expected two errors, got: {errs:?}");
        assert_eq!(errs[0].field, "alpha_accounts");
        assert_eq!(errs[1].field, "beta_accounts");
    }

    #[test]
    fn for_iterable_skipped_when_field_not_exported() {
        // The binding exists but the iterable field isn't exported.
        // `UpstreamFieldError` already surfaces this; shape check
        // stays silent to avoid duplicate diagnostics.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.NONEXISTENT {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "missing field → silent, got: {errs:?}");
    }

    // ================================================================
    // #1894 follow-up: attribute-access shape compatibility
    // (`check_upstream_state_attribute_access_shapes`)
    // ================================================================

    #[test]
    fn attribute_access_struct_with_known_field_is_ok() {
        // Upstream exports `account: struct { id: String, region: String }`,
        // downstream writes `orgs.account.id` — the `id` field is declared,
        // shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![
                        ("id".to_string(), TypeExpr::String),
                        ("region".to_string(), TypeExpr::String),
                    ],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "known struct field must pass, got: {errs:?}"
        );
    }

    #[test]
    fn list_field_access_keeps_the_dedicated_upstream_shape_diagnostic() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                let consumer = test.r.res {
                    name = orgs.names.foo
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("names", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::string());

        let generic_errors =
            check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            generic_errors.is_empty(),
            "generic reference validation must leave this case to the dedicated upstream diagnostic: {generic_errors:?}"
        );

        let errors = check_upstream_state_attribute_access_shape_fallbacks(&parsed, &exports);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].to_string(),
            "test.r.res.consumer attribute `name`: upstream_state `orgs.names` is declared as `list(String)` but `.foo` reads it as a struct; iterate the list with `for x in orgs.names` to access elements"
        );
    }

    #[test]
    fn attribute_access_struct_with_unknown_field_flags_mismatch() {
        // `orgs.account.region` against a struct that doesn't declare
        // `region` is the canonical "downstream depends on upstream's
        // schema and the upstream changed" failure.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.region
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "unknown struct field must fail, got: {errs:?}"
        );
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "account");
        assert_eq!(errs[0].field_path, vec!["region".to_string()]);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("region"),
            "message must name the missing field: {msg}"
        );
    }

    #[test]
    fn attribute_access_against_list_export_flags_mismatch() {
        // `orgs.accounts.foo` against `accounts: list(_)` is a category
        // error — the user has to iterate first.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.foo
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "field access on list must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list"),
            "message must mention list shape: {msg}"
        );
    }

    #[test]
    fn attribute_access_against_map_export_resolves_as_key_access() {
        // `orgs.accounts.alpha` against `accounts: map(_)` is valid
        // map-key access, equivalent to `orgs.accounts['alpha']`. The
        // shape walk descends `map(T).k → T` for the dot form, the
        // same way it does for the subscript form. Issue #2447 — both
        // notations must resolve a single map entry.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.alpha
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(TypeExpr::Simple("aws_account_id".to_string()))),
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "dot-notation against map must be accepted as key access, got: {errs:?}"
        );
    }

    #[test]
    fn attribute_access_against_scalar_export_flags_mismatch() {
        // `orgs.region.foo` against `region: String` — scalars have no
        // fields.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region.foo
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "field access on scalar must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
    }

    #[test]
    fn attribute_access_skipped_when_export_has_no_declared_type() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("account".to_string(), inference_not_run_entry());
        exports.insert("orgs".to_string(), fields);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
    }

    #[test]
    fn attribute_access_skipped_when_binding_unknown() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
    }

    #[test]
    fn attribute_access_skipped_when_field_not_exported() {
        // `orgs.NONEXISTENT.id` — field-name check already surfaces
        // the missing top-level export; the shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.NONEXISTENT.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "missing field → silent, got: {errs:?}");
    }

    #[test]
    fn attribute_access_skipped_when_field_path_empty() {
        // `orgs.account` (no `.foo`) — that's just a top-level field
        // ref, handled by the typed consumer through `BindingIndex::ref_type`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "no field_path → silent, got: {errs:?}");
    }

    #[test]
    fn attribute_access_nested_struct_field_walks_path() {
        // `orgs.account.network.vpc_id` against
        // `account: struct { network: struct { vpc_id: String } }` — the
        // walker must descend into nested structs.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.network.vpc_id
                }
            "#,
            "test",
        );
        let inner = TypeExpr::Struct {
            fields: vec![("vpc_id".to_string(), TypeExpr::String)],
        };
        let outer = TypeExpr::Struct {
            fields: vec![("network".to_string(), inner)],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "nested struct path must pass, got: {errs:?}"
        );
    }

    #[test]
    fn attribute_access_nested_struct_unknown_field_flagged() {
        // Same fixture but the deep field is wrong — must still be
        // caught.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.network.bad_field
                }
            "#,
            "test",
        );
        let inner = TypeExpr::Struct {
            fields: vec![("vpc_id".to_string(), TypeExpr::String)],
        };
        let outer = TypeExpr::Struct {
            fields: vec![("network".to_string(), inner)],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "nested unknown field must fail, got: {errs:?}"
        );
        assert_eq!(
            errs[0].field_path,
            vec!["network".to_string(), "bad_field".to_string()]
        );
        assert_eq!(errs[0].bad_segment, "bad_field");
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("bad_field"),
            "message must name the missing leaf: {msg}"
        );
    }

    #[test]
    fn attribute_access_struct_containing_list_flags_at_list_position() {
        // `account: struct { tags: list(String) }` — `orgs.account.tags`
        // resolves cleanly (struct → list, no further `.field`); but
        // `orgs.account.tags.foo` walks past the list and must flag
        // there. The `mismatched_at` is the list, not the outer struct.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.tags.foo
                }
            "#,
            "test",
        );
        let outer = TypeExpr::Struct {
            fields: vec![(
                "tags".to_string(),
                TypeExpr::List(Box::new(TypeExpr::String)),
            )],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 1, "list-at-depth must flag, got: {errs:?}");
        assert_eq!(errs[0].bad_segment, "foo");
        assert!(
            matches!(errs[0].mismatched_at, TypeExpr::List(_)),
            "mismatched_at must be the inner list, got: {:?}",
            errs[0].mismatched_at
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list") && msg.contains("for x in"),
            "message must mention list and suggest iteration: {msg}"
        );
    }

    #[test]
    fn attribute_access_shape_check_against_real_directory_fixture() {
        // Directory-scoped acceptance: upstream's `exports.crn` lives
        // in a sibling directory and `resolve_upstream_exports` parses
        // it off disk. Mirrors the parity test for the for-iterable
        // sibling (`for_iterable_shape_check_against_real_directory_fixture`).
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: list(String) = ["111111111111"]
            }"#,
        );
        write_crn(
            &upstream_dir,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        write_crn(
            &base,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        write_crn(
            &base,
            "main.crn",
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.foo
                }
            "#,
        );
        let parsed = parse_directory(&base, &ctx()).expect("parse_directory");
        let (exports, resolve_errs) =
            resolve_upstream_exports(&base, &parsed.upstream_states, &ctx());
        assert!(
            resolve_errs.is_empty(),
            "unexpected resolve errors: {resolve_errs:?}"
        );
        let errs = check_upstream_state_attribute_access_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "expected one shape mismatch from list export + .field access, got: {errs:?}"
        );
        assert_eq!(errs[0].field, "accounts");
        assert_eq!(errs[0].bad_segment, "foo");
        assert!(matches!(errs[0].mismatched_at, TypeExpr::List(_)));
    }

    // ================================================================
    // #2318: subscript-after-field shape compatibility
    // (`check_upstream_state_subscript_shapes`)
    // ================================================================

    #[test]
    fn subscript_int_against_list_export_is_ok() {
        // `orgs.accounts[0]` against `accounts: list(String)` — integer
        // subscripts read list elements; shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "[int] on list(_) must pass, got: {errs:?}");
    }

    #[test]
    fn subscript_str_against_map_export_is_ok() {
        // `orgs.accounts["alpha"]` against `accounts: map(String)` —
        // string subscripts read map entries; shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(TypeExpr::String)))],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "[\"k\"] on map(_) must pass, got: {errs:?}"
        );
    }

    #[test]
    fn subscript_int_against_map_export_flags_mismatch() {
        // `orgs.accounts[0]` against `accounts: map(String)` — wrong
        // syntax; should fire with a hint to use `["k"]`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(TypeExpr::String)))],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 1, "[int] on map must fail, got: {errs:?}");
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "accounts");
        assert!(matches!(errs[0].bad_subscript, Subscript::Int { index: 0 }));
        let msg = errs[0].diagnostic_message();
        assert!(msg.contains("map"), "message must mention map shape: {msg}");
        assert!(
            msg.contains("[0]"),
            "message must echo the user's literal subscript, got: {msg}"
        );
    }

    #[test]
    fn subscript_str_against_list_export_flags_mismatch() {
        // `orgs.accounts["alpha"]` against `accounts: list(String)` —
        // wrong syntax; should fire with a hint to use `[i]`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert_eq!(errs.len(), 1, "[\"k\"] on list must fail, got: {errs:?}");
        assert!(matches!(errs[0].bad_subscript, Subscript::Str { .. }));
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list"),
            "message must mention list shape: {msg}"
        );
    }

    #[test]
    fn postfix_access_on_upstream_ref_participates_in_subscript_shape_checks() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                let accounts = orgs.accounts
                test.r.res {
                    direct = (orgs.accounts)[0]
                    alias  = accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);

        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);

        assert_eq!(
            errs.len(),
            1,
            "the valid integer postfix access should pass and the string alias access should fail: {errs:?}"
        );
        assert!(matches!(errs[0].bad_subscript, Subscript::Str { .. }));
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "accounts");
    }

    #[test]
    fn subscript_against_struct_export_flags_mismatch() {
        // `orgs.account[0]` against `account: struct {...}` — structs
        // don't host any subscript form. The diagnostic stays silent on
        // the "use the other syntax" hint because neither would help.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "subscript on struct must fail, got: {errs:?}"
        );
    }

    #[test]
    fn subscript_against_scalar_export_flags_mismatch() {
        // `orgs.region[0]` against `region: String` — scalars are not
        // subscriptable.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "subscript on scalar must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
    }

    #[test]
    fn subscript_after_struct_field_walks_path() {
        // `orgs.account.children[0]` against
        // `account: struct { children: list(String) }` — the walker
        // must descend through the struct field before checking the
        // subscript.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.children[0]
                }
            "#,
            "test",
        );
        let outer = TypeExpr::Struct {
            fields: vec![(
                "children".to_string(),
                TypeExpr::List(Box::new(TypeExpr::String)),
            )],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "subscript after struct field walks path, got: {errs:?}"
        );
    }

    #[test]
    fn subscript_skipped_when_export_has_no_declared_type() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("accounts".to_string(), inference_not_run_entry());
        exports.insert("orgs".to_string(), fields);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
    }

    #[test]
    fn subscript_skipped_when_binding_unknown() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
    }

    #[test]
    fn subscript_skipped_when_field_path_mismatches() {
        // `orgs.account.bad[0]` — the `.bad` already mismatches the
        // declared struct type. Don't double-fire on the subscript;
        // the attribute-access check owns that diagnostic.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.bad[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "field-path mismatch is the attribute-access check's job, got: {errs:?}"
        );
    }

    #[test]
    fn subscript_chained_descends_into_inner_type() {
        // `orgs.matrix[0][1]` against `matrix: list(list(String))` —
        // each integer subscript peels one `list(_)` layer.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.matrix[0][1]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "matrix",
                TypeExpr::List(Box::new(TypeExpr::List(Box::new(TypeExpr::String)))),
            )],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert!(
            errs.is_empty(),
            "list(list(_))[0][1] must pass, got: {errs:?}"
        );
    }

    #[test]
    fn subscript_chained_flags_inner_mismatch() {
        // `orgs.matrix[0]["k"]` against `matrix: list(list(String))` —
        // first `[0]` peels to `list(String)`; second `["k"]` then
        // mismatches.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.matrix[0]["k"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "matrix",
                TypeExpr::List(Box::new(TypeExpr::List(Box::new(TypeExpr::String)))),
            )],
        )]);
        let errs = check_upstream_state_subscript_shapes(&parsed, &exports);
        assert_eq!(
            errs.len(),
            1,
            "second subscript mismatch must fire, got: {errs:?}"
        );
        assert!(matches!(errs[0].bad_subscript, Subscript::Str { .. }));
    }

    // ================================================================
    // #2319: UpstreamRefDiagnostic trait
    // ================================================================

    #[test]
    fn upstream_ref_diagnostic_trait_covers_all_four_error_types() {
        // Building a `Vec<&dyn UpstreamRefDiagnostic>` proves each type
        // implements the trait and that the LSP/CLI can iterate them
        // uniformly. The CLI/LSP wirings rely on this being possible.
        let field_err = UpstreamFieldError {
            location: "loc-a".to_string(),
            binding: "orgs".to_string(),
            field: "missing".to_string(),
            suggestion: Some("accounts".to_string()),
        };
        let shape_err = UpstreamForIterableShapeError {
            location: "for-expression `for x in orgs.accounts`".to_string(),
            binding: "orgs".to_string(),
            field: "accounts".to_string(),
            export_type: TypeExpr::Map(Box::new(TypeExpr::String)),
            binding_kind: ForIterableBindingKind::List,
        };
        let attr_err = UpstreamAttributeAccessShapeError {
            location: "loc-d".to_string(),
            binding: "orgs".to_string(),
            field: "account".to_string(),
            field_path: vec!["bad".to_string()],
            mismatched_at: TypeExpr::Struct {
                fields: vec![("id".to_string(), TypeExpr::String)],
            },
            bad_segment: "bad".to_string(),
        };
        let subscript_err = UpstreamSubscriptShapeError {
            location: "loc-e".to_string(),
            binding: "orgs".to_string(),
            field: "accounts".to_string(),
            field_path: vec![],
            mismatched_at: TypeExpr::Map(Box::new(TypeExpr::String)),
            bad_subscript: Subscript::Int { index: 0 },
        };
        // Tuples of (dyn-trait reference, expected location, expected
        // binding, expected field, expected diagnostic_message). Looping
        // catches a future struct that silently desyncs trait impls
        // from inherent fields.
        let cases: Vec<(&dyn UpstreamRefDiagnostic, &str, &str, &str, String)> = vec![
            (
                &field_err,
                "loc-a",
                "orgs",
                "missing",
                field_err.diagnostic_message(),
            ),
            (
                &shape_err,
                "for-expression `for x in orgs.accounts`",
                "orgs",
                "accounts",
                shape_err.diagnostic_message(),
            ),
            (
                &attr_err,
                "loc-d",
                "orgs",
                "account",
                attr_err.diagnostic_message(),
            ),
            (
                &subscript_err,
                "loc-e",
                "orgs",
                "accounts",
                subscript_err.diagnostic_message(),
            ),
        ];
        for (d, expected_location, expected_binding, expected_field, expected_message) in cases {
            assert_eq!(d.location(), expected_location);
            assert_eq!(d.binding(), expected_binding);
            assert_eq!(d.field(), expected_field);
            assert_eq!(d.diagnostic_message(), expected_message);
            // `Display` supertrait must produce the canonical
            // `"location: message"` form — the CLI's combined-error
            // path relies on this format.
            assert_eq!(
                d.to_string(),
                format!("{}: {}", d.location(), d.diagnostic_message())
            );
        }
    }

    #[test]
    fn resolve_upstream_exports_with_schemas_uses_inference_for_unannotated() {
        // Stage 2 (#2360): the resolver hoists the upstream parse
        // through `apply_inference`, so an unannotated literal export
        // surfaces as a typed entry to consumers without the user
        // having to write `: String`.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "exports {\n  region = \"us-east-1\"\n}\n",
        );

        let upstream_state = upstream("ups", "upstream");
        let (exports, _errors) = resolve_upstream_exports_with_schemas(
            tmp.path(),
            &[upstream_state],
            &ctx(),
            Some(&crate::schema::SchemaRegistry::new()),
        );
        let region_entry = exports
            .get("ups")
            .and_then(|m| m.get("region"))
            .expect("region should be inferred");
        assert_eq!(
            region_entry.static_type,
            UpstreamExportType::Static(ExportStaticType::Inferred(consumer_inferred_type(
                TypeExpr::String,
            )))
        );
    }

    #[test]
    fn resolve_upstream_exports_resolves_declarations_before_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "arguments { value: test.types.Identifier }\n\
             let item = test.Item {}\n\
             exports { out = [value, item.identifier] }\n",
        );

        let identity = crate::schema::TypeIdentity::from_schema_type("test", "types", "Identifier");
        let owned = crate::parser::OwnedTypeIdentity::provider("test", identity);
        let config = LoadedProviderContext::for_test(ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                std::collections::HashSet::from([owned.clone()]),
                Box::new(|_target, _value| Ok(())),
            ),
            ..ProviderContext::default()
        });
        let mut schemas = crate::schema::SchemaRegistry::new();
        schemas.insert(
            "test",
            crate::schema::ResourceSchema::new("Item")
                .attribute(
                    crate::schema::AttributeSchema::optional(
                        "identifier",
                        crate::schema::AttributeType::refined_string(
                            Some(owned.identity().clone()),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("schema fixture must be valid"),
        );

        let (exports, errors) = resolve_upstream_exports_with_schemas(
            tmp.path(),
            &[upstream("ups", "upstream")],
            &config,
            Some(&schemas),
        );

        assert!(errors.is_empty(), "unexpected resolve errors: {errors:?}");
        assert_eq!(
            exports["ups"]["out"].static_type,
            UpstreamExportType::Static(ExportStaticType::Inferred(consumer_inferred_type(
                TypeExpr::List(Box::new(TypeExpr::SchemaType { owned })),
            )))
        );
    }

    #[test]
    fn module_expansion_failure_is_reported_instead_of_inferred_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        let module_dir = upstream_dir.join("module");
        fs::create_dir_all(&module_dir).unwrap();
        write_crn(
            &module_dir,
            "main.crn",
            "arguments { count: Int }\nattributes { out: Int = count }\n",
        );
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"let module = use { source = './module' }
let instance = module { count = 'wrong' }
exports { out: Int = instance.out }
"#,
        );

        let (exports, errors) = resolve_upstream_exports_with_schemas(
            tmp.path(),
            &[upstream("ups", "upstream")],
            &ctx(),
            Some(&crate::schema::SchemaRegistry::new()),
        );

        assert!(
            !exports.contains_key("ups"),
            "a failed expansion must not publish Unknown exports: {exports:?}"
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert!(
            errors[0]
                .reason
                .contains("expected Int, got string 'wrong'"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn providerless_upstream_module_unknown_consumer_custom_type_is_unchecked() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        let module_dir = upstream_dir.join("module");
        fs::create_dir_all(&module_dir).unwrap();
        write_crn(
            &module_dir,
            "main.crn",
            "arguments { vpc: awscc.ec2.VpcId }\nattributes { out: String = vpc }\n",
        );
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"let module = use { source = './module' }
let instance = module { vpc = 'vpc-123' }
exports { out: String = instance.out }
"#,
        );

        let (exports, errors) = resolve_upstream_exports_providerless_with_schemas(
            tmp.path(),
            &[upstream("ups", "upstream")],
            &ProviderContext::default(),
            Some(&crate::schema::SchemaRegistry::new()),
        );

        assert!(
            errors.is_empty(),
            "unknown consumer type must defer: {errors:?}"
        );
        assert!(
            exports
                .get("ups")
                .is_some_and(|values| values.contains_key("out")),
            "the upstream export must remain available: {exports:?}"
        );
    }
}
