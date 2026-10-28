//! End-to-end regression coverage for carina#3884.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "carina {operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn write_common_files(dir: &Path) {
    fs::create_dir_all(dir).expect("create project directory");
    fs::write(
        dir.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .expect("write backend configuration");
    fs::write(dir.join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
}

#[test]
fn unresolved_upstream_builtin_arguments_stay_deferred() {
    let root = tempfile::tempdir().expect("create temporary root");
    let upstream = root.path().join("up");
    let downstream = root.path().join("down");
    write_common_files(&upstream);
    write_common_files(&downstream);

    fs::write(
        upstream.join("main.crn"),
        "exports {\n  vpc_id = \"vpc-abc123\"\n}\n",
    )
    .expect("write upstream exports");
    fs::write(
        downstream.join("upstream.crn"),
        "let network = upstream_state { source = \"../up\" }\n",
    )
    .expect("write upstream_state configuration");
    fs::write(
        downstream.join("main.crn"),
        r#"let v = upper(network.vpc_id)

let direct = mock.test.resource {
  name = upper(network.vpc_id)
}

let via_let = mock.test.resource {
  name = v
}

let nested = mock.test.resource {
  name = upper(lower(network.vpc_id))
}

let interpolated = mock.test.resource {
  name = "${upper(network.vpc_id)}-x"
}

let list_arg = mock.test.resource {
  name = join("-", [network.vpc_id, "a"])
}

let piped = mock.test.resource {
  name = network.vpc_id |> upper()
}
"#,
    )
    .expect("write downstream resources");

    // The upstream project is deliberately never initialized or applied, so
    // its declared export has no state value for the downstream plan to read.
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");

    let plan = run_cli(&downstream, &["plan", "."]);
    assert_success(&plan, "plan downstream");
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(
        !stdout.contains("Error") && !stdout.contains("got Unknown"),
        "built-ins must not evaluate unknown upstream exports:\n{stdout}",
    );

    let deferred_upper = "name: upper((known after upstream apply: network.vpc_id))";
    assert!(
        stdout.matches(deferred_upper).count() >= 3,
        "direct, let-bound, and piped calls must stay deferred:\n{stdout}",
    );
    for expected in [
        "name: upper(lower((known after upstream apply: network.vpc_id)))",
        "name: \"${upper((known after upstream apply: network.vpc_id))}-x\"",
        "name: join('-', [(known after upstream apply: network.vpc_id), 'a'])",
    ] {
        assert!(
            stdout.contains(expected),
            "plan output must contain `{expected}`:\n{stdout}",
        );
    }
}
