//! The only ingestion boundary for a configured provider instance.
use super::RedactionSet;
use crate::provider::*;
#[cfg(test)]
use crate::resource::{DataSource, Resource};
use crate::resource::{ResourceId, State};

/// Core-minted admission inventory for the complete desired configuration.
/// Obtain it from `ResolvedFile::provider_plan` or a rehydrated saved
/// configuration; resource
/// slices cannot stand in for a whole plan. State-only operations are explicit.
///
/// Caller-built files cannot mint a whole-plan witness.
///
/// ```compile_fail
/// use carina_core::parser::ExpandedFile;
/// use carina_core::resource::Resource;
/// let mut file = ExpandedFile::<()>::default();
/// file.resources.push(Resource::with_provider("aws", "s3.Bucket", "partial", None));
/// let _ = file.provider_plan();
/// ```
#[derive(Clone, Debug)]
pub struct ProviderPlan {
    routes: std::collections::HashMap<ProviderInstanceKey, RedactionSet>,
}
impl ProviderPlan {
    /// No desired configuration exists (for example, state-only cleanup).
    pub fn state_only() -> Self {
        Self {
            routes: Default::default(),
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
            plan.include(&resource.id, resource.attributes.values());
        }
        for source in data_sources {
            plan.include(&source.id, source.attributes.values());
        }
        plan
    }
    pub(crate) fn include<'a, P: crate::resource::ValueKind + 'a>(
        &mut self,
        id: &ResourceId,
        values: impl IntoIterator<Item = &'a crate::resource::Value<P>>,
    ) {
        self.routes
            .entry(ProviderInstanceKey::from(id))
            .or_insert_with(RedactionSet::before_request)
            .extend(&RedactionSet::collect(values));
    }
    /// Mint one configured or built-in provider session from this whole plan.
    pub fn session(
        &self,
        provider: &str,
        binding: Option<&str>,
        config: Option<&ProviderReadyConfig>,
    ) -> ProviderSession {
        ProviderSession::new(self, provider, binding, config)
    }
}

/// Core-minted inputs shared by a provider and its normalizer.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderSession {
    config: Option<ProviderReadyConfig>,
    provider: String,
    binding: Option<String>,
    secrets: RedactionSet,
}
impl ProviderSession {
    /// Core-owned call inputs for guest handoff; this does not grant runtime
    /// initialization authority to a validation input.
    pub fn guest_inputs(&self) -> ProviderValidationInputs {
        ProviderValidationInputs {
            secrets: self.secrets.clone(),
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
        Self {
            config: config.cloned(),
            provider: provider.to_owned(),
            binding: binding.map(str::to_owned),
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
}

/// Inputs for validation before a checked configuration exists. This type
/// cannot initialize a provider boundary or become a runtime session.
#[derive(Clone, Debug)]
pub struct ProviderValidationInputs {
    secrets: RedactionSet,
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
        values: impl IntoIterator<Item = &'a crate::resource::Value<P, B>>,
    ) -> Self {
        Self {
            secrets: RedactionSet::collect(values),
        }
    }
    pub fn redaction_set(&self) -> &RedactionSet {
        &self.secrets
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
        let (provider, outcome) = factory
            .create_provider(session, InitializeOutcomeMint::new())
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
            }),
            reference_scope: outcome.reference_scope,
        })
    }
    pub async fn initialize_normalizer(
        factory: &dyn ProviderFactory,
        session: &crate::provider::ProviderSession,
    ) -> HostResult<ConfiguredNormalizer> {
        let ext = factory
            .create_normalizer(session)
            .await
            .map_err(|error| error.with_secrets(&session.secrets))?;
        let mut schemas = crate::schema::SchemaRegistry::new();
        let witness = factory.schema_witness();
        schemas
            .insert_provider_witness(factory.name(), &witness)
            .map_err(|error| crate::provider::HostError::internal(error.to_string()))?;
        if session.provider != factory.name() {
            schemas
                .insert_provider_witness(&session.provider, &witness)
                .map_err(|error| crate::provider::HostError::internal(error.to_string()))?;
        }
        Ok(ConfiguredNormalizer {
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
        self.provider.name()
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, HostResult<ReadOutcome>> {
        let secrets = self.secrets.clone();
        let call = self.provider.read(id, identifier, request);
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, HostResult<State>> {
        self.secrets
            .extend(&RedactionSet::collect(resource.diagnostic_values()));
        let secrets = self.secrets.clone();
        let call = self.provider.read_data_source(resource);
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<HostCreateOutcome, HostCreateFailure>> {
        self.secrets.extend(&RedactionSet::collect(
            request.resource().diagnostic_values(),
        ));
        let secrets = self.secrets.clone();
        let sent = SentAttributes::from_resource(request.resource().as_resource());
        let call = self.provider.create(id, request, attempt);
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
        self.secrets.extend(&RedactionSet::collect(
            request.request().resource().diagnostic_values(),
        ));
        let secrets = self.secrets.clone();
        let sent = SentAttributes::from_resource(request.request().resource().as_resource());
        let call = self.provider.resume_create(request);
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
    ) -> BoxFuture<'_, HostResult<HostUpdateOutcome>> {
        self.secrets
            .extend(&RedactionSet::collect(request.diagnostic_values()));
        let secrets = self.secrets.clone();
        let id = id.clone();
        let call = self.provider.update(&id, identifier, request);
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

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: DeleteRequest,
    ) -> BoxFuture<'_, HostResult<()>> {
        let secrets = self.secrets.clone();
        let call = self.provider.delete(id, identifier, request);
        Box::pin(async move { call.await.map_err(|error| error.with_secrets(&secrets)) })
    }

    fn required_permissions(
        &self,
        id: &ResourceId,
        op: crate::effect::PlanOp,
    ) -> HostResult<Vec<super::RequiredPermission>> {
        self.provider
            .required_permissions(id, op)
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
        self.provider
            .satisfier_hint(id, path)
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
            self.provider
                .resolve_references(requests)
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
        self.secrets = session.secrets;
        self.schemas = schemas.clone();
        self.normalizer.config_secrets = self.secrets.clone();
        self.normalizer.schemas = schemas.clone();
    }
    pub(crate) fn fixture(provider: impl Provider + 'a) -> Self {
        let secrets = ProviderPlan::state_only().session("", None, None).secrets;
        Self {
            provider: Box::new(provider),
            normalizer: ConfiguredNormalizer {
                route: None,
                provider_name: String::new(),
                ext: Box::new(NoopNormalizer),
                config_secrets: secrets.clone(),
                schemas: crate::schema::SchemaRegistry::new(),
            },
            secrets,
            schemas: crate::schema::SchemaRegistry::new(),
        }
    }
}
