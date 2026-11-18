use carina_core::resource::{ResourceId, Value};
use carina_core::{provider::*, schema::*};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct FixtureFactory(Arc<Fixture>);

fn decode(value: &serde_json::Value) -> Value {
    carina_core::value::json_to_dsl_value(value)
        .unwrap()
        .unwrap()
}

fn request_json(attributes: &WireAttributes) -> serde_json::Value {
    serde_json::Value::Object(
        attributes
            .transport_iter()
            .map(|(key, value)| (key.to_owned(), serde_json::to_value(value.value()).unwrap()))
            .collect(),
    )
}

fn decode_attributes(attributes: &serde_json::Value) -> HashMap<String, Value> {
    attributes
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), decode(value)))
        .collect()
}

impl ProviderFactory for FixtureFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Declared annotation regression provider"
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
        let fixture = self.0.clone();
        Box::pin(async move {
            Ok(InitializeOutcome::unavailable().with_provider(Box::new(FixtureProvider(fixture))))
        })
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let mut thing = ResourceSchema::new("Thing")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("emitted", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "items",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .with_def(
                "Name",
                AttributeType::refined_string(
                    Some(TypeIdentity::from_dotted("test.Name")),
                    Some(PatternSpec::new("allowed", PatternMatch::Full)),
                    None,
                    None,
                ),
            );
        if self.0.private.lock().unwrap().is_some() {
            thing = thing.attribute(
                AttributeSchema::provider_populated("private", AttributeType::string()).build(),
            );
        }
        vec![
            thing.validate().unwrap(),
            ResourceSchema::new("NamedThing")
                .attribute(
                    AttributeSchema::required("name", AttributeType::string())
                        .create_only()
                        .identity()
                        .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Lookup")
                .as_data_source()
                .attribute(AttributeSchema::required("query", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("value", AttributeType::string()).build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(AttributeSchema::required("value", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("marker", AttributeType::string()).build())
                .validate()
                .unwrap(),
        ]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

use crate::{
    commands,
    error::{AppError, format_error_with_source_chain},
    test_support::TestCommandRuntime,
    wiring::LoadedProviderFactory,
};
use carina_core::{parser::ProviderContext, paths::ResolvedPath, shutdown::ShutdownToken};
use std::path::Path;

struct Fixture {
    emitted: Mutex<String>,
    private: Mutex<Option<String>>,
    rows: Mutex<BTreeMap<String, serde_json::Value>>,
    operations: Mutex<Vec<(String, String, serde_json::Value)>>,
}
impl Fixture {
    fn attributes(&self, row: &serde_json::Value, kind: &str) -> HashMap<String, Value> {
        let mut attributes = decode_attributes(row);
        if kind == "Thing"
            && let Some(private) = self.private.lock().unwrap().as_ref()
        {
            attributes.insert(
                "private".into(),
                Value::Deferred(carina_core::resource::DeferredValue::Secret(
                    Box::new(Value::Concrete(
                        carina_core::resource::ConcreteValue::String(private.clone().into()),
                    ))
                    .into(),
                )),
            );
        }
        attributes
    }
    fn record(&self, operation: &str, id: &ResourceId, attributes: serde_json::Value) {
        self.operations.lock().unwrap().push((
            operation.into(),
            id.resource_type.clone(),
            attributes,
        ));
    }
    fn count(&self, operation: &str, kind: &str) -> usize {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|(op, ty, _)| op == operation && ty == kind)
            .count()
    }
    fn sink_values(&self) -> Vec<serde_json::Value> {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|(operation, kind, _)| operation == "create" && kind == "Sink")
            .map(|(_, _, attributes)| attributes["value"].clone())
            .collect()
    }
}
struct FixtureProvider(Arc<Fixture>);
impl std::ops::Deref for FixtureProvider {
    type Target = Fixture;
    fn deref(&self) -> &Fixture {
        &self.0
    }
}
impl Provider for FixtureProvider {
    fn name(&self) -> &str {
        "test"
    }
    fn read_data_source(
        &self,
        source: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = source.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::not_found(id)) })
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        panic!("no pending creates")
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let mut json = request_json(request.resource().wire_attributes());
        self.record("create", id, json.clone());
        if id.resource_type == "Thing" {
            json["emitted"] = self.emitted.lock().unwrap().clone().into();
            json["items"] = serde_json::json!([self.emitted.lock().unwrap().clone()]);
        }
        self.rows
            .lock()
            .unwrap()
            .insert(id.to_string(), json.clone());
        let attributes = self.attributes(&json, &id.resource_type);
        let identifier =
            AppliedIdentifier::new(format!("fixture-{}", json["name"].as_str().unwrap())).unwrap();
        Box::pin(async move {
            Ok(attempt
                .issued(identifier, Sent::AllRequestAttributes)
                .succeeded(attributes))
        })
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let row = self.rows.lock().unwrap().get(&id.to_string()).cloned();
        self.record("read", id, row.clone().unwrap_or(serde_json::Value::Null));
        let outcome = match row {
            Some(row) => ReadOutcome::found(AppliedState::from_identifier(
                id.clone(),
                identifier.clone(),
                self.attributes(&row, &id.resource_type),
            )),
            None => ReadOutcome::not_found(id.clone()),
        };
        Box::pin(async move { Ok(outcome) })
    }
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        _: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        self.record("update", id, serde_json::Value::Null);
        let mut rows = self.rows.lock().unwrap();
        if !rows.contains_key(&id.to_string()) {
            let previous = rows
                .values()
                .find(|row| {
                    format!("fixture-{}", row["name"].as_str().unwrap()) == identifier.as_str()
                })
                .unwrap()
                .clone();
            rows.insert(id.to_string(), previous);
        }
        let row = rows
            .get_mut(&id.to_string())
            .unwrap()
            .as_object_mut()
            .unwrap();
        for operation in request.wire_patch().iter() {
            match operation {
                WirePatchOp::Add { key, value }
                | WirePatchOp::Replace { key, value }
                | WirePatchOp::Reassert { key, value } => {
                    row.insert(
                        key.to_string(),
                        serde_json::to_value(value.value()).unwrap(),
                    );
                }
                WirePatchOp::Remove { key } => {
                    row.remove(key.as_str());
                }
            }
        }
        if id.resource_type == "Thing" {
            row.insert(
                "emitted".into(),
                self.emitted.lock().unwrap().clone().into(),
            );
        }
        let state = AppliedState::from_identifier(
            id.clone(),
            identifier.clone(),
            decode_attributes(&serde_json::Value::Object(row.clone())),
        );
        Box::pin(async move { Ok(UpdateOutcome::success(state)) })
    }
    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        self.record("delete", attempt.id(), serde_json::Value::Null);
        self.rows.lock().unwrap().remove(&attempt.id().to_string());
        Box::pin(async { Ok(()) })
    }
}

#[derive(serde::Deserialize, Clone)]
struct Case {
    name: String,
    main: String,
    child: Option<String>,
    emitted: String,
}
fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!(
        "../tests/fixtures/fn_declared_checks/variants.json"
    ))
    .unwrap()
}

struct Directory {
    root: tempfile::TempDir,
    runtime: TestCommandRuntime,
    fixture: Arc<Fixture>,
}
impl Directory {
    fn new(case: &Case) -> Self {
        Self::in_directory(case, tempfile::tempdir().unwrap())
    }
    fn in_directory(case: &Case, root: tempfile::TempDir) -> Self {
        let fixture = Arc::new(Fixture {
            emitted: Mutex::new(case.emitted.clone()),
            private: Mutex::new(None),
            rows: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(Vec::new()),
        });
        let runtime = TestCommandRuntime::new();
        std::fs::write(root.path().join("main.crn"), &case.main).unwrap();
        std::fs::write(
            root.path().join("providers.crn"),
            "provider test { source = 'file://./native-fixture.wasm' }\n",
        )
        .unwrap();
        let state = root.path().join("state.json");
        std::fs::write(
            root.path().join("backend.crn"),
            format!(
                "backend local {{ path = {} }}\n",
                carina_core::parser::display_string_literal(state.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::write(
            root.path().join("carina-backend.lock"),
            serde_json::json!({"backend_type":"local", "attributes":{"path":state}}).to_string(),
        )
        .unwrap();
        if let Some(child) = &case.child {
            std::fs::create_dir(root.path().join("child")).unwrap();
            std::fs::write(root.path().join("child/main.crn"), child).unwrap();
        }
        Self {
            root,
            runtime,
            fixture,
        }
    }
    fn path(&self) -> &Path {
        self.root.path()
    }
    fn context(&self) -> ProviderContext {
        ProviderContext::new(self.runtime.environment().clone())
    }
    fn factories(&self) -> Vec<LoadedProviderFactory> {
        LoadedProviderFactory::for_test(vec![Box::new(FixtureFactory(self.fixture.clone()))])
    }
    fn validate(&self) -> commands::validate::ValidateReport {
        commands::validate::collect_with_factories(
            &ResolvedPath::new(self.path()).unwrap(),
            self.factories(),
            HashMap::new(),
        )
    }
    async fn plan(&self, out: Option<&Path>) -> Result<bool, AppError> {
        self.plan_output(out, false).await
    }
    async fn plan_output(&self, out: Option<&Path>, json: bool) -> Result<bool, AppError> {
        commands::plan::run_plan_using(
            &self.runtime,
            self.path(),
            out,
            None,
            None,
            crate::DetailLevel::None,
            false,
            false,
            json,
            false,
            false,
            &self.context(),
            |runtime, providers, _| {
                Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                    runtime,
                    self.factories(),
                    providers,
                ))
            },
        )
        .await
    }
    async fn apply(&self) -> Result<(), AppError> {
        commands::apply::run_apply_with_factories_for_test(
            &self.runtime,
            self.path(),
            self.factories(),
        )
        .await
    }
    async fn apply_saved(&self, path: &Path) -> Result<(), AppError> {
        commands::apply::run_apply_from_plan_using(
            commands::apply::SavedApplyInvocation::for_project(
                &self.runtime,
                &ResolvedPath::new(path).unwrap(),
                path,
                &self.context(),
                commands::apply::SavedApplyOptions {
                    auto_approve: true,
                    lock: true,
                    parallelism: crate::DEFAULT_PARALLELISM,
                    accept_legacy_name_overrides: false,
                    cancel: ShutdownToken::running(),
                },
                &commands::apply::cli_observer_factory,
            ),
            |runtime, providers, _| {
                Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                    runtime,
                    self.factories(),
                    providers,
                ))
            },
        )
        .await
    }
    async fn refresh(&self) -> Result<(), AppError> {
        commands::state::run_state_refresh_using(
            &self.runtime,
            self.path(),
            true,
            &self.context(),
            ShutdownToken::running(),
            |runtime, providers, _| {
                Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                    runtime,
                    self.factories(),
                    providers,
                ))
            },
        )
        .await
    }
    async fn destroy(&self) -> Result<(), AppError> {
        commands::destroy::run_destroy_using(
            commands::destroy::DestroyInvocation {
                runtime: &self.runtime,
                path: self.path(),
                auto_approve: true,
                lock: true,
                refresh: true,
                force: false,
                parallelism: crate::DEFAULT_PARALLELISM,
                provider_context: &self.context(),
                cancel: ShutdownToken::running(),
            },
            |runtime, providers, _| {
                Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                    runtime,
                    self.factories(),
                    providers,
                ))
            },
        )
        .await
    }
    async fn assert_unlocked(&self) {
        assert!(
            !self.path().join("state.lock").exists(),
            "command released backend lock"
        );
    }
}
fn rendered<T>(result: &Result<T, AppError>) -> String {
    match result {
        Ok(_) => "OK".into(),
        Err(AppError::ExecutionFailed(report)) => report
            .failures()
            .iter()
            .map(|failure| failure.kind().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format_error_with_source_chain(error),
    }
}
fn static_rejection(case: &Case) -> bool {
    case.name.contains("scalar-known")
        || (case.name.contains("-return-identity-")
            && !case.name.contains("-param-return-identity-"))
        || (case.name.starts_with("c2-module-param-"))
        || (case.name.ends_with("invalid") && case.name.contains("literal"))
}
fn known_handle_rejection(case: &Case) -> bool {
    case.name == "c1-handle-known-invalid"
}
fn late_rejection(case: &Case) -> bool {
    case.name.ends_with("invalid") && !static_rejection(case) && !known_handle_rejection(case)
}
fn savable(case: &Case) -> bool {
    !static_rejection(case) && case.name.contains("literal")
}
const SUFFIX: &str = "expected test.Name, got string 'bad-name': test rejected `test.Name`: Invalid Name value 'bad-name': does not match required pattern /allowed/ (Full)";
fn assert_declared_error(text: &str, prefix: &str) {
    let diagnostic = format!("{prefix} {SUFFIX}");
    assert!(
        text.contains(&diagnostic),
        "expected exact diagnostic {diagnostic:?}, got {text}"
    );
}
fn assert_exact_declared_diagnostic(
    text: &str,
    case: &Case,
    directory: &Directory,
    prefix: &str,
    body_line: Option<usize>,
) {
    let source = case.child.as_deref().unwrap_or(&case.main);
    let expected = if let Some(line) = body_line.filter(|_| !prefix.contains("return value")) {
        format!("Invalid expression at line {line}: {prefix} {SUFFIX}")
    } else if body_line.is_some() || prefix.contains("return value") {
        let line = body_line.unwrap_or_else(|| {
            source
                .lines()
                .position(|line| line.starts_with("fn "))
                .unwrap()
                + 1
        });
        let file = directory.path().join(if case.child.is_some() {
            "child/main.crn"
        } else {
            "main.crn"
        });
        format!(
            "{}: Invalid expression at line {line}: {prefix} {SUFFIX}",
            file.display()
        )
    } else {
        let line = source
            .lines()
            .position(|line| line.contains("value = "))
            .unwrap()
            + 1;
        format!("Invalid expression at line {line}: {prefix} {SUFFIX}")
    };
    assert_eq!(text, expected);
}

async fn commands_case(case: Case) {
    commands_case_with_body_site(case, None).await
}
async fn commands_case_with_body_site(case: Case, body_line: Option<usize>) {
    let validate = Directory::new(&case);
    let report = validate.validate();
    let validation = report.diagnostic_output_for_test().to_string();
    let plan = Directory::new(&case);
    let planned = plan.plan(None).await;
    let apply = Directory::new(&case);
    let applied = apply.apply().await;
    assert_eq!(
        report.exit_code() != 0,
        static_rejection(&case),
        "validate {}: {validation}",
        case.name
    );
    assert_eq!(
        planned.is_err(),
        static_rejection(&case) || known_handle_rejection(&case),
        "plan {}: {}",
        case.name,
        rendered(&planned)
    );
    let reject = static_rejection(&case) || known_handle_rejection(&case) || late_rejection(&case);
    assert_eq!(
        applied.is_err(),
        reject,
        "apply {}: {}",
        case.name,
        rendered(&applied)
    );
    assert_eq!(apply.fixture.count("create", "Sink"), usize::from(!reject));
    if static_rejection(&case) {
        let expected = expected_saved_plan_error(&case.name, &plan);
        assert_eq!(rendered(&planned), expected);
        assert_eq!(
            rendered(&applied),
            expected_saved_plan_error(&case.name, &apply)
        );
    }
    if known_handle_rejection(&case) {
        assert_declared_error(&rendered(&planned), "function 'f': return value:");
        assert_exact_declared_diagnostic(
            &rendered(&planned),
            &case,
            &plan,
            "function 'f': return value:",
            body_line,
        );
        assert_exact_declared_diagnostic(
            &rendered(&applied),
            &case,
            &apply,
            "function 'f': return value:",
            body_line,
        );
    }
    if late_rejection(&case) {
        let prefix = if case.name.contains("handle-late") {
            "function 'f': return value:"
        } else if case.name.contains("return-untyped") || case.name.contains("return-const") {
            "function 'g': return value:"
        } else {
            "function 'g': parameter 'x':"
        };
        assert_declared_error(&rendered(&applied), prefix);
        assert_exact_declared_diagnostic(&rendered(&applied), &case, &apply, prefix, body_line);
        assert_eq!(apply.fixture.count("create", "Thing"), 1);
        let state = std::fs::read_to_string(apply.path().join("state.json")).unwrap();
        assert!(
            state.contains("fixture-src"),
            "partial source progress: {state}"
        );
        assert!(!state.contains("fixture-sink"));
    }
    apply.assert_unlocked().await;
}
async fn plan_out_case(case: Case) {
    let dir = Directory::new(&case);
    let saved = dir.path().join("saved.plan.json");
    let result = dir.plan(Some(&saved)).await;
    assert_eq!(
        result.is_ok(),
        savable(&case),
        "{}: {}",
        case.name,
        rendered(&result)
    );
    assert_eq!(saved.exists(), savable(&case));
    if savable(&case) {
        dir.apply_saved(&saved).await.unwrap();
        assert_eq!(dir.fixture.count("create", "Sink"), 1);
    } else if static_rejection(&case) || known_handle_rejection(&case) {
        assert!(result.is_err());
        assert!(!saved.exists());
        assert_eq!(
            rendered(&result),
            expected_saved_plan_error(&case.name, &dir)
        );
    } else {
        assert!(result.is_err());
        assert!(!saved.exists());
        assert_eq!(
            rendered(&result),
            expected_retained_plan_refusal(
                dir.path(),
                if case.name.starts_with("c1-") {
                    "f"
                } else {
                    "g"
                }
            )
        );
    }
    if !saved.exists() {
        let missing = dir.apply_saved(&saved).await;
        assert!(missing.is_err(), "a refused plan has no artifact to apply");
        assert!(
            !rendered(&missing).contains(SUFFIX),
            "missing-file apply is not a type rejection"
        );
    }
    dir.assert_unlocked().await;
}

fn expected_saved_plan_error(case: &str, directory: &Directory) -> String {
    let snapshots: BTreeMap<String, String> = serde_json::from_str(include_str!(
        "../tests/fixtures/fn_declared_checks/plan_out_errors.json"
    ))
    .unwrap();
    snapshots[case].replace("{project}", directory.path().to_str().unwrap())
}

fn expected_retained_plan_refusal(project: &Path, function: &str) -> String {
    format!(
        "Cannot save plan: the plan contains an unevaluated call to the user-defined function '{function}'. A saved plan cannot carry this call.\nRun `{}` directly on the project, without a saved plan.",
        carina_core::hint::ProjectCommand::new("apply", project)
    )
}

mod saved_plans;

mod secret_state;
macro_rules! fixture_tests {
    ($name:ident, $plan_out:ident, $case:literal) => {
        #[tokio::test]
        async fn $name() {
            commands_case(cases().into_iter().find(|c| c.name == $case).unwrap()).await;
        }
        #[tokio::test]
        async fn $plan_out() {
            plan_out_case(cases().into_iter().find(|c| c.name == $case).unwrap()).await;
        }
    };
}
fixture_tests!(
    c1_handle_known_invalid,
    c1_handle_known_invalid_plan_out,
    "c1-handle-known-invalid"
);
fixture_tests!(
    c1_handle_known_valid,
    c1_handle_known_valid_plan_out,
    "c1-handle-known-valid"
);
fixture_tests!(
    c1_handle_late_invalid,
    c1_handle_late_invalid_plan_out,
    "c1-handle-late-invalid"
);
fixture_tests!(
    c1_handle_late_valid,
    c1_handle_late_valid_plan_out,
    "c1-handle-late-valid"
);
fixture_tests!(
    c1_scalar_known_invalid,
    c1_scalar_known_invalid_plan_out,
    "c1-scalar-known-invalid"
);
fixture_tests!(
    c1_literal_return_invalid,
    c1_literal_return_invalid_plan_out,
    "c1-literal-return-invalid"
);
fixture_tests!(
    c1_scalar_known_valid,
    c1_scalar_known_valid_plan_out,
    "c1-scalar-known-valid"
);
fixture_tests!(
    c1_literal_return_valid,
    c1_literal_return_valid_saved_plan_control,
    "c1-literal-return-valid"
);
fixture_tests!(
    c2_module_param_const_late_invalid,
    c2_module_param_const_late_invalid_plan_out,
    "c2-module-param-const-late-invalid"
);
fixture_tests!(
    c2_module_param_const_late_valid,
    c2_module_param_const_late_valid_plan_out,
    "c2-module-param-const-late-valid"
);
fixture_tests!(
    c2_module_param_identity_late_invalid,
    c2_module_param_identity_late_invalid_plan_out,
    "c2-module-param-identity-late-invalid"
);
fixture_tests!(
    c2_module_param_identity_late_valid,
    c2_module_param_identity_late_valid_plan_out,
    "c2-module-param-identity-late-valid"
);
fixture_tests!(
    c2_module_param_return_identity_late_invalid,
    c2_module_param_return_identity_late_invalid_plan_out,
    "c2-module-param-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_param_return_identity_late_valid,
    c2_module_param_return_identity_late_valid_plan_out,
    "c2-module-param-return-identity-late-valid"
);
fixture_tests!(
    c2_module_return_identity_late_invalid,
    c2_module_return_identity_late_invalid_plan_out,
    "c2-module-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_return_identity_late_valid,
    c2_module_return_identity_late_valid_plan_out,
    "c2-module-return-identity-late-valid"
);
fixture_tests!(
    c2_module_return_untyped_late_invalid,
    c2_module_return_untyped_late_invalid_plan_out,
    "c2-module-return-untyped-late-invalid"
);
fixture_tests!(
    c2_module_return_untyped_late_valid,
    c2_module_return_untyped_late_valid_plan_out,
    "c2-module-return-untyped-late-valid"
);
fixture_tests!(
    c2_module_return_const_late_invalid,
    c2_module_return_const_late_invalid_plan_out,
    "c2-module-return-const-late-invalid"
);
fixture_tests!(
    c2_module_return_const_late_valid,
    c2_module_return_const_late_valid_plan_out,
    "c2-module-return-const-late-valid"
);
fixture_tests!(
    c2_module_upstream_param_const_late_invalid,
    c2_module_upstream_param_const_late_invalid_plan_out,
    "c2-module-upstream-param-const-late-invalid"
);
fixture_tests!(
    c2_module_upstream_param_const_late_valid,
    c2_module_upstream_param_const_late_valid_plan_out,
    "c2-module-upstream-param-const-late-valid"
);
fixture_tests!(
    c2_module_upstream_param_identity_late_invalid,
    c2_module_upstream_param_identity_late_invalid_plan_out,
    "c2-module-upstream-param-identity-late-invalid"
);
fixture_tests!(
    c2_module_upstream_param_identity_late_valid,
    c2_module_upstream_param_identity_late_valid_plan_out,
    "c2-module-upstream-param-identity-late-valid"
);
fixture_tests!(
    c2_module_upstream_param_return_identity_late_invalid,
    c2_module_upstream_param_return_identity_late_invalid_plan_out,
    "c2-module-upstream-param-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_upstream_param_return_identity_late_valid,
    c2_module_upstream_param_return_identity_late_valid_plan_out,
    "c2-module-upstream-param-return-identity-late-valid"
);
fixture_tests!(
    c2_module_upstream_return_identity_late_invalid,
    c2_module_upstream_return_identity_late_invalid_plan_out,
    "c2-module-upstream-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_upstream_return_identity_late_valid,
    c2_module_upstream_return_identity_late_valid_plan_out,
    "c2-module-upstream-return-identity-late-valid"
);
fixture_tests!(
    c2_module_upstream_return_untyped_late_invalid,
    c2_module_upstream_return_untyped_late_invalid_plan_out,
    "c2-module-upstream-return-untyped-late-invalid"
);
fixture_tests!(
    c2_module_upstream_return_untyped_late_valid,
    c2_module_upstream_return_untyped_late_valid_plan_out,
    "c2-module-upstream-return-untyped-late-valid"
);
fixture_tests!(
    c2_module_upstream_return_const_late_invalid,
    c2_module_upstream_return_const_late_invalid_plan_out,
    "c2-module-upstream-return-const-late-invalid"
);
fixture_tests!(
    c2_module_upstream_return_const_late_valid,
    c2_module_upstream_return_const_late_valid_plan_out,
    "c2-module-upstream-return-const-late-valid"
);
fixture_tests!(
    c2_module_handle_param_const_late_invalid,
    c2_module_handle_param_const_late_invalid_plan_out,
    "c2-module-handle-param-const-late-invalid"
);
fixture_tests!(
    c2_module_handle_param_const_late_valid,
    c2_module_handle_param_const_late_valid_plan_out,
    "c2-module-handle-param-const-late-valid"
);
fixture_tests!(
    c2_module_handle_param_identity_late_invalid,
    c2_module_handle_param_identity_late_invalid_plan_out,
    "c2-module-handle-param-identity-late-invalid"
);
fixture_tests!(
    c2_module_handle_param_identity_late_valid,
    c2_module_handle_param_identity_late_valid_plan_out,
    "c2-module-handle-param-identity-late-valid"
);
fixture_tests!(
    c2_module_handle_param_return_identity_late_invalid,
    c2_module_handle_param_return_identity_late_invalid_plan_out,
    "c2-module-handle-param-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_handle_param_return_identity_late_valid,
    c2_module_handle_param_return_identity_late_valid_plan_out,
    "c2-module-handle-param-return-identity-late-valid"
);
fixture_tests!(
    c2_module_handle_return_identity_late_invalid,
    c2_module_handle_return_identity_late_invalid_plan_out,
    "c2-module-handle-return-identity-late-invalid"
);
fixture_tests!(
    c2_module_handle_return_identity_late_valid,
    c2_module_handle_return_identity_late_valid_plan_out,
    "c2-module-handle-return-identity-late-valid"
);
fixture_tests!(
    c2_module_handle_return_untyped_late_invalid,
    c2_module_handle_return_untyped_late_invalid_plan_out,
    "c2-module-handle-return-untyped-late-invalid"
);
fixture_tests!(
    c2_module_handle_return_untyped_late_valid,
    c2_module_handle_return_untyped_late_valid_plan_out,
    "c2-module-handle-return-untyped-late-valid"
);
fixture_tests!(
    c2_module_handle_return_const_late_invalid,
    c2_module_handle_return_const_late_invalid_plan_out,
    "c2-module-handle-return-const-late-invalid"
);
fixture_tests!(
    c2_module_handle_return_const_late_valid,
    c2_module_handle_return_const_late_valid_plan_out,
    "c2-module-handle-return-const-late-valid"
);
fixture_tests!(
    c2_root_param_const_late_invalid,
    c2_root_param_const_late_invalid_plan_out,
    "c2-root-param-const-late-invalid"
);
fixture_tests!(
    c2_root_param_const_late_valid,
    c2_root_param_const_late_valid_plan_out,
    "c2-root-param-const-late-valid"
);
fixture_tests!(
    c2_root_param_identity_late_invalid,
    c2_root_param_identity_late_invalid_plan_out,
    "c2-root-param-identity-late-invalid"
);
fixture_tests!(
    c2_root_param_identity_late_valid,
    c2_root_param_identity_late_valid_plan_out,
    "c2-root-param-identity-late-valid"
);
fixture_tests!(
    c2_root_param_return_identity_late_invalid,
    c2_root_param_return_identity_late_invalid_plan_out,
    "c2-root-param-return-identity-late-invalid"
);
fixture_tests!(
    c2_root_param_return_identity_late_valid,
    c2_root_param_return_identity_late_valid_plan_out,
    "c2-root-param-return-identity-late-valid"
);
fixture_tests!(
    c2_root_return_identity_late_invalid,
    c2_root_return_identity_late_invalid_plan_out,
    "c2-root-return-identity-late-invalid"
);
fixture_tests!(
    c2_root_return_identity_late_valid,
    c2_root_return_identity_late_valid_plan_out,
    "c2-root-return-identity-late-valid"
);
fixture_tests!(
    c2_root_return_untyped_late_invalid,
    c2_root_return_untyped_late_invalid_plan_out,
    "c2-root-return-untyped-late-invalid"
);
fixture_tests!(
    c2_root_return_untyped_late_valid,
    c2_root_return_untyped_late_valid_plan_out,
    "c2-root-return-untyped-late-valid"
);
fixture_tests!(
    c2_root_return_const_late_invalid,
    c2_root_return_const_late_invalid_plan_out,
    "c2-root-return-const-late-invalid"
);
fixture_tests!(
    c2_root_return_const_late_valid,
    c2_root_return_const_late_valid_plan_out,
    "c2-root-return-const-late-valid"
);
fixture_tests!(
    c2_module_param_const_literal_invalid,
    c2_module_param_const_literal_invalid_plan_out,
    "c2-module-param-const-literal-invalid"
);
fixture_tests!(
    c2_module_param_const_literal_valid,
    c2_module_param_const_literal_valid_plan_out,
    "c2-module-param-const-literal-valid"
);
fixture_tests!(
    c2_module_return_identity_literal_invalid,
    c2_module_return_identity_literal_invalid_plan_out,
    "c2-module-return-identity-literal-invalid"
);
fixture_tests!(
    c2_module_return_identity_literal_valid,
    c2_module_return_identity_literal_valid_plan_out,
    "c2-module-return-identity-literal-valid"
);
fixture_tests!(
    c2_root_param_const_literal_invalid,
    c2_root_param_const_literal_invalid_plan_out,
    "c2-root-param-const-literal-invalid"
);
fixture_tests!(
    c2_root_param_const_literal_valid,
    c2_root_param_const_literal_valid_saved_plan_control,
    "c2-root-param-const-literal-valid"
);
fixture_tests!(
    c2_root_return_identity_literal_invalid,
    c2_root_return_identity_literal_invalid_plan_out,
    "c2-root-return-identity-literal-invalid"
);
fixture_tests!(
    c2_root_return_identity_literal_valid,
    c2_root_return_identity_literal_valid_plan_out,
    "c2-root-return-identity-literal-valid"
);

fn fixture_case(name: &str) -> Case {
    cases().into_iter().find(|case| case.name == name).unwrap()
}

#[tokio::test]
async fn declared_checks_nested_arguments_locals_wrappers_in_module_and_root() {
    for scope in ["root", "module-upstream", "module-handle"] {
        for (wrapper, body_line) in [
            ("fn outer(x) { g(x) }", Some(1)),
            ("fn outer(x) { let unused = g(x)\n 'const' }", Some(1)),
            ("fn outer(x) { 'const' }", None),
        ] {
            let mut case = fixture_case(&format!("c2-{scope}-param-identity-late-invalid"));
            let source = case.child.as_mut().unwrap_or(&mut case.main);
            source.insert_str(0, &format!("{wrapper}\n"));
            let call = if scope == "module-handle" {
                "g(source.emitted)"
            } else {
                "g(src.emitted)"
            };
            let replacement = if wrapper == "fn outer(x) { 'const' }" {
                format!("outer({call})")
            } else {
                call.replacen("g(", "outer(", 1)
            };
            *source = source.replace(call, &replacement);
            commands_case_with_body_site(case, body_line).await;
        }
    }
}

#[tokio::test]
async fn declared_checks_untyped_builtin_controls_inline_plan_out() {
    for scope in ["root", "module-upstream"] {
        for (function, call) in [
            ("fn g(x) { upper(x) }", "g(src.emitted)"),
            ("fn g(x) { x }", "g(upper(src.emitted))"),
        ] {
            let mut case = fixture_case(&format!("c2-{scope}-param-identity-late-invalid"));
            let source = case.child.as_mut().unwrap_or(&mut case.main);
            *source = source
                .replace("fn g(x: test.Name): String { x }", function)
                .replace("g(src.emitted)", call);
            let directory = Directory::new(&case);
            let saved = directory.path().join("saved.plan.json");
            directory.plan(Some(&saved)).await.unwrap();
            directory.apply_saved(&saved).await.unwrap();
            assert_eq!(directory.fixture.count("create", "Sink"), 1);
            assert_eq!(
                directory.fixture.sink_values(),
                [serde_json::json!("BAD-NAME")]
            );
            directory.assert_unlocked().await;
        }
    }
}

#[tokio::test]
async fn declared_checks_primitive_annotations_wait_for_values_plan_out() {
    for scope in ["root", "module-upstream"] {
        for (function, call) in [
            ("fn g(x: String): String { x }", "g(src.emitted)"),
            (
                "fn g(x: list(String)): list(String) { x }",
                "g([src.emitted])[0]",
            ),
        ] {
            let mut case = fixture_case(&format!("c2-{scope}-param-identity-late-invalid"));
            let source = case.child.as_mut().unwrap_or(&mut case.main);
            *source = source
                .replace("fn g(x: test.Name): String { x }", function)
                .replace("g(src.emitted)", call);
            let directory = Directory::new(&case);
            let saved = directory.path().join("saved.plan.json");
            let result = directory.plan(Some(&saved)).await;
            assert_eq!(
                rendered(&result),
                expected_retained_plan_refusal(directory.path(), "g")
            );
            assert!(!saved.exists());
            directory.apply().await.unwrap();
            assert_eq!(directory.fixture.count("create", "Sink"), 1);
            assert_eq!(
                directory.fixture.sink_values(),
                [serde_json::json!("bad-name")]
            );
            directory.assert_unlocked().await;
        }
    }
}

#[tokio::test]
async fn declared_checks_authored_sibling_and_state_values() {
    for validity in ["invalid", "valid"] {
        let mut case = fixture_case(&format!(
            "c2-module-upstream-param-identity-late-{validity}"
        ));
        let child = case.child.as_mut().unwrap();
        *child = child
            .replace(
                "name = 'src'",
                &format!("name = 'src' plain = '{}'", case.emitted),
            )
            .replace("g(src.emitted)", "g(src.plain)");
        let directory = Directory::new(&case);
        let planned = directory.plan(None).await;
        if validity == "invalid" {
            assert_declared_error(&rendered(&planned), "function 'g': parameter 'x':");
            assert_eq!(directory.fixture.count("create", "Sink"), 0);
        } else {
            planned.unwrap();
            directory.apply().await.unwrap();
            assert!(
                !directory.plan(None).await.unwrap(),
                "valid state produces no-op plan"
            );
        }
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn declared_checks_authored_and_state_valid_plan_out() {
    let mut case = fixture_case("c2-module-upstream-param-identity-late-valid");
    let child = case.child.as_mut().unwrap();
    *child = child
        .replace("name = 'src'", "name = 'src' plain = 'allowed'")
        .replace("g(src.emitted)", "g(src.plain)");
    case.child = case.child.map(|source| {
        source
            .replace(
                "fn g(x: test.Name): String { x }",
                "fn g(r: test.Thing): test.Name { r.plain }",
            )
            .replace("g(src.plain)", "g(src)")
    });
    let directory = Directory::new(&case);
    std::fs::write(
        directory.path().join("child/source.crn"),
        "let src = test.Thing { name = 'src' plain = 'allowed' }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        case.child.as_ref().unwrap().replace(
            "let src = test.Thing { name = 'src' plain = 'allowed' }",
            "",
        ),
    )
    .unwrap();
    for populated in [false, true] {
        if populated {
            directory.apply().await.unwrap();
        }
        let saved = directory.path().join("saved.plan.json");
        let result = directory.plan(Some(&saved)).await;
        assert!(result.is_err());
        assert_eq!(
            rendered(&result),
            expected_retained_plan_refusal(directory.path(), "g")
        );
        assert!(!saved.exists());
    }
}

#[tokio::test]
async fn declared_checks_existing_sink_update_rejects_after_source_update() {
    let mut case = fixture_case("c1-handle-late-valid");
    case.main = case
        .main
        .replace("test.Sink", "let sink = test.Sink")
        .replace("name = 'sink'", "name = 'sink' marker = 'before'");
    let directory = Directory::new(&case);
    directory.apply().await.unwrap();
    let original = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
    *directory.fixture.emitted.lock().unwrap() = "bad-name".into();
    std::fs::write(
        directory.path().join("main.crn"),
        case.main
            .replace("name = 'src'", "name = 'src' plain = 'changed'")
            .replace("marker = 'before'", "marker = 'after'"),
    )
    .unwrap();
    directory.fixture.operations.lock().unwrap().clear();
    let result = directory.apply().await;
    assert_declared_error(&rendered(&result), "function 'f': return value:");
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    assert_eq!(directory.fixture.count("update", "Sink"), 0);
    assert_eq!(directory.fixture.count("update", "Thing"), 1);
    let state = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
    let sink_row = |text: &str| {
        let state: serde_json::Value = serde_json::from_str(text).unwrap();
        state["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["resource_type"] == "Sink")
            .unwrap()["current"]
            .clone()
    };
    assert_eq!(
        sink_row(&state),
        sink_row(&original),
        "previous Sink state must survive unchanged"
    );
    assert!(
        state.contains("bad-name"),
        "source progress survives: {state}"
    );
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_sensitive_handle_diagnostic_is_masked() {
    let mut case = fixture_case("c1-handle-known-invalid");
    case.main = case
        .main
        .replace("plain = 'bad-name'", "plain = secret('bad-name')")
        .replace("value = f(src)", "value = secret(f(src))");
    let directory = Directory::new(&case);
    let result = directory.plan(None).await;
    let error = rendered(&result);
    assert!(
        error.contains(carina_core::resource::INSIDE_SECRET_MESSAGE),
        "{error}"
    );
    assert!(!error.contains("bad-name"), "{error}");
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
}

#[tokio::test]
async fn declared_checks_provider_loaded_lsp_directory_matches_validate() {
    use carina_lsp::{document::Document, test_support::Diagnostics};
    for name in [
        "c1-handle-known-invalid",
        "c2-module-upstream-param-const-late-invalid",
    ] {
        let directory = Directory::new(&fixture_case(name));
        let report = directory.validate();
        assert_eq!(
            report.exit_code(),
            0,
            "{}",
            report.diagnostic_output_for_test()
        );
        let factories: Arc<Vec<Box<dyn ProviderFactory>>> =
            Arc::new(vec![Box::new(FixtureFactory(directory.fixture.clone()))]);
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            factories,
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = Diagnostics::new(directory.path(), Arc::new(loaded), vec!["test".into()]);
        let file = directory.path().join("main.crn");
        let document = Document::new_with_path(
            std::fs::read_to_string(&file).unwrap(),
            file.clone(),
            Arc::new(directory.context()),
        );
        let errors: Vec<_> = engine
            .analyze(&document, Some(&file))
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            })
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
    }
}

#[tokio::test]
async fn declared_checks_destroy_and_refresh_use_construction_inputs() {
    let directory = Directory::new(&fixture_case(
        "c2-module-upstream-param-identity-late-valid",
    ));
    directory.apply().await.unwrap();
    directory.refresh().await.unwrap();
    assert!(directory.fixture.count("read", "Thing") > 0);
    directory.destroy().await.unwrap();
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_defaults_nested_and_multiple_module_instances() {
    let mut case = fixture_case("c2-module-handle-param-const-late-invalid");
    case.main
        .push_str("let second = child { source = src }\nchild { source = src }\n");
    case.child = Some("arguments { source: test.Thing }\nfn g(x: test.Name, extra: String = 'default'): String { extra }\nlet nested = use { source = './nested' }\nlet n = nested { source = source }\n".into());
    let directory = Directory::new(&case);
    std::fs::create_dir(directory.path().join("child/nested")).unwrap();
    std::fs::write(directory.path().join("child/nested/main.crn"), "arguments { source: test.Thing }\nfn g(x: test.Name, extra: String = 'default'): String { extra }\ntest.Sink { name = 'sink' value = g(source.emitted) }\n").unwrap();
    directory.plan(None).await.unwrap();
    let result = directory.apply().await;
    assert_declared_error(&rendered(&result), "function 'g': parameter 'x':");
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    assert_eq!(directory.fixture.count("create", "Thing"), 1);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_immediate_deferred_loops_and_selected_conditionals() {
    for body in [
        "for x in [src.emitted] { test.Sink { name = 'sink' value = g(x) } }",
        "for x in src.items { test.Sink { name = 'sink' value = g(x) } }",
        "if true { test.Sink { name = 'sink' value = g(src.emitted) } }",
    ] {
        let mut case = fixture_case("c2-module-upstream-param-const-late-invalid");
        case.child = Some(format!(
            "arguments {{ label: String }}\nlet src = test.Thing {{ name = 'src' }}\nfn g(x: test.Name): String {{ 'const' }}\n{body}\n"
        ));
        let directory = Directory::new(&case);
        directory.plan(None).await.unwrap();
        let result = directory.apply().await;
        assert_declared_error(&rendered(&result), "function 'g': parameter 'x':");
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
        assert_eq!(directory.fixture.count("create", "Thing"), 1);
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn declared_checks_invalid_declaration_fails_before_dispatch() {
    let mut case = fixture_case("c1-handle-late-invalid");
    case.main = case.main.replace(": test.Name", ": test.MissingName");
    let directory = Directory::new(&case);
    let result = directory.plan(None).await;
    assert!(result.is_err());
    assert!(
        rendered(&result).contains("test.MissingName"),
        "{}",
        rendered(&result)
    );
    assert_eq!(directory.fixture.count("create", "Thing"), 0);
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_invalid_value_already_in_state_rejects_at_plan() {
    let case = fixture_case("c2-module-upstream-param-identity-late-invalid");
    let directory = Directory::new(&case);
    let child_path = directory.path().join("child/main.crn");
    std::fs::write(
        &child_path,
        "arguments { label: String }\nlet src = test.Thing { name = 'src' }\n",
    )
    .unwrap();
    directory.apply().await.unwrap();
    directory.fixture.operations.lock().unwrap().clear();
    std::fs::write(&child_path, case.child.as_ref().unwrap()).unwrap();
    let planned = directory.plan(None).await;
    assert_declared_error(&rendered(&planned), "function 'g': parameter 'x':");
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_scalar_authored_and_state_saved_plan_control() {
    let mut case = fixture_case("c2-module-upstream-param-identity-late-valid");
    let child = case.child.as_mut().unwrap();
    *child = child
        .replace("name = 'src'", "name = 'src' plain = 'allowed'")
        .replace("g(src.emitted)", "g(src.plain)");
    let directory = Directory::new(&case);
    for populated in [false, true] {
        let saved = directory.path().join(if populated {
            "noop.plan.json"
        } else {
            "create.plan.json"
        });
        directory.plan(Some(&saved)).await.unwrap();
        directory.apply_saved(&saved).await.unwrap();
    }
    assert_eq!(directory.fixture.count("create", "Sink"), 1);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_invalid_value_already_in_state_plan_out() {
    let case = fixture_case("c2-module-upstream-param-identity-late-invalid");
    let directory = Directory::new(&case);
    let child_path = directory.path().join("child/main.crn");
    std::fs::write(
        &child_path,
        "arguments { label: String }\nlet src = test.Thing { name = 'src' }\n",
    )
    .unwrap();
    directory.apply().await.unwrap();
    directory.fixture.operations.lock().unwrap().clear();
    std::fs::write(&child_path, case.child.as_ref().unwrap()).unwrap();
    let saved = directory.path().join("saved.plan.json");
    let planned = directory.plan(Some(&saved)).await;
    assert!(planned.is_err());
    assert_declared_error(&rendered(&planned), "function 'g': parameter 'x':");
    assert_exact_declared_diagnostic(
        &rendered(&planned),
        &case,
        &directory,
        "function 'g': parameter 'x':",
        None,
    );
    assert!(!saved.exists());
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    assert_eq!(directory.fixture.count("update", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn declared_checks_authored_invalid_control_plan_out() {
    let mut case = fixture_case("c2-module-upstream-param-identity-late-invalid");
    let child = case.child.as_mut().unwrap();
    *child = child
        .replace("name = 'src'", "name = 'src' plain = 'bad-name'")
        .replace("g(src.emitted)", "g(src.plain)");
    let directory = Directory::new(&case);
    let saved = directory.path().join("saved.plan.json");
    let planned = directory.plan(Some(&saved)).await;
    assert!(planned.is_err());
    assert_declared_error(&rendered(&planned), "function 'g': parameter 'x':");
    assert_eq!(
        rendered(&planned),
        expected_saved_plan_error("authored-invalid-control", &directory)
    );
    assert!(!saved.exists());
    assert_eq!(directory.fixture.count("create", "Thing"), 0);
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

mod runtime_checks;

mod saved_document;

mod directory_calls;

mod secret_updates;
