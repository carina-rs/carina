use super::*;
use crate::parser::ProviderContext;
use crate::schema::{
    PatternAttributeSchema as Attribute, PatternAttributeType as Type,
    PatternResourceSchema as Schema,
};

mod deferred_children;
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
            .attribute(Attribute::provider_populated("enabled", Type::bool()).build())
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

const WAIT_ALIAS_TARGET: &str = r#"
let seed = mock.test.Resource { name = "visible" comment = secret("canary") }
"#;
const WAIT_ALIAS_CONSUMER: &str = r#"
let ready = wait seed { until = seed.enabled == true }
let sink = mock.test.Resource {
  name = "sink"
  tags = { private = ready.comment public = ready.name }
}
"#;

fn assert_wait_alias_evidence(report: &SecrecyReport) {
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(
        !report.is_pending(),
        "wait alias left the file pending: {report:?}"
    );
    assert!(report.admitted().is_some(), "{report:?}");
    assert_eq!(report.rows.len(), 2, "waits must not add managed rows");
    assert_eq!(
        attribute(report, "sink", "tags"),
        Evidence::Map(
            BTreeMap::from([
                ("private".into(), Evidence::Leaf(Secrecy::Secret)),
                ("public".into(), Evidence::Leaf(Secrecy::Public)),
            ])
            .into()
        )
    );
}

#[test]
fn wait_alias_preserves_target_secrecy_in_single_file() {
    assert_wait_alias_evidence(&report(&format!(
        "{WAIT_ALIAS_TARGET}{WAIT_ALIAS_CONSUMER}"
    )));
}

#[test]
fn wait_alias_preserves_target_secrecy_across_sibling_files() {
    let directory = tempfile::tempdir().unwrap();
    for (name, source) in [
        ("providers.crn", "provider mock {}\n"),
        ("main.crn", WAIT_ALIAS_TARGET),
        ("wait.crn", WAIT_ALIAS_CONSUMER),
    ] {
        std::fs::write(directory.path().join(name), source).unwrap();
    }
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let file = crate::config_loader::parse_directory(&path, &context()).unwrap();
    assert_wait_alias_evidence(&secrecy_report(file.parsed(), &schemas()));
}

#[test]
fn wait_alias_with_unresolved_target_stays_pending() {
    let report = report(
        r#"
let ready = wait missing { until = missing.enabled == true }
let sink = mock.test.Resource { name = "sink" comment = ready.comment }
"#,
    );
    assert!(
        report.is_pending(),
        "unresolved wait target was admitted: {report:?}"
    );
    assert!(report.admitted().is_none(), "{report:?}");
}

#[test]
fn wait_alias_target_attribute_reports_its_own_mixed_finding() {
    let report = report(
        r#"
let seed = mock.test.Resource { name = "seed" comment = secret("canary") ?? "public" }
let ready = wait seed { until = seed.comment == "ready" }
let sink = mock.test.Resource { name = "sink" comment = ready.name }
"#,
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].message(),
        "Attribute 'comment': ?? alternatives mix secret and public values; all alternatives must be secret or all public."
    );
    assert!(report.admitted().is_none(), "{report:?}");
}

#[test]
fn wait_alias_secret_attribute_still_reports_mixed_coalesce_with_public_until() {
    let report = report(&format!(
        "{WAIT_ALIAS_TARGET}\n\
         let ready = wait seed {{ until = seed.enabled == true }}\n\
         let sink = mock.test.Resource {{ name = 'sink' comment = ready.comment ?? 'public' }}"
    ));
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce
        })
    );
}

#[test]
fn wait_alias_public_attribute_does_not_inherit_secret_until_evidence() {
    assert_public_wait_attribute_matches_direct_reference("'p'", 0);
}

#[test]
fn wait_alias_public_attribute_mixed_with_secret_matches_direct_reference() {
    assert_public_wait_attribute_matches_direct_reference("secret('z')", 1);
}

fn assert_public_wait_attribute_matches_direct_reference(alternative: &str, findings: usize) {
    let target = "let seed = mock.test.Resource { name = 'seed' comment = secret('x') }\n";
    let wait = "let ready = wait seed { until = seed.comment == 'x' }\n";
    for directory in [false, true] {
        let layout = if directory {
            "directory"
        } else {
            "single file"
        };
        let derive = |reference: &str| {
            let consumer = format!(
                "let sink = mock.test.Resource {{ name = 'sink' comment = {reference} ?? {alternative} }}\n"
            );
            if directory {
                wait_alias_directory_report(
                    &[
                        ("providers.crn", "provider mock {}\n"),
                        ("resources.crn", target),
                        ("wait.crn", wait),
                        ("main.crn", &consumer),
                    ],
                    false,
                )
            } else {
                report(&format!("{target}{wait}{consumer}"))
            }
        };
        let alias = derive("ready.name");
        let direct = derive("seed.name");
        assert_eq!(alias.findings().len(), findings, "{layout}");
        assert_eq!(alias.findings().len(), direct.findings().len(), "{layout}");
        for (alias, direct) in alias.findings().iter().zip(direct.findings()) {
            assert_eq!(alias.cause(), direct.cause(), "{layout}");
            assert_eq!(
                alias.cause(),
                &DerivationError::MixedAlternatives(MixedAlternatives {
                    operator: Alternatives::Coalesce
                }),
                "{layout}"
            );
        }
        assert_eq!(alias.is_pending(), direct.is_pending(), "{layout}");
        assert_eq!(
            alias.admitted().is_some(),
            direct.admitted().is_some(),
            "{layout}"
        );
        assert_eq!(alias.admitted().is_some(), findings == 0, "{layout}");
        if findings == 0 {
            assert!(!alias.is_pending(), "{layout}");
            for report in [&alias, &direct] {
                assert_eq!(
                    attribute(report, "sink", "comment"),
                    Evidence::Leaf(Secrecy::Public),
                    "{layout}"
                );
            }
        }
    }
}

#[test]
fn wait_alias_until_rejects_inline_coalesce_and_interpolation() {
    // Until's RHS must be a closed scalar; neither spelling reaches secrecy derivation.
    for (predicate, interpolation) in [
        ("seed.name == (secret('canary') ?? 'public')", false),
        ("seed.name == \"${secret('canary') ?? 'public'}\"", true),
    ] {
        let source = format!("{WAIT_ALIAS_TARGET}let ready = wait seed {{ until = {predicate} }}");
        let error =
            crate::parser::parse_file(&source, std::path::Path::new("secrecy.crn"), &context())
                .expect_err("until only accepts a closed RHS");
        let mut detail = &error;
        loop {
            match detail {
                crate::parser::ParseError::Located { error, .. } => detail = error,
                crate::parser::ParseError::AtElement(finding) => detail = finding.detail(),
                _ => break,
            }
        }
        if interpolation {
            assert!(
                matches!(
                    detail,
                    crate::parser::ParseError::InterpolationInConstraintString {
                        kind: crate::parser::ConstraintStringPosition::WaitCondition,
                        ..
                    }
                ),
                "{error:?}"
            );
            assert_eq!(
                error.diagnostic_message(),
                "interpolation is not supported in a `wait` condition"
            );
        } else {
            let crate::parser::ParseError::ProtectedFile(failure) = detail else {
                panic!("expected protected syntax error, got {error:?}");
            };
            assert!(matches!(
                failure.kind(),
                crate::parser::ParseFailureKind::Syntax
            ));
            assert_eq!(failure.range().start_line, 3);
            assert_eq!(
                failure.range().start_column,
                source.lines().nth(2).unwrap().find("??").unwrap() + 1
            );
        }
    }
}

fn assert_wait_alias_pending(report: &SecrecyReport) {
    assert!(report.is_pending(), "{report:?}");
    assert!(report.admitted().is_none(), "{report:?}");
    assert!(report.findings().is_empty(), "{report:?}");
}

#[test]
fn wait_alias_invalid_backward_chain_stays_pending_without_following_another_wait() {
    let report = report(&format!(
        "{WAIT_ALIAS_TARGET}\n\
         let first = wait seed {{ until = seed.enabled == true }}\n\
         let ready = wait first {{ until = first.enabled == true }}\n\
         let sink = mock.test.Resource {{ name = 'sink' comment = ready.comment }}"
    ));
    assert_wait_alias_pending(&report);
}

#[test]
fn wait_alias_invalid_forward_chain_stays_pending() {
    let report = report(&format!(
        "{WAIT_ALIAS_TARGET}\n\
         let ready = wait first {{ until = first.enabled == true }}\n\
         let first = wait seed {{ until = seed.enabled == true }}\n\
         let sink = mock.test.Resource {{ name = 'sink' comment = ready.comment }}"
    ));
    assert_wait_alias_pending(&report);
}

#[test]
fn wait_alias_to_deferred_for_never_admits_public_evidence_for_a_secret_target() {
    let file = parsed(
        r#"
let seed = mock.test.Resource { name = "seed" }
let targets = for key, value in seed.tags {
  mock.test.Resource { name = key comment = secret("canary") }
}
let ready = wait targets { until = targets.enabled == true }
let sink = mock.test.Resource { name = "sink" comment = ready.comment }
"#,
    );
    assert_eq!(file.deferred_for_phase.expressions.len(), 1);
    assert_wait_alias_pending(&secrecy_report(&file, &schemas()));
}

#[test]
fn wait_alias_to_deferred_if_never_admits_public_evidence_for_a_secret_target() {
    let file = parsed(
        r#"
arguments { enabled: Bool }
let conditional = if enabled {
  mock.test.Resource { name = "conditional" comment = secret("canary") }
}
let ready = wait conditional { until = conditional.enabled == true }
let sink = mock.test.Resource { name = "sink" comment = ready.comment }
"#,
    );
    assert_eq!(file.deferred_module_ifs.len(), 1);
    assert_wait_alias_pending(&secrecy_report(&file, &schemas()));
}

#[test]
fn wait_alias_to_data_source_uses_observed_evidence_instead_of_query_secrecy() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        Schema::new("test.Resource")
            .as_data_source()
            .attribute(Attribute::optional("comment", Type::string()).build())
            .attribute(Attribute::provider_populated("enabled", Type::bool()).build())
            .validate()
            .unwrap(),
    );
    let file = parsed(
        r#"
let lookup = read mock.test.Resource { comment = secret("query") }
let ready = wait lookup { until = lookup.enabled == true }
let sink = mock.test.Resource { name = "sink" comment = ready.comment tags = { direct = lookup.comment } }
"#,
    );
    let report = secrecy_report(&file, &schemas);
    // Provider observations are Public even when their query input is Secret.
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    assert_eq!(
        attribute(&report, "sink", "tags"),
        Evidence::Map(BTreeMap::from([("direct".into(), Evidence::Leaf(Secrecy::Public))]).into())
    );
}

fn wait_alias_directory_report(files: &[(&str, &str)], expand_modules: bool) -> SecrecyReport {
    let directory = tempfile::tempdir().unwrap();
    for (name, source) in files {
        let path = directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let root = crate::config_loader::parse_directory(&path, &context).unwrap();
    if expand_modules {
        let expanded =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::module_resolver::resolve_modules_with_providerless_config(
                    root, &path, session, &schemas,
                )
                .unwrap()
            });
        crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
            .judged()
            .secrecy()
            .clone()
    } else {
        secrecy_report(root.parsed(), &schemas)
    }
}

#[test]
fn wait_retained_target_binding_survives_expansion_and_address_rewriting() {
    let leaf = "arguments { label: String }\nlet seed = mock.test.Resource { name = label comment = 'plain' }\nlet ready = wait seed { until = seed.name == 'ready' }\nattributes { out = ready.comment }\n";
    let cases = vec![
        (
            "root",
            1,
            vec![(
                "main.crn",
                "let seed = mock.test.Resource { name = 'seed' comment = 'plain' }\nlet ready = wait seed { until = seed.name == 'seed' }\n",
            )],
        ),
        (
            "module",
            1,
            vec![
                (
                    "main.crn",
                    "let m = use { source = './leaf' }\nlet instance = m { label = 'module' }\n",
                ),
                ("leaf/main.crn", leaf),
            ],
        ),
        (
            "nested-module",
            1,
            vec![
                (
                    "main.crn",
                    "let m = use { source = './outer' }\nlet instance = m {}\n",
                ),
                (
                    "outer/main.crn",
                    "arguments { unused: String = 'plain' }\nlet m = use { source = '../leaf' }\nlet nested = m { label = 'nested' }\nattributes { out = nested.out }\n",
                ),
                ("leaf/main.crn", leaf),
            ],
        ),
        (
            "for-body-module",
            2,
            vec![
                (
                    "main.crn",
                    "let m = use { source = './leaf' }\nlet instances = for label in ['a', 'b'] { m { label = label } }\n",
                ),
                ("leaf/main.crn", leaf),
            ],
        ),
    ];
    for (case, count, files) in cases {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        for (name, source) in files {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let context = context();
        let schemas = schemas();
        let parsed = crate::config_loader::parse_directory(&path, &context)
            .unwrap_or_else(|_| panic!("{case}: directory parse rejected"));
        let expanded =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::module_resolver::resolve_modules_with_providerless_config(
                    parsed, &path, session, &schemas,
                )
                .unwrap_or_else(|_| panic!("{case}: module expansion rejected"))
            });
        let configured =
            crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context);
        let judged = configured.judged();
        let file = judged.file();
        assert_eq!(file.wait_bindings.len(), count, "{case}: wait count");
        for wait in &file.wait_bindings {
            let mut definition = judged
                .report()
                .secrecy_definition(&wait.binding)
                .unwrap_or_else(|| panic!("{case}: retained Wait definition missing"));
            while matches!(
                definition.secrecy_kind(),
                ExprKind::Alias | ExprKind::Parens | ExprKind::Definition
            ) && definition.secrecy_children().len() == 1
            {
                definition = &definition.secrecy_children()[0];
            }
            assert!(
                matches!(definition.secrecy_kind(), ExprKind::Wait),
                "{case}: root is not Wait"
            );
            let target = definition
                .secrecy_children()
                .first()
                .unwrap_or_else(|| panic!("{case}: Wait has no first operand"));
            let ExprKind::Binding {
                binding,
                authored_binding: _,
            } = target.secrecy_kind()
            else {
                panic!("{case}: Wait target is not a retained binding");
            };
            assert_eq!(
                binding, &wait.target,
                "{case}: retained/metadata address mismatch"
            );
            assert!(
                file.iter_all_resources()
                    .any(|resource| resource.binding_address() == Some(binding)),
                "{case}: target address has no declared resource owner"
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WaitCollisionOwner {
    Upstream,
    Argument,
    ModuleOutput,
    PlainLet,
    Resource,
}

impl WaitCollisionOwner {
    fn label(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Argument => "argument",
            Self::ModuleOutput => "module-output",
            Self::PlainLet => "plain-let",
            Self::Resource => "resource",
        }
    }
}

fn wait_owner_class_files(
    owner: WaitCollisionOwner,
    target_collision: bool,
    owner_after: bool,
    ordinary_reference: bool,
) -> Vec<(String, String)> {
    let name = if target_collision { "seed" } else { "ready" };
    let owner_file = if owner_after {
        "z_owner.crn"
    } else {
        "a_owner.crn"
    };
    let mut files = vec![("providers.crn".into(), "provider mock {}\n".into())];
    let prefix = if owner == WaitCollisionOwner::Argument {
        "module/"
    } else {
        ""
    };
    if owner == WaitCollisionOwner::Argument {
        files.push(("main.crn".into(), format!("let m = use {{ source = './module' }}\nlet instance = m {{ {name} = {{ name = 'seed' comment = 'public' }} }}\n")));
    }
    files.push((
        format!("{prefix}resources.crn"),
        "let seed = mock.test.Resource { name = 'seed' comment = secret('s') }\n".into(),
    ));
    let ready = if ordinary_reference && !target_collision {
        "let ready = seed\n"
    } else {
        "let ready = wait seed { until = seed.comment == 'ready' }\n"
    };
    let read = if ordinary_reference && target_collision {
        "seed"
    } else {
        "ready"
    };
    files.push((format!("{prefix}main.crn"), format!("{ready}let sink = mock.test.Resource {{ name = 'sink' comment = {read}.comment ?? 'public' }}\n")));
    let declaration = match owner {
        WaitCollisionOwner::Upstream => {
            files.push((
                "upstream/main.crn".into(),
                "exports { name: String = 'seed' comment: String = 'public' }\n".into(),
            ));
            format!("let {name} = upstream_state {{ source = './upstream' }}\n")
        }
        WaitCollisionOwner::Argument => format!("arguments {{ {name}: map(String) }}\n"),
        WaitCollisionOwner::ModuleOutput => {
            files.push((
                "owner/main.crn".into(),
                "attributes { name = 'seed' comment = 'public' }\n".into(),
            ));
            format!("let m = use {{ source = './owner' }}\nlet {name} = m {{}}\n")
        }
        WaitCollisionOwner::PlainLet => {
            format!("let {name} = {{ name = 'seed' comment = 'public' }}\n")
        }
        WaitCollisionOwner::Resource => {
            format!("let {name} = mock.test.Resource {{ name = 'owner' comment = 'public' }}\n")
        }
    };
    files.push((format!("{prefix}{owner_file}"), declaration));
    files
}

fn wait_owner_class_observation(
    directory: &std::path::Path,
    files: &[(String, String)],
) -> Result<SecrecyReport, String> {
    for (name, source) in files {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let path = crate::paths::ResolvedPath::new(directory).unwrap();
    let context = context();
    let schemas = schemas();
    let parsed = crate::config_loader::parse_directory(&path, &context)
        .map_err(|error| error.to_string())?;
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed, &path, session, &schemas,
        )
    })
    .map_err(|error| error.to_string())?;
    Ok(
        crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
            .judged()
            .secrecy()
            .clone(),
    )
}

#[test]
fn wait_owner_order_class_matches_ordinary_reference_for_every_owner_and_file_order() {
    let mut mismatches = Vec::new();
    for owner in [
        WaitCollisionOwner::Upstream,
        WaitCollisionOwner::Argument,
        WaitCollisionOwner::ModuleOutput,
        WaitCollisionOwner::PlainLet,
        WaitCollisionOwner::Resource,
    ] {
        for target in [true, false] {
            for after in [false, true] {
                let cell = format!(
                    "{}-{}-{}",
                    owner.label(),
                    if target { "target" } else { "alias" },
                    if after { "after" } else { "before" }
                );
                let directory = tempfile::tempdir().unwrap();
                let wait = wait_owner_class_observation(
                    directory.path(),
                    &wait_owner_class_files(owner, target, after, false),
                );
                let ordinary = wait_owner_class_observation(
                    directory.path(),
                    &wait_owner_class_files(owner, target, after, true),
                );
                match (wait, ordinary) {
                    (Ok(wait), Ok(ordinary)) => {
                        let equal = wait.findings().len() == ordinary.findings().len()
                            && wait
                                .findings()
                                .iter()
                                .zip(ordinary.findings())
                                .all(|(a, b)| a.cause() == b.cause());
                        // In both upstream-target orders, the selected owner is
                        // non-managed: waiting on it is Pending (class iv), while
                        // the ordinary upstream reference is Public and admitted.
                        let state_matches = if owner == WaitCollisionOwner::Upstream && target {
                            wait.is_pending()
                                && wait.admitted().is_none()
                                && !ordinary.is_pending()
                                && ordinary.admitted().is_some()
                        } else {
                            wait.is_pending() == ordinary.is_pending()
                                && wait.admitted().is_some() == ordinary.admitted().is_some()
                        };
                        if !equal || !state_matches {
                            mismatches.push(format!(
                                "{cell}: findings equal={equal}, pending wait/direct={}/{}, admitted wait/direct={}/{}",
                                wait.is_pending(), ordinary.is_pending(),
                                wait.admitted().is_some(), ordinary.admitted().is_some()
                            ));
                        }
                    }
                    (Err(wait), Err(ordinary)) => {
                        let equal =
                            owner == WaitCollisionOwner::Argument && target && wait == ordinary;
                        if !equal {
                            mismatches.push(cell);
                        }
                    }
                    _ => {
                        mismatches.push(cell);
                    }
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "owner order differs for cells: {mismatches:?}"
    );
}

#[test]
fn wait_target_upstream_collision_preserves_main_owner_precedence() {
    let report = wait_alias_directory_report(
        &[
            (
                "resources.crn",
                "provider mock {}\nlet seed = mock.test.Resource { name = 'seed' comment = secret('s') }\n",
            ),
            (
                "a_up.crn",
                "let seed = upstream_state { source = './upstream' }\n",
            ),
            (
                "main.crn",
                "let ready = wait seed { until = seed.name == 'seed' }\nmock.test.Resource { name = 'sink' comment = ready.comment ?? 'public' }\n",
            ),
            (
                "upstream/main.crn",
                "exports { name: String = 'seed' comment: String = 'public' }\n",
            ),
        ],
        false,
    );
    assert_eq!(
        report.findings().len(),
        0,
        "the upstream owner must prevent a resource-secret mixing finding"
    );
}

fn wait_alias_collision_report(resource_comment: &str, seed_comment: &str) -> SecrecyReport {
    wait_alias_directory_report(
        &[
            (
                "resources.crn",
                &format!(
                    "provider mock {{}}\nlet seed = mock.test.Resource {{ name = 'seed' comment = {seed_comment} }}\n"
                ),
            ),
            (
                "a.crn",
                &format!(
                    "let other = mock.test.Resource {{ name = 'other' comment = {resource_comment} }}\n"
                ),
            ),
            (
                "main.crn",
                "let other = wait seed { until = seed.name == 'seed' }\nlet sink = mock.test.Resource { name = 'sink' comment = other.comment ?? 'public' }\n",
            ),
        ],
        false,
    )
}

#[test]
fn wait_alias_collision_keeps_the_declared_secret_resource() {
    let report = wait_alias_collision_report("secret('canary')", "'plain'");
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(report.findings()[0].attribute(), "comment");
    assert!(
        report.findings()[0]
            .location()
            .file()
            .path()
            .ends_with("main.crn")
    );
    assert_eq!(report.findings()[0].location().span().start_line(), 2);
}

#[test]
fn wait_alias_collision_keeps_the_declared_public_resource() {
    let report = wait_alias_collision_report("'plain'", "secret('canary')");
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
}

fn assert_wait_collision_finding(report: &SecrecyReport) {
    assert_eq!(report.findings().len(), 1, "{report:?}");
    let finding = &report.findings()[0];
    assert_eq!(finding.attribute(), "comment");
    assert!(finding.location().file().path().ends_with("main.crn"));
    assert_eq!(finding.location().span().start_line(), 2);
    assert_eq!(finding.location().span().start_column(), 60);
    assert_eq!(
        finding.cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce
        })
    );
}

const WAIT_COLLISION_RESOURCES: &str =
    "provider mock {}\nlet seed = mock.test.Resource { name = \"seed\" comment = \"plain\" }\n";
const WAIT_COLLISION_CONSUMER: &str = "let other = wait seed { until = seed.name == \"seed\" }\nmock.test.Resource { name = \"sink\" comment = other.comment ?? \"public\" }\n";

#[test]
fn wait_alias_collision_keeps_module_output_before_wait_definition() {
    let report = wait_alias_directory_report(
        &[
            (
                "a.crn",
                "let m = use { source = './m' }\nlet other = m {}\n",
            ),
            ("resources.crn", WAIT_COLLISION_RESOURCES),
            ("main.crn", WAIT_COLLISION_CONSUMER),
            ("m/main.crn", "attributes { comment = secret(\"s\") }\n"),
        ],
        true,
    );
    assert_wait_collision_finding(&report);
}

#[test]
fn wait_alias_collision_keeps_last_literal_definition_after_wait() {
    let report = wait_alias_directory_report(
        &[
            ("z.crn", "let other = { comment = secret(\"s\") }\n"),
            ("resources.crn", WAIT_COLLISION_RESOURCES),
            ("main.crn", WAIT_COLLISION_CONSUMER),
        ],
        false,
    );
    assert_wait_collision_finding(&report);
}

#[test]
fn wait_alias_collision_keeps_last_wait_definition_after_literal() {
    let report = wait_alias_directory_report(
        &[
            ("a.crn", "let other = { comment = secret(\"s\") }\n"),
            ("resources.crn", WAIT_COLLISION_RESOURCES),
            ("main.crn", WAIT_COLLISION_CONSUMER),
        ],
        false,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(report.rows.len(), 2);
    assert!(
        report
            .rows
            .values()
            .all(|row| row.attributes.get("comment") == Some(&Evidence::Leaf(Secrecy::Public))),
        "{report:?}"
    );
}

#[test]
fn wait_alias_unreferenced_valid_until_keeps_structural_evidence() {
    let report = report(
        "let seed = mock.test.Resource { name = 'seed' }\n\
         let ready = wait seed { until = seed.name == 'seed' }\n",
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    assert!(report.admitted().is_some(), "{report:?}");
    assert_eq!(
        attribute(&report, "seed", "name"),
        Evidence::Leaf(Secrecy::Public)
    );
}

fn wait_alias_duplicate_wait_report(first: &str, last: &str) -> SecrecyReport {
    wait_alias_directory_report(
        &[
            (
                "resources.crn",
                "provider mock {}\nlet s = mock.test.Resource { name = 's' comment = secret('s') }\nlet p = mock.test.Resource { name = 'p' comment = 'plain' }\n",
            ),
            (
                "a.crn",
                &format!("let other = wait {first} {{ until = {first}.name == '{first}' }}\n"),
            ),
            (
                "z.crn",
                &format!("let other = wait {last} {{ until = {last}.name == '{last}' }}\n"),
            ),
            (
                "main.crn",
                "let sink = mock.test.Resource { name = 'sink' comment = other.comment ?? 'public' }\n",
            ),
        ],
        false,
    )
}

#[test]
fn wait_alias_duplicate_waits_use_last_public_target() {
    let report = wait_alias_duplicate_wait_report("s", "p");
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(
        attribute(&report, "sink", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
}

#[test]
fn wait_alias_duplicate_waits_use_last_secret_target() {
    let report = wait_alias_duplicate_wait_report("p", "s");
    assert_eq!(report.findings().len(), 1, "{report:?}");
    let finding = &report.findings()[0];
    assert!(finding.location().file().path().ends_with("main.crn"));
    assert_eq!(finding.location().span().start_line(), 1);
    assert_eq!(finding.location().span().start_column(), 71);
    assert_eq!(
        finding.cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce
        })
    );
}

#[test]
fn wait_alias_module_instances_keep_argument_evidence_and_finding_locations() {
    let consumers = "let t1 = mock.test.Resource { name = 't1' comment = r1.out ?? 'public' }\n\
                     let t2 = mock.test.Resource { name = 't2' comment = r2.out ?? 'public' }\n";
    let report = wait_alias_directory_report(
        &[
            ("providers.crn", "provider mock {}\n"),
            (
                "main.crn",
                "let m = use { source = './module' }\nlet r1 = m { label = secret('s') }\nlet r2 = m { label = 'plain' }\n",
            ),
            ("consumers.crn", consumers),
            ("module/arguments.crn", "arguments { label: String }\n"),
            (
                "module/resources.crn",
                "let seed = mock.test.Resource { name = 'seed' comment = label }\n",
            ),
            (
                "module/main.crn",
                "let ready = wait seed { until = seed.name == 'seed' }\nattributes { out = ready.comment }\n",
            ),
        ],
        true,
    );
    assert_eq!(
        attribute(&report, "r1.seed", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    assert_eq!(
        attribute(&report, "r2.seed", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    assert_eq!(
        attribute(&report, "t2", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    let finding = &report.findings()[0];
    assert_eq!(finding.attribute(), "comment");
    assert!(finding.location().file().path().ends_with("consumers.crn"));
    assert_eq!(finding.location().span().start_line(), 1);
    assert_eq!(
        finding.location().span().start_column(),
        consumers.find("??").unwrap() + 1
    );
}

#[test]
fn wait_alias_literal_target_is_pending() {
    assert_wait_alias_pending(&report(
        "let value = { name = 'plain' out = secret('s') }\n\
         let ready = wait value { until = value.name == 'plain' }\n\
         let sink = mock.test.Resource { comment = ready.out ?? 'public' }\n",
    ));
}

#[test]
fn wait_alias_module_call_target_is_pending() {
    let report = wait_alias_directory_report(
        &[
            ("providers.crn", "provider mock {}\n"),
            (
                "main.crn",
                "let m = use { source = './module' }\nlet value = m {}\nlet ready = wait value { until = value.out == 'plain' }\nlet sink = mock.test.Resource { comment = ready.out ?? 'public' }\n",
            ),
            (
                "module/main.crn",
                "arguments { unused: String = 'unused' }\nattributes { out = secret('s') }\n",
            ),
        ],
        true,
    );
    assert_wait_alias_pending(&report);
}

#[test]
fn wait_alias_upstream_target_is_pending() {
    assert_wait_alias_pending(&report(
        "let value = upstream_state { source = './upstream' }\n\
         let ready = wait value { until = value.x == 'plain' }\n\
         let sink = mock.test.Resource { comment = ready.x ?? secret('s') }\n",
    ));
}

// The stack covers parse-time knowledge classification of nested user-function calls (#4481), not the secrecy walk.
fn with_parser_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(test)
        .expect("failed to spawn deep-expression test thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

const SECRET_TOKEN_SOURCE: &str = "let token = secret('canary')\n";
const DIRECTIVES_SOURCE: &str = r#"
let protected = mock.test.Resource {
  name = "protected"
  comment = token
  directives { prevent_destroy = true }
}
"#;
const PLAIN_RESOURCE_SOURCE: &str =
    "let plain = mock.test.Resource { name = 'plain' comment = 'visible' }\n";

fn assert_directives_secrecy(report: &SecrecyReport) {
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    let graph = report
        .admitted()
        .expect("complete resource secrecy evidence");
    assert_eq!(report.rows.len(), 2);
    assert_eq!(
        attribute(report, "protected", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    assert_eq!(
        attribute(report, "plain", "comment"),
        Evidence::Leaf(Secrecy::Public)
    );
    for (id, row) in report.rows.iter() {
        assert_eq!(
            row.attributes
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["name", "comment"]
        );
        assert_eq!(row.attributes["name"], Evidence::Leaf(Secrecy::Public));
        assert_eq!(graph.attributes(id), Some(&*row.attributes));
    }
}

#[test]
fn resource_directives_do_not_make_file_secrecy_pending() {
    assert_directives_secrecy(&report(&format!(
        "{SECRET_TOKEN_SOURCE}{DIRECTIVES_SOURCE}{PLAIN_RESOURCE_SOURCE}"
    )));
}

#[test]
fn resource_directives_do_not_make_directory_secrecy_pending() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.crn"), DIRECTIVES_SOURCE).unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        format!("provider mock {{}}\n{SECRET_TOKEN_SOURCE}{PLAIN_RESOURCE_SOURCE}"),
    )
    .unwrap();
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &context())
        .unwrap()
        .parsed()
        .clone();
    assert_directives_secrecy(&secrecy_report(&parsed, &schemas()));
}

#[test]
fn resource_directives_named_attribute_keeps_its_secrecy_slot() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        Schema::new("test.Custom")
            .attribute(Attribute::optional("directives", Type::string()).build())
            .validate()
            .unwrap(),
    );
    let file = parsed("let custom = mock.test.Custom { directives = secret('canary') }");
    let report = secrecy_report(&file, &schemas);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    assert!(report.admitted().is_some());
    assert_eq!(
        attribute(&report, "custom", "directives"),
        Evidence::Leaf(Secrecy::Secret)
    );
}

#[test]
fn resource_attribute_count_mismatch_keeps_secrecy_pending() {
    let mut file = parsed("let plain = mock.test.Resource { name = 'plain' }");
    assert!(secrecy_report(&file, &schemas()).admitted().is_some());
    file.resources[0].attributes.insert(
        "comment".into(),
        Value::Concrete(ConcreteValue::String("no-authored-slot".into())),
    );
    let report = secrecy_report(&file, &schemas());
    assert!(report.findings().is_empty(), "{report:?}");
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows.values().next().unwrap().attributes.len(), 1);
    assert_eq!(file.resources[0].attributes.len(), 2);
    assert!(report.is_pending(), "{report:?}");
    assert!(report.admitted().is_none());
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
        Evidence::Map(
            BTreeMap::from([
                ("private".into(), Evidence::Leaf(Secrecy::Secret)),
                ("public".into(), Evidence::Leaf(Secrecy::Public)),
            ])
            .into()
        )
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

// Kept at 3 levels: the parser itself is super-exponential on this shape (#4495); raise once that is fixed.
// f0 is the leaf: f3 has 4 bodies, or 2^4 - 1 = 15 body evaluations without memoization.
fn function_fan_out_source(last: usize, conditional: bool) -> String {
    use std::fmt::Write as _;
    let mut source = "fn f0(x: String): String { x }\n".to_string();
    for index in 1..=last {
        let previous = index - 1;
        let body = if conditional {
            format!("if true {{ f{previous}(x) }} else {{ f{previous}(x) }}")
        } else {
            format!("f{previous}(x) ?? f{previous}(x)")
        };
        writeln!(source, "fn f{index}(x: String): String {{ {body} }}").unwrap();
    }
    source
}

#[test]
fn function_coalesce_fan_out_evaluates_each_body_once() {
    assert_function_fan_out_evaluates_each_body_once(false);
}

#[test]
fn function_if_fan_out_evaluates_each_body_once() {
    assert_function_fan_out_evaluates_each_body_once(true);
}

#[test]
fn function_declaration_walks_share_unbound_argument_memos() {
    for conditional in [false, true] {
        let file = parsed(&function_fan_out_source(3, conditional));
        let schemas = schemas();
        let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
        let owner = Owner {
            syntax: &syntax,
            schemas: &schemas,
            resources: IndexMap::new(),
            declarations: Vec::new(),
            declaration_index: OnceLock::new(),
            reference_free: RefCell::default(),
            literal_evidence: RefCell::default(),
            function_evidence: RefCell::default(),
            function_observations: RefCell::default(),
            argument_environments: RefCell::default(),
            upstream: HashSet::new(),
            empty_environment: Arc::default(),
        };
        let mut evaluations = 0;
        for function in syntax.secrecy_functions() {
            let mut walk = Walk::new(&owner, "function".into(), function.location().clone())
                .in_source(&function);
            assert_eq!(
                walk.run(Thunk {
                    source: Source::Syntax(function),
                    environment: owner.empty_environment.clone()
                }),
                None
            );
            assert!(walk.findings.is_empty());
            assert_eq!(walk.visits, 0);
            evaluations += walk.function_body_evaluations;
        }
        // Memoized: 3 unbound callee bodies (f0..f2); without memos: 2 + 6 + 14 = 22.
        assert_eq!(evaluations, 3);
    }
}

fn assert_function_fan_out_evaluates_each_body_once(conditional: bool) {
    let mut source = function_fan_out_source(3, conditional);
    for index in 0..4 {
        source.push_str(&format!(
            "let sink{index} = mock.test.Resource {{ comment = f3('a') }}\n"
        ));
    }
    let report = report(&source);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.admitted().is_some(), "{report:?}");
    for index in 0..4 {
        assert_eq!(
            attribute(&report, &format!("sink{index}"), "comment"),
            Evidence::Leaf(Secrecy::Public)
        );
    }
    // Memoized: 4 bodies for one Public argument evidence; without memos: at least 4 * 15 = 60.
    assert_eq!(
        report
            .traversals()
            .iter()
            .map(|stats| stats.function_body_evaluations)
            .sum::<usize>(),
        4,
        "consumers must share reference-free results: {:?}",
        report.traversals()
    );
    assert!(
        report
            .traversals()
            .iter()
            .all(|stats| stats.reference_visits == 0 && stats.limit.is_none())
    );
}

#[test]
fn function_memo_replays_warm_reference_dags_at_4096_and_4097_visits() {
    for calls in [4095, 4096] {
        let arguments = std::iter::repeat_n("g('x')", calls)
            .collect::<Vec<_>>()
            .join(", ");
        let report = report(&format!(
            "let leaf = mock.test.Resource {{ comment = 'public' }}\nlet seed = mock.test.Resource {{ comment = leaf.comment }}\nfn g(x: String): String {{ seed.comment }}\nlet sink = mock.test.Resource {{ comment = join('', [{arguments}]) }}\n"
        ));
        let stats = report
            .traversals()
            .iter()
            .find(|stats| stats.reference_visits == calls + 1)
            .unwrap();
        assert_eq!(stats.maximum_depth, 2);
        assert_eq!(stats.function_body_evaluations, 1);
        assert_eq!(stats.function_memo_hits, calls - 1);
        if calls == 4095 {
            assert!(report.admitted().is_some(), "{report:?}");
            assert!(report.findings().is_empty());
            assert_eq!(stats.limit, None);
        } else {
            assert_eq!(report.findings().len(), 1, "{report:?}");
            assert_eq!(stats.limit, Some(LimitKind::ReferenceVisits));
            assert_eq!(
                report.findings()[0].message(),
                "Attribute 'comment': deciding whether the value is a secret needs more than 4096 reference lookups from this attribute."
            );
        }
    }
}

#[test]
fn function_memo_replays_fresh_local_reference_identities_at_the_visit_boundary() {
    for calls in [2048, 2049] {
        let arguments = std::iter::repeat_n("g('x')", calls)
            .collect::<Vec<_>>()
            .join(", ");
        let report = report(&format!(
            "let leaf = mock.test.Resource {{ comment = 'public' }}\nlet captured = {{ value = leaf.comment }}\nfn g(x: String): String {{ captured.value }}\nlet sink = mock.test.Resource {{ comment = join('', [{arguments}]) }}\n"
        ));
        let visits = (calls * 2).min(4097);
        let stats = report
            .traversals()
            .iter()
            .find(|stats| stats.reference_visits == visits)
            .unwrap();
        assert_eq!(stats.maximum_depth, 2);
        assert_eq!(stats.function_body_evaluations, 1);
        assert_eq!(stats.function_memo_hits, calls - 1);
        assert_eq!(
            stats.limit,
            (calls == 2049).then_some(LimitKind::ReferenceVisits)
        );
        assert_eq!(report.findings().len(), usize::from(calls == 2049));
        assert_eq!(report.admitted().is_some(), calls == 2048);
    }
}

#[test]
fn nested_function_memo_replays_each_invocations_local_reference_identity() {
    let calls = std::iter::repeat_n("f('x')", 100)
        .collect::<Vec<_>>()
        .join(", ");
    let report = report(&format!(
        "let leaf = mock.test.Resource {{ comment = 'public' }}\nlet captured = {{ value = leaf.comment }}\nfn g(x: String): String {{ captured.value }}\nfn f(x: String): String {{ g(x) ?? g(x) }}\nlet sink = mock.test.Resource {{ comment = join('', [{calls}]) }}\n"
    ));
    assert!(report.admitted().is_some(), "{report:?}");
    let stats = report
        .traversals()
        .iter()
        .find(|stats| stats.reference_visits == 400)
        .unwrap();
    assert_eq!(stats.maximum_depth, 2);
    assert_eq!(stats.function_body_evaluations, 2);
    assert_eq!(stats.limit, None);
}

#[test]
fn function_memo_replays_nested_reference_calls_with_outer_environment_renaming() {
    let calls = std::iter::repeat_n("f('x')", 100)
        .collect::<Vec<_>>()
        .join(", ");
    let report = report(&format!(
        "let leaf = mock.test.Resource {{ comment = 'public' }}\nlet captured = {{ value = leaf.comment }}\nfn g(x: String): String {{ captured.value }}\nlet entry = {{ value = g('x') }}\nfn f(x: String): String {{ entry.value ?? entry.value }}\nlet sink = mock.test.Resource {{ comment = join('', [{calls}]) }}\n"
    ));
    assert!(report.admitted().is_some(), "{report:?}");
    let stats = report
        .traversals()
        .iter()
        .find(|stats| stats.reference_visits == 400)
        .unwrap();
    assert_eq!(stats.maximum_depth, 3);
    assert_eq!(stats.function_body_evaluations, 2);
    assert_eq!(stats.limit, None);
}

#[test]
fn function_memo_replays_resource_argument_references_outside_the_callee_activation() {
    let report = report(
        r#"
fn g(r: mock.test.Resource): String { r.comment }
let leaf = mock.test.Resource { comment = "public" }
let pending = mock.test.Resource {}
let middle = mock.test.Resource { comment = join("", [g(leaf), pending.missing]) }
let sink = mock.test.Resource { comment = join("", [g(middle), g(middle)]) }
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.is_pending());
    let stats = report
        .traversals()
        .iter()
        .find(|stats| stats.reference_visits == 10)
        .unwrap();
    assert_eq!(stats.maximum_depth, 2);
    assert!(stats.function_memo_hits >= 2);
    assert_eq!(stats.limit, None);
}

#[test]
fn value_function_fan_out_shares_reference_free_results_by_argument_evidence() {
    let functions = parsed(&function_fan_out_source(3, false)).user_functions;
    let functions = Arc::new(functions);
    // Omit retained function syntax to exercise begin_value_function itself,
    // rather than the usual lexical-syntax route for module argument values.
    let file = parsed("mock.test.Resource { comment = 'public' }");
    let schemas = schemas();
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
    let owner = Owner {
        syntax: &syntax,
        schemas: &schemas,
        resources: IndexMap::new(),
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        upstream: HashSet::new(),
        empty_environment: Arc::default(),
    };
    let location = functions["f3"].body.location.clone();
    let mut evaluations = 0;
    for argument in ["a", "b"] {
        let mut walk = Walk::new(&owner, "comment".into(), location.clone());
        let value = Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: Arc::new(functions["f3"].clone()),
            functions: functions.clone(),
            args: vec![crate::resource::FunctionArgument::from_value(
                Value::Concrete(ConcreteValue::String(argument.into())),
            )],
            source_line: 1.into(),
        });
        assert_eq!(
            walk.run(Thunk {
                source: Source::Value(Arc::new(value)),
                environment: owner.empty_environment.clone()
            }),
            Some(Evidence::Leaf(Secrecy::Public))
        );
        assert!(walk.findings.is_empty());
        assert_eq!(walk.visits, 0);
        evaluations += walk.function_body_evaluations;
    }
    // Memoized: 4 bodies for shared Public argument evidence; without memos: 2 * 15 = 30.
    assert_eq!(evaluations, 4);
}

#[test]
fn function_memo_replays_reference_depth_at_127_and_128_in_both_orders() {
    use std::fmt::Write as _;
    for last in [126, 127] {
        for shallow_first in [false, true] {
            let mut source = "let leaf = mock.test.Resource { comment = 'public' }\nfn g(x: String): String { leaf.comment }\nlet r1 = mock.test.Resource { comment = g('x') }\n".to_string();
            for index in 2..=last {
                writeln!(
                    source,
                    "let r{index} = mock.test.Resource {{ comment = r{}.comment }}",
                    index - 1
                )
                .unwrap();
            }
            let operands = if shallow_first {
                format!("g('x'), r{last}.comment")
            } else {
                format!("r{last}.comment, g('x')")
            };
            writeln!(
                source,
                "let sink = mock.test.Resource {{ comment = join('', [{operands}]) }}"
            )
            .unwrap();
            let report = report(&source);
            let stats = report
                .traversals()
                .iter()
                .find(|stats| stats.maximum_depth == last + 1)
                .unwrap();
            assert_eq!(
                stats.reference_visits,
                last + 1 + usize::from(shallow_first || last == 126)
            );
            assert_eq!(stats.limit, (last == 127).then_some(LimitKind::Depth));
            assert_eq!(report.findings().len(), usize::from(last == 127));
            assert_eq!(report.admitted().is_some(), last == 126);
            if last == 127 {
                assert_eq!(
                    report.findings()[0].message(),
                    "Attribute 'comment': the value follows a chain of 128 or more references from this attribute; Carina cannot decide whether it is a secret."
                );
            }
        }
    }
}

#[test]
fn function_memo_distinguishes_secret_public_and_selected_path_arguments() {
    // Function-parameter conditions are runtime values and cannot be parsed.
    let report = report(
        r#"
fn g(x: String): String { x }
fn project(key: String, values: list(map(String))): String { map(key, values)[0] }
let sink = mock.test.Resource { tags = {
  private = g(secret("a")) public = g("b")
  selected_private = project(".private", [{ private = secret("x") public = "y" }])
  selected_public = project(".public", [{ private = secret("x") public = "y" }])
} }
"#,
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.admitted().is_some(), "{report:?}");
    let Evidence::Map(fields) = attribute(&report, "sink", "tags") else {
        panic!("map evidence")
    };
    for (key, secrecy) in [
        ("private", Secrecy::Secret),
        ("public", Secrecy::Public),
        ("selected_private", Secrecy::Secret),
        ("selected_public", Secrecy::Public),
    ] {
        assert_eq!(fields[key], Evidence::Leaf(secrecy), "{key}");
    }
}

#[test]
fn function_memo_keeps_pending_resource_arguments_and_requested_paths_distinct() {
    let report = report(
        r#"
let private = mock.test.Resource { tags = { token = secret("a") public = "b" } }
let public = mock.test.Resource { tags = { token = "c" public = "d" } }
fn g(r: mock.test.Resource): map(String) { r.tags }
let sink = mock.test.Resource { tags = {
  private = g(private).token public = g(public).token sibling = g(private).public
} }
"#,
    );
    assert!(report.admitted().is_some(), "{report:?}");
    assert!(report.findings().is_empty());
    let Evidence::Map(fields) = attribute(&report, "sink", "tags") else {
        panic!("map evidence")
    };
    assert_eq!(fields["private"], Evidence::Leaf(Secrecy::Secret));
    assert_eq!(fields["public"], Evidence::Leaf(Secrecy::Public));
    assert_eq!(fields["sibling"], Evidence::Leaf(Secrecy::Public));
}

#[test]
fn function_memo_keys_hash_and_compare_deep_argument_evidence_on_the_heap() {
    fn facts(secrecy: Secrecy) -> ArgumentFacts {
        let mut proof = Evidence::Leaf(secrecy);
        for index in 0..10_000 {
            proof = if index % 2 == 0 {
                Evidence::List(Arc::new(vec![proof]))
            } else {
                Evidence::Map(Arc::new(BTreeMap::from([("field".into(), proof)])))
            };
        }
        ArgumentFacts {
            proof: Some(proof),
            observations: ArgumentObservations::default(),
            origin: None,
        }
    }
    let mut first = facts(Secrecy::Secret);
    let mut same = facts(Secrecy::Secret);
    let mut public = facts(Secrecy::Public);
    let hash = |facts: &ArgumentFacts| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        facts.hash(&mut hash);
        hash.finish()
    };
    assert!(first == same);
    assert!(first != public);
    assert_eq!(hash(&first), hash(&same));
    // Release these intentionally deep fixtures on the heap as well.
    let mut remaining = vec![
        first.proof.take().unwrap(),
        same.proof.take().unwrap(),
        public.proof.take().unwrap(),
    ];
    while let Some(proof) = remaining.pop() {
        match proof {
            Evidence::Leaf(_) => {}
            Evidence::List(items) => remaining.extend(Arc::try_unwrap(items).unwrap()),
            Evidence::Map(fields) => {
                remaining.extend(Arc::try_unwrap(fields).unwrap().into_values())
            }
        }
    }
}

#[test]
fn function_memo_keeps_mixed_body_findings_once_with_the_function_owner() {
    let calls = std::iter::repeat_n("g('a')", 100)
        .collect::<Vec<_>>()
        .join(", ");
    let report = report(&format!(
        "fn g(x: String): String {{ x ?? secret('canary') }}\nlet sink = mock.test.Resource {{ comment = join('', [{calls}]) }}\n"
    ));
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].message(),
        "Function 'g': ?? alternatives mix secret and public values; all alternatives must be secret or all public."
    );
    assert_eq!(report.findings()[0].location().span().start_line(), 1);
    assert!(report.findings()[0].call_site().is_none());
    assert!(
        report
            .traversals()
            .iter()
            .map(|stats| stats.function_body_evaluations)
            .sum::<usize>()
            <= 2
    );
}

#[test]
fn module_argument_function_fan_out_uses_the_same_memo() {
    let mut body = function_fan_out_source(3, false);
    body.push_str("arguments { value: String = f3('a') }\nlet sink = mock.test.Resource { comment = value }\n");
    let (_directory, file) = expanded_test_module(&body, "");
    let report = crate::config_loader::load_buffer_configuration(&file, &schemas(), &context())
        .judged()
        .secrecy()
        .clone();
    assert!(report.admitted().is_some(), "{report:?}");
    assert!(report.findings().is_empty());
    // Memoized: 4 bodies for one Public argument evidence; without memos: at least 15.
    assert_eq!(
        report
            .traversals()
            .iter()
            .map(|stats| stats.function_body_evaluations)
            .sum::<usize>(),
        4
    );
    assert!(
        report
            .traversals()
            .iter()
            .all(|stats| stats.reference_visits <= 1 && stats.limit.is_none())
    );
}

#[test]
fn loop_slot_function_fan_out_reuses_only_matching_argument_evidence() {
    let mut source = function_fan_out_source(3, true);
    source.push_str("for item in [{ name = 'one' private = true }, { name = 'two' private = false }] {\nmock.test.Resource { name = item.name comment = f3(if item.private { secret('a') } else { 'b' }) }\n}\n");
    let report = report(&source);
    assert!(report.admitted().is_some(), "{report:?}");
    assert!(report.findings().is_empty());
    let mut rows = report.rows.values();
    assert_eq!(
        rows.next().unwrap().attributes["comment"],
        Evidence::Leaf(Secrecy::Secret)
    );
    assert_eq!(
        rows.next().unwrap().attributes["comment"],
        Evidence::Leaf(Secrecy::Public)
    );
    assert!(rows.next().is_none());
    // Memoized: 8 bodies (4 per Secret/Public argument evidence); without memos: at least 2 * 15 = 30.
    assert_eq!(
        report
            .traversals()
            .iter()
            .map(|stats| stats.function_body_evaluations)
            .sum::<usize>(),
        8
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
fn direct_and_referenced_attribute_walks_keep_independent_memos() {
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
        stats.memo_hits, 0,
        "earlier attribute walks must not prepopulate the sink memo"
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
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        upstream: HashSet::new(),
        empty_environment: Arc::default(),
    };
    let resource = &file.resources[0];
    let slot = resource.authored_slots.get("comment").unwrap();
    let source = syntax
        .secrecy_root(&resource.id, slot.secrecy_node())
        .unwrap();
    let mut walk =
        Walk::new(&owner, "comment".into(), source.location().clone()).in_source(&source);
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
    assert!(walk.memo.is_empty());
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
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        upstream: HashSet::new(),
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
    resource_chain_with_expression(last, shape, |previous| format!("r{previous}.comment"))
}

fn resource_chain_with_expression(
    last: usize,
    shape: ChainShape,
    expression: impl Fn(usize) -> String,
) -> SecrecyReport {
    use std::fmt::Write as _;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let mut resources =
        "let r0 = mock.test.Resource { name = 'r0' comment = secret('canary') }\n".to_string();
    for index in 1..last {
        writeln!(
            resources,
            "let r{index} = mock.test.Resource {{ name = 'r{index}' comment = {} }}",
            expression(index - 1)
        )
        .unwrap();
    }
    let consumer = format!(
        "let r{last} = mock.test.Resource {{ name = 'r{last}' comment = {} }}\n",
        expression(last - 1)
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
            "Attribute 'comment': the value follows a chain of 128 or more references from this attribute; Carina cannot decide whether it is a secret."
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
fn issue_4474_deep_chains_parse_and_resolve_without_stack_growth_in_both_shapes() {
    // Parser value resolution and dependency walks traverse all 1000 hops; the secrecy walk stops at 128.
    // 1000 is well beyond main's ~100-reference stack overflow; longer chains only
    // add per-resource validation time.
    for shape in CHAIN_SHAPES {
        for last in [256, 1000] {
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
fn expression_steps_do_not_add_reference_depth_in_both_shapes() {
    for shape in CHAIN_SHAPES {
        for wrap_in_list in [false, true] {
            for last in [127, 128] {
                let report = resource_chain_with_expression(last, shape, |previous| {
                    if wrap_in_list {
                        format!("join('', [r{previous}.comment])")
                    } else {
                        format!("upper(r{previous}.comment)")
                    }
                });
                assert_eq!(
                    report.findings().len(),
                    usize::from(last == 128),
                    "{shape:?}, list={wrap_in_list}, {last}: {report:?}"
                );
                if last == 128 {
                    assert_eq!(
                        report.findings()[0].cause(),
                        &DerivationError::LimitReached(LimitReached {
                            kind: LimitKind::Depth
                        })
                    );
                    assert!(report.admitted().is_none());
                } else {
                    assert!(!report.is_pending(), "{report:?}");
                    assert!(report.admitted().is_some());
                    assert_eq!(
                        attribute(&report, "r127", "comment"),
                        Evidence::Leaf(Secrecy::Secret)
                    );
                }
                assert!(report.traversals().iter().any(|stats| {
                    stats.attribute == "comment"
                        && stats.maximum_depth == last
                        && stats.limit == (last == 128).then_some(LimitKind::Depth)
                }));
            }
        }
    }
}

#[test]
fn one_reference_in_deep_expressions_does_not_reach_the_depth_limit_in_both_shapes() {
    // The large stack is only for the parser's syntax recursion tracked in #4481.
    // The secrecy owner uses heap worklists, which the 256/1000-reference chain
    // tests pin on the default stack.
    with_parser_stack(|| {
        for shape in CHAIN_SHAPES {
            for (wrap_in_list, levels) in [(false, 130), (true, 70)] {
                let report = resource_chain_with_expression(1, shape, |previous| {
                    (0..levels).fold(format!("r{previous}.comment"), |expression, _| {
                        if wrap_in_list {
                            format!("join('', [{expression}])")
                        } else {
                            format!("upper({expression})")
                        }
                    })
                });
                assert!(
                    report.findings().is_empty(),
                    "{shape:?}, {levels}: {report:?}"
                );
                assert!(!report.is_pending(), "{report:?}");
                assert!(report.admitted().is_some());
                assert_eq!(
                    attribute(&report, "r1", "comment"),
                    Evidence::Leaf(Secrecy::Secret)
                );
                assert!(report.traversals().iter().any(|stats| {
                    stats.attribute == "comment"
                        && stats.maximum_depth == 1
                        && stats.reference_visits == 1
                        && stats.limit.is_none()
                }));
            }
        }
    });
}

fn module_output_chain(instances: usize, shape: ChainShape) -> SecrecyReport {
    use std::fmt::Write as _;
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let arguments = "arguments { value: String }\n";
    let output = "attributes { output: String = value }\n";
    match shape {
        ChainShape::SingleFile => {
            std::fs::write(module.join("main.crn"), arguments.to_string() + output).unwrap();
        }
        ChainShape::SiblingFiles => {
            std::fs::write(module.join("arguments.crn"), arguments).unwrap();
            std::fs::write(module.join("main.crn"), output).unwrap();
        }
    }
    let mut modules = "let template = use { source = './module' }\n".to_string();
    for index in 1..=instances {
        let value = if index == 1 {
            "secret('canary')".to_string()
        } else {
            format!("m{}.output", index - 1)
        };
        writeln!(modules, "let m{index} = template {{ value = {value} }}").unwrap();
    }
    let consumer = format!(
        "let sink = mock.test.Resource {{ name = 'sink' comment = m{instances}.output }}\n"
    );
    match shape {
        ChainShape::SingleFile => {
            std::fs::write(
                directory.path().join("main.crn"),
                "provider mock {}\n".to_string() + &modules + &consumer,
            )
            .unwrap();
        }
        ChainShape::SiblingFiles => {
            std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
            std::fs::write(directory.path().join("resources.crn"), modules).unwrap();
            std::fs::write(directory.path().join("main.crn"), consumer).unwrap();
        }
    }
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let root = crate::config_loader::parse_directory(&base, &context).unwrap();
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            root, &base, session, &schemas,
        )
        .unwrap()
    });
    crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
        .judged()
        .secrecy()
        .clone()
}

#[test]
fn module_output_chains_accept_63_and_reject_64_in_both_shapes() {
    for shape in CHAIN_SHAPES {
        for instances in [63, 64] {
            let report = module_output_chain(instances, shape);
            assert_eq!(
                report.findings().len(),
                usize::from(instances == 64),
                "{shape:?}, {instances}: {report:?}"
            );
            if instances == 64 {
                assert_eq!(
                    report.findings()[0].cause(),
                    &DerivationError::LimitReached(LimitReached {
                        kind: LimitKind::Depth
                    })
                );
                assert_eq!(
                    report.findings()[0].message(),
                    LimitKind::Depth.message("Attribute", "comment")
                );
                assert!(report.admitted().is_none());
            } else {
                assert!(!report.is_pending(), "{report:?}");
                assert!(report.admitted().is_some());
                assert_eq!(
                    attribute(&report, "sink", "comment"),
                    Evidence::Leaf(Secrecy::Secret)
                );
            }
            assert!(report.traversals().iter().any(|stats| {
                stats.attribute == "comment"
                    && stats.maximum_depth == instances * 2
                    && stats.limit == (instances == 64).then_some(LimitKind::Depth)
            }));
        }
    }
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
        Evidence::Map(
            BTreeMap::from([
                ("protected".into(), Evidence::Leaf(Secrecy::Secret)),
                ("public".into(), Evidence::Leaf(Secrecy::Public)),
            ])
            .into()
        )
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
fn reference_visits_reject_4202_reference_sink_in_both_orders_and_file_layouts() {
    use std::fmt::Write as _;
    for shape in CHAIN_SHAPES {
        for sink_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut leaves = String::new();
            let mut hubs = String::new();
            for prefix in ["a", "b"] {
                for index in 1..=2100 {
                    writeln!(
                        leaves,
                        "let {prefix}{index} = mock.test.Resource {{ comment = 'x' }}"
                    )
                    .unwrap();
                }
                let references = (1..=2100)
                    .map(|index| format!("{prefix}{index}.comment"))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    hubs,
                    "let hub_{prefix} = mock.test.Resource {{ comment = join('', [{references}]) }}"
                )
                .unwrap();
            }
            let sink = "let sink = mock.test.Resource { comment = join('', [hub_a.comment, hub_b.comment]) }\n";
            let path = directory.path().join("main.crn");
            let file = match shape {
                ChainShape::SingleFile => {
                    let body = if sink_first {
                        sink.to_string() + &hubs
                    } else {
                        hubs.clone() + sink
                    };
                    crate::parser::parse_file(&(leaves.clone() + &body), &path, &context()).unwrap()
                }
                ChainShape::SiblingFiles => {
                    std::fs::write(&path, &leaves).unwrap();
                    let (sink_file, hubs_file) = if sink_first {
                        ("00-sink.crn", "01-hubs.crn")
                    } else {
                        ("01-sink.crn", "00-hubs.crn")
                    };
                    std::fs::write(directory.path().join(sink_file), sink).unwrap();
                    std::fs::write(directory.path().join(hubs_file), &hubs).unwrap();
                    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
                    crate::config_loader::parse_directory(&base, &context())
                        .unwrap()
                        .parsed()
                        .clone()
                }
            };
            let report = secrecy_report(&file, &schemas());
            assert_eq!(
                report.findings().len(),
                1,
                "{shape:?}, sink_first={sink_first}: {report:?}"
            );
            assert_eq!(
                report.findings()[0].message(),
                "Attribute 'comment': deciding whether the value is a secret needs more than 4096 reference lookups from this attribute."
            );
            let finding = &report.findings()[0];
            let (sink_file, sink_line) = match shape {
                ChainShape::SingleFile => (
                    path,
                    leaves.lines().count()
                        + if sink_first {
                            1
                        } else {
                            hubs.lines().count() + 1
                        },
                ),
                ChainShape::SiblingFiles => (
                    directory.path().join(if sink_first {
                        "00-sink.crn"
                    } else {
                        "01-sink.crn"
                    }),
                    1,
                ),
            };
            assert_eq!(finding.location().file().path(), sink_file);
            assert_eq!(finding.location().span().start_line(), sink_line);
            assert_eq!(
                finding.location().span().start_column(),
                sink.find("comment").unwrap() + 1
            );
            for hub in ["hub_a", "hub_b"] {
                assert_eq!(
                    attribute(&report, hub, "comment"),
                    Evidence::Leaf(Secrecy::Public)
                );
            }
            assert!(
                report
                    .traversals()
                    .iter()
                    .filter(|stats| stats.reference_visits == 2100 && stats.limit.is_none())
                    .count()
                    >= 2
            );
            // The sink needs 2 + 2100 + 2100 = 4202 encounters. Stop at the
            // first over-budget encounter, irrespective of earlier walks.
            assert!(
                report
                    .traversals()
                    .iter()
                    .any(|stats| stats.reference_visits == 4097
                        && stats.limit == Some(LimitKind::ReferenceVisits))
            );
        }
    }
}

#[test]
fn thirteen_level_fan_out_dag_counts_27_encounters_per_consumer() {
    use std::fmt::Write as _;
    let mut source = "let r0 = mock.test.Resource { comment = 'x' }\n".to_string();
    for index in 1..=13 {
        writeln!(source, "let r{index} = mock.test.Resource {{ comment = join('', [r{previous}.comment, r{previous}.comment]) }}", previous = index - 1).unwrap();
    }
    source.push_str("let first = mock.test.Resource { comment = r13.comment }\nlet second = mock.test.Resource { comment = r13.comment }\n");
    let report = report(&source);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.admitted().is_some());
    assert!(
        report
            .traversals()
            .iter()
            .filter(|stats| stats.reference_visits == 27
                && stats.memo_hits == 13
                && stats.limit.is_none())
            .count()
            >= 2
    );
}

#[test]
fn partially_applied_lookup_checks_call_and_pipe_at_the_authored_use() {
    for expression in ["pick(secret('d'))", "secret('d') |> pick()"] {
        let source = format!(
            "let pick = lookup({{ a = 'public' }}, 'k')\nmock.test.Resource {{ comment = {expression} }}\n"
        );
        let report = report(&source);
        assert_eq!(report.findings().len(), 1, "{expression}: {report:?}");
        let finding = &report.findings()[0];
        assert_eq!(
            finding.message(),
            "Attribute 'comment': lookup() alternatives mix secret and public values; all alternatives must be secret or all public."
        );
        assert_eq!(finding.location().span().start_line(), 2);
        assert_eq!(finding.location().span().end_line(), 2);
        let column = source.lines().nth(1).unwrap().find("pick").unwrap() + 1;
        assert_eq!(finding.location().span().start_column(), column);
        assert_eq!(
            finding.location().span().end_column(),
            column
                + if expression.starts_with("pick(") {
                    expression.len()
                } else {
                    "pick()".len()
                }
        );
    }
}

#[test]
fn partially_applied_builtins_keep_secret_evidence_for_calls_and_pipes() {
    for (binding, call, pipe) in [
        (
            "lookup({ a = secret('a') })",
            "pick('k', secret('d'))",
            "secret('d') |> pick('k')",
        ),
        (
            "lookup({ a = secret('a') }, 'k')",
            "pick(secret('d'))",
            "secret('d') |> pick()",
        ),
        ("join(secret('-'))", "pick(['d'])", "['d'] |> pick()"),
    ] {
        for expression in [call, pipe] {
            for name in ["pick", "alias"] {
                let expression = expression.replace("pick", name);
                let file = parsed(&format!(
                    "let pick = {binding}\nlet alias = pick\nlet sink = mock.test.Resource {{ name = 'sink' comment = {expression} }}\n"
                ));
                let schemas = schemas();
                let configuration =
                    crate::config_loader::load_buffer_configuration(&file, &schemas, &context());
                let report = configuration.judged().secrecy();
                assert!(
                    report.findings().is_empty(),
                    "{binding}, {expression}: {report:?}"
                );
                // Partial builtin calls have incomplete type judgement, so the
                // report stays pending even when its recorded evidence is secret.
                assert_eq!(
                    attribute(report, "sink", "comment"),
                    Evidence::Leaf(Secrecy::Secret)
                );
            }
        }
    }
}

#[test]
fn findings_name_the_authored_declaration_independently_of_reference_walk_order() {
    let binding = "let t = 'public' ?? secret('canary')\n";
    let sink = "mock.test.Resource { comment = t }\n";
    for source in [binding.to_string() + sink, sink.to_string() + binding] {
        let report = report(&source);
        assert_eq!(report.findings().len(), 1, "{report:?}");
        assert_eq!(
            report.findings()[0].message(),
            "Let binding 't': ?? alternatives mix secret and public values; all alternatives must be secret or all public."
        );
    }
    for (source, expected) in [
        (
            "exports { x: String = 'public' ?? secret('canary') }",
            "Export 'x'",
        ),
        (
            "let template = use { source = './unused' }\nlet instance = template { x = 'public' ?? secret('canary') }",
            "Module argument 'x'",
        ),
    ] {
        let report = report(source);
        assert_eq!(report.findings().len(), 1, "{source}: {report:?}");
        assert_eq!(
            report.findings()[0].message(),
            format!(
                "{expected}: ?? alternatives mix secret and public values; all alternatives must be secret or all public."
            )
        );
    }
}

#[test]
fn instantiated_output_and_default_argument_findings_keep_their_declaration_kinds() {
    for (body, expected) in [
        (
            "attributes { x: String = 'public' ?? secret('canary') }\n",
            "Output 'x'",
        ),
        (
            "arguments { x: String = 'public' ?? secret('canary') }\nmock.test.Resource { comment = x }\n",
            "Module argument 'x'",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("child/main.crn"), body).unwrap();
        std::fs::write(
            directory.path().join("main.crn"),
            "provider mock {}\nlet child = use { source = './child' }\nlet instance = child {}\n",
        )
        .unwrap();
        let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let context = context();
        let schemas = schemas();
        let root = crate::config_loader::parse_directory(&base, &context).unwrap();
        let expanded =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::module_resolver::resolve_modules_with_providerless_config(
                    root, &base, session, &schemas,
                )
                .unwrap()
            });
        let report = crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
            .judged()
            .secrecy()
            .clone();
        assert_eq!(report.findings().len(), 1, "{body}: {report:?}");
        assert_eq!(
            report.findings()[0].message(),
            format!(
                "{expected}: ?? alternatives mix secret and public values; all alternatives must be secret or all public."
            )
        );
        assert!(report.findings()[0].call_site().is_some());
    }
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
        Evidence::Map(BTreeMap::from([("public".into(), Evidence::Leaf(Secrecy::Public),)]).into())
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

#[test]
fn pending_reference_does_not_disable_memoization_of_later_dag() {
    use std::fmt::Write as _;
    for operands in ["${p}${r13.comment}", "${r13.comment}${p}"] {
        let mut source = "let pending = mock.test.Resource {}\nlet p = pending.missing\nlet r0 = mock.test.Resource { comment = 'x' }\n".to_string();
        for index in 1..=13 {
            writeln!(source, "let r{index} = mock.test.Resource {{ comment = join('', [r{previous}.comment, r{previous}.comment]) }}", previous = index - 1).unwrap();
        }
        writeln!(
            source,
            "let sink = mock.test.Resource {{ comment = \"{operands}\" }}"
        )
        .unwrap();
        let report = report(&source);
        assert!(report.findings().is_empty(), "{operands}: {report:?}");
        assert!(report.is_pending());
        assert!(
            report
                .traversals()
                .iter()
                .any(|stats| stats.attribute == "comment"
                    && stats.reference_visits == 29
                    && stats.memo_hits == 13
                    && stats.limit.is_none()),
            "{operands}: {:?}",
            report.traversals()
        );
    }
}

#[test]
fn literal_roots_skip_declaration_lookup_and_build_the_index_once() {
    DECLARATION_INDEX_OPERATIONS.with(|counts| counts.set((0, 0)));
    let source = (0..1000)
        .map(|index| {
            format!(
                "let r{index} = mock.test.Resource {{ name = 'r{index}' comment = 'literal' }}\n"
            )
        })
        .collect::<String>();
    let report = report(&source);
    assert!(report.admitted().is_some());
    assert_eq!(
        DECLARATION_INDEX_OPERATIONS.with(std::cell::Cell::get),
        (1, 0)
    );
}

#[test]
fn declaration_index_prunes_disjoint_attribute_spans_inside_one_resource() {
    use std::fmt::Write as _;
    let mut schema = Schema::new("test.Wide");
    let mut source = "let value = 'public'\nlet wide = mock.test.Wide {\n".to_string();
    for index in 0..1000 {
        schema = schema.attribute(Attribute::optional(format!("a{index}"), Type::string()).build());
        writeln!(source, "a{index} = value").unwrap();
    }
    source.push_str("}\n");
    let mut schemas = schemas();
    schemas.insert("mock", schema.validate().unwrap());
    DECLARATION_SPAN_CHECKS.with(|checks| checks.set(0));
    let report = secrecy_report(&parsed(&source), &schemas);
    assert!(report.admitted().is_some());
    assert_eq!(report.rows.values().next().unwrap().attributes.len(), 1000);
    let checks = DECLARATION_SPAN_CHECKS.with(std::cell::Cell::get);
    assert!(
        checks < 10_000,
        "disjoint siblings were scanned for each lookup: {checks}"
    );
}

#[test]
fn large_reference_free_let_reuses_evidence_across_attributes_without_changing_visits() {
    let fields = (0..1024)
        .map(|index| format!("k{index} = secret('canary')"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut source = format!("let shared = {{ {fields} }}\n");
    for index in 0..50 {
        source.push_str(&format!(
            "let r{index} = mock.test.Resource {{ name = 'r{index}' tags = shared }}\n"
        ));
    }
    let report = report(&source);
    assert!(report.admitted().is_some(), "{report:?}");
    let expected = Evidence::Map(
        (0..1024)
            .map(|index| (format!("k{index}"), Evidence::Leaf(Secrecy::Secret)))
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    for index in 0..50 {
        assert_eq!(attribute(&report, &format!("r{index}"), "tags"), expected);
    }
    let Evidence::Map(first) = &report.rows.values().next().unwrap().attributes["tags"] else {
        panic!("the literal let retains its map evidence")
    };
    for row in report.rows.values() {
        let Evidence::Map(fields) = &row.attributes["tags"] else {
            panic!("the shared map keeps its shape")
        };
        assert!(
            Arc::ptr_eq(first, fields),
            "memo hits must share the immutable evidence"
        );
    }
    let traversals = report
        .traversals()
        .iter()
        .filter(|stats| stats.attribute == "tags")
        .collect::<Vec<_>>();
    assert!(
        traversals.iter().all(|stats| stats.reference_visits == 1
            && stats.memo_hits == 0
            && stats.limit.is_none())
    );
    assert!(
        report
            .traversals()
            .iter()
            .map(|stats| stats.syntax_entries)
            .sum::<usize>()
            < 10_000,
        "reference-free syntax was walked again for each attribute: {:?}",
        report.traversals()
    );
}

#[test]
fn pending_selected_argument_reentry_is_outside_the_callee_activation() {
    let file = parsed(
        r#"
arguments { c: Bool }
fn g(m: map(String)): String { m.a }
let sink = mock.test.Resource { comment = g({ a = g({ a = if c { secret("x") } else { "y" } }) }) }
"#,
    );
    let schemas = schemas();
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
    let owner = Owner {
        syntax: &syntax,
        schemas: &schemas,
        resources: IndexMap::new(),
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        upstream: HashSet::new(),
        empty_environment: Arc::default(),
    };
    let resource = &file.resources[0];
    let slot = resource.authored_slots.get("comment").unwrap();
    let source = syntax
        .secrecy_root(&resource.id, slot.secrecy_node())
        .unwrap();
    let mut walk =
        Walk::new(&owner, "comment".into(), source.location().clone()).in_source(&source);
    let evidence = walk.run(Thunk {
        source: Source::Syntax(source),
        environment: owner.empty_environment.clone(),
    });
    assert!(walk.findings.is_empty(), "{:?}", walk.findings);
    assert!(evidence.is_none());
}

fn expanded_test_module(
    body: &str,
    arguments: &str,
) -> (tempfile::TempDir, crate::parser::ModuleResolvedInferredFile) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("child/main.crn"), body).unwrap();
    std::fs::write(directory.path().join("main.crn"), format!("provider mock {{}}\nlet seed = mock.test.Resource {{ name = 'seed' }}\nlet child = use {{ source = './child' }}\nlet instance = child {{ {arguments} }}\n")).unwrap();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let file = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
        .unwrap()
        .parsed;
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            file, &base, session, &schemas,
        )
        .unwrap()
    });
    (directory, expanded)
}

#[test]
fn module_argument_sources_have_distinct_nonempty_environment_identities() {
    let (_directory, file) = expanded_test_module(
        "arguments { value: String = 'public' }\nattributes { output: String = value }\n",
        "",
    );
    let schemas = schemas();
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas, &context());
    let owner = Owner {
        syntax: &syntax,
        schemas: &schemas,
        resources: IndexMap::new(),
        declarations: Vec::new(),
        declaration_index: OnceLock::new(),
        reference_free: RefCell::default(),
        literal_evidence: RefCell::default(),
        function_evidence: RefCell::default(),
        function_observations: RefCell::default(),
        argument_environments: RefCell::default(),
        upstream: HashSet::new(),
        empty_environment: Arc::default(),
    };
    let binding = crate::resource::binding_address::binding_selected_attribute(
        &file.compositions[0].binding_address(),
        "value",
    );
    let ReferenceSource::Source(source, path) =
        owner.reference_source(&binding, &[], owner.empty_environment.clone())
    else {
        panic!("the expanded argument has an authored source")
    };
    assert!(path.is_empty());
    assert_ne!(source.environment.id, owner.empty_environment.id);
    assert_ne!(source.environment.id, 0);
    let ReferenceSource::Source(repeated, _) =
        owner.reference_source(&binding, &[], owner.empty_environment.clone())
    else {
        panic!("the same argument remains available")
    };
    assert_eq!(
        source.environment.id, repeated.environment.id,
        "repeated encounters must retain their reference memo identity"
    );
}

#[test]
fn module_argument_fan_out_keeps_the_existing_reference_visit_count() {
    use std::fmt::Write as _;
    let mut body = "arguments { value: String = r13.comment }\nlet r0 = mock.test.Resource { comment = 'x' }\n".to_string();
    for index in 1..=13 {
        writeln!(body, "let r{index} = mock.test.Resource {{ comment = join('', [r{previous}.comment, r{previous}.comment]) }}", previous = index - 1).unwrap();
    }
    body.push_str("let sink = mock.test.Resource { comment = \"${value}${value}\" }\n");
    let (_directory, file) = expanded_test_module(&body, "");
    let report = crate::config_loader::load_buffer_configuration(&file, &schemas(), &context())
        .judged()
        .secrecy()
        .clone();
    assert!(report.admitted().is_some(), "{report:?}");
    assert!(
        report
            .traversals()
            .iter()
            .any(|stats| stats.attribute == "comment"
                && stats.reference_visits == 29
                && stats.memo_hits == 14
                && stats.limit.is_none()),
        "{:?}",
        report.traversals()
    );
}

#[test]
fn shape_builtins_preserve_public_selected_members_and_secret_controls() {
    for expression in [
        "keys(tokens)[0] ?? 'k'",
        "values(tokens)[1] ?? 'x'",
        "flatten([[secret('a')], ['b']])[1] ?? 'x'",
        "flatten([secret('a'), 'b'])[1] ?? 'x'",
        "\"${length(tokens)}\" ?? 'x'",
        "map('.public', [{ private = secret('a') public = 'b' }])[0] ?? 'x'",
    ] {
        let report = report(&format!(
            "let tokens = {{ a = secret('a') b = 'b' }}\nlet sink = mock.test.Resource {{ comment = {expression} }}\n"
        ));
        assert!(report.findings().is_empty(), "{expression}: {report:?}");
        assert!(report.admitted().is_some(), "{expression}: {report:?}");
        assert_eq!(
            attribute(&report, "sink", "comment"),
            Evidence::Leaf(Secrecy::Public)
        );
    }
    for expression in [
        "values(tokens)[0] ?? 'x'",
        "join('', values(tokens)) ?? 'x'",
        "keys(secret(tokens))[0] ?? 'k'",
    ] {
        let report = report(&format!(
            "let tokens = {{ a = secret('a') b = 'b' }}\nlet sink = mock.test.Resource {{ comment = {expression} }}\n"
        ));
        assert_eq!(report.findings().len(), 1, "{expression}: {report:?}");
        assert_eq!(
            report.findings()[0].cause(),
            &DerivationError::MixedAlternatives(MixedAlternatives {
                operator: Alternatives::Coalesce
            })
        );
    }
}

#[test]
fn concat_evidence_preserves_runtime_data_last_order_and_secret_members() {
    for (index, expected_findings) in [(0, 0), (1, 1)] {
        let report = report(&format!(
            "let sink = mock.test.Resource {{ comment = concat([secret('a')], ['b'])[{index}] ?? 'x' }}\n"
        ));
        assert_eq!(
            report.findings().len(),
            expected_findings,
            "{index}: {report:?}"
        );
        if index == 0 {
            assert!(report.admitted().is_some());
            assert_eq!(
                attribute(&report, "sink", "comment"),
                Evidence::Leaf(Secrecy::Public)
            );
        } else {
            assert_eq!(
                report.findings()[0].cause(),
                &DerivationError::MixedAlternatives(MixedAlternatives {
                    operator: Alternatives::Coalesce
                })
            );
        }
    }
}

#[test]
fn flatten_keeps_unknown_list_lengths_pending_instead_of_inventing_positions() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        Schema::new("test.ListSource")
            .attribute(Attribute::provider_populated("items", Type::list(Type::string())).build())
            .validate()
            .unwrap(),
    );
    let file = parsed(
        "let seed = mock.test.ListSource {}\nlet sink = mock.test.Resource { comment = flatten([seed.items, [secret('a')]])[1] ?? secret('fallback') }\n",
    );
    let report = secrecy_report(&file, &schemas);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(
        report.is_pending(),
        "the selected element depends on the unknown list length: {report:?}"
    );
    assert!(report.admitted().is_none());
}

#[test]
fn lookup_with_a_secret_key_protects_the_result_after_checking_alternatives() {
    let valid = report(
        "let sink = mock.test.Resource { comment = lookup({ a = 'public' }, secret('a'), 'default') ?? secret('fallback') }\n",
    );
    assert!(valid.findings().is_empty(), "{valid:?}");
    assert!(valid.admitted().is_some());
    assert_eq!(
        attribute(&valid, "sink", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    let mixed = report(
        "let sink = mock.test.Resource { comment = lookup({ a = secret('canary') }, secret('a'), 'public') }\n",
    );
    assert_eq!(mixed.findings().len(), 1, "{mixed:?}");
    assert_eq!(
        mixed.findings()[0].cause(),
        &DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Lookup
        })
    );
}

#[test]
fn mixed_function_body_operator_is_reported_once_with_function_owner() {
    let report = report(
        r#"
fn g(x: String): String { x ?? secret("canary") }
let first = mock.test.Resource { comment = g("a") }
let second = mock.test.Resource { comment = g("b") }
"#,
    );
    assert_eq!(report.findings().len(), 1, "{report:?}");
    assert_eq!(
        report.findings()[0].message(),
        "Function 'g': ?? alternatives mix secret and public values; all alternatives must be secret or all public."
    );
    assert!(report.findings()[0].call_site().is_none());
    assert_eq!(report.findings()[0].location().span().start_line(), 2);
    assert_eq!(
        report.findings()[0].location().span().end_column()
            - report.findings()[0].location().span().start_column(),
        2
    );
}

#[test]
fn issue_4490_resource_parameter_interpolation_is_secret_at_root_and_in_module() {
    let body = r#"
let seed = mock.test.Resource { name = "seed" tags = { token = secret("canary") } }
fn g(r: mock.test.Resource): String { "prefix-${r.tags.token}" }
let sink = mock.test.Resource { name = "sink" comment = g(seed) }
"#;
    let root = report(body);
    assert!(root.findings().is_empty(), "{root:?}");
    assert!(root.admitted().is_some(), "{root:?}");
    assert_eq!(
        attribute(&root, "sink", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );

    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        format!("arguments {{ unused: String = 'unused' }}\n{body}"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "provider mock {}\nlet child = use { source = './child' }\nlet instance = child {}\n",
    )
    .unwrap();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let file = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
        .unwrap()
        .parsed;
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            file, &base, session, &schemas,
        )
        .unwrap()
    });
    let report = crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
        .judged()
        .secrecy()
        .clone();
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(report.admitted().is_some(), "{report:?}");
    let comments = report
        .rows
        .values()
        .filter_map(|row| row.attributes.get("comment"))
        .collect::<Vec<_>>();
    assert_eq!(comments, vec![&Evidence::Leaf(Secrecy::Secret)]);
}

fn assert_directive_expression_admitted(case: &str, report: &SecrecyReport) {
    assert!(report.findings().is_empty(), "{case}: {report:?}");
    assert!(report.admitted().is_some(), "{case}: {report:?}");
    assert!(
        report
            .rows
            .values()
            .all(|row| !row.attributes.contains_key("directives")),
        "metadata must not enter attribute rows: {case}: {report:?}"
    );
}

#[test]
fn resource_directives_ignore_changes_local_map_is_metadata() {
    let report = report(
        "let tags = { Name = 'outside' }\nlet sink = mock.test.Resource { name = 'sink' tags = {} directives { ignore_changes = [tags.Name] } }\n",
    );
    assert_directive_expression_admitted("ignore_changes local map", &report);
}

#[test]
fn resource_directives_with_failed_expression_stays_pending() {
    let report = report(
        "let b = mock.test.Resource { name = 'b' }\nlet sink = mock.test.Resource { name = 'sink' directives { ignore_changes = [b.x] } }\n",
    );
    let file = parsed(
        "let b = mock.test.Resource { name = 'b' }\nlet sink = mock.test.Resource { name = 'sink' directives { ignore_changes = [b.x] } }\n",
    );
    let syntax = crate::parser::retained::judge_expressions(&file, &schemas(), &context());
    assert!(syntax.has_failed_expressions());
    assert_eq!(report.rows.len(), 2);
    assert!(report.rows.values().all(|row| row.attributes.len() == 1));
    assert!(
        report
            .traversals
            .iter()
            .any(|walk| walk.attribute == "directives")
    );
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(
        report.is_pending(),
        "the judge failure remains out of scope: {report:?}"
    );
    assert!(report.admitted().is_none());
}

fn assert_directive_dependency_keeps_base_pending(case: &str, report: &SecrecyReport) {
    assert!(report.findings().is_empty(), "{case}: {report:?}");
    assert!(
        report.is_pending(),
        "base cannot derive this dependency: {case}: {report:?}"
    );
    assert!(report.admitted().is_none());
    assert_eq!(report.rows.len(), 2);
    assert!(
        report
            .rows
            .values()
            .all(|row| row.attributes.len() == 1 && row.attributes.contains_key("name")),
        "the count check passes: {report:?}"
    );
    assert!(
        report
            .traversals
            .iter()
            .any(|walk| walk.attribute == "directives")
    );
}

#[test]
fn resource_directives_depends_on_if_binding_is_metadata() {
    let prefix = "let selected = if true { mock.test.Resource { name = 'selected' } } else { mock.test.Resource { name = 'unused' } }\n";
    let control = report(&format!(
        "{prefix}let sink = mock.test.Resource {{ name = 'sink' }}\n"
    ));
    assert_directive_expression_admitted("if binding control", &control);
    let observed = report(&format!(
        "{prefix}let sink = mock.test.Resource {{ name = 'sink' directives {{ depends_on = [selected] }} }}\n"
    ));
    assert_directive_dependency_keeps_base_pending("depends_on if binding", &observed);
}

#[test]
fn resource_directives_depends_on_for_binding_is_metadata() {
    let prefix = "let items = for name in ['one'] { mock.test.Resource { name = name } }\n";
    let control = report(&format!(
        "{prefix}let sink = mock.test.Resource {{ name = 'sink' }}\n"
    ));
    assert_directive_expression_admitted("for binding control", &control);
    let observed = report(&format!(
        "{prefix}let sink = mock.test.Resource {{ name = 'sink' directives {{ depends_on = [items] }} }}\n"
    ));
    assert_directive_dependency_keeps_base_pending("depends_on for binding", &observed);
}

fn directive_module_report(root: &str, body: &str) -> SecrecyReport {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("child/main.crn"), body).unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        format!("let child = use {{ source = './child' }}\n{root}"),
    )
    .unwrap();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let context = context();
    let schemas = schemas();
    let file = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
        .unwrap()
        .parsed;
    let expanded = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            file, &base, session, &schemas,
        )
        .unwrap()
    });
    for resource in &expanded.resources {
        assert!(resource.authored_slots.get("directives").is_ok());
        let slots = resource
            .authored_slots
            .iter()
            .filter_map(|(name, slot)| {
                slot.attribute_row_key(name)
                    .map(|_| name.transport_identity().to_owned())
            })
            .collect::<std::collections::BTreeSet<_>>();
        let attributes = resource
            .attributes
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(slots, attributes, "module-expanded resource slots");
    }
    crate::config_loader::load_buffer_configuration(&expanded, &schemas, &context)
        .judged()
        .secrecy()
        .clone()
}

#[test]
fn resource_directives_module_argument_is_metadata() {
    let body = "arguments { flag: Bool }\nmock.test.Resource { name = 'child' directives { prevent_destroy = flag } }\n";
    let report = directive_module_report("let instance = child { flag = true }\n", body);
    assert_directive_expression_admitted("module argument", &report);
}

#[test]
fn resource_directives_keep_ordinary_discarded_attribute_findings() {
    let report = report(
        "if false { mock.test.Resource { name = 'discarded' comment = secret('canary') ?? 'public' directives { prevent_destroy = true } } } else { mock.test.Resource { name = 'selected' } }\n",
    );
    assert!(
        report
            .findings()
            .iter()
            .any(|finding| matches!(finding.cause(), DerivationError::MixedAlternatives(_))),
        "{report:?}"
    );
    assert!(report.admitted().is_none());
}

#[test]
fn resource_directives_keep_underivable_attribute_count_pending() {
    let file = parsed(
        "let role = mock.test.Resource { name = 'role' tags = {} }\nlet local = role.tags\nlet consumer = mock.test.Resource { name = 'consumer' comment = local.absent directives { prevent_destroy = true } }\n",
    );
    let consumer = file
        .resources
        .iter()
        .find(|resource| resource.id.identity_str() == Some("consumer"))
        .unwrap();
    assert_eq!(consumer.attributes.len(), 2);
    assert_eq!(
        consumer
            .authored_slots
            .iter()
            .filter(|(name, slot)| slot.attribute_row_key(name).is_some())
            .count(),
        2
    );
    assert_eq!(consumer.authored_slots.iter().count(), 3);
    let report = secrecy_report(&file, &schemas());
    assert!(report.findings().is_empty(), "{report:?}");
    let row = report.rows.get(&consumer.id).expect("consumer evidence");
    assert_eq!(row.attributes.len(), 1);
    assert_eq!(row.attributes["name"], Evidence::Leaf(Secrecy::Public));
    assert!(!row.attributes.contains_key("comment"));
    assert!(report.is_pending(), "{report:?}");
    assert!(report.admitted().is_none());
}

#[test]
fn resource_directives_mixed_alternatives_keep_findings_and_attribute_count() {
    let file = parsed(
        "mock.test.Resource { name = 'sink' comment = secret('canary') directives { prevent_destroy = true ?? secret(true) } }\n",
    );
    let observed = secrecy_report(&file, &schemas());
    assert!(
        observed
            .findings()
            .iter()
            .any(|finding| matches!(finding.cause(), DerivationError::MixedAlternatives(_))),
        "{observed:?}"
    );
    assert!(
        observed
            .findings()
            .iter()
            .any(|finding| finding.message().starts_with("Attribute 'directives':")),
        "{observed:?}"
    );
    let row = observed
        .rows
        .values()
        .next()
        .expect("ordinary resource evidence");
    assert_eq!(file.resources[0].attributes.len(), 2);
    assert_eq!(
        row.attributes.len(),
        file.resources[0].attributes.len(),
        "the count comparison passes"
    );
    assert_eq!(row.attributes["name"], Evidence::Leaf(Secrecy::Public));
    assert_eq!(row.attributes["comment"], Evidence::Leaf(Secrecy::Secret));
    assert!(!row.attributes.contains_key("directives"));
    assert!(
        observed.admitted().is_none(),
        "findings prevent file admission"
    );
}

#[test]
fn resource_directives_assignment_and_block_keep_base_attribute_in_both_orders() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        Schema::new("test.Custom")
            .attribute(Attribute::optional("name", Type::string()).build())
            .attribute(Attribute::optional("directives", Type::string()).build())
            .validate()
            .unwrap(),
    );
    for body in [
        "directives = x directives { prevent_destroy = true }",
        "directives { prevent_destroy = true } directives = x",
        "directives = x directives {}",
        "directives {} directives = x",
    ] {
        let file = parsed(&format!(
            "let x = secret('canary')\nlet sink = mock.test.Custom {{ name = 'sink' {body} }}\n"
        ));
        let observed = secrecy_report(&file, &schemas);
        assert!(observed.findings().is_empty(), "{body}: {observed:?}");
        let row = observed.rows.values().next().unwrap();
        assert_eq!(
            row.attributes.len(),
            file.resources[0].attributes.len(),
            "{body}: {observed:?}"
        );
        assert_eq!(
            row.attributes["directives"],
            Evidence::Leaf(Secrecy::Secret)
        );
        assert!(
            observed.is_pending(),
            "the base judge remains pending: {body}: {observed:?}"
        );
        assert!(observed.admitted().is_none(), "{body}: {observed:?}");
    }
}
