//! End-to-end regression coverage for carina#4127.
//!
//! Provider custom types used by exports must be checked with the same loaded
//! provider context as validation. The real failure used directory-scoped
//! bindings from `main.crn` in typed exports from `exports.crn`, so this test
//! keeps that multi-file shape and runs the spawned CLI against the WASM mock
//! provider.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let provider_source = project.join("provider.wasm");
        fs::copy(wasm_mock_path(), &provider_source).expect("copy mock provider fixture");

        fs::write(
            project.join("providers.crn"),
            format!(
                "provider mock {{\n  source = \"file://{}\"\n}}\n",
                provider_source.display()
            ),
        )
        .expect("write provider configuration");
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend configuration");
        fs::write(
            project.join("main.crn"),
            r#"let registry_prod = "prod-account"
let registry_dev = "dev-account"
let admin_access_roles = ["admin-role", "audit-role"]
"#,
        )
        .expect("write value bindings");
        fs::write(project.join("exports.crn"), valid_exports()).expect("write typed exports");

        let scenario = Self {
            _temp: temp,
            project,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn replace_exports(&self, source: &str) {
        fs::write(self.project.join("exports.crn"), source).expect("replace typed exports");
    }

    fn write_guarded_module_call(&self) {
        let module = self.project.join("guarded");
        fs::create_dir(&module).expect("create guarded module");
        fs::write(
            module.join("main.crn"),
            r#"arguments {
  token: mock.test.sdk_validator.Guarded
}
"#,
        )
        .expect("write guarded module");
        fs::write(
            self.project.join("main.crn"),
            r#"let guarded_module = use { source = "./guarded" }
let guarded = guarded_module {
  token = "reject-me"
}
"#,
        )
        .expect("write guarded module call");
        fs::write(self.project.join("exports.crn"), "").expect("clear unrelated exports");
    }

    fn write_resource_attribute_exports(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"let registry_prod = mock.test.recovery_probe {
  value = "prod-account"
}

let registry_dev = mock.test.recovery_probe {
  value = "dev-account"
}
"#,
        )
        .expect("write export source resources");
        fs::write(
            self.project.join("exports.crn"),
            r#"exports {
  accounts: map(mock.RecoveryProbe) = {
    registry_prod = registry_prod.value
    registry_dev  = registry_dev.value
  }
  admin_access_role_arns: list(mock.RecoveryProbe) = [
    registry_prod.value,
    registry_dev.value,
  ]
}
"#,
        )
        .expect("write resource-attribute exports");
    }
}

fn wasm_mock_path() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("carina-cli is in the workspace root");
    ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "build the e2e provider fixture first: \
                 cargo build -p carina-provider-mock --target wasm32-wasip2"
            )
        })
}

fn valid_exports() -> &'static str {
    r#"exports {
  accounts: map(mock.RecoveryProbe) = {
    registry_prod = registry_prod
    registry_dev  = registry_dev
  }
  admin_access_role_arns: list(mock.RecoveryProbe) = admin_access_roles
}
"#
}

fn invalid_exports() -> &'static str {
    r#"exports {
  invalid_tokens: list(mock.test.sdk_validator.Guarded) = admin_access_roles
}
"#
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    let text = output_text(output);
    assert!(output.status.success(), "{label} failed:\n{text}");
}

fn assert_provider_rejection(label: &str, output: &Output) {
    let text = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly passed:\n{text}"
    );
    assert!(
        text.contains("intentional SDK schema validator rejection"),
        "{label} did not surface the provider's custom-type rejection:\n{text}"
    );
    assert!(
        !text.contains("provider custom types are not loaded"),
        "{label} used an unloaded provider context:\n{text}"
    );
}

#[test]
fn provider_custom_type_exports_plan_and_validate_map_and_list() {
    let scenario = Scenario::new();

    assert_success("carina validate", &scenario.carina(&["validate", "."]));
    assert_success(
        "carina plan",
        &scenario.carina(&["plan", "--refresh=false", "."]),
    );
}

#[test]
fn provider_custom_type_resource_attribute_exports_apply_then_plan_from_state() {
    let scenario = Scenario::new();
    scenario.write_resource_attribute_exports();

    assert_success(
        "carina apply",
        &scenario.carina(&["apply", "--auto-approve", "."]),
    );
    assert!(
        scenario.project.join("carina.state.json").is_file(),
        "apply must persist state before the plan regression step"
    );
    assert_success(
        "carina plan with existing state",
        &scenario.carina(&["plan", "--refresh=false", "."]),
    );
}

#[test]
fn provider_custom_type_module_rejection_lint_uses_loaded_provider() {
    let scenario = Scenario::new();
    scenario.write_guarded_module_call();

    assert_provider_rejection("carina lint", &scenario.carina(&["lint", "."]));
}

#[test]
fn provider_custom_type_exports_surface_provider_rejection() {
    let scenario = Scenario::new();
    scenario.replace_exports(invalid_exports());

    assert_provider_rejection("carina validate", &scenario.carina(&["validate", "."]));
    assert_provider_rejection(
        "carina plan",
        &scenario.carina(&["plan", "--refresh=false", "."]),
    );
}
