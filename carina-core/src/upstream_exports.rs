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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config_loader::{find_crn_files_in_dir, parse_directory};
use crate::parser::{ParsedFile, ProviderContext, UpstreamState};
use crate::resource::Value;
use crate::schema::suggest_similar_name;

pub type UpstreamExports = HashMap<String, HashSet<String>>;

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
            out.insert(us.binding.clone(), HashSet::new());
            continue;
        }
        match parse_directory(&source_abs, config) {
            Ok(parsed) => {
                let keys: HashSet<String> = parsed
                    .export_params
                    .iter()
                    .map(|e| e.name.clone())
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
        .map(|(b, keys)| (b.as_str(), keys.iter().map(String::as_str).collect()))
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
                if keys.contains(field) {
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
        if keys.contains(&deferred.iterable_attr) {
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
                    keys.iter().map(|s| s.to_string()).collect(),
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
        assert!(got.get("orgs").unwrap().contains("accounts"));

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
        assert!(keys.contains("accounts"));
        assert!(keys.contains("region"));
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
        assert!(keys.contains("accounts"));
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
            keys.contains("accounts"),
            "export from accounts.crn must be merged, got {:?}",
            keys
        );
        assert!(
            keys.contains("region"),
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
}
