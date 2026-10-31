//! Integration test for shared provider attributes (#2717).
//!
//! Reproduces the CLI pipeline (parse → load → resolve → module-expand →
//! finalize) against a multi-file fixture that sources `default_tags`
//! from a sibling module via `use` + module-call + field access.
//! Asserts the resolved tag map matches what would be produced by a
//! literal `default_tags = { ... }` map.

use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::resource::{ConcreteValue, Value};

#[test]
fn share_provider_attrs_resolves_default_tags() {
    let fixture: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "share_provider_attrs",
        "component",
    ]
    .iter()
    .collect();

    let config = load_configuration(&fixture).expect("load_configuration");
    let user_root = tempfile::tempdir().expect("create isolated user directory");
    let user_dirs = carina_core::UserDirs::under(user_root.path());

    // Run the CLI's full validate-and-resolve pipeline so module
    // expansion + finalize happen. The test-support boundary still exercises
    // the real command-scoped load; this source-less mock configuration uses
    // the built-in provider catalog.
    let parsed = carina_cli::commands::validate_and_resolve_with_provider_load_for_test(
        &user_dirs,
        config.parsed,
        &fixture,
        true,
        &config.inference_errors,
        &config.duplicate_declarations,
    )
    .expect("validate_and_resolve must succeed");

    let provider = parsed
        .providers
        .iter()
        .find(|p| p.name == "mock")
        .expect("provider mock present");

    assert!(
        provider.unresolved_attributes.is_empty(),
        "finalize must drain unresolved_attributes; got: {:?}",
        provider.unresolved_attributes,
    );

    let tags = &provider.default_tags;
    assert_eq!(
        tags.get("ManagedBy"),
        Some(&Value::Concrete(ConcreteValue::String("carina".into()))),
    );
    assert_eq!(
        tags.get("Project"),
        Some(&Value::Concrete(ConcreteValue::String("carina-rs".into()))),
    );
    assert_eq!(
        tags.get("Repository"),
        Some(&Value::Concrete(ConcreteValue::String(
            "carina-rs/infra".into()
        ))),
    );
    assert_eq!(
        tags.get("Environment"),
        Some(&Value::Concrete(ConcreteValue::String("dev".into()))),
        "Environment must come from the module-call argument; got {tags:?}",
    );
    assert_eq!(
        tags.get("Component"),
        Some(&Value::Concrete(ConcreteValue::String("registry".into()))),
    );
}
