use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn aws_fixture_path() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("carina-cli is in the workspace root");
    [
        "carina_provider_mock_http_aws.wasm",
        "carina-provider-mock-http-aws.wasm",
    ]
    .into_iter()
    .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
    .find(|path| path.is_file())
    .unwrap_or_else(|| {
        panic!(
            "build the AWS e2e fixture first: cargo build -p carina-provider-mock -p carina-provider-mock-http --target wasm32-wasip2"
        )
    })
}

fn isolated_command(
    project: &Path,
    home: &Path,
    provider_source: &Path,
    accept_egress: bool,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CARINA_TEST_HOME", home)
        .env("CARINA_TRUST_RECORD_DIR", home.join(".carina/trust"))
        .env("CARINA_PLUGIN_CACHE_DIR", home.join(".carina/plugin-cache"))
        .env("SSL_CERT_FILE", "/etc/ssl/cert.pem")
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env(
            "CARINA_TRUST_LOCAL_PROVIDER",
            format!("{}=aws", provider_source.display()),
        );
    for name in std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_string_lossy().starts_with("AWS_"))
    {
        command.env_remove(name);
    }
    command.env("AWS_EC2_METADATA_DISABLED", "true");
    if accept_egress {
        command.env("CARINA_ACCEPT_EGRESS", "file://mock-aws.wasm");
    } else {
        command.env_remove("CARINA_ACCEPT_EGRESS");
    }
    command
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    let mut stdout = tempfile::tempfile().expect("create stdout capture");
    let mut stderr = tempfile::tempfile().expect("create stderr capture");
    let mut child = command
        .stdout(Stdio::from(
            stdout.try_clone().expect("clone stdout capture"),
        ))
        .stderr(Stdio::from(
            stderr.try_clone().expect("clone stderr capture"),
        ))
        .spawn()
        .expect("spawn carina");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll carina") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("carina did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };

    let read_capture = |file: &mut fs::File| {
        file.seek(SeekFrom::Start(0)).expect("rewind capture");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read capture");
        bytes
    };
    Output {
        status,
        stdout: read_capture(&mut stdout),
        stderr: read_capture(&mut stderr),
    }
}

fn rendered(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn apply_without_aws_credentials_fails_fast_with_the_typed_startup_error() {
    let project = tempfile::tempdir().expect("create project");
    let home = tempfile::tempdir().expect("create empty home");
    let provider_source = project.path().join("mock-aws.wasm");
    fs::copy(aws_fixture_path(), &provider_source).expect("copy AWS-world fixture");
    fs::write(
        project.path().join("providers.crn"),
        "provider aws {\n  source = \"file://mock-aws.wasm\"\n}\n",
    )
    .expect("write provider config");
    fs::write(project.path().join("main.crn"), "let marker = true\n")
        .expect("write project config");

    let mut init = isolated_command(project.path(), home.path(), &provider_source, true);
    init.args(["init", "."]);
    let initialized = run_with_timeout(init, Duration::from_secs(120));
    assert!(initialized.status.success(), "{}", rendered(&initialized));

    let mut apply = isolated_command(project.path(), home.path(), &provider_source, false);
    apply.args(["apply", ".", "--auto-approve", "--lock=false"]);
    let output = run_with_timeout(apply, Duration::from_secs(30));

    assert!(!output.status.success(), "{}", rendered(&output));
    let text = rendered(&output);
    assert!(
        text.contains("Failed to resolve AWS credentials:"),
        "{text}"
    );
    assert!(text.contains("AWS_PROFILE"), "{text}");
    assert!(text.contains("aws sso login"), "{text}");
    assert!(text.contains("AWS_ACCESS_KEY_ID"), "{text}");
}
