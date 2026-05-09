//! Integration test for shared provider attributes (#2717).
//!
//! Reproduces the CLI pipeline (parse → load → resolve → module-expand →
//! finalize) against a multi-file fixture that sources `default_tags`
//! from a sibling module via `use` + module-call + field access.
//! Asserts the resolved tag map matches what would be produced by a
//! literal `default_tags = { ... }` map.

use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::resource::Value;

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

    let mut config = load_configuration(&fixture).expect("load_configuration");

    // Run the CLI's full validate-and-resolve pipeline so module
    // expansion + finalize happen. `skip_resource_validation = true`
    // bypasses provider-plugin loading; the mock provider's WASM is not
    // available in the test environment, but we only need pipeline
    // semantics here.
    carina_cli::commands::validate_and_resolve(&mut config.parsed, &fixture, true)
        .expect("validate_and_resolve must succeed");

    let provider = config
        .parsed
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
    assert_eq!(tags.get("ManagedBy"), Some(&Value::String("carina".into())),);
    assert_eq!(
        tags.get("Project"),
        Some(&Value::String("carina-rs".into())),
    );
    assert_eq!(
        tags.get("Repository"),
        Some(&Value::String("carina-rs/infra".into())),
    );
    assert_eq!(
        tags.get("Environment"),
        Some(&Value::String("dev".into())),
        "Environment must come from the module-call argument; got {tags:?}",
    );
    assert_eq!(
        tags.get("Component"),
        Some(&Value::String("registry".into())),
    );
}
