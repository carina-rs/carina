use std::process::Command;

#[test]
fn lint_reports_same_block_duplicate_export_without_last_wins_text() {
    let fixture = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        fixture.path().join("main.crn"),
        "exports {\n  x = \"one\"\n  x = \"two\"\n}\n",
    )
    .expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", fixture.path().to_str().expect("UTF-8 temp path")])
        .output()
        .expect("run carina lint");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "duplicate exports must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert_eq!(
        stderr.matches("duplicate export 'x'").count(),
        1,
        "lint must render one duplicate group; stderr: {stderr}",
    );
    assert!(
        stderr.contains("duplicate export 'x': declared 2 times in main.crn"),
        "lint must preserve same-file multiset provenance; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("last value will be used"),
        "lint must not claim duplicate exports are last-write-wins; stderr: {stderr}",
    );
}

#[test]
fn lint_reports_same_block_duplicate_module_attribute_without_last_wins_text() {
    let fixture = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        fixture.path().join("main.crn"),
        "attributes {\n  x = \"one\"\n  x = \"two\"\n}\n",
    )
    .expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", fixture.path().to_str().expect("UTF-8 temp path")])
        .output()
        .expect("run carina lint");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "duplicate module attributes must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert_eq!(
        stderr.matches("duplicate module attribute 'x'").count(),
        1,
        "lint must render one duplicate group; stderr: {stderr}",
    );
    assert!(
        stderr.contains("duplicate module attribute 'x': declared 2 times in main.crn"),
        "lint must preserve same-file multiset provenance; stderr: {stderr}",
    );
    assert!(
        !stderr.contains("last value will be used"),
        "lint must not claim duplicate module attributes are last-write-wins; stderr: {stderr}",
    );
}

#[test]
fn lint_reports_duplicate_declaration_in_imported_module_once() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("module");
    std::fs::create_dir(&module).expect("module dir");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let component = use { source = \"module\" }\n\nlet instance = component { }\n",
    )
    .expect("write caller");
    std::fs::write(
        module.join("a.crn"),
        "attributes {\n  shared = \"from-a\"\n}\n",
    )
    .expect("write module a");
    std::fs::write(
        module.join("b.crn"),
        "attributes {\n  shared = \"from-b\"\n}\n",
    )
    .expect("write module b");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", fixture.path().to_str().expect("UTF-8 temp path")])
        .output()
        .expect("run carina lint");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = "module: duplicate module attribute 'shared': declared in a.crn and b.crn";

    assert!(
        !output.status.success(),
        "module duplicates must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert_eq!(
        stderr
            .matches("duplicate module attribute 'shared'")
            .count(),
        1,
        "lint must render exactly one prefixed module duplicate group; stderr: {stderr}",
    );
    assert!(
        stderr.contains(message),
        "lint must preserve the module prefix and sibling provenance; stderr: {stderr}",
    );
}
