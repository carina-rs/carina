//! Lifecycle directives must be decided after module argument substitution.

use std::fs;
use std::process::{Command, Output};

struct Scenario(tempfile::TempDir);

impl Scenario {
    fn new() -> Self {
        let scenario = Self(tempfile::tempdir().unwrap());
        scenario.write(
            "main.crn",
            "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
        );
        scenario
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn module(&self, directive: &str, value: &str, name: &str) {
        self.write(
            "module/main.crn",
            &format!(
                "arguments {{ flag: Bool }}\nlet r = mock.test.resource {{\n  name = '{name}'\n  directives {{\n    {directive} = flag\n  }}\n}}\n"
            ),
        );
        self.write(
            "call.crn",
            &format!(
                "let m = use {{ source = './module' }}\nlet instance = m {{ flag = {value} }}\n"
            ),
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.0.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", self.0.path().join("mock.json"))
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {}", rendered(&output));
        output
    }

    fn rejects_default_opt_out_once(&self, arguments: &[&str]) {
        for command in ["validate", "plan"] {
            let output = self.run(&[command, "."]);
            let text = rendered(&output);
            assert!(!output.status.success(), "default origin was lost: {text}");
            assert_eq!(text.matches("false came from").count(), 1, "{text}");
            assert_eq!(
                text.matches("the default of argument").count(),
                arguments.len(),
                "{text}"
            );
            for argument in arguments {
                assert!(
                    text.contains(&format!("`{argument}`")),
                    "missing default {argument}: {text}"
                );
            }
            let diagnostic = text
                .lines()
                .find(|line| line.contains("false came from"))
                .unwrap();
            assert!(!diagnostic.contains("called at"), "{diagnostic}");
            let location = diagnostic
                .find("call.crn:2:")
                .expect("the omitted call must be located");
            assert!(
                location < diagnostic.find("directives.prevent_destroy").unwrap(),
                "diagnostic must start at the omitted call: {text}"
            );
        }
    }
}

fn nested_expression_scenario(expression: &str) -> Scenario {
    let scenario = Scenario::new();
    scenario.write("inner/main.crn", "arguments { protect: Bool }\nlet r = mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = protect } }\n");
    scenario.write("outer/main.crn", &format!("arguments {{ keep: Bool = false }}\nfn identity(value: Bool): Bool {{ value }}\nlet inner = use {{ source = '../inner' }}\nlet child = inner {{ protect = {expression} }}\n"));
    scenario.write(
        "call.crn",
        "let outer = use { source = './outer' }\nlet instance = outer {}\n",
    );
    scenario.success(&["init", "."]);
    scenario
}

fn check_nested_expression_origins(expression: &str) {
    let scenario = nested_expression_scenario(expression);
    scenario.rejects_default_opt_out_once(&["keep"]);
    scenario.write(
        "call.crn",
        "let outer = use { source = './outer' }\nlet instance = outer { keep = false }\n",
    );
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
}

#[test]
fn default_origin_survives_nested_conditional_arguments() {
    check_nested_expression_origins("if keep { true } else { false }");
}

#[test]
fn default_origin_survives_nested_coalesce_arguments() {
    check_nested_expression_origins("keep ?? true");
}

#[test]
fn default_origin_survives_nested_user_function_arguments() {
    check_nested_expression_origins("identity(keep)");
}

#[test]
fn default_condition_does_not_taint_literal_module_arguments() {
    let scenario = nested_expression_scenario("false");
    scenario.write("outer/main.crn", "arguments { keep: Bool = false }\nlet inner = use { source = '../inner' }\nlet child = if keep { inner { protect = true } } else { inner { protect = false } }\n");
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
}

#[test]
fn default_condition_does_not_taint_literal_resource_directives() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { enabled: Bool = true }\nlet child = if enabled { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = false } } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
}

#[test]
fn default_iterable_origin_only_reaches_loop_dependent_directives() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { flags: list(Bool) = [false] }\nlet children = for flag in flags { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = false } } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
    scenario.write("module/main.crn", "arguments { flags: list(Bool) = [false] }\nlet children = for flag in flags { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = flag } } }\n");
    scenario.rejects_default_opt_out_once(&["flags"]);
}

#[test]
fn explicit_iterable_does_not_erase_defaulted_directive_origin() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { flags: list(Bool) protect: Bool = false }\nlet children = for flag in flags { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = lookup({ keep = protect }, 'keep', flag) } } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m { flags = [true] }\n",
    );
    scenario.success(&["init", "."]);
    scenario.rejects_default_opt_out_once(&["protect"]);
    scenario.write("module/main.crn", "arguments { flags: list(Bool) = [true] protect: Bool = false }\nlet children = for flag in flags { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = lookup({ keep = protect }, 'keep', flag) } } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.rejects_default_opt_out_once(&["flags", "protect"]);
}

#[test]
fn defaulted_map_values_require_explicit_opt_out() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { flags: map(Bool) = { r = false } }\nlet children = for name, flag in flags { mock.test.default_prevent_destroy { name = name directives { prevent_destroy = flag } } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.rejects_default_opt_out_once(&["flags"]);
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m { flags = { r = false } }\n",
    );
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", "."]);
}

#[test]
fn all_contributing_defaults_are_reported_together() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { first: Bool = false second: Bool = false }\nlet r = mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = lookup({ keep = first }, 'keep', second) } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.rejects_default_opt_out_once(&["first", "second"]);
}

#[test]
fn argument_default_references_retain_both_origins() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { first: Bool = false second: Bool = first }\nlet r = mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = second } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.rejects_default_opt_out_once(&["first", "second"]);
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m { first = false }\n",
    );
    scenario.rejects_default_opt_out_once(&["second"]);
}

#[test]
fn saved_defaulted_loop_opt_out_is_rejected_after_iterable_materializes() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { protect: Bool = false options: list(struct { key: String, value: String }) }\nlet children = for option in options { mock.test.default_prevent_destroy { name = option.key directives { prevent_destroy = lookup({ unused = true }, option.key, protect) } } }\n");
    scenario.write("call.crn", "let source = mock.test.server_added_list { name = 'source' }\nlet m = use { source = './module' }\nlet instance = m { options = source.target_group_attributes }\n");
    scenario.success(&["init", "."]);
    scenario.success(&["plan", ".", "--out", "deferred.plan"]);
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("deferred.plan")).unwrap())
            .unwrap();
    assert_eq!(
        saved["version"],
        carina_cli::commands::plan::PlanFile::CURRENT_VERSION
    );
    assert!(
        saved["plan"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| effect.get("DeferredCreate").is_some()),
        "{saved:#?}"
    );
    let output = scenario.run(&["apply", "deferred.plan", "--auto-approve", "--lock=false"]);
    let text = rendered(&output);
    assert!(
        !output.status.success(),
        "saved default-origin protection failed open: {text}"
    );
    assert_eq!(text.matches("false came from").count(), 1, "{text}");
    assert!(
        text.contains("deferred-for directive resolution failed"),
        "{text}"
    );
    assert!(
        text.contains("`protect`") && text.contains("call.crn:3:"),
        "{text}"
    );
    let provider: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("mock.json")).unwrap()).unwrap();
    assert_eq!(
        provider["states"].as_object().unwrap().len(),
        1,
        "no protected child may be created: {provider:#?}"
    );
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn module_argument_prevent_destroy_refuses_destroy() {
    let scenario = Scenario::new();
    scenario.module("prevent_destroy", "true", "keep");
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    let output = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
    assert!(
        !output.status.success(),
        "protected resource was deleted: {}",
        rendered(&output)
    );
    assert!(
        rendered(&output).contains("prevent_destroy"),
        "{}",
        rendered(&output)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("carina.state.json")).unwrap())
            .unwrap();
    assert_eq!(state["resources"].as_array().unwrap().len(), 1);
    assert_eq!(
        state["resources"][0]["directives"]["prevent_destroy"],
        serde_json::json!({"kind": "explicit", "value": true})
    );
}

#[test]
fn module_argument_create_before_destroy_orders_replacement() {
    let scenario = Scenario::new();
    scenario.module("create_before_destroy", "true", "old");
    scenario.success(&["init", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    scenario.module("create_before_destroy", "true", "new");
    let output = scenario.success(&["plan", ".", "--out", "replacement.json"]);
    assert!(
        rendered(&output).contains("create before destroy"),
        "{}",
        rendered(&output)
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("replacement.json")).unwrap())
            .unwrap();
    let effects = saved["plan"]["effects"].as_array().unwrap();
    assert!(effects[0].get("Create").is_some(), "{effects:#?}");
    assert!(effects[1].get("Delete").is_some(), "{effects:#?}");
    assert_eq!(
        saved["sorted_resources"][0]["directives"]["create_before_destroy"],
        true
    );
    assert_eq!(
        saved["sorted_resources"][0]["directives"]["prevent_destroy"],
        serde_json::json!({"kind": "unspecified"})
    );
    scenario.success(&[
        "apply",
        "replacement.json",
        "--auto-approve",
        "--lock=false",
    ]);
}

#[test]
fn unresolved_or_non_bool_directives_are_validation_errors() {
    for directive in ["prevent_destroy", "create_before_destroy", "force_delete"] {
        for expression in ["source.name", "'true'", "1"] {
            let scenario = Scenario::new();
            scenario.write("resources.crn", &format!("let source = mock.test.resource {{ name = 'source' }}\nlet r = mock.test.resource {{\n  name = 'target'\n  directives {{\n    {directive} = {expression}\n  }}\n}}\n"));
            scenario.success(&["init", "."]);
            let output = scenario.run(&["validate", "."]);
            let text = rendered(&output);
            assert!(
                !output.status.success(),
                "{directive} = {expression} was accepted: {text}"
            );
            assert!(
                text.contains(directive) && (text.contains("line 5") || text.contains(":5:")),
                "missing directive/location: {text}"
            );
        }
    }
}

#[test]
fn literal_and_folded_false_opt_out_of_schema_default() {
    for expression in ["false", "protect"] {
        let scenario = Scenario::new();
        scenario.write("flags.crn", "let protect = false\n");
        scenario.write("resource.crn", &format!("let r = mock.test.default_prevent_destroy {{ name = 'delete-me' directives {{ prevent_destroy = {expression} }} }}\n"));
        scenario.success(&["init", "."]);
        scenario.success(&["validate", "."]);
        scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
        scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
    }
}

#[test]
fn module_non_bool_directives_report_the_module_source() {
    for directive in ["prevent_destroy", "create_before_destroy", "force_delete"] {
        let scenario = Scenario::new();
        scenario.module(directive, "'true'", "invalid");
        let path = scenario.0.path().join("module/main.crn");
        let source = fs::read_to_string(&path)
            .unwrap()
            .replace("flag: Bool", "flag: String");
        fs::write(&path, source).unwrap();
        scenario.success(&["init", "."]);
        let output = scenario.run(&["validate", "."]);
        let text = rendered(&output);
        assert!(!output.status.success(), "{text}");
        assert!(
            text.contains(directive) && text.contains("module/main.crn:5:"),
            "{text}"
        );
    }
}

#[test]
fn unknown_upstream_directives_are_validation_errors() {
    for directive in ["prevent_destroy", "create_before_destroy", "force_delete"] {
        let scenario = Scenario::new();
        scenario.write(
            "upstream/main.crn",
            "backend local { path = 'carina.state.json' }\nexports { protect = true }\n",
        );
        scenario.write(
            "upstream.crn",
            "let network = upstream_state { source = './upstream' }\n",
        );
        scenario.write("resources.crn", &format!("let r = mock.test.resource {{\n  name = 'r'\n  directives {{\n    {directive} = network.protect\n  }}\n}}\n"));
        scenario.success(&["init", "."]);
        let output = scenario.run(&["validate", "."]);
        let text = rendered(&output);
        assert!(!output.status.success(), "{text}");
        assert!(
            text.contains(directive) && text.contains("resources.crn:4:"),
            "{text}"
        );
    }
}

#[test]
fn repeated_directive_expression_is_reported_once_per_command() {
    let scenario = Scenario::new();
    scenario.write("resources.crn", "for name in ['one', 'two'] { mock.test.resource { name = name directives { prevent_destroy = 1 } } }\n");
    scenario.success(&["init", "."]);
    for command in ["validate", "plan"] {
        let output = scenario.run(&[command, "."]);
        let text = rendered(&output);
        assert!(!output.status.success(), "{text}");
        assert_eq!(
            text.matches("directives.prevent_destroy must resolve")
                .count(),
            1,
            "{text}"
        );
    }
}

#[test]
fn standalone_module_loop_directives_validate_before_instantiation() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { flags: list(Bool) }\nfor flag in flags { mock.test.resource { name = 'r' directives { prevent_destroy = flag force_delete = flag create_before_destroy = flag } } }\n");
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "module"]);
}

#[test]
fn protected_module_argument_defaults_require_explicit_opt_out() {
    for nested in [false, true] {
        let scenario = Scenario::new();
        scenario.write("module/main.crn", "arguments { protect: Bool = false }\nlet r = mock.test.default_prevent_destroy { name = 'protected' directives { prevent_destroy = protect } }\n");
        let source = if nested {
            scenario.write("outer/main.crn", "arguments { protect: Bool = false }\nlet inner = use { source = '../module' }\nlet child = inner { protect = protect }\n");
            "./outer"
        } else {
            "./module"
        };
        scenario.write(
            "call.crn",
            &format!("let m = use {{ source = '{source}' }}\nlet instance = m {{}}\n"),
        );
        scenario.success(&["init", "."]);
        for command in ["validate", "plan"] {
            let output = scenario.run(&[command, "."]);
            let text = rendered(&output);
            assert!(!output.status.success(), "default false opted out: {text}");
            assert!(
                text.contains("default of argument `protect`") && text.contains(source),
                "{text}"
            );
            assert!(
                text.contains("protect = false")
                    && text.contains("explicitly")
                    && text.contains("call.crn:2:"),
                "{text}"
            );
        }
    }
}

#[test]
fn protected_loop_argument_defaults_are_checked_during_validate() {
    for explicit in [false, true] {
        let scenario = Scenario::new();
        scenario.write("module/main.crn", "arguments { flags: list(Bool) = [false] }\nlet children = for flag in flags { mock.test.default_prevent_destroy { name = 'r' directives { prevent_destroy = flag } } }\n");
        let arguments = if explicit { "flags = [false]" } else { "" };
        scenario.write(
            "call.crn",
            &format!("let m = use {{ source = './module' }}\nlet instance = m {{ {arguments} }}\n"),
        );
        scenario.success(&["init", "."]);
        let output = scenario.run(&["validate", "."]);
        let text = rendered(&output);
        assert_eq!(output.status.success(), explicit, "{text}");
        if explicit {
            scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
            scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
        } else {
            assert!(
                text.contains("default of argument `flags`") && text.contains("call.crn:2:"),
                "{text}"
            );
        }
    }
}

#[test]
fn explicit_module_argument_false_can_opt_out_of_schema_protection() {
    for nested in [false, true] {
        let scenario = Scenario::new();
        scenario.write("module/main.crn", "arguments { protect: Bool = false }\nlet r = mock.test.default_prevent_destroy { name = 'protected' directives { prevent_destroy = protect } }\n");
        let source = if nested {
            scenario.write("outer/main.crn", "arguments { protect: Bool = false }\nlet inner = use { source = '../module' }\nlet child = inner { protect = protect }\n");
            "./outer"
        } else {
            "./module"
        };
        scenario.write(
            "call.crn",
            &format!(
                "let m = use {{ source = '{source}' }}\nlet instance = m {{ protect = false }}\n"
            ),
        );
        scenario.success(&["init", "."]);
        scenario.success(&["validate", "."]);
        scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
        scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
    }
}

#[test]
fn inner_literal_false_and_unprotected_defaults_remain_allowed() {
    for protected in [false, true] {
        let scenario = Scenario::new();
        let kind = if protected {
            "default_prevent_destroy"
        } else {
            "resource"
        };
        scenario.write("module/main.crn", &format!("arguments {{ protect: Bool = false }}\nlet r = mock.test.{kind} {{ name = 'r' directives {{ prevent_destroy = protect force_delete = protect create_before_destroy = protect }} }}\n"));
        let source = if protected {
            scenario.write("outer/main.crn", "arguments { protect: Bool = false }\nlet inner = use { source = '../module' }\nlet child = inner { protect = false }\n");
            "./outer"
        } else {
            "./module"
        };
        scenario.write(
            "call.crn",
            &format!("let m = use {{ source = '{source}' }}\nlet instance = m {{}}\n"),
        );
        scenario.success(&["init", "."]);
        scenario.success(&["validate", "."]);
        scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
        scenario.success(&["destroy", ".", "--auto-approve", "--lock=false"]);
    }
}

#[test]
fn other_directive_defaults_do_not_disable_schema_protection() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { flag: Bool = false }\nlet r = mock.test.default_prevent_destroy { name = 'r' directives { force_delete = flag create_before_destroy = flag } }\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m {}\n",
    );
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "."]);
    scenario.success(&["apply", ".", "--auto-approve", "--lock=false"]);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("carina.state.json")).unwrap())
            .unwrap();
    let directives = &state["resources"][0]["directives"];
    assert_eq!(directives["force_delete"], false);
    assert_eq!(directives["create_before_destroy"], false);
    assert_eq!(
        directives["prevent_destroy"],
        serde_json::json!({"kind": "unspecified"})
    );
    let output = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
    assert!(!output.status.success(), "{}", rendered(&output));
    assert!(
        rendered(&output).contains("prevent_destroy"),
        "{}",
        rendered(&output)
    );
}

#[test]
fn standalone_module_directive_exemptions_require_bare_bool_values() {
    for (arguments, expression, loop_header) in [
        ("protect: Bool", "\"${protect}\"", ""),
        ("protect: Bool", "length([protect])", ""),
        ("protect: Bool", "protect ?? false", ""),
        ("protect: Bool", "if protect { true } else { false }", ""),
        (
            "flags: list(Bool)",
            "lookup({ keep = flag }, 'keep', false)",
            "for flag in flags",
        ),
        ("names: list(String)", "flag", "for flag in names"),
    ] {
        let scenario = Scenario::new();
        scenario.write(
            "module/arguments.crn",
            &format!("arguments {{ {arguments} }}\n"),
        );
        let body = format!(
            "mock.test.resource {{ name = 'r' directives {{ prevent_destroy = {expression} }} }}"
        );
        let source = if loop_header.is_empty() {
            body
        } else {
            format!("{loop_header} {{ {body} }}")
        };
        scenario.write("module/resources.crn", &source);
        scenario.success(&["init", "."]);
        let output = scenario.run(&["validate", "module"]);
        let text = rendered(&output);
        assert!(!output.status.success(), "{expression} accepted: {text}");
        assert!(
            text.contains("directives.prevent_destroy") && text.contains("resources.crn:1:"),
            "{text}"
        );
        let expected = if arguments == "names: list(String)" {
            "got String"
        } else {
            "cannot be checked until the module is instantiated"
        };
        assert!(text.contains(expected), "{text}");
        assert!(!text.contains("unknown until apply"), "{text}");
    }
}

#[test]
fn non_local_loop_directives_explain_the_element_restriction() {
    for expression in [
        "flag ?? false",
        "lookup({ keep = flag }, 'keep', peer.identifier)",
    ] {
        let scenario = Scenario::new();
        scenario.write("module/main.crn", &format!("arguments {{ flags: list(Bool) }}\nlet children = for flag in flags {{\n  mock.test.resource {{ name = 'child' directives {{ prevent_destroy = {expression} }} }}\n}}\nlet peer = mock.test.resource {{ name = 'peer' }}\n"));
        scenario.write(
            "call.crn",
            "let m = use { source = './module' }\nlet instance = m { flags = [true] }\n",
        );
        scenario.success(&["init", "."]);
        for command in ["validate", "plan"] {
            let output = scenario.run(&[command, "."]);
            let text = rendered(&output);
            assert!(!output.status.success(), "{text}");
            assert!(
                text.contains("main.crn:3:") && text.contains("directives.prevent_destroy"),
                "{text}"
            );
            assert!(
                text.contains("loop directive must depend only on the current element"),
                "{text}"
            );
            assert!(!text.contains("unknown until apply"), "{text}");
        }
    }
}

#[test]
fn module_argument_loop_directives_resolve_per_element() {
    for flag in [true, false] {
        let scenario = Scenario::new();
        scenario.write("module/arguments.crn", "arguments { flags: list(Bool) }\n");
        scenario.write("module/resources.crn", "let children = for flag in flags {\n  mock.test.resource { name = 'child' directives { prevent_destroy = flag force_delete = flag create_before_destroy = flag } }\n}\n");
        scenario.write(
            "call.crn",
            &format!(
                "let m = use {{ source = './module' }}\nlet instance = m {{ flags = [{flag}] }}\n"
            ),
        );
        scenario.success(&["init", "."]);
        scenario.success(&["validate", "."]);
        scenario.success(&["plan", ".", "--out", "loop.plan"]);
        scenario.success(&["apply", "loop.plan", "--auto-approve", "--lock=false"]);
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(scenario.0.path().join("carina.state.json")).unwrap())
                .unwrap();
        let directives = &state["resources"][0]["directives"];
        assert_eq!(
            directives["prevent_destroy"],
            serde_json::json!({"kind": "explicit", "value": flag})
        );
        assert_eq!(directives["force_delete"], flag);
        assert_eq!(directives["create_before_destroy"], flag);
        let output = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
        assert_eq!(output.status.success(), !flag, "{}", rendered(&output));
    }
}

#[test]
fn module_loop_directive_functions_resolve_after_substitution() {
    let scenario = Scenario::new();
    scenario.write("module/arguments.crn", "arguments { flags: list(Bool) }\n");
    scenario.write("module/resources.crn", "let children = for flag in flags {\n  mock.test.resource { name = 'child' directives { prevent_destroy = lookup({ keep = flag }, 'keep', false) } }\n}\n");
    scenario.write(
        "call.crn",
        "let m = use { source = './module' }\nlet instance = m { flags = [true] }\n",
    );
    scenario.success(&["init", "."]);
    scenario.success(&["plan", ".", "--out", "loop.plan"]);
    scenario.success(&["apply", "loop.plan", "--auto-approve", "--lock=false"]);
    let output = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
    assert!(!output.status.success(), "{}", rendered(&output));
    assert!(
        rendered(&output).contains("prevent_destroy"),
        "{}",
        rendered(&output)
    );
}

#[test]
fn resource_dependent_loop_directives_resolve_per_element() {
    let scenario = Scenario::new();
    scenario.write(
        "source.crn",
        "let source = mock.test.resource { name = 'source' }\n",
    );
    scenario.write("children.crn", "let children = for child, flag in (source.tags ?? { child = true }) {\n  mock.test.resource { name = child directives { prevent_destroy = flag force_delete = flag create_before_destroy = flag } }\n}\n");
    scenario.success(&["init", "."]);
    scenario.success(&["validate", "."]);
    scenario.success(&["plan", ".", "--out", "loop.plan"]);
    scenario.success(&["apply", "loop.plan", "--auto-approve", "--lock=false"]);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(scenario.0.path().join("carina.state.json")).unwrap())
            .unwrap();
    let resources = state["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2, "{state:#?}");
    assert!(
        resources
            .iter()
            .any(|resource| resource["directives"]["prevent_destroy"]
                == serde_json::json!({"kind": "explicit", "value": true}))
    );
    let output = scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]);
    assert!(!output.status.success(), "{}", rendered(&output));
    assert!(
        rendered(&output).contains("prevent_destroy"),
        "{}",
        rendered(&output)
    );
}

#[test]
fn invalid_loop_element_directives_report_the_authored_location() {
    for command in ["validate", "plan", "apply"] {
        let scenario = Scenario::new();
        scenario.write(
            "source.crn",
            "let source = mock.test.resource { name = 'source' tags = { child = 'not-a-bool' } }\n",
        );
        scenario.write("children.crn", "let children = for child, flag in source.tags {\n  mock.test.resource { name = child directives { prevent_destroy = flag } }\n}\n");
        scenario.success(&["init", "."]);
        let output = if command == "apply" {
            scenario.run(&["apply", ".", "--auto-approve", "--lock=false"])
        } else {
            scenario.run(&[command, "."])
        };
        let text = rendered(&output);
        assert!(!output.status.success(), "{text}");
        assert!(
            text.contains("children.crn:2:") && text.contains("directives.prevent_destroy"),
            "{text}"
        );
        assert!(
            text.contains("got String") && text.contains("mock.test.resource"),
            "{text}"
        );
        let state_path = scenario.0.path().join("carina.state.json");
        if state_path.exists() {
            let state: serde_json::Value =
                serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
            assert!(
                state["resources"].as_array().unwrap().len() <= 1,
                "{state:#?}"
            );
        }
    }
}

#[test]
fn standalone_module_directive_arguments_require_bool() {
    for directive in ["prevent_destroy", "create_before_destroy", "force_delete"] {
        for kind in ["String", "Int"] {
            let scenario = Scenario::new();
            scenario.write(
                "module/arguments.crn",
                &format!("arguments {{ flag: {kind} }}\n"),
            );
            scenario.write("module/resource.crn", &format!("let r = mock.test.resource {{ name = 'r' directives {{ {directive} = flag }} }}\n"));
            scenario.success(&["init", "."]);
            let output = scenario.run(&["validate", "module"]);
            let text = rendered(&output);
            assert!(
                !output.status.success(),
                "{kind} directive was accepted: {text}"
            );
            assert!(
                text.contains(directive) && text.contains("resource.crn:1:"),
                "{text}"
            );
            assert!(text.contains(&format!("got {kind}")), "{text}");
        }
    }
}

#[test]
fn directive_errors_distinguish_module_instances_and_value_kinds() {
    let scenario = Scenario::new();
    scenario.write("module/main.crn", "arguments { prefix: String }\nlet r = mock.test.resource { name = prefix directives { prevent_destroy = 1 } }\n");
    scenario.write("call.crn", "let m = use { source = './module' }\nlet first = m { prefix = 'one' }\nlet second = m { prefix = 'two' }\n");
    scenario.success(&["init", "."]);
    let output = scenario.run(&["validate", "."]);
    let text = rendered(&output);
    assert!(!output.status.success(), "{text}");
    let errors = text
        .lines()
        .filter(|line| line.contains("directives.prevent_destroy must resolve"))
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 2, "{text}");
    assert_ne!(errors[0], errors[1], "{text}");
    assert!(
        errors
            .iter()
            .all(|line| line.contains("got Int") && line.contains("mock.test.resource")),
        "{text}"
    );
    assert!(
        text.contains("call.crn:2:") && text.contains("call.crn:3:"),
        "{text}"
    );
}
