use super::*;

#[test]
fn conversion_missing_operand_judgement_uses_only_the_conversion_channel() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = crate::conversion::tests::catalog();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "input",
                    AttributeType::refined_string(
                        Some(crate::schema::TypeIdentity::from_dotted("mock.Name")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "mock.Sink { input = 'ok' as mock.Name }",
        std::path::Path::new("missing-operand.crn"),
        &catalog,
    )
    .unwrap();
    let configuration = crate::config_loader::load_buffer_configuration(
        crate::config_loader::issue_buffer_configuration(&parsed),
        &schemas,
        &catalog,
    );
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let mut slot = resource.authored_slot("input").unwrap().clone();
    let operand = slot
        .cursor()
        .unwrap()
        .canonical()
        .unwrap()
        .node
        .node
        .children[0]
        .node_id();
    let mut snapshot = slot.snapshot.as_ref().clone();
    let instance = snapshot.instance.clone();
    Arc::make_mut(&mut snapshot.nodes).remove(&(instance, operand));
    slot.snapshot = Arc::new(snapshot);
    let cursor = AuthoredCursor::authored(slot.cursor().unwrap().canonical().unwrap());
    let schema = resource.schema(&schemas).unwrap();
    let declaration = &schema.schema().as_schema().attributes()["input"].attr_type;
    let outcome = super::super::check_conversion::<
        crate::validation::admission::kind::ResourceAttribute,
        ParserPlaceholder,
        RelativeBindingName,
    >(
        declaration,
        crate::schema::value_validation::CheckValue::Discarded,
        &crate::schema::LeafPath::from_segments([]),
        cursor.clone(),
        schema.schema().as_schema().defs(),
        crate::parser::retained::ConversionContext {
            catalog: &catalog,
            reference: crate::validation::admission::ReferenceSource::Unchecked,
        },
    )
    .expect("a conversion cannot return an ordinary seam error");
    let findings = match outcome {
        crate::schema::value_validation::AuthoredOutcome::Claimed(witness) => {
            witness.consume(declaration, &cursor).unwrap().decide(
                |_, _| panic!("missing evidence cannot accept a conversion"),
                |findings| findings,
            )
        }
        crate::schema::value_validation::AuthoredOutcome::Continue(_, _)
        | crate::schema::value_validation::AuthoredOutcome::Transport(_) => {
            panic!("a conversion needs a verdict")
        }
    };
    assert!(findings.errors.is_empty());
    assert_eq!(findings.conversions.findings().len(), 1);
    assert_eq!(
        findings.conversions.findings()[0].kind(),
        crate::ConversionErrorKind::Source
    );
}

#[test]
fn ordinary_if_and_coalesce_missing_children_do_not_claim_a_conversion() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = crate::conversion::tests::catalog();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    for value in ["if true { 'ok' } else { 'fine' }", "'ok' ?? 'fine'"] {
        let parsed = crate::parser::parse_file(
            &format!("mock.Sink {{ input = {value} }}"),
            std::path::Path::new("ordinary-children.crn"),
            &catalog,
        )
        .unwrap();
        let configuration = crate::config_loader::load_buffer_configuration(
            crate::config_loader::issue_buffer_configuration(&parsed),
            &schemas,
            &catalog,
        );
        let tree =
            crate::parser::AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let mut slot = resource.authored_slot("input").unwrap().clone();
        let node = slot
            .cursor()
            .unwrap()
            .canonical()
            .unwrap()
            .node
            .node
            .clone();
        let mut snapshot = slot.snapshot.as_ref().clone();
        for child in &node.children {
            Arc::make_mut(&mut snapshot.nodes)
                .remove(&(snapshot.instance.clone(), child.node_id()));
        }
        slot.snapshot = Arc::new(snapshot);
        let cursor = AuthoredCursor::authored(slot.cursor().unwrap().canonical().unwrap());
        let schema = resource.schema(&schemas).unwrap();
        let declaration = &schema.schema().as_schema().attributes()["input"].attr_type;
        let outcome = super::super::check_conversion::<
            crate::validation::admission::kind::ResourceAttribute,
            ParserPlaceholder,
            RelativeBindingName,
        >(
            declaration,
            crate::schema::value_validation::CheckValue::Discarded,
            &crate::schema::LeafPath::from_segments([]),
            cursor,
            schema.schema().as_schema().defs(),
            crate::parser::retained::ConversionContext {
                catalog: &catalog,
                reference: crate::validation::admission::ReferenceSource::Unchecked,
            },
        )
        .expect("ordinary alternatives cannot fail the conversion seam");
        assert!(
            matches!(
                outcome,
                crate::schema::value_validation::AuthoredOutcome::Continue(_, _)
            ),
            "ordinary {value} was claimed"
        );
    }
}
