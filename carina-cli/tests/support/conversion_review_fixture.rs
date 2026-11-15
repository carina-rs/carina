#[path = "conversion_factory.rs"]
mod base;

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
        schemas.push(
            ResourceSchema::new("AliasedQuery")
                .as_data_source()
                .attribute(
                    AttributeSchema::optional(
                        "rules",
                        AttributeType::list(AttributeType::struct_(
                            "Rule",
                            vec![StructField::optional(
                                "conditions",
                                AttributeType::list(AttributeType::struct_(
                                    "Condition",
                                    vec![StructField::optional("name", named()).build()],
                                )),
                            )
                            .with_block_name("condition")
                            .build()],
                        )),
                    )
                    .with_block_name("rule")
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.push(
            ResourceSchema::new("StructUnionQuery")
                .as_data_source()
                .attribute(AttributeSchema::optional(
                    "attr",
                    AttributeType::union(vec![
                        AttributeType::struct_(
                            "A",
                            vec![StructField::required("a", AttributeType::string()).build()],
                        ),
                        AttributeType::struct_(
                            "B",
                            vec![StructField::optional("b", named()).build()],
                        ),
                    ]),
                ).build())
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
        schemas.push(
            ResourceSchema::new("StructSink")
                .attribute(
                    AttributeSchema::optional(
                        "cfg",
                        AttributeType::struct_(
                            "NamedConfig",
                            vec![StructField::optional("name", named()).build()],
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
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
