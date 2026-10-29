//! Recovery commands may skip provider/resource validation, but they must not
//! skip module-boundary reference typing before expansion.

use std::process::Command;

fn ordinary_resource_reference_fixture(source: &str) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    std::fs::write(fixture.path().join("main.crn"), source).expect("root configuration");
    fixture
}

fn multi_file_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    for (relative_path, source) in files {
        let path = fixture.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture directory");
        }
        std::fs::write(path, source).expect("fixture file");
    }
    fixture
}

fn run_carina(fixture: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .args(args)
        .output()
        .expect("run carina")
}

#[derive(Clone, Copy)]
enum ModuleOutputFixture {
    Direct,
    Nested,
}

fn module_output_mismatch_fixture(kind: ModuleOutputFixture) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        fixture.path().join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .expect("backend configuration");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
        .expect("provider configuration");

    match kind {
        ModuleOutputFixture::Direct => {
            let module = fixture.path().join("m");
            std::fs::create_dir_all(&module).expect("module directory");
            std::fs::write(
                fixture.path().join("main.crn"),
                r#"let m = use { source = './m' }
let x = m { n = 1 }
let y = m { n = x.s }
"#,
            )
            .expect("root configuration");
            std::fs::write(module.join("arguments.crn"), "arguments { n: Int }\n")
                .expect("module arguments");
            std::fs::write(
                module.join("attributes.crn"),
                "attributes {\n  o: Int = n\n  s: String = 'str'\n}\n",
            )
            .expect("module attributes");
        }
        ModuleOutputFixture::Nested => {
            let mid = fixture.path().join("mid");
            let leaf = mid.join("leaf");
            std::fs::create_dir_all(&leaf).expect("nested module directories");
            std::fs::write(
                fixture.path().join("main.crn"),
                "let mid = use { source = './mid' }\nmid { a = 'hello' }\n",
            )
            .expect("root configuration");
            std::fs::write(mid.join("arguments.crn"), "arguments { a: String }\n")
                .expect("mid arguments");
            std::fs::write(
                mid.join("calls.crn"),
                r#"let leaf = use { source = './leaf' }
let x = leaf { n = 1 }
let y = leaf { n = x.s }
"#,
            )
            .expect("nested calls");
            std::fs::write(leaf.join("arguments.crn"), "arguments { n: Int }\n")
                .expect("leaf arguments");
            std::fs::write(
                leaf.join("attributes.crn"),
                "attributes {\n  o: Int = n\n  s: String = 'str'\n}\n",
            )
            .expect("leaf attributes");
        }
    }
    fixture
}

fn leaf_output_mismatch_fixture() -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("m");
    std::fs::create_dir_all(&module).expect("module directory");
    std::fs::write(
        fixture.path().join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .expect("backend configuration");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
        .expect("provider configuration");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let m = use { source = './m' }\nlet x = m { a = 'hello' }\n",
    )
    .expect("root configuration");
    std::fs::write(module.join("arguments.crn"), "arguments { a: String }\n")
        .expect("module arguments");
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  out: Int = a\n}\n",
    )
    .expect("module attributes");
    fixture
}

fn assert_reference_type_failure(fixture: &tempfile::TempDir, args: &[&str], expected: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .args(args)
        .output()
        .expect("run command");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "{args:?} must reject the reference mismatch; stderr={stderr}"
    );
    for needle in expected {
        assert!(
            stderr.contains(needle),
            "{args:?} must include {needle:?}; stderr={stderr}"
        );
    }
    assert!(
        !stderr.contains("_80c187e") && !stderr.contains("module call 'mid_"),
        "{args:?} must use authored module-call names, not instance hashes; stderr={stderr}"
    );
}

fn nested_forwarding_mismatch_fixture() -> tempfile::TempDir {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"backend local { path = 'carina.state.json' }
provider mock {}
let outer = use { source = './outer' }
outer { a = 'wrong' }
"#,
    )
    .expect("root configuration");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = './inner' }
arguments { a: String }
inner { x = a }
"#,
    )
    .expect("outer module");
    std::fs::write(
        inner.join("main.crn"),
        r#"arguments { x: Int }
mock.test.resource {
  name = 'inner'
  count = x
}
"#,
    )
    .expect("inner module");
    fixture
}

#[test]
fn destroy_and_state_refresh_reject_nested_forwarding_mismatch() {
    let fixture = nested_forwarding_mismatch_fixture();
    for args in [
        &["destroy", "--auto-approve", "--lock=false", "."][..],
        &["state", "refresh", "--lock=false", "."][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(fixture.path())
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .expect("run recovery command");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{args:?} must stop before expansion; stderr={stderr}"
        );
        assert!(
            stderr.contains("Type mismatch: expected Int, got String (from a)")
                && stderr.contains("outer/main.crn")
                && stderr.contains("line 3"),
            "{args:?} must report the shared BindingIndex mismatch; stderr={stderr}"
        );
    }
}

#[test]
fn all_cli_paths_reject_direct_module_output_forwarding_mismatch() {
    let fixture = module_output_mismatch_fixture(ModuleOutputFixture::Direct);
    for args in [
        &["validate", "."][..],
        &["lint", "."][..],
        &["destroy", "--auto-approve", "--lock=false", "."][..],
        &["state", "refresh", "--lock=false", "."][..],
    ] {
        assert_reference_type_failure(
            &fixture,
            args,
            &["module call 'y'", "expected Int", "got String", "from x.s"],
        );
    }
}

#[test]
fn all_cli_paths_reject_nested_module_output_forwarding_mismatch_without_hashes() {
    let fixture = module_output_mismatch_fixture(ModuleOutputFixture::Nested);
    for args in [
        &["validate", "."][..],
        &["lint", "."][..],
        &["destroy", "--auto-approve", "--lock=false", "."][..],
        &["state", "refresh", "--lock=false", "."][..],
    ] {
        assert_reference_type_failure(
            &fixture,
            args,
            &["module call 'y'", "expected Int", "got String", "from x.s"],
        );
    }
}

#[test]
fn recovery_commands_reject_reference_mismatch_in_leaf_module() {
    let fixture = leaf_output_mismatch_fixture();
    for args in [
        &["destroy", "--auto-approve", "--lock=false", "."][..],
        &["state", "refresh", "--lock=false", "."][..],
    ] {
        assert_reference_type_failure(
            &fixture,
            args,
            &["attribute 'out'", "expected Int", "got String", "from a"],
        );
    }
}

#[test]
fn recovery_and_lint_defer_unknown_plain_resource_attributes() {
    for source in [
        r#"backend local { path = "state.json" }
provider mock {}
let a = mock.test.resource { name = "a" }
let b = mock.test.resource {
  name = "b"
  tags = { x = a.bogus }
}
"#,
        r#"backend local { path = "state.json" }
provider mock {}
let cert = mock.test.resource { name = "c" status = "ISSUED" }
let cert_issued = wait cert { until = cert.status == "ISSUED" }
let lst = mock.test.resource {
  name = "lst"
  upstream = cert_issued.id
}
"#,
    ] {
        for args in [
            &["destroy", "--auto-approve", "--lock=false", "."][..],
            &["state", "refresh", "--lock=false", "."][..],
            &["lint", "."][..],
        ] {
            let fixture = ordinary_resource_reference_fixture(source);
            if args
                .first()
                .is_some_and(|command| matches!(*command, "destroy" | "state"))
            {
                let init = run_carina(&fixture, &["init", "."]);
                assert!(
                    init.status.success(),
                    "init failed before {args:?}: {}",
                    String::from_utf8_lossy(&init.stderr)
                );
            }
            let output = run_carina(&fixture, args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{args:?} must leave plain resource validation skippable; stderr={stderr}"
            );
        }
    }
}

#[test]
fn validate_aggregates_resource_schema_and_reference_errors() {
    let fixture = ordinary_resource_reference_fixture(
        r#"provider mock {}
let a = mock.test.resource {
  name = "a"
  nope = "x"
}
let b = mock.test.resource {
  name = "b"
  tags = { x = a.bogus }
}
"#,
    );

    let output = run_carina(&fixture, &["validate", "."]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "validate must fail; stderr={stderr}"
    );
    assert!(
        stderr.contains("Unknown attribute 'nope'"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains("unknown attribute 'bogus'"),
        "stderr={stderr}"
    );
    assert!(
        !stderr.contains("Module resolution error"),
        "ordinary resource errors must stay in validation aggregation; stderr={stderr}"
    );
}

#[test]
fn evaluated_environment_value_is_redacted_at_function_boundary() {
    let fixture = ordinary_resource_reference_fixture(
        r#"fn f(x: Int): String { 'ok' }
let result = f(env('CARINA_3848_SECRET'))
"#,
    );
    let plaintext = "hunter2plaintext";
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .env("CARINA_3848_SECRET", plaintext)
        .args(["validate", "."])
        .output()
        .expect("run carina validate");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "validate must reject env String -> Int"
    );
    assert!(
        stderr.contains("function 'f': parameter 'x': expected Int, got string"),
        "stderr={stderr}"
    );
    assert!(!stderr.contains(plaintext), "stderr={stderr}");
}

#[test]
fn environment_plaintext_is_redacted_across_cli_directory_typecheck_paths() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "module literal union argument",
            &[
                (
                    "main.crn",
                    "let m = use { source = './m' }\nm { n = f() }\n",
                ),
                ("functions.crn", "fn f() { env('LEAKX') }\n"),
                ("m/main.crn", "arguments { n: 'dev' | 'prod' }\n"),
            ],
        ),
        (
            "module list argument",
            &[
                (
                    "main.crn",
                    "let m = use { source = './m' }\nm { l = list_values() }\n",
                ),
                (
                    "functions.crn",
                    "fn list_values() { [env('LEAKX'), 'b'] }\n",
                ),
                ("m/main.crn", "arguments { l: list(Int) }\n"),
            ],
        ),
        (
            "literal union export",
            &[
                ("functions.crn", "fn f() { env('LEAKX') }\n"),
                ("exports.crn", "exports { e: 'dev' | 'prod' = f() }\n"),
            ],
        ),
        (
            "nested function export",
            &[
                ("functions.crn", "fn k(x) { x }\n"),
                ("exports.crn", "exports { q: Int = k(env('LEAKX')) }\n"),
            ],
        ),
    ];

    for (label, files) in cases {
        let fixture = multi_file_fixture(files);
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(fixture.path())
            .env("NO_COLOR", "1")
            .env("LEAKX", "hunter2SECRET")
            .args(["validate", "."])
            .output()
            .expect("run carina validate");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !output.status.success(),
            "{label} must fail; stderr={stderr}"
        );
        assert!(stderr.contains("got string"), "{label}: stderr={stderr}");
        assert!(
            !stderr.contains("hunter2SECRET"),
            "{label} leaked env plaintext: stderr={stderr}"
        );
    }
}

#[test]
fn authored_literal_mismatch_still_displays_plaintext_in_cli() {
    let fixture = multi_file_fixture(&[
        (
            "main.crn",
            "let m = use { source = './m' }\nm { n = 'production' }\n",
        ),
        ("m/main.crn", "arguments { n: 'dev' | 'prod' }\n"),
    ]);

    let output = run_carina(&fixture, &["validate", "."]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "validate must fail; stderr={stderr}"
    );
    assert!(
        stderr.contains("got string 'production'"),
        "authored literal detail must remain visible; stderr={stderr}"
    );
}

#[test]
fn deferred_environment_call_is_rechecked_after_directory_merge() {
    let fixture = ordinary_resource_reference_fixture(
        r#"fn f(x: Int): String { 'ok' }
let result = f(env('CARINA_3848_DEFINITELY_UNSET'))
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .env_remove("CARINA_3848_DEFINITELY_UNSET")
        .args(["validate", "."])
        .output()
        .expect("run carina validate");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "the outer function must not discard a deferred env() argument"
    );
    assert!(
        stderr.contains("CARINA_3848_DEFINITELY_UNSET") && stderr.contains("environment variable"),
        "stderr={stderr}"
    );
}
