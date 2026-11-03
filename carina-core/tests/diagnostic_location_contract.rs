//! Compile the public location contracts under nextest's isolated HOME.
//! This keeps the compile-fail guarantees executable in nextest-only runs.
use std::{path::PathBuf, process::Command};

#[test]
fn diagnostic_location_contract_rejects_unprovenanced_construction() {
    let dependencies = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let library = std::fs::read_dir(&dependencies)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("libcarina_core-") && name.ends_with(".rlib"))
        })
        .max_by_key(|entry| entry.metadata().unwrap().modified().unwrap())
        .unwrap()
        .path();
    let compiler = std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO")).with_file_name(if cfg!(windows) {
                "rustc.exe"
            } else {
                "rustc"
            })
        });
    let directory = tempfile::tempdir().unwrap();
    let compile = |source: &str| {
        let input = directory.path().join("contract.rs");
        std::fs::write(&input, source).unwrap();
        Command::new(&compiler)
            .args([
                "--edition=2024",
                "--crate-type=lib",
                "--emit=metadata",
                "--crate-name=location_contract",
            ])
            .arg("--extern")
            .arg(format!("carina_core={}", library.display()))
            .arg("-L")
            .arg(format!("dependency={}", dependencies.display()))
            .arg("-o")
            .arg(directory.path().join("contract.rmeta"))
            .arg(input)
            .output()
            .unwrap()
    };
    let positive = compile(
        "use carina_core::parser::{DeclarationLocation, Located}; pub fn report(location: DeclarationLocation) -> Located<&'static str> { Located::new(location, \"error\") }",
    );
    assert!(
        positive.status.success(),
        "compiler setup: {}",
        String::from_utf8_lossy(&positive.stderr)
    );
    for (source, code) in [
        (
            "use carina_core::parser::Located; fn report() { let _ = Located::new(None, \"error\"); }",
            "E0308",
        ),
        (
            "use carina_core::parser::DeclarationLocation; fn fabricate() { let _ = DeclarationLocation::from_offsets(\"file.crn\", 0, 1); }",
            "E0599",
        ),
        (
            "use carina_core::parser::DeclarationLocation; fn fabricate() { let _ = DeclarationLocation::reference_prefix_from_pair; }",
            "E0624",
        ),
        (
            "use carina_core::parser::DeclarationLocation; fn fabricate() { let _ = DeclarationLocation::block_header_from_pair; }",
            "E0624",
        ),
        (
            "use carina_core::parser::FileScoped; fn fabricate() { let _ = FileScoped::new(std::path::Path::new(\"file.crn\"), \"error\"); }",
            "E0624",
        ),
        (
            "use carina_core::parser::{FileScoped, SourceFileId}; fn fabricate(file: SourceFileId) -> FileScoped<&'static str> { FileScoped { file, detail: \"error\" } }",
            "E0451",
        ),
        (
            "use carina_core::parser::{DirectoryScoped, Located}; fn attach(finding: DirectoryScoped<&'static str>) -> Located<&'static str> { finding }",
            "E0308",
        ),
        (
            "use carina_core::parser::DirectoryScoped; fn fabricate() { let _ = DirectoryScoped::new(\"skip location\"); }",
            "E0599",
        ),
        (
            "use carina_core::{parser::ParsedFile, upstream_exports::{check_upstream_state_field_references, UpstreamExports}}; fn analyze(file: &ParsedFile) { check_upstream_state_field_references(file, &UpstreamExports::new()); }",
            "E0308",
        ),
        (
            "use carina_core::{parser::{ParsedFile, ProviderContext}, schema::SchemaRegistry, validation::validate_deferred_for_iterable_types}; fn analyze(file: &ParsedFile, context: &ProviderContext) { validate_deferred_for_iterable_types(file, &SchemaRegistry::new(), context); }",
            "E0308",
        ),
        (
            "use carina_core::{parser::{DeclarationLocation, Located}, validation::TypeResolutionDiagnostic}; fn location(finding: &Located<TypeResolutionDiagnostic>) { let _: Option<&DeclarationLocation> = finding.location(); }",
            "E0308",
        ),
        (
            "use carina_core::{parser::ParsedFile, schema::SchemaRegistry, validation::wait::validate_wait_bindings}; fn analyze(file: &ParsedFile) { validate_wait_bindings(file, &SchemaRegistry::new()); }",
            "E0308",
        ),
        (
            "use carina_core::{parser::ParsedFile, schema::SchemaRegistry, validation::deferred_populate::validate_deferred_populate_refs}; fn analyze(file: &ParsedFile) { validate_deferred_populate_refs(file, &SchemaRegistry::new()); }",
            "E0308",
        ),
        (
            "use carina_core::parser::{ParsedFile, check_provider_instance_routing}; fn analyze(file: &ParsedFile) { check_provider_instance_routing(file); }",
            "E0308",
        ),
    ] {
        let result = compile(source);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !result.status.success() && stderr.contains(code),
            "{source}\nexpected {code}: {stderr}"
        );
    }
}
