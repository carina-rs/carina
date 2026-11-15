use super::*;
use crate::parser::ProviderContext;
use crate::schema::{
    PatternAttributeSchema as Attribute, PatternAttributeType as Type,
    PatternResourceSchema as Schema,
};

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}
fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        Schema::new("test.Resource")
            .attribute(Attribute::optional("name", Type::string()).build())
            .attribute(Attribute::optional("comment", Type::string()).build())
            .attribute(Attribute::optional("n", Type::int()).build())
            .attribute(Attribute::optional("tags", Type::map(Type::string())).build())
            .attribute(Attribute::provider_populated("identifier", Type::string()).build())
            .validate()
            .unwrap(),
    );
    schemas
}
fn parsed(source: &str) -> crate::parser::ParsedFile {
    crate::parser::parse_file(source, std::path::Path::new("secrecy.crn"), &context()).unwrap()
}
fn report(source: &str) -> SecrecyReport {
    let file = parsed(source);
    secrecy_report(&file, &schemas())
}
fn secrecy_report(file: &crate::parser::ParsedFile, schemas: &SchemaRegistry) -> SecrecyReport {
    crate::config_loader::load_buffer_configuration(file, schemas, &context())
        .judged()
        .secrecy()
        .clone()
}
fn attribute(report: &SecrecyReport, binding: &str, name: &str) -> Evidence {
    let file_id = report
        .rows
        .iter()
        .find(|(id, _)| id.identity_str() == Some(binding))
        .map(|(_, row)| row);
    file_id
        .unwrap_or_else(|| panic!("missing row {binding}: {report:?}"))
        .attributes[name]
        .clone()
}

#[test]
fn references_operands_interpolation_and_selected_paths_retain_secrecy() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" n = secret(3) tags = { token = secret("canary") public = "visible" } }
let alias = seed.tags
let token = alias.token
let sink = mock.test.Resource {
  name = "sink"
  comment = upper("${token}-${seed.n + 1}")
  tags = { private = token public = alias.public }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    assert_eq!(
        attribute(&report, "sink", "tags"),
        Evidence::Map(BTreeMap::from([
            ("private".into(), Evidence::Leaf(Secrecy::Secret)),
            ("public".into(), Evidence::Leaf(Secrecy::Public)),
        ]))
    );
}

#[test]
fn function_body_defaults_locals_and_captured_siblings_are_structural() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" }
let captured = { token = secret("canary") }
fn protect(x: String): String { secret(x) }
fn ignored(x: String): String { secret("constant") }
fn captured_value(x: String): String { captured.token }
fn with_default(x: String = secret("default")): String { let local = x local }
let sink = mock.test.Resource {
  name = "sink"
  comment = protect(seed.identifier)
  tags = { ignored = ignored(seed.identifier) captured = captured_value("literal") defaulted = with_default() }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment").secrecy(),
        Secrecy::Secret
    );
    assert_eq!(
        uniform(&attribute(&report, "sink", "tags")),
        Uniformity::Known(Secrecy::Secret)
    );
}

#[test]
fn finite_nested_calls_to_the_same_function_are_not_recursion() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" }
fn protect(x: String): String { secret(x) }
let sink = mock.test.Resource { name = "sink" comment = protect(protect(seed.identifier)) }
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment").secrecy(),
        Secrecy::Secret
    );
}

#[test]
fn deferred_mutual_recursion_uses_the_existing_function_error() {
    let source = r#"
fn first(x: String): String { second(x) }
fn second(x: String): String { first(x) }
let seed = mock.test.Resource { name = "seed" }
let sink = mock.test.Resource { name = "sink" comment = first(seed.identifier) }
"#;
    // The function prepass rejects this cycle before secrecy can run, even
    // though the authored call argument is a deferred provider attribute.
    let error = crate::parser::parse_file(source, std::path::Path::new("secrecy.crn"), &context())
        .expect_err("mutual recursion must fail during loading");
    assert!(
        matches!(
            &error,
            crate::parser::ParseError::Located { path, error, .. }
                if path == std::path::Path::new("secrecy.crn")
                    && matches!(error.as_ref(), crate::parser::ParseError::RecursiveFunction(name) if name == "first")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.diagnostic_message(),
        crate::parser::ParseError::RecursiveFunction("first".into()).to_string()
    );
}

#[test]
fn discarded_and_wrapped_coalesces_are_checked_at_the_operator() {
    let source =
        "mock.test.Resource { name = 'sink' comment = secret('public' ?? secret('canary')) }\n";
    let report = report(source);
    assert_eq!(report.findings().len(), 1, "{report:?}");
    let finding = &report.findings()[0];
    let span = finding.location().span();
    assert_eq!(span.end_column() - span.start_column(), 2);
    assert_eq!(span.start_column(), source.find("??").unwrap() + 1);
    assert_eq!(
        finding.message(),
        "Attribute 'comment': ?? alternatives mix secret and public values; all alternatives must be secret or all public."
    );
    assert!(!format!("{report:?}").contains("canary"));
}

#[test]
fn operator_location_skips_question_marks_in_trivia() {
    let source = "mock.test.Resource { name = 'sink' comment = 'public' /* ?? */ ?? /* ?? */ secret('canary') }\n";
    let report = report(source);
    let span = report.findings()[0].location().span();
    assert_eq!(span.start_column(), source.find("?? /*").unwrap() + 1);
}

#[test]
fn lookup_checks_alias_function_map_members_and_unused_default() {
    let report = report(
        r#"
let choices = { private = secret("canary") public = "visible" }
fn candidates(values: map(String)): map(String) { values }
let sink = mock.test.Resource { name = "sink" comment = lookup(candidates(choices), "public", "fallback") }
"#,
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Lookup
        })
    );
}

#[test]
fn map_alternatives_cannot_exchange_secret_and_public_paths() {
    let report = report(
        r#"
let sink = mock.test.Resource {
  name = "sink"
  tags = { a = secret("x") b = "public" } ?? { a = "public" b = secret("y") }
}
"#,
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert!(report.admitted().is_none());
}

#[test]
fn empty_collections_agree_with_public_and_secret_alternatives() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        Schema::new("test.Collections")
            .attribute(Attribute::optional("list", Type::list(Type::string())).build())
            .attribute(Attribute::optional("map", Type::map(Type::string())).build())
            .validate()
            .unwrap(),
    );
    for (value, expected) in [
        ("secret('x')", Secrecy::Secret),
        ("'public'", Secrecy::Public),
    ] {
        let list = format!("[{value}] ?? []");
        let map = format!("{{ k = {value} }} ?? {{}}");
        let file = parsed(&format!(
            "let sink = mock.test.Collections {{ list = {list} map = {map} }}"
        ));
        let report = secrecy_report(&file, &schemas);
        assert!(report.findings().is_empty(), "{list}, {map}: {report:?}");
        assert!(!report.is_pending(), "{report:?}");
        assert!(report.admitted().is_some());
        for name in ["list", "map"] {
            assert_eq!(attribute(&report, "sink", name).secrecy(), expected);
        }
    }

    // Keep the left values deferred through a provider-populated attribute.
    // Treating the empty defaults as Public would reject these Secret lefts.
    let file = parsed(
        r#"
let pending = mock.test.Resource {}
let src = mock.test.Collections {
  list = secret([pending.identifier])
  map = secret({ k = pending.identifier })
}
let sink = mock.test.Collections { list = src.list ?? [] map = src.map ?? {} }
"#,
    );
    let report = secrecy_report(&file, &schemas);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    assert!(report.admitted().is_some());
    for binding in ["src", "sink"] {
        for name in ["list", "map"] {
            assert_eq!(
                attribute(&report, binding, name),
                Evidence::Leaf(Secrecy::Secret)
            );
        }
    }

    let file = parsed(
        "mock.test.Collections { list = ['public'] ?? [secret('x')] map = { k = 'public' } ?? { k = secret('x') } }",
    );
    let report = secrecy_report(&file, &schemas);
    assert_eq!(report.findings().len(), 2, "{report:?}");
    assert!(
        report
            .findings()
            .iter()
            .all(|finding| matches!(finding.cause(), DerivationError::MixedAlternatives(_)))
    );
    assert!(report.admitted().is_none());
}

#[test]
fn attribute_less_module_resources_keep_distinct_expansion_rows() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        // The loader needs a module signature; the resource still has no attributes.
        "arguments { unused: String = 'default' }\nmock.test.Resource {}\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "provider mock {}\nlet child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n",
    ).unwrap();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let root = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
        .unwrap()
        .parsed;
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            root, &base, session, &schemas,
        )
        .unwrap()
    });
    let report = crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
        .judged()
        .secrecy()
        .clone();
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    assert_eq!(report.rows.len(), 2, "{report:?}");
    let mut rows = report.rows.values();
    let first = rows.next().unwrap();
    let second = rows.next().unwrap();
    assert_eq!(first.declaration, second.declaration);
    assert_ne!(
        first.instance.expect("first resource instance"),
        second.instance.expect("second resource instance")
    );
    assert!(first.attributes.is_empty() && second.attributes.is_empty());
    assert!(report.admitted().is_some());
}

#[test]
fn direct_and_referenced_attribute_walks_share_the_slot_environment() {
    let report = report(
        "let token = secret('canary')\nlet seed = mock.test.Resource { name = 'seed' comment = token }\nlet sink = mock.test.Resource { name = 'sink' comment = seed.comment }\n",
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.admitted().is_some());
    let stats = report
        .traversals()
        .iter()
        .find(|stats| stats.attribute == "comment" && stats.reference_visits == 2)
        .expect("the sink encounters seed.comment and token");
    assert_eq!(
        stats.memo_hits, 1,
        "the direct token walk must populate the same memo key"
    );
    assert_eq!(stats.maximum_depth, 2);
}

#[test]
fn repeated_errors_block_memo_admission_and_deduplicate_only_in_the_report() {
    let file = parsed(
        "let invalid = secret('public' ?? secret('canary'))\nmock.test.Resource { comment = join('', [invalid, invalid, invalid]) }\n",
    );
    let schemas = schemas();
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
    let owner = Owner {
        syntax: &syntax,
        schemas: &schemas,
        resources: IndexMap::new(),
        upstream: HashSet::new(),
        memo: RefCell::default(),
        empty_environment: Arc::default(),
    };
    let resource = &file.resources[0];
    let slot = resource.authored_slots.get("comment").unwrap();
    let source = syntax
        .secrecy_root(&resource.id, slot.secrecy_node())
        .unwrap();
    let mut walk = Walk::new(&owner, "comment".into(), source.location().clone());
    assert_eq!(
        walk.run(Thunk {
            source: Source::Syntax(source),
            environment: owner.empty_environment.clone(),
        }),
        Some(Evidence::Leaf(Secrecy::Secret))
    );
    assert_eq!(walk.visits, 3);
    assert_eq!(
        walk.memo_hits, 0,
        "no traversal with a repeated error may enter the memo"
    );
    assert_eq!(walk.findings.len(), 3);
    assert!(owner.memo.borrow().is_empty());
    let mut report = SecrecyReport::default();
    report.add_findings(walk.findings);
    assert_eq!(report.findings().len(), 1);
}

#[test]
fn schema_valid_unauthored_source_path_is_public_and_invalid_path_stays_pending() {
    let public = report(
        "let seed = mock.test.Resource { name = 'seed' }\nlet sink = mock.test.Resource { name = 'sink' comment = seed.identifier ?? 'default' }\n",
    );
    assert!(public.findings().is_empty(), "{public:?}");
    assert_eq!(
        attribute(&public, "sink", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    let invalid = report(
        "let seed = mock.test.Resource { name = 'seed' }\nlet sink = mock.test.Resource { name = 'sink' comment = seed.typo ?? 'default' }\n",
    );
    assert!(invalid.is_pending());
    assert!(invalid.admitted().is_none());
}

#[test]
fn missing_schema_never_manufactures_public_evidence() {
    for source in [
        "let seed = mock.test.Resource { name = 'seed' }\nlet sink = mock.test.Resource { name = 'sink' comment = seed.identifier ?? secret('default') }\n",
        "let source = read mock.test.Resource { name = 'source' }\n",
    ] {
        let file = parsed(source);
        let report = secrecy_report(&file, &SchemaRegistry::new());
        assert!(report.is_pending());
        assert!(report.findings().is_empty());
        assert!(report.admitted().is_none());
    }
}

#[test]
fn uninstantiated_module_cannot_mint_a_graph_or_a_d2_verdict() {
    for source in [
        "arguments { value: String }\nmock.test.Resource { name = 'sink' comment = value ?? secret('default') }\n",
        "attributes { output: String = 'public' ?? secret('default') }\n",
    ] {
        let report = report(source);
        assert!(report.is_pending());
        assert!(report.findings().is_empty());
        assert!(report.admitted().is_none());
    }
}

#[test]
fn unavailable_builtin_keeps_argument_secrecy_separate_from_failure() {
    let file = parsed("mock.test.Resource { name = 'sink' comment = 'public' }\n");
    let schemas = schemas();
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
    let owner = Owner {
        syntax: &syntax,
        schemas: &schemas,
        resources: IndexMap::new(),
        upstream: HashSet::new(),
        memo: RefCell::default(),
        empty_environment: Arc::default(),
    };
    let value = Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![
            Value::Concrete(ConcreteValue::String("canary".into()))
                .into_secret()
                .into(),
        ],
        reason: crate::resource::UnavailableReason::from_builtin_error("unavailable".into()),
        source_line: Default::default(),
    });
    let location = file.resources[0].declaration.location().unwrap().clone();
    let mut walk = Walk::new(&owner, "comment".into(), location);
    assert_eq!(
        walk.run(Thunk {
            source: Source::Value(Arc::new(value)),
            environment: Arc::default()
        }),
        Some(Evidence::Leaf(Secrecy::Secret))
    );
    assert!(walk.findings.is_empty());
}

#[derive(Clone, Copy, Debug)]
enum ChainShape {
    SingleFile,
    SiblingFiles,
}

const CHAIN_SHAPES: [ChainShape; 2] = [ChainShape::SingleFile, ChainShape::SiblingFiles];

fn resource_chain(last: usize, shape: ChainShape) -> SecrecyReport {
    use std::fmt::Write as _;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let mut resources =
        "let r0 = mock.test.Resource { name = 'r0' comment = secret('canary') }\n".to_string();
    for index in 1..last {
        writeln!(
            resources,
            "let r{index} = mock.test.Resource {{ name = 'r{index}' comment = r{}.comment }}",
            index - 1
        )
        .unwrap();
    }
    let consumer = format!(
        "let r{last} = mock.test.Resource {{ name = 'r{last}' comment = r{}.comment }}\n",
        last - 1
    );
    let parsed = match shape {
        ChainShape::SingleFile => {
            std::fs::remove_file(directory.path().join("providers.crn")).unwrap();
            let source = "provider mock {}\n".to_string() + &resources + &consumer;
            let path = directory.path().join("main.crn");
            std::fs::write(&path, &source).unwrap();
            crate::parser::parse_file(&source, &path, &context()).unwrap()
        }
        ChainShape::SiblingFiles => {
            std::fs::write(directory.path().join("resources.crn"), resources).unwrap();
            std::fs::write(directory.path().join("main.crn"), consumer).unwrap();
            let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
            crate::config_loader::parse_directory(&path, &context())
                .unwrap()
                .parsed()
                .clone()
        }
    };
    crate::config_loader::load_buffer_configuration(&parsed, &schemas(), &context())
        .judged()
        .secrecy()
        .clone()
}

#[test]
fn issue_4474_reference_chain_of_127_reaches_successful_validation_in_both_shapes() {
    for shape in CHAIN_SHAPES {
        let report = resource_chain(127, shape);
        assert!(report.findings().is_empty(), "{shape:?}: {report:?}");
        assert!(!report.is_pending(), "{shape:?}: {report:?}");
        assert!(report.admitted().is_some());
        assert!(report.traversals().iter().any(|stats| {
            stats.attribute == "comment" && stats.maximum_depth == 127 && stats.limit.is_none()
        }));
    }
}

#[test]
fn issue_4474_reference_chain_of_128_reports_the_depth_limit_in_both_shapes() {
    for shape in CHAIN_SHAPES {
        let report = resource_chain(128, shape);
        assert_eq!(report.findings().len(), 1, "{shape:?}: {report:?}");
        assert_eq!(
            report.findings()[0].cause(),
            &DerivationError::LimitReached(LimitReached {
                kind: LimitKind::Depth
            })
        );
        assert_eq!(
            report.findings()[0].message(),
            "Attribute 'comment': the value follows a chain of 128 or more reference or expression steps from this attribute; Carina cannot decide whether it is a secret."
        );
        assert!(report.admitted().is_none());
        assert!(report.traversals().iter().any(|stats| {
            stats.attribute == "comment"
                && stats.maximum_depth == 128
                && stats.limit == Some(LimitKind::Depth)
        }));
    }
}

#[test]
fn issue_4474_deeper_chains_report_errors_without_stack_growth_in_both_shapes() {
    for shape in CHAIN_SHAPES {
        for last in [256, 5000] {
            let report = resource_chain(last, shape);
            assert_eq!(report.findings().len(), last - 127, "{shape:?}, {last}");
            assert!(report.findings().iter().all(|finding| finding.cause()
                == &DerivationError::LimitReached(LimitReached {
                    kind: LimitKind::Depth
                })));
            assert!(report.admitted().is_none());
        }
    }
}

#[test]
fn reference_visits_pin_4096_accepted_and_4097_rejected_including_memo_hits() {
    for visits in [4096, 4097] {
        let resource = |name: &str, count: usize| {
            format!(
                "let {name} = mock.test.Resource {{ name = '{name}' comment = join('', [{}]) }}\n",
                vec!["r0.comment"; count].join(", ")
            )
        };
        let source = "let r0 = mock.test.Resource { name = 'r0' comment = secret('canary') }\n"
            .to_string()
            + &resource("first", 4096)
            + &resource("second", visits);
        let report = report(&source);
        let expected = usize::from(visits == 4097);
        assert_eq!(report.findings().len(), expected, "{report:?}");
        if visits == 4097 {
            assert_eq!(
                report.findings()[0].cause(),
                &DerivationError::LimitReached(LimitReached {
                    kind: LimitKind::ReferenceVisits
                })
            );
            assert_eq!(
                report.findings()[0].message(),
                "Attribute 'comment': deciding whether the value is a secret needs more than 4096 reference lookups from this attribute."
            );
            assert!(report.admitted().is_none());
        } else {
            assert!(!report.is_pending(), "{report:?}");
            assert!(report.admitted().is_some());
        }
        assert!(report.traversals().iter().any(|stats| {
            stats.attribute == "comment" && stats.reference_visits == 4096 && stats.limit.is_none()
        }));
        assert!(report.traversals().iter().any(|stats| {
            stats.attribute == "comment"
                && stats.reference_visits == visits
                && stats.limit == (visits == 4097).then_some(LimitKind::ReferenceVisits)
        }));
    }
}

#[test]
fn depth_keeps_charging_expression_steps_as_well_as_reference_hops() {
    use std::fmt::Write as _;
    let mut source =
        "let r0 = mock.test.Resource { name = 'r0' comment = secret('canary') }\n".to_string();
    for index in 1..=64 {
        writeln!(
            source,
            "let r{index} = mock.test.Resource {{ name = 'r{index}' comment = upper(r{}.comment) }}",
            index - 1
        )
        .unwrap();
    }
    let report = report(&source);
    // Each upper input and each resource reference costs one depth step.
    // This is the existing counting rule, not a new limit for function calls.
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].cause(),
        &DerivationError::LimitReached(LimitReached {
            kind: LimitKind::Depth
        })
    );
    assert!(report.traversals().iter().any(|stats| {
        stats.attribute == "comment"
            && stats.maximum_depth == 128
            && stats.limit == Some(LimitKind::Depth)
    }));
}

#[test]
fn upstream_origins_are_public_until_explicitly_wrapped_at_the_use_site() {
    let report = report(
        r#"
let up = upstream_state { source = "./up" }
let sink = mock.test.Resource {
  name = "sink"
  comment = up.output ?? "default"
  tags = { protected = secret(up.output) public = up.output }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    assert_eq!(
        attribute(&report, "sink", "tags"),
        Evidence::Map(BTreeMap::from([
            ("protected".into(), Evidence::Leaf(Secrecy::Secret)),
            ("public".into(), Evidence::Leaf(Secrecy::Public)),
        ]))
    );
}

#[test]
fn deferred_direct_recursion_uses_the_existing_function_error() {
    let source = r#"
fn recurse(x: String): String { recurse(x) }
let seed = mock.test.Resource { name = "seed" }
let sink = mock.test.Resource { name = "sink" comment = recurse(seed.identifier) }
"#;
    // Loading owns this error, before a judged secrecy graph can exist.
    let error = crate::parser::parse_file(source, std::path::Path::new("secrecy.crn"), &context())
        .expect_err("direct recursion must fail during loading");
    assert!(
        matches!(
            &error,
            crate::parser::ParseError::Located { path, error, .. }
                if path == std::path::Path::new("secrecy.crn")
                    && matches!(error.as_ref(), crate::parser::ParseError::RecursiveFunction(name) if name == "recurse")
        ),
        "{error:?}"
    );
    assert_eq!(
        error.diagnostic_message(),
        crate::parser::ParseError::RecursiveFunction("recurse".into()).to_string()
    );
}

#[test]
fn static_if_uses_the_selected_branch_without_a_mixed_if_rule() {
    let report = report(
        r#"
let sink = mock.test.Resource {
  name = "sink"
  comment = if true { secret("canary") } else { "public" }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
}

#[test]
fn traversal_counters_measure_real_cache_hits_and_per_attribute_resets() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" comment = secret("canary") }
let sink = mock.test.Resource { name = "sink" comment = join("", [seed.comment, seed.comment]) }
"#,
    );
    let stats = report
        .traversals()
        .iter()
        .find(|stats| stats.attribute == "comment" && stats.reference_visits == 2)
        .unwrap();
    assert!(stats.maximum_depth > 0);
    assert_eq!(stats.memo_hits, 1);
    assert!(stats.limit.is_none());
}

#[test]
fn authored_finite_loop_elements_preserve_homogeneous_selected_paths() {
    let report = report(
        r#"
for item in [{ name = "one" token = secret("x") }, { name = "two" token = secret("y") }] {
  mock.test.Resource { name = item.name comment = item.token ?? secret("default") }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(report.rows.len(), 2);
    assert!(
        report
            .rows
            .values()
            .all(|row| row.attributes["comment"].secrecy() == Secrecy::Secret)
    );
}

#[test]
fn authored_finite_loop_element_cannot_hide_a_mixed_coalesce() {
    let report = report(
        r#"
for item in [{ name = "one" token = secret("x") }, { name = "two" token = secret("y") }] {
  mock.test.Resource { name = item.name comment = item.token ?? "default" }
}
"#,
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce
        })
    );
}

#[test]
fn loop_instances_keep_their_own_authored_if_decisions() {
    let report = report(
        r#"
for item in [{ name = "one" private = true }, { name = "two" private = false }] {
  mock.test.Resource {
    name = item.name
    comment = if item.private { secret("canary") } else { "public" }
  }
}
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(report.rows.len(), 2, "{report:?}");
    // Report rows preserve expansion iteration order; name is only an attribute.
    let mut rows = report.rows.values();
    let first = rows.next().expect("first loop instance");
    let second = rows.next().expect("second loop instance");
    assert_eq!(first.declaration, second.declaration);
    assert_ne!(
        first.instance.expect("first expansion identity"),
        second.instance.expect("second expansion identity")
    );
    assert_eq!(first.attributes["comment"], Evidence::Leaf(Secrecy::Secret));
    assert_eq!(
        second.attributes["comment"],
        Evidence::Leaf(Secrecy::Public)
    );
}

#[test]
fn function_resource_parameters_follow_the_authored_attribute_path() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" tags = { token = secret("canary") public = "visible" } }
fn token(r: mock.test.Resource): String { r.tags.token }
fn public_tag(r: mock.test.Resource): String { r.tags.public }
let sink = mock.test.Resource { name = "sink" comment = token(seed) tags = { public = public_tag(seed) } }
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    assert_eq!(
        attribute(&report, "sink", "tags"),
        Evidence::Map(BTreeMap::from([(
            "public".into(),
            Evidence::Leaf(Secrecy::Public),
        )]))
    );
    assert!(report.admitted().is_some(), "{report:?}");
}

#[test]
fn projected_provider_values_cannot_replace_authored_secret_evidence() {
    let mut file = parsed(
        r#"
let seed = mock.test.Resource { name = "seed" comment = secret("canary") }
let sink = mock.test.Resource { name = "sink" comment = seed.comment ?? "default" }
"#,
    );
    file.resources[0].attributes.insert(
        "comment".into(),
        Value::Concrete(ConcreteValue::String("observed-public-value".into())),
    );
    let report = secrecy_report(&file, &schemas());
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce
        })
    );
    assert!(report.admitted().is_none());
}

#[test]
fn independent_resource_attributes_share_one_location_index() {
    use std::fmt::Write as _;
    let mut source = String::new();
    for index in 0..1000 {
        writeln!(
            source,
            "let r{index} = mock.test.Resource {{ name = 'r{index}' comment = 'c' }}"
        )
        .unwrap();
    }
    let parsed = parsed(&source);
    let (report, builds) =
        crate::parser::retained::count_secrecy_index_builds(|| secrecy_report(&parsed, &schemas()));
    assert_eq!(
        builds, 1,
        "attribute lookup must reuse one syntax index per report"
    );
    let secrecy = &report;
    assert!(secrecy.findings().is_empty());
    assert!(!secrecy.is_pending());
    assert_eq!(secrecy.rows.len(), 1000);
    let admitted = secrecy.admitted().unwrap();
    assert!(Arc::ptr_eq(&secrecy.rows, &admitted.rows));
    assert!(Arc::ptr_eq(&admitted.rows, &admitted.clone().rows));
    for row in secrecy.rows.values() {
        assert_eq!(row.attributes.len(), 2);
        assert!(
            row.attributes
                .values()
                .all(|value| value == &Evidence::Leaf(Secrecy::Public))
        );
    }
}
