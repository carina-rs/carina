use std::process::Command;

fn run_validate(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", path.to_str().unwrap()])
        .output()
        .expect("failed to execute carina")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn validate_warns_for_single_quoted_interpolation_like_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("main.crn"), "let policy = 'a${b.c}d'\n").unwrap();

    let output = run_validate(dir);
    let stderr = stderr(&output);
    assert!(
        output.status.success(),
        "warning must not fail validate\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr
    );
    assert!(
        stderr.contains("main.crn:1")
            && stderr.contains("single-quoted string contains '${...}'")
            && stderr.contains("use double quotes (\"...\") for interpolation")
            && stderr.contains("keep single quotes if the literal text is intended"),
        "validate must print the parse warning with file and line, got:\n{}",
        stderr
    );
}

#[test]
fn validate_single_quoted_interpolation_like_warning_negative_cases() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(
        dir.join("main.crn"),
        r#"
let env = "prod"
let double_quoted = "arn:${env}:root"
let plain_single = 'arn:env:root'
let lone_dollar = '$'
let dollar_name = '$env'
let missing_close = '${env'
"#,
    )
    .unwrap();

    let output = run_validate(dir);
    let stderr = stderr(&output);
    assert!(
        output.status.success(),
        "negative cases should validate\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr
    );
    assert!(
        !stderr.contains("single-quoted string contains '${...}'"),
        "negative cases must not print the single-quote warning, got:\n{}",
        stderr
    );
}

#[test]
fn validate_warns_for_single_quoted_interpolation_like_sequence_in_sibling_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("main.crn"), "let env = \"prod\"\n").unwrap();
    std::fs::write(dir.join("policy.crn"), "let policy = 'arn:${env}:root'\n").unwrap();

    let output = run_validate(dir);
    let stderr = stderr(&output);
    assert!(
        output.status.success(),
        "warning in a sibling file must not fail validate\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr
    );
    assert!(
        stderr.contains("policy.crn:1")
            && stderr.contains("single-quoted string contains '${...}'"),
        "directory-scoped validate must warn for sibling .crn files, got:\n{}",
        stderr
    );
}

#[test]
fn validate_warns_for_single_quoted_interpolation_like_sequence_in_imported_module() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let module_dir = dir.join("modules").join("policy");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        dir.join("main.crn"),
        r#"let policy_mod = use {
    source = "./modules/policy"
}

let policy = policy_mod {
    env = "prod"
}
"#,
    )
    .unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  env: String\n}\n\nlet policy = 'arn:${env}:root'\n",
    )
    .unwrap();

    let output = run_validate(dir);
    let stderr = stderr(&output);
    assert!(
        output.status.success(),
        "warning in an imported module must not fail validate\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr
    );
    assert!(
        stderr.contains("modules/policy/main.crn:5")
            && stderr.contains("single-quoted string contains '${...}'"),
        "validate must warn with the imported module file path, got:\n{}",
        stderr
    );
}
