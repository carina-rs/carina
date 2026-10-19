//! CLI regressions for rejecting bare structural handles as values.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
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

fn assert_init_and_plan_succeed(root: &Path) -> String {
    let init = run(root, &["init", "."]);
    assert_success("carina init", &init);

    let plan = run(root, &["plan", "--refresh=false", "."]);
    let text = output_text(&plan);
    assert!(plan.status.success(), "carina plan failed:\n{text}");
    text
}

fn assert_validate_rejects(files: &[(&str, &str)], expected: &str, line: usize) {
    let fixture = tempfile::tempdir().expect("temporary project");
    for (path, contents) in files {
        write_file(fixture.path(), path, contents);
    }

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success(),
        "validate unexpectedly passed:\n{text}"
    );
    assert!(
        text.contains(expected) && text.contains(&format!("line {line}")),
        "missing {expected:?} at line {line}:\n{text}",
    );
}

#[test]
fn validate_rejects_bare_resource_handles_in_value_positions() {
    for (source, binding, line) in [
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nmock.test.resource { parent = vpc }\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nlet values = [vpc]\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nlet values = { primary = vpc }\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nexports { handle: String = vpc }\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nexports { handle = vpc }\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nlet rendered = \"prefix-${vpc}\"\n",
            "vpc",
            2,
        ),
        (
            "let vpc = mock.test.resource { name = \"vpc\" }\nlet alias = vpc\nmock.test.resource { parent = alias }\n",
            "alias",
            3,
        ),
        (
            "mock.test.resource { parent = later }\nlet later = mock.test.resource { name = \"later\" }\n",
            "later",
            1,
        ),
    ] {
        assert_validate_rejects(
            &[("main.crn", source)],
            &format!("'{binding}' is a resource, not a value"),
            line,
        );
    }
}

#[test]
fn validate_rejects_every_structural_handle_kind() {
    for (source, expected, line) in [
        (
            "let items = for name in [\"one\"] { mock.test.resource { name = name } }\nmock.test.resource { parent = items }\n",
            "'items' is a for binding, not a value",
            2,
        ),
        (
            "let selected = if true { mock.test.resource { name = \"one\" } }\nmock.test.resource { parent = selected }\n",
            "'selected' is an if binding, not a value",
            2,
        ),
        (
            "let imported = use { source = \"./module\" }\nmock.test.resource { parent = imported }\n",
            "'imported' is a module import, not a value",
            2,
        ),
        (
            "let imported = use { source = \"./module\" }\nlet instance = imported {}\nmock.test.resource { parent = instance }\n",
            "'instance' is a module call, not a value",
            3,
        ),
        (
            "let source = mock.test.resource { name = \"source\" }\nlet ready = wait source { until = source.ready == true }\nmock.test.resource { parent = ready }\n",
            "'ready' is a wait binding, not a value",
            3,
        ),
        (
            "let orgs = upstream_state { source = \"../organizations\" }\nmock.test.resource { parent = orgs }\n",
            "'orgs' is an upstream state, not a value",
            2,
        ),
        (
            "let secondary = provider mock {}\nmock.test.resource { parent = secondary }\n",
            "'secondary' is a provider instance, not a value",
            2,
        ),
    ] {
        assert_validate_rejects(&[("main.crn", source)], expected, line);
    }
}

#[test]
fn validate_rejects_sibling_and_module_boundary_bare_handles() {
    assert_validate_rejects(
        &[
            ("main.crn", "mock.test.resource { parent = sibling }\n"),
            (
                "sibling.crn",
                "let sibling = mock.test.resource { name = \"sibling\" }\n",
            ),
        ],
        "'sibling' is a resource, not a value",
        1,
    );

    assert_validate_rejects(
        &[
            (
                "main.crn",
                "let vpc = mock.test.resource { name = \"vpc\" }\nlet imported = use { source = \"./module\" }\nlet instance = imported { parent = vpc }\n",
            ),
            ("module/arguments.crn", "arguments { parent: String }\n"),
        ],
        "'vpc' is a resource, not a value",
        3,
    );

    assert_validate_rejects(
        &[
            (
                "main.crn",
                "let vpc = mock.test.resource { name = \"vpc\" }\nlet imported = use { source = \"./module\" }\nlet instance = imported { parent = vpc }\n",
            ),
            (
                "module/arguments.crn",
                "arguments { parent: mock.test.resource }\n",
            ),
            (
                "module/main.crn",
                "mock.test.resource { parent = parent }\n",
            ),
        ],
        "'parent' is a resource handle, not a value",
        1,
    );

    assert_validate_rejects(
        &[
            (
                "main.crn",
                "let imported = use { source = \"./module\" }\nlet instance = imported {}\n",
            ),
            (
                "module/main.crn",
                "let internal = mock.test.resource { name = \"internal\" }\nattributes { handle: String = internal }\n",
            ),
        ],
        "'internal' is a resource, not a value",
        2,
    );
}

#[test]
fn validate_names_the_file_that_owns_a_sibling_bare_handle_error() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "consumer.crn",
        "mock.test.resource { parent = sibling }\n",
    );
    write_file(
        fixture.path(),
        "sibling.crn",
        "let sibling = mock.test.resource { name = \"sibling\" }\n",
    );

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success(),
        "validate unexpectedly passed:\n{text}"
    );
    assert!(
        text.contains("consumer.crn")
            && text.contains("'sibling' is a resource, not a value")
            && text.contains("line 1"),
        "the CLI error must name the owning file:\n{text}",
    );
}

#[test]
fn cli_defers_ref_annotation_checks_and_preserves_sibling_function_handles() {
    let value_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        value_fixture.path(),
        "functions.crn",
        "fn tag(r: mock.compute.Instance, suffix: String): String { suffix }\n",
    );
    write_file(
        value_fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
mock.test.resource {
  name = "consumer"
  parent = tag(vpc.arn, "ok")
}
"#,
    );
    write_file(
        value_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    let validate = run(value_fixture.path(), &["validate", "."]);
    assert_success(
        "bootstrap validation of a value passed to a Ref-annotated parameter",
        &validate,
    );

    let handle_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        handle_fixture.path(),
        "functions.crn",
        "fn name_of(r: mock.compute.Instance): String { r.name }\n",
    );
    write_file(
        handle_fixture.path(),
        "main.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\n",
    );
    write_file(
        handle_fixture.path(),
        "use.crn",
        r#"mock.test.resource {
  name = "consumer"
  parent = name_of(vpc)
  parent_name = vpc |> name_of()
}
"#,
    );
    write_file(
        handle_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    let validate = run(handle_fixture.path(), &["validate", "."]);
    assert_success("sibling function handle validation", &validate);
    let plan = assert_init_and_plan_succeed(handle_fixture.path());
    assert!(
        plan.contains("parent: 'vpc'") && plan.contains("parent_name: 'vpc'"),
        "sibling function calls must be evaluated before planning:\n{plan}",
    );

    let export_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        export_fixture.path(),
        "functions.crn",
        "fn name_of(r: mock.compute.Instance): String { r.name }\n",
    );
    write_file(
        export_fixture.path(),
        "main.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\nexports { name = name_of(vpc) }\n",
    );
    write_file(export_fixture.path(), "providers.crn", "provider mock {}\n");
    assert_success(
        "unannotated export inference from a deferred sibling function",
        &run(export_fixture.path(), &["validate", "."]),
    );
}

#[test]
fn cli_runs_sibling_function_handle_type_checks_after_schemas_are_loaded() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "functions.crn",
        "fn name_of(r: mock.compute.Instance): String { r.name }\n",
    );
    write_file(
        fixture.path(),
        "main.crn",
        r#"let wrong = mock.test.resource { name = "wrong" }
mock.test.resource {
  name = "consumer"
  parent = name_of(wrong)
}
"#,
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("expects resource type 'mock.compute.Instance'")
            && text.contains("got mock.test.resource"),
        "the enriched function boundary must reject the wrong handle type:\n{text}",
    );
}

#[test]
fn cli_preserves_a_handle_through_a_function_local_alias() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "functions.crn",
        r#"fn name_of(r: mock.compute.Instance): String {
  let aliased = r
  aliased.name
}
"#,
    );
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
mock.test.resource {
  name = "consumer"
  parent = name_of(vpc)
}
"#,
    );
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    assert_success(
        "function-local structural alias validation",
        &run(fixture.path(), &["validate", "."]),
    );
    let plan = assert_init_and_plan_succeed(fixture.path());
    assert!(
        plan.contains("parent: 'vpc'"),
        "the aliased handle access must resolve to the resource attribute:\n{plan}",
    );
}

#[test]
fn cli_applies_module_parameter_access_to_value_arguments() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let imported = use { source = "./module" }
imported { r = vpc.arn }
"#,
    );
    write_file(
        fixture.path(),
        "module/main.crn",
        r#"arguments { r: mock.compute.Instance }
mock.test.resource {
  name = "consumer"
  parent = r.name
}
"#,
    );
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    let lowercase = text.to_ascii_lowercase();
    assert!(
        !output.status.success()
            && lowercase.contains("cannot apply access .name to string")
            && text.contains("module/main.crn")
            && text.contains("line 4"),
        "module value access must be applied, not dropped:\n{text}",
    );
}

#[test]
fn cli_binds_function_and_module_parameters_from_argument_shape() {
    let value_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        value_fixture.path(),
        "functions.crn",
        r#"fn identity(r: mock.compute.Instance): String { r }
fn decorate(r: mock.compute.Instance): String { "${r}-suffix" }
"#,
    );
    write_file(
        value_fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
mock.test.resource {
  name = "consumer"
  parent = identity(vpc.arn)
  parent_name = decorate(vpc.arn)
}
"#,
    );
    write_file(
        value_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    assert_success(
        "value-shaped function arguments",
        &run(value_fixture.path(), &["validate", "."]),
    );
    assert_init_and_plan_succeed(value_fixture.path());

    let module_value_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        module_value_fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let imported = use { source = "./module" }
imported { r = vpc.arn }
"#,
    );
    write_file(
        module_value_fixture.path(),
        "module/main.crn",
        r#"arguments { r: mock.compute.Instance }
mock.test.resource {
  name = "consumer"
  parent = r
  parent_name = "${r}-suffix"
}
"#,
    );
    write_file(
        module_value_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    assert_success(
        "value-shaped module arguments",
        &run(module_value_fixture.path(), &["validate", "."]),
    );
    assert_init_and_plan_succeed(module_value_fixture.path());

    let handle_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        handle_fixture.path(),
        "functions.crn",
        "# body intentionally uses the handle bare\nfn identity(r: mock.compute.Instance): String { r }\n",
    );
    write_file(
        handle_fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
mock.test.resource {
  name = "consumer"
  parent = identity(vpc)
}
"#,
    );
    write_file(
        handle_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    let output = run(handle_fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("functions.crn")
            && text.contains("line 2")
            && text.contains("'r' is a resource handle, not a value"),
        "a handle-shaped function parameter must fail at its bare use:\n{text}",
    );
}

#[test]
fn imported_module_bare_handle_errors_name_the_owning_file() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        "let imported = use { source = \"./module\" }\nimported { n = \"x\" }\n",
    );
    write_file(
        fixture.path(),
        "module/a.crn",
        "arguments { n: String }\nlet vpc = mock.test.resource { name = n }\n",
    );
    write_file(
        fixture.path(),
        "module/b.crn",
        "mock.test.resource {\n  name = \"consumer\"\n  parent = vpc\n}\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("module/b.crn")
            && text.contains("line 3")
            && text.contains("'vpc' is a resource, not a value"),
        "module parse errors must retain their source file:\n{text}",
    );
}

#[test]
fn validate_rejects_structural_kinds_that_depends_on_cannot_honor() {
    for (declaration, binding, kind) in [
        (
            "let xs = for n in [\"a\", \"b\"] { mock.test.resource { name = n } }",
            "xs",
            "for binding",
        ),
        (
            "let src = mock.test.resource { name = \"src\" }\nlet ready = wait src { until = src.name == \"src\" }",
            "ready",
            "wait binding",
        ),
        (
            "let remote = upstream_state { source = \"../remote\" }",
            "remote",
            "upstream state",
        ),
        (
            "let secondary = provider mock {}",
            "secondary",
            "provider instance",
        ),
    ] {
        let fixture = tempfile::tempdir().expect("temporary project");
        write_file(fixture.path(), "decl.crn", &format!("{declaration}\n"));
        write_file(
            fixture.path(),
            "use.crn",
            &format!(
                "mock.test.resource {{\n  name = \"consumer\"\n  directives {{ depends_on = [{binding}] }}\n}}\n"
            ),
        );
        write_file(fixture.path(), "providers.crn", "provider mock {}\n");

        let output = run(fixture.path(), &["validate", "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success()
                && text.contains("use.crn")
                && text.contains("line 3")
                && text.contains(binding)
                && text.contains(kind)
                && text.contains("cannot be used as a dependency target"),
            "depends_on must reject {kind} '{binding}' at the directive:\n{text}",
        );
    }
}

#[test]
fn cli_rejects_non_resource_handles_at_ref_typed_boundaries() {
    for (declaration, call, expected_kind) in [
        (
            "let up = upstream_state { source = \"./upstream\" }",
            "name_of(up)",
            "'up' is an upstream state, not a resource of type mock.compute.Instance",
        ),
        (
            "let imported = use { source = \"./output_module\" }\nlet instance = imported { n = \"x\" }",
            "name_of(instance)",
            "'instance' is a module call, not a resource of type mock.compute.Instance",
        ),
    ] {
        let fixture = tempfile::tempdir().expect("temporary project");
        fs::create_dir(fixture.path().join("upstream")).expect("upstream directory");
        write_file(
            fixture.path(),
            "functions.crn",
            "fn name_of(r: mock.compute.Instance): String { r.name }\n",
        );
        write_file(
            fixture.path(),
            "declarations.crn",
            &format!("{declaration}\n"),
        );
        write_file(
            fixture.path(),
            "consumer.crn",
            &format!("mock.test.resource {{\n  name = \"consumer\"\n  parent = {call}\n}}\n"),
        );
        write_file(
            fixture.path(),
            "output_module/main.crn",
            "arguments { n: String }\nattributes { name: String = n }\n",
        );
        write_file(fixture.path(), "providers.crn", "provider mock {}\n");

        let output = run(fixture.path(), &["validate", "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success()
                && text.contains("consumer.crn")
                && text.contains("line 3")
                && text.contains(expected_kind),
            "a Ref-typed function must reject the non-resource handle:\n{text}",
        );
    }

    for (declaration, argument, expected_kind) in [
        (
            "let up = upstream_state { source = \"./upstream\" }",
            "up",
            "'up' is an upstream state, not a resource of type mock.compute.Instance",
        ),
        (
            "let source = use { source = \"./output_module\" }\nlet instance = source { n = \"x\" }",
            "instance",
            "'instance' is a module call, not a resource of type mock.compute.Instance",
        ),
    ] {
        let fixture = tempfile::tempdir().expect("temporary project");
        fs::create_dir(fixture.path().join("upstream")).expect("upstream directory");
        write_file(
            fixture.path(),
            "main.crn",
            &format!(
                "{declaration}\nlet imported = use {{ source = \"./consumer_module\" }}\nimported {{ r = {argument} }}\n"
            ),
        );
        write_file(
            fixture.path(),
            "consumer_module/main.crn",
            "arguments { r: mock.compute.Instance }\nmock.test.resource { name = r.name }\n",
        );
        write_file(
            fixture.path(),
            "output_module/main.crn",
            "arguments { n: String }\nattributes { name: String = n }\n",
        );
        write_file(fixture.path(), "providers.crn", "provider mock {}\n");

        let output = run(fixture.path(), &["validate", "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success() && text.contains("main.crn") && text.contains(expected_kind),
            "a Ref-typed module argument must reject the non-resource handle:\n{text}",
        );
    }
}

#[test]
fn cli_checks_module_parameter_handles_at_the_nested_function_call_site() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet imported = use { source = \"./module\" }\nimported { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "module/arguments.crn",
        "arguments { r: mock.compute.Instance }\n",
    );
    write_file(
        fixture.path(),
        "module/functions.crn",
        "fn name_of(x: mock.test.resource): String { x.name }\n",
    );
    write_file(
        fixture.path(),
        "module/main.crn",
        "mock.test.resource {\n  name = \"consumer\"\n  parent = name_of(r)\n}\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("module/main.crn")
            && text.contains("line 3")
            && text.contains("expects resource type 'mock.test.resource'")
            && text.contains("got mock.compute.Instance"),
        "the substituted module handle must be checked at the nested call site:\n{text}",
    );
    assert!(
        !text.contains("././"),
        "module paths must be normalized relative to the project root:\n{text}",
    );
}

#[test]
fn cli_evaluates_user_functions_used_as_for_iterables_before_schema_checks() {
    for sibling_function in [false, true] {
        let fixture = tempfile::tempdir().expect("temporary project");
        let function = "fn names(r: mock.compute.Instance): list(String) { [\"a\", \"b\"] }\n";
        if sibling_function {
            write_file(fixture.path(), "functions.crn", function);
            write_file(
                fixture.path(),
                "main.crn",
                "let vpc = mock.compute.Instance { name = \"vpc\" }\nfor n in names(vpc) { mock.test.resource { name = n } }\n",
            );
        } else {
            write_file(
                fixture.path(),
                "main.crn",
                &format!(
                    "{function}let vpc = mock.compute.Instance {{ name = \"vpc\" }}\nfor n in names(vpc) {{ mock.test.resource {{ name = n }} }}\n"
                ),
            );
        }
        write_file(
            fixture.path(),
            "providers.crn",
            "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
        );

        let validate = run(fixture.path(), &["validate", "."]);
        assert_success("function-backed for iterable validation", &validate);
        let plan = assert_init_and_plan_succeed(fixture.path());
        assert!(
            plan.contains("name: 'a'") && plan.contains("name: 'b'"),
            "the user-function result must drive for expansion:\n{plan}",
        );
    }
}

#[test]
fn cli_rejects_builtin_handle_arguments_at_the_authored_call() {
    for expression in ["upper(vpc)", "vpc |> upper()", "{ nested = upper(vpc) }"] {
        let fixture = tempfile::tempdir().expect("temporary project");
        write_file(
            fixture.path(),
            "resource.crn",
            "let vpc = mock.compute.Instance { name = \"vpc\" }\n",
        );
        write_file(
            fixture.path(),
            "consumer.crn",
            &format!("let result = {expression}\n"),
        );
        write_file(fixture.path(), "providers.crn", "provider mock {}\n");

        let output = run(fixture.path(), &["validate", "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success()
                && text.contains("consumer.crn")
                && text.contains("line 1")
                && text.contains("'vpc' is a resource, not a value"),
            "a builtin must reject its handle argument at the call site:\n{text}",
        );
        assert_eq!(
            text.matches("'vpc' is a resource, not a value").count(),
            1,
            "the builtin error must be emitted once:\n{text}",
        );
        assert!(
            !text.contains("upper(): upper:"),
            "the builtin name must not be doubled:\n{text}",
        );
    }
}

#[test]
fn cli_accepts_module_parameter_dependency_targets_in_root_and_module_scope() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet imported = use { source = \"./module\" }\nimported { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "module/main.crn",
        "arguments { r: mock.compute.Instance }\nmock.test.resource {\n  name = \"consumer\"\n  parent = r.name\n  directives { depends_on = [r] }\n}\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    assert_success(
        "root validation of module-parameter depends_on",
        &run(fixture.path(), &["validate", "."]),
    );
    assert_success(
        "module-directory validation of module-parameter depends_on",
        &run(&fixture.path().join("module"), &["validate", "."]),
    );
}

#[test]
fn cli_locates_deferred_function_type_errors_at_the_sibling_call() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "functions.crn",
        "fn name_of(r: mock.compute.Instance): String { r.name }\n",
    );
    write_file(
        fixture.path(),
        "bindings.crn",
        "let wrong = mock.test.resource { name = \"wrong\" }\n",
    );
    write_file(
        fixture.path(),
        "consumer.crn",
        "mock.test.resource {\n  name = \"consumer\"\n  parent = name_of(wrong)\n}\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("./consumer.crn")
            && text.contains("line 3")
            && text.contains("expects resource type 'mock.compute.Instance'")
            && text.contains("got mock.test.resource"),
        "the deferred check must retain the call site's file and line:\n{text}",
    );
}

#[test]
fn root_parse_warnings_keep_the_existing_dot_slash_path_prefix() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        "let literal = '${not_interpolated}'\n",
    );

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert_success("warning-only validation", &output);
    assert!(
        text.contains("./main.crn:1") && text.contains("single-quoted string contains '${...}'"),
        "root warning path formatting changed:\n{text}",
    );
}

#[test]
fn imported_module_errors_use_one_normalized_located_path_form() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet imported = use { source = \"./mod\" }\nimported { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "mod/main.crn",
        "arguments { r: mock.compute.Instance }\nfn thing_name(x: mock.test.Thing): String { x.name }\nmock.test.resource { name = thing_name(r) }\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success(),
        "validation unexpectedly passed:\n{text}"
    );
    assert!(
        text.contains(
            "Module resolution error: Parse error in mod/main.crn: Invalid expression at line 3"
        ),
        "module errors must use the canonical located form:\n{text}",
    );
    assert!(
        !text.contains("Parse error: mod/main.crn") && !text.contains("././"),
        "legacy/doubled path forms leaked:\n{text}",
    );
}

#[test]
fn module_constraints_substitute_handle_attribute_access_before_apply() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let checked = use { source = "./checked" }
checked { r = vpc }
"#,
    );
    write_file(
        fixture.path(),
        "checked/arguments.crn",
        r#"arguments {
  r: mock.compute.Instance {
    validation {
      condition = r.name == "vpc"
      error_message = "wrong validation resource name"
    }
  }
}
"#,
    );
    write_file(
        fixture.path(),
        "checked/constraints.crn",
        "require r.name == \"vpc\", \"wrong resource name\"\n",
    );
    write_file(
        fixture.path(),
        "checked/resource.crn",
        "mock.test.resource { name = \"consumer\" parent = r.name }\n",
    );
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    let apply = run(fixture.path(), &["apply", "--auto-approve", "."]);
    assert_success("handle-backed module constraint apply", &apply);
}

#[test]
fn nested_module_constraints_substitute_forwarded_handle_attribute_access() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let outer = use { source = "./outer" }
outer { r = vpc }
"#,
    );
    write_file(
        fixture.path(),
        "outer/main.crn",
        r#"arguments { r: mock.compute.Instance }
let checked = use { source = "../checked" }
checked { inner = r }
"#,
    );
    write_file(
        fixture.path(),
        "checked/main.crn",
        r#"arguments { inner: mock.compute.Instance }
require inner.name == "vpc", "wrong nested resource name"
mock.test.resource { name = "consumer" parent = inner.name }
"#,
    );
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    let apply = run(fixture.path(), &["apply", "--auto-approve", "."]);
    assert_success("nested handle-backed module constraint apply", &apply);
}

#[test]
fn nested_handle_constraint_violation_is_rejected_before_apply() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let outer = use { source = "./outer" }
outer { r = vpc }
"#,
    );
    write_file(
        fixture.path(),
        "outer/main.crn",
        r#"arguments { r: mock.compute.Instance }
let checked = use { source = "../checked" }
checked { inner = r }
"#,
    );
    write_file(
        fixture.path(),
        "checked/main.crn",
        r#"arguments { inner: mock.compute.Instance }
require inner.name == "nope", "wrong nested resource name"
mock.test.resource { name = "consumer" parent = inner.name }
"#,
    );
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    for command in ["validate", "plan"] {
        let output = run(fixture.path(), &[command, "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success() && text.contains("wrong nested resource name"),
            "a nested handle constraint violation must fail during {command}:\n{text}",
        );
    }
}

#[test]
fn module_constraints_reject_false_or_bare_handle_operands_before_apply() {
    let false_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        false_fixture.path(),
        "main.crn",
        r#"let vpc = mock.compute.Instance { name = "vpc" }
let checked = use { source = "./checked" }
checked { r = vpc }
"#,
    );
    write_file(
        false_fixture.path(),
        "checked/arguments.crn",
        "arguments { r: mock.compute.Instance }\n",
    );
    write_file(
        false_fixture.path(),
        "checked/constraints.crn",
        "require r.name == \"nope\", \"wrong resource name\"\n",
    );
    write_file(
        false_fixture.path(),
        "checked/resource.crn",
        "mock.test.resource { name = \"consumer\" parent = r.name }\n",
    );
    write_file(
        false_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );

    for command in ["validate", "plan"] {
        let output = run(false_fixture.path(), &[command, "."]);
        let text = output_text(&output);
        assert!(
            !output.status.success() && text.contains("wrong resource name"),
            "a statically knowable false handle constraint must fail during {command}, before apply:\n{text}",
        );
    }

    for (arguments, constraint, expression, source_file, line) in [
        (
            "arguments { r: mock.compute.Instance }\n",
            "require r == \"x\", \"bare handle\"\n",
            "r == \"x\"",
            "checked/constraints.crn",
            1,
        ),
        (
            "arguments { r: mock.compute.Instance }\n",
            "require\n  length(r) > 0,\n  \"bare handle\"\n",
            "length(r)",
            "checked/constraints.crn",
            2,
        ),
        (
            r#"arguments {
  r: mock.compute.Instance {
    validation {
      condition = length(r) > 0
      error_message = "bare handle"
    }
  }
}
"#,
            "",
            "validation length(r)",
            "checked/arguments.crn",
            4,
        ),
    ] {
        let fixture = tempfile::tempdir().expect("temporary project");
        write_file(
            fixture.path(),
            "main.crn",
            r#"let vpc = mock.compute.Instance { name = "vpc" }
let checked = use { source = "./checked" }
checked { r = vpc }
"#,
        );
        write_file(fixture.path(), "checked/arguments.crn", arguments);
        write_file(fixture.path(), "checked/constraints.crn", constraint);
        write_file(fixture.path(), "providers.crn", "provider mock {}\n");

        let validate = run(fixture.path(), &["validate", "."]);
        let text = output_text(&validate);
        assert!(
            !validate.status.success()
                && text.contains(source_file)
                && text.contains(&format!("line {line}"))
                && text.contains("'r' is a resource handle, not a value"),
            "a bare handle in {expression:?} must fail at the constraint use site:\n{text}",
        );
    }
}

#[test]
fn sibling_handle_if_condition_has_the_same_located_error_as_same_file() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "a.crn",
        "let vpc = mock.compute.Instance { name = \"vpc\" }\n",
    );
    write_file(
        fixture.path(),
        "b.crn",
        "let selected = if vpc { mock.test.resource { name = \"yes\" } } else { mock.test.resource { name = \"no\" } }\n",
    );
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("./b.crn")
            && text.contains("line 1")
            && text.contains("'vpc' is a resource, not a value"),
        "the sibling condition must retain its typed handle and source location:\n{text}",
    );
    assert!(!text.contains("line 0"), "line zero leaked:\n{text}");
}

#[test]
fn deferred_module_user_functions_resolve_before_apply() {
    let output_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        output_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    write_file(
        output_fixture.path(),
        "main.crn",
        r#"let vpc = mock.test.resource { name = "vpc" }
let imported = use { source = "./module" }
let instance = imported { r = vpc.name }
mock.test.resource { name = "consumer" parent = instance.o }
"#,
    );
    write_file(
        output_fixture.path(),
        "module/main.crn",
        r#"arguments { r: String }
fn suffix(value: String): String { "${value}-x" }
fn name_with_suffix(value: String): String { suffix(value) }
attributes { o: String = name_with_suffix(r) }
mock.test.resource { name = "module-resource" }
"#,
    );

    assert_success(
        "module-output user-function validation",
        &run(output_fixture.path(), &["validate", "."]),
    );
    assert_success("carina init", &run(output_fixture.path(), &["init", "."]));
    assert_success(
        "module-output user-function apply",
        &run(output_fixture.path(), &["apply", "--auto-approve", "."]),
    );

    let nested_fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        nested_fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    write_file(
        nested_fixture.path(),
        "main.crn",
        "let outer = use { source = \"./outer\" }\nouter { r = \"literal\" }\n",
    );
    write_file(
        nested_fixture.path(),
        "outer/main.crn",
        "arguments { r: String }\nlet inner = use { source = \"../inner\" }\ninner { x = r }\n",
    );
    write_file(
        nested_fixture.path(),
        "inner/main.crn",
        r#"arguments { x: String }
fn suffix(value: String): String { "${value}-x" }
fn name_with_suffix(value: String): String { suffix(value) }
mock.test.resource { name = "consumer" parent = name_with_suffix(x) }
"#,
    );

    assert_success(
        "nested user-function validation",
        &run(nested_fixture.path(), &["validate", "."]),
    );
    assert_success("carina init", &run(nested_fixture.path(), &["init", "."]));
    assert_success(
        "nested user-function apply",
        &run(nested_fixture.path(), &["apply", "--auto-approve", "."]),
    );
}

#[test]
fn deferred_module_handle_user_function_resolves_before_apply() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.test.resource { name = "vpc" }
let imported = use { source = "./module" }
let instance = imported { r = vpc }
mock.test.resource { name = "consumer" parent = instance.o }
"#,
    );
    write_file(
        fixture.path(),
        "module/main.crn",
        r#"arguments { r: mock.test.resource }
fn suffix(value: String): String { "${value}-x" }
fn name_with_suffix(value: mock.test.resource): String { suffix(value.name) }
attributes { o: String = name_with_suffix(r) }
mock.test.resource { name = "module-resource" }
"#,
    );

    assert_success(
        "handle-valued module-output function validation",
        &run(fixture.path(), &["validate", "."]),
    );
    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    assert_success(
        "handle-valued module-output function apply",
        &run(fixture.path(), &["apply", "--auto-approve", "."]),
    );
}

#[test]
fn deferred_module_user_function_body_errors_during_validate() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");
    write_file(
        fixture.path(),
        "main.crn",
        r#"let vpc = mock.test.resource { name = "vpc" }
let imported = use { source = "./module" }
let instance = imported { r = vpc.name }
mock.test.resource { name = "consumer" parent = instance.o }
"#,
    );
    write_file(
        fixture.path(),
        "module/main.crn",
        r#"arguments { r: String }
fn name_with_suffix(value: String): String { missing_helper(value) }
attributes { o: String = name_with_suffix(r) }
"#,
    );

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success(),
        "validate must reject a bad deferred function body before plan:\n{text}",
    );
    assert!(
        text.contains("missing_helper")
            && text.contains("module/main.crn")
            && text.contains("line 2"),
        "the validation error must retain the function-body location:\n{text}",
    );
}

#[test]
fn nested_forwarded_handle_remains_a_dependency_operand() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.test.resource { name = \"vpc\" }\nlet outer = use { source = \"./outer\" }\nouter { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "outer/main.crn",
        "arguments { r: mock.test.resource }\nlet inner = use { source = \"../inner\" }\ninner { x = r }\n",
    );
    write_file(
        fixture.path(),
        "inner/main.crn",
        "arguments { x: mock.test.resource }\nmock.test.resource {\n  name = \"consumer\"\n  directives { depends_on = [x] }\n}\n",
    );

    assert_success(
        "nested forwarded dependency validation",
        &run(fixture.path(), &["validate", "."]),
    );
    assert_success(
        "standalone outer-module validation",
        &run(&fixture.path().join("outer"), &["validate", "."]),
    );
    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    assert_success(
        "nested forwarded dependency apply",
        &run(fixture.path(), &["apply", "--auto-approve", "."]),
    );
}

#[test]
fn nested_bare_handle_error_uses_the_inner_operand_location() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.test.resource { name = \"vpc\" }\nlet outer = use { source = \"./outer\" }\nouter { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "outer/main.crn",
        "arguments { r: mock.test.resource }\nlet inner = use { source = \"../inner\" }\n\n\ninner { x = r }\n",
    );
    write_file(
        fixture.path(),
        "inner/main.crn",
        "arguments { x: mock.test.resource }\n\n\n\n\nmock.test.resource {\n  name = \"consumer\"\n  parent = x\n}\n",
    );

    let output = run(fixture.path(), &["validate", "."]);
    let text = output_text(&output);
    assert!(
        !output.status.success()
            && text.contains("inner/main.crn")
            && text.contains("line 8")
            && text.contains("'x' is a resource handle, not a value"),
        "the inner bare use must own the complete diagnostic:\n{text}",
    );
    assert!(
        !text.contains("outer/../inner") && !text.contains("line 5: 'r'"),
        "mixed nested-module provenance leaked into the diagnostic:\n{text}",
    );
}

#[test]
fn deferred_handle_constraint_reports_the_selected_operand() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(
        fixture.path(),
        "providers.crn",
        "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
    );
    write_file(
        fixture.path(),
        "main.crn",
        "let vpc = mock.test.resource { name = \"vpc\" }\nlet checked = use { source = \"./checked\" }\nchecked { r = vpc }\n",
    );
    write_file(
        fixture.path(),
        "checked/main.crn",
        "arguments { r: mock.test.resource }\nrequire r.identifier == \"nope\", \"bad identifier\"\nmock.test.resource { name = \"consumer\" parent = r.identifier }\n",
    );

    assert_success("carina init", &run(fixture.path(), &["init", "."]));
    let apply = run(fixture.path(), &["apply", "--auto-approve", "."]);
    let text = output_text(&apply);
    assert!(
        !apply.status.success()
            && text.contains("bad identifier")
            && text.contains("r.identifier ="),
        "the deferred failure must render the selected operand:\n{text}",
    );
    assert!(
        !text.contains("got r = vpc"),
        "the handle binding must not replace the failing operand:\n{text}",
    );
}

#[test]
fn validation_subscript_uses_the_regular_expression_parser() {
    let fixture = tempfile::tempdir().expect("temporary project");
    write_file(fixture.path(), "providers.crn", "provider mock {}\n");
    write_file(
        fixture.path(),
        "main.crn",
        "let checked = use { source = \"./checked\" }\nchecked { values = { \"ONE\" = \"one\" } }\n",
    );
    write_file(
        fixture.path(),
        "checked/main.crn",
        "arguments { values: map(String) }\nrequire values[(\"ONE\")] == \"one\", \"wrong element\"\nmock.test.resource { name = \"consumer\" }\n",
    );

    assert_success(
        "validation expression subscript",
        &run(fixture.path(), &["validate", "."]),
    );
}
