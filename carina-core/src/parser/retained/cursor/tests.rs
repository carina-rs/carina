use super::*;
use crate::parser::retained::{ExprValue, judge_expressions};
use crate::parser::{ProviderContext, TypeExpr};
use crate::schema::SchemaRegistry;

fn slot(source: &str) -> JudgedSlot {
    let parsed = crate::parser::parse_and_resolve(source).unwrap();
    let expressions = parsed.parser_binding_metadata.syntax().expressions();
    let expression: ExprValue = expressions
        .binding(&RelativeBindingName::new("input"))
        .unwrap()
        .clone();
    let snapshot = Arc::new(judge_expressions(
        &parsed,
        &SchemaRegistry::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ));
    let mut slot = JudgedSlot::from_sink(
        SinkExpression {
            expression,
            payload: crate::eval_value::ParsedSinkValue::Ready(parsed.variables["input"].clone()),
        },
        crate::schema::SchemaKind::Resource,
        ExpansionInstanceId::fresh(),
    );
    slot.attach_snapshot(snapshot).unwrap();
    slot
}

#[test]
fn cursor_api_children_and_edges_share_the_judged_snapshot() {
    let slot = slot("let input = secret(('text'))");
    let AuthoredCursor::Authored(mut usage) = slot.cursor().unwrap() else {
        panic!("authored slot")
    };
    assert_eq!(usage.edge(), EdgeRole::SlotRoot);
    loop {
        let children = usage.node().children().collect::<Vec<_>>();
        for child in &children {
            assert_eq!(child.generation(), slot.generation());
            assert_eq!(
                child.location().declaration().file(),
                usage.node().location().declaration().file()
            );
        }
        let Some(child) = children.first() else { break };
        let role = if matches!(usage.node().kind(), ExprKind::Secret) {
            EdgeRole::SecretArgument
        } else {
            EdgeRole::Parens
        };
        let descended = usage.child(0, role).unwrap();
        assert_eq!(descended.edge(), role);
        assert_eq!(descended.node().node_id(), child.node_id());
        assert_eq!(descended.node().generation(), usage.node().generation());
        usage = descended;
    }
    assert_eq!(
        usage.node().judgement().static_type(),
        Ok(&TypeExpr::String)
    );
}

#[test]
fn cursor_api_rebuilding_a_projection_advances_its_generation() {
    let mut slot = slot("let input = 'before'");
    let snapshot = slot.snapshot.clone();
    let generation = slot.generation();
    let node = match slot.cursor().unwrap() {
        AuthoredCursor::Authored(usage) => usage.node().node_id(),
        _ => panic!("authored slot"),
    };
    slot.replace_projection(slot.projection().unwrap().clone());
    assert_ne!(slot.generation(), generation);
    assert!(slot.cursor().is_err());
    // Reattach only a snapshot for the same syntax identity.
    slot.attach_snapshot(snapshot).unwrap();
    let AuthoredCursor::Authored(usage) = slot.cursor().unwrap() else {
        panic!("authored slot")
    };
    assert_eq!(usage.node().generation(), slot.generation());
    assert_eq!(usage.node().node_id(), node);
}

#[test]
fn seam_slots_share_inventory_nodes_and_selected_schema_kind() {
    let mut parsed = crate::parser::parse_and_resolve(
        "let object = mock.Test { items = [{ value = 'x' }] }\nlet input = read mock.Test { value = 'query' }",
    ).unwrap();
    let schemas = SchemaRegistry::new();
    crate::parser::retained::judge_configuration(
        &mut parsed,
        &schemas,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let inventory = parsed.parser_binding_metadata.syntax().expressions();
    fn includes(expr: &ExprValue, id: NodeId) -> bool {
        expr.node_id() == id
            || expr
                .authored
                .children
                .iter()
                .any(|child| includes(child, id))
    }
    for resource in tree.resources() {
        let name = if resource.kind() == crate::parser::AnalysisItemKind::DataSource {
            "value"
        } else {
            "items"
        };
        let slot = resource.authored_slot(name).unwrap();
        let AuthoredCursor::Authored(usage) = slot.cursor().unwrap() else {
            panic!("authored slot")
        };
        assert!(
            inventory
                .roots()
                .iter()
                .any(|root| includes(root, usage.node().node_id()))
        );
        assert_eq!(
            slot.schema_kind(),
            if name == "value" {
                crate::schema::SchemaKind::DataSource
            } else {
                crate::schema::SchemaKind::Resource
            }
        );
    }
}

#[derive(Debug)]
struct Visit {
    kind: &'static str,
    edge: EdgeRole,
    discarded: bool,
    node: NodeId,
    usage: UseId,
    covered: bool,
    union: bool,
}

fn walk(source: &str, declaration: crate::schema::AttributeType) -> Vec<Visit> {
    use crate::schema::value_validation::{
        CheckValue, WalkCoverage, validate_authored_value_for_admission,
    };
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed =
        crate::parser::parse_file(source, std::path::Path::new("cursor.crn"), &catalog).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Test")
            .attribute(crate::schema::AttributeSchema::optional("input", declaration).build())
            .validate()
            .unwrap(),
    );
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .resources()
        .find(|resource| resource.id().resource_type == "Test")
        .unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let schema = resource.schema(&schemas).unwrap();
    let declaration = &schema.schema().as_schema().attributes()["input"].attr_type;
    let visits = std::cell::RefCell::new(Vec::new());
    let check = |declaration: &crate::schema::CompiledAttributeType,
                 value: CheckValue<'_, _, _>,
                 _: &crate::schema::LeafPath,
                 cursor: AuthoredCursor<'_>| {
        let (usage, covered) = match cursor {
            AuthoredCursor::Authored(usage) => (usage, false),
            AuthoredCursor::CoveredBy(usage) => (usage, true),
            AuthoredCursor::Projected(_) => panic!("authored fixture became projected"),
        };
        let kind = match usage.node().kind() {
            ExprKind::Scalar(_) => "scalar",
            ExprKind::List => "list",
            ExprKind::Map(_) => "map",
            ExprKind::If => "if",
            ExprKind::Coalesce => "coalesce",
            ExprKind::Call(_) => "call",
            ExprKind::Interpolation { .. } => "interpolation",
            ExprKind::Integer { .. } => "arithmetic",
            ExprKind::Access(_) | ExprKind::DynamicAccess => "access",
            ExprKind::Reference { .. } => "reference",
            _ => "forwarding",
        };
        visits.borrow_mut().push(Visit {
            kind,
            edge: usage.edge(),
            discarded: matches!(value, CheckValue::Discarded),
            node: usage.node().node_id(),
            usage: usage.occurrence(),
            covered,
            union: matches!(declaration.kind, crate::schema::AttrTypeKind::Union(_)),
        });
        Ok(Default::default())
    };
    let findings = validate_authored_value_for_admission(
        declaration,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        slot.cursor().unwrap(),
        &check,
        &mut WalkCoverage::default(),
    );
    assert!(findings.local_error().is_none(), "{findings:?}");
    visits.into_inner()
}

#[test]
fn seam_walk_literal_children_parens_and_secret_edges() {
    use crate::schema::AttributeType as T;
    let visits = walk(
        "mock.Test { input = [secret(('x'))] }",
        T::list(T::string()),
    );
    assert!(
        visits
            .iter()
            .any(|visit| visit.kind == "list" && visit.edge == EdgeRole::SlotRoot)
    );
    assert!(
        visits
            .iter()
            .any(|visit| visit.kind == "scalar" && visit.edge == EdgeRole::Parens)
    );
    let visits = walk("mock.Test { input = secret('x') }", T::string());
    assert!(
        visits
            .iter()
            .any(|visit| visit.kind == "scalar" && visit.edge == EdgeRole::SecretArgument)
    );
    let visits = walk("mock.Test { input = { a = 'x' } }", T::map(T::string()));
    assert!(
        visits
            .iter()
            .any(|visit| visit.kind == "scalar" && visit.edge == EdgeRole::MapValue)
    );
}

#[test]
fn seam_walk_known_if_and_coalesce_keep_distinct_discarded_uses() {
    use crate::schema::AttributeType as T;
    for source in [
        "mock.Test { input = if true { 'selected' } else { 'discarded' } }",
        "mock.Test { input = 'selected' ?? 'discarded' }",
    ] {
        let visits = walk(source, T::string());
        let alternatives = visits
            .iter()
            .filter(|visit| visit.kind == "scalar")
            .collect::<Vec<_>>();
        assert_eq!(alternatives.len(), 2, "{visits:?}");
        assert_eq!(
            alternatives.iter().filter(|visit| visit.discarded).count(),
            1,
            "{visits:?}"
        );
        assert_ne!(alternatives[0].node, alternatives[1].node);
        assert_eq!(
            alternatives[0].edge,
            if source.contains("if true") {
                EdgeRole::IfBranch
            } else {
                EdgeRole::CoalesceOperand
            }
        );
    }
}

#[test]
fn seam_walk_aliases_get_independent_definition_uses() {
    let visits = walk(
        "let shared = 'x'\nmock.Test { input = [shared, shared] }",
        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
    );
    let aliases = visits
        .iter()
        .filter(|visit| visit.kind == "scalar")
        .collect::<Vec<_>>();
    assert_eq!(aliases.len(), 2, "{visits:?}");
    assert_eq!(aliases[0].node, aliases[1].node);
    assert_ne!(aliases[0].usage, aliases[1].usage);
    assert!(
        aliases
            .iter()
            .all(|visit| visit.edge == EdgeRole::AliasDefinition)
    );
}

#[test]
fn seam_walk_computed_results_are_covered_without_visiting_operands() {
    use crate::schema::AttributeType as T;
    for (expression, declaration, expected) in [
        ("split(',', 'a,b')", T::list(T::string()), "call"),
        ("\"prefix-${1}\"", T::string(), "interpolation"),
        ("1 + 2", T::int(), "arithmetic"),
        ("({ a = ['x'] }).a", T::list(T::string()), "access"),
    ] {
        let visits = walk(
            &format!("mock.Test {{ input = {expression} }}"),
            declaration,
        );
        assert!(!visits.is_empty());
        assert!(
            visits
                .iter()
                .all(|visit| visit.covered && visit.kind == expected),
            "{visits:?}"
        );
    }
}

#[test]
fn seam_computed_operand_evidence_is_paired_by_parser_identity() {
    let mut parsed = crate::parser::parse_and_resolve(
        "arguments { input: Int }\nmock.Test { value = input + 1 }",
    )
    .unwrap();
    crate::parser::retained::judge_configuration(
        &mut parsed,
        &SchemaRegistry::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let slot = parsed.resources[0].authored_slots.get("value").unwrap();
    let Value::Deferred(crate::resource::DeferredValue::IntegerArithmetic(expression)) =
        slot.projection().unwrap()
    else {
        panic!("deferred arithmetic")
    };
    let AuthoredCursor::Authored(usage) = slot.cursor().unwrap() else {
        panic!("authored")
    };
    let node = usage
        .node()
        .retained_node(expression.operator_span().node_id().unwrap())
        .unwrap();
    let operands = node.children().collect::<Vec<_>>();
    assert_eq!(operands.len(), 2);
    assert_eq!(operands[0].judgement().static_type(), Ok(&TypeExpr::Int));
    assert_eq!(operands[1].judgement().static_type(), Ok(&TypeExpr::Int));
    assert_ne!(node.node_id(), usage.node().node_id());
}

fn with_directory(
    source: &[(&str, &str)],
    run: impl FnOnce(&crate::parser::ParsedFile, &SchemaRegistry, &ProviderContext),
) {
    let directory = tempfile::tempdir().unwrap();
    for (file, contents) in source {
        std::fs::write(directory.path().join(file), contents).unwrap();
    }
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &catalog,
    )
    .unwrap()
    .into_parsed();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Test")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "input",
                    crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    run(&parsed, &schemas, &catalog);
}

fn slot_coverage(
    parsed: &crate::parser::ParsedFile,
    schemas: &SchemaRegistry,
    catalog: &ProviderContext,
    run: impl FnOnce(&ExpressionReport, &crate::schema::value_validation::WalkCoverage),
) {
    use crate::schema::value_validation::{WalkCoverage, validate_authored_value_for_admission};
    let configuration = crate::config_loader::load_buffer_configuration(parsed, schemas, catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let mut coverage = WalkCoverage::default();
    for resource in tree.resources() {
        let slot = resource.authored_slot("input").unwrap();
        let proof = slot
            .projection()
            .unwrap()
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let schema = resource.schema(schemas).unwrap();
        let findings = validate_authored_value_for_admission(
            &schema.schema().as_schema().attributes()["input"].attr_type,
            &proof,
            schema.schema().as_schema().defs(),
            crate::schema::FieldAncestry::WRITABLE,
            &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
            slot.cursor().unwrap(),
            &|_, _, _, _| Ok(Default::default()),
            &mut coverage,
        );
        assert!(findings.local_error().is_none(), "{findings:?}");
    }
    run(judged.report(), &coverage);
}

#[test]
fn seam_directory_completeness_keeps_hidden_alias_use_and_dead_let() {
    with_directory(
        &[
            ("main.crn", "mock.Test { input = [shared, \"${shared}\"] }"),
            ("values.crn", "let shared = 'shared'\nlet dead = 'dead'"),
            ("providers.crn", "provider mock {}"),
        ],
        |parsed, schemas, catalog| {
            slot_coverage(parsed, schemas, catalog, |report, coverage| {
                let completeness = report.completeness_report(coverage);
                let unreached = completeness
                    .unreached(|node| matches!(node.kind(), ExprKind::Scalar(TypeExpr::String)))
                    .collect::<Vec<_>>();
                assert!(
                    unreached
                        .iter()
                        .any(|usage| usage.edge() == EdgeRole::AliasDefinition),
                    "{unreached:?}"
                );
                assert!(
                    unreached
                        .iter()
                        .any(|usage| usage.edge() == EdgeRole::SlotRoot),
                    "{unreached:?}"
                );
            });
        },
    );
}

#[test]
fn seam_completeness_and_alias_work_scale_linearly_without_routes() {
    let mut work = Vec::new();
    for count in [32, 64] {
        let mut source = "let first = 'x'\n".to_string();
        for index in 0..count {
            source.push_str(&format!(
                "let alias{index} = {}\n",
                if index == 0 {
                    "first".into()
                } else {
                    format!("alias{}", index - 1)
                }
            ));
        }
        source.push_str(&format!(
            "mock.Test {{ input = [alias{}, \"${{alias{}}}\"] }}",
            count - 1,
            count - 1
        ));
        with_directory(
            &[("main.crn", &source), ("providers.crn", "provider mock {}")],
            |parsed, schemas, catalog| {
                slot_coverage(parsed, schemas, catalog, |report, coverage| {
                    let completeness = report.completeness_report(coverage);
                    assert!(
                        completeness
                            .unreached(|node| matches!(node.kind(), ExprKind::Scalar(_)))
                            .next()
                            .is_some()
                    );
                    work.push(coverage.work() + completeness.work());
                });
            },
        );
    }
    assert!(work[1] <= 2 * work[0] + 32, "{work:?}");
}

#[test]
fn seam_alias_chains_do_not_share_their_final_use_identity() {
    let visits = walk(
        "let base = 'x'\nlet shared = base\nmock.Test { input = [shared, shared] }",
        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
    );
    let leaves = visits
        .iter()
        .filter(|visit| visit.kind == "scalar")
        .collect::<Vec<_>>();
    assert_eq!(leaves.len(), 2);
    assert_ne!(leaves[0].usage, leaves[1].usage);
}

#[test]
fn seam_loop_instances_share_syntax_but_own_roots_and_generations() {
    let mut parsed =
        crate::parser::parse_and_resolve("for item in ['a', 'b'] { mock.Test { input = [item] } }")
            .unwrap();
    crate::parser::retained::judge_configuration(
        &mut parsed,
        &SchemaRegistry::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let slots = parsed
        .resources
        .iter()
        .map(|resource| resource.authored_slots.get("input").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(slots.len(), 2);
    assert_ne!(slots[0].instance(), slots[1].instance());
    assert_ne!(slots[0].generation(), slots[1].generation());
    let node = |slot: &JudgedSlot| match slot.cursor().unwrap() {
        AuthoredCursor::Authored(usage) => usage.node().node_id(),
        _ => panic!("authored"),
    };
    assert_eq!(node(slots[0]), node(slots[1]));
}

#[test]
fn seam_module_instances_transport_substituted_slots_and_conditions() {
    module_instances("enabled", true);
}

#[test]
fn seam_module_computed_conditions_reuse_the_actual_selection() {
    module_instances("choose(enabled)", true);
}

#[test]
fn seam_expanded_completeness_uses_final_slot_roots() {
    module_instances("enabled", false);
}

fn module_instances(condition: &str, rejudge: bool) {
    let child_source = format!(
        "arguments {{ label: String enabled: Bool }}\nfn choose(flag: Bool): Bool {{ flag }}\nlet resource = mock.Test {{ input = if {condition} {{ [label] }} else {{ ['off'] }} }}"
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    for (file, source) in [
        (
            "main.crn",
            "let child = use { source = './child' }\nlet one = child { label = 'one' enabled = true }\nlet two = child { label = 'two' enabled = false }",
        ),
        ("providers.crn", "provider mock {}"),
        ("child/extra.crn", "let unused = 'directory fixture'"),
        ("child/main.crn", child_source.as_str()),
    ] {
        std::fs::write(directory.path().join(file), source).unwrap();
    }
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let root = crate::config_loader::parse_directory(&path, &catalog).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Test")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "input",
                    crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let mut parsed = crate::module_resolver::resolve_modules_with_providerless_config(
            root, &path, session, &schemas,
        )
        .unwrap();
        if rejudge {
            crate::parser::retained::judge_configuration(&mut parsed, &schemas, &catalog);
        }
        let slots = parsed
            .resources
            .iter()
            .map(|resource| {
                let slot = resource.authored_slots.get("input").unwrap();
                assert_eq!(slot.projection(), Some(&resource.attributes["input"]));
                let AuthoredCursor::Authored(usage) = slot.cursor().unwrap() else {
                    panic!("authored")
                };
                (
                    usage.node().node_id(),
                    usage.instance(),
                    usage.node().generation(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].0, slots[1].0);
        assert_ne!(slots[0].1, slots[1].1);
        assert_ne!(slots[0].2, slots[1].2);
        let mut coverage = Default::default();
        for resource in &parsed.resources {
            let slot = resource.authored_slots.get("input").unwrap();
            let proof = slot
                .projection()
                .unwrap()
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .unwrap()
                .into_classification();
            let schema = schemas.get_for(resource).unwrap();
            let findings = crate::schema::value_validation::validate_authored_value_for_admission(
                &schema.schema().as_schema().attributes()["input"].attr_type,
                &proof,
                schema.schema().as_schema().defs(),
                crate::schema::FieldAncestry::WRITABLE,
                &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
                slot.cursor().unwrap(),
                &|_, _, _, _| Ok(Default::default()),
                &mut coverage,
            );
            assert!(findings.local_error().is_none(), "{findings:?}");
        }
        let completeness = parsed.expression_report().completeness_report(&coverage);
        assert!(
            completeness.correspondence_errors().is_empty(),
            "{:?}",
            completeness.correspondence_errors()
        );
        assert_eq!(
            completeness
                .unreached(|node| slots.iter().any(|slot| slot.0 == node.node_id()))
                .count(),
            0
        );
    });
}

#[test]
fn seam_union_checks_the_borrowed_union_before_member_trials() {
    let visits = walk(
        "mock.Test { input = 'value' }",
        crate::schema::AttributeType::union(vec![
            crate::schema::AttributeType::string(),
            crate::schema::AttributeType::int(),
        ]),
    );
    assert!(visits[0].union, "{visits:?}");
    assert!(!visits[1].union, "{visits:?}");
    assert!(
        visits
            .iter()
            .all(|visit| visit.node == visits[0].node && visit.edge == EdgeRole::SlotRoot)
    );
}

#[test]
fn seam_pending_coalesce_operands_are_distinct_current_authored_nodes() {
    let visits = walk(
        "arguments { value: String }\nmock.Test { input = value ?? 'fallback' }",
        crate::schema::AttributeType::string(),
    );
    let operands = visits
        .iter()
        .filter(|visit| visit.edge == EdgeRole::CoalesceOperand)
        .collect::<Vec<_>>();
    assert_eq!(operands.len(), 2, "{visits:?}");
    assert_ne!(operands[0].node, operands[1].node);
    assert!(
        operands
            .iter()
            .all(|visit| !visit.discarded && !visit.covered)
    );
}

#[test]
fn seam_map_keys_are_covered_and_do_not_call_the_assignment_callback() {
    let visits = walk(
        "mock.Test { input = { key = 'value' } }",
        crate::schema::AttributeType::map(crate::schema::AttributeType::string()),
    );
    assert_eq!(visits.len(), 2, "{visits:?}");
    let slot = slot("let input = { key = 'value' }");
    let cursor = slot.cursor().unwrap().for_map_key();
    let AuthoredCursor::CoveredBy(usage) = cursor else {
        panic!("map key is covered")
    };
    assert_eq!(usage.edge(), EdgeRole::MapKey);
}

#[test]
fn seam_no_lookup_alias_child_has_its_exact_authored_node() {
    let visits = walk(
        "arguments { value: String }\nlet items = [value]\nmock.Test { input = items }",
        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
    );
    assert_eq!(visits.len(), 2, "{visits:?}");
    assert_eq!(visits[0].kind, "list");
    assert_ne!(visits[0].node, visits[1].node);
    assert_eq!(visits[1].edge, EdgeRole::ListItem);
    assert!(!visits[1].covered);
}

#[test]
fn seam_directory_replacement_keeps_reference_root_and_covers_descendants() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("producer.crn"),
        "let source = mock.Source { output = ['one'] }",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "mock.Test { input = source.output }",
    )
    .unwrap();
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &catalog,
    )
    .unwrap()
    .into_parsed();
    let mut schemas = SchemaRegistry::new();
    for (name, field) in [("Source", "output"), ("Test", "input")] {
        schemas.insert(
            "mock",
            crate::schema::ResourceSchema::new(name)
                .attribute(
                    crate::schema::AttributeSchema::optional(
                        field,
                        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
    }
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .resources()
        .find(|resource| resource.id().resource_type == "Test")
        .unwrap();
    let slot = resource.authored_slot("input").unwrap();
    assert_eq!(slot.projection(), Some(&resource.attributes()["input"]));
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let visits = std::cell::RefCell::new(Vec::new());
    let check = |_: &crate::schema::CompiledAttributeType,
                 _: crate::schema::value_validation::CheckValue<'_, _, _>,
                 _: &crate::schema::LeafPath,
                 cursor: AuthoredCursor<'_>| {
        visits.borrow_mut().push(match cursor {
            AuthoredCursor::Authored(usage) => {
                assert!(matches!(usage.node().kind(), ExprKind::Reference { .. }));
                false
            }
            AuthoredCursor::CoveredBy(usage) => {
                assert!(matches!(usage.node().kind(), ExprKind::Reference { .. }));
                true
            }
            _ => panic!("authored"),
        });
        Ok(Default::default())
    };
    let schema = resource.schema(&schemas).unwrap();
    let result = crate::schema::value_validation::validate_authored_value_for_admission(
        &schema.schema().as_schema().attributes()["input"].attr_type,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        slot.cursor().unwrap(),
        &check,
        &mut Default::default(),
    );
    assert!(result.local_error().is_none(), "{result:?}");
    assert_eq!(visits.into_inner(), vec![false, true]);
}

#[test]
fn seam_admission_coverage_accumulates_across_resource_slots() {
    with_directory(
        &[
            ("values.crn", "let shared = 'x'"),
            (
                "main.crn",
                "mock.Test { input = [shared] }\nmock.Test { input = [shared, \"${shared}\"] }",
            ),
        ],
        |parsed, schemas, catalog| {
            let configuration =
                crate::config_loader::load_buffer_configuration(parsed, schemas, catalog);
            let judged = configuration.judged();
            let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
            let bindings =
                crate::binding_index::BindingIndex::from_parsed(judged.file(), schemas, catalog);
            crate::validation::deferred_types::with_session_for_test(catalog, |session| {
                let mut coverage = crate::validation::admission::WalkCoverage::default();
                let mut counts = Vec::new();
                for resource in tree.project_resources() {
                    let values = resource.attributes();
                    let classified = crate::resource::knowledge::collect_classified_attributes(
                        values.iter().map(|(name, value)| (name.clone(), value)),
                        &crate::resource::knowledge::KnowledgeContext::validation(),
                        |_, error| panic!("{error}"),
                    );
                    let findings = crate::validation::admission::attributes::resource_attribute_admissions_with_coverage(resource, judged.file(), resource.schema(schemas).unwrap(), &classified, resource.quoted_string_attrs(), judged.report().for_resource(resource.id()).unwrap(), &bindings, session, schemas, &mut coverage);
                    assert_eq!(findings.errors().count(), 0, "{findings:?}");
                    counts.push(coverage.len());
                }
                assert!(counts[1] > counts[0]);
                let completeness = judged.report().completeness_report(&coverage);
                assert!(completeness.correspondence_errors().is_empty());
                let hidden = completeness
                    .unreached(|node| matches!(node.kind(), ExprKind::Scalar(_)))
                    .collect::<Vec<_>>();
                assert_eq!(hidden.len(), 1, "{hidden:?}");
                assert_eq!(hidden[0].edge(), EdgeRole::AliasDefinition);
            });
        },
    );
}

#[test]
fn seam_schema_block_aliases_keep_the_synthetic_authored_containers() {
    use crate::schema::{AttributeSchema, AttributeType as T, ResourceSchema, StructField};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut parsed =
        crate::parser::parse_and_resolve("mock.Test { item { rule { value = 'x' } } }").unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(
                AttributeSchema::optional(
                    "items",
                    T::list(T::struct_(
                        "Item",
                        vec![
                            StructField::optional(
                                "rules",
                                T::list(T::struct_(
                                    "Rule",
                                    vec![StructField::optional("value", T::string()).build()],
                                )),
                            )
                            .with_block_name("rule")
                            .build(),
                        ],
                    )),
                )
                .with_block_name("item")
                .build(),
            )
            .validate()
            .unwrap(),
    );
    crate::schema::resolve_file_block_names_with_schemas(&mut parsed, &schemas).unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let slot = resource.authored_slot("items").unwrap();
    let attributes = resource.attributes();
    let proof = attributes["items"]
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let schema = resource.schema(&schemas).unwrap();
    let findings = crate::schema::value_validation::validate_authored_value_for_admission(
        &schema.schema().as_schema().attributes()["items"].attr_type,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        slot.cursor().unwrap(),
        &|_, _, _, _| Ok(Default::default()),
        &mut Default::default(),
    );
    assert!(findings.local_error().is_none(), "{findings:?}");
}

#[test]
fn seam_deferred_loop_substitution_rebuilds_the_whole_slot() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse("let upstream = upstream_state { source = '../upstream' }\nfor value in upstream.values { mock.Test { input = [value] } }", &catalog).unwrap();
    let parsed = crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        crate::module_resolver::resolve_modules_providerless(
            crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
            &crate::paths::ResolvedPath::new(std::path::Path::new(".")).unwrap(),
            session,
        )
        .unwrap()
    })
    .into_expanded(&SchemaRegistry::new(), &catalog)
    .unwrap();
    let deferred = &parsed.deferred_for_expressions()[0];
    let generation = deferred
        .template_resource()
        .as_resource()
        .authored_slots
        .get("input")
        .unwrap()
        .generation();
    let values: Value = Value::Concrete(crate::resource::ConcreteValue::List(vec![
        Value::Concrete(crate::resource::ConcreteValue::String("a".into())),
        Value::Concrete(crate::resource::ConcreteValue::String("b".into())),
    ]));
    let known = values
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification()
        .into_known()
        .unwrap();
    let children = crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        crate::parser::expand_deferred_children(
            deferred,
            crate::resource::knowledge::PlanIterable::from_known(known).unwrap(),
            crate::resource::AccessMissPolicy::Error,
            session,
        )
        .unwrap()
    });
    assert_eq!(children.len(), 2);
    for resource in &children {
        let slot = resource.authored_slots.get("input").unwrap();
        assert_eq!(
            slot.projection(),
            Some(&resource.attributes["input"].clone().into_placeholder_type())
        );
        assert_ne!(slot.generation(), generation);
        assert!(
            slot.cursor().is_err(),
            "substitution must invalidate the old judgement"
        );
    }
    assert_ne!(
        children[0].authored_slots.get("input").unwrap().instance(),
        children[1].authored_slots.get("input").unwrap().instance()
    );
}

#[test]
fn seam_identity_metadata_preserves_existing_syntax_equality() {
    let first =
        crate::parser::parse_and_resolve("let value = 'x'\nmock.Test { input = [value] }").unwrap();
    let second =
        crate::parser::parse_and_resolve("let value = 'x'\nmock.Test { input = [value] }").unwrap();
    let left = first.parser_binding_metadata.syntax().expressions();
    let mut right = second
        .parser_binding_metadata
        .syntax()
        .expressions()
        .clone();
    // Preserve the pre-existing provider-authority pairing while comparing syntax.
    right.report = left.report.clone();
    assert_eq!(left, &right);
    let schemas = SchemaRegistry::new();
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let left = judge_expressions(&first, &schemas, &catalog);
    let mut right = left.clone();
    right.nodes = Arc::default();
    right.syntax_nodes = Arc::default();
    right.projections = Arc::default();
    assert_eq!(left, right);
}

#[test]
fn seam_known_if_preserves_the_single_main_local_obligation() {
    use crate::schema::{AttributeSchema, AttributeType as T, ResourceSchema, TypeIdentity};
    use crate::validation::admission::{Candidate, admit};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    let identity = Some(TypeIdentity::bare("Id"));
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(
                AttributeSchema::optional(
                    "output",
                    T::refined_string(identity.clone(), None, Some((None, Some(64))), None),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(
                AttributeSchema::optional(
                    "input",
                    T::refined_string(identity, None, Some((None, Some(36))), None),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_and_resolve("let source = mock.Source {}\nmock.Test { input = if true { source.output } else { source.output } }").unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let resource = &judged.file().resources[1];
    let slot = resource.authored_slots.get("input").unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(&resource.id, "input", &catalog)
        .unwrap();
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let proof = slot
            .projection()
            .unwrap()
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let pending = admit(&destination, candidate, &context)
            .unwrap()
            .ready()
            .unwrap_err();
        assert_eq!(pending.local_obligation().unwrap().paths().len(), 1);
    });
}

#[test]
fn seam_generated_prefix_result_has_explicit_projection_authority() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let mut parsed =
        crate::parser::parse_and_resolve("let target = mock.Test { name_prefix = 'test-' }")
            .unwrap();
    crate::identifier::resolve_attr_prefixes(&mut parsed.resources, &schemas).unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.project_resources().next().unwrap();
    assert!(resource.projected_phase("name").is_some());
    assert!(resource.projected_phase("missing").is_none());
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let attributes = resource.attributes();
        let classified = crate::resource::knowledge::collect_classified_attributes(
            attributes.iter().map(|(name, value)| (name.clone(), value)),
            &crate::resource::knowledge::KnowledgeContext::validation(),
            |_, error| panic!("{error}"),
        );
        let findings = crate::validation::admission::attributes::resource_attribute_admissions(
            resource,
            judged.file(),
            resource.schema(&schemas).unwrap(),
            &classified,
            resource.quoted_string_attrs(),
            judged.report().for_resource(resource.id()).unwrap(),
            &bindings,
            session,
            &schemas,
        );
        assert_eq!(findings.errors().count(), 0, "{findings:?}");
    });
}

#[test]
fn seam_substituted_loop_value_keeps_its_root_and_covers_descendants() {
    let visits = walk(
        "for item in [['x']] { mock.Test { input = item } }",
        crate::schema::AttributeType::list(crate::schema::AttributeType::string()),
    );
    assert_eq!(visits.len(), 2, "{visits:?}");
    assert!(!visits[0].covered);
    assert!(visits[1].covered);
    assert_eq!(visits[0].node, visits[1].node);
}

#[test]
fn seam_live_sink_capture_miss_is_a_typed_correspondence_failure() {
    let source = crate::parser::preprocess_file("mock.Test { input = 'value' }").unwrap();
    let path = std::path::Path::new("capture.crn");
    let tokens = source.parse().unwrap();
    let inventory = super::super::Expressions::from_pairs(tokens.clone(), &source, path);
    // A second token queue has different parser identities, even at equal ranges.
    let foreign = source
        .parse()
        .unwrap()
        .flatten()
        .find(|pair| pair.as_rule() == crate::parser::Rule::expression)
        .unwrap();
    let reduced = std::cell::Cell::new(false);
    let result = super::super::evaluate_sink(foreign, &source, path, &inventory.captures, || {
        reduced.set(true);
        Ok(crate::eval_value::ParsedSinkValue::Ready(Value::Concrete(
            crate::resource::ConcreteValue::String("value".into()),
        )))
    });
    assert!(
        matches!(result, Err(crate::parser::ParseError::RetainedEvidence(_))),
        "a live sink cannot recompile foreign syntax"
    );
    assert!(!reduced.get());
    drop(tokens);
}

#[test]
fn seam_completeness_tracks_the_materialized_slot_generation() {
    use crate::schema::{AttributeSchema, AttributeType as T, ResourceSchema};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", T::list(T::string())).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "mock.Test { input = ['authored', join('', ['hidden'])] }",
        std::path::Path::new("materialized.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let resource = &judged.file().resources[0];
    let original = resource.authored_slots.get("input").unwrap();
    let materialized = Value::Concrete(crate::resource::ConcreteValue::List(vec![
        Value::Concrete(crate::resource::ConcreteValue::String("authored".into())),
        Value::Concrete(crate::resource::ConcreteValue::String("hidden".into())),
    ]));
    let slot = original.materialized(&materialized);
    assert_ne!(slot.generation(), original.generation());
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let schema = schemas.get_for(resource).unwrap();
    let mut coverage = Default::default();
    let findings = crate::schema::value_validation::validate_authored_value_for_admission(
        &schema.schema().as_schema().attributes()["input"].attr_type,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        slot.cursor().unwrap(),
        &|_, _, _, _| Ok(Default::default()),
        &mut coverage,
    );
    assert!(findings.local_error().is_none(), "{findings:?}");
    let completeness = judged.report().completeness_report(&coverage);
    assert_eq!(
        completeness
            .unreached(|node| matches!(node.kind(), ExprKind::Scalar(TypeExpr::String)))
            .count(),
        2
    );
    let mut next = judged.file().clone();
    let slot = next.resources[0]
        .authored_slots
        .get("input")
        .unwrap()
        .clone();
    next.resources[0]
        .authored_slots
        .slots
        .get_mut("input")
        .unwrap()
        .replace_projection(slot.projection().unwrap().clone());
    crate::parser::retained::judge_configuration(&mut next, &schemas, &catalog);
    let stale = next.expression_report().completeness_report(&coverage);
    assert_eq!(
        stale
            .unreached(|node| matches!(node.kind(), ExprKind::Scalar(TypeExpr::String)))
            .count(),
        3
    );
}

#[test]
fn seam_storage_owner_address_survives_independent_source_views() {
    let source = "mock.Test { input = 'one' }\nmock.Test { input = 'two' }";
    let one = crate::parser::parse_and_resolve(source).unwrap();
    let two = crate::parser::parse_and_resolve(source).unwrap();
    let addresses = |file: &crate::parser::ParsedFile| {
        file.resources
            .iter()
            .map(|resource| resource.authored_slots.declaration_id().unwrap().clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(addresses(&one), addresses(&two));
    assert_ne!(addresses(&one)[0], addresses(&one)[1]);
    assert_ne!(
        one.resources[0]
            .authored_slots
            .get("input")
            .unwrap()
            .root
            .id,
        two.resources[0]
            .authored_slots
            .get("input")
            .unwrap()
            .root
            .id
    );
}

#[test]
fn seam_missing_resource_snapshot_invalidates_a_previous_cursor() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let parsed = crate::parser::parse_and_resolve("mock.Test { input = 'value' }").unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let mut file = judged.file().clone();
    assert!(
        file.resources[0]
            .authored_slots
            .get("input")
            .unwrap()
            .cursor()
            .is_ok()
    );
    super::super::attach_slot_reports(&mut file, &Arc::new(ExpressionReport::empty()));
    assert!(
        file.resources[0]
            .authored_slots
            .get("input")
            .unwrap()
            .cursor()
            .is_err(),
        "a missing live snapshot cannot retain old judgement"
    );
}

#[test]
fn seam_deferred_read_aliases_use_the_selected_data_source_schema() {
    use crate::schema::{
        AttributeSchema, AttributeType as T, ResourceSchema, SchemaKind, StructField,
    };
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_file("let source = mock.Source {}\nlet lookups = for item in source.items { read mock.Lookup { filter { value = item } } }", std::path::Path::new("deferred-read.crn"), &catalog).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Lookup")
            .attribute(AttributeSchema::optional("different", T::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Lookup")
            .as_data_source()
            .attribute(
                AttributeSchema::optional(
                    "filters",
                    T::list(T::struct_(
                        "Filter",
                        vec![StructField::optional("value", T::string()).build()],
                    )),
                )
                .with_block_name("filter")
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let tree = crate::parser::AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
    let read = tree
        .resources()
        .find(|resource| resource.id().resource_type == "Lookup")
        .unwrap();
    assert_eq!(
        read.authored_slot("filter").unwrap().schema_kind(),
        SchemaKind::DataSource
    );
    assert_eq!(
        read.authored_slot("filters").unwrap().schema_kind(),
        SchemaKind::DataSource
    );
}

#[test]
fn seam_pending_sink_retains_authored_storage_and_unreached_use() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let parsed = crate::parser::parse_file(
        "let source = mock.Source {}\nmock.Test { input = source block { value = source } }",
        std::path::Path::new("pending.crn"),
        &catalog,
    )
    .unwrap();
    assert_eq!(parsed.bare_binding_uses().pending_slots().count(), 2);
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    for name in ["input", "block"] {
        let slot = judged.file().resources[1].authored_slots.get(name).unwrap();
        assert!(slot.projection().is_none());
        let AuthoredCursor::Authored(usage) = slot.cursor().unwrap() else {
            panic!("authored pending sink")
        };
        let child = judged.file().resources[1].instantiate();
        let child_slot = child.authored_slots.get(name).unwrap();
        assert_ne!(child_slot.generation(), slot.generation());
        assert!(child_slot.cursor().is_err());
        let completeness = judged.report().completeness_report(&Default::default());
        assert_eq!(
            completeness
                .unreached(|node| node.node_id() == usage.node().node_id())
                .count(),
            1
        );
    }
}

#[test]
fn seam_directory_replacement_transports_canonical_block_slots() {
    use crate::resource::ConcreteValue as C;
    use crate::schema::{AttributeSchema, AttributeType as T, ResourceSchema, StructField};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(
                AttributeSchema::optional(
                    "items",
                    T::list(T::struct_(
                        "Item",
                        vec![StructField::optional("value", T::string()).build()],
                    )),
                )
                .with_block_name("item")
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut parsed = crate::parser::parse_file(
        "mock.Test { item { value = 'before' } }",
        std::path::Path::new("replacement.crn"),
        &catalog,
    )
    .unwrap();
    crate::parser::retained::judge_configuration(&mut parsed, &schemas, &catalog);
    let slots = &mut parsed.resources[0].authored_slots;
    let generation = slots.get("items").unwrap().generation();
    let replacement = Value::Concrete(C::List(vec![Value::Concrete(C::Map(
        indexmap::IndexMap::from([("value".into(), Value::Concrete(C::String("after".into())))]),
    ))]));
    slots.replace_projections(&indexmap::IndexMap::from([(
        "items".into(),
        replacement.clone(),
    )]));
    let slot = slots.get("items").unwrap();
    assert_eq!(slot.projection(), Some(&replacement));
    assert_ne!(slot.generation(), generation);
    assert!(slot.cursor().is_err());
}
