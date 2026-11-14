use super::*;
use crate::parser::{AnalysisTree, ProviderContext};
use crate::resource::{ConcreteValue, DeferredValue, RelativeBindingName};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};
use crate::validation::deferred_types::with_session_for_test;

thread_local! {
    static INJECT_ERRORS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Inject at the worker's collection seam. This does not claim that DSL
// validation can reach KnowledgeError, or change evaluation permissions.
pub(super) fn collect_injected_errors<B: BindingAddress>(
    classified: &IndexMap<String, Classification<'_, ParserPlaceholder, B>>,
    mut failed: impl FnMut(&str, KnowledgeError<B>),
) {
    if INJECT_ERRORS.get() {
        for name in classified.keys() {
            failed(
                name,
                KnowledgeError::InvalidStage {
                    operation: "injected classification failure",
                },
            );
        }
    }
}

fn with_injected_errors<R>(run: impl FnOnce() -> R) -> R {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            INJECT_ERRORS.set(self.0);
        }
    }
    let _reset = Reset(INJECT_ERRORS.replace(true));
    run()
}

#[test]
fn both_pass_errors_preserve_order_duplicates_and_preflight_equivalence() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("other", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed =
        crate::parser::parse_and_resolve("mock.Test { name = 'before' other = 'unchanged' }")
            .unwrap();
    assert_both_passes(&parsed, &schemas, &catalog, ValidationEvaluation::None);
    assert_both_passes(
        &parsed,
        &schemas,
        &catalog,
        ValidationEvaluation::relative_static(),
    );
    assert_both_passes(
        &parsed.resolve_root_for_test(),
        &schemas,
        &catalog,
        ValidationEvaluation::expanded(),
    );
}

fn assert_both_passes<E, D, B>(
    parsed: &crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &SchemaRegistry,
    catalog: &ProviderContext,
    evaluation: ValidationEvaluation<B>,
) where
    B: BindingAddress,
    E: Clone + crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: Clone + crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let configuration = crate::config_loader::load_buffer_configuration(parsed, schemas, catalog);
    let judged = configuration.judged();
    let tree = AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let mut values = resource.attributes().into_owned();
    values.insert(
        "name".into(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![Value::Concrete(ConcreteValue::String("after".into())).into()],
            source_line: 0.into(),
        }),
    );
    let make_inputs =
        || ResourceAttributeInputs::from_resolved(resource, &values, judged.report()).unwrap();
    let bindings = crate::binding_index::BindingIndex::from_parsed(judged.file(), schemas, catalog);
    with_session_for_test(catalog, |session| {
        let inputs = make_inputs();
        let mut coverage = WalkCoverage::default();
        let preflight = crate::validation::admission::tests::without_admission_work(|| {
            with_injected_errors(|| classification_preflight(&inputs, evaluation, session))
        });
        assert!(coverage.is_empty());
        assert_eq!(coverage.work(), 0);
        let outcome = with_injected_errors(|| {
            resource_attribute_admissions(
                resource,
                judged.file(),
                resource.schema(schemas).unwrap(),
                resource.quoted_string_attrs(),
                inputs,
                evaluation,
                &bindings,
                session,
                schemas,
                &mut coverage,
            )
        });
        assert_eq!(outcome.classification_diagnostics.0, preflight.0);
        let expected = if matches!(evaluation, ValidationEvaluation::None) {
            vec!["name", "other"]
        } else {
            vec!["name", "other", "name"]
        };
        assert_eq!(
            preflight
                .0
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(preflight.0.iter().all(|(_, error)| matches!(
            error,
            KnowledgeError::InvalidStage {
                operation: "injected classification failure"
            }
        )));
        assert!(coverage.work() > 0);
        assert_eq!(outcome.values.errors().count(), 0);
    });
}

#[test]
fn evaluation_modes_are_copy_without_copy_bindings_and_use_the_supplied_session() {
    fn copy<T: Copy>() {}
    copy::<ValidationEvaluation<RelativeBindingName>>();
    copy::<ValidationEvaluation<crate::resource::AbsoluteBindingAddress>>();
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_and_resolve("mock.Test { name = 'before' }").unwrap();
    let schemas = SchemaRegistry::new();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let tree = AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let mut values = resource.attributes().into_owned();
    values.insert(
        "name".into(),
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![Value::Concrete(ConcreteValue::String("ADMISSION_VALUE".into())).into()],
            source_line: 0.into(),
        }),
    );
    let inputs =
        ResourceAttributeInputs::from_report(resource, &values, configuration.judged().report());
    let other = ProviderContext::new(crate::EnvironmentSnapshot::new([(
        "ADMISSION_VALUE".into(),
        "from-session".into(),
    )]));
    with_session_for_test(&other, |session| {
        let (value, diagnostics) = with_classified_inputs(
            &inputs,
            ValidationEvaluation::relative_static(),
            session,
            |inputs, _| inputs.value("name").unwrap().clone(),
        );
        assert!(diagnostics.0.is_empty());
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("from-session".into()))
        );
    });
}
