use std::fs;
use std::process::Command;

#[test]
fn fmt_check_accepts_wait_dotted_dependencies_in_a_multifile_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let files = [
        ("main.crn", carina_core::formatter::format("let cert = mock.test.resource { name = \"cert\" }\nlet r = mock.test.resource { name = \"dependency\" }\n", &Default::default()).unwrap()),
        ("wait.crn", "let ready = wait cert {\n  until      = cert.name == 'cert'\n  depends_on = [r, producer.validation_record]\n}\n".to_string()),
        ("providers.crn", "provider mock {\n}\n".to_string()),
        ("modules.crn", carina_core::formatter::format("let records = use { source = './producer' }\nlet producer = records {}\n", &Default::default()).unwrap()),
    ];
    for (filename, source) in &files {
        fs::write(fixture.path().join(filename), source).unwrap();
    }
    fs::create_dir(fixture.path().join("producer")).unwrap();
    let producer = carina_core::formatter::format("let record = mock.test.resource { name = 'validation' }\nattributes { validation_record: mock.test.resource = record }\n", &Default::default()).unwrap();
    fs::write(fixture.path().join("producer/main.crn"), producer).unwrap();
    carina_core::config_loader::parse_directory(fixture.path(), &Default::default())
        .expect("multi-file fixture parses with its module binding");
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .args(["fmt", "--check", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "carina fmt --check failed: {output:?}"
    );
    for (filename, source) in &files {
        assert_eq!(
            fs::read_to_string(fixture.path().join(filename)).unwrap(),
            *source
        );
    }
}
