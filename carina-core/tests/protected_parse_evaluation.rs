use carina_core::parser::{ParseError, ProviderContext, SourceSpanIndex, parse_file};
use std::path::Path;

fn parse_error(source: &str) -> ParseError {
    SourceSpanIndex::parse(source).expect("fixture must pass the canonical grammar");
    parse_file(
        source,
        Path::new("main.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .expect_err("fixture must fail during AST evaluation")
}

#[test]
fn round13_protected_ast_evaluation_retains_only_kind_and_position() {
    for source in [
        "let hidden = { private = 1 }['CANARY'] |> secret()",
        "let hidden = secret({ private = 1 }['CANARY'])",
        "let hidden = secret(length({ private = 1 }['CANARY']))",
        "let hidden = secret(\"${{ private = 1 }['CANARY']}\")",
        "let hidden = secret(<<EOF\n${{ private = 1 }['CANARY']}\nEOF\n)",
        "let hidden = 'payload' |> CANARY() |> secret() |> length()",
        "let hidden = { CANARY = 1 } + 1 |> secret() ?? 1",
    ] {
        let error = parse_error(source);
        for rendered in [
            error.to_string(),
            error.diagnostic_message(),
            format!("{error:?}"),
        ] {
            assert!(!rendered.contains("CANARY"), "{rendered}");
            assert!(!rendered.contains("private"), "{rendered}");
        }
        let failure = error
            .protected_failure()
            .expect("protected evaluation must have a content-free error type");
        assert!(failure.range().start_line > 0);
        assert!(failure.range().start_column > 0);
    }
}

#[test]
fn round13_public_ast_evaluation_keeps_its_original_error() {
    for (source, line) in [
        ("let visible = { private = 1 }['CANARY']", 1),
        (
            "let hidden = secret('safe')\nlet visible = { private = 1 }['CANARY']",
            2,
        ),
    ] {
        let error = parse_error(source);
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid expression at line {line}: key 'CANARY' not found in map; available keys: 'private'"
            )
        );
        assert!(error.protected_failure().is_none());
    }
}

#[test]
fn round13_deferred_function_secret_evaluation_has_no_authored_payload() {
    for source in [
        "fn pick(input) { secret(input['CANARY']) }\nlet hidden = pick({ private = 1 })",
        "fn pick(input) { input['CANARY'] |> secret() }\nlet hidden = pick({ private = 1 })",
        "fn pick(input) { input['CANARY'] }\nlet hidden = secret(pick({ private = 1 }))",
    ] {
        SourceSpanIndex::parse(source).unwrap();
        let error = carina_core::parser::parse_file_and_resolve(
            source,
            Path::new("main.crn"),
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .unwrap_err();
        for text in [
            error.to_string(),
            format!("{error:?}"),
            error.diagnostic_message(),
        ] {
            assert!(
                !text.contains("CANARY") && !text.contains("private"),
                "{text}"
            );
        }
        assert!(error.diagnostic_range().is_some(), "{error:?}");
    }
}
