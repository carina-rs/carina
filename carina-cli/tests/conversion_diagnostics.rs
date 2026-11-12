//! A rejected authored conversion has one diagnostic across directory aliases.
use std::process::Command;

#[test]
fn conversion_cli_chains_report_one_outer_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    for depth in [2, 3] {
        std::fs::write(
            directory.path().join("main.crn"),
            format!(
                "let sink = mock.test.resource {{ name = 'ok'{} }}\n",
                " as String".repeat(depth)
            ),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", "."])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{text}");
        assert_eq!(
            text.matches("a conversion cannot be the operand of another conversion")
                .count(),
            1,
            "{text}"
        );
        assert_eq!(text.matches("Error:").count(), 1, "{text}");
        assert!(text.contains("main.crn:1:40:"), "{text}");
    }
}

#[test]
fn conversion_cli_module_keeps_the_ordinary_error_prefix() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let child = use { source = './child' }\nchild { input = 7 }\n",
    )
    .unwrap();
    let mut ordinary = Vec::new();
    for converted in [false, true] {
        let extra = if converted {
            "\nlet converted = 'ok' as String\n"
        } else {
            ""
        };
        std::fs::write(directory.path().join("child/main.crn"), format!("arguments {{ input: Int }}\nlet sink = mock.test.resource {{ name = input }}\n{extra}")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", "."])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{text}");
        let message = text
            .lines()
            .find(|line| line.contains("cannot assign Int") && line.contains("input"))
            .unwrap_or_else(|| panic!("missing ordinary mismatch: {text}"));
        ordinary.push(message.to_string());
    }
    assert!(ordinary[0].contains("child:"), "{ordinary:?}");
    assert_eq!(ordinary[0], ordinary[1]);
}

#[test]
fn conversion_cli_directory_alias_rejection_is_reported_once() {
    for count in [1, 8] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            directory.path().join("sibling.crn"),
            "let converted = 'ok' as String\n",
        )
        .unwrap();
        let resources = (0..count)
            .map(|index| format!("let sink{index} = mock.test.resource {{ name = converted }}\n"))
            .collect::<String>();
        std::fs::write(directory.path().join("main.crn"), resources).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", "."])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{text}");
        assert_eq!(
            text.matches("conversion target must be a loaded named scalar type")
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("sibling.crn:1:17:"), "{text}");
    }
}

#[test]
fn conversion_cli_directory_parse_rejection_keeps_the_convert_location() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("sibling.crn"),
        "let converted = if 42 as String { 'ok' } else { 'ok' }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "let sink = mock.test.resource { name = converted }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["validate", "."])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{text}");
    assert_eq!(
        text.matches("conversion is not supported inside this expression")
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains("sibling.crn:1:20:"), "{text}");
}

#[test]
fn conversion_cli_module_body_rejection_has_one_owner() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(directory.path().join("main.crn"),
        "let child = use { source = './child' }\nlet one = child { input = 'ok' }\nlet two = child { input = 'ok' }\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { input: String }\nlet sink = mock.test.resource { name = input as String }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory.path())
        .args(["validate", ".", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success(), "{json}");
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{json}");
    assert_eq!(errors[0]["code"], "conversion_target", "{json}");
    assert!(
        errors[0]["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("child/main.crn"),
        "{json}"
    );
}
