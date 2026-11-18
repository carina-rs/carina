use carina_cli::commands::apply::run_apply_with_factories_for_test;
use carina_cli::error::AppError;
use carina_cli::test_support::{TestCommandRuntime, run_observed_plan_with_factories};
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::identifier::AnonymousIdentifierReconciliationError;
use carina_core::provider::*;
use carina_core::resource::{ResourceId, State, Value};
use carina_core::schema::*;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Store {
    rows: BTreeMap<String, Json>,
    operations: Vec<&'static str>,
}

#[derive(Clone)]
struct Factory(Arc<Mutex<Store>>);

impl ProviderFactory for Factory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Function identity migration fixture"
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
        let provider = FixtureProvider(self.0.clone());
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("Thing")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("emitted", AttributeType::string()).build(),
                ),
            ResourceSchema::new("IdentitySink")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::required("value", AttributeType::string())
                        .create_only()
                        .identity()
                        .build(),
                ),
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(AttributeSchema::required("value", AttributeType::string()).build()),
        ]
        .into_iter()
        .map(|schema| schema.validate().unwrap())
        .collect()
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

fn attributes(row: &Json) -> HashMap<String, Value> {
    row.as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                carina_core::value::json_to_dsl_value(value)
                    .unwrap()
                    .unwrap(),
            )
        })
        .collect()
}

struct FixtureProvider(Arc<Mutex<Store>>);
impl Provider for FixtureProvider {
    fn name(&self) -> &str {
        "test"
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn read_data_source(
        &self,
        source: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = source.id.clone();
        Box::pin(async move { Ok(State::not_found(id)) })
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        panic!("fixture has no pending creates")
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let mut row = Json::Object(
            request
                .resource()
                .wire_attributes()
                .transport_iter()
                .map(|(key, value)| (key.to_owned(), serde_json::to_value(value.value()).unwrap()))
                .collect(),
        );
        if id.resource_type == "Thing" {
            row["emitted"] = "allowed".into();
        }
        let mut store = self.0.lock().unwrap();
        let identifier = format!("fixture-{}-{}", id.resource_type, store.rows.len() + 1);
        store.rows.insert(identifier.clone(), row.clone());
        store.operations.push("create");
        let identifier = AppliedIdentifier::new(identifier).unwrap();
        Box::pin(async move {
            Ok(attempt
                .issued(identifier, Sent::AllRequestAttributes)
                .succeeded(attributes(&row)))
        })
    }
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let mut store = self.0.lock().unwrap();
        store.operations.push("read");
        let outcome = match store.rows.get(identifier.as_str()) {
            Some(row) => ReadOutcome::found(AppliedState::from_identifier(
                id.clone(),
                identifier.clone(),
                attributes(row),
            )),
            None => ReadOutcome::not_found(id.clone()),
        };
        Box::pin(async move { Ok(outcome) })
    }
    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
        _: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        self.0.lock().unwrap().operations.push("update");
        panic!("moving a state row must not update the provider")
    }
    fn delete(&self, _: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        self.0.lock().unwrap().operations.push("delete");
        panic!("moving a state row must not delete the provider resource")
    }
}

pub(super) async fn check_legacy_rows() {
    for (kind, old, new) in [
        (
            "IdentitySink",
            "instance.test_identity_sink_0312f2cf",
            "instance.test_identity_sink_04c023e3",
        ),
        (
            "Sink",
            "instance.test_sink_22c482a506328080",
            "instance.test_sink_2190428c0824b060",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let state_path = path.join("state.json");
        let child = path.join("child/main.crn");
        std::fs::create_dir(path.join("child")).unwrap();
        std::fs::write(
            path.join("providers.crn"),
            "provider test { source = 'file://./native-fixture.wasm' }\n",
        )
        .unwrap();
        std::fs::write(
            path.join("backend.crn"),
            format!(
                "backend local {{ path = {} }}\n",
                carina_core::parser::display_string_literal(state_path.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::write(
            path.join("carina-backend.lock"),
            json!({"backend_type": "local", "attributes": {"path": state_path}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            path.join("main.crn"),
            "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
        )
        .unwrap();
        let body = |value: &str| {
            format!(
                "arguments {{ label: String }}\nlet src = test.Thing {{ name = 'src' }}\nfn p(x: String): String {{ \"p-${{x}}\" }}\ntest.{kind} {{ name = 'sink' value = {value} }}\n"
            )
        };
        // Main substitutes p's body before hashing a module's anonymous
        // resource. Applying that expanded source creates the legacy row
        // through the real command; no state IDs or snapshots are fabricated.
        std::fs::write(&child, body("\"p-${lower(src.emitted)}\"")).unwrap();
        let runtime = TestCommandRuntime::new();
        let store = Arc::new(Mutex::new(Store::default()));
        let factories = || LoadedProviderFactory::for_test(vec![Box::new(Factory(store.clone()))]);
        run_apply_with_factories_for_test(&runtime, path, factories())
            .await
            .unwrap();
        let before = std::fs::read(&state_path).unwrap();
        let state: Json = serde_json::from_slice(&before).unwrap();
        let rows = state["resources"].as_array().unwrap();
        let row = rows
            .iter()
            .find(|row| row["resource_type"] == kind)
            .unwrap();
        assert_eq!(row["identity"], old);
        assert_eq!(row["current"]["attributes"]["value"], "p-allowed");
        let identifier = row["current"]["identifier"].clone();
        let provider_rows = store.lock().unwrap().rows.clone();
        store.lock().unwrap().operations.clear();
        std::fs::write(&child, body("p(lower(src.emitted))")).unwrap();

        if kind == "IdentitySink" {
            let error =
                run_observed_plan_with_factories(&runtime, path, None, true, false, factories())
                    .await
                    .unwrap_err();
            let AppError::AnonymousIdentifierReconciliation(error) = error else {
                panic!("expected anonymous-identity reconciliation error")
            };
            let AnonymousIdentifierReconciliationError::UnresolvedDeclaredIdentity(error) = *error
            else {
                panic!("expected UnresolvedDeclaredIdentityMigration")
            };
            assert_eq!(error.resource().to_string(), format!("test.{kind}.{new}"));
            assert_eq!(error.candidate_rows().len(), 1);
            assert_eq!(
                error.candidate_rows()[0].to_string(),
                format!("test.{kind}.{old}")
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "Anonymous resource `test.{kind}.{new}` has an authored declared-identity expression that cannot be evaluated from binding state while unclaimed legacy state rows remain: `test.{kind}.{old}`. Add a `moved` block to select the intended state row."
                )
            );
            assert!(store.lock().unwrap().operations.is_empty());
            assert_eq!(std::fs::read(&state_path).unwrap(), before);
        } else {
            println!("MUTABLE_MIGRATION_PLAN_BEGIN");
            assert!(
                run_observed_plan_with_factories(&runtime, path, None, true, false, factories(),)
                    .await
                    .unwrap()
            );
            println!("MUTABLE_MIGRATION_PLAN_END");
        }
        assert!(
            store
                .lock()
                .unwrap()
                .operations
                .iter()
                .all(|op| *op == "read")
        );

        std::fs::write(
            path.join("moved.crn"),
            format!("moved {{\n  from = test.{kind} '{old}'\n  to = test.{kind} '{new}'\n}}\n"),
        )
        .unwrap();
        run_observed_plan_with_factories(&runtime, path, None, true, false, factories())
            .await
            .unwrap();
        run_apply_with_factories_for_test(&runtime, path, factories())
            .await
            .unwrap();
        let state: Json = serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        let rows = state["resources"].as_array().unwrap();
        assert!(!rows.iter().any(|row| row["identity"] == old));
        let row = rows
            .iter()
            .find(|row| row["resource_type"] == kind)
            .unwrap();
        assert_eq!(row["identity"], new);
        assert_eq!(row["current"]["identifier"], identifier);
        let store = store.lock().unwrap();
        assert_eq!(store.rows, provider_rows);
        assert!(store.operations.iter().all(|op| *op == "read"));
    }
}
