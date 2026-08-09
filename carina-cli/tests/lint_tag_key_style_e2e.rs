use std::process::Command;

#[test]
fn lint_renders_one_tag_key_warning_with_file_and_line() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let main_file = fixture.path().join("main.crn");
    std::fs::write(
        &main_file,
        "attributes {\n  tags = {\n    Name = \"application\"\n    Environment = \"production\"\n    managed_by = \"carina\"\n  }\n}\n",
    )
    .expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["lint", fixture.path().to_str().expect("UTF-8 temp path")])
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("run carina lint");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "warning: {}:5  Tag key 'managed_by' doesn't match the dominant style (PascalCase). Use consistent casing for tag keys.",
        main_file.display()
    );
    let warning_lines: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("warning: "))
        .collect();

    assert!(
        !output.status.success(),
        "tag-key warning must make lint fail; stdout: {stdout}\nstderr: {stderr}",
    );
    assert_eq!(warning_lines, vec![expected.as_str()]);
    assert!(
        stderr.contains("Found 1 lint warning(s)."),
        "lint must report exactly one collected warning; stderr: {stderr}"
    );
}
