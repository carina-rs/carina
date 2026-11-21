//! Public desired values use the same final preparation for refresh and plan.
use super::*;
use carina_core::provider::{
    CanonicalProviderConfigAttributes, InitializeOutcome, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderSession, ProviderValidationInputs,
};
use carina_core::schema::{ValidatedProviderSchemaWitness, ValidatedResourceSchema};
use carina_core::value::PersistedValue;

#[derive(Clone)]
struct MeasurementFactory {
    provider: Arc<DeposedRefreshTestProvider>,
    schema: ValidatedResourceSchema,
    lowercase: bool,
    fail_desired: Arc<std::sync::atomic::AtomicBool>,
    pause_desired: Option<Arc<tokio::sync::Notify>>,
}

impl ProviderFactory for MeasurementFactory {
    fn name(&self) -> &str {
        "round6"
    }
    fn display_name(&self) -> &str {
        "Round 6 in-process measurement"
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
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
        vec![self.schema.clone()]
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        let normalizer = MeasurementNormalizer {
            provider: Arc::clone(&self.provider),
            lowercase: self.lowercase,
            fail_desired: Arc::clone(&self.fail_desired),
            pause_desired: self.pause_desired.clone(),
        };
        Box::pin(async move { Ok(Box::new(normalizer) as Box<dyn ProviderNormalizer>) })
    }
}

struct MeasurementNormalizer {
    provider: Arc<DeposedRefreshTestProvider>,
    lowercase: bool,
    fail_desired: Arc<std::sync::atomic::AtomicBool>,
    pause_desired: Option<Arc<tokio::sync::Notify>>,
}
impl ProviderNormalizer for MeasurementNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [carina_core::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            if let Some(started) = &self.pause_desired {
                started.notify_one();
                std::future::pending::<()>().await;
            }
            if self
                .fail_desired
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                return Err(ProviderError::internal(
                    "fixture desired preparation unavailable",
                ));
            }
            self.provider.normalize_desired(resources).await?;
            if self.lowercase {
                for resource in resources {
                    if let Some(Value::Concrete(ConcreteValue::String(raw))) =
                        resource.attributes.get_mut("computed")
                    {
                        *raw = raw.expose_plaintext().to_ascii_lowercase().into();
                    }
                }
            }
            Ok(())
        })
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut carina_core::provider::StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        self.provider.normalize_state(states)
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut carina_core::provider::StateTransformMap,
        saved: &'a carina_core::provider::SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        self.provider.hydrate_read_state(states, saved)
    }
}

impl MeasurementFactory {
    fn loaded(&self) -> Vec<crate::wiring::LoadedProviderFactory> {
        crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(self.clone())])
    }
}

// Run the fixture command sequence in a child harness so stdout, stderr and
// JSON output can be checked without redirecting process-global test streams.
fn retained_secret_child(test: &str) -> bool {
    if std::env::var("CARINA_RETAINED_SECRET_CASE").as_deref() == Ok(test) {
        return true;
    }
    let nonce = tempfile::tempdir().unwrap();
    let marker = format!(
        "round6-old-{}",
        nonce.path().file_name().unwrap().to_string_lossy()
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("commands::state::tests::retained_secret_measurements::{test}"),
            "--nocapture",
        ])
        .env("CARINA_RETAINED_SECRET_CASE", test)
        .env("CARINA_RETAINED_SECRET_MARKER", &marker)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let safe = format!("stdout:\n{stdout}\nstderr:\n{stderr}").replace(&marker, "<MASKED>");
    assert!(output.status.success(), "command fixture failed:\n{safe}");
    assert!(
        stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
        "child harness must execute exactly one regression:\n{safe}"
    );
    assert!(
        !stdout.contains(&marker) && !stderr.contains(&marker),
        "command fixture exposed old material:\n{safe}"
    );
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicDesiredCase {
    EnumIdentifier,
    EnumString,
    Lowercase,
    StringList,
    Duration,
    Float,
    FloatBeforeRefresh,
    NoFinalWitness,
    CancelPreparation,
    OrderedList,
    ReorderedList,
    DefaultedMap,
    ZeroDefaultedMap,
    BlockAlias,
}

impl PublicDesiredCase {
    fn spec(self) -> (&'static str, &'static str, AttributeType, Value, bool) {
        use carina_core::schema::PatternStructField as Field;
        let list = |items: &[&str]| {
            Value::Concrete(ConcreteValue::List(
                items.iter().map(|s| string_value(s)).collect(),
            ))
        };
        let map = |items: Vec<(&str, Value)>| {
            Value::Concrete(ConcreteValue::Map(
                items.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))
        };
        match self {
            Self::EnumIdentifier | Self::EnumString => {
                let aliases = if self == Self::EnumIdentifier {
                    vec![("public".into(), "Public".into())]
                } else {
                    vec![]
                };
                let ty = AttributeType::enum_(
                    carina_core::schema::enum_identity("Mode", Some("round6.test.Resource")),
                    Some(vec!["public".into()]),
                    aliases,
                    None,
                    None,
                );
                let encoded: PersistedValue = serde_json::from_value(json!({"$enum":{"identity":{"provider":"round6","segments":["test","Resource"],"kind":"Mode"},"api_value":"public"}})).unwrap();
                (
                    if self == Self::EnumIdentifier {
                        "EnumIdentifier"
                    } else {
                        "enum string"
                    },
                    if self == Self::EnumIdentifier {
                        "Public"
                    } else {
                        "'public'"
                    },
                    ty,
                    encoded.decode().unwrap().to_value_at("computed").unwrap(),
                    false,
                )
            }
            Self::Lowercase => (
                "normalize_desired_lowercase",
                "'PuBlIc'",
                AttributeType::string(),
                string_value("public"),
                false,
            ),
            Self::NoFinalWitness | Self::CancelPreparation => (
                "refresh without final desired witness",
                "'public'",
                AttributeType::string(),
                string_value("public"),
                true,
            ),
            Self::StringList => (
                "StringList vs list",
                "['public']",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
                list(&["public"]),
                false,
            ),
            // The provider's typed read value is a duration; the existing
            // persisted codec projects it to a JSON number at the state seam.
            Self::Duration => (
                "Duration vs number",
                "60s",
                AttributeType::duration(),
                Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60))),
                true,
            ),
            Self::Float | Self::FloatBeforeRefresh => (
                "Int for Float slot",
                "1",
                AttributeType::float(),
                Value::Concrete(ConcreteValue::Float(1.0.try_into().unwrap())),
                false,
            ),
            Self::OrderedList | Self::ReorderedList => (
                if self == Self::OrderedList {
                    "ordered reordered list"
                } else {
                    "unordered reordered list"
                },
                "['public', 'other']",
                if self == Self::OrderedList {
                    AttributeType::list(AttributeType::string())
                } else {
                    AttributeType::unordered_list(AttributeType::string())
                },
                list(&["other", "public"]),
                self == Self::OrderedList,
            ),
            Self::DefaultedMap | Self::ZeroDefaultedMap => (
                if self == Self::DefaultedMap {
                    "provider non-empty defaulted map field"
                } else {
                    "provider false defaulted map field"
                },
                "{ name = 'public' }",
                AttributeType::struct_(
                    "Config",
                    vec![
                        Field::optional("name", AttributeType::string()).build(),
                        Field::optional(
                            "defaulted",
                            if self == Self::DefaultedMap {
                                AttributeType::string()
                            } else {
                                AttributeType::bool()
                            },
                        )
                        .build(),
                    ],
                ),
                map(vec![
                    ("name", string_value("public")),
                    (
                        "defaulted",
                        if self == Self::DefaultedMap {
                            string_value("default")
                        } else {
                            Value::Concrete(ConcreteValue::Bool(false))
                        },
                    ),
                ]),
                self == Self::DefaultedMap,
            ),
            Self::BlockAlias => (
                "block-alias key",
                "{ alias { name = 'public' } }",
                AttributeType::struct_(
                    "Config",
                    vec![
                        Field::optional(
                            "canonical",
                            AttributeType::list(AttributeType::struct_(
                                "Item",
                                vec![Field::optional("name", AttributeType::string()).build()],
                            )),
                        )
                        .with_block_name("alias")
                        .build(),
                    ],
                ),
                map(vec![(
                    "canonical",
                    Value::Concrete(ConcreteValue::List(vec![map(vec![(
                        "name",
                        string_value("public"),
                    )])])),
                )]),
                false,
            ),
        }
    }
}

async fn assert_public_desired_commands(shape: PublicDesiredCase) {
    let project = tempfile::tempdir().unwrap();
    let path = project.path();
    let state_path = path.join("carina.state.json");
    let (case, authored, computed_type, public, expected_hash) = shape.spec();
    let source = format!(
        "backend local {{ path = '{}' }}\nprovider round6 {{}}\nlet sink = round6.test.Resource {{ name = 'sink' computed = {authored} }}\n",
        state_path.display()
    );
    // Match the injected factory's existing saved-plan artifact evidence.
    let artifact = path.join("fixture.wasm");
    std::fs::write(&artifact, b"in-process test factory round6").unwrap();
    let source = source.replace(
        "provider round6 {}",
        &format!(
            "provider round6 {{ source = 'file://{}' }}",
            artifact.display()
        ),
    );
    std::fs::write(path.join("main.crn"), &source).unwrap();
    std::fs::write(path.join("carina-backend.lock"),
        serde_json::to_string_pretty(&json!({"backend_type":"local", "attributes":{"path":state_path.display().to_string()}})).unwrap()
    ).unwrap();
    let schema = ResourceSchema::new("test.Resource")
        .attribute(
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(AttributeSchema::optional("computed", computed_type).build())
        .validate()
        .unwrap();
    let id = ResourceId::with_provider_identity("round6", "test.Resource", "sink", None);
    let initial_material = (shape == PublicDesiredCase::FloatBeforeRefresh).then(|| public.clone());
    let fresh: State = State::existing(
        id.clone(),
        HashMap::from([
            ("name".into(), string_value("sink")),
            ("computed".into(), public),
        ]),
    )
    .with_identifier("sink-id");
    let factory = MeasurementFactory {
        provider: Arc::new(
            DeposedRefreshTestProvider::default()
                .with_saved_attr_hydration()
                .with_read_state(&id, "sink-id", fresh),
        ),
        schema,
        lowercase: shape == PublicDesiredCase::Lowercase,
        fail_desired: Arc::new(std::sync::atomic::AtomicBool::new(
            shape == PublicDesiredCase::NoFinalWitness,
        )),
        pause_desired: (shape == PublicDesiredCase::CancelPreparation)
            .then(|| Arc::new(tokio::sync::Notify::new())),
    };
    let runtime = crate::test_support::TestCommandRuntime::new();
    let marker = std::env::var("CARINA_RETAINED_SECRET_MARKER").unwrap();
    let hash = PersistedValue::encode_desired_value(
        &Value::Deferred(DeferredValue::Secret(
            Box::new(
                initial_material
                    .clone()
                    .unwrap_or_else(|| string_value(&marker)),
            )
            .into(),
        )),
        None,
    )
    .unwrap();
    let mut initial = StateFile::new();
    let mut row = ResourceState::try_applied("test.Resource", "sink", "round6", "sink-id")
        .unwrap()
        .with_attribute("name", json!("sink"))
        .with_attribute("computed", hash.as_json().clone());
    if shape == PublicDesiredCase::FloatBeforeRefresh {
        let material = initial_material.unwrap();
        let resource: carina_core::resource::Resource =
            carina_core::resource::Resource::with_provider("round6", "test.Resource", "sink", None)
                .with_absolute_binding(absolute_binding("sink"))
                .with_attribute("name", string_value("sink"))
                .with_attribute(
                    "computed",
                    Value::Deferred(DeferredValue::Secret(Box::new(material.clone()).into())),
                );
        let resolved = carina_core::resource::ResolvedResource::resolve_raw_fixture(
            resource,
            &SchemaRegistry::new(),
        )
        .unwrap();
        let prior: State = State::existing(
            resolved.id.clone(),
            HashMap::from([
                ("name".into(), string_value("sink")),
                ("computed".into(), material),
            ]),
        )
        .with_identifier("sink-id");
        row = ResourceState::from_provider_state_for_resource_and_schema(
            &resolved,
            &prior,
            None,
            None,
            carina_state::RowBasis::Imported,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        .unwrap();
    }
    row.binding = Some(absolute_binding("sink"));
    initial.upsert_resource(row).unwrap();
    let backend = crate::commands::local_backend_for_test(state_path.clone());
    backend.write_state(&initial).await.unwrap();
    let base = carina_core::paths::ResolvedPath::new(path).unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    if shape == PublicDesiredCase::CancelPreparation {
        let loaded =
            load_configuration_with_config(&base, &context, &SchemaRegistry::new()).unwrap();
        let validated = ProviderLoad::for_test_with_provider_phase(
            &runtime,
            factory.loaded(),
            &loaded.parsed.providers,
        )
        .validate_state_refresh_config(
            loaded.parsed,
            loaded.unresolved_parsed,
            &base,
            &loaded.duplicate_declarations,
        )
        .unwrap();
        let root = ShutdownToken::running();
        let (trigger, cancel) = root.with_graceful_shutdown_trigger();
        let disabled = StateLock::disabled_for_test();
        let mut refresh = Box::pin(run_state_refresh_locked(
            validated,
            backend.as_ref(),
            disabled.as_ref(),
            &base,
            &context,
            cancel,
        ));
        tokio::select! {
            result = &mut refresh => panic!("refresh completed before paused desired preparation: {}", result.is_ok()),
            _ = factory.pause_desired.as_ref().unwrap().notified() => {}
        }
        trigger.request_graceful_shutdown();
        let result = futures::FutureExt::now_or_never(refresh.as_mut());
        assert!(
            matches!(result, Some(Err(AppError::Interrupted))),
            "refresh must stop polling paused desired preparation when its shutdown token is cancelled"
        );
        let persisted = backend.read_state().await.unwrap().unwrap();
        assert_eq!(
            persisted.serial, initial.serial,
            "cancelled preparation must not save state"
        );
        assert!(
            !std::fs::read_to_string(&state_path)
                .unwrap()
                .contains(&marker)
        );
        return;
    }
    if matches!(
        shape,
        PublicDesiredCase::DefaultedMap | PublicDesiredCase::OrderedList
    ) {
        let changes = crate::test_support::run_refreshing_plan_with_factories(
            &runtime,
            path,
            factory.loaded(),
        )
        .await
        .unwrap();
        assert!(
            changes,
            "main also reports a fresh diff for this ordered list / non-empty extra field"
        );
        assert!(
            !std::fs::read_to_string(&state_path)
                .unwrap()
                .contains(&marker),
            "fresh control must not expose old material in state"
        );
        let current = backend.read_state().await.unwrap().unwrap();
        assert!(
            current
                .find_resource("round6", "test.Resource", "sink")
                .unwrap()
                .persisted_attributes()
                .unwrap()
                .get("computed")
                .unwrap()
                .secret_hash()
                .is_some(),
            "planning must leave the control's stored hash unchanged"
        );
        let saved = path.join("fresh-diff.plan.json");
        let json_changes = crate::test_support::run_observed_plan_with_factories(
            &runtime,
            path,
            Some(&saved),
            true,
            true,
            factory.loaded(),
        )
        .await
        .unwrap();
        assert_eq!(
            json_changes, changes,
            "JSON and saved plan must preserve the fresh control's Update"
        );
        assert!(
            !std::fs::read_to_string(saved).unwrap().contains(&marker),
            "fresh control saved plan must hide old material"
        );
        return;
    }
    if shape == PublicDesiredCase::FloatBeforeRefresh {
        let changes =
            crate::test_support::run_plan_with_factories(&runtime, path, factory.loaded())
                .await
                .unwrap();
        assert!(
            changes,
            "cached Float/Int hash material differs until a refreshed write releases it"
        );
    }
    let mut outcomes = Vec::new();
    for step in [
        "refresh",
        "plan",
        "cached plan",
        "apply",
        "final plan",
        "final cached plan",
    ] {
        let result: Result<Option<bool>, AppError> = match step {
            "refresh" => {
                let loaded =
                    load_configuration_with_config(&base, &context, &SchemaRegistry::new())
                        .unwrap();
                let providers = ProviderLoad::for_test_with_provider_phase(
                    &runtime,
                    factory.loaded(),
                    &loaded.parsed.providers,
                );
                let validated = providers
                    .validate_state_refresh_config(
                        loaded.parsed,
                        loaded.unresolved_parsed,
                        &base,
                        &loaded.duplicate_declarations,
                    )
                    .unwrap();
                run_state_refresh_locked(
                    validated,
                    backend.as_ref(),
                    StateLock::disabled_for_test().as_ref(),
                    &base,
                    &context,
                    ShutdownToken::running(),
                )
                .await
                .map(|()| None)
            }
            "plan" | "final plan" => crate::test_support::run_refreshing_plan_with_factories(
                &runtime,
                path,
                factory.loaded(),
            )
            .await
            .map(Some),
            "cached plan" | "final cached plan" => {
                crate::test_support::run_plan_with_factories(&runtime, path, factory.loaded())
                    .await
                    .map(Some)
            }
            "apply" => crate::commands::apply::run_apply_with_factories_for_test(
                &runtime,
                path,
                factory.loaded(),
            )
            .await
            .map(|()| None),
            _ => unreachable!(),
        };
        if step == "refresh" {
            factory
                .fail_desired
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        let text = std::fs::read_to_string(&state_path).unwrap();
        assert!(
            !text.contains(&marker),
            "old marker must be absent from state"
        );
        let state = backend.read_state().await.unwrap().unwrap();
        let hash = state
            .find_resource("round6", "test.Resource", "sink")
            .unwrap()
            .persisted_attributes()
            .unwrap()
            .get("computed")
            .unwrap()
            .secret_hash()
            .is_some();
        if hash {
            let shown = format_state_show(&state);
            assert!(
                shown.contains("(secret)"),
                "protected state must display a mask"
            );
            assert!(
                !shown.contains(&marker),
                "state show must hide old material"
            );
        }
        let (exit, changes, error) = match result {
            Ok(changes) => (0, changes, None),
            Err(error) => (
                1,
                None,
                Some(error.to_string().replace(&marker, "<MASKED>")),
            ),
        };
        outcomes.push((step, exit, changes, hash));
        if shape == PublicDesiredCase::EnumString {
            assert!(
                hash,
                "invalid authored input must not release the candidate hash"
            );
            if step == "refresh" {
                assert!(error.is_none(), "{case}: {error:?}; steps: {outcomes:?}");
            } else {
                assert_eq!(
                    error.as_deref(),
                    Some(
                        "round6.test.Resource.sink: 'computed' (Mode) expects an enum identifier, got a string literal 'public'. Use one of: round6.test.Resource.Mode.public"
                    ),
                    "the same invalid enum string is rejected on main"
                );
            }
        } else {
            assert!(error.is_none(), "{case}: {error:?}; steps: {outcomes:?}");
        }
    }
    if shape == PublicDesiredCase::EnumString {
        for refresh in [true, false] {
            let saved = path.join(if refresh {
                "fresh-rejected.plan.json"
            } else {
                "rejected.plan.json"
            });
            let error = crate::test_support::run_observed_plan_with_factories(
                &runtime,
                path,
                Some(&saved),
                refresh,
                true,
                factory.loaded(),
            )
            .await
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("expects an enum identifier, got a string literal")
            );
            assert!(
                !saved.exists(),
                "rejected input must not create a saved plan"
            );
        }
        return;
    }
    assert!(
        outcomes.iter().all(|(_, exit, changes, hash)| *exit == 0
            && *changes != Some(true)
            && *hash == expected_hash),
        "{case}: both plan forms must converge (expected hash: {expected_hash}): {outcomes:?}"
    );
    for refresh in [true, false] {
        let saved = path.join(if refresh {
            "fresh-public.plan.json"
        } else {
            "public.plan.json"
        });
        let saved_changes = crate::test_support::run_observed_plan_with_factories(
            &runtime,
            path,
            Some(&saved),
            refresh,
            true,
            factory.loaded(),
        )
        .await
        .unwrap();
        let saved_text = std::fs::read_to_string(saved).unwrap();
        assert!(
            !saved_text.contains(&marker),
            "saved plan must not contain old material"
        );
        assert!(
            !saved_changes,
            "matching public values add no JSON or saved-plan effects"
        );
    }
}

#[tokio::test]
async fn retained_secret_public_enum_converges() {
    if retained_secret_child("retained_secret_public_enum_converges") {
        assert_public_desired_commands(PublicDesiredCase::EnumIdentifier).await;
    }
}

#[tokio::test]
async fn retained_secret_normalized_public_literal_converges() {
    if retained_secret_child("retained_secret_normalized_public_literal_converges") {
        assert_public_desired_commands(PublicDesiredCase::Lowercase).await;
    }
}

macro_rules! public_desired_case {
    ($name:ident, $shape:ident) => {
        #[tokio::test]
        async fn $name() {
            if retained_secret_child(stringify!($name)) {
                assert_public_desired_commands(PublicDesiredCase::$shape).await;
            }
        }
    };
}
public_desired_case!(retained_secret_authored_enum_string_is_rejected, EnumString);
public_desired_case!(
    retained_secret_ordered_reordered_list_has_fresh_diff_on_main,
    OrderedList
);
public_desired_case!(retained_secret_string_list_converges_at_refresh, StringList);
public_desired_case!(
    retained_secret_duration_keeps_hash_and_quiet_plans,
    Duration
);
public_desired_case!(retained_secret_int_for_float_converges_at_refresh, Float);
public_desired_case!(
    retained_secret_unordered_reordered_list_converges,
    ReorderedList
);
public_desired_case!(
    retained_secret_defaulted_map_has_fresh_diff_on_main,
    DefaultedMap
);
public_desired_case!(retained_secret_block_alias_converges, BlockAlias);
public_desired_case!(
    retained_secret_false_defaulted_map_converges_at_refresh,
    ZeroDefaultedMap
);

public_desired_case!(
    retained_secret_int_for_float_cached_before_refresh_updates,
    FloatBeforeRefresh
);
public_desired_case!(
    retained_secret_refresh_without_final_witness_keeps_hash_and_quiet_plans,
    NoFinalWitness
);

public_desired_case!(
    retained_secret_refresh_preparation_observes_shutdown,
    CancelPreparation
);
