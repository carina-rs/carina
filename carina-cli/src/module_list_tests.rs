//! Tests for the `module list` subcommand.

use std::path::PathBuf;

use carina_core::config_loader;

#[test]
fn module_list_shows_imports() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_dir = PathBuf::from(format!("{}/tests/fixtures/module_list", manifest_dir));

    let config = config_loader::load_configuration(&fixture_dir).unwrap();
    let imports = &config.parsed.imports;

    assert_eq!(imports.len(), 2);
    assert_eq!(imports[0].alias, "web_tier");
    assert_eq!(imports[0].path, "./modules/web_tier");
    assert_eq!(imports[1].alias, "network");
    assert_eq!(imports[1].path, "./modules/network");
}

#[test]
fn module_list_empty_when_no_imports() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_dir = PathBuf::from(format!("{}/tests/fixtures/module_info", manifest_dir));

    let config = config_loader::load_configuration(&fixture_dir).unwrap();
    let imports = &config.parsed.imports;

    assert!(imports.is_empty());
}
