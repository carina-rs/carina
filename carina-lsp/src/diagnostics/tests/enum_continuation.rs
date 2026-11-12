use super::super::enum_diagnostic_retains_symbolic;
use carina_core::validation::SymbolicFindingOrigin;

#[test]
fn unrenderable_enum_error_still_owns_the_generic_diagnostic_path() {
    use super::super::render_local_enum_diagnostic;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
    let root = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("main.crn", "# another buffer\n"),
        ("sibling.crn", "mock.EnumSink { value = 'bad' }\n"),
        ("providers.crn", "provider mock {}\n"),
    ] {
        std::fs::write(root.path().join(name), body).unwrap();
    }
    let doc = super::create_document_at("# another buffer\n", root.path().join("main.crn"));
    let sibling = super::create_document_at(
        "mock.EnumSink { value = 'bad' }\n",
        root.path().join("sibling.crn"),
    );
    let tree = carina_core::parser::AnalysisTree::try_from_file(sibling.parsed().unwrap()).unwrap();
    let resource = tree.project_resources().next().unwrap();
    let schema = ResourceSchema::new("EnumSink")
        .attribute(
            AttributeSchema::optional(
                "value",
                AttributeType::enum_(
                    TypeIdentity::from_dotted("mock.Choice"),
                    Some(vec!["abc".into()]),
                    Vec::new(),
                    None,
                    None,
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let value: carina_core::resource::Value = carina_core::resource::Value::Concrete(
        carina_core::resource::ConcreteValue::String("bad".into()),
    );
    let error = schema
        .effective_attribute("value")
        .unwrap()
        .validate(&value)
        .unwrap_err()
        .with_attribute("value");
    let finding = carina_core::parser::Located::new(
        resource
            .attribute_value_location("value", &carina_core::schema::FieldPath::new())
            .clone(),
        error,
    );
    let data = crate::code_action::enum_diagnostic_data_for_error(finding.detail());
    let handled = data.is_some();
    let outcome = render_local_enum_diagnostic(&doc, &finding, None, data.unwrap());
    assert!(outcome.is_none());
    let retained: Vec<_> = [
        SymbolicFindingOrigin::Ordinary,
        SymbolicFindingOrigin::AuthoredHook,
        SymbolicFindingOrigin::RecoveredReference,
    ]
    .into_iter()
    .filter(|origin| enum_diagnostic_retains_symbolic(handled, *origin))
    .collect();
    assert_eq!(retained, [SymbolicFindingOrigin::RecoveredReference]);
}

#[test]
fn rich_enum_diagnostic_continues_only_with_recovered_references() {
    let findings = [
        (SymbolicFindingOrigin::Ordinary, "ordinary symbolic error"),
        (SymbolicFindingOrigin::AuthoredHook, "authored hook error"),
        (
            SymbolicFindingOrigin::RecoveredReference,
            "source reference mismatch",
        ),
    ];
    let emitted: Vec<_> = findings
        .iter()
        .filter(|(origin, _)| enum_diagnostic_retains_symbolic(true, *origin))
        .map(|(_, message)| *message)
        .collect();
    assert_eq!(emitted, ["source reference mismatch"]);
    assert!(
        findings
            .iter()
            .all(|(origin, _)| enum_diagnostic_retains_symbolic(false, *origin))
    );
}
