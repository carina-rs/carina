//! End-to-end regressions for module-relative binding names crossing expansion.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect;
use carina_core::resource::{DeferredValue, Value};
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let root = temp.path().to_path_buf();
        Self { _temp: temp, root }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture directory");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn assert_success(&self, label: &str, output: &Output) {
        assert!(
            output.status.success(),
            "{label} failed:\n{}",
            output_text(output),
        );
    }

    fn validate_and_save_plan(&self) -> PlanFile {
        let validate = self.run(&["validate", "."]);
        self.assert_success("carina validate", &validate);

        let init = self.run(&["init", "."]);
        self.assert_success("carina init", &init);

        let plan = self.run(&["plan", "--refresh=false", "--out", "saved-plan.json", "."]);
        self.assert_success("carina plan", &plan);

        serde_json::from_slice(
            &fs::read(self.root.join("saved-plan.json")).expect("read saved plan"),
        )
        .expect("decode saved plan")
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn write_multifile_root(project: &Project, body: &str) {
    project.write("main.crn", body);
    project.write("providers.crn", "provider mock {}\n");
}

fn resource<'a>(plan: &'a PlanFile, binding: &str) -> &'a carina_core::resource::SavedResource {
    plan.unresolved_resources
        .iter()
        .find(|resource| resource.binding.as_ref().map(|binding| binding.as_str()) == Some(binding))
        .unwrap_or_else(|| panic!("missing expanded resource {binding:?}"))
}

#[test]
fn secret_reference_inside_multifile_module_is_scope_qualified_for_validate_and_plan() {
    let project = Project::new();
    write_multifile_root(
        &project,
        r#"let app_module = use { source = "./modules/app" }

let app = app_module {}
"#,
    );
    project.write(
        "modules/app/main.crn",
        r#"arguments {
  marker: String = "app"
}

let consumer = mock.test.resource {
  name    = "consumer"
  comment = secret(role.identifier)
}
"#,
    );
    project.write(
        "modules/app/iam.crn",
        r#"let role = mock.test.resource {
  name = "role"
}
"#,
    );

    let plan = project.validate_and_save_plan();
    let consumer = resource(&plan, "app.consumer");
    let secret = consumer
        .attributes
        .get("comment")
        .expect("consumer comment");
    assert!(
        matches!(
            secret,
            Value::Deferred(DeferredValue::RedactedSecret { .. })
        ),
        "saved plans must redact the resolved secret expression: {secret:?}",
    );
    assert_eq!(
        consumer
            .dependency_bindings
            .iter()
            .map(|binding| binding.as_str())
            .collect::<Vec<_>>(),
        ["app.role"],
    );
}

#[test]
fn module_depends_on_is_scope_qualified_for_direct_nested_and_for_calls() {
    let project = Project::new();
    write_multifile_root(
        &project,
        r#"let app_module = use { source = "./modules/app" }
let outer_module = use { source = "./modules/outer" }

let direct = app_module {}
let nested = outer_module {}
let looped = for _ in ["blue", "green"] {
  app_module {}
}
"#,
    );
    project.write(
        "modules/app/main.crn",
        r#"arguments {
  marker: String = "app"
}

let consumer = mock.test.resource {
  name = "consumer"

  directives {
    depends_on = [role]
  }
}
"#,
    );
    project.write(
        "modules/app/iam.crn",
        r#"let role = mock.test.resource {
  name = "role"
}
"#,
    );
    project.write(
        "modules/outer/main.crn",
        r#"arguments {
  marker: String = "outer"
}

let app_module = use { source = "../app" }

let inner = app_module {}
"#,
    );
    project.write(
        "modules/outer/extra.crn",
        "let outer_note = \"outer-multifile\"\n",
    );

    let plan = project.validate_and_save_plan();
    for (consumer, dependency) in [
        ("direct.consumer", "direct.role"),
        ("nested.inner.consumer", "nested.inner.role"),
        ("looped[0].consumer", "looped[0].role"),
        ("looped[1].consumer", "looped[1].role"),
    ] {
        assert_eq!(
            resource(&plan, consumer)
                .directives
                .depends_on
                .iter()
                .map(|binding| binding.as_str())
                .collect::<Vec<_>>(),
            [dependency],
            "wrong qualified dependency for {consumer}",
        );
    }
}

#[test]
fn module_local_reference_wins_over_same_named_multifile_root_binding() {
    let project = Project::new();
    write_multifile_root(
        &project,
        r#"let app_module = use { source = "./modules/app" }

let app = app_module {}
"#,
    );
    project.write(
        "root-role.crn",
        r#"let role = mock.test.resource {
  name = "root-role"
}
"#,
    );
    project.write(
        "modules/app/main.crn",
        r#"arguments {
  marker: String = "app"
}
"#,
    );
    project.write(
        "modules/app/role.crn",
        r#"let role = mock.test.resource {
  name = "module-role"
}
"#,
    );
    project.write(
        "modules/app/consumer.crn",
        r#"let consumer = mock.test.resource {
  name    = "consumer"
  comment = role.identifier
}
"#,
    );

    let plan = project.validate_and_save_plan();
    let root_role = resource(&plan, "role")
        .binding
        .as_ref()
        .expect("root role binding");
    let module_role = resource(&plan, "app.role")
        .binding
        .as_ref()
        .expect("module role binding");
    let consumer = resource(&plan, "app.consumer");

    assert_eq!(
        consumer.dependency_bindings.iter().collect::<Vec<_>>(),
        vec![module_role],
        "the module-local reference must create an app.role dependency",
    );
    match consumer.attributes.get("comment") {
        Some(Value::Deferred(DeferredValue::ResourceRef { path })) => {
            assert_eq!(path.binding_address(), module_role);
            assert_ne!(path.binding_address(), root_role);
        }
        other => panic!("expected an unresolved module-local ResourceRef, got {other:?}"),
    }
}

#[test]
fn forwarded_module_parameter_is_pending_for_wait_target_and_dependencies() {
    let project = Project::new();
    project.write(
        "mod/main.crn",
        r#"arguments {
  dep: mock.test.resource
}

let inner_mod = use { source = "../inner" }
let i = inner_mod { dep = dep }
"#,
    );
    project.write("mod/extra.crn", "let module_note = \"multi-file outer\"\n");
    project.write(
        "inner/main.crn",
        r#"arguments {
  dep: mock.test.resource
}

let r = mock.test.resource {
  name = "inner"
}
"#,
    );
    project.write(
        "inner/waits.crn",
        r#"let dependency_wait = wait r {
  until      = r.name == "inner"
  depends_on = [dep]
}

let target_wait = wait dep {
  until = dep.name == "outer"
}
"#,
    );

    let validate = project.run(&["validate", "mod"]);
    project.assert_success("carina validate mod", &validate);
}

#[test]
fn anonymous_module_remap_updates_create_and_delete_dependency_addresses() {
    let project = Project::new();
    project.write("providers.crn", "provider mock {}\n");
    project.write("notes.crn", "let root_note = \"multi-file root\"\n");
    project.write(
        "main.crn",
        r#"let m = use { source = "./modules/m" }

m { label = "consumer-a1" }
"#,
    );
    project.write("modules/m/arguments.crn", "arguments { label: String }\n");
    project.write(
        "modules/m/seed.crn",
        r#"let seed = mock.test.resource {
  name = "seed"
}
"#,
    );
    project.write(
        "modules/m/consumer.crn",
        r#"let consumer = mock.test.resource {
  name = label

  directives {
    depends_on = [seed]
  }
}
"#,
    );

    project.assert_success("carina init", &project.run(&["init", "."]));
    project.assert_success(
        "initial carina apply",
        &project.run(&["apply", "--auto-approve", "."]),
    );

    project.write(
        "main.crn",
        r#"let m = use { source = "./modules/m" }

m { label = "consumer-a2" }
"#,
    );
    let plan_output = project.run(&[
        "plan",
        "--refresh=false",
        "--out",
        "remapped-plan.json",
        ".",
    ]);
    project.assert_success("remapped carina plan", &plan_output);
    let plan: PlanFile = serde_json::from_slice(
        &fs::read(project.root.join("remapped-plan.json")).expect("read remapped plan"),
    )
    .expect("decode remapped plan");

    let consumer = plan
        .unresolved_resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str().ends_with(".consumer"))
        })
        .expect("expanded consumer resource");
    let seed = plan
        .unresolved_resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str().ends_with(".seed"))
        })
        .expect("expanded seed resource");
    let consumer_binding = consumer.binding.as_ref().expect("consumer binding");
    let seed_binding = seed.binding.as_ref().expect("seed binding");

    let create_dependencies = plan
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            UnresolvedSavedEffect::Create(resource)
                if resource.binding.as_ref() == Some(consumer_binding) =>
            {
                Some(&resource.directives.depends_on)
            }
            UnresolvedSavedEffect::Read { .. }
            | UnresolvedSavedEffect::Create(_)
            | UnresolvedSavedEffect::ResumeCreate { .. }
            | UnresolvedSavedEffect::Update { .. }
            | UnresolvedSavedEffect::Delete { .. }
            | UnresolvedSavedEffect::Import { .. }
            | UnresolvedSavedEffect::Remove { .. }
            | UnresolvedSavedEffect::Move { .. }
            | UnresolvedSavedEffect::ReleaseListKeys { .. }
            | UnresolvedSavedEffect::Wait { .. }
            | UnresolvedSavedEffect::DeferredCreate { .. }
            | UnresolvedSavedEffect::DeferredReplace(_) => None,
        })
        .expect("replacement create for consumer");
    assert_eq!(
        create_dependencies,
        &vec![seed_binding.clone()],
        "Create.directives.depends_on must follow the state-prefix remap",
    );

    let delete_dependencies = plan
        .plan
        .effects()
        .iter()
        .find_map(|effect| match effect {
            UnresolvedSavedEffect::Delete {
                binding: Some(binding),
                explicit_dependencies,
                ..
            } if binding == consumer_binding => Some(explicit_dependencies),
            UnresolvedSavedEffect::Read { .. }
            | UnresolvedSavedEffect::Create(_)
            | UnresolvedSavedEffect::ResumeCreate { .. }
            | UnresolvedSavedEffect::Update { .. }
            | UnresolvedSavedEffect::Delete { .. }
            | UnresolvedSavedEffect::Import { .. }
            | UnresolvedSavedEffect::Remove { .. }
            | UnresolvedSavedEffect::Move { .. }
            | UnresolvedSavedEffect::ReleaseListKeys { .. }
            | UnresolvedSavedEffect::Wait { .. }
            | UnresolvedSavedEffect::DeferredCreate { .. }
            | UnresolvedSavedEffect::DeferredReplace(_) => None,
        })
        .expect("replacement delete for consumer");
    assert_eq!(
        delete_dependencies,
        &std::collections::HashSet::from([seed_binding.clone()]),
        "Delete.explicit_dependencies must follow the state-prefix remap",
    );
}
