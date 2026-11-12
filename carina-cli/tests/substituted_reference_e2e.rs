//! Parse-time resource substitution must retain the reference assignment rule.
use std::path::PathBuf;
use std::process::{Command, Output};

fn run(dir: &std::path::Path, args: &[&str]) -> (Output, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output, text)
}

fn fixture(source: &str, consumer: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let wasm = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| target.join("wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("build the WASM mock fixture before running this test");
    let uri = format!("file://{}", wasm.display());
    for (name, body) in [
        ("sources.crn", source.to_owned()),
        ("main.crn", consumer.to_owned()),
        (
            "providers.crn",
            format!("provider mock {{ source = '{uri}' }}\n"),
        ),
        (
            "backend.crn",
            "backend local { path = 'state.json' }\n".to_owned(),
        ),
    ] {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let (output, text) = run(dir.path(), &["init", ".", "--accept-egress", &uri]);
    assert!(output.status.success(), "{text}");
    dir
}

#[test]
fn substituted_reference_validate_matches_unknown_source_verdicts_and_messages() {
    let dir = fixture(
        "let a = mock.test.read_back {}\n",
        "let sink = mock.test.recovery_probe { value = a.computed }\n",
    );
    let errors = |text: &str| {
        text.lines()
            .filter_map(|line| line.strip_prefix("Error: "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let (control, text) = run(dir.path(), &["validate", "."]);
    assert!(!control.status.success(), "{text}");
    let control_errors = errors(&text);
    assert_eq!(control_errors.len(), 1, "{text}");
    for (unknown_source, known_source, reference, accepted) in [
        (
            "let a = mock.test.estimated_input {}",
            "let a = mock.test.estimated_input { name = 'abc' }",
            "a.name",
            false,
        ),
        (
            "let r = mock.test.reference { name = 'r' }",
            "let r = mock.test.reference { name = 'r' key = 'k' }",
            "r.key",
            false,
        ),
    ] {
        for sibling in [false, true] {
            let consumer =
                format!("let sink = mock.test.recovery_probe {{ value = {reference} }}\n");
            let write = |source: &str| {
                std::fs::write(
                    dir.path().join("sources.crn"),
                    if sibling { source } else { "" },
                )
                .unwrap();
                std::fs::write(
                    dir.path().join("main.crn"),
                    if sibling {
                        consumer.clone()
                    } else {
                        format!("{source}\n{consumer}")
                    },
                )
                .unwrap();
            };
            write(unknown_source);
            let (unknown, text) = run(dir.path(), &["validate", "."]);
            assert_eq!(
                unknown.status.success(),
                accepted,
                "{reference}, sibling={sibling}: {text}"
            );
            let unknown_errors = errors(&text);
            if !accepted {
                let expected: Vec<_> = control_errors
                    .iter()
                    .map(|message| {
                        let message = message.replace("a.computed", reference);
                        if reference == "r.key" {
                            message.replace("String", "mock.keys.Key.Reference")
                        } else {
                            message
                        }
                    })
                    .collect();
                assert_eq!(unknown_errors, expected);
            }
            write(known_source);
            let (output, text) = run(dir.path(), &["validate", "."]);
            assert_eq!(
                output.status.success(),
                unknown.status.success(),
                "{reference}, sibling={sibling}: {text}"
            );
            assert_eq!(
                errors(&text),
                unknown_errors,
                "{reference}, sibling={sibling}"
            );
            assert!(!text.contains(" as "), "{text}");
        }
    }
}

#[test]
fn substituted_reference_validate_keeps_literals_aliases_and_discarded_sources_accepted() {
    let dir = fixture(
        "let a = mock.test.estimated_input { name = 'abc' }\nlet literal = 'abc'\nlet alias = literal\n",
        "let direct = mock.test.recovery_probe { value = 'abc' }\nlet aliased = mock.test.recovery_probe { value = alias }\nlet coalesced = mock.test.recovery_probe { value = 'abc' ?? a.name }\nlet selected = mock.test.recovery_probe { value = if true { 'abc' } else { a.name } }\nlet reference_direct = mock.test.reference { name = 'direct' key = 'abc' }\nlet reference_alias = mock.test.reference { name = 'alias' key = alias }\n",
    );
    let (output, text) = run(dir.path(), &["validate", "."]);
    assert!(output.status.success(), "{text}");
}

#[test]
fn substituted_constrained_identityless_value_keeps_validate_and_plan_acceptance() {
    let dir = fixture(
        "let a = mock.test.estimated_input { name = 'abc' }\n",
        "let sink = mock.test.pattern_modes { full = 'abc' }\n",
    );
    for value in ["'abc'", "a.name"] {
        std::fs::write(
            dir.path().join("main.crn"),
            format!("let sink = mock.test.pattern_modes {{ full = {value} }}\n"),
        )
        .unwrap();
        for command in ["validate", "plan"] {
            let (output, text) = run(dir.path(), &[command, "."]);
            assert!(output.status.success(), "{command} {value}: {text}");
            assert!(
                !text.lines().any(|line| line.starts_with("Error: ")),
                "{text}"
            );
        }
    }
}

#[test]
fn secret_substituted_reference_masks_its_origin_like_unknown_source() {
    let consumer = "let sink = mock.test.recovery_probe { value = secret(a.name) }\n";
    let dir = fixture("let a = mock.test.estimated_input {}\n", consumer);
    for sibling in [false, true] {
        for source in [
            "let a = mock.test.estimated_input {}\n",
            "let a = mock.test.estimated_input { name = 'abc' }\n",
        ] {
            std::fs::write(
                dir.path().join("sources.crn"),
                if sibling { source } else { "" },
            )
            .unwrap();
            std::fs::write(
                dir.path().join("main.crn"),
                if sibling {
                    consumer.to_owned()
                } else {
                    format!("{source}{consumer}")
                },
            )
            .unwrap();
            let (output, text) = run(dir.path(), &["validate", "."]);
            assert!(!output.status.success(), "{text}");
            assert_eq!(
                text.lines()
                    .filter_map(|line| line.strip_prefix("Error: "))
                    .collect::<Vec<_>>(),
                [
                    "mock.test.recovery_probe.sink: cannot assign String to 'value': expected mock.RecoveryProbe, got String (from (secret))"
                ],
                "{text}",
            );
        }
    }
}

#[test]
fn issue_4384_reference_to_identified_string_is_rejected_by_validate() {
    let main = "mock.test.recovery_probe { value = r.key }\n";
    let dir = fixture("let r = mock.test.reference { name = 'r' }\n", main);
    for source in [
        "let r = mock.test.reference { name = 'r' }\n",
        "let r = mock.test.reference { name = 'r' key = 'k' }\n",
    ] {
        std::fs::write(dir.path().join("sources.crn"), source).unwrap();
        let (output, text) = run(dir.path(), &["validate", "."]);
        assert!(!output.status.success(), "{source}{main}\n{text}");
        assert_eq!(
            text.lines()
                .filter_map(|line| line.strip_prefix("Error: "))
                .collect::<Vec<_>>(),
            [
                "mock.test.recovery_probe.<pending>: cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)"
            ],
            "{text}"
        );
    }
}
