//! Integration tests for carina#3182 — `upstream_state` references
//! inside `provider { ... }` configuration.
//!
//! Before the fix, the planner rejected
//! `provider aws { assume_role = { role_arn = upstream.arn } }` with
//! `cannot serialize at WASM provider boundary: unresolved reference
//! <binding>.<attr>` because `provider.attributes` was never substituted
//! with the upstream's loaded values. The fix runs
//! [`carina_core::parser::resolve_provider_attributes_with_remote`] right
//! after `load_upstream_states` (plan/apply) and skips serializing
//! still-deferred provider attributes at validate time.
//!
//! These tests follow the directory-scoped rule: each fixture is a
//! `tempfile::tempdir()` mirroring the real `infra/aws/management/...`
//! shape, with the downstream's configuration split across `backend.crn`,
//! `upstream.crn`, `providers.crn`, and `main.crn` so the merged-file
//! resolver path is exercised end-to-end.

use std::fs;
use std::path::Path;

use carina_core::config_loader::load_configuration;
use carina_core::parser::{ProviderContext, resolve_provider_attributes_with_remote};
use carina_core::resource::{ConcreteValue, DeferredValue, Value};

/// Write a minimal upstream directory that exports `delegation_writer_role_arn`.
fn write_upstream(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("main.crn"),
        r#"backend local { path = "carina.state.json" }
exports {
  delegation_writer_role_arn: String = "arn:aws:iam::412038850359:role/carina-route53-delegation-writer"
}
"#,
    )
    .unwrap();
}

/// Write a downstream directory whose `providers.crn` consumes the
/// upstream value through a provider-level `assume_role`. Splits the
/// configuration across multiple `.crn` files (the directory-scoped
/// convention) so the test exercises the merged-file resolver path.
fn write_downstream(dir: &Path, upstream_rel: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("backend.crn"),
        r#"backend local { path = "carina.state.json" }
"#,
    )
    .unwrap();
    fs::write(
        dir.join("upstream.crn"),
        format!(
            r#"let mgmt = upstream_state {{ source = "{upstream_rel}" }}
"#
        ),
    )
    .unwrap();
    fs::write(
        dir.join("providers.crn"),
        r#"let management = provider aws {
  region = "ap-northeast-1"
  assume_role = {
    role_arn = mgmt.delegation_writer_role_arn
    session_name = "carina-test"
  }
}
"#,
    )
    .unwrap();
    // A no-op resource file keeps the shape close to real infra layouts.
    fs::write(
        dir.join("main.crn"),
        r#"exports { region: String = "ap-northeast-1" }
"#,
    )
    .unwrap();
}

#[test]
fn provider_attributes_resolve_upstream_state_refs_post_load() {
    // The acceptance case from the issue body: the downstream uses the
    // upstream's exported role ARN inside `provider aws { assume_role }`.
    // After parse, the ref is still deferred; after
    // `resolve_provider_attributes_with_remote` with the upstream's loaded
    // bindings it must be a concrete string ready for WASM serialization.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("mgmt");
    let downstream = tmp.path().join("env");
    write_upstream(&upstream);
    write_downstream(&downstream, "../mgmt");

    let mut config = load_configuration(&downstream).expect("load_configuration");

    // Sanity: pre-resolve, the ref survives nested inside `assume_role`.
    let pc_pre = config
        .parsed
        .providers
        .iter()
        .find(|p| p.name == "aws")
        .expect("aws provider present");
    let assume_pre = pc_pre.attributes.get("assume_role").unwrap();
    match assume_pre {
        Value::Concrete(ConcreteValue::Map(m)) => match m.get("role_arn").unwrap() {
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                assert_eq!(path.binding(), "mgmt");
            }
            other => panic!("expected deferred role_arn pre-resolve, got: {other:?}"),
        },
        other => panic!("expected Map for assume_role, got: {other:?}"),
    }

    // Simulate `load_upstream_states`'s output (binding → attr → value).
    let mut mgmt_attrs = std::collections::HashMap::new();
    mgmt_attrs.insert(
        "delegation_writer_role_arn".to_string(),
        Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::412038850359:role/carina-route53-delegation-writer".to_string(),
        )),
    );
    let mut remote = std::collections::HashMap::new();
    remote.insert("mgmt".to_string(), mgmt_attrs);

    resolve_provider_attributes_with_remote(
        &mut config.parsed,
        &remote,
        &ProviderContext::default(),
    )
    .expect("resolve must succeed");

    let pc = config
        .parsed
        .providers
        .iter()
        .find(|p| p.name == "aws")
        .expect("aws provider present");
    let assume = pc.attributes.get("assume_role").unwrap();
    let Value::Concrete(ConcreteValue::Map(m)) = assume else {
        panic!("expected Map for assume_role post-resolve, got: {assume:?}");
    };
    assert_eq!(
        m.get("role_arn"),
        Some(&Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::412038850359:role/carina-route53-delegation-writer".to_string()
        ))),
        "role_arn must be substituted from the upstream binding; got: {m:?}",
    );
    assert_eq!(
        m.get("session_name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "carina-test".to_string()
        ))),
        "literal sibling must be preserved post-resolve",
    );
}
