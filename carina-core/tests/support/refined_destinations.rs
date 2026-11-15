// Boundary/directory must stay in sync with carina-core/src/validation/admission/tests/refined_destinations.rs.
//! Directory-backed forwarding boundaries. Authored annotations name an
//! identity; the final schema slot owns its separate member constraints.
use super::fixture;

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Resource,
    Let,
    Export,
    ModuleArgument,
    FunctionParameter,
    FunctionReturn,
    Attributes,
    ModuleAttributes,
}

pub fn directory(boundary: Boundary, resolved_length: Option<usize>) -> tempfile::TempDir {
    let input =
        resolved_length.map_or_else(|| "source.name".into(), |n| format!("'{}'", "A".repeat(n)));
    let slot = |value: &str| format!("let sink = awscc.MemberSink {{ name = {value} }}\n");
    let (main, child) = match boundary {
        Boundary::Resource => (slot(&input), None),
        // Typed lets are not syntax; a let forwards identity-only value evidence.
        Boundary::Let => (format!("let name = {input}\n{}", slot("name")), None),
        Boundary::FunctionParameter => (
            format!(
                "fn forward(name: aws.Name): aws.Name {{ name }}\n{}",
                slot(&format!("forward({input})"))
            ),
            None,
        ),
        Boundary::FunctionReturn => (
            format!(
                "fn forward(): aws.Name {{ {input} }}\n{}",
                slot("forward()")
            ),
            None,
        ),
        Boundary::Export => (slot(&input), None),
        Boundary::Attributes | Boundary::ModuleAttributes => (
            format!(
                "attributes {{ name: aws.Name = {input} }}\n{}",
                slot(&input)
            ),
            None,
        ),
        Boundary::ModuleArgument => (
            format!(
                "let forward = use {{ source = './forward' }}\nlet out = forward {{ name = {input} }}\n"
            ),
            Some(format!("arguments {{ name: aws.Name }}\n{}", slot("name"))),
        ),
    };
    let exports = if matches!(boundary, Boundary::Export) {
        format!("exports {{ name: aws.Name = {input} }}\n")
    } else {
        "exports {}\n".into()
    };
    let dir = fixture::directory(
        &format!("let source = awscc.MemberSource {{}}\n{main}"),
        &exports,
    );
    if matches!(boundary, Boundary::ModuleAttributes) {
        let module = dir.path().join("outputs");
        std::fs::create_dir(&module).unwrap();
        let source = std::fs::read_to_string(dir.path().join("main.crn")).unwrap();
        std::fs::write(module.join("main.crn"), source).unwrap();
        std::fs::write(
            dir.path().join("main.crn"),
            "let outputs = use { source = './outputs' }\nlet result = outputs {}\n",
        )
        .unwrap();
    }
    if let Some(child) = child {
        std::fs::create_dir(dir.path().join("forward")).unwrap();
        std::fs::write(dir.path().join("forward/main.crn"), child).unwrap();
    }
    dir
}
