use std::path::Path;

use carina_core::EnvironmentSnapshot;
use carina_core::parser::{ProviderContext, parse_file};

fn overflowing_literal() -> String {
    format!("1{}.0", "0".repeat(400))
}

fn assert_overflow_rejected(source: &str, literal_line: usize) {
    let path = Path::new("non_finite_float.crn");
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let finite_source = source.replace(&overflowing_literal(), "1.0");
    parse_file(&finite_source, path, &context).expect("the finite control must parse");

    let error = parse_file(source, path, &context)
        .expect_err("overflow must be rejected before constructing an infinite Float");
    let message = error.to_string();
    assert!(message.to_lowercase().contains("float"), "{message}");
    assert!(
        message.contains(&format!("line {literal_line}")),
        "{message}"
    );
}

#[test]
#[ignore = "carina#4257: enable with the FiniteF64 swap"]
fn parser_rejects_overflowing_float_literal() {
    // Currently the literal parses successfully as ConcreteValue::Float(inf).
    let source = format!("\n\nlet ratio = {}\n", overflowing_literal());
    assert_overflow_rejected(&source, 3);
}

#[test]
#[ignore = "carina#4257: enable with the FiniteF64 swap"]
fn validate_expr_rejects_overflowing_float_literal() {
    // Currently the validation condition stores ValidateExpr::Float(inf).
    let source = format!(
        "arguments {{\n\
             ratio: Float {{\n\
                 validation {{\n\
                     condition = ratio < {}\n\
                     error_message = \"ratio must be below the limit\"\n\
                 }}\n\
             }}\n\
         }}\n",
        overflowing_literal(),
    );
    assert_overflow_rejected(&source, 4);
}

#[test]
#[ignore = "carina#4257: enable with the FiniteF64 swap"]
fn multiline_wait_until_rejects_overflowing_float_literal() {
    // Currently the multiline until RHS is accepted as ConcreteValue::Float(inf).
    let source = format!(
        "let source = mock.test.Thing {{ name = 'source' }}\n\
         let ready = wait source {{\n\
             until = source.ratio ==\n\
                 {}\n\
         }}\n",
        overflowing_literal(),
    );
    assert_overflow_rejected(&source, 4);
}
