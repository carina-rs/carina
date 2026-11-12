//! B1 contracts exercised through authored source and the ordinary validation
//! entry points. No synthetic Convert nodes or conversion routing fixtures.
//!
//! Tests return parse failures so today's missing `as` grammar is a normal red
//! test result, rather than a panic while assembling retained syntax.

use super::*;
use crate::binding_index::{BindingIndex, ResolvedBindings};
use crate::parser::{
    DeferredForPhaseTraversal, ExportParamLike, File, IdentifiedCustomTypeDeclaration,
    OwnedTypeIdentity, ProviderCustomTypes,
};
use crate::resource::knowledge::KnowledgeContext;
use crate::resource::{AbsoluteBindingAddress, ConcreteValue, ParserPlaceholder, Value};
use crate::schema::value_validation::WalkCoverage;
use crate::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StringIdentityLayer,
    StringMemberConstraints, StructField, TypeIdentity,
};
use crate::validation::admission::{Candidate, admit, attributes::ResourceAttributeInputs};
use crate::validation::deferred_types::with_session_for_test;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::path::Path;

type TestResult = Result<(), String>;
type Site = (String, usize, usize);

#[derive(Debug)]
struct Finding {
    conversion: Option<(ConversionErrorKind, Site)>,
    rendered: String,
}

/// The only conversion-error extractor. In particular this compiles before
/// InferenceError, ParseError and TypeError acquire conversion variants.
/// Wrappers must expose the ConversionError itself through Error::source;
/// flattening it into a string loses both the kind and its authored location.
fn finding(error: &(dyn Error + 'static)) -> Finding {
    let mut conversion = None;
    let mut rendered = format!("{error}\n{error:?}");
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<ConversionError>() {
            let location = error.location();
            conversion = Some((
                error.kind(),
                (
                    location
                        .file()
                        .path()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    location.span().start_line(),
                    location.span().start_column(),
                ),
            ));
        }
        rendered.push_str(&format!("\n{error}\n{error:?}"));
        current = error.source();
    }
    Finding {
        conversion,
        rendered,
    }
}

#[derive(Debug)]
struct Outcome {
    findings: Vec<Finding>,
    resources: Vec<(String, Vec<Finding>)>,
    published_error: Option<Finding>,
    work: usize,
}

impl Outcome {
    fn assert_accepted(&self) {
        assert!(
            self.findings.is_empty() && self.published_error.is_none(),
            "expected accepted source: {self:#?}"
        );
    }

    fn assert_findings(&self, expected: &[(ConversionErrorKind, Site)]) {
        let mut actual = self
            .findings
            .iter()
            .filter_map(|finding| finding.conversion.clone())
            .collect::<Vec<_>>();
        // Completeness failures can belong to the aggregate entry point
        // rather than an individual resource. Observe that independent result
        // when the per-resource run has no structured conversion findings.
        if actual.is_empty() {
            actual.extend(
                self.published_error
                    .iter()
                    .filter_map(|error| error.conversion.clone()),
            );
        }
        let mut expected = expected.to_vec();
        actual.sort_by_key(|(kind, site)| (site.clone(), format!("{kind:?}")));
        expected.sort_by_key(|(kind, site)| (site.clone(), format!("{kind:?}")));
        // Do not deduplicate here: duplicate diagnostics are a B1 regression.
        assert_eq!(actual, expected, "{self:#?}");
        assert!(
            !self.findings.is_empty() || self.published_error.is_some(),
            "expected rejection: {self:#?}"
        );
    }

    fn assert_masked(&self, plaintext: &str) {
        let rendered = self
            .findings
            .iter()
            .chain(self.published_error.iter())
            .map(|finding| finding.rendered.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!self.findings.is_empty(), "expected rejection: {self:#?}");
        assert!(rendered.contains("(secret)"), "{rendered}");
        assert!(!rendered.contains(plaintext), "{rendered}");
    }

    fn assert_instance_isolation(&self) {
        assert_eq!(self.resources.len(), 2, "{self:#?}");
        for (label, accepted) in [("ok", true), ("toolong-for-slot", false)] {
            let errors = self
                .resources
                .iter()
                .find(|(name, _)| name == label)
                .map(|(_, errors)| errors);
            assert_eq!(
                errors.map(|errors| errors.is_empty()),
                Some(accepted),
                "instance {label:?}: {self:#?}"
            );
        }
        assert_eq!(self.findings.len(), 1, "{self:#?}");
    }
}

fn named() -> AttributeType {
    named_string("mock.Name", 1, 8)
}

fn named_string(identity: &str, min: u64, max: u64) -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::from_dotted(identity)),
        None,
        Some((Some(min), Some(max))),
        None,
    )
}

fn member(min: u64, max: u64) -> AttributeType {
    AttributeType::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Name")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        StringMemberConstraints::new(None, Some((Some(min), Some(max)))),
    )
}

pub(super) fn catalog() -> ProviderContext {
    use crate::schema::CompiledAttributeType as T;
    let declarations = vec![
        T::refined_string(
            Some(TypeIdentity::from_dotted("mock.Name")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        T::refined_string(
            Some(TypeIdentity::from_dotted("mock.Other")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        T::refined_int(
            Some(TypeIdentity::from_dotted("mock.Count")),
            Some((Some(1), Some(8))),
        ),
        T::refined_float(
            Some(TypeIdentity::from_dotted("mock.Ratio")),
            Some((Some(crate::finite(1.0)), Some(crate::finite(8.0)))),
        ),
    ];
    let known = declarations
        .iter()
        .map(|declaration| {
            IdentifiedCustomTypeDeclaration::new(declaration)
                .map(|declaration| OwnedTypeIdentity::from_declaration("mock", declaration))
                .expect("named scalar fixture declaration")
        })
        .collect();
    ProviderContext {
        custom_types: ProviderCustomTypes::loaded(known, Box::new(|_, _| Ok(())))
            .with_declarations_for_test(BTreeMap::from([("mock".into(), declarations)])),
        ..ProviderContext::new(crate::EnvironmentSnapshot::empty())
    }
}

struct Fixture {
    catalog: ProviderContext,
    schemas: SchemaRegistry,
}

impl Fixture {
    fn new(source: AttributeType, target: AttributeType) -> Self {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Source")
                .attribute(AttributeSchema::optional("output", source).build())
                .validate()
                .expect("source fixture schema"),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("target", target).build())
                .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("other", named_string("mock.Other", 1, 8)).build(),
                )
                .validate()
                .expect("sink fixture schema"),
        );
        // Deliberately different managed/read declarations pin rb1 #3.
        schemas.insert(
            "mock",
            ResourceSchema::new("Lookup")
                .attribute(AttributeSchema::optional("different", AttributeType::int()).build())
                .validate()
                .expect("managed lookup fixture schema"),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("Lookup")
                .as_data_source()
                .attribute(AttributeSchema::optional("target", named()).build())
                .validate()
                .expect("data-source fixture schema"),
        );
        Self {
            catalog: catalog(),
            schemas,
        }
    }

    fn string() -> Self {
        Self::new(AttributeType::string(), named())
    }

    fn check(&self, source: &str) -> Result<Outcome, String> {
        let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &self.catalog)
            .map_err(|error| format!("normal parse rejected authored source: {error}"))?
            .resolve_root_for_test();
        check_parsed(&parsed, &self.schemas, &self.catalog)
    }

    fn directory(&self, files: &[(&str, &str)]) -> Result<Outcome, String> {
        let directory = directory(files)?;
        let path = crate::paths::ResolvedPath::new(directory.path()).map_err(|e| e.to_string())?;
        let parsed = crate::config_loader::parse_directory(&path, &self.catalog)?
            .into_parsed()
            .resolve_root_for_test();
        check_parsed(&parsed, &self.schemas, &self.catalog)
    }
}

fn directory(files: &[(&str, &str)]) -> Result<tempfile::TempDir, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    for (file, source) in files {
        let path = directory.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, source).map_err(|e| e.to_string())?;
    }
    Ok(directory)
}

fn check_parsed<E, D>(
    parsed: &File<E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>,
    schemas: &SchemaRegistry,
    catalog: &ProviderContext,
) -> Result<Outcome, String>
where
    E: Clone + ExportParamLike<ParserPlaceholder, AbsoluteBindingAddress>,
    D: Clone
        + DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, AbsoluteBindingAddress>,
{
    check_with_supplied_report(parsed, schemas, catalog, None)
}

fn check_with_supplied_report<E, D>(
    parsed: &File<E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>,
    schemas: &SchemaRegistry,
    catalog: &ProviderContext,
    supplied: Option<&crate::parser::retained::ExpressionReport<AbsoluteBindingAddress>>,
) -> Result<Outcome, String>
where
    E: Clone + ExportParamLike<ParserPlaceholder, AbsoluteBindingAddress>,
    D: Clone
        + DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, AbsoluteBindingAddress>,
{
    // Reports now travel with the file. Seed stale or detached reports before
    // the ordinary judgement boundary so these cases still require rejudging.
    let mut parsed = parsed.clone();
    if let Some(report) = supplied {
        crate::parser::retained::attach_report(&mut parsed, report.clone());
    }
    let configuration = crate::config_loader::load_buffer_configuration(&parsed, schemas, catalog);
    let judged = configuration.judged();
    let tree =
        crate::parser::AnalysisTree::try_from_file(judged.file()).map_err(|e| e.to_string())?;
    let bindings = BindingIndex::from_parsed(judged.file(), schemas, catalog);
    let mut coverage = WalkCoverage::default();
    with_session_for_test(catalog, |session| {
        let mut findings = judged
            .report()
            .findings()
            .iter()
            .map(|e| finding(e))
            .collect::<Vec<_>>();
        let signatures = crate::module_resolver::ResolvedModuleSignatures::new();
        let context =
            crate::validation::ValidationTypeContext::in_session(judged, session, &signatures);
        let bare = crate::validation::check_expanded_bare_binding_uses(&context);
        findings.extend(bare.findings().iter().map(|e| finding(e)));
        let mut resources = Vec::new();
        for resource in tree
            .resources()
            .filter(|resource| resource.kind() != crate::parser::AnalysisItemKind::Composition)
        {
            let attributes = resource.attributes();
            let inputs = ResourceAttributeInputs::for_resource(resource, judged.report())
                .map_err(|error| error.to_string())?;
            let classified = inputs.classify(&KnowledgeContext::validation(), |_, _| {});
            let schema = resource
                .schema(schemas)
                .ok_or_else(|| format!("missing fixture schema: {:?}", resource.id()))?;
            let admitted = crate::validation::admission::attributes::resource_attribute_admissions(
                resource,
                judged.file(),
                schema,
                &classified,
                resource.quoted_string_attrs(),
                &inputs,
                &bindings,
                session,
                schemas,
                &mut coverage,
            );
            let local = admitted
                .errors()
                .chain(admitted.symbolic_errors().map(|(_, _, error)| error))
                .map(|e| finding(e))
                .collect::<Vec<_>>();
            findings.extend(local.iter().map(|error| Finding {
                conversion: error.conversion.clone(),
                rendered: error.rendered.clone(),
            }));
            let label = match attributes.get("plain") {
                Some(Value::Concrete(ConcreteValue::String(label))) => label
                    .diagnostic_literal()
                    .map(str::to_owned)
                    .unwrap_or_else(|| resource.id().to_string()),
                Some(Value::Concrete(_) | Value::Deferred(_)) | None => resource.id().to_string(),
            };
            resources.push((label, local));
        }
        // Also require the CLI's public aggregate admission entry point to
        // agree. It currently renders errors into strings; keep the structured
        // findings above for kind/location and diagnostic-ownership assertions.
        let published_error = crate::validation::validate_resource_admissions(
            judged,
            &bindings,
            &HashSet::from(["mock".into()]),
            &mut Vec::new(),
        )
        .err()
        .map(|error| finding(&error));
        let work = coverage.work() + judged.report().completeness_report(&coverage).work();
        Ok(Outcome {
            findings,
            resources,
            published_error,
            work,
        })
    })
}

fn site_in(file: &str, source: &str, expression: &str) -> Site {
    let offset = source
        .find(expression)
        .expect("expected expression is in fixture source");
    let prefix = &source[..offset];
    (
        file.into(),
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
        prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1,
    )
}

fn site(source: &str, expression: &str) -> Site {
    site_in("conversion.crn", source, expression)
}

#[test]
fn conversion_foreign_named_source_matches_resource_destination() -> TestResult {
    for source in [AttributeType::string(), named_string("foreign.Name", 1, 64)] {
        Fixture::new(source, named()).check(
            "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }",
        )?.assert_accepted();
    }
    Ok(())
}

#[test]
fn conversion_destination_identity_must_match() -> TestResult {
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    Fixture::new(AttributeType::string(), named_string("mock.Other", 1, 8))
        .check(source)?
        .assert_findings(&[(
            ConversionErrorKind::Destination,
            site(source, "producer.output as"),
        )]);
    Ok(())
}

#[test]
fn conversion_source_kind_is_not_inferred_from_a_string_projection() -> TestResult {
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    Fixture::new(AttributeType::int(), named())
        .check(source)?
        .assert_findings(&[(
            ConversionErrorKind::ScalarKind,
            site(source, "producer.output as"),
        )]);
    Ok(())
}

#[test]
fn conversion_unknown_source_type_never_enters_ordinary_admission() -> TestResult {
    let source = "fn supply() { 'ok' }\nlet sink = mock.Sink { target = supply() as mock.Name }";
    Fixture::string()
        .check(source)?
        .assert_findings(&[(ConversionErrorKind::Source, site(source, "supply() as"))]);
    Ok(())
}

#[test]
fn conversion_non_scalar_source_never_enters_ordinary_admission() -> TestResult {
    let source = "let sink = mock.Sink { target = ['ok'] as mock.Name }";
    Fixture::string()
        .check(source)?
        .assert_findings(&[(ConversionErrorKind::Source, site(source, "['ok'] as"))]);
    Ok(())
}

#[test]
fn conversion_source_disjointness_is_checked_before_retyping() -> TestResult {
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    Fixture::new(named_string("foreign.Name", 20, 40), named())
        .check(source)?
        .assert_findings(&[(
            ConversionErrorKind::Disjoint,
            site(source, "producer.output as"),
        )]);
    Ok(())
}

#[test]
fn conversion_rebuild_r2_2_member_bounds_reject_disjoint_source() -> TestResult {
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    Fixture::new(named_string("foreign.Name", 4, 5), member(1, 3))
        .check(source)?
        .assert_findings(&[(
            ConversionErrorKind::Disjoint,
            site(source, "producer.output as"),
        )]);
    Ok(())
}

#[test]
fn conversion_rebuild_r2_1_rejected_int_to_float_never_enters_ordinary_admission() -> TestResult {
    let source = "let sink = mock.Sink { target = 42 as mock.Ratio }";
    let target = AttributeType::refined_float(Some(TypeIdentity::from_dotted("mock.Ratio")), None);
    Fixture::new(AttributeType::int(), target)
        .check(source)?
        .assert_findings(&[(ConversionErrorKind::ScalarKind, site(source, "42 as"))]);
    Ok(())
}

#[test]
fn conversion_rebuild_r2_3_source_matches_ordinary_interpolation() -> TestResult {
    for source in [AttributeType::int(), AttributeType::float()] {
        Fixture::new(source, named()).check(
            "let producer = mock.Source {}\nlet sink = mock.Sink { target = \"${producer.output}\" as mock.Name }",
        )?.assert_accepted();
    }
    Ok(())
}

#[test]
fn conversion_int_and_float_sources_require_only_same_kind_and_non_disjoint_bounds() -> TestResult {
    for (source, target, path, accepted) in [
        (
            AttributeType::refined_int(
                Some(TypeIdentity::from_dotted("foreign.Count")),
                Some((Some(1), Some(64))),
            ),
            AttributeType::refined_int(
                Some(TypeIdentity::from_dotted("mock.Count")),
                Some((Some(1), Some(8))),
            ),
            "mock.Count",
            true,
        ),
        (
            AttributeType::refined_int(
                Some(TypeIdentity::from_dotted("foreign.Count")),
                Some((Some(20), Some(40))),
            ),
            AttributeType::refined_int(
                Some(TypeIdentity::from_dotted("mock.Count")),
                Some((Some(1), Some(8))),
            ),
            "mock.Count",
            false,
        ),
        (
            AttributeType::refined_float(
                Some(TypeIdentity::from_dotted("foreign.Ratio")),
                Some((Some(crate::finite(1.0)), Some(crate::finite(64.0)))),
            ),
            AttributeType::refined_float(
                Some(TypeIdentity::from_dotted("mock.Ratio")),
                Some((Some(crate::finite(1.0)), Some(crate::finite(8.0)))),
            ),
            "mock.Ratio",
            true,
        ),
        (
            AttributeType::refined_float(
                Some(TypeIdentity::from_dotted("foreign.Ratio")),
                Some((Some(crate::finite(20.0)), Some(crate::finite(40.0)))),
            ),
            AttributeType::refined_float(
                Some(TypeIdentity::from_dotted("mock.Ratio")),
                Some((Some(crate::finite(1.0)), Some(crate::finite(8.0)))),
            ),
            "mock.Ratio",
            false,
        ),
    ] {
        let source_text = format!(
            "let producer = mock.Source {{}}\nlet sink = mock.Sink {{ target = producer.output as {path} }}"
        );
        let outcome = Fixture::new(source, target).check(&source_text)?;
        if accepted {
            outcome.assert_accepted();
        } else {
            outcome.assert_findings(&[(
                ConversionErrorKind::Disjoint,
                site(&source_text, "producer.output as"),
            )]);
        }
    }
    Ok(())
}

#[test]
fn conversion_secret_operand_is_allowed_but_secret_argument_is_rejected() -> TestResult {
    let fixture = Fixture::string();
    fixture
        .check("let sink = mock.Sink { target = (secret('ok')) as mock.Name }")?
        .assert_accepted();
    let source = "let sink = mock.Sink { target = secret('ok' as mock.Name) }";
    fixture.check(source)?.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "'ok' as"),
    )]);
    Ok(())
}

#[test]
fn conversion_known_secret_failure_is_owned_by_destination_validation() -> TestResult {
    let result = Fixture::string()
        .check("let sink = mock.Sink { target = (secret('sensitive-toolong')) as mock.Name }")?;
    assert_eq!(result.findings.len(), 1, "{result:#?}");
    result.assert_masked("sensitive-toolong");
    Ok(())
}

#[test]
fn conversion_protected_wrong_kind_preserves_the_error_class() -> TestResult {
    let source = "let sink = mock.Sink { target = (secret(8675309)) as mock.Name }";
    let outcome = Fixture::string().check(source)?;
    outcome.assert_findings(&[(
        ConversionErrorKind::ScalarKind,
        site(source, "(secret(8675309)) as"),
    )]);
    outcome.assert_masked("8675309");
    Ok(())
}

#[test]
fn conversion_alias_used_at_typed_slot_is_accepted() -> TestResult {
    Fixture::string().check(
        "let producer = mock.Source {}\nlet converted = producer.output as mock.Name\nlet alias = converted\nlet sink = mock.Sink { target = alias }",
    )?.assert_accepted();
    Ok(())
}

#[test]
fn conversion_unused_alias_and_dead_alias_chain_are_errors() -> TestResult {
    for tail in ["", "let alias = converted", "let _ = converted"] {
        let source = format!("let converted = 'ok' as mock.Name\n{tail}");
        Fixture::string()
            .check(&source)?
            .assert_findings(&[(ConversionErrorKind::Unused, site(&source, "'ok' as"))]);
    }
    Ok(())
}

#[test]
fn conversion_local_aliases_unused_locals_and_underscore_are_checked() -> TestResult {
    let fixture = Fixture::string();
    fixture
        .check("let sink = mock.Sink {\n let inner = 'ok' as mock.Name\n target = inner\n}")?
        .assert_accepted();
    for source in [
        "let sink = mock.Sink {\n let inner = 'ok' as mock.Name\n target = 'ok'\n}",
        "let _ = 'ok' as mock.Name",
    ] {
        fixture
            .check(source)?
            .assert_findings(&[(ConversionErrorKind::Unused, site(source, "'ok' as"))]);
    }
    Ok(())
}

#[test]
fn conversion_rv1_every_alias_use_path_must_reach_a_destination() -> TestResult {
    let values = "let converted = 'ok' as mock.Name\nlet alias = converted\n";
    for attributes in [
        "target = alias plain = \"${alias}\"",
        "plain = \"${alias}\" target = alias",
    ] {
        let main = format!("let sink = mock.Sink {{ {attributes} }}");
        let outcome = Fixture::string().directory(&[
            ("providers.crn", "provider mock {}"),
            ("values.crn", values),
            ("main.crn", &main),
        ])?;
        outcome.assert_findings(&[(
            ConversionErrorKind::UnsupportedPosition,
            site_in("values.crn", values, "'ok' as"),
        )]);
        assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    }
    Ok(())
}

#[test]
fn conversion_rb1_ordinary_top_level_let_uses_its_existing_string_judgement() -> TestResult {
    Fixture::string()
        .check("let raw = 'ok'\nlet sink = mock.Sink { target = raw as mock.Name }")?
        .assert_accepted();
    Ok(())
}

#[test]
fn conversion_rb1_converted_alias_enters_the_selected_data_source_attribute() -> TestResult {
    Fixture::string()
        .directory(&[
            ("providers.crn", "provider mock {}"),
            (
                "values.crn",
                "let producer = mock.Source {}\nlet converted = producer.output as mock.Name",
            ),
            (
                "main.crn",
                "let query = read mock.Lookup { target = converted }",
            ),
        ])?
        .assert_accepted();
    Ok(())
}

struct ConfigFactory;

impl crate::provider::ProviderFactory for ConfigFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "mock"
    }
    fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
        crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "endpoint".into(),
            named(),
        )]))
        .expect("typed provider-config fixture")
    }
    fn validate_config(
        &self,
        _: &crate::provider::CanonicalProviderConfigAttributes,
        _: &crate::provider::ProviderValidationInputs,
    ) -> crate::provider::ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &crate::provider::ProviderSession,
        _: crate::provider::InitializeOutcomeMint,
    ) -> crate::provider::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
    > {
        Box::pin(async {
            Err(crate::provider::ProviderError::internal(
                "validation-only fixture",
            ))
        })
    }
    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            vec![],
        )
        .expect("provider-config fixture witness")
    }
}

#[test]
fn conversion_rb1_provider_config_uses_its_independently_typed_destination() -> TestResult {
    let fixture = Fixture::string();
    let source = "provider mock { endpoint = 'ok' as mock.Name }";
    fixture.check(source)?.assert_accepted();
    let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?;
    crate::validation::validate_provider_config(&parsed, &[Box::new(ConfigFactory)])
}

#[test]
fn conversion_aliases_branches_and_literal_containers_match() -> TestResult {
    for (expression, target) in [
        ("(converted)", named()),
        ("if true { converted } else { converted }", named()),
        ("converted ?? converted", named()),
        ("[converted, converted]", AttributeType::list(named())),
        (
            "{ first = converted second = converted }",
            AttributeType::map(named()),
        ),
        (
            "{ entries = [converted] }",
            AttributeType::struct_(
                "Record",
                vec![StructField::required("entries", AttributeType::list(named())).build()],
            ),
        ),
    ] {
        let source = format!(
            "let converted = 'ok' as mock.Name\nlet alias = {expression}\nlet sink = mock.Sink {{ target = alias }}"
        );
        Fixture::new(AttributeType::string(), target)
            .check(&source)?
            .assert_accepted();
    }
    Ok(())
}

#[test]
fn conversion_review_mixed_branches_fail_closed() -> TestResult {
    for expression in [
        "if true { converted } else { 'ok' }",
        "if true { 'ok' } else { converted }",
        "if false { converted } else { 'ok' }",
        "if false { 'ok' } else { converted }",
        "converted ?? 'ok'",
        "'ok' ?? converted",
    ] {
        let source = format!(
            "let producer = mock.Source {{}}\nlet converted = producer.output as mock.Name\nlet sink = mock.Sink {{ target = {expression} }}"
        );
        let outcome = Fixture::string().check(&source)?;
        outcome.assert_findings(&[(
            ConversionErrorKind::UnsupportedPosition,
            site(&source, expression),
        )]);
        assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    }
    Ok(())
}

#[test]
fn conversion_nested_mixed_branches_fail_closed_with_matching_plain_references() -> TestResult {
    for (expression, target) in [
        (
            "if true { [converted] } else { [producer.output] }",
            AttributeType::list(named()),
        ),
        (
            "if true { [producer.output] } else { [converted] }",
            AttributeType::list(named()),
        ),
        (
            "if true { { a = converted } } else { { a = producer.output } }",
            AttributeType::map(named()),
        ),
        (
            "if true { { a = producer.output } } else { { a = converted } }",
            AttributeType::map(named()),
        ),
        (
            "[converted] ?? [producer.output]",
            AttributeType::list(named()),
        ),
        (
            "[producer.output] ?? [converted]",
            AttributeType::list(named()),
        ),
    ] {
        let source = format!(
            "let producer = mock.Source {{}}\nlet converted = producer.output as mock.Name\nlet sink = mock.Sink {{ target = {expression} }}"
        );
        Fixture::new(named(), target)
            .check(&source)?
            .assert_findings(&[(
                ConversionErrorKind::UnsupportedPosition,
                site(&source, expression),
            )]);
    }
    Ok(())
}

#[test]
fn conversion_every_alias_use_must_match_in_either_order() -> TestResult {
    for attributes in [
        "target = converted other = converted",
        "other = converted target = converted",
    ] {
        let source =
            format!("let converted = 'ok' as mock.Name\nlet sink = mock.Sink {{ {attributes} }}");
        let outcome = Fixture::string().check(&source)?;
        outcome.assert_findings(&[(ConversionErrorKind::Destination, site(&source, "'ok' as"))]);
        assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    }
    Ok(())
}

#[test]
fn conversion_union_exactly_one_matching_identity_is_accepted() -> TestResult {
    for members in [
        vec![named(), AttributeType::string()],
        vec![AttributeType::string(), named()],
    ] {
        Fixture::new(AttributeType::string(), AttributeType::union(members))
            .check("let sink = mock.Sink { target = 'ok' as mock.Name }")?
            .assert_accepted();
    }
    Ok(())
}

#[test]
fn conversion_union_zero_matching_identities_is_rejected() -> TestResult {
    let source = "let sink = mock.Sink { target = 'ok' as mock.Name }";
    Fixture::new(
        AttributeType::string(),
        AttributeType::union(vec![
            AttributeType::string(),
            named_string("mock.Other", 1, 8),
        ]),
    )
    .check(source)?
    .assert_findings(&[(ConversionErrorKind::Destination, site(source, "'ok' as"))]);
    Ok(())
}

#[test]
fn conversion_union_two_matching_identities_is_ambiguous() -> TestResult {
    let source = "let sink = mock.Sink { target = 'ok' as mock.Name }";
    for members in [
        vec![member(1, 2), member(3, 8)],
        vec![member(3, 8), member(1, 2)],
    ] {
        Fixture::new(AttributeType::string(), AttributeType::union(members))
            .check(source)?
            .assert_findings(&[(
                ConversionErrorKind::AmbiguousDestination,
                site(source, "'ok' as"),
            )]);
    }
    Ok(())
}

#[test]
fn conversion_known_condition_does_not_validate_the_discarded_value() -> TestResult {
    Fixture::string().check(
        "let sink = mock.Sink { target = if false { (secret('sensitive-toolong')) as mock.Name } else { (secret('ok')) as mock.Name } }",
    )?.assert_accepted();
    Ok(())
}

#[test]
fn conversion_discarded_branch_still_checks_scalar_kind() -> TestResult {
    let source =
        "let sink = mock.Sink { target = if false { 42 as mock.Name } else { 'ok' as mock.Name } }";
    Fixture::string()
        .check(source)?
        .assert_findings(&[(ConversionErrorKind::ScalarKind, site(source, "42 as"))]);
    Ok(())
}

#[test]
fn conversion_discarded_branch_still_checks_placement() -> TestResult {
    let source = "let converted = 'ok' as mock.Name\nlet sink = mock.Sink { plain = if false { upper(converted) } else { 'ok' } }";
    Fixture::string().check(source)?.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "'ok' as"),
    )]);
    Ok(())
}

#[test]
fn conversion_no_as_discarded_ordinary_wrong_kind_matches_main() -> TestResult {
    Fixture::new(AttributeType::string(), AttributeType::string())
        .check("let sink = mock.Sink { target = if false { 42 } else { 'ok' } }")?
        .assert_accepted();
    Ok(())
}

#[test]
fn conversion_ordinary_unused_let_remains_legal() -> TestResult {
    Fixture::string()
        .check("let ordinary = 'unused'")?
        .assert_accepted();
    Ok(())
}

#[test]
fn conversion_pending_branches_use_destination_value_checks_and_mask_secrets() -> TestResult {
    for (bad, good, plaintext) in [
        ("'toolong-for-slot'", "'ok'", None),
        (
            "(secret('sensitive-toolong'))",
            "(secret('ok'))",
            Some("sensitive-toolong"),
        ),
    ] {
        for expression in ["if choose { bad } else { good }", "maybe_converted ?? bad"] {
            let source = format!(
                "arguments {{ choose: Bool maybe: String }}\nlet bad = {bad} as mock.Name\nlet good = {good} as mock.Name\nlet maybe_converted = maybe as mock.Name\nlet sink = mock.Sink {{ target = {expression} }}"
            );
            // The unused declarations are omitted rather than accidentally
            // testing Unused instead of full admission of pending branches.
            let source = if expression.starts_with("if") {
                source.replace("let maybe_converted = maybe as mock.Name\n", "")
            } else {
                source.replace(&format!("let good = {good} as mock.Name\n"), "")
            };
            let outcome = Fixture::string().check(&source)?;
            assert_eq!(
                outcome.findings.len(),
                1,
                "pending destination skipped value validation: {outcome:#?}"
            );
            if let Some(plaintext) = plaintext {
                outcome.assert_masked(plaintext);
            }
        }
    }
    Ok(())
}

#[test]
fn conversion_pending_valid_branches_are_accepted() -> TestResult {
    for expression in ["if choose { first } else { second }", "first ?? second"] {
        let source = format!(
            "arguments {{ choose: Bool maybe: String }}\nlet first = maybe as mock.Name\nlet second = 'ok' as mock.Name\nlet sink = mock.Sink {{ target = {expression} }}"
        );
        Fixture::string().check(&source)?.assert_accepted();
    }
    Ok(())
}

#[test]
fn conversion_inside_interpolation_is_one_unsupported_finding() -> TestResult {
    let source = "let sink = mock.Sink { plain = \"${'ok' as mock.Name}\" }";
    let outcome = Fixture::string().check(source)?;
    outcome.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "'ok' as"),
    )]);
    assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    Ok(())
}

#[test]
fn conversion_inside_call_argument_is_one_unsupported_finding() -> TestResult {
    let source = "let sink = mock.Sink { plain = upper('ok' as mock.Name) }";
    let outcome = Fixture::string().check(source)?;
    outcome.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "'ok' as"),
    )]);
    assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    Ok(())
}

#[test]
fn conversion_inside_arithmetic_is_one_unsupported_finding() -> TestResult {
    let source = "let sink = mock.Sink { target = (2 as mock.Count) + 1 }";
    let outcome = Fixture::new(AttributeType::int(), AttributeType::int()).check(source)?;
    outcome.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "2 as"),
    )]);
    assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    Ok(())
}

#[test]
fn conversion_inside_access_is_one_unsupported_finding() -> TestResult {
    let source = "let sink = mock.Sink { plain = ['ok' as mock.Name][0] }";
    let outcome = Fixture::string().check(source)?;
    outcome.assert_findings(&[(
        ConversionErrorKind::UnsupportedPosition,
        site(source, "'ok' as"),
    )]);
    assert_eq!(outcome.findings.len(), 1, "{outcome:#?}");
    Ok(())
}

#[test]
fn conversion_exactly_one_diagnostic_per_rejected_convert_with_many_alias_uses() -> TestResult {
    for count in [1, 8, 16] {
        let uses = vec!["alias"; count].join(", ");
        let source = format!(
            "let converted = 42 as mock.Name\nlet alias = converted\nlet sink = mock.Sink {{ target = [{uses}] }}"
        );
        let outcome =
            Fixture::new(AttributeType::string(), AttributeType::list(named())).check(&source)?;
        outcome.assert_findings(&[(ConversionErrorKind::ScalarKind, site(&source, "42 as"))]);
        assert_eq!(outcome.findings.len(), 1, "N={count}: {outcome:#?}");
    }
    Ok(())
}

fn check_pending_discharge(at_apply: Option<(&str, bool)>) -> TestResult {
    let fixture = Fixture::string();
    let source = "let producer = mock.Source {}\nlet converted = producer.output as mock.Name\nlet sink = mock.Sink { target = converted }";
    let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|error| format!("normal parse rejected authored source: {error}"))?
        .resolve_root_for_test();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        &fixture.schemas,
        &fixture.catalog,
    );
    let judged = configuration.judged();
    let tree =
        crate::parser::AnalysisTree::try_from_file(judged.file()).map_err(|e| e.to_string())?;
    let sink = tree
        .resources()
        .find(|resource| resource.binding() == Some("sink"))
        .ok_or("parsed sink is missing")?;
    let slot = sink.authored_slot("target").map_err(|e| e.to_string())?;
    let value = slot
        .projection()
        .ok_or("parsed target projection is missing")?;
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map_err(|e| e.to_string())?
        .into_classification();
    let bindings = BindingIndex::from_parsed(judged.file(), &fixture.schemas, &fixture.catalog);
    let destination = fixture
        .schemas
        .resource_attribute_destination(sink.id(), "target", &fixture.catalog)
        .map_err(|e| e.to_string())?;
    with_session_for_test(&fixture.catalog, |session| {
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session)
                .map_err(|e| format!("slot admission: {:?}", finding(&e)))?;
        let admission = admit(&destination, candidate, &context)
            .map_err(|e| format!("expected pending conversion admission: {:?}", finding(&e)))?;
        let pending = match admission {
            crate::validation::admission::Admission::Pending(pending) => pending,
            crate::validation::admission::Admission::Ready(placed) => {
                return Err(format!(
                    "unknown operand was placed before apply: {placed:?}"
                ));
            }
        };
        let obligation = pending.local_obligation();
        assert!(
            obligation.is_some(),
            "conversion did not retain its NeedsLocal obligation: {pending:?}"
        );
        assert_eq!(
            obligation.map(|local| local.paths()),
            Some(
                &[crate::schema::LeafPath::from_segments([
                    crate::schema::LeafPathSegment::StructField("target".into()),
                ])][..]
            )
        );
        if let Some((text, accepted)) = at_apply {
            let applied_source = format!("let producer = mock.Source {{ output = '{text}' }}");
            let applied = crate::parser::parse_file(
                &applied_source,
                Path::new("applied.crn"),
                &fixture.catalog,
            )
            .map_err(|e| e.to_string())?
            .resolve_root_for_test();
            let resolved = ResolvedBindings::for_expanded_validation(&applied);
            // discharge re-enters admit_apply; no conversion runtime value or
            // forged matched-source token is involved in this fixture.
            let placed = pending.discharge(&resolved, session);
            assert_eq!(placed.is_ok(), accepted, "apply value {text:?}: {placed:?}");
            if let Ok(placed) = placed {
                assert_eq!(
                    placed.value(),
                    &Value::Concrete(ConcreteValue::String(text.into()))
                );
            }
        }
        Ok(())
    })
}

#[test]
fn conversion_raw_resource_and_binding_references_register_local_checks() -> TestResult {
    check_pending_discharge(None)
}

#[test]
fn conversion_pending_discharge_checks_the_destination_without_conversion_values() -> TestResult {
    check_pending_discharge(Some(("ok", true)))?;
    check_pending_discharge(Some(("toolong-for-slot", false)))
}

#[test]
fn conversion_for_expansion_instances_are_isolated() -> TestResult {
    for items in ["['ok', 'toolong-for-slot']", "['toolong-for-slot', 'ok']"] {
        let source = format!(
            "for item in {items} {{ mock.Sink {{ plain = item target = item as mock.Name }} }}"
        );
        let outcome = Fixture::string().check(&source)?;
        outcome.assert_instance_isolation();
    }
    Ok(())
}

#[test]
fn conversion_module_instances_are_isolated() -> TestResult {
    let fixture = Fixture::string();
    for calls in [
        "let one = child { label = 'ok' }\nlet two = child { label = 'toolong-for-slot' }",
        "let two = child { label = 'toolong-for-slot' }\nlet one = child { label = 'ok' }",
    ] {
        let root_source = format!("let child = use {{ source = './child' }}\n{calls}");
        let directory = directory(&[
            ("providers.crn", "provider mock {}"),
            ("main.crn", &root_source),
            ("child/extra.crn", "let unused = 'directory fixture'"),
            (
                "child/main.crn",
                "arguments { label: String }\nlet sink = mock.Sink { plain = label target = label as mock.Name }",
            ),
        ])?;
        let path = crate::paths::ResolvedPath::new(directory.path()).map_err(|e| e.to_string())?;
        let root = crate::config_loader::parse_directory(&path, &fixture.catalog)?;
        let parsed = with_session_for_test(&fixture.catalog, |session| {
            crate::module_resolver::resolve_modules_with_providerless_config(
                root,
                &path,
                session,
                &fixture.schemas,
            )
        })
        .map_err(|e| format!("normal module parse/expansion rejected authored source: {e}"))?;
        let outcome = check_parsed(&parsed, &fixture.schemas, &fixture.catalog)?;
        outcome.assert_instance_isolation();
    }
    Ok(())
}

#[test]
fn conversion_review_cached_placement_does_not_replace_original_source_checks() -> TestResult {
    let initial = Fixture::string();
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &initial.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?
        .resolve_root_for_test();
    let configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        &initial.schemas,
        &initial.catalog,
    );
    let previously_judged = configuration.judged().file().clone();
    let changed = Fixture::new(AttributeType::int(), named());
    check_with_supplied_report(
        &previously_judged,
        &changed.schemas,
        &initial.catalog,
        Some(configuration.judged().report()),
    )?
    .assert_findings(&[(
        ConversionErrorKind::ScalarKind,
        site(source, "producer.output as"),
    )]);
    Ok(())
}

#[test]
fn conversion_changed_bindings_are_rejudged_before_admission() -> TestResult {
    let fixture = Fixture::string();
    let initial_source = "let raw = 'ok'\nlet sink = mock.Sink { target = raw as mock.Name }";
    let initial = crate::parser::parse_file(
        initial_source,
        Path::new("conversion.crn"),
        &fixture.catalog,
    )
    .map_err(|e| format!("normal parse rejected authored source: {e}"))?
    .resolve_root_for_test();
    let old = crate::config_loader::load_buffer_configuration(
        &initial,
        &fixture.schemas,
        &fixture.catalog,
    );
    let source = "let raw = 42\nlet sink = mock.Sink { target = raw as mock.Name }";
    let changed = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?
        .resolve_root_for_test();
    check_with_supplied_report(
        &changed,
        &fixture.schemas,
        &fixture.catalog,
        Some(old.judged().report()),
    )?
    .assert_findings(&[(ConversionErrorKind::ScalarKind, site(source, "raw as"))]);
    Ok(())
}

#[test]
fn conversion_empty_supplied_report_cannot_hide_authored_syntax() -> TestResult {
    let fixture = Fixture::new(
        AttributeType::int(),
        AttributeType::refined_float(Some(TypeIdentity::from_dotted("mock.Ratio")), None),
    );
    let source = "let sink = mock.Sink { target = 42 as mock.Ratio }";
    let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?
        .resolve_root_for_test();
    let empty = crate::parser::parse_file("", Path::new("empty.crn"), &fixture.catalog)
        .map_err(|e| e.to_string())?
        .resolve_root_for_test();
    check_with_supplied_report(
        &parsed,
        &fixture.schemas,
        &fixture.catalog,
        Some(empty.expression_report()),
    )?
    .assert_findings(&[(ConversionErrorKind::ScalarKind, site(source, "42 as"))]);
    Ok(())
}

#[test]
fn conversion_source_snapshot_rejects_a_detached_report() -> TestResult {
    let fixture = Fixture::new(AttributeType::int(), named());
    let source = "let producer = mock.Source {}\nlet sink = mock.Sink { target = producer.output as mock.Name }";
    let parsed = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?
        .resolve_root_for_test();
    let other = crate::parser::parse_file(source, Path::new("conversion.crn"), &fixture.catalog)
        .map_err(|e| format!("normal parse rejected authored source: {e}"))?
        .resolve_root_for_test();
    let detached_fixture = Fixture::string();
    let detached = crate::config_loader::load_buffer_configuration(
        &other,
        &detached_fixture.schemas,
        &fixture.catalog,
    );
    check_with_supplied_report(
        &parsed,
        &fixture.schemas,
        &fixture.catalog,
        Some(detached.judged().report()),
    )?
    .assert_findings(&[(
        ConversionErrorKind::ScalarKind,
        site(source, "producer.output as"),
    )]);
    Ok(())
}

#[test]
fn conversion_completeness_work_scales_linearly_for_aliases_branches_and_containers() -> TestResult
{
    for shape in ["aliases", "branches", "containers"] {
        let mut work = Vec::new();
        for count in [16, 32] {
            let mut source = String::new();
            let mut target = named();
            let mut expression = "'ok' as mock.Name".to_owned();
            match shape {
                "aliases" => {
                    source.push_str("let first = 'ok' as mock.Name\n");
                    let mut previous = "first".to_owned();
                    for index in 0..count {
                        let alias = format!("alias{index}");
                        source.push_str(&format!("let {alias} = {previous}\n"));
                        previous = alias;
                    }
                    expression = format!("[{previous}, {previous}]");
                    target = AttributeType::list(named());
                }
                "branches" => {
                    for _ in 0..count {
                        expression =
                            format!("if true {{ {expression} }} else {{ 'ok' as mock.Name }}");
                    }
                }
                "containers" => {
                    for _ in 0..count {
                        expression = format!("[{expression}]");
                        target = AttributeType::list(target);
                    }
                }
                _ => unreachable!("fixture shape"),
            }
            source.push_str(&format!("let sink = mock.Sink {{ target = {expression} }}"));
            let outcome = Fixture::new(AttributeType::string(), target).check(&source)?;
            outcome.assert_accepted();
            work.push(outcome.work);
        }
        assert!(
            work[1] > work[0] && work[1] <= 2 * work[0] + 32,
            "{shape}: N and 2N work = {work:?}"
        );
    }
    Ok(())
}
