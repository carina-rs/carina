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
use crate::parser::{ParsedFile, ProviderContext, ResourceContext, TypeExpr, UpstreamState};
use crate::resource::Value;
use crate::schema::{AttributeType, ResourceSchema, suggest_similar_name};

/// Exports declared by each `upstream_state` binding: binding name →
/// (export name → declared type, or `None` if the export has no annotation).
///
/// Phase 1 (#1990) only needed the key set; Phase 2 (#1992) adds the type
/// side so downstream consumers whose expected type is known can be checked
/// for shape compatibility.
pub type UpstreamExports = HashMap<String, HashMap<String, Option<TypeExpr>>>;

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
    /// (e.g. `"aws.s3.bucket.main attribute `name`"`).
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
                let keys: HashMap<String, Option<TypeExpr>> = parsed
                    .export_params
                    .iter()
                    .map(|e| (e.name.clone(), e.type_expr.clone()))
                    .collect();
                out.insert(us.binding.clone(), keys);
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

/// Walk a parsed project and return an error for every reference whose root
/// binding is in `exports` but whose field isn't in its declared key set.
/// Also covers deferred for-iterables (e.g. `for _ in orgs.accounts`), which
/// parse into `deferred_for_expressions` rather than `Value::ResourceRef`.
///
/// Bindings absent from `exports` are skipped — the caller decides what to
/// do about unresolved upstreams.
pub fn check_upstream_state_field_references(
    parsed: &ParsedFile,
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

        for (name, value) in parsed.variables.iter() {
            check(value, &format!("let {}", name));
        }
        for resource in &parsed.resources {
            for (attr_name, expr) in resource.attributes.iter() {
                check(
                    expr.as_value(),
                    &format!("{} attribute `{}`", resource.id, attr_name),
                );
            }
        }
        for attr in &parsed.attribute_params {
            if let Some(value) = &attr.value {
                check(value, &format!("attributes.{}", attr.name));
            }
        }
        for export in &parsed.export_params {
            if let Some(value) = &export.value {
                check(value, &format!("exports.{}", export.name));
            }
        }
        for call in &parsed.module_calls {
            let caller = call.binding_name.as_deref().unwrap_or(&call.module_name);
            for (arg_name, v) in call.arguments.iter() {
                check(v, &format!("module `{}` argument `{}`", caller, arg_name));
            }
        }
        // Deferred for-expression bodies: the body is parked on the
        // deferred expression until plan-time expansion and isn't reached
        // by the `resources` walk above.
        for deferred in &parsed.deferred_for_expressions {
            for (attr_name, expr) in deferred.template_resource.attributes.iter() {
                check(
                    expr.as_value(),
                    &format!(
                        "for-body `{}` {} attribute `{}`",
                        deferred.header, deferred.template_resource.id, attr_name
                    ),
                );
            }
            for (attr_name, value) in &deferred.attributes {
                check(
                    value,
                    &format!("for-body `{}` attribute `{}`", deferred.header, attr_name),
                );
            }
        }
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

/// For each resource attribute whose value is an `upstream_state` field
/// reference, compare the export's declared type against the attribute's
/// expected type and emit an error when they don't fit.
///
/// Exports without a declared type (no `: T` annotation) are skipped —
/// there's nothing to compare.
pub fn check_upstream_state_field_types(
    parsed: &ParsedFile,
    exports: &UpstreamExports,
    schemas: &HashMap<String, ResourceSchema>,
    schema_key_fn: &dyn Fn(&crate::resource::Resource) -> String,
) -> Vec<UpstreamTypeError> {
    let mut errors: Vec<UpstreamTypeError> = Vec::new();
    for (ctx, resource) in parsed.iter_all_resources() {
        let key = schema_key_fn(resource);
        let Some(schema) = schemas.get(&key) else {
            continue;
        };
        for (attr_name, expr) in resource.attributes.iter() {
            if attr_name.starts_with('_') {
                continue;
            }
            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                continue;
            };
            let location = match ctx {
                ResourceContext::Direct => format!("{} attribute `{}`", resource.id, attr_name),
                ResourceContext::Deferred(d) => format!(
                    "for-body `{}` {} attribute `{}`",
                    d.header, resource.id, attr_name
                ),
            };
            check_ref_against_type(
                expr.as_value(),
                &attr_schema.attr_type,
                exports,
                &location,
                &mut errors,
            );
        }
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

fn check_ref_against_type(
    value: &Value,
    expected: &AttributeType,
    exports: &UpstreamExports,
    location: &str,
    errors: &mut Vec<UpstreamTypeError>,
) {
    value.visit_refs(&mut |path| {
        let binding = path.binding();
        let field = path.attribute();
        let Some(keys) = exports.get(binding) else {
            return;
        };
        let Some(Some(export_type)) = keys.get(field) else {
            // Either the field isn't in the export set (already reported
            // by `check_upstream_state_field_references`) or it has no
            // declared type — nothing to type-check against.
            return;
        };
        if crate::validation::is_type_expr_compatible_with_schema(export_type, expected) {
            return;
        }
        errors.push(UpstreamTypeError {
            location: location.to_string(),
            binding: binding.to_string(),
            field: field.to_string(),
            export_type: export_type.clone(),
            expected_type: expected.clone(),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    keys.iter().map(|s| (s.to_string(), None)).collect(),
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
                accounts: string = "x"
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
                accounts: string = "x"
                region: string = "ap-northeast-1"
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
                accounts: string = "x"
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
                accounts: string = "x"
            }"#,
        );
        write_crn(
            &upstream_dir,
            "region.crn",
            r#"exports {
                region: string = "ap-northeast-1"
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
                bad: string = orgs.account
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
                good: string = orgs.accounts
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
                x: string = orgs.anything
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
                x: string = other.foo
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
                awscc.ec2.vpc {
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
    fn check_rejects_unexported_field_in_for_expression_iterable() {
        // `for name, _ in orgs.account` — issue #1990's canonical repro.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for name, _ in orgs.account {
                awscc.ec2.vpc {
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
                        .map(|(name, ty)| (name.to_string(), Some(ty.clone())))
                        .collect(),
                )
            })
            .collect()
    }

    /// Minimal schema registry for test resources: `test.r.res` with a single
    /// typed attribute.
    fn schema_with_attr(
        attr_name: &str,
        attr_type: crate::schema::AttributeType,
    ) -> HashMap<String, crate::schema::ResourceSchema> {
        use crate::schema::{AttributeSchema, ResourceSchema};
        let schema =
            ResourceSchema::new("test.r.res").attribute(AttributeSchema::new(attr_name, attr_type));
        let mut map = HashMap::new();
        map.insert("test.r.res".to_string(), schema);
        map
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
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        fields.insert("count".to_string(), None);
        exports.insert("orgs".to_string(), fields);
        let schemas = schema_with_attr("name", crate::schema::AttributeType::String);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
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
        use crate::schema::AttributeType;
        fn noop_validate(_v: &crate::resource::Value) -> Result<(), String> {
            Ok(())
        }
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
            name: "KmsKeyArn".to_string(),
            base: Box::new(AttributeType::Custom {
                name: "Arn".to_string(),
                base: Box::new(AttributeType::String),
                validate: noop_validate,
                namespace: None,
                to_dsl: None,
            }),
            validate: noop_validate,
            namespace: None,
            to_dsl: None,
        };
        let schemas = schema_with_attr("name", kms_arn);
        let errs = check_upstream_state_field_types(&parsed, &exports, &schemas, &|r| {
            format!("{}.{}", r.id.provider, r.id.resource_type)
        });
        assert!(
            errs.is_empty(),
            "Custom type chain must accept base ancestor, got: {errs:?}"
        );
    }
}
