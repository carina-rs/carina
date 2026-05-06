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
use crate::parser::{ProviderContext, ResourceContext, TypeExpr, UpstreamState};
use crate::resource::{Subscript, Value};
use crate::schema::{AttributeType, SchemaRegistry, suggest_similar_name};

/// One export's declared type and literal value, as carried through
/// [`UpstreamExports`]. `type_expr` is `None` when the upstream's
/// `exports` block omits the annotation and inference can't recover
/// it. `value` is currently always `Some` (the parser requires
/// `name = expr`) but is kept `Option` for forward compatibility with
/// future grammar widenings.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamExportEntry {
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// Exports for a single `upstream_state` binding: export name → entry.
pub type UpstreamExportEntries = HashMap<String, UpstreamExportEntry>;

/// Exports declared by each `upstream_state` binding: binding name →
/// per-export entry map. Carrying the literal `Value` alongside the
/// `TypeExpr` lets downstream consumers (LSP map-literal-key
/// completion, future doc tools) read the body without re-parsing
/// the upstream directory.
pub type UpstreamExports = HashMap<String, UpstreamExportEntries>;

/// A diagnostic about a `binding.field` reference whose downstream usage
/// doesn't fit the upstream's exports.
///
/// Five error types share this shape — name-not-exported
/// ([`UpstreamFieldError`]), top-level type mismatch
/// ([`UpstreamTypeError`]), `for`-iterable shape mismatch
/// ([`UpstreamForIterableShapeError`]), attribute-access shape
/// mismatch ([`UpstreamAttributeAccessShapeError`]), and subscript
/// shape mismatch ([`UpstreamSubscriptShapeError`]). They share their
/// CLI and LSP wirings through this trait so adding a sixth check is
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
pub fn resolve_upstream_exports(
    base_dir: &Path,
    upstream_states: &[UpstreamState],
    config: &ProviderContext,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    resolve_upstream_exports_with_schemas(base_dir, upstream_states, config, None)
}

/// As [`resolve_upstream_exports`], but consult `schemas` to infer
/// missing `type_expr` annotations from each export's rhs (#2361).
/// When `schemas` is `None` the behavior is identical to the legacy
/// entry point — the export's `type_expr` is forwarded as-is. Production
/// callers (CLI validate, LSP diagnostics, LSP completion) thread the
/// real schema registry so unannotated exports become typed before
/// downstream type-checks see them.
pub fn resolve_upstream_exports_with_schemas(
    base_dir: &Path,
    upstream_states: &[UpstreamState],
    config: &ProviderContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> (UpstreamExports, Vec<UpstreamResolveError>) {
    let mut out: UpstreamExports = HashMap::new();
    let mut errors: Vec<UpstreamResolveError> = Vec::new();
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source);
        if !source_abs.is_dir() {
            continue;
        }
        if matches!(find_crn_files_in_dir(&source_abs), Ok(files) if files.is_empty()) {
            out.insert(us.binding.clone(), HashMap::new());
            continue;
        }
        match parse_directory(&source_abs, config) {
            Ok(parsed) => {
                // When schemas are available, hoist the upstream parse
                // through `apply_inference` so every export carries a
                // bare `TypeExpr` (possibly the `Unknown` sentinel for
                // failures) — the same pipeline the loader runs on the
                // current directory (#2360 stage 2). Inference errors
                // are silently dropped here: the upstream's own
                // validate run reports them, and re-emitting from this
                // resolver would double-report.
                let entries: UpstreamExportEntries = match schemas {
                    Some(s) => {
                        let (inferred, _errors) =
                            crate::validation::inference::apply_inference(parsed, s);
                        inferred
                            .export_params
                            .into_iter()
                            // Preserve the legacy "no static type"
                            // surface for sentinels — Unknown carries
                            // nothing worth forwarding to the consumer.
                            .map(|e| {
                                (
                                    e.name,
                                    UpstreamExportEntry {
                                        type_expr: e.type_expr.into_known(),
                                        value: e.value,
                                    },
                                )
                            })
                            .collect()
                    }
                    // `schemas` is None (legacy entry point) — forward
                    // the declared type unchanged. Note that without
                    // schemas there is no way to run inference, so
                    // unannotated exports stay `None`.
                    None => parsed
                        .export_params
                        .into_iter()
                        .map(|e| {
                            (
                                e.name,
                                UpstreamExportEntry {
                                    type_expr: e.type_expr,
                                    value: e.value,
                                },
                            )
                        })
                        .collect(),
                };
                out.insert(us.binding.clone(), entries);
            }
            Err(reason) => {
                errors.push(UpstreamResolveError {
                    binding: us.binding.clone(),
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
fn resource_attr_location(
    ctx: ResourceContext<'_>,
    resource: &crate::resource::Resource,
    attr_name: &str,
) -> String {
    match ctx {
        ResourceContext::Direct => format!("{} attribute `{}`", resource.id, attr_name),
        ResourceContext::Deferred(d) => format!(
            "for-body `{}` {} attribute `{}`",
            d.header, resource.id, attr_name
        ),
    }
}

/// Walk every resource attribute in the project (Direct + deferred-for
/// templates), yielding `(ResourceContext, &Resource, attr_name, &Value)`
/// for each non-internal attribute (skips `_*` keys). Used by all three
/// upstream-ref checks; centralized so the iter_all_resources walk and
/// the `_*` skip are written once.
fn for_each_resource_attr<E, F>(parsed: &crate::parser::File<E>, mut f: F)
where
    F: FnMut(ResourceContext<'_>, &crate::resource::Resource, &str, &Value),
{
    for (ctx, resource) in parsed.iter_all_resources() {
        for (attr_name, value) in resource.attributes.iter() {
            if attr_name.starts_with('_') {
                continue;
            }
            f(ctx, resource, attr_name, value);
        }
    }
}

/// Walk every ref-bearing value outside resources — `let` bindings,
/// `attributes` parameter defaults, `exports` values, and module-call
/// arguments — yielding `(value, location_string)`. Used by
/// [`check_upstream_state_field_references`] and
/// [`check_upstream_state_attribute_access_shapes`]; centralized so a
/// fifth check that wants the same reach gets it for free.
///
/// `scope` controls which subset is walked, mirroring what each
/// existing caller walks today. See [`NonResourceScope`] for the two
/// shapes.
fn for_each_non_resource_value<E, F>(
    parsed: &crate::parser::File<E>,
    scope: NonResourceScope,
    mut f: F,
) where
    E: crate::parser::ExportParamLike,
    F: FnMut(&Value, &str),
{
    if matches!(scope, NonResourceScope::All) {
        for (name, value) in parsed.variables.iter() {
            f(value, &format!("let {}", name));
        }
        for attr in &parsed.attribute_params {
            if let Some(value) = &attr.value {
                f(value, &format!("attributes.{}", attr.name));
            }
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value() {
            f(value, &format!("exports.{}", export.name()));
        }
    }
    for call in &parsed.module_calls {
        let caller = call.binding_name.as_deref().unwrap_or(&call.module_name);
        for (arg_name, value) in call.arguments.iter() {
            f(
                value,
                &format!("module `{}` argument `{}`", caller, arg_name),
            );
        }
    }
}

/// Which non-resource scopes a check walks today. The variants are not
/// a generic taxonomy — they pin the existing per-caller asymmetry so
/// the refactor stays behavior-preserving. Whether
/// [`check_upstream_state_attribute_access_shapes`] *should* widen to
/// `All` (i.e. also walk `let`/attribute_params, like field-references
/// does) is an open question for a follow-up; until then,
/// `ExportsAndModules` exists to preserve the historical reach.
#[derive(Debug, Clone, Copy)]
enum NonResourceScope {
    /// Variables (`let`), attribute_params, export_params, module_calls
    /// — what `check_upstream_state_field_references` walks today.
    All,
    /// Just export_params and module_calls — what
    /// `check_upstream_state_attribute_access_shapes` walks today.
    ExportsAndModules,
}

/// Walk a parsed project and return an error for every reference whose root
/// binding is in `exports` but whose field isn't in its declared key set.
/// Also covers deferred for-iterables (e.g. `for _ in orgs.accounts`), which
/// parse into `deferred_for_expressions` rather than `Value::ResourceRef`.
///
/// Bindings absent from `exports` are skipped — the caller decides what to
/// do about unresolved upstreams.
pub fn check_upstream_state_field_references<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    exports: &UpstreamExports,
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
        let mut check = |value: &Value, location: &str| {
            value.visit_refs(&mut |path| {
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

        // Direct resources and deferred for-body template resources share
        // one walk via `iter_all_resources` (helper). Location strings
        // use the `ResourceContext::Deferred` branch to mention the for
        // header so users can tell body errors from top-level ones.
        for_each_resource_attr(parsed, |ctx, resource, attr_name, value| {
            check(value, &resource_attr_location(ctx, resource, attr_name));
        });
        for_each_non_resource_value(parsed, NonResourceScope::All, |value, location| {
            check(value, location);
        });
    }

    // Deferred for-expression iterables are a direct
    // (binding, attribute) pair rather than a Value tree, so check them
    // after the closure block has released its borrow on `errors`.
    for deferred in &parsed.deferred_for_expressions {
        let Some(keys) = exports.get(deferred.iterable_binding.as_str()) else {
            continue;
        };
        if keys.contains_key(&deferred.iterable_attr) {
            continue;
        }
        let known = known_by_binding
            .get(deferred.iterable_binding.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        errors.push(UpstreamFieldError {
            location: format!("for-expression `{}`", deferred.header),
            binding: deferred.iterable_binding.clone(),
            field: deferred.iterable_attr.clone(),
            suggestion: suggest_similar_name(&deferred.iterable_attr, known),
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

/// A reference to an `upstream_state` export whose declared type is
/// incompatible with the consumer's expected type.
///
/// Complements `UpstreamFieldError`: this one fires when the *name* is
/// valid but the *type* isn't. Types are kept structured so future code
/// actions (e.g. wrap in a cast, jump to definition) can inspect them.
#[derive(Debug, Clone)]
pub struct UpstreamTypeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    pub export_type: TypeExpr,
    pub expected_type: AttributeType,
}

impl UpstreamTypeError {
    pub fn diagnostic_message(&self) -> String {
        format!(
            "upstream_state `{}.{}` is declared as `{}` but this position expects `{}`",
            self.binding,
            self.field,
            self.export_type,
            self.expected_type.type_name()
        )
    }
}

impl std::fmt::Display for UpstreamTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamTypeError {}

impl UpstreamRefDiagnostic for UpstreamTypeError {
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
        UpstreamTypeError::diagnostic_message(self)
    }
}

/// For each resource attribute whose value is an `upstream_state` field
/// reference, compare the export's declared type against the attribute's
/// expected type and emit an error when they don't fit.
///
/// Exports without a declared type (no `: T` annotation) are skipped —
/// there's nothing to compare.
pub fn check_upstream_state_field_types<E>(
    parsed: &crate::parser::File<E>,
    exports: &UpstreamExports,
    registry: &SchemaRegistry,
) -> Vec<UpstreamTypeError> {
    let mut errors: Vec<UpstreamTypeError> = Vec::new();
    for_each_resource_attr(parsed, |ctx, resource, attr_name, value| {
        let Some(schema) = registry.get_for(resource) else {
            return;
        };
        let Some(attr_schema) = schema.attributes.get(attr_name) else {
            return;
        };
        let location = resource_attr_location(ctx, resource, attr_name);
        check_ref_against_type(
            value,
            &attr_schema.attr_type,
            exports,
            &location,
            &mut errors,
        );
    });
    errors.sort_by(|a, b| {
        (a.location.as_str(), a.binding.as_str(), a.field.as_str()).cmp(&(
            b.location.as_str(),
            b.binding.as_str(),
            b.field.as_str(),
        ))
    });
    errors
}

fn check_ref_against_type(
    value: &Value,
    expected: &AttributeType,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamTypeError>,
) {
    walk_value_against_type(value, expected, exports, location, errors);
}

/// Positional walker: descend `value` and `expected` in lockstep,
/// threading the inner expected type to each leaf `Value::ResourceRef`
/// so the comparison fires against the *position's* schema type rather
/// than the outer attribute's. Without this, a ref deep inside a
/// struct field or interpolation gets compared to the outer attr type
/// and either false-flags or silently passes (the
/// `is_scalar_type_expr` short-circuit before #2475).
///
/// Assumes the receiver `AttributeType` exposes its container shape
/// directly: a `Map`/`List`/`Struct` receiver is matched as such.
/// `Custom { base: <Container> }` is not unwrapped here — provider
/// schemas keep `Custom` over scalar bases only, so the catch-all
/// best-effort walk is sufficient. If a future schema introduces a
/// `Custom { base: Map(_) }` shape, add an explicit `Custom` arm that
/// recurses on `base`.
fn walk_value_against_type(
    value: &Value,
    expected: &AttributeType,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamTypeError>,
) {
    match value {
        Value::ResourceRef { path } => {
            check_resource_ref_at_position(path, expected, exports, location, errors);
        }
        Value::List(items) => {
            // Descend `list(T)` element-wise. For non-list receivers
            // the existing top-level shape check (run elsewhere)
            // already flags the kind mismatch, so we just walk each
            // element against the same expected type — the leaf-ref
            // comparison will fire if the element doesn't fit.
            let inner = match expected {
                AttributeType::List { inner, .. } => inner.as_ref(),
                _ => expected,
            };
            for item in items {
                walk_value_against_type(item, inner, exports, location, errors);
            }
        }
        Value::Map(entries) => match expected {
            AttributeType::Map { value: inner, .. } => {
                for v in entries.values() {
                    walk_value_against_type(v, inner, exports, location, errors);
                }
            }
            AttributeType::Struct { fields, .. } => {
                // Resolve via `build_accepted_field_map` so `block_name`
                // aliases (`field { ... }` block syntax) reach the
                // same field as the canonical name. Without this a
                // ref written under the alias key would silently skip
                // the type check.
                let accepted = crate::schema::build_accepted_field_map(fields);
                for (key, v) in entries {
                    if let Some(field) = accepted.get(key.as_str()) {
                        walk_value_against_type(v, &field.field_type, exports, location, errors);
                    }
                    // Unknown struct fields are flagged by the schema
                    // validator; don't double-report here.
                }
            }
            _ => {
                // Receiver isn't a `Map`/`Struct` — Union receivers in
                // particular land here. We don't try each Union member
                // (would require committing to a best-fit member, which
                // is the schema validator's job at the top level);
                // walking each value against the whole Union still lets
                // leaf-ref checks dispatch via
                // `is_type_expr_compatible_with_schema`'s Union arm.
                for v in entries.values() {
                    walk_value_against_type(v, expected, exports, location, errors);
                }
            }
        },
        Value::Interpolation(parts) => {
            // The result of an interpolation is always a string, so
            // every embedded `Expr` part sits in a String position
            // regardless of where the interpolation itself appears.
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(v) = part {
                    walk_value_against_type(v, &AttributeType::String, exports, location, errors);
                }
            }
        }
        Value::Secret(inner) => {
            walk_value_against_type(inner, expected, exports, location, errors);
        }
        Value::FunctionCall { args, .. } => {
            // Function arguments occupy function-internal positions
            // whose declared types live on the function definition
            // (out of reach here). Walk with `expected` as a
            // best-effort so leaf refs get *some* check; precision
            // would require typed function signatures.
            for arg in args {
                walk_value_against_type(arg, expected, exports, location, errors);
            }
        }
        Value::String(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::StringList(_)
        | Value::Unknown(_) => {}
    }
}

/// Compare a single `ResourceRef` against the receiver type at its
/// position. Narrows the export type through the access path's
/// `field_path` / `subscripts` first (a positional walker by itself is
/// not enough — `accounts['k']` still has to step through `map(T) → T`
/// before comparing).
fn check_resource_ref_at_position(
    path: &crate::resource::AccessPath,
    expected: &AttributeType,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamTypeError>,
) {
    let binding = path.binding();
    let field = path.attribute();
    let Some(keys) = exports.get(binding) else {
        return;
    };
    let Some(UpstreamExportEntry {
        type_expr: Some(export_type),
        ..
    }) = keys.get(field)
    else {
        return;
    };
    let Some(narrowed) = narrow_type_expr(export_type, path.field_path(), path.subscripts()) else {
        // Kind mismatch through the access path — already reported by
        // `check_upstream_state_attribute_access_shapes` /
        // `check_upstream_state_subscript_shapes`. Skip to avoid
        // double-reporting.
        return;
    };
    if crate::validation::is_type_expr_compatible_with_schema(&narrowed, expected) {
        return;
    }
    // Narrowed-string-shape vs string-receiver: a `Simple("aws_account_id")`
    // (or any other string-compatible scalar identity) can sit in any
    // string-compatible position. The strict compat check rejects this
    // direction (it walks the receiver's `Custom` chain looking for the
    // exact identity; `String` carries no chain), but the runtime
    // value is a string, so the position is satisfiable. The reverse
    // direction (`String → Custom{Specific}`) stays strict via
    // `attr_type_demands_specific_custom`. #2475.
    if narrowed.is_string_shaped() && crate::validation::is_string_compatible_type(expected) {
        return;
    }
    errors.push(UpstreamTypeError {
        location: location.to_string(),
        binding: binding.to_string(),
        field: field.to_string(),
        export_type: narrowed,
        expected_type: expected.clone(),
    });
}

use crate::validation::{narrow_type_expr, walk_type_expr_path};

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
        let suggested = match (&self.export_type, self.binding_kind) {
            (TypeExpr::List(_), ForIterableBindingKind::Map) => {
                "; use `for x in ...` to iterate the list"
            }
            (TypeExpr::Map(_), ForIterableBindingKind::List) => {
                "; use `for k, v in ...` to iterate the map"
            }
            // Scalar exports can't be iterated at all; saying nothing is
            // honest. The upstream contract has to change first.
            _ => "",
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
pub fn check_upstream_state_for_iterable_shapes<E>(
    parsed: &crate::parser::File<E>,
    exports: &UpstreamExports,
) -> Vec<UpstreamForIterableShapeError> {
    let mut errors: Vec<UpstreamForIterableShapeError> = Vec::new();
    for deferred in &parsed.deferred_for_expressions {
        let Some(fields) = exports.get(deferred.iterable_binding.as_str()) else {
            continue;
        };
        let Some(UpstreamExportEntry {
            type_expr: Some(export_type),
            ..
        }) = fields.get(&deferred.iterable_attr)
        else {
            continue;
        };
        let binding_kind = ForIterableBindingKind::from_for_binding(&deferred.binding);
        let export_kind = match export_type {
            TypeExpr::List(_) => Some(ForIterableBindingKind::List),
            TypeExpr::Map(_) => Some(ForIterableBindingKind::Map),
            _ => None,
        };
        // Scalars feeding a `for` are a different kind of bug; the
        // shape-check fires on them too because no kind matches.
        if export_kind == Some(binding_kind) {
            continue;
        }
        errors.push(UpstreamForIterableShapeError {
            location: format!("for-expression `{}`", deferred.header),
            binding: deferred.iterable_binding.clone(),
            field: deferred.iterable_attr.clone(),
            export_type: export_type.clone(),
            binding_kind,
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
            other => format!(
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

/// Walk every `Value::ResourceRef` whose root is an `upstream_state`
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
///   already handled by `check_upstream_state_field_types`.
///
/// Sibling of [`check_upstream_state_for_iterable_shapes`] from #2317;
/// both surface "downstream usage doesn't fit upstream's declared
/// shape" before apply.
pub fn check_upstream_state_attribute_access_shapes<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    exports: &UpstreamExports,
) -> Vec<UpstreamAttributeAccessShapeError> {
    let mut errors: Vec<UpstreamAttributeAccessShapeError> = Vec::new();
    for_each_resource_attr(parsed, |ctx, resource, attr_name, value| {
        visit_attribute_access(
            value,
            exports,
            &resource_attr_location(ctx, resource, attr_name),
            &mut errors,
        );
    });
    // Module-call arguments and export values are ResourceRef-bearing
    // too — they aren't iterated by `iter_all_resources`. Walking them
    // (via the helper) matches the reach of
    // `check_upstream_state_field_references` for those scopes.
    for_each_non_resource_value(
        parsed,
        NonResourceScope::ExportsAndModules,
        |value, location| {
            visit_attribute_access(value, exports, location, &mut errors);
        },
    );
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
fn visit_attribute_access(
    value: &Value,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamAttributeAccessShapeError>,
) {
    value.visit_refs(&mut |path| {
        let field_path = path.field_path();
        if field_path.is_empty() {
            return;
        }
        let binding = path.binding();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(UpstreamExportEntry {
            type_expr: Some(export_type),
            ..
        }) = fields.get(attribute)
        else {
            return;
        };
        if let Err((mismatched_at, bad_segment)) = walk_type_expr_path(export_type, field_path) {
            errors.push(UpstreamAttributeAccessShapeError {
                location: location.to_string(),
                binding: binding.to_string(),
                field: attribute.to_string(),
                field_path: field_path.to_vec(),
                mismatched_at: mismatched_at.clone(),
                bad_segment: bad_segment.to_string(),
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
        let suggestion = match (&self.mismatched_at, &self.bad_subscript) {
            (TypeExpr::List(_), Subscript::Str { .. }) => {
                "; integer subscript `[i]` reads list elements"
            }
            (TypeExpr::Map(_), Subscript::Int { .. }) => {
                "; string subscript `[\"k\"]` reads map entries"
            }
            // Struct/scalar don't host any subscript form, so the
            // "use the other syntax" hint would be misleading. Stay
            // silent and let the caller show the declared type.
            _ => "",
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

/// Walk every `Value::ResourceRef` whose root is an `upstream_state`
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
pub fn check_upstream_state_subscript_shapes<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    exports: &UpstreamExports,
) -> Vec<UpstreamSubscriptShapeError> {
    let mut errors: Vec<UpstreamSubscriptShapeError> = Vec::new();
    for_each_resource_attr(parsed, |ctx, resource, attr_name, value| {
        visit_subscript_access(
            value,
            exports,
            &resource_attr_location(ctx, resource, attr_name),
            &mut errors,
        );
    });
    for_each_non_resource_value(
        parsed,
        NonResourceScope::ExportsAndModules,
        |value, location| {
            visit_subscript_access(value, exports, location, &mut errors);
        },
    );
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
fn visit_subscript_access(
    value: &Value,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamSubscriptShapeError>,
) {
    value.visit_refs(&mut |path| {
        let subscripts = path.subscripts();
        if subscripts.is_empty() {
            return;
        }
        let binding = path.binding();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(UpstreamExportEntry {
            type_expr: Some(export_type),
            ..
        }) = fields.get(attribute)
        else {
            return;
        };
        // If the field path itself doesn't resolve, the
        // attribute-access check will already report it. Skip here so
        // we don't double-fire.
        let field_path = path.field_path();
        let Ok(at_field_end) = walk_type_expr_path(export_type, field_path) else {
            return;
        };
        // Step through each subscript, descending into the element
        // type when it fits and emitting at the first kind mismatch.
        let mut current = at_field_end;
        for sub in subscripts {
            match (current, sub) {
                (TypeExpr::List(inner), Subscript::Int { .. }) => {
                    current = inner.as_ref();
                }
                (TypeExpr::Map(inner), Subscript::Str { .. }) => {
                    current = inner.as_ref();
                }
                (mismatched_at, bad) => {
                    errors.push(UpstreamSubscriptShapeError {
                        location: location.to_string(),
                        binding: binding.to_string(),
                        field: attribute.to_string(),
                        field_path: field_path.to_vec(),
                        mismatched_at: mismatched_at.clone(),
                        bad_subscript: bad.clone(),
                    });
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParsedFile;
    use std::fs;
    use std::path::PathBuf;

    fn write_crn(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn ctx() -> ProviderContext {
        ProviderContext::default()
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: binding.to_string(),
            source: PathBuf::from(source),
        }
    }

    fn parse_project(source: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.crn"), source).unwrap();
        parse_directory(tmp.path(), &ctx()).expect("parse_directory")
    }

    fn mk_exports(pairs: &[(&str, &[&str])]) -> UpstreamExports {
        pairs
            .iter()
            .map(|(binding, keys)| {
                (
                    binding.to_string(),
                    keys.iter()
                        .map(|s| {
                            (
                                s.to_string(),
                                UpstreamExportEntry {
                                    type_expr: None,
                                    value: None,
                                },
                            )
                        })
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
        let ty = entry.type_expr.as_ref().expect("type annotation present");
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
        use crate::resource::Value;

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
            entry.type_expr.is_some(),
            "type annotation must still be present"
        );
        let value = entry.value.as_ref().expect("export value must be carried");
        let Value::Map(entries) = value else {
            panic!("expected Value::Map for map literal export, got {value:?}");
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
        // A readable upstream whose .crn fails to parse must surface as a
        // resolve error — otherwise downstream typos get silently masked.
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
        assert!(errs[0].to_string().contains("orgs"));
        assert!(errs[0].to_string().contains("organizations"));
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
        assert!(check_upstream_state_field_references(&parsed, &exports).is_empty());
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
    // Phase 2 of #1992: type compatibility (`check_upstream_state_field_types`)
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
                        .map(|(name, ty)| {
                            (
                                name.to_string(),
                                UpstreamExportEntry {
                                    type_expr: Some(ty.clone()),
                                    value: None,
                                },
                            )
                        })
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
        let schema =
            ResourceSchema::new("r.res").attribute(AttributeSchema::new(attr_name, attr_type));
        let mut registry = SchemaRegistry::new();
        registry.insert("test", schema);
        registry
    }

    fn parse_project_with_provider(source: &str, provider_name: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        let full = format!(
            "provider {} {{\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}}\n{}",
            provider_name, source
        );
        fs::write(tmp.path().join("main.crn"), full).unwrap();
        parse_directory(tmp.path(), &ctx()).expect("parse_directory")
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
        assert_eq!(errs[0].binding, "orgs");
        assert_eq!(errs[0].field, "count");
        assert!(matches!(errs[0].export_type, TypeExpr::Int));
        assert!(matches!(
            errs[0].expected_type,
            crate::schema::AttributeType::String
        ));
        assert!(errs[0].diagnostic_message().contains("String"));
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        fields.insert(
            "count".to_string(),
            UpstreamExportEntry {
                type_expr: None,
                value: None,
            },
        );
        exports.insert("orgs".to_string(), fields);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn type_check_skips_unknown_field() {
        // Field isn't in the export set at all — that's the field-name
        // checker's job (#1990). The type checker must not double-report.
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
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
            crate::schema::AttributeType::list(crate::schema::AttributeType::String),
        );
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "expected one error inside for body, got {errs:?}"
        );
        assert!(errs[0].location.contains("for"));
    }

    #[test]
    fn type_check_accepts_custom_type_chain() {
        // Consumer attribute is `Custom { name: "KmsKeyArn", base: Arn }`;
        // export declares plain `TypeExpr::Simple("arn")`. The type checker
        // walks Custom's base chain, so `arn` accepts `KmsKeyArn`.
        use crate::schema::{AttributeType, noop_validator};
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.key_arn
                }
            "#,
            "test",
        );
        let exports =
            mk_typed_exports(&[("orgs", &[("key_arn", TypeExpr::Simple("arn".to_string()))])]);
        let kms_arn = AttributeType::Custom {
            semantic_name: Some("KmsKeyArn".to_string()),
            base: Box::new(AttributeType::Custom {
                semantic_name: Some("Arn".to_string()),
                base: Box::new(AttributeType::String),
                pattern: None,
                length: None,
                validate: noop_validator(),
                namespace: None,
                to_dsl: None,
            }),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        };
        let schemas = schema_with_attr("name", kms_arn);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "Custom type chain must accept base ancestor, got: {errs:?}"
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
        use crate::schema::{AttributeType, noop_validator};
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
        let aws_account_id = AttributeType::Custom {
            semantic_name: Some("AwsAccountId".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: noop_validator(),
            namespace: None,
            to_dsl: None,
        };
        let schemas = schema_with_attr("name", aws_account_id);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let policy_struct = AttributeType::Struct {
            name: "IamPolicyDocument".to_string(),
            fields: vec![StructField::new("statement", AttributeType::String).required()],
        };
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        // `build_accepted_field_map`, which honours `block_name`
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
        let policy_struct = AttributeType::Struct {
            name: "PolicyDoc".to_string(),
            fields: vec![
                StructField::new("statements", AttributeType::String)
                    .required()
                    .with_block_name("statement"),
            ],
        };
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let cfg_struct = AttributeType::Struct {
            name: "Cfg".to_string(),
            fields: vec![StructField::new("port", AttributeType::Int).required()],
        };
        let schemas = schema_with_attr("config", cfg_struct);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let policy_struct = AttributeType::Struct {
            name: "PolicyDoc".to_string(),
            fields: vec![StructField::new("statement", AttributeType::String).required()],
        };
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let policy_struct = AttributeType::Struct {
            name: "IamPolicyDocument".to_string(),
            fields: vec![StructField::new("statement", AttributeType::String).required()],
        };
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        let schemas = schema_with_attr("tags", AttributeType::map(AttributeType::String));
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas);
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
        fields.insert(
            "accounts".to_string(),
            UpstreamExportEntry {
                type_expr: None,
                value: None,
            },
        );
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
        fields.insert(
            "account".to_string(),
            UpstreamExportEntry {
                type_expr: None,
                value: None,
            },
        );
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
        // ref, handled by `check_upstream_state_field_types`.
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
        fields.insert(
            "accounts".to_string(),
            UpstreamExportEntry {
                type_expr: None,
                value: None,
            },
        );
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
    fn upstream_ref_diagnostic_trait_covers_all_five_error_types() {
        // Building a `Vec<&dyn UpstreamRefDiagnostic>` proves each type
        // implements the trait and that the LSP/CLI can iterate them
        // uniformly. The CLI/LSP wirings rely on this being possible.
        let field_err = UpstreamFieldError {
            location: "loc-a".to_string(),
            binding: "orgs".to_string(),
            field: "missing".to_string(),
            suggestion: Some("accounts".to_string()),
        };
        let type_err = UpstreamTypeError {
            location: "loc-b".to_string(),
            binding: "orgs".to_string(),
            field: "count".to_string(),
            export_type: TypeExpr::Int,
            expected_type: crate::schema::AttributeType::String,
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
                &type_err,
                "loc-b",
                "orgs",
                "count",
                type_err.diagnostic_message(),
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
        let region_type = exports
            .get("ups")
            .and_then(|m| m.get("region"))
            .and_then(|e| e.type_expr.as_ref())
            .expect("region should be inferred");
        assert_eq!(region_type, &TypeExpr::String);
    }
}
