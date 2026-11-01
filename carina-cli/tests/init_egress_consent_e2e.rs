//! Directory-scoped CLI coverage for provider-declared egress consent.
//!
//! Each scenario uses separate provider and project files so the spawned
//! binary crosses the same directory-loading boundary as a real project.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scenario {
    _temp: tempfile::TempDir,
    project: PathBuf,
    provider_source: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        Self::with_provider("mock_http", wasm_mock_http_path(), "mock-http.wasm")
    }

    fn with_provider(kind: &str, fixture: PathBuf, file_name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let provider_source = project.join(file_name);
        fs::copy(fixture, &provider_source).expect("copy provider fixture");
        fs::write(
            project.join("providers.crn"),
            format!("provider {kind} {{\n  source = \"file://{file_name}\"\n}}\n"),
        )
        .expect("write provider declaration");
        fs::write(project.join("main.crn"), "let project_marker = true\n")
            .expect("write sibling project file");
        Self {
            _temp: temp,
            project,
            provider_source,
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("CARINA_ACCEPT_EGRESS")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn canonical_source(&self) -> String {
        format!(
            "file://{}",
            self.provider_source
                .file_name()
                .expect("provider source file name")
                .to_string_lossy()
        )
    }
}

fn wasm_mock_http_path() -> PathBuf {
    wasm_fixture_path(&[
        "carina_provider_mock_http.wasm",
        "carina-provider-mock-http.wasm",
    ])
}

fn wasm_mock_path() -> PathBuf {
    wasm_fixture_path(&["carina_provider_mock.wasm", "carina-provider-mock.wasm"])
}

fn wasm_fixture_path(names: &[&str]) -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("carina-cli is in the workspace root");
    names
    .iter()
    .copied()
    .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
    .find(|path| path.is_file())
    .unwrap_or_else(|| {
        panic!(
            "build the e2e provider fixtures first: cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2"
        )
    })
}

fn append_custom_section(bytes: &mut Vec<u8>, name: &str, data: &[u8]) {
    fn push_u32_leb(output: &mut Vec<u8>, mut value: u32) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    let mut payload = Vec::new();
    push_u32_leb(&mut payload, name.len().try_into().unwrap());
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(data);
    bytes.push(0);
    push_u32_leb(bytes, payload.len().try_into().unwrap());
    bytes.extend_from_slice(&payload);
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copied project directory");
    for entry in fs::read_dir(source).expect("read project directory") {
        let entry = entry.expect("read project entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("read project entry type").is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy project file");
        }
    }
}

fn rendered(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn replace_all_same_length(bytes: &mut [u8], old: &[u8], new: &[u8]) {
    assert_eq!(old.len(), new.len());
    let mut replacements = 0;
    for offset in 0..=bytes.len().saturating_sub(old.len()) {
        if &bytes[offset..offset + old.len()] == old {
            bytes[offset..offset + old.len()].copy_from_slice(new);
            replacements += 1;
        }
    }
    assert!(replacements > 0, "provider fixture contains the old host");
}

fn replace_provider_file(path: &Path, bytes: &[u8]) {
    let replacement = path.with_extension("replacement.wasm");
    fs::write(&replacement, bytes).expect("write rebuilt provider");
    fs::rename(replacement, path).expect("replace provider atomically");
}

#[test]
fn non_tty_init_requires_the_canonical_source_to_be_accepted() {
    let scenario = Scenario::new();

    let denied = scenario.carina(&["init", "."]);

    assert!(!denied.status.success(), "{}", rendered(&denied));
    let denied_text = rendered(&denied);
    assert!(
        denied_text.contains(&scenario.canonical_source()),
        "{denied_text}"
    );
    assert!(denied_text.contains("--accept-egress"), "{denied_text}");
    assert_eq!(
        denied_text
            .matches("requires explicit egress consent")
            .count(),
        1,
        "{denied_text}"
    );

    let source = scenario.canonical_source();
    let accepted = scenario.carina(&["init", ".", "--accept-egress", &source]);

    assert!(accepted.status.success(), "{}", rendered(&accepted));
    let lock = fs::read_to_string(scenario.project.join("carina-providers.lock"))
        .expect("read provider lock");
    assert!(lock.contains("[provider.egress]"), "{lock}");
    assert!(lock.contains("consent = \"explicit\""), "{lock}");
    assert!(lock.contains("\"phase-test.invalid\""), "{lock}");
    assert!(lock.contains("\"resolved-metadata.invalid\""), "{lock}");
}

#[test]
fn comma_separated_environment_accepts_file_urls_and_multiple_entries() {
    let scenario = Scenario::new();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&scenario.project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env(
            "CARINA_ACCEPT_EGRESS",
            " file://main.crn, file://mock-http.wasm ",
        )
        .args(["init", "."])
        .output()
        .expect("run carina with accepted egress environment");

    assert!(output.status.success(), "{}", rendered(&output));
}

#[test]
fn file_consent_survives_copying_the_project_to_another_absolute_directory() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();
    let initialized = scenario.carina(&["init", ".", "--accept-egress", &source]);
    assert!(initialized.status.success(), "{}", rendered(&initialized));
    let original_lock = fs::read_to_string(scenario.project.join("carina-providers.lock")).unwrap();

    let copied_root = tempfile::tempdir().expect("create copied project root");
    let copied_project = copied_root.path().join("copied-project");
    copy_directory(&scenario.project, &copied_project);
    let reinitialized = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&copied_project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_ACCEPT_EGRESS")
        .args(["init", "."])
        .output()
        .expect("run carina in copied project");

    assert!(
        reinitialized.status.success(),
        "{}",
        rendered(&reinitialized)
    );
    let copied_lock = fs::read_to_string(copied_project.join("carina-providers.lock")).unwrap();
    let source_lines = |lock: &str| {
        lock.lines()
            .filter(|line| line.starts_with("source = "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(source_lines(&original_lock), source_lines(&copied_lock));
    assert!(original_lock.contains("source = \"file://mock-http.wasm\""));

    let plan = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(&copied_project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_ACCEPT_EGRESS")
        .args(["plan", "."])
        .output()
        .expect("run plan in copied project");
    assert!(plan.status.success(), "{}", rendered(&plan));
}

#[test]
fn relative_file_resolution_uses_the_command_cwd_but_consent_is_project_relative() {
    let root = tempfile::tempdir().expect("create command cwd");
    let project = root.path().join("project");
    fs::create_dir(&project).expect("create project");
    fs::copy(wasm_mock_http_path(), project.join("mock-http.wasm")).expect("copy provider fixture");
    fs::write(
        project.join("providers.crn"),
        "provider mock_http {\n  source = \"file://project/mock-http.wasm\"\n}\n",
    )
    .expect("write provider declaration");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(root.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("CARINA_ACCEPT_EGRESS")
            .args(args)
            .output()
            .expect("run carina outside the project directory")
    };

    let denied = run(&["init", "project"]);
    assert!(!denied.status.success(), "{}", rendered(&denied));
    let denied_text = rendered(&denied);
    let printed_source = denied_text
        .split_once("provider source ")
        .and_then(|(_, rest)| rest.split_once(" requires explicit egress consent"))
        .map(|(source, _)| source)
        .expect("non-interactive error names the canonical consent source");
    assert_eq!(printed_source, "file://mock-http.wasm", "{denied_text}");

    let accepted = run(&["init", "project", "--accept-egress", printed_source]);
    assert!(accepted.status.success(), "{}", rendered(&accepted));
    let lock = fs::read_to_string(project.join("carina-providers.lock")).unwrap();
    assert!(
        lock.contains("source = \"file://project/mock-http.wasm\""),
        "{lock}"
    );

    let plan = run(&["plan", "project"]);
    assert!(plan.status.success(), "{}", rendered(&plan));
}

#[test]
fn empty_declaration_needs_no_consent_until_hosts_are_declared() {
    let scenario = Scenario::with_provider("mock", wasm_mock_path(), "mock.wasm");

    let initialized = scenario.carina(&["init", "."]);
    assert!(initialized.status.success(), "{}", rendered(&initialized));
    let lock = fs::read_to_string(scenario.project.join("carina-providers.lock")).unwrap();
    assert!(!lock.contains("[provider.egress]"), "{lock}");

    let plan = scenario.carina(&["plan", "."]);
    assert!(plan.status.success(), "{}", rendered(&plan));
    let apply = scenario.carina(&["apply", "--auto-approve", "."]);
    assert!(apply.status.success(), "{}", rendered(&apply));

    let mut provider = fs::read(&scenario.provider_source).expect("read empty provider");
    append_custom_section(
        &mut provider,
        "carina:egress",
        br#"{"version":1,"hosts":["api.example.com"]}"#,
    );
    replace_provider_file(&scenario.provider_source, &provider);

    let changed_init = scenario.carina(&["init", "."]);
    assert!(
        !changed_init.status.success(),
        "{}",
        rendered(&changed_init)
    );
    assert!(
        rendered(&changed_init).contains("requires explicit egress consent"),
        "{}",
        rendered(&changed_init)
    );

    let changed_plan = scenario.carina(&["plan", "."]);
    assert!(
        !changed_plan.status.success(),
        "{}",
        rendered(&changed_plan)
    );
    assert!(
        rendered(&changed_plan).contains("no consented egress record"),
        "{}",
        rendered(&changed_plan)
    );
}

#[test]
fn locked_and_accept_egress_are_a_cli_usage_error() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();

    let output = scenario.carina(&["init", ".", "--locked", "--accept-egress", &source]);

    assert_eq!(output.status.code(), Some(2), "{}", rendered(&output));
    let text = rendered(&output);
    assert!(
        text.contains("'--locked' cannot be used with '--accept-egress <SOURCE>'"),
        "{text}"
    );
}

#[test]
fn accept_egress_is_not_a_global_option() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();

    let output = scenario.carina(&["--accept-egress", &source, "init", "."]);

    assert_eq!(output.status.code(), Some(2), "{}", rendered(&output));
    assert!(
        rendered(&output).contains("unexpected argument '--accept-egress'"),
        "{}",
        rendered(&output)
    );
}

#[test]
fn runtime_factory_creation_refuses_a_changed_local_declaration() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();
    let initialized = scenario.carina(&["init", ".", "--accept-egress", &source]);
    assert!(initialized.status.success(), "{}", rendered(&initialized));

    let mut bytes = fs::read(&scenario.provider_source).expect("read provider fixture");
    let old = b"phase-test.invalid";
    let same_normalized = b"Phase-test.invalid";
    replace_all_same_length(&mut bytes, old, same_normalized);
    replace_provider_file(&scenario.provider_source, &bytes);

    let plan_with_installed_copy = scenario.carina(&["plan", "."]);
    assert!(
        plan_with_installed_copy.status.success(),
        "plan should still load the installed copy before init: {}",
        rendered(&plan_with_installed_copy)
    );
    let carried = scenario.carina(&["init", "."]);
    assert!(carried.status.success(), "{}", rendered(&carried));
    assert!(
        rendered(&carried).contains("egress hosts unchanged"),
        "{}",
        rendered(&carried)
    );

    replace_all_same_length(&mut bytes, same_normalized, old);
    replace_provider_file(&scenario.provider_source, &bytes);
    let upgraded = scenario.carina(&["init", ".", "--upgrade"]);
    assert!(upgraded.status.success(), "{}", rendered(&upgraded));
    assert!(
        rendered(&upgraded).contains("egress hosts unchanged"),
        "{}",
        rendered(&upgraded)
    );

    let new = b"other-test.invalid";
    replace_all_same_length(&mut bytes, old, new);
    replace_provider_file(&scenario.provider_source, &bytes);

    let changed_init = scenario.carina(&["init", "."]);
    assert!(
        !changed_init.status.success(),
        "{}",
        rendered(&changed_init)
    );
    let changed_text = rendered(&changed_init);
    assert!(
        changed_text.contains("egress hosts changed"),
        "{changed_text}"
    );
    assert!(
        changed_text.contains("+ other-test.invalid"),
        "{changed_text}"
    );
    assert!(
        changed_text.contains("- phase-test.invalid"),
        "{changed_text}"
    );

    let plan = scenario.carina(&["plan", "."]);

    assert!(!plan.status.success(), "{}", rendered(&plan));
    let text = rendered(&plan);
    assert!(
        text.contains("egress declaration differs from the lock"),
        "{text}"
    );
    assert!(text.contains("carina init"), "{text}");

    let lock_path = scenario.project.join("carina-providers.lock");
    let lock_before = fs::read(&lock_path).expect("read lock before locked consent failure");
    let locked = scenario.carina(&["init", ".", "--locked"]);
    assert!(!locked.status.success(), "{}", rendered(&locked));
    assert!(
        rendered(&locked).contains("locked egress consent is missing or stale"),
        "{}",
        rendered(&locked)
    );
    assert_eq!(
        rendered(&locked)
            .matches("locked egress consent is missing or stale")
            .count(),
        1,
        "{}",
        rendered(&locked)
    );
    assert_eq!(
        fs::read(lock_path).expect("read lock after locked consent failure"),
        lock_before,
        "a locked egress-consent error must leave the lock bytes unchanged"
    );
}

#[test]
fn invalid_locked_egress_pattern_is_scoped_to_runtime_and_init_repairs_it() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();
    let initialized = scenario.carina(&["init", ".", "--accept-egress", &source]);
    assert!(initialized.status.success(), "{}", rendered(&initialized));

    let lock_path = scenario.project.join("carina-providers.lock");
    let lock = fs::read_to_string(&lock_path).expect("read provider lock");
    let damaged = lock.replace("\"phase-test.invalid\"", "\"*.com\"");
    assert_ne!(damaged, lock, "fixture lock contains the declared host");
    fs::write(&lock_path, damaged).expect("write invalid egress record");

    let validate = scenario.carina(&["validate", "."]);
    assert!(validate.status.success(), "{}", rendered(&validate));

    let plan = scenario.carina(&["plan", "."]);
    assert!(!plan.status.success(), "{}", rendered(&plan));
    let plan_text = rendered(&plan);
    assert!(plan_text.contains(&source), "{plan_text}");
    assert!(plan_text.contains("*.com"), "{plan_text}");

    let repaired = scenario.carina(&["init", ".", "--accept-egress", &source]);
    assert!(repaired.status.success(), "{}", rendered(&repaired));
    let repaired_lock = fs::read_to_string(lock_path).expect("read repaired provider lock");
    assert!(!repaired_lock.contains("\"*.com\""), "{repaired_lock}");
    assert!(
        repaired_lock.contains("\"phase-test.invalid\""),
        "{repaired_lock}"
    );
}

#[test]
fn schema_commands_validate_the_section_without_requiring_consent() {
    let scenario = Scenario::new();
    let source = scenario.canonical_source();
    let initialized = scenario.carina(&["init", ".", "--accept-egress", &source]);
    assert!(initialized.status.success(), "{}", rendered(&initialized));

    let lock_path = scenario.project.join("carina-providers.lock");
    let lock = fs::read_to_string(&lock_path).expect("read provider lock");
    let (without_egress, _) = lock
        .split_once("\n[provider.egress]")
        .expect("init wrote an egress sub-table");
    fs::write(&lock_path, format!("{without_egress}\n")).expect("remove egress consent");

    let validated = scenario.carina(&["validate", "."]);
    assert!(validated.status.success(), "{}", rendered(&validated));
    let formatted = scenario.carina(&["fmt", "."]);
    assert!(formatted.status.success(), "{}", rendered(&formatted));

    let runtime = scenario.carina(&["plan", "."]);
    assert!(!runtime.status.success(), "{}", rendered(&runtime));
    assert!(
        rendered(&runtime).contains("no consented egress record"),
        "{}",
        rendered(&runtime)
    );

    let mut bytes = fs::read(&scenario.provider_source).expect("read provider fixture");
    replace_all_same_length(&mut bytes, b"\"version\":1", b"\"version\":2");
    replace_provider_file(&scenario.provider_source, &bytes);

    let invalid_init = scenario.carina(&["init", "."]);
    assert!(
        !invalid_init.status.success(),
        "{}",
        rendered(&invalid_init)
    );
    assert!(
        rendered(&invalid_init).contains("unknown egress declaration version 2"),
        "{}",
        rendered(&invalid_init)
    );

    let invalid_validate = scenario.carina(&["validate", "."]);
    assert!(
        !invalid_validate.status.success(),
        "{}",
        rendered(&invalid_validate)
    );
    assert!(
        rendered(&invalid_validate).contains("unknown egress declaration version 2"),
        "{}",
        rendered(&invalid_validate)
    );
    let invalid_fmt = scenario.carina(&["fmt", "."]);
    assert!(invalid_fmt.status.success(), "{}", rendered(&invalid_fmt));
}
