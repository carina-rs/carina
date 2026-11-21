//! carina#4480: validate-time promises; retained calls keep their existing behavior.

#[path = "support/fn_type_promise.rs"]
mod fixture;

#[path = "fn_type_promise/review_round1.rs"]
mod review_round1;

#[path = "fn_type_promise/module_body.rs"]
mod module_body;

#[path = "fn_type_promise/review_round2.rs"]
mod review_round2;

use carina_cli::commands::validate::{ValidateDiagnostics, validate_diagnostics_with_factories};
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::paths::ResolvedPath;
use fixture::{Directory, ENVIRONMENT, PromiseFactory, SOURCE, Scope};

#[derive(Clone, Copy)]
enum Position {
    Parameter,
    Return,
}

impl Position {
    fn context(self) -> &'static str {
        match self {
            Self::Parameter => "function 'promise': parameter 'value'",
            Self::Return => "function 'promise': return value",
        }
    }

    fn declaration(self, ty: &str) -> String {
        match self {
            Self::Parameter => format!("fn promise(value: {ty}): String {{ 'accepted' }}\n"),
            Self::Return => format!("fn promise(value): {ty} {{ value }}\n"),
        }
    }

    fn sink(self, ty: &str, expression: &str) -> String {
        let attribute = match (self, ty) {
            (Self::Return, "Int") => "number",
            (Self::Return, "list(String)") => "items",
            _ => "value",
        };
        format!("test.Sink {{ name = 'sink' {attribute} = promise({expression}) }}\n")
    }
}

fn validate(directory: &Directory) -> ValidateDiagnostics {
    validate_diagnostics_with_factories(
        &ResolvedPath::new(directory.root()).unwrap(),
        LoadedProviderFactory::for_test(vec![Box::new(PromiseFactory)]),
        Default::default(),
    )
}

fn messages(diagnostics: &ValidateDiagnostics) -> Vec<&str> {
    diagnostics.json["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|error| error["message"].as_str().unwrap())
        .collect()
}

fn assert_accepted(directory: &Directory) {
    let diagnostics = validate(directory);
    assert!(
        diagnostics.errors.is_empty(),
        "validate must accept this call: {}",
        diagnostics.json
    );
}

fn assert_message(directory: &Directory, expected: &str) {
    assert_messages(directory, &[expected]);
}

fn assert_messages(directory: &Directory, expected: &[&str]) {
    let diagnostics = validate(directory);
    assert_eq!(messages(&diagnostics), expected, "{}", diagnostics.json);
    for error in diagnostics.json["errors"].as_array().unwrap() {
        let location = &error["location"];
        assert!(
            location["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("main.crn")),
            "call-site diagnostic must retain its file: {error}"
        );
        assert!(location["range"].is_object(), "{error}");
    }
}

fn assert_mismatch(directory: &Directory, position: Position, declared: &str, actual: &str) {
    assert_message(
        directory,
        &format!("{}: expected {declared}, got {actual}", position.context()),
    );
}

fn assert_rule3(directory: &Directory, position: Position, declared: &str, actual: &str) {
    let evidence = match (position, actual) {
        (Position::Parameter, "no declared type") => {
            "its argument comes from upstream export 'upstream.untyped', which has no declared type"
                .to_owned()
        }
        (Position::Return, "no declared type") => {
            "its value comes from upstream export 'upstream.untyped', which has no declared type"
                .to_owned()
        }
        (Position::Parameter, "unknown") => "its argument has no known type".to_owned(),
        (Position::Return, "unknown") => "the body has no known type".to_owned(),
        (Position::Parameter, _) => format!("its argument has type {actual}"),
        (Position::Return, _) => format!("the body's type is {actual}"),
    };
    let repair = if actual == "no declared type" {
        "Pass a value known at validate, or declare the export's type.".to_owned()
    } else if actual == "unknown" || actual == declared {
        "Pass a value known at validate, or give the argument a type that proves it fits."
            .to_owned()
    } else {
        let position = match position {
            Position::Parameter => "parameter",
            Position::Return => "return type",
        };
        format!(
            "Pass a value known at validate, or declare the {position} as {actual} and let the resource attribute that receives it check the value."
        )
    };
    assert_message(
        directory,
        &format!(
            "{} is declared as {declared}, but {evidence}, so validate cannot prove it fits. {repair}",
            position.context()
        ),
    );
}

fn case_directory(
    scope: Scope,
    position: Position,
    declared: &str,
    setup: &str,
    expression: &str,
) -> Directory {
    let directory = Directory::new(scope);
    let argument = directory.argument_expression(expression, declared);
    let (declaration, argument) = match position {
        Position::Parameter => (position.declaration(declared), argument),
        Position::Return => {
            // Return evidence comes from the formal declaration, never from
            // substituting this call's actual expression into the body.
            let parameter = if expression == "upstream.untyped" {
                "value"
            } else {
                "value: String"
            };
            let (body, argument) = match expression {
                "upper(src.identifier)" => ("upper(value)", "src.identifier".to_owned()),
                "src.comment ?? 'dev'" => ("value ?? 'dev'", "src.comment".to_owned()),
                "env" => ("let alias = value\n alias", argument),
                "lookup({ env = src.identifier }, 'env', 'dev')" => (
                    "lookup({ env = value }, 'env', 'dev')",
                    "src.identifier".to_owned(),
                ),
                _ => ("value", argument),
            };
            (
                format!("fn promise({parameter}): {declared} {{ {body} }}\n"),
                argument,
            )
        }
    };
    directory.write_source(&format!(
        "{SOURCE}{}{setup}{declaration}{}",
        directory.upstream_binding(),
        position.sink(declared, &argument),
    ));
    directory
}

macro_rules! rule3_case {
    ($name:ident, $scope:expr, $position:expr, $ty:expr, $setup:expr, $expression:expr, $actual:expr) => {
        rule3_case!(
            $name,
            $scope,
            $position,
            $ty,
            $setup,
            $expression,
            $actual,
            assert_rule3
        );
    };
    ($name:ident, $scope:expr, $position:expr, $ty:expr, $setup:expr, $expression:expr, $actual:expr, $check:ident) => {
        #[test]
        fn $name() {
            let directory = case_directory($scope, $position, $ty, $setup, $expression);
            let actual =
                if matches!($position, Position::Return) && $expression == "upstream.untyped" {
                    "unknown"
                } else if matches!($scope, Scope::Module) && $expression == "upstream.untyped" {
                    "String"
                } else {
                    $actual
                };
            $check(&directory, $position, $ty, actual);
        }
    };
}

macro_rules! rule3_matrix {
    ($scope:expr, $position:expr) => {
        rule3_case!(
            identifier,
            $scope,
            $position,
            ENVIRONMENT,
            "",
            "src.identifier",
            "String"
        );
        rule3_case!(
            typed_upstream,
            $scope,
            $position,
            ENVIRONMENT,
            "",
            "upstream.typed",
            "String"
        );
        rule3_case!(
            untyped_upstream,
            $scope,
            $position,
            ENVIRONMENT,
            "",
            "upstream.untyped",
            "no declared type"
        );
        rule3_case!(
            provider_resource,
            $scope,
            $position,
            "test.Name",
            "",
            "src.identifier",
            "String",
            assert_mismatch
        );
        rule3_case!(
            any_lookup,
            $scope,
            $position,
            "String",
            "",
            "lookup({ env = src.identifier }, 'env', 'dev')",
            "unknown"
        );
        #[test]
        fn deferred_loop_element() {
            let directory = Directory::new($scope);
            directory.write_source(&format!(
                "{SOURCE}{}for env in src.items {{ {} }}\n",
                match $position {
                    Position::Parameter => $position.declaration(ENVIRONMENT),
                    Position::Return =>
                        format!("fn promise(value: String): {ENVIRONMENT} {{ value }}\n"),
                },
                $position.sink(ENVIRONMENT, "env"),
            ));
            assert_rule3(&directory, $position, ENVIRONMENT, "String");
        }

        #[test]
        fn sibling_file_function() {
            let directory = Directory::new($scope);
            std::fs::write(
                directory.source_file().with_file_name("functions.crn"),
                match $position {
                    Position::Parameter => $position.declaration(ENVIRONMENT),
                    Position::Return => {
                        format!("fn promise(value: String): {ENVIRONMENT} {{ value }}\n")
                    }
                },
            )
            .unwrap();
            directory.write_source(&format!(
                "{SOURCE}{}",
                $position.sink(ENVIRONMENT, "src.identifier")
            ));
            assert_rule3(&directory, $position, ENVIRONMENT, "String");
        }
    };
}

macro_rules! rule3_compositions {
    ($scope:expr, $position:expr) => {
        rule3_case!(
            upper,
            $scope,
            $position,
            ENVIRONMENT,
            "",
            "upper(src.identifier)",
            "String"
        );
        rule3_case!(
            let_alias,
            $scope,
            $position,
            ENVIRONMENT,
            "let env = src.identifier\n",
            "env",
            "String"
        );
        rule3_case!(
            coalesce,
            $scope,
            $position,
            ENVIRONMENT,
            "",
            "src.comment ?? 'dev'",
            "String"
        );
        rule3_case!(
            typed_upstream_int,
            $scope,
            $position,
            "Int",
            "",
            "upstream.typed",
            "String",
            assert_mismatch
        );
        rule3_case!(
            untyped_upstream_int,
            $scope,
            $position,
            "Int",
            "",
            "upstream.untyped",
            "no declared type"
        );
        rule3_case!(
            untyped_upstream_list,
            $scope,
            $position,
            "list(String)",
            "",
            "upstream.untyped",
            "no declared type"
        );
        rule3_case!(
            provider_data_source,
            $scope,
            $position,
            "test.Name",
            "let found = read test.Lookup { query = 'find' }\n",
            "found.value",
            "String",
            assert_mismatch
        );
        rule3_case!(
            provider_upstream,
            $scope,
            $position,
            "test.Name",
            "",
            "upstream.typed",
            "String",
            assert_mismatch
        );
    };
}

mod tp1_rule3 {
    use super::*;
    mod root {
        use super::*;
        mod parameter {
            use super::*;
            rule3_matrix!(Scope::Root, Position::Parameter);
            rule3_compositions!(Scope::Root, Position::Parameter);
        }
        mod returns {
            use super::*;
            rule3_matrix!(Scope::Root, Position::Return);
        }
    }
    mod module {
        use super::*;
        mod parameter {
            use super::*;
            rule3_matrix!(Scope::Module, Position::Parameter);
        }
        mod returns {
            use super::*;
            rule3_matrix!(Scope::Module, Position::Return);
            rule3_compositions!(Scope::Module, Position::Return);
        }
    }
}

macro_rules! rule2_matrix {
    ($scope:expr) => {
        #[test]
        fn string_to_string() {
            rule2($scope, "String", "src.identifier", "value");
        }
        #[test]
        fn name_attribute_to_name() {
            rule2($scope, "test.Name", "src.typed_name", "value");
        }
        #[test]
        fn declared_string_parameter_cannot_prove_name_return() {
            let directory = Directory::new($scope);
            directory.write_source(&format!(
                "{SOURCE}fn promise(value: String): test.Name {{ value }}\ntest.Sink {{ name = 'sink' value = promise(src.typed_name) }}\n"
            ));
            assert_mismatch(&directory, Position::Return, "test.Name", "String");
        }
        #[test]
        fn int_to_int() {
            rule2($scope, "Int", "src.count", "number");
        }
        #[test]
        fn typed_upstream_name() {
            rule2($scope, "test.Name", "upstream.named", "value");
        }
        #[test]
        fn sibling_file_function_accepts_proving_type() {
            let directory = Directory::new($scope);
            std::fs::write(
                directory.source_file().with_file_name("functions.crn"),
                "fn promise(value: String): String { value }\n",
            )
            .unwrap();
            directory.write_source(&format!(
                "{SOURCE}test.Sink {{ name = 'sink' value = promise(src.identifier) }}\n"
            ));
            assert_accepted(&directory);
        }
    };
}

macro_rules! rule2_scalar_variations {
    (root, $scope:expr) => {
        #[test]
        fn constructed_string_to_string() {
            rule2($scope, "String", "upper(src.identifier)", "value");
        }
        #[test]
        fn typed_upstream_string() {
            rule2($scope, "String", "upstream.typed", "value");
        }
    };
    (module, $scope:expr) => {
        #[test]
        fn builtin_int_to_int() {
            rule2($scope, "Int", "length(src.items)", "number");
        }
        #[test]
        fn typed_upstream_int() {
            rule2($scope, "Int", "upstream.count", "number");
        }
    };
}

fn rule2(scope: Scope, ty: &str, expression: &str, attribute: &str) {
    let directory = Directory::new(scope);
    let expression = directory.argument_expression(expression, ty);
    directory.write_source(&format!(
        "{SOURCE}{}fn promise(value: {ty}): {ty} {{ value }}\ntest.Sink {{ name = 'sink' {attribute} = promise({expression}) }}\n",
        directory.upstream_binding(),
    ));
    assert_accepted(&directory);
}

mod tp1_rule2 {
    use super::*;
    mod root {
        use super::*;
        rule2_matrix!(Scope::Root);
        rule2_scalar_variations!(root, Scope::Root);
    }
    mod module {
        use super::*;
        rule2_matrix!(Scope::Module);
        rule2_scalar_variations!(module, Scope::Module);

        #[test]
        fn untyped_upstream_int() {
            let directory = case_directory(
                Scope::Module,
                Position::Parameter,
                "Int",
                "",
                "upstream.untyped",
            );
            assert_accepted(&directory);
        }

        #[test]
        fn untyped_upstream_list() {
            let directory = case_directory(
                Scope::Module,
                Position::Parameter,
                "list(String)",
                "",
                "upstream.untyped",
            );
            assert_accepted(&directory);
        }
    }
}

fn known_case(scope: Scope, position: Position, shape: &str, accepted: bool) {
    let directory = Directory::new(scope);
    let value = if accepted { "'dev'" } else { "'qa'" };
    let call = position.sink(ENVIRONMENT, "env");
    let body = match shape {
        "literal" => position.sink(ENVIRONMENT, value),
        "let" => format!("let env = {value}\n{call}"),
        "for" => format!("for env in [{value}] {{ {call} }}\n"),
        "lookup" => position.sink(
            ENVIRONMENT,
            &format!("lookup({{ env = {value} }}, 'env', 'prod')"),
        ),
        // A non-selected branch cannot invalidate a known value.
        "if" => format!("let env = if true {{ {value} }} else {{ 'unused' }}\n{call}"),
        _ => unreachable!("known fixture shape"),
    };
    directory.write_source(&format!("{}{body}", position.declaration(ENVIRONMENT)));
    if accepted {
        assert_accepted(&directory);
    } else {
        let diagnostics = validate(&directory);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains(position.context())),
            "known invalid value must fail the declared position: {}",
            diagnostics.json
        );
    }
}

fn known_provider_literal(scope: Scope, position: Position, accepted: bool) {
    let expression = if accepted { "'allowed'" } else { "'other'" };
    let directory = Directory::new(scope);
    directory.write_source(&format!(
        "{}{SOURCE}{}",
        position.declaration("test.Name"),
        position.sink("test.Name", expression),
    ));
    if accepted {
        assert_accepted(&directory);
    } else {
        let diagnostics = validate(&directory);
        assert!(
            messages(&diagnostics)
                .iter()
                .any(|message| message.contains(position.context())),
            "known provider-typed value must use the existing value check: {}",
            diagnostics.json
        );
    }
}

macro_rules! rule1_matrix {
    ($scope:expr, $position:expr) => {
        #[test]
        fn literal_accept() {
            known_case($scope, $position, "literal", true);
        }
        #[test]
        fn literal_reject() {
            known_case($scope, $position, "literal", false);
        }
    };
}

macro_rules! rule1_compositions {
    ($scope:expr, $position:expr) => {
        #[test]
        fn let_accept() {
            known_case($scope, $position, "let", true);
        }
        #[test]
        fn let_reject() {
            known_case($scope, $position, "let", false);
        }
        #[test]
        fn for_accept() {
            known_case($scope, $position, "for", true);
        }
        #[test]
        fn for_reject() {
            known_case($scope, $position, "for", false);
        }
        #[test]
        fn if_accept() {
            known_case($scope, $position, "if", true);
        }
        #[test]
        fn if_reject() {
            known_case($scope, $position, "if", false);
        }
        #[test]
        fn provider_literal_accept() {
            known_provider_literal($scope, $position, true);
        }
        #[test]
        fn provider_literal_reject() {
            known_provider_literal($scope, $position, false);
        }
        #[test]
        fn known_lookup_accept() {
            known_case($scope, $position, "lookup", true);
        }
        #[test]
        fn known_lookup_reject() {
            known_case($scope, $position, "lookup", false);
        }
    };
}

mod tp1_rule1 {
    use super::*;
    mod root {
        use super::*;
        mod parameter {
            use super::*;
            rule1_matrix!(Scope::Root, Position::Parameter);
            rule1_compositions!(Scope::Root, Position::Parameter);
        }
        mod returns {
            use super::*;
            rule1_matrix!(Scope::Root, Position::Return);
        }
    }
    mod module {
        use super::*;
        mod parameter {
            use super::*;
            rule1_matrix!(Scope::Module, Position::Parameter);
        }
        mod returns {
            use super::*;
            rule1_matrix!(Scope::Module, Position::Return);
            rule1_compositions!(Scope::Module, Position::Return);
        }
    }

    fn module_argument(position: Position, accepted: bool) {
        let directory = Directory::new(Scope::Module);
        let value = if accepted { "'dev'" } else { "'qa'" };
        std::fs::write(
            directory.root().join("main.crn"),
            format!("let child = use {{ source = './child' }}\nchild {{ env = {value} }}\n"),
        )
        .unwrap();
        directory.write_source(&format!(
            "arguments {{ env: String }}\n{}{}",
            match position {
                Position::Parameter => position.declaration(ENVIRONMENT),
                Position::Return =>
                    format!("fn promise(value: String): {ENVIRONMENT} {{ value }}\n"),
            },
            position.sink(ENVIRONMENT, "env"),
        ));
        if accepted {
            assert_accepted(&directory);
        } else {
            let diagnostics = validate(&directory);
            assert!(
                messages(&diagnostics)
                    .iter()
                    .any(|message| message.contains(position.context())),
                "{}",
                diagnostics.json
            );
        }
    }

    #[test]
    fn module_argument_parameter_accept() {
        module_argument(Position::Parameter, true);
    }
    #[test]
    fn module_argument_parameter_reject() {
        module_argument(Position::Parameter, false);
    }
    #[test]
    fn module_argument_return_accept() {
        module_argument(Position::Return, true);
    }
    #[test]
    fn module_argument_return_reject() {
        module_argument(Position::Return, false);
    }
}

macro_rules! per_call_matrix {
    ($scope:expr) => {
        #[test]
        fn actual_string_type_does_not_prove_return() {
            per_call($scope, "src.identifier", false);
        }
        #[test]
        fn unused_non_proving_function_is_accepted() {
            let directory = Directory::new($scope);
            directory.write_source(&format!(
                "{SOURCE}fn pick(x: String): {ENVIRONMENT} {{ x }}\nfn captured(): {ENVIRONMENT} {{ src.identifier }}\ntest.Sink {{ name = 'sink' value = 'dev' }}\n"
            ));
            assert_accepted(&directory);
        }
    };
}

fn per_call(scope: Scope, expression: &str, accepted: bool) {
    let directory = Directory::new(scope);
    directory.write_source(&format!(
        "{SOURCE}fn pick(x: String): {ENVIRONMENT} {{ x }}\nlet known = test.Sink {{ name = 'known' value = pick('dev') }}\nlet actual = test.Sink {{ name = 'actual' value = pick({expression}) }}\n"
    ));
    if accepted {
        assert_accepted(&directory);
    } else {
        let diagnostics = validate(&directory);
        let errors = messages(&diagnostics);
        assert_eq!(
            errors.len(),
            1,
            "one failing call, one diagnostic: {}",
            diagnostics.json
        );
        assert!(
            errors[0].contains("function 'pick': return value"),
            "{errors:?}"
        );
        assert!(errors[0].contains("at validate"), "{errors:?}");
        assert_eq!(
            diagnostics.json["errors"][0]["location"]["range"]["start"]["line"], 4,
            "the error belongs to the unknown call, not the declaration or known call: {}",
            diagnostics.json
        );
    }
}

mod tp1_per_call {
    use super::*;
    mod root {
        use super::*;
        per_call_matrix!(Scope::Root);
    }
    mod module {
        use super::*;
        per_call_matrix!(Scope::Module);
    }
}

fn lsp_errors(directory: &Directory) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    use std::sync::Arc;

    use carina_core::{
        EnvironmentSnapshot, parser::ProviderContext, provider::*, schema::SchemaRegistry,
    };
    use carina_lsp::{document::Document, test_support::Diagnostics};
    use tower_lsp::lsp_types::DiagnosticSeverity;

    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(PromiseFactory) as Box<dyn ProviderFactory>]),
        SchemaRegistry::new(),
        EnvironmentSnapshot::empty(),
    );
    let engine = Diagnostics::new(directory.root(), Arc::new(loaded), vec!["test".into()]);
    let path = directory.source_file();
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    engine
        .analyze(&document, Some(&path))
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect()
}

fn lsp_validate_parity(scope: Scope, position: Position) {
    let directory = case_directory(scope, position, ENVIRONMENT, "", "src.identifier");
    let cli = validate(&directory);
    let lsp = lsp_errors(&directory);
    assert_eq!(lsp.len(), 1, "LSP must reject the call: {lsp:#?}");
    assert_eq!(messages(&cli), [lsp[0].message.as_str()], "{}", cli.json);
    assert_eq!(
        cli.json["errors"][0]["location"]["range"]["start"]["line"],
        u64::from(lsp[0].range.start.line) + 1,
        "CLI and LSP must point to the same authored call"
    );
}

#[test]
fn tp3_lsp_validate_nested_promise_and_schema_error_parity() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn inner(y: {ENVIRONMENT}): String {{ 'ok' }}\nfn outer(x: String): String {{ inner(x) }}\nlet sink = test.Sink {{ name = 'sink' value = outer(src.identifier) }}\nlet other = test.Sink {{ name = 'other' bogus = 1 }}\n"
    ));
    let cli = validate(&directory);
    let lsp = lsp_errors(&directory);
    assert_eq!(messages(&cli).len(), 2, "{}", cli.json);
    assert_eq!(lsp.len(), 2, "{lsp:#?}");
    let mut cli_messages = messages(&cli);
    cli_messages.sort();
    let mut lsp_messages: Vec<_> = lsp.iter().map(|error| error.message.as_str()).collect();
    lsp_messages.sort();
    assert_eq!(cli_messages, lsp_messages, "{}", cli.json);
}

#[test]
fn tp1_lsp_validate_rule3_parity_root() {
    lsp_validate_parity(Scope::Root, Position::Parameter);
}

#[test]
fn tp1_lsp_validate_rule3_parity_module() {
    lsp_validate_parity(Scope::Module, Position::Parameter);
}

mod tp1_missing_schema {
    use super::*;

    fn check(scope: Scope, position: Position, ty: &str) {
        let directory = case_directory(scope, position, ty, "", "src.identifier");
        let diagnostics = validate_diagnostics_with_factories(
            &ResolvedPath::new(directory.root()).unwrap(),
            Vec::new(),
            std::collections::HashMap::from([("test".into(), "provider not installed".into())]),
        );
        let errors = messages(&diagnostics);
        assert!(
            errors
                .iter()
                .all(|error| !error.contains("function 'promise'")),
            "missing schemas must not produce rule 3: {}",
            diagnostics.json
        );
        assert!(
            diagnostics
                .json
                .to_string()
                .contains("provider not installed"),
            "fixture must actually exercise a failed provider load: {}",
            diagnostics.json
        );
    }

    macro_rules! missing_schema_matrix {
        ($scope:expr) => {
            #[test]
            fn parameter_union() {
                check($scope, Position::Parameter, ENVIRONMENT);
            }
            #[test]
            fn return_union() {
                check($scope, Position::Return, ENVIRONMENT);
            }
            #[test]
            fn parameter_provider_type() {
                check($scope, Position::Parameter, "test.Name");
            }
            #[test]
            fn return_provider_type() {
                check($scope, Position::Return, "test.Name");
            }
        };
    }

    mod root {
        use super::*;
        missing_schema_matrix!(Scope::Root);
    }
    mod module {
        use super::*;
        missing_schema_matrix!(Scope::Module);
    }
}

mod tp1_handle_returns {
    use super::*;

    fn check(scope: Scope, declared: &str) {
        let directory = Directory::new(scope);
        let attribute = if declared == "Int" { "number" } else { "items" };
        directory.write_source(&format!(
            "{SOURCE}fn promise(value: test.Thing): {declared} {{ value.identifier }}\ntest.Sink {{ name = 'sink' {attribute} = promise(src) }}\n"
        ));
        if matches!(scope, Scope::Module) && declared == "list(String)" {
            // Main's template preflight reports both the invalid declaration
            // and the independent reference mismatch at this resource sink.
            assert_messages(
                &directory,
                &[
                    "function 'promise': return value: expected list(String), got String",
                    "Type mismatch: expected List<String>, got String (from src.identifier)",
                ],
            );
        } else {
            assert_mismatch(&directory, Position::Return, declared, "String");
        }
    }

    #[test]
    fn root_int_from_handle_attribute() {
        check(Scope::Root, "Int");
    }

    #[test]
    fn module_list_from_handle_attribute() {
        check(Scope::Module, "list(String)");
    }
}

mod tp1_module_argument {
    use super::*;

    fn check(position: Position) {
        let directory = Directory::new(Scope::Module);
        std::fs::write(
            directory.root().join("main.crn"),
            format!("{SOURCE}let child = use {{ source = './child' }}\nchild {{ env = src.identifier }}\n"),
        )
        .unwrap();
        directory.write_source(&format!(
            "arguments {{ env: String }}\n{}{}",
            match position {
                Position::Parameter => position.declaration(ENVIRONMENT),
                Position::Return =>
                    format!("fn promise(value: String): {ENVIRONMENT} {{ value }}\n"),
            },
            position.sink(ENVIRONMENT, "env"),
        ));
        assert_rule3(&directory, position, ENVIRONMENT, "String");
    }

    #[test]
    fn parameter_unknown_string_rule3() {
        check(Position::Parameter);
    }

    #[test]
    fn return_unknown_string_rule3() {
        check(Position::Return);
    }
}

#[test]
fn tp2_approved_parameter_diagnostic() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn pick(x: {ENVIRONMENT}): String {{ 'accepted' }}\ntest.Sink {{ name = 'sink' value = pick(src.identifier) }}\n"
    ));
    assert_eq!(
        messages(&validate(&directory)),
        [
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value."
        ]
    );
}

#[test]
fn tp2_approved_return_diagnostic() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn pick(x: String): {ENVIRONMENT} {{ x }}\ntest.Sink {{ name = 'sink' value = pick(src.identifier) }}\n"
    ));
    assert_eq!(
        messages(&validate(&directory)),
        [
            "function 'pick': return value is declared as 'dev' | 'prod', but the body's type is String, so validate cannot prove it fits. Pass a value known at validate, or declare the return type as String and let the resource attribute that receives it check the value."
        ]
    );
}

#[test]
fn tp2_approved_untyped_export_diagnostic() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "let orgs = upstream_state {{ source = '../upstream' }}\nfn pick(x: {ENVIRONMENT}): String {{ 'accepted' }}\ntest.Sink {{ name = 'sink' value = pick(orgs.untyped) }}\n"
    ));
    assert_eq!(
        messages(&validate(&directory)),
        [
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument comes from upstream export 'orgs.untyped', which has no declared type, so validate cannot prove it fits. Pass a value known at validate, or declare the export's type."
        ]
    );
}

#[test]
fn tp2_captured_untyped_export_return() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{}fn promise(): String {{ upstream.untyped }}\ntest.Sink {{ name = 'sink' value = promise() }}\n",
        directory.upstream_binding(),
    ));
    assert_rule3(&directory, Position::Return, "String", "no declared type");
}

#[test]
fn tp2_known_if_does_not_check_unselected_call() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value: {ENVIRONMENT}): String {{ value }}\ntest.Sink {{ name = 'sink' value = if true {{ promise('dev') }} else {{ promise(src.identifier) }} }}\n"
    ));
    assert_accepted(&directory);
}

#[test]
fn tp2_pipe_argument_rule3() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value: {ENVIRONMENT}): String {{ value }}\ntest.Sink {{ name = 'sink' value = src.identifier |> promise() }}\n"
    ));
    assert_rule3(&directory, Position::Parameter, ENVIRONMENT, "String");
}

#[test]
fn tp2_typed_collection_rule2() {
    rule2(Scope::Root, "list(String)", "src.items", "items");
}

#[test]
fn tp2_collection_builtin_elements_rule3() {
    let directory = Directory::new(Scope::Module);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value: list(String)): list(String) {{ flatten([value]) }}\ntest.Sink {{ name = 'sink' items = promise(src.items) }}\n"
    ));
    assert_rule3(&directory, Position::Return, "list(String)", "unknown");
}

#[test]
fn tp2_unknown_default_argument_rule3() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value: {ENVIRONMENT} = src.identifier): String {{ 'accepted' }}\ntest.Sink {{ name = 'sink' value = promise() }}\n"
    ));
    assert_rule3(&directory, Position::Parameter, ENVIRONMENT, "String");
}

#[test]
fn tp2_lsp_return_parity_root() {
    lsp_validate_parity(Scope::Root, Position::Return);
}

#[test]
fn tp2_lsp_return_parity_module() {
    lsp_validate_parity(Scope::Module, Position::Return);
}

#[test]
fn tp2_untyped_body_primitive_return_owns_error() {
    let directory = Directory::new(Scope::Module);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value): Int {{ value }}\ntest.Sink {{ name = 'sink' number = promise(src.identifier) }}\n"
    ));
    assert_rule3(&directory, Position::Return, "Int", "unknown");
}

#[test]
fn tp2_empty_loop_does_not_check_unreached_call() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value: {ENVIRONMENT}): String {{ value }}\nfor ignored in [] {{ test.Sink {{ name = 'sink' value = promise(src.identifier) }} }}\n"
    ));
    assert_accepted(&directory);
}

#[test]
fn tp2_module_if_checks_selected_calls() {
    for selected in [false, true] {
        let directory = Directory::new(Scope::Module);
        std::fs::write(
            directory.root().join("main.crn"),
            format!("let child = use {{ source = './child' }}\nchild {{ enabled = {selected} }}\n"),
        )
        .unwrap();
        directory.write_source(&format!(
            "arguments {{ enabled: Bool }}\n{SOURCE}fn promise(value: {ENVIRONMENT}): String {{ value }}\ntest.Sink {{ name = 'sink' value = if enabled {{ promise(src.identifier) }} else {{ 'accepted' }} }}\n"
        ));
        if selected {
            assert_rule3(&directory, Position::Parameter, ENVIRONMENT, "String");
        } else {
            assert_accepted(&directory);
        }
    }
}

#[test]
fn tp2_untyped_parameter_cannot_prove_int_return() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{SOURCE}fn promise(value): Int {{ value }}\ntest.Sink {{ name = 'sink' number = promise(src.count) }}\n"
    ));
    assert_rule3(&directory, Position::Return, "Int", "unknown");
}
