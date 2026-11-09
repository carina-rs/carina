//! The only ingestion boundary for a configured provider instance.
use super::RedactionSet;
use crate::provider::*;
#[cfg(test)]
use crate::resource::{DataSource, Resource};
use crate::resource::{ResourceId, State};

// Only this boundary can mint a raw-readable host schema cause. Provider-authored
// SchemaRegistryError values remain opaque after ordinary provider error binding.
#[derive(Debug)]
pub(super) struct EventSchemaInstallError(crate::schema::SchemaRegistryError);
impl EventSchemaInstallError {
    pub(super) fn source_error(&self) -> &crate::schema::SchemaRegistryError {
        &self.0
    }
}
impl std::fmt::Display for EventSchemaInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}
impl std::error::Error for EventSchemaInstallError {}

fn event_schema_install_error(error: crate::schema::SchemaRegistryError) -> HostError {
    HostError::internal_caused_by(
        "Failed to install provider schemas",
        super::HostDiagnostic::capture_host_or_provider(EventSchemaInstallError(error)),
    )
}

/// Core-minted admission inventory for the complete desired configuration.
/// Obtain it from `ResolvedFile::provider_plan` or a rehydrated saved
/// configuration; resource
/// slices cannot stand in for a whole plan. State-only operations are explicit.
///
/// Caller-built files cannot mint a whole-plan witness.
///
/// ```compile_fail,E0624
/// use carina_core::parser::ExpandedFile;
/// use carina_core::resource::Resource;
/// let mut file = ExpandedFile::<()>::default();
/// file.resources.push(Resource::with_provider("aws", "s3.Bucket", "partial", None));
/// let _ = file.provider_plan();
/// ```
#[derive(Clone, Debug)]
pub struct ProviderPlan {
    routes: std::collections::HashMap<ProviderInstanceKey, RedactionSet>,
    addresses: super::events::ProtectedAddresses,
}
impl ProviderPlan {
    /// No desired configuration exists (for example, state-only cleanup).
    pub fn state_only() -> Self {
        Self {
            routes: Default::default(),
            addresses: Default::default(),
        }
    }
    #[cfg(test)]
    pub(crate) fn from_desired<S, D>(
        desired: &[Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >],
        data_sources: &[DataSource],
    ) -> Self {
        let mut plan = Self::state_only();
        for resource in desired {
            plan.include(&resource.id, resource.attributes.iter());
        }
        for source in data_sources {
            plan.include(&source.id, source.attributes.iter());
        }
        plan
    }
    pub(crate) fn include<'a, P: crate::resource::ValueKind + 'a>(
        &mut self,
        id: &ResourceId,
        values: impl IntoIterator<Item = (&'a String, &'a crate::resource::Value<P>)>,
    ) {
        let values = values.into_iter().collect::<Vec<_>>();
        super::events::include_protected_values(&mut self.addresses, id, values.iter().copied());
        self.routes
            .entry(ProviderInstanceKey::from(id))
            .or_insert_with(RedactionSet::before_request)
            .extend(&RedactionSet::collect(
                values.into_iter().map(|(_name, value)| value),
            ));
    }
    host_api! {
        /// Mint one configured or built-in provider session from this whole plan.
        fn session(
            &self,
            provider: &str,
            binding: Option<&str>,
            config: Option<&ProviderReadyConfig>,
        ) -> ProviderSession {
            self.session_builder(provider, binding, config).build()
        }
    }
    host_api! {
        /// Select host presentation before constructing and sharing a session.
        fn session_builder(
            &self,
            provider: &str,
            binding: Option<&str>,
            config: Option<&ProviderReadyConfig>,
        ) -> ProviderSessionBuilder {
            ProviderSessionBuilder {
                plan: self.clone(),
                provider: provider.to_owned(),
                binding: binding.map(str::to_owned),
                config: config.cloned(),
                sink: super::events::EventSink::Default,
            }
        }
    }
}

/// Host-owned construction, consumed before the session can be shared.
/// No session can be converted back into this builder.
///
/// ```
/// # #[cfg(all(feature = "provider-host", not(target_arch = "wasm32")))] {
/// use carina_core::provider::ProviderPlan;
/// let session = ProviderPlan::state_only().session_builder("native", None, None)
///     .with_diagnostic_sink(|_event| {}).build();
/// let _handle = session.diagnostics();
/// # }
/// ```
pub struct ProviderSessionBuilder {
    plan: ProviderPlan,
    provider: String,
    binding: Option<String>,
    config: Option<ProviderReadyConfig>,
    sink: super::events::EventSink,
}
impl ProviderSessionBuilder {
    pub fn with_diagnostic_sink(
        mut self,
        sink: impl Fn(super::BoundProviderEvent) + Send + Sync + 'static,
    ) -> Self {
        self.sink = super::events::EventSink::Custom(std::sync::Arc::new(sink));
        self
    }
    pub fn build(self) -> ProviderSession {
        ProviderSession::new(
            &self.plan,
            &self.provider,
            self.binding.as_deref(),
            self.config.as_ref(),
            self.sink,
        )
    }
}

/// Core-minted inputs shared by a provider and its normalizer.
///
/// ```compile_fail,E0599
/// use carina_core::provider::ProviderSession;
/// fn redirect(session: &ProviderSession) {
///     let _ = session.clone().with_diagnostic_sink(|_event| {});
/// }
/// ```
#[derive(Clone, Debug)]
pub struct ProviderSession {
    config: Option<ProviderReadyConfig>,
    provider: String,
    binding: Option<String>,
    secrets: RedactionSet,
    diagnostics: super::ProviderDiagnosticChannel,
    diagnostic_owner: super::DiagnosticOwner,
}
impl PartialEq for ProviderSession {
    fn eq(&self, other: &Self) -> bool {
        // Liveness is not part of session value equality; the old Arc<()> owner
        // also compared equal across independent allocations.
        let Self {
            config,
            provider,
            binding,
            secrets,
            diagnostics,
            diagnostic_owner: _owner,
        } = self;
        let Self {
            config: other_config,
            provider: other_provider,
            binding: other_binding,
            secrets: other_secrets,
            diagnostics: other_diagnostics,
            diagnostic_owner: _other_owner,
        } = other;
        config == other_config
            && provider == other_provider
            && binding == other_binding
            && secrets == other_secrets
            && diagnostics == other_diagnostics
    }
}
impl ProviderSession {
    /// Core-owned call inputs for guest handoff; this does not grant runtime
    /// initialization authority to a validation input.
    pub fn guest_inputs(&self) -> ProviderValidationInputs {
        ProviderValidationInputs {
            secrets: self.secrets.clone(),
            diagnostics: self.diagnostics.clone(),
            diagnostic_owner: self.diagnostic_owner.clone(),
        }
    }
    pub fn guest_inventory(&self) -> super::GuestInputInventory {
        self.guest_inputs().guest_inventory()
    }
    fn new(
        plan: &ProviderPlan,
        provider: &str,
        binding: Option<&str>,
        config: Option<&ProviderReadyConfig>,
        sink: super::events::EventSink,
    ) -> Self {
        let secrets = RedactionSet::collect(
            config
                .iter()
                .flat_map(|config| config.attributes().values()),
        );
        if let Some(desired) = plan.routes.get(&ProviderInstanceKey::new(
            provider,
            binding.map(str::to_owned),
        )) {
            secrets.extend(desired);
        }
        let addresses = plan
            .addresses
            .iter()
            .filter(|(id, _paths)| {
                id.provider == provider && id.provider_instance.as_deref() == binding
            })
            .map(|(id, paths)| (id.clone(), paths.clone()))
            .collect();
        let (diagnostics, diagnostic_owner) = super::ProviderDiagnosticChannel::with_sink(
            secrets.clone(),
            provider.to_owned(),
            addresses,
            sink,
        );
        Self {
            config: config.cloned(),
            provider: provider.to_owned(),
            binding: binding.map(str::to_owned),
            diagnostics,
            diagnostic_owner,
            secrets,
        }
    }
    pub fn configuration(&self) -> Option<&ProviderReadyConfig> {
        self.config.as_ref()
    }
    pub fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
    pub fn redaction_set(&self) -> &RedactionSet {
        &self.secrets
    }
    pub fn diagnostics(&self) -> super::ProviderDiagnosticChannel {
        self.diagnostics.clone()
    }
    fn install_event_schemas(&self, factory: &dyn ProviderFactory) -> HostResult<()> {
        self.diagnostics
            .set_schema_witness(&factory.schema_witness())
            .map_err(event_schema_install_error)
    }
}

/// Inputs for validation before a checked configuration exists. This type
/// cannot initialize a provider boundary or become a runtime session.
#[derive(Clone, Debug)]
pub struct ProviderValidationInputs {
    secrets: RedactionSet,
    diagnostics: super::ProviderDiagnosticChannel,
    diagnostic_owner: super::DiagnosticOwner,
}
impl ProviderValidationInputs {
    pub fn guest_inventory(&self) -> super::GuestInputInventory {
        super::GuestInputInventory::new(self)
    }
    pub(crate) fn collect<
        'a,
        P: crate::resource::ValueKind + 'a,
        B: crate::resource::BindingAddress + 'a,
    >(
        provider: &str,
        values: impl IntoIterator<Item = &'a crate::resource::Value<P, B>>,
    ) -> Self {
        let secrets = RedactionSet::collect(values);
        let (diagnostics, diagnostic_owner) = super::ProviderDiagnosticChannel::new(
            secrets.clone(),
            provider.to_owned(),
            Default::default(),
        );
        Self {
            diagnostics,
            diagnostic_owner,
            secrets,
        }
    }
    pub fn redaction_set(&self) -> &RedactionSet {
        &self.secrets
    }
    pub fn diagnostics(&self) -> super::ProviderDiagnosticChannel {
        self.diagnostics.clone()
    }
    pub(super) fn diagnostic_owner(&self) -> super::DiagnosticOwner {
        self.diagnostic_owner.clone()
    }
    host_api! {
        fn inherit_diagnostic_host(&self, owner: &Self) -> Self {
            Self {
                secrets: self.secrets.clone(),
                diagnostics: self.diagnostics.inherit_host(&owner.diagnostics),
                diagnostic_owner: self.diagnostic_owner.clone(),
            }
        }
    }
    host_api! {
        /// A replacement component inherits host presentation, with a fresh inventory
        /// because it cannot remember values supplied to the discarded component.
        fn for_new_guest(&self) -> Self {
            let secrets = RedactionSet::before_request();
            let (diagnostics, diagnostic_owner) = self.diagnostics.for_new_guest(secrets.clone());
            Self {
                secrets,
                diagnostics,
                diagnostic_owner,
            }
        }
    }
    host_api! {
        fn install_event_schemas(
            &self,
            witness: &crate::schema::ValidatedProviderSchemaWitness,
        ) -> Result<(), crate::schema::SchemaRegistryError> {
            self.diagnostics.set_schema_witness(witness)
        }
    }
    pub(super) fn from_parts(
        secrets: RedactionSet,
        diagnostics: super::ProviderDiagnosticChannel,
        diagnostic_owner: super::DiagnosticOwner,
    ) -> Self {
        Self {
            secrets,
            diagnostics,
            diagnostic_owner,
        }
    }
}

// Kept private for core fixtures; production callers must supply a whole-plan
// witness and cannot accidentally substitute empty slices at initialization.
#[cfg(test)]
pub(crate) fn prepare_provider_session<S, D>(
    provider: &str,
    binding: Option<&str>,
    config: Option<&ProviderReadyConfig>,
    desired: &[Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    data_sources: &[DataSource],
) -> ProviderSession {
    ProviderPlan::from_desired(desired, data_sources).session(provider, binding, config)
}

/// A provider, its normalizer, and their shared append-only redaction set.
pub struct ProviderBoundary<'a> {
    provider: Box<dyn Provider + 'a>,
    normalizer: ConfiguredNormalizer,
    secrets: RedactionSet,
    schemas: crate::schema::SchemaRegistry,
    diagnostics: super::ProviderDiagnosticChannel,
}
impl ProviderBoundary<'_> {
    pub(crate) fn owns_desired(&self, id: &ResourceId) -> bool {
        self.normalizer.owns_desired(id)
    }
}
impl ProviderBoundary<'static> {
    pub async fn initialize(
        factory: &dyn ProviderFactory,
        session: &crate::provider::ProviderSession,
    ) -> HostResult<InitializedProvider> {
        session.install_event_schemas(factory)?;
        let (provider, outcome) = session
            .diagnostics
            .dispatch(|| factory.create_provider(session, InitializeOutcomeMint::new()))
            .await
            .map_err(|error| error.with_secrets(&session.secrets))?
            .into_parts();
        let normalizer = Self::initialize_normalizer(factory, session).await?;
        Ok(InitializedProvider {
            provider: std::sync::Arc::new(Self {
                provider,
                schemas: normalizer.schemas.clone(),
                normalizer,
                secrets: session.secrets.clone(),
                diagnostics: session.diagnostics.clone(),
            }),
            reference_scope: outcome.reference_scope,
        })
    }
    pub async fn initialize_normalizer(
        factory: &dyn ProviderFactory,
        session: &crate::provider::ProviderSession,
    ) -> HostResult<ConfiguredNormalizer> {
        session.install_event_schemas(factory)?;
        let ext = session
            .diagnostics
            .dispatch(|| factory.create_normalizer(session))
            .await
            .map_err(|error| error.with_secrets(&session.secrets))?;
        let mut schemas = crate::schema::SchemaRegistry::new();
        let witness = factory.schema_witness();
        schemas
            .insert_provider_witness(factory.name(), &witness)
            .map_err(event_schema_install_error)?;
        if session.provider != factory.name() {
            schemas
                .insert_provider_witness(&session.provider, &witness)
                .map_err(event_schema_install_error)?;
        }
        Ok(ConfiguredNormalizer {
            diagnostic_inputs: session.guest_inputs(),
            route: Some(ProviderInstanceKey::new(
                session.provider.clone(),
                session.binding.clone(),
            )),
            provider_name: factory.name().to_owned(),
            ext,
            config_secrets: session.secrets.clone(),
            schemas,
        })
    }
}

impl Provider<super::Bound> for ProviderBoundary<'_> {
    fn name(&self) -> &str {
        self.diagnostics.dispatch_sync(|| self.provider.name())
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, HostResult<ReadOutcome>> {
        self.diagnostics.include_address(id);
        let secrets = self.secrets.clone();
        let call = self
            .diagnostics
            .dispatch(|| self.provider.read(id, identifier, request));
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, HostResult<State>> {
        self.diagnostics.include_ready_data_source(resource);
        self.secrets
            .extend(&RedactionSet::collect(resource.diagnostic_values()));
        let secrets = self.secrets.clone();
        let call = self
            .diagnostics
            .dispatch(|| self.provider.read_data_source(resource));
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<HostCreateOutcome, HostCreateFailure>> {
        self.diagnostics.include_ready_resource(request.resource());
        self.secrets.extend(&RedactionSet::collect(
            request.resource().diagnostic_values(),
        ));
        let secrets = self.secrets.clone();
        let sent = SentAttributes::from_resource(request.resource().as_resource());
        let call = self
            .diagnostics
            .dispatch(|| self.provider.create(id, request, attempt));
        Box::pin(async move {
            call.await
                .and_then(|outcome| outcome.admit(sent, &self.schemas))
                .map(|outcome| outcome.with_secrets(&secrets))
                .map_err(|failure| failure.admit(&self.schemas).with_secrets(&secrets))
        })
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<HostResumeOutcome, HostResumeFailure>> {
        self.diagnostics
            .include_ready_resource(request.request().resource());
        self.secrets.extend(&RedactionSet::collect(
            request.request().resource().diagnostic_values(),
        ));
        let secrets = self.secrets.clone();
        let sent = SentAttributes::from_resource(request.request().resource().as_resource());
        let call = self
            .diagnostics
            .dispatch(|| self.provider.resume_create(request));
        Box::pin(async move {
            call.await
                .and_then(|outcome| outcome.admit(sent, &self.schemas))
                .map(|outcome| outcome.with_secrets(&secrets))
                .map_err(|failure| failure.admit(&self.schemas).with_secrets(&secrets))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        attempt: UpdateAttempt,
    ) -> BoxFuture<'_, HostResult<HostUpdateOutcome>> {
        self.diagnostics.include_address(id);
        self.secrets
            .extend(&RedactionSet::collect(request.diagnostic_values()));
        let secrets = self.secrets.clone();
        let id = id.clone();
        let call = self
            .diagnostics
            .dispatch(|| self.provider.update(&id, identifier, request, attempt));
        Box::pin(async move {
            let outcome = call.await.map_err(|error| error.with_secrets(&secrets))?;
            if let Some(diagnostic) = outcome.diagnostic() {
                diagnostic
                    .validate(&id, &self.schemas)
                    .map_err(|error| error.with_secrets(&secrets))?;
            }
            Ok(outcome.with_secrets(&secrets))
        })
    }

    fn delete(&self, attempt: DeleteAttempt) -> BoxFuture<'_, HostResult<()>> {
        let secrets = self.secrets.clone();
        let call = self.diagnostics.dispatch(|| self.provider.delete(attempt));
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn required_permissions(
        &self,
        id: &ResourceId,
        op: crate::effect::PlanOp,
    ) -> HostResult<Vec<super::RequiredPermission>> {
        self.diagnostics
            .dispatch_sync(|| self.provider.required_permissions(id, op))
            .map_err(|e| e.with_secrets(&self.secrets))
            .and_then(|actions| {
                actions
                    .into_iter()
                    .enumerate()
                    .map(|(index, action)| {
                        super::RequiredPermission::admit(action, index, &self.secrets).map_err(
                            |error| {
                                HostError::invalid_input_caused_by(
                                    "invalid provider permissions",
                                    error,
                                )
                            },
                        )
                    })
                    .collect()
            })
    }
    fn satisfier_hint(
        &self,
        id: &ResourceId,
        path: &crate::wait::predicate::AttrPath,
    ) -> HostResult<Vec<crate::wait::BindingPattern>> {
        self.diagnostics
            .dispatch_sync(|| self.provider.satisfier_hint(id, path))
            .map_err(|e| e.with_secrets(&self.secrets))
    }
    fn resolve_references<'a>(
        &'a self,
        requests: &'a [crate::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        HostResult<
            Vec<
                Result<
                    crate::references::ReferenceOutcome<super::Bound>,
                    crate::references::ReferenceError<super::Bound>,
                >,
            >,
        >,
    > {
        Box::pin(async move {
            use crate::references::{ReferenceError, ReferenceOutcome};
            self.diagnostics
                .dispatch(|| self.provider.resolve_references(requests))
                .await
                .map(|results| {
                    results
                        .into_iter()
                        .map(|result| match result {
                            Ok(ReferenceOutcome::Resolved(value)) => {
                                Ok(ReferenceOutcome::Resolved(value))
                            }
                            Ok(ReferenceOutcome::NotFound(error)) => Ok(
                                ReferenceOutcome::NotFound(error.with_secrets(&self.secrets)),
                            ),
                            Err(ReferenceError::AccessDenied(error)) => {
                                Err(ReferenceError::AccessDenied(std::sync::Arc::new(
                                    error.as_ref().clone().with_secrets(&self.secrets),
                                )))
                            }
                            Err(ReferenceError::Transient(error)) => {
                                Err(ReferenceError::Transient(std::sync::Arc::new(
                                    error.as_ref().clone().with_secrets(&self.secrets),
                                )))
                            }
                            Err(ReferenceError::InvalidReference(error)) => {
                                Err(ReferenceError::InvalidReference(std::sync::Arc::new(
                                    error.as_ref().clone().with_secrets(&self.secrets),
                                )))
                            }
                        })
                        .collect()
                })
                .map_err(|e| e.with_secrets(&self.secrets))
        })
    }
}

impl ProviderNormalizer<super::Bound> for ProviderBoundary<'_> {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizer.normalize_desired(resources)
    }
    fn normalize_dispatch_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizer.normalize_dispatch_desired(resources)
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizer.normalize_state(states)
    }
    fn hydrate_read_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
        saved: &'a SavedAttrs,
    ) -> BoxFuture<'a, HostResult<()>> {
        self.normalizer.hydrate_read_state(states, saved)
    }
}

#[cfg(test)]
impl<'a> ProviderBoundary<'a> {
    pub(crate) fn set_fixture_session(
        &mut self,
        session: ProviderSession,
        schemas: &crate::schema::SchemaRegistry,
    ) {
        let diagnostic_inputs = session.guest_inputs();
        self.secrets = session.secrets;
        self.diagnostics = session.diagnostics;
        self.diagnostics.set_schemas(schemas.clone());
        self.schemas = schemas.clone();
        self.normalizer.config_secrets = self.secrets.clone();
        self.normalizer.diagnostic_inputs = diagnostic_inputs;
        self.normalizer.schemas = schemas.clone();
    }
    pub(crate) fn fixture(provider: impl Provider + 'a) -> Self {
        let session = ProviderPlan::state_only().session("", None, None);
        let diagnostic_inputs = session.guest_inputs();
        let secrets = session.secrets;
        Self {
            provider: Box::new(provider),
            normalizer: ConfiguredNormalizer {
                diagnostic_inputs,
                route: None,
                provider_name: String::new(),
                ext: Box::new(NoopNormalizer),
                config_secrets: secrets.clone(),
                schemas: crate::schema::SchemaRegistry::new(),
            },
            secrets,
            diagnostics: session.diagnostics,
            schemas: crate::schema::SchemaRegistry::new(),
        }
    }
}

#[cfg(test)]
mod schema_install_tests {
    use super::*;
    use crate::schema::SchemaRegistryError;

    #[test]
    fn schema_install_failure_retains_its_typed_source() {
        let conflict = SchemaRegistryError::ConflictingAssignabilityEdges {
            owner: "host-provider".into(),
        };
        let installation: Result<(), SchemaRegistryError> = Err(conflict.clone());
        let error = installation
            .map_err(event_schema_install_error)
            .expect_err("schema installation failure must remain fatal");
        let mut source = std::error::Error::source(&error);
        let mut found = false;
        while let Some(cause) = source {
            if let Some(actual) = cause.downcast_ref::<SchemaRegistryError>() {
                assert_eq!(actual, &conflict);
                found = true;
            }
            source = cause.source();
        }
        assert!(
            found,
            "the schema conflict must remain reachable through Error::source"
        );
    }

    #[test]
    fn provider_authored_schema_errors_keep_raw_sources_opaque() {
        let context = crate::test_utils::provider_text::context(&["schema-owner-secret"]);
        let provider_error = ProviderError::internal_caused_by(
            "provider schema failure",
            SchemaRegistryError::ConflictingAssignabilityEdges {
                owner: "schema-owner-secret".into(),
            },
        );
        let error = provider_error.with_secrets(context.session().redaction_set());
        assert!(!error.to_string().contains("schema-owner-secret"));
        assert!(std::error::Error::source(&error).is_none());
    }
}
