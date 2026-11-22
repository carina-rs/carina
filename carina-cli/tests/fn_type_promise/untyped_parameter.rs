use super::*;

const RETURN_SOURCE: &str = "fn outer2(x): String { x }\nlet sink = test.Sink { name = 'sink' value = outer2(src.identifier) }\n";
const NESTED_SOURCE: &str = "fn inner(y: String): String { y }\nfn outer(x): String { inner(x) }\nlet sink = test.Sink { name = 'sink' value = outer(src.identifier) }\n";
const RETURN_MESSAGE: &str = "function 'outer2': return value is declared as String, but it comes from parameter 'x', which has no declared type, so validate cannot prove it fits. Pass a value known at validate, or declare the type of parameter 'x'.";
const NESTED_MESSAGE: &str = "function 'inner': parameter 'y' is declared as String, but its argument comes from parameter 'x' of function 'outer', which has no declared type, so validate cannot prove it fits. Pass a value known at validate, or declare the type of parameter 'x'.";

fn directory(source: &str) -> Directory {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!("{SOURCE}{source}"));
    directory
}

#[test]
fn tp7_untyped_parameter_return_exact_text() {
    assert_message(&directory(RETURN_SOURCE), RETURN_MESSAGE);
}

#[test]
fn tp7_untyped_parameter_nested_call_exact_text() {
    assert_message(&directory(NESTED_SOURCE), NESTED_MESSAGE);
}

#[test]
fn tp7_untyped_parameter_lsp_exact_text() {
    for (source, expected, line) in [
        (RETURN_SOURCE, RETURN_MESSAGE, 2),
        (NESTED_SOURCE, NESTED_MESSAGE, 3),
    ] {
        let errors = lsp_errors(&directory(source));
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert_eq!(errors[0].message, expected);
        assert_eq!(errors[0].range.start.line, line);
    }
}
