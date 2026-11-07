//! Native provider fixtures for carina#4305. Errors cross the same wire
//! serialization boundary as WASM errors; plaintext comes from checked inputs.
#![allow(dead_code)] // Each consuming integration-test binary uses a subset.

#[path = "../../../carina-core/tests/support/provider_text.rs"]
pub mod text_fixtures;
use std::collections::BTreeMap;
use std::sync::Mutex;
use text_fixtures::NativeWireFixture;

use carina_core::effect::PlanOp;
use carina_core::provider::{
    AppliedIdentifier, BoxFuture, CanonicalProviderConfigAttributes, CreateAttempt, CreateFailure,
    CreateOutcome, CreateRequest, DeleteRequest, InitializeOutcome, InitializeOutcomeMint,
    Provider, ProviderError, ProviderFactory, ProviderInitialization, ProviderReadyDataSource,
    ProviderResult, ReadOutcome, ReadRequest, ResumeCreateRequest, ResumeFailure, ResumeOutcome,
    UpdateOutcome, UpdateRequest, WireAttributes, WirePatchOp, WireValue,
};
use carina_core::resource::{ResourceId, State};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockProvider, MockScope};

pub const SECRET: &str = "provider-text-private-4305";
// SECRET_VALUE_MASK is crate-private in carina-core::schema.
pub const MASK: &str = "(secret)";
pub const PUBLIC: &str = "public-value-4305";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    CreateMessage,
    CreateCause,
    Update,
    Delete,
    Refresh,
    RecoveryRefresh,
    DataSource,
    ConfigCause,
    JsonEscaped,
    Repeated,
    HostRelay,
    HostCause,
    RefreshDesired,
    ScalarStatus,
    PartialMissingKey,
    DestroyRefresh,
    MovedRefresh,
    StateOnly,
    ShortMissing,
    UnknownMissing,
    HostStatus,
    Transient,
}

pub fn wire_string(value: &WireValue) -> String {
    serde_json::to_value(value.value())
        .expect("wire value serializes")
        .as_str()
        .expect("fixture input is a plaintext string at the provider boundary")
        .to_owned()
}

pub fn password(attributes: &WireAttributes) -> String {
    wire_string(attributes.get("password").expect("checked password input"))
}

pub fn round_trip(error: ProviderError) -> ProviderError {
    ProviderError::from_wire(error.to_wire())
}

pub struct EchoMock {
    inner: MockProvider,
    failure: Failure,
    // Read/delete have no attribute payload. Like a real provider, retain the
    // credential received during initialization or a prior successful create.
    password: Mutex<Option<String>>,
    deletes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl EchoMock {
    pub fn new(failure: Failure) -> Self {
        Self {
            inner: ConfiguredBuiltinMock::new(MockBehavior::default(), MockScope::memory())
                .create_provider(),
            failure,
            password: Mutex::new(None),
            deletes: Default::default(),
        }
    }

    pub fn with_delete_counter(
        mut self,
        deletes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        self.deletes = deletes;
        self
    }
    fn remembered_password(&self) -> String {
        self.password
            .lock()
            .unwrap()
            .clone()
            .expect("provider received plaintext")
    }

    fn error(&self, operation: &str, plaintext: &str) -> ProviderError {
        round_trip(ProviderError::api_error(format!(
            "mock {operation} rejected password {plaintext}; public={PUBLIC}"
        )))
    }
}

impl Provider for EchoMock {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        if self.failure == Failure::StateOnly {
            return Box::pin(std::future::ready(Ok(ReadOutcome::found(
                carina_core::provider::AppliedState::from_identifier(
                    id.clone(),
                    identifier.clone(),
                    std::collections::HashMap::from([(
                        "name".into(),
                        carina_core::resource::Value::Concrete(
                            carina_core::resource::ConcreteValue::String("orphan".into()),
                        ),
                    )]),
                ),
            ))));
        }
        if matches!(
            self.failure,
            Failure::RefreshDesired | Failure::DestroyRefresh | Failure::MovedRefresh
        ) {
            let error = self.error(
                "refresh read",
                if self.failure == Failure::DestroyRefresh {
                    "hunter2"
                } else {
                    SECRET
                },
            );
            return Box::pin(async { Err(error) });
        }
        if matches!(self.failure, Failure::Refresh | Failure::RecoveryRefresh) {
            let operation = if self.failure == Failure::RecoveryRefresh {
                "recovery read"
            } else {
                "refresh read"
            };
            let error = self.error(operation, &self.remembered_password());
            Box::pin(async { Err(error) })
        } else {
            self.inner.read(id, identifier, request)
        }
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        assert_eq!(self.failure, Failure::DataSource);
        let plaintext = password(resource.wire_attributes());
        let error = self.error("data source read", &plaintext);
        Box::pin(async { Err(error) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let plaintext = if self.failure == Failure::ScalarStatus {
            "3".to_owned()
        } else {
            password(request.resource().wire_attributes())
        };
        *self.password.lock().unwrap() = Some(plaintext.clone());
        if matches!(
            self.failure,
            Failure::PartialMissingKey | Failure::ShortMissing | Failure::UnknownMissing
        ) {
            let outcome = attempt
                .issued(
                    AppliedIdentifier::new("mock-id").unwrap(),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .partial(
                    std::collections::HashMap::new(),
                    carina_core::provider::PartialReadDiagnostic::new(
                        format!("public incomplete read {plaintext}").into(),
                        vec![
                            match self.failure {
                                Failure::ShortMissing => "subnet_1_id",
                                Failure::UnknownMissing => "not_in_schema",
                                _ => "password",
                            }
                            .into(),
                        ],
                    )
                    .unwrap(),
                );
            return Box::pin(async { Ok(outcome) });
        }
        let error = match self.failure {
            Failure::HostStatus => ProviderError::api_error("provider digits 404").with_status(404),
            Failure::Transient => {
                ProviderError::api_error(format!("transient rejected password {plaintext}"))
                    .with_transient(true)
            }
            Failure::HostRelay => carina_core::provider::HostError::internal(format!(
                "mock create rejected password {plaintext}; public={PUBLIC}"
            ))
            .into(),
            Failure::HostCause => ProviderError::internal_caused_by(
                "mock create rejected password",
                std::io::Error::other(format!("password {plaintext}; public={PUBLIC}")),
            ),
            Failure::ScalarStatus => {
                ProviderError::api_error("mock create rejected password 3 true").with_status(403)
            }
            Failure::CreateMessage => self.error("create", &plaintext),
            Failure::CreateCause => round_trip(ProviderError::api_error_caused_by(
                "mock create rejected password",
                std::io::Error::other(format!("invalid password {plaintext}; public={PUBLIC}")),
            )),
            Failure::JsonEscaped => {
                let encoded = serde_json::to_string(&plaintext).unwrap();
                self.error("create", &encoded)
            }
            Failure::Repeated => self.error("create", &format!("{plaintext} then {plaintext}")),
            Failure::Update | Failure::Delete | Failure::RecoveryRefresh => {
                return self.inner.create(id, request, attempt);
            }
            other => panic!("unexpected create in {other:?}"),
        };
        Box::pin(async { Err(attempt.not_created(error)) })
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        self.inner.resume_create(request)
    }

    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        assert!(matches!(
            self.failure,
            Failure::Update | Failure::RecoveryRefresh
        ));
        let plaintext = request
            .wire_patch()
            .iter()
            .find_map(|operation| match operation {
                WirePatchOp::Add { key, value }
                | WirePatchOp::Replace { key, value }
                | WirePatchOp::Reassert { key, value }
                    if key == "password" =>
                {
                    Some(wire_string(value))
                }
                _ => None,
            })
            .expect("checked update patch carries plaintext password");
        *self.password.lock().unwrap() = Some(plaintext.clone());
        let error = if self.failure == Failure::RecoveryRefresh {
            round_trip(ProviderError::api_error("mock update rejected request"))
        } else {
            self.error("update", &plaintext)
        };
        Box::pin(async { Err(error) })
    }

    fn delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        if self.failure == Failure::StateOnly {
            self.deletes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Box::pin(async { Ok(()) });
        }
        assert_eq!(self.failure, Failure::Delete);
        let error = self.error("delete", &self.remembered_password());
        Box::pin(async { Err(error) })
    }

    fn required_permissions(&self, _: &ResourceId, _: PlanOp) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

pub struct EchoFactory(pub Failure);

impl ProviderFactory for EchoFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Provider text secret regression fixture"
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::from([(
            "password".into(),
            AttributeType::string(),
        )]))
        .unwrap()
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        if self.0 == Failure::ConfigCause {
            let plaintext = password(attributes.wire_attributes());
            Err(round_trip(ProviderError::api_error_caused_by(
                "mock config rejected",
                std::io::Error::other(format!(
                    "invalid config password {plaintext}; public={PUBLIC}"
                )),
            )))
        } else {
            Ok(())
        }
    }

    fn create_provider(
        &self,
        config: &carina_core::provider::ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        let provider = EchoMock::new(self.0);
        if let Some(config) = config.configuration()
            && config.wire_attributes().get("password").is_some()
        {
            *provider.password.lock().unwrap() = Some(password(config.wire_attributes()));
        }
        Box::pin(async { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) })
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("password", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("numeric", AttributeType::int()).build())
                .attribute(AttributeSchema::optional("boolean", AttributeType::bool()).build())
                .attribute(
                    AttributeSchema::optional("subnet_1_id", AttributeType::string()).build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("test.lookup")
                .as_data_source()
                .attribute(AttributeSchema::required("password", AttributeType::string()).build())
                .validate()
                .unwrap(),
        ]
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

pub fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    for schema in EchoFactory(Failure::CreateMessage).schemas() {
        schemas.insert("mock", schema);
    }
    schemas
}
