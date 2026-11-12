//! A rejected authored conversion has one diagnostic across directory aliases.
use std::process::Command;

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
