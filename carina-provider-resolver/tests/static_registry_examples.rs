use std::fs;
use std::path::Path;

const EXAMPLE_MARKER_PREFIX: &str = "<!-- example: ";
const EXAMPLE_MARKER_SUFFIX: &str = " -->";

fn example_path_from_marker(line: &str) -> Option<&str> {
    line.strip_prefix(EXAMPLE_MARKER_PREFIX)
        .and_then(|line| line.strip_suffix(EXAMPLE_MARKER_SUFFIX))
        .filter(|path| !path.is_empty())
}

#[test]
fn inlined_examples_match_their_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let document =
        fs::read_to_string(root.join("site/src/content/reference/registry/static-registry.md"))
            .expect("failed to read site/src/content/reference/registry/static-registry.md");
    let example_dir = root.join("docs/registry/static-registry-example");
    let lines: Vec<_> = document.lines().collect();
    let mut json_fence_count = 0;

    for (index, line) in lines.iter().enumerate() {
        let relative_path = example_path_from_marker(line);

        if line.starts_with("<!--") && line.contains("example") {
            assert!(
                relative_path.is_some(),
                "invalid example marker on line {}",
                index + 1
            );
        }

        if *line == "```json" {
            json_fence_count += 1;
            assert!(
                index
                    .checked_sub(1)
                    .and_then(|index| example_path_from_marker(lines[index]))
                    .is_some(),
                "```json fence on line {} is not immediately preceded by a valid example marker",
                index + 1
            );
        }
    }

    assert!(
        json_fence_count > 0,
        "site/src/content/reference/registry/static-registry.md has no ```json fences"
    );

    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let relative_path = example_path_from_marker(line);

        let Some(relative_path) = relative_path else {
            index += 1;
            continue;
        };

        assert_eq!(
            lines.get(index + 1),
            Some(&"```json"),
            "example {relative_path} is not immediately followed by a ```json fence"
        );

        let content_start = index + 2;
        let content_end = lines[content_start..]
            .iter()
            .position(|line| *line == "```")
            .map(|offset| content_start + offset)
            .unwrap_or_else(|| panic!("example {relative_path} has no closing fence"));
        let mut content = Vec::new();
        for line in &lines[content_start..content_end] {
            content.extend_from_slice(line.as_bytes());
            content.push(b'\n');
        }

        let example_path = example_dir.join(relative_path);
        assert!(
            example_path.is_file(),
            "example file does not exist: {relative_path}"
        );
        let file_content = fs::read(&example_path)
            .unwrap_or_else(|error| panic!("failed to read example {relative_path}: {error}"));
        assert_eq!(
            content, file_content,
            "documentation example diverges from {relative_path}"
        );

        index = content_end + 1;
    }
}

#[test]
fn example_files_are_valid_json() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    assert_json_files_parse(&root.join("docs/registry/static-registry-example"));
}

fn assert_json_files_parse(directory: &Path) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()));

    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "failed to read an entry in {}: {error}",
                directory.display()
            )
        });
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));

        if file_type.is_dir() {
            assert_json_files_parse(&path);
        } else if file_type.is_file() {
            let content = fs::read(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            serde_json::from_slice::<serde_json::Value>(&content)
                .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()));
        }
    }
}
