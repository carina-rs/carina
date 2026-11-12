use super::*;
use crate::parser::retained::{AuthoredCursor, CursorKind, EdgeRole, NodeId, UseId};
use crate::resource::{ParserPlaceholder, RelativeBindingName};
use crate::schema::value_validation::{AssignmentFindings, CheckValue, visit_discarded};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Position {
    node: NodeId,
    usage: UseId,
    edge: EdgeRole,
    path: LeafPath,
}

#[derive(Debug)]
struct Visit {
    position: Position,
    discarded: bool,
    declaration: String,
}

struct Trace {
    live: Vec<Visit>,
    discarded: Vec<Visit>,
    findings: usize,
}

fn trace(source: &str, declaration: AttributeType, emit: bool) -> Trace {
    trace_materialized(source, declaration, emit, None)
}

fn trace_materialized(
    source: &str,
    declaration: AttributeType,
    emit: bool,
    projection: Option<&crate::resource::Value<ParserPlaceholder, RelativeBindingName>>,
) -> Trace {
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", declaration).build())
            .validate()
            .unwrap(),
    );
    let parsed =
        crate::parser::parse_file(source, std::path::Path::new("round4.crn"), &catalog).unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let source_slot = resource.authored_slot("input").unwrap();
    let materialized = projection.map(|value| source_slot.materialized(value));
    let slot = materialized.as_ref().unwrap_or(source_slot);
    let root_id = slot.cursor().unwrap().canonical().unwrap().node().node_id();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(resource.id(), "input", &catalog)
        .unwrap();
    let live = RefCell::new(Vec::new());
    let discarded = RefCell::new(Vec::new());
    let control = Cell::new(false);
    let coverage = RefCell::new(crate::schema::value_validation::WalkCoverage::default());
    let hook = |attr: &CompiledAttributeType,
                value: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                path: &LeafPath,
                cursor: AuthoredCursor<'_>| {
        let usage = match cursor.kind {
            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => usage,
            CursorKind::Projected(_) => panic!("lost authored position"),
        };
        let visit = Visit {
            position: Position {
                node: usage.node().node_id(),
                usage: usage.occurrence(),
                edge: usage.edge(),
                path: path.clone(),
            },
            discarded: matches!(value, CheckValue::Discarded),
            declaration: attr.type_name(),
        };
        if control.get() {
            assert!(
                coverage.borrow().reached(&usage),
                "authored position lacks coverage: {visit:?}"
            );
            discarded.borrow_mut().push(visit);
        } else {
            live.borrow_mut().push(visit);
        }
        Ok(AssignmentFindings {
            local: if emit && !control.get() && usage.node().node_id() == root_id {
                vec![assignment::LocalPath::for_recheck(
                    kind::ResourceAttribute,
                    path.clone(),
                )]
            } else {
                Vec::new()
            },
            ..Default::default()
        })
    };
    let findings = with_session_for_test(&catalog, |session| {
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let untyped = Vec::new;
        let context = context.with_checks(
            AssignmentChecks {
                authored: Some(&hook),
                ordinary: None,
            },
            &untyped,
        );
        let judgement =
            assignment::judge_with_findings(&destination, &candidate, &context).unwrap();
        *coverage.borrow_mut() = context.coverage.take();
        // judge_with_findings moves local tokens into the verdict. Reading
        // take_assignment() afterwards would count the emptied input bundle.
        match judgement.verdict {
            AssignmentVerdict::NeedsLocal(local) => local.paths().len(),
            AssignmentVerdict::Proven(_) | AssignmentVerdict::NeedsSchema(_) => 0,
        }
    });
    control.set(true);
    let schema = resource.schema(&schemas).unwrap();
    visit_discarded(
        &schema.schema().as_schema().attributes()["input"].attr_type,
        schema.schema().as_schema().defs(),
        &crate::schema::FieldPath::new().push_field("input"),
        crate::schema::FieldAncestry::WRITABLE,
        slot.cursor().unwrap(),
        crate::schema::value_validation::AssignmentCallbacks {
            ordinary: &|_, _, _, _| Ok(Default::default()),
            authored: Some(&hook),
        },
        &mut Default::default(),
    )
    .unwrap();
    Trace {
        live: live.into_inner(),
        discarded: discarded.into_inner(),
        findings,
    }
}

fn exactly_once(visits: &[Visit]) {
    let mut seen = HashSet::new();
    for visit in visits {
        assert!(
            seen.insert((visit.position.node, visit.position.usage)),
            "authored hook invoked more than once: {visits:?}"
        );
    }
}

fn same_navigation(trace: Trace) {
    exactly_once(&trace.live);
    exactly_once(&trace.discarded);
    let live = trace
        .live
        .iter()
        .map(|visit| visit.position.clone())
        .collect::<HashSet<_>>();
    let discarded = trace
        .discarded
        .iter()
        .map(|visit| visit.position.clone())
        .collect::<HashSet<_>>();
    assert_eq!(
        live, discarded,
        "live: {:?}; discarded: {:?}",
        trace.live, trace.discarded
    );
}

#[test]
fn round4_parens_hooks_match_live_and_discarded_navigation() {
    same_navigation(trace(
        "mock.Sink { input = if true { (('live')) } else { (('dead')) } }",
        AttributeType::string(),
        false,
    ));
}

#[test]
fn round4_secret_hooks_match_live_and_discarded_navigation() {
    same_navigation(trace(
        "mock.Sink { input = if true { secret(('live')) } else { secret(('dead')) } }",
        AttributeType::string(),
        false,
    ));
}

#[test]
fn round4_alias_chain_hooks_match_live_and_discarded_navigation() {
    same_navigation(trace(
        "let base = 'value'\nlet first = base\nlet second = first\nmock.Sink { input = if true { second } else { second } }",
        AttributeType::string(),
        false,
    ));
}

#[test]
fn round4_discarded_union_list_items_reach_hook_and_coverage() {
    let trace = trace(
        "mock.Sink { input = if true { 'live' } else { ['dead'] } }",
        AttributeType::union(vec![
            AttributeType::list(AttributeType::string()),
            AttributeType::string(),
        ]),
        false,
    );
    assert!(
        trace
            .live
            .iter()
            .any(|visit| visit.discarded && visit.position.edge == EdgeRole::ListItem),
        "{:?}",
        trace.live
    );
}

#[test]
fn round4_discarded_union_map_values_reach_hook_and_coverage() {
    let trace = trace(
        "mock.Sink { input = if true { 'live' } else { { 'a.b' = 'dead' } } }",
        AttributeType::union(vec![
            AttributeType::map(AttributeType::string()),
            AttributeType::string(),
        ]),
        false,
    );
    assert!(
        trace
            .live
            .iter()
            .any(|visit| visit.discarded && visit.position.edge == EdgeRole::MapValue),
        "{:?}",
        trace.live
    );
}

#[test]
fn round4_union_retains_one_hook_finding_from_selected_member() {
    let trace = trace(
        "mock.Sink { input = 'value' }",
        AttributeType::union(vec![AttributeType::int(), AttributeType::string()]),
        true,
    );
    assert_eq!(trace.findings, 1, "{:?}", trace.live);
    assert!(trace.live.iter().any(|visit| visit.declaration == "String"));
    exactly_once(&trace.live);
    exactly_once(&trace.discarded);
}

#[test]
fn round5_union_projected_container_has_one_contribution_per_authored_use() {
    let value = crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(vec![
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::Int(1)),
    ]));
    let trace = trace_materialized(
        "arguments { values: list(Int) }\nmock.Sink { input = values }",
        AttributeType::union(vec![
            AttributeType::list(AttributeType::int()),
            AttributeType::list(AttributeType::float()),
        ]),
        true,
        Some(&value),
    );
    assert_eq!(
        trace.findings, 1,
        "a lowered item is not a second authored use: {:?}",
        trace.live
    );
    same_navigation(trace);
}

#[test]
fn round4_map_value_paths_match_live_and_discarded_navigation() {
    same_navigation(trace(
        "mock.Sink { input = if true { { 'a.b' = 'live' } } else { { 'a.b' = 'dead' } } }",
        AttributeType::map(AttributeType::string()),
        false,
    ));
}

#[test]
fn round5_live_computed_parts_reach_authored_hook() {
    for (source, declaration, role) in [
        (
            "let base = 2\nmock.Sink { input = base + 1 }",
            AttributeType::int(),
            EdgeRole::ArithmeticOperand,
        ),
        (
            "let base = 'value'\nmock.Sink { input = \"${base}\" }",
            AttributeType::string(),
            EdgeRole::InterpolationPart,
        ),
        (
            "fn identity(value: String): String { value }\nlet base = 'value'\nmock.Sink { input = identity(base) }",
            AttributeType::string(),
            EdgeRole::CallArgument,
        ),
        (
            "arguments { base: map(String) }\nmock.Sink { input = base['key'] }",
            AttributeType::string(),
            EdgeRole::AccessBase,
        ),
    ] {
        let trace = trace(source, declaration, false);
        assert!(
            trace.live.iter().any(|visit| visit.position.edge == role),
            "{role:?}: {:?}",
            trace.live
        );
        if role != EdgeRole::AccessBase {
            assert!(
                trace
                    .live
                    .iter()
                    .any(|visit| visit.position.edge == EdgeRole::AliasDefinition),
                "computed parts must follow value aliases: {:?}",
                trace.live
            );
        }
        same_navigation(trace);
    }
}

#[test]
fn round5_module_body_slots_keep_per_instance_authored_roots() {
    use crate::resource::AbsoluteBindingAddress;
    use crate::validation::admission::attributes::ResourceAttributeInputs;
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    for (file, source) in [
        (
            "main.crn",
            "let child = use { source = './child' }\nlet first = child { x = 'one' }\nlet second = child { x = 'two' }\n",
        ),
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("exports.crn", "exports {}\n"),
        ("child/arguments.crn", "arguments { x: String }\n"),
        ("child/main.crn", "let body = mock.Sink { input = x }\n"),
        ("child/exports.crn", "exports {}\n"),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &catalog).unwrap();
    with_session_for_test(&catalog, |session| {
        let expanded = crate::module_resolver::resolve_modules_with_providerless_config(
            parsed, &path, session, &schemas,
        )
        .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&expanded, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut identities = Vec::new();
        let mut values = Vec::new();
        for resource in tree
            .resources()
            .filter(|resource| resource.id().resource_type == "Sink")
        {
            let slot = resource.authored_slot("input").unwrap();
            let root = slot.cursor().unwrap().canonical().unwrap();
            let inputs = ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap();
            let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
                panic!("{error}")
            });
            let mut coverage = Default::default();
            let report = attributes::resource_attribute_admissions(
                resource,
                judged.file(),
                resource.schema(&schemas).unwrap(),
                &classified,
                resource.quoted_string_attrs(),
                &inputs,
                &bindings,
                session,
                &schemas,
                &mut coverage,
            );
            assert_eq!(report.errors().count(), 0, "{report:?}");
            assert_eq!(report.symbolic_errors().count(), 0, "{report:?}");
            assert!(
                coverage.reached(&root),
                "module-body slot took ordinary-only admission"
            );
            let visits = RefCell::new(Vec::new());
            let hook = |_: &CompiledAttributeType,
                        _: CheckValue<'_, ParserPlaceholder, AbsoluteBindingAddress>,
                        _: &LeafPath,
                        cursor: AuthoredCursor<'_, AbsoluteBindingAddress>| {
                let CursorKind::Authored(usage) = cursor.kind else {
                    panic!("module-body slot was projected or covered");
                };
                visits.borrow_mut().push((
                    usage.node().node_id(),
                    usage.instance(),
                    usage.node().environment().id(),
                ));
                Ok(Default::default())
            };
            let proof = slot
                .projection()
                .unwrap()
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification();
            let (candidate, context) =
                Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
            let untyped = Vec::new;
            let context = context.with_checks(
                AssignmentChecks {
                    authored: Some(&hook),
                    ordinary: None,
                },
                &untyped,
            );
            let destination = schemas
                .resource_attribute_destination(resource.id(), "input", &catalog)
                .unwrap();
            assert!(admit(&destination, candidate, &context).is_ok());
            assert!(
                visits
                    .borrow()
                    .iter()
                    .any(|(node, instance, _)| *node == root.node().node_id()
                        && *instance == slot.instance())
            );
            identities.push((
                root.node().node_id(),
                slot.instance(),
                root.node().environment().id(),
            ));
            values.push(slot.projection().unwrap().clone());
        }
        assert_eq!(identities.len(), 2);
        assert_eq!(
            identities[0].0, identities[1].0,
            "same authored module node"
        );
        assert_ne!(
            identities[0].1, identities[1].1,
            "distinct expansion instances"
        );
        assert_ne!(
            identities[0].2, identities[1].2,
            "distinct binding environments"
        );
        assert_ne!(
            values[0], values[1],
            "substituted arguments must not cross instances"
        );
    });
}
