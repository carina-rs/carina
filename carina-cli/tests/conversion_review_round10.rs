#[path = "support/conversion_factory.rs"]
mod base;

#[path = "support/conversion_round13.rs"]
mod round13;

use carina_core::{
    paths::ResolvedPath,
    provider::*,
    resource::{ConcreteValue, ResourceId, State, Value},
    schema::*,
};
use serde_json::{Value as Json, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tower_lsp::lsp_types::Diagnostic;

type Events = Arc<Mutex<Vec<Json>>>;

fn named() -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::from_dotted("mock.Name")),
        None,
        Some((Some(1), Some(8))),
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

struct MeasuringFactory {
    events: Events,
}
impl ProviderFactory for MeasuringFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Round 10 local measurement"
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let inner = carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        )
        .create_provider();
        let events = self.events.clone();
        Box::pin(async move {
            Ok(
                InitializeOutcome::unavailable().with_provider(Box::new(MeasuringProvider {
                    inner: Box::new(inner),
                    events,
                })),
            )
        })
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let mut schemas = base::factories().remove(0).schemas();
        schemas.push(
            ResourceSchema::new("Types")
                .attribute(
                    AttributeSchema::optional(
                        "other",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.Other")),
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
        schemas.push(
            ResourceSchema::new("PrefixSink")
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::refined_string(
                            Some(TypeIdentity::from_dotted("mock.PrefixName")),
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
        schemas.push(
            ResourceSchema::new("ListSink")
                .attribute(AttributeSchema::optional("attr", AttributeType::list(named())).build())
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("FloatSink")
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::refined_float(
                            Some(TypeIdentity::from_dotted("mock.Ratio")),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("BoundedFloatSink")
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::refined_float(
                            Some(TypeIdentity::from_dotted("mock.BoundedRatio")),
                            Some((
                                Some(carina_core::FiniteF64::new(1.0).unwrap()),
                                Some(carina_core::FiniteF64::new(8.0).unwrap()),
                            )),
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("IntSink")
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::refined_int(
                            Some(TypeIdentity::from_dotted("mock.Count")),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("NestedQuery")
                .as_data_source()
                .attribute(
                    AttributeSchema::optional(
                        "attr",
                        AttributeType::struct_(
                            "Nested",
                            vec![StructField::optional("value", named()).build()],
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        for (query, sink, destination) in [
            (
                "AmbiguousQuery",
                "AmbiguousSink",
                AttributeType::union(vec![member(1, 2), member(3, 8)]),
            ),
            (
                "UnionQuery",
                "UnionSink",
                AttributeType::union(vec![named(), AttributeType::string()]),
            ),
        ] {
            schemas.push(
                ResourceSchema::new(query)
                    .as_data_source()
                    .attribute(AttributeSchema::optional("attr", destination.clone()).build())
                    .validate()
                    .unwrap(),
            );
            schemas.push(
                ResourceSchema::new(sink)
                    .attribute(AttributeSchema::optional("attr", destination).build())
                    .validate()
                    .unwrap(),
            );
        }
        schemas
    }
}

struct MeasuringProvider {
    inner: Box<dyn Provider>,
    events: Events,
}
impl Provider for MeasuringProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        self.events
            .lock()
            .unwrap()
            .push(json!({"op":"read", "id":id.to_string(), "request":format!("{request:?}")}));
        self.inner.read(id, identifier, request)
    }
    fn read_data_source(
        &self,
        source: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.events.lock().unwrap().push(json!({"op":"read_data_source", "id":source.id.to_string(), "attributes":serde_json::to_value(&source.attributes).unwrap()}));
        let id = source.id.clone();
        let attributes = source.attributes.clone();
        // This local fixture echoes checked query inputs; no network or cloud IO.
        Box::pin(async move { Ok(State::existing(id, attributes.into_iter().collect())) })
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        self.events.lock().unwrap().push(json!({
            "op": "create",
            "id": id.to_string(),
            "attributes": serde_json::to_value(&request.resource().as_resource().attributes).unwrap(),
        }));
        self.inner.create(id, request, attempt)
    }
    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        self.inner.resume_create(request)
    }
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        self.inner.update(id, identifier, request, attempt)
    }
    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        self.events.lock().unwrap().push(json!({"op":"delete", "id":attempt.id().to_string(), "identifier":attempt.identifier().to_string(), "directives":format!("{:?}", attempt.request().directives())}));
        self.inner.delete(attempt)
    }
    fn required_permissions(
        &self,
        id: &ResourceId,
        op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        self.inner.required_permissions(id, op)
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    events: Events,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let fixture = Self {
            directory,
            events: Default::default(),
        };
        fixture.write("providers.crn", "provider mock {}\n");
        fixture.write(
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        fixture
    }
    fn path(&self) -> &Path {
        self.directory.path()
    }
    fn write(&self, file: &str, text: &str) {
        let path = self.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn factories(&self) -> Vec<Box<dyn ProviderFactory>> {
        vec![Box::new(MeasuringFactory {
            events: self.events.clone(),
        })]
    }
    fn validate(&self) -> Json {
        carina_cli::commands::validate::validate_diagnostics_with_factories(
            &ResolvedPath::new(self.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(self.factories()),
            Default::default(),
        )
        .json
    }
    fn lsp_file(&self, file: &str, text: &str) -> Vec<Diagnostic> {
        let loaded = loaded_provider_snapshot_from_factories(
            Arc::new(self.factories()),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = carina_lsp::test_support::Diagnostics::new(
            self.path(),
            Arc::new(loaded),
            vec!["mock".into()],
        );
        let path = self.path().join(file);
        let document = carina_lsp::document::Document::new_with_path(
            text.into(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        engine.analyze(&document, Some(&path))
    }
    async fn plan(&self) -> Json {
        self.events.lock().unwrap().clear();
        let runtime = carina_cli::test_support::TestCommandRuntime::new();
        let init =
            carina_cli::commands::init::run_init(&runtime, self.path(), false, false, false, false)
                .await;
        let result = match init {
            Ok(()) => {
                carina_cli::test_support::run_plan_with_factories(
                    &runtime,
                    self.path(),
                    carina_cli::wiring::LoadedProviderFactory::for_test(self.factories()),
                )
                .await
            }
            Err(error) => {
                return json!({"ok":false,"setup_error":carina_cli::error::format_error_with_source_chain(&error)});
            }
        };
        let plan = std::fs::read(self.path().join("measured.plan.json"))
            .ok()
            .map(|bytes| serde_json::from_slice::<Json>(&bytes).unwrap());
        match result {
            Ok(changed) => {
                json!({"ok":true,"changed":changed,"plan":plan,"events":self.events.lock().unwrap().clone()})
            }
            Err(error) => {
                json!({"ok":false,"error":carina_cli::error::format_error_with_source_chain(&error),"debug":format!("{error:?}"),"events":self.events.lock().unwrap().clone()})
            }
        }
    }
    fn upstream(&self, integers: bool) {
        use carina_core::parser::ExpandedBackendConfig;
        use carina_state::{BackendLock, StateFile};
        let ty = if integers { "Int" } else { "String" };
        let values = if integers { "[7]" } else { "['ok']" };
        self.write(
            "up/main.crn",
            &format!("exports {{ names: list({ty}) = {values} flag: Bool = true }}\n"),
        );
        self.write(
            "up/backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        let mut state = StateFile::new();
        state.insert_export_json_for_test(
            "names",
            if integers { json!([7]) } else { json!(["ok"]) },
        );
        state.insert_export_json_for_test("flag", json!(true));
        std::fs::write(
            self.path().join("up/carina.state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let backend = ExpandedBackendConfig {
            backend_type: "local".into(),
            attributes: HashMap::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String("carina.state.json".into())),
            )])
            .into(),
            attribute_spans: Default::default(),
        };
        BackendLock::for_config(Some(&backend))
            .unwrap()
            .save(&ResolvedPath::new(self.path().join("up")).unwrap())
            .unwrap();
    }
    async fn observe(&self, label: &str, source: &str) -> Json {
        self.write("main.crn", source);
        let observation = json!({"label":label,"source":source,"validate":self.validate(),"lsp":self.lsp_file("main.crn",source),"plan":self.plan().await});
        let normalized = serde_json::to_string(&observation)
            .unwrap()
            .replace(self.path().to_str().unwrap(), "<fixture>");
        println!("R10 {normalized}");
        serde_json::from_str(&normalized).unwrap()
    }
}
fn conversion_count(json: &Json) -> usize {
    json["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|error| {
            error["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
        })
        .count()
}
fn lsp_conversion_count(json: &Json) -> usize {
    json.as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| {
            diagnostic["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("conversion_"))
        })
        .count()
}

#[derive(Clone, Copy)]
enum DeferredCase {
    WrongTarget,
    WrongKind,
    LiteralKind,
    Ambiguous,
    Union,
    Valid,
    PlainMismatch,
    PlainLoop,
}
async fn deferred(case: DeferredCase, read: bool, resource_iterable: bool) {
    let fixture = Fixture::new();
    fixture.upstream(matches!(
        case,
        DeferredCase::WrongKind | DeferredCase::PlainMismatch | DeferredCase::PlainLoop
    ));
    let (query, sink, expression, rejection) = match case {
        DeferredCase::WrongTarget => (
            "Query",
            "Sink",
            "n as mock.Other",
            Some("conversion_destination"),
        ),
        DeferredCase::WrongKind => ("Query", "Sink", "n as mock.Name", None),
        DeferredCase::LiteralKind => (
            "Query",
            "Sink",
            "7 as mock.Name",
            Some("conversion_scalar_kind"),
        ),
        DeferredCase::Ambiguous => (
            "AmbiguousQuery",
            "AmbiguousSink",
            "n as mock.Name",
            Some("conversion_ambiguous_destination"),
        ),
        DeferredCase::Union => (
            "UnionQuery",
            "UnionSink",
            "p.output as mock.Name",
            Some("conversion_unsupported_position"),
        ),
        DeferredCase::Valid => ("Query", "Sink", "'ok' as mock.Name", None),
        DeferredCase::PlainMismatch => ("Query", "Sink", "7", None),
        DeferredCase::PlainLoop => ("Query", "Sink", "n", None),
    };
    let prelude = if resource_iterable {
        "let p = mock.Source {}\n"
    } else {
        "let up = upstream_state { source = './up' }\n"
    };
    let iterable = if resource_iterable {
        "p.items"
    } else {
        "up.names"
    };
    let body = if read {
        format!("read mock.{query} {{ attr = {expression} }}")
    } else {
        format!("mock.{sink} {{ attr = {expression} }}")
    };
    let producer = if matches!(case, DeferredCase::Union) {
        "let p = mock.Source {}\n"
    } else {
        ""
    };
    let source = format!("{prelude}{producer}let outputs = for n in {iterable} {{ {body} }}\n");
    let result = fixture
        .observe(
            &format!(
                "deferred read={read} resource_iterable={resource_iterable} {expression} {query}"
            ),
            &source,
        )
        .await;
    if read {
        assert_eq!(result["validate"]["status"], "error", "{result}");
        // The ordinary finalizer still blocks every deferred read. Judge-owned
        // static source failures can precede it, including in a stopped run.
        let static_rejection = usize::from(
            matches!(case, DeferredCase::LiteralKind)
                || (resource_iterable && matches!(case, DeferredCase::WrongTarget)),
        );
        assert_eq!(
            conversion_count(&result["validate"]),
            static_rejection,
            "{result}"
        );
        assert_eq!(
            lsp_conversion_count(&result["lsp"]),
            static_rejection,
            "{result}"
        );
        assert!(
            result["validate"]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["message"]
                    .as_str()
                    .unwrap()
                    .contains("only a managed resource body can be deferred until apply")),
            "{result}"
        );
        assert_eq!(result["plan"]["ok"], false, "{result}");
        assert!(
            result["plan"]["events"].as_array().unwrap().is_empty(),
            "{result}"
        );
    } else if let Some(code) = rejection {
        assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
        assert!(
            result["validate"]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["code"] == code),
            "{result}"
        );
        assert_eq!(lsp_conversion_count(&result["lsp"]), 1, "{result}");
        assert_eq!(
            result["plan"]["ok"], false,
            "unjudged conversion reached planning: {result}"
        );
    } else if matches!(case, DeferredCase::Valid) {
        assert_eq!(result["validate"]["status"], "ok", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
        assert_eq!(result["plan"]["ok"], true, "{result}");
    } else if matches!(case, DeferredCase::PlainLoop | DeferredCase::WrongKind) {
        assert_eq!(result["validate"]["status"], "ok", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
        assert_eq!(result["plan"]["ok"], false, "{result}");
        assert!(
            result["plan"]["error"]
                .as_str()
                .unwrap()
                .contains("plan failed with 1 error(s)"),
            "{result}"
        );
    } else {
        assert_eq!(result["validate"]["status"], "error", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        // The current LSP has no static finding for this no-as control.
        assert!(
            !result["lsp"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["severity"] == 1),
            "{result}"
        );
        assert_eq!(result["plan"]["ok"], false, "{result}");
    }
}
macro_rules! deferred_case {
    ($name:ident, $case:ident, $read:expr, $resource:expr) => {
        #[tokio::test]
        async fn $name() {
            deferred(DeferredCase::$case, $read, $resource).await;
        }
    };
}
deferred_case!(
    r10_deferred_read_wrong_identity_is_blocked_by_finalization,
    WrongTarget,
    true,
    false
);
deferred_case!(
    r10_deferred_read_int_source_is_blocked_by_finalization,
    WrongKind,
    true,
    false
);
deferred_case!(
    r10_deferred_read_literal_kind_is_blocked_by_finalization,
    LiteralKind,
    true,
    false
);
deferred_case!(
    r10_managed_known_int_operand_control,
    LiteralKind,
    false,
    false
);
deferred_case!(
    r10_deferred_read_ambiguous_destination_is_blocked_by_finalization,
    Ambiguous,
    true,
    false
);
deferred_case!(
    r10_deferred_read_unknown_union_is_blocked_by_finalization,
    Union,
    true,
    false
);
deferred_case!(
    r10_deferred_read_valid_conversion_is_blocked_by_finalization,
    Valid,
    true,
    false
);
deferred_case!(
    r10_read_plain_static_mismatch_control,
    PlainMismatch,
    true,
    false
);
deferred_case!(
    r10_managed_wrong_identity_control,
    WrongTarget,
    false,
    false
);
deferred_case!(
    r10_managed_wrong_scalar_kind_control,
    WrongKind,
    false,
    false
);
deferred_case!(r10_managed_ambiguous_control, Ambiguous, false, false);
deferred_case!(r10_managed_unknown_union_control, Union, false, false);
deferred_case!(r10_managed_valid_control, Valid, false, false);
deferred_case!(
    r10_managed_plain_mismatch_control,
    PlainMismatch,
    false,
    false
);
deferred_case!(
    r10_read_plain_int_loop_source_control,
    PlainLoop,
    true,
    false
);
deferred_case!(
    r10_managed_plain_int_loop_source_control,
    PlainLoop,
    false,
    false
);
deferred_case!(
    r10_resource_iterable_read_wrong_identity_is_blocked_by_finalization,
    WrongTarget,
    true,
    true
);
deferred_case!(
    r10_resource_iterable_read_valid_is_blocked_by_finalization,
    Valid,
    true,
    true
);

macro_rules! nested_shape {
    ($name:ident, $converted:expr, $pending:expr) => {
        #[tokio::test]
        async fn $name() {
            let fixture = Fixture::new();
            fixture.upstream(false);
            let condition = if $pending { "up.flag" } else { "false" };
            let expression = format!("if {condition} {{ [['x' as mock.Name]] }} else {{ ['y' as mock.Name] }}");
            let expression = if $converted { expression } else { expression.replace(" as mock.Name", "") };
            let source = format!("let up = upstream_state {{ source = './up' }}\nmock.ListSink {{ attr = {expression} }}\n");
            let result = fixture.observe(stringify!($name), &source).await;
            if $pending {
                assert_eq!(result["validate"]["status"], "error", "{result}");
                assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
                assert!(result["validate"]["errors"].as_array().unwrap().iter().any(|e|e["message"].as_str().unwrap().contains("statically known")), "{result}");
                assert_eq!(result["plan"]["ok"], false, "{result}");
            } else {
                assert_eq!(result["validate"]["status"], "ok", "{result}");
                assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
                assert_eq!(result["plan"]["ok"], true, "{result}");
            }
        }
    };
}
nested_shape!(
    r10_discarded_nested_shape_conversion_matches_plain_control,
    true,
    false
);
nested_shape!(r10_discarded_nested_shape_plain_control, false, false);
nested_shape!(
    r10_runtime_if_syntax_blocks_the_pending_shape_example,
    true,
    true
);
nested_shape!(r10_pending_nested_shape_plain_control, false, true);

#[tokio::test]
async fn r10_prefix_missing_value_reports_p3_on_cli_and_lsp() {
    let fixture = Fixture::new();
    let source = "mock.PrefixSink { attr_prefix = 'ok' as mock.PrefixName }\n";
    let result = fixture.observe("prefix-root", source).await;
    assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
    assert!(
        result["validate"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "conversion_unsupported_position"
                && e["message"] == "conversion cannot be checked here"),
        "{result}"
    );
    let plain = fixture
        .observe(
            "prefix-root-plain-control",
            &source.replace(" as mock.PrefixName", ""),
        )
        .await;
    let messages = |diagnostics: &Json| {
        diagnostics
            .as_array()
            .unwrap()
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic["message"].clone(),
                    diagnostic["severity"].clone(),
                    diagnostic["code"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(messages(&result["lsp"]), messages(&plain["lsp"]));
    assert_eq!(lsp_conversion_count(&result["lsp"]), 0, "{result}");
    assert_eq!(result["plan"]["ok"], false, "{result}");
}

async fn reinstanced_prefix(module: bool) {
    let fixture = Fixture::new();
    let body = "mock.PrefixSink { attr_prefix = 'ok' as mock.PrefixName }";
    let source = if module {
        fixture.write(
            "child/main.crn",
            &format!("arguments {{ unused: String = 'ok' }}\n{body}\n"),
        );
        "let child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n"
            .to_owned()
    } else {
        format!("let sinks = for n in ['a','b'] {{ {body} }}\n")
    };
    let result = fixture
        .observe(&format!("reinstanced-prefix module={module}"), &source)
        .await;
    let module_lsp = if module {
        let child = std::fs::read_to_string(fixture.path().join("child/main.crn")).unwrap();
        Some(fixture.lsp_file("child/main.crn", &child))
    } else {
        None
    };
    println!(
        "R10-MODULE-LSP {}",
        serde_json::to_string(&module_lsp)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
    if !module {
        let plain = fixture
            .observe(
                "prefix-loop-plain-control",
                &source.replace(" as mock.PrefixName", ""),
            )
            .await;
        let messages = |diagnostics: &Json| {
            diagnostics
                .as_array()
                .unwrap()
                .iter()
                .map(|diagnostic| {
                    (
                        diagnostic["message"].clone(),
                        diagnostic["severity"].clone(),
                        diagnostic["code"].clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(messages(&result["lsp"]), messages(&plain["lsp"]));
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0, "{result}");
    }
}
#[tokio::test]
async fn r10_loop_reinstanced_missing_prefix_is_not_lost() {
    reinstanced_prefix(false).await;
}
#[tokio::test]
async fn r10_module_reinstanced_missing_prefix_is_not_lost() {
    reinstanced_prefix(true).await;
}

#[tokio::test]
async fn r10_review_read_binding_spelling_is_an_ordinary_syntax_error() {
    let fixture = Fixture::new();
    fixture.upstream(false);
    let source = "let up = upstream_state { source = './up' }\nfor n in up.names { let q = read mock.Query { attr = n as mock.Other } }\n";
    let result = fixture.observe("literal-review-spelling", source).await;
    assert_eq!(result["validate"]["status"], "error");
    assert_eq!(conversion_count(&result["validate"]), 0);
    assert!(
        result["validate"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("Syntax error"))
    );
}

#[tokio::test]
async fn r10_schema_absence_is_owned_by_ordinary_diagnostics() {
    for loaded in [true, false] {
        let fixture = Fixture::new();
        let mut observations = Vec::new();
        for converted in [false, true] {
            let source = if converted {
                "mock.NoSchema { attr = 'ok' as mock.Other }\n"
            } else {
                "mock.NoSchema { attr = 'ok' }\n"
            };
            fixture.write("main.crn", source);
            let factories = if loaded { fixture.factories() } else { vec![] };
            let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &ResolvedPath::new(fixture.path()).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories),
                Default::default(),
            )
            .json;
            let snapshot = loaded_provider_snapshot_from_factories(
                Arc::new(if loaded { fixture.factories() } else { vec![] }),
                SchemaRegistry::new(),
                carina_core::EnvironmentSnapshot::empty(),
            );
            let engine = carina_lsp::test_support::Diagnostics::new(
                fixture.path(),
                Arc::new(snapshot),
                if loaded { vec!["mock".into()] } else { vec![] },
            );
            let path = fixture.path().join("main.crn");
            let doc = carina_lsp::document::Document::new_with_path(
                source.into(),
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            let lsp = engine.analyze(&doc, Some(&path));
            let runtime = carina_cli::test_support::TestCommandRuntime::new();
            carina_cli::commands::init::run_init(
                &runtime,
                fixture.path(),
                false,
                false,
                false,
                false,
            )
            .await
            .unwrap();
            let plan = carina_cli::test_support::run_plan_with_factories(
                &runtime,
                fixture.path(),
                carina_cli::wiring::LoadedProviderFactory::for_test(if loaded {
                    fixture.factories()
                } else {
                    vec![]
                }),
            )
            .await;
            let plan = match plan {
                Ok(changed) => json!({"ok":true,"changed":changed}),
                Err(error) => {
                    json!({"ok":false,"error":carina_cli::error::format_error_with_source_chain(&error)})
                }
            };
            let observation = json!({"label":"schema-absence","loaded":loaded,"source":source,"validate":cli,"lsp":lsp,"plan":plan});
            println!(
                "R10 {}",
                serde_json::to_string(&observation)
                    .unwrap()
                    .replace(fixture.path().to_str().unwrap(), "<fixture>")
            );
            assert_eq!(conversion_count(&observation["validate"]), 0);
            assert_eq!(lsp_conversion_count(&observation["lsp"]), 0);
            observations.push(observation);
        }
        assert_eq!(
            observations[0]["validate"]["status"],
            observations[1]["validate"]["status"]
        );
    }
}

fn binary(fixture: &Fixture, args: &[&str]) -> Json {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env(
            "CARINA_MOCK_STATE_FILE",
            fixture.path().join("provider-state.json"),
        )
        .env(
            "CARINA_MOCK_OP_LOG",
            fixture.path().join("provider-ops.log"),
        )
        .output()
        .unwrap();
    json!({"ok":output.status.success(),"code":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)})
}
fn binary_seed(fixture: &Fixture, source: &str) {
    fixture.write("main.crn", source);
    let init = binary(fixture, &["init"]);
    assert_eq!(init["ok"], true, "{init}");
    let apply = binary(fixture, &["apply", "--auto-approve"]);
    assert_eq!(apply["ok"], true, "{apply}");
    fixture.write("provider-ops.log", "");
}

#[test]
fn r11_read_only_managed_calls_block_judge_conversion_failures() {
    let fixture = Fixture::new();
    binary_seed(
        &fixture,
        "let saved = mock.test.resource { name = 'persisted-name' }\n",
    );
    let source = "let saved = mock.test.resource { name = 'AUTHORED-UNJUDGED-R10' as Int }\n";
    fixture.write("main.crn", source);
    let validate = binary(&fixture, &["validate", "--json"]);
    let refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    let refreshed_state =
        std::fs::read_to_string(fixture.path().join("carina.state.json")).unwrap();
    let refresh_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    fixture.write("provider-ops.log", "");
    let destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    let destroy_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let observation = json!({"label":"read-only-managed","source":source,"validate":validate,"refresh":refresh,"refresh_ops":refresh_ops,"refreshed_state":serde_json::from_str::<Json>(&refreshed_state).unwrap(),"destroy":destroy,"destroy_ops":destroy_ops});
    println!(
        "R10 {}",
        serde_json::to_string(&observation)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(validate["ok"], false, "{observation}");
    assert_eq!(refresh["ok"], false, "{observation}");
    assert_eq!(destroy["ok"], false, "{observation}");
    assert!(refresh_ops.is_empty(), "{observation}");
    assert!(destroy_ops.is_empty(), "{observation}");
    fixture.write("main.crn", &source.replace(" as Int", ""));
    let plain_refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    assert_eq!(plain_refresh["ok"], true, "{plain_refresh}");
    let plain_destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    assert_eq!(plain_destroy["ok"], true, "{plain_destroy}");
    let plain_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    assert!(
        plain_ops.contains("read ") && plain_ops.contains("delete "),
        "{plain_ops}"
    );
    assert!(
        !refreshed_state.contains("AUTHORED-UNJUDGED-R10"),
        "{observation}"
    );
    assert!(refreshed_state.contains("persisted-name"), "{observation}");
}

#[test]
fn r10_state_refresh_cannot_dispatch_an_unjudged_data_source_argument() {
    let fixture = Fixture::new();
    binary_seed(
        &fixture,
        "let saved = mock.test.resource { name = 'persisted-name' }\nlet query = mock.iam.Roles { names = ['stored'] }\n",
    );
    // Keep the provider's query result, but remove its managed state row so
    // the only iam.Roles read below is the configured data-source call.
    let state_path = fixture.path().join("carina.state.json");
    let mut saved = carina_state::check_and_migrate(&std::fs::read_to_string(&state_path).unwrap())
        .unwrap()
        .into_state();
    assert!(
        saved
            .remove_resource("mock", "iam.Roles", "query")
            .is_some()
    );
    std::fs::write(&state_path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let source = "let saved = mock.test.resource { name = 'persisted-name' }\nlet query = read mock.iam.Roles { name_regex = 'AUTHORED-UNJUDGED-R10' as Int }\n";
    fixture.write("main.crn", source);
    let validate = binary(&fixture, &["validate", "--json"]);
    let refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    let operations = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let state = std::fs::read_to_string(fixture.path().join("carina.state.json")).unwrap();
    fixture.write("provider-ops.log", "");
    let destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    let destroy_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let observation = json!({"label":"read-only-data-source","source":source,"validate":validate,"refresh":refresh,"operations":operations,"state":serde_json::from_str::<Json>(&state).unwrap(),"destroy":destroy,"destroy_ops":destroy_ops});
    println!(
        "R10 {}",
        serde_json::to_string(&observation)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(validate["ok"], false, "{observation}");
    assert_eq!(refresh["ok"], false, "{observation}");
    assert_eq!(destroy["ok"], false, "{observation}");
    assert!(destroy_ops.is_empty(), "{observation}");
    assert!(
        !operations
            .lines()
            .any(|line| line.starts_with("read ") && line.contains("iam.Roles")),
        "unjudged authored data-source input crossed the provider boundary: {observation}"
    );
}

#[tokio::test]
async fn r10_prefix_plain_twin_matches_main_prefix_resolution() {
    let fixture = Fixture::new();
    let result = fixture
        .observe("prefix-plain", "mock.PrefixSink { attr_prefix = 'ok' }\n")
        .await;
    assert_eq!(result["validate"]["status"], "ok", "{result}");
    assert_eq!(result["plan"]["ok"], true, "{result}");
    assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
    assert!(
        result["lsp"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"] == "Unknown attribute 'attr_prefix'"),
        "{result}"
    );
}

#[tokio::test]
async fn r10_known_iterable_read_reaches_live_conversion_admission() {
    for (expression, code) in [
        ("n as mock.Name", None),
        ("n as mock.Other", Some("conversion_destination")),
        ("7 as mock.Name", Some("conversion_scalar_kind")),
    ] {
        let fixture = Fixture::new();
        let source = format!(
            "let queries = for n in ['ok'] {{ read mock.Query {{ attr = {expression} }} }}\n"
        );
        let result = fixture.observe("known-iterable-read", &source).await;
        assert_eq!(
            conversion_count(&result["validate"]),
            usize::from(code.is_some()),
            "{result}"
        );
        assert_eq!(
            lsp_conversion_count(&result["lsp"]),
            usize::from(code.is_some()),
            "{result}"
        );
        if let Some(code) = code {
            assert!(
                result["validate"]["errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|error| error["code"] == code),
                "{result}"
            );
            assert_eq!(result["plan"]["ok"], false, "{result}");
        } else {
            assert_eq!(result["validate"]["status"], "ok", "{result}");
            assert_eq!(result["plan"]["ok"], true, "{result}");
            assert!(
                result["plan"]["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["op"] == "read_data_source"),
                "{result}"
            );
        }
    }
}

#[tokio::test]
async fn r11_reinstanced_bare_operand_has_only_its_ordinary_slot_precheck() {
    let fixture = Fixture::new();
    let source = "let p = mock.Source {}\nlet sinks = for n in ['a', 'b'] { mock.Sink { attr = p as mock.Name } }\n";
    let measured = fixture
        .observe("r11-reinstanced-bare-operand", source)
        .await;
    assert_eq!(measured["validate"]["status"], "error", "{measured}");
    assert_eq!(conversion_count(&measured["validate"]), 0, "{measured}");
    assert_eq!(lsp_conversion_count(&measured["lsp"]), 0, "{measured}");
    assert!(
        measured["lsp"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("is a resource, not a value")),
        "{measured}"
    );
    let twin = fixture
        .observe(
            "r11-reinstanced-bare-plain",
            &source.replace(" as mock.Name", ""),
        )
        .await;
    let ordinary = |findings: &Json| {
        findings
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| {
                finding["message"]
                    .as_str()
                    .unwrap()
                    .contains("is a resource, not a value")
            })
            .map(|finding| {
                (
                    finding["message"].clone(),
                    finding["code"].clone(),
                    finding["severity"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ordinary(&measured["validate"]["errors"]),
        ordinary(&twin["validate"]["errors"])
    );
    assert_eq!(ordinary(&measured["lsp"]), ordinary(&twin["lsp"]));
}

#[tokio::test]
async fn r11_main_deferred_read_plain_whole_and_nested_references_are_blocked_by_finalization() {
    let fixture = Fixture::new();
    fixture.upstream(false);
    for body in [
        "read mock.Query { attr = n }",
        "read mock.NestedQuery { attr = { value = n } }",
    ] {
        let source = format!(
            "let up = upstream_state {{ source = './up' }}\nlet outputs = for n in up.names {{ {body} }}\n"
        );
        let observed = fixture
            .observe("main-deferred-read-plain-reference", &source)
            .await;
        assert_eq!(observed["validate"]["status"], "error", "{observed}");
        assert_eq!(conversion_count(&observed["validate"]), 0);
        assert_eq!(lsp_conversion_count(&observed["lsp"]), 0);
        for findings in [&observed["validate"]["errors"], &observed["lsp"]] {
            assert!(
                findings
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| finding["message"]
                        .as_str()
                        .unwrap()
                        .contains("only a managed resource body can be deferred until apply")),
                "{observed}"
            );
        }
        assert_eq!(observed["plan"]["ok"], false, "{observed}");
        assert!(observed["plan"]["events"].as_array().unwrap().is_empty());
    }
}
