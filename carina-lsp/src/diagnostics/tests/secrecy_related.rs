use super::*;
use carina_core::secrecy::{
    Alternatives, DerivationError, LimitKind, LimitReached, MixedAlternatives,
};

#[test]
fn secrecy_related_text_describes_each_derivation_cause() {
    for (cause, expected) in [
        (
            DerivationError::MixedAlternatives(MixedAlternatives {
                operator: Alternatives::Coalesce,
            }),
            "Authored alternative in module instance 'child'",
        ),
        (
            DerivationError::MixedAlternatives(MixedAlternatives {
                operator: Alternatives::Lookup,
            }),
            "Authored alternative in module instance 'child'",
        ),
        (
            DerivationError::LimitReached(LimitReached {
                kind: LimitKind::Depth,
            }),
            "Secrecy derivation limit reached in module instance 'child'",
        ),
        (
            DerivationError::LimitReached(LimitReached {
                kind: LimitKind::ReferenceVisits,
            }),
            "Secrecy derivation limit reached in module instance 'child'",
        ),
        (
            DerivationError::RecursiveFunction("recurse".into()),
            "Recursive function in module instance 'child'",
        ),
    ] {
        assert_eq!(secrecy_related_message(&cause, "child"), expected);
    }
}

#[test]
fn secrecy_related_position_rejects_zero_coordinates() {
    for (line, column) in [(0, 1), (1, 0), (0, 0), (2, 1)] {
        assert_eq!(secrecy_related_position("text", line, column), None);
    }
    assert_eq!(
        secrecy_related_position("😀 text", 1, 3),
        Some(Position::new(0, 3))
    );
}

#[test]
fn routed_secrecy_identity_uses_range_message_and_cause() {
    let first = AnalyzedDiagnostic {
        diagnostic: Diagnostic {
            message: "same authored finding".into(),
            ..Default::default()
        },
        cause: Some(DerivationError::MixedAlternatives(MixedAlternatives {
            operator: Alternatives::Coalesce,
        })),
    };
    let mut repeated = first.clone();
    repeated.diagnostic.related_information = Some(Vec::new());
    assert!(
        first.same(&repeated),
        "related rendering must not duplicate a cause at one site"
    );
    repeated.cause = Some(DerivationError::MixedAlternatives(MixedAlternatives {
        operator: Alternatives::Lookup,
    }));
    assert!(!first.same(&repeated));
    repeated = first.clone();
    repeated.diagnostic.range.end.character += 1;
    assert!(!first.same(&repeated));
    repeated = first.clone();
    repeated.diagnostic.message.push_str(" different");
    assert!(!first.same(&repeated));
}

#[test]
fn unavailable_related_source_keeps_the_authored_operator_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("child/main.crn");
    std::fs::create_dir(module.parent().unwrap()).unwrap();
    std::fs::write(
        &module,
        "arguments { token: String }\nmock.test.Resource { comment = token ?? 'default' }\n",
    )
    .unwrap();
    let root = directory.path().join("main.crn");
    let text = "provider mock {}\nlet child = use { source = './child' }\nlet instance = child { token = secret('canary') }\n";
    std::fs::write(&root, text).unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        carina_core::schema::PatternResourceSchema::new("test.Resource")
            .attribute(
                carina_core::schema::PatternAttributeSchema::optional(
                    "comment",
                    carina_core::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let base = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = carina_core::config_loader::parse_directory(&base, &context).unwrap();
    let expanded =
        carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
            carina_core::module_resolver::resolve_modules_with_providerless_config(
                parsed, &base, session, &schemas,
            )
            .unwrap()
        });
    let configuration =
        carina_core::config_loader::load_buffer_configuration(&expanded, &schemas, &context);
    let finding = configuration.judged().secrecy().findings().first().unwrap();
    let document = create_document_at(&std::fs::read_to_string(&module).unwrap(), module.clone());
    let doc = document.for_diagnostics().unwrap();
    let sources = carina_core::config_loader::SourceOverrides::single(
        carina_core::paths::ResolvedPath::new(&root).unwrap(),
        text.into(),
    );
    let original = render_secrecy(&doc, finding, &[], &sources).unwrap().1;
    assert!(original.related_information.is_some());
    let open = render_secrecy(&doc, finding, &[(root.clone(), Arc::from(""))], &sources)
        .unwrap()
        .1;
    assert!(
        open.related_information.is_none(),
        "an open unmappable buffer must not fall back to its disk text"
    );
    std::fs::remove_file(&root).unwrap();
    let pinned = render_secrecy(&doc, finding, &[], &sources).unwrap().1;
    assert_eq!(
        pinned.related_information, original.related_information,
        "range rendering must use the analysis snapshot even after a disk change"
    );
    let unavailable_sources = carina_core::config_loader::SourceOverrides::new();
    let missing = render_secrecy(&doc, finding, &[], &unavailable_sources)
        .expect("a missing related file must not hide the error")
        .1;
    assert_eq!(missing.range, original.range);
    assert_eq!(missing.message, original.message);
    assert!(missing.related_information.is_none());
    std::fs::write(&root, "").unwrap();
    let unmappable_sources = carina_core::config_loader::SourceOverrides::single(
        carina_core::paths::ResolvedPath::new(&root).unwrap(),
        String::new(),
    );
    let unmappable = render_secrecy(&doc, finding, &[], &unmappable_sources)
        .expect("a missing related line must not hide the error")
        .1;
    assert!(unmappable.related_information.is_none());
}
