//! Confinement for generated bindings and freshly decoded guest values.
use super::diagnostics::{
    MetadataWasmChain, WasmNormalizerResultCountError, WasmNormalizerResultKeysError,
};
use super::*;
mod desired_normalization;
pub(super) mod guest_stderr;
pub(super) mod guest_store;
mod wasm_convert;
use version_checked_bindings::wit_types;
pub(super) use version_checked_bindings::{SchemaOnlyBindings, WasmBindings};

// Raw permission text stays opaque until core admits it at ProviderBoundary.
struct UnboundPermissions(Vec<String>);

mod version_checked_bindings {
    use super::*;

    mod basic {
        wasmtime::component::bindgen!({
            path: "../carina-plugin-wit/wit",
            world: "carina-provider",
            require_store_data_send: true,
            exports: { default: async },
        });
    }

    mod http {
        wasmtime::component::bindgen!({
            path: "../carina-plugin-wit/wit",
            world: "carina-provider-with-http",
            require_store_data_send: true,
            exports: { default: async },
            with: {
                "carina:provider/types": super::basic::carina::provider::types,
                "carina:provider/provider": super::basic::exports::carina::provider::provider,
                "wasi:http": wasmtime_wasi_http::p2::bindings::http,
                "wasi:io": wasmtime_wasi::p2::bindings::io,
            },
        });
    }

    mod aws {
        wasmtime::component::bindgen!({
            path: "../carina-plugin-wit/wit",
            world: "carina-provider-aws",
            require_store_data_send: true,
            imports: { default: async },
            exports: { default: async },
            with: {
                "carina:provider/types": super::basic::carina::provider::types,
                "carina:provider/provider": super::basic::exports::carina::provider::provider,
                "wasi:http": wasmtime_wasi_http::p2::bindings::http,
                "wasi:io": wasmtime_wasi::p2::bindings::io,
            },
        });
    }

    pub(crate) use aws::carina::provider::aws_credentials;
    pub(crate) use basic::carina::provider::types as wit_types;
    pub(crate) use basic::exports::carina::provider::provider::{
        DesiredResource as WitDesiredResource, PlanOp as WitPlanOp,
    };

    use basic::CarinaProvider;
    use http::CarinaProviderWithHttp;

    const PROVIDER_INTERFACE_EXPORT: &str = "carina:provider/provider@0.1.0";

    // Wasmtime 43 generates types owned by an exported interface separately
    // for each world, even when `with` shares that interface for referenced
    // types. Keep the unavoidable world-to-world conversion private here.
    fn http_plan_op(operation: WitPlanOp) -> http::exports::carina::provider::provider::PlanOp {
        use http::exports::carina::provider::provider::PlanOp as HttpPlanOp;

        match operation {
            WitPlanOp::Create => HttpPlanOp::Create,
            WitPlanOp::Read => HttpPlanOp::Read,
            WitPlanOp::Update => HttpPlanOp::Update,
            WitPlanOp::Delete => HttpPlanOp::Delete,
        }
    }

    /// Raw typed bindings are private to this module. The parent module can
    /// obtain [`WasmBindings`] only through `instantiate_basic` or
    /// `instantiate_http`, both of which perform the stable `info` preflight
    /// and protocol-version check before constructing these bindings.
    enum RawWasmBindings {
        Basic(CarinaProvider),
        Http(CarinaProviderWithHttp),
    }

    pub(in crate::wasm_factory) struct WasmBindings {
        raw: RawWasmBindings,
        info_json: String,
    }

    struct ProtocolCheckedInstance {
        instance: wasmtime::component::Instance,
        info_json: String,
    }

    /// Dynamically addressed exports for read-only schema loading.
    ///
    /// Keeping the instance unbound avoids runtime initialization and CRUD
    /// binding construction. Metadata uses the same current-protocol gate
    /// and strict schema decoders as [`WasmBindings`].
    pub(in crate::wasm_factory) struct SchemaOnlyBindings {
        instance: wasmtime::component::Instance,
        info_json: String,
    }

    impl ProtocolCheckedInstance {
        async fn instantiate(
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let pre = linker
                .instantiate_pre(component)
                .map_err(WasmBindingCreationError::Instantiation)?;
            let instance = store
                .call(|store| pre.instantiate_async(store))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let info = store
                .call(|store| std::future::ready(untyped_info_func(store, &instance)))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let (info_json,) = store
                .call(|store| info.call_async(store, ()))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
                .map_err(WasmBindingCreationError::InfoCall)?;
            wasm_convert::check_protocol_version(&info_json)
                .map_err(WasmBindingCreationError::ProtocolVersion)?;
            Ok(Self {
                instance,
                info_json,
            })
        }
    }

    fn untyped_info_func(
        store: &mut Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> wasmtime::Result<wasmtime::component::TypedFunc<(), (String,)>> {
        untyped_string_func(store, instance, "info")
    }

    fn untyped_string_func(
        store: &mut Store<HostState>,
        instance: &wasmtime::component::Instance,
        export: &str,
    ) -> wasmtime::Result<wasmtime::component::TypedFunc<(), (String,)>> {
        let provider = instance
            .get_export_index(&mut *store, None, PROVIDER_INTERFACE_EXPORT)
            .ok_or_else(|| {
                wasmtime::Error::msg(
                    [
                        "no exported instance named `",
                        PROVIDER_INTERFACE_EXPORT,
                        "`",
                    ]
                    .concat(),
                )
            })?;
        let function = instance
            .get_export_index(&mut *store, Some(&provider), export)
            .ok_or_else(|| {
                wasmtime::Error::msg(
                    [
                        "instance export `",
                        PROVIDER_INTERFACE_EXPORT,
                        "` does not have export `",
                        export,
                        "`",
                    ]
                    .concat(),
                )
            })?;
        instance.get_typed_func::<(), (String,)>(&mut *store, &function)
    }

    fn untyped_identity_attributes_func(
        store: &mut Store<HostState>,
        instance: &wasmtime::component::Instance,
    ) -> wasmtime::Result<wasmtime::component::TypedFunc<(), (Vec<String>,)>> {
        let provider = instance
            .get_export_index(&mut *store, None, PROVIDER_INTERFACE_EXPORT)
            .ok_or_else(|| {
                wasmtime::Error::msg(
                    [
                        "no exported instance named `",
                        PROVIDER_INTERFACE_EXPORT,
                        "`",
                    ]
                    .concat(),
                )
            })?;
        let function = instance
            .get_export_index(&mut *store, Some(&provider), "identity-attributes")
            .ok_or_else(|| {
                wasmtime::Error::msg(
                    [
                        "instance export `",
                        PROVIDER_INTERFACE_EXPORT,
                        "` does not have export `identity-attributes`",
                    ]
                    .concat(),
                )
            })?;
        instance.get_typed_func::<(), (Vec<String>,)>(&mut *store, &function)
    }

    impl SchemaOnlyBindings {
        pub(in crate::wasm_factory) async fn instantiate(
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let pre = linker
                .instantiate_pre(component)
                .map_err(WasmBindingCreationError::Instantiation)?;
            let instance = store
                .call(|store| pre.instantiate_async(store))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let info = store
                .call(|store| std::future::ready(untyped_info_func(store, &instance)))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            let (info_json,) = store
                .call(|store| info.call_async(store, ()))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "info",
                    source,
                })?
                .map_err(WasmBindingCreationError::InfoCall)?;
            wasm_convert::check_protocol_version(&info_json)
                .map_err(WasmBindingCreationError::ProtocolVersion)?;
            Ok(Self {
                instance,
                info_json,
            })
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) fn info_json(&self) -> &str {
            &self.info_json
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_string_export(
            &self,
            store: &mut Store<HostState>,
            export: &'static str,
        ) -> wasmtime::Result<String> {
            let function = untyped_string_func(store, &self.instance, export)?;
            let (value,) = function.call_async(store, ()).await?;
            Ok(value)
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_identity_attributes(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<Vec<String>> {
            let function = untyped_identity_attributes_func(store, &self.instance)?;
            let (value,) = function.call_async(store, ()).await?;
            Ok(value)
        }
    }

    impl WasmBindings {
        pub(in crate::wasm_factory) async fn instantiate_basic(
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = store
                .call(|store| std::future::ready(CarinaProvider::new(store, &checked.instance)))
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            Ok(Self {
                raw: RawWasmBindings::Basic(bindings),
                info_json: checked.info_json,
            })
        }

        pub(in crate::wasm_factory) async fn instantiate_http(
            store: &mut GuestStore,
            component: &Component,
            linker: &Linker<HostState>,
        ) -> Result<Self, WasmBindingCreationError> {
            let checked = ProtocolCheckedInstance::instantiate(store, component, linker).await?;
            let bindings = store
                .call(|store| {
                    std::future::ready(CarinaProviderWithHttp::new(store, &checked.instance))
                })
                .await
                .map_err(|source| WasmBindingCreationError::WallBudgetExceeded {
                    operation: "instantiate",
                    source,
                })?
                .map_err(WasmBindingCreationError::Instantiation)?;
            Ok(Self {
                raw: RawWasmBindings::Http(bindings),
                info_json: checked.info_json,
            })
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) fn info_json(&self) -> &str {
            &self.info_json
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_schemas(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => b.carina_provider_provider().call_schemas(store).await,
                RawWasmBindings::Http(b) => b.carina_provider_provider().call_schemas(store).await,
            }
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_provider_config_attribute_types(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_attribute_types(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_attribute_types(store)
                        .await
                }
            }
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_provider_config_completions(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<String> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_completions(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_provider_config_completions(store)
                        .await
                }
            }
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_identity_attributes(
            &self,
            store: &mut Store<HostState>,
        ) -> wasmtime::Result<Vec<String>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_identity_attributes(store)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_identity_attributes(store)
                        .await
                }
            }
        }

        pub(super) async fn call_validate_config(
            &self,
            store: &mut Store<HostState>,
            attrs: &[(String, wit_types::Value)],
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_validate_config(store, attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_validate_config(store, attrs)
                        .await
                }
            }
        }

        pub(super) async fn call_resolve_references(
            &self,
            store: &mut Store<HostState>,
            requests: &[wit_types::ReferenceRequest],
        ) -> wasmtime::Result<
            Result<
                Vec<Result<wit_types::ReferenceOutcome, wit_types::ReferenceError>>,
                wit_types::ProviderError,
            >,
        > {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_resolve_references(store, requests)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_resolve_references(store, requests)
                        .await
                }
            }
        }

        pub(super) async fn call_validate_custom_type(
            &self,
            store: &mut Store<HostState>,
            identity: &wit_types::TypeIdentity,
            value: &str,
        ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_validate_custom_type(store, identity, value)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_validate_custom_type(store, identity, value)
                        .await
                }
            }
        }

        pub(super) async fn call_initialize(
            &self,
            store: &mut Store<HostState>,
            attrs: &[(String, wit_types::Value)],
        ) -> wasmtime::Result<Result<wit_types::InitializeResult, wit_types::ProviderError>>
        {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_initialize(store, attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_initialize(store, attrs)
                        .await
                }
            }
        }

        pub(super) async fn call_read(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: wit_types::ReadRequest,
        ) -> wasmtime::Result<Result<wit_types::ReadOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_read(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_read(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_read_data_source(
            &self,
            store: &mut Store<HostState>,
            resource: &wit_types::ResourceDef,
        ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_read_data_source(store, resource)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_read_data_source(store, resource)
                        .await
                }
            }
        }

        pub(super) async fn call_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
        ) -> wasmtime::Result<Result<wit_types::CreateStep, wit_types::CreateVerdict>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_create(store, id, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_create(store, id, request)
                        .await
                }
            }
        }

        pub(super) async fn call_poll_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
            pending: &wit_types::PendingCreate,
            poll: &wit_types::PollRequest,
        ) -> wasmtime::Result<Result<wit_types::CreateStep, wit_types::PollVerdict>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_create(store, id, request, pending, *poll)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_create(store, id, request, pending, *poll)
                        .await
                }
            }
        }

        pub(super) async fn call_resume_create(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            request: &wit_types::CreateRequest,
            pending: &wit_types::ResumePending,
            poll: &wit_types::PollRequest,
        ) -> wasmtime::Result<Result<wit_types::ResumeStep, wit_types::ResumeVerdict>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_resume_create(store, id, request, pending, *poll)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_resume_create(store, id, request, pending, *poll)
                        .await
                }
            }
        }

        pub(super) async fn call_update(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: &wit_types::UpdateRequest,
        ) -> wasmtime::Result<Result<wit_types::UpdateOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_update(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_update(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_poll_update(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: &wit_types::UpdateRequest,
            pending: &wit_types::PendingUpdate,
            poll: &wit_types::PollRequest,
        ) -> wasmtime::Result<Result<wit_types::UpdateOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_update(store, id, identifier, request, pending, *poll)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_update(store, id, identifier, request, pending, *poll)
                        .await
                }
            }
        }

        pub(super) async fn call_delete(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: wit_types::DeleteRequest,
        ) -> wasmtime::Result<Result<wit_types::DeleteOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_delete(store, id, identifier, request)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_delete(store, id, identifier, request)
                        .await
                }
            }
        }

        pub(super) async fn call_poll_delete(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            identifier: &str,
            request: wit_types::DeleteRequest,
            pending: &wit_types::PendingDelete,
        ) -> wasmtime::Result<Result<wit_types::DeleteOutcome, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_poll_delete(store, id, identifier, request, pending)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_poll_delete(store, id, identifier, request, pending)
                        .await
                }
            }
        }

        pub(super) async fn call_required_permissions(
            &self,
            store: &mut Store<HostState>,
            id: &wit_types::ResourceId,
            operation: PlanOp,
        ) -> wasmtime::Result<UnboundPermissions> {
            let operation = wasm_convert::core_to_wit_plan_op(operation);
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_required_permissions(store, id, operation)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_required_permissions(store, id, http_plan_op(operation))
                        .await
                }
            }
            .map(UnboundPermissions)
        }

        pub(super) async fn call_satisfier_hint(
            &self,
            store: &mut Store<HostState>,
            target_id: &wit_types::ResourceId,
            attr_path: &[String],
        ) -> wasmtime::Result<Vec<wit_types::BindingPattern>> {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_satisfier_hint(store, target_id, attr_path)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_satisfier_hint(store, target_id, attr_path)
                        .await
                }
            }
        }

        pub(super) async fn call_normalize_desired(
            &self,
            store: &mut Store<HostState>,
            resources: &[WitDesiredResource],
        ) -> wasmtime::Result<Result<Vec<WitDesiredResource>, wit_types::ProviderError>> {
            match &self.raw {
                RawWasmBindings::Basic(bindings) => {
                    bindings
                        .carina_provider_provider()
                        .call_normalize_desired(store, resources)
                        .await
                }
                RawWasmBindings::Http(bindings) => {
                    bindings
                        .carina_provider_provider()
                        .call_normalize_desired(store, resources)
                        .await
                }
            }
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_normalize_state(
            &self,
            store: &mut Store<HostState>,
            states: &[(String, wit_types::PersistedState)],
        ) -> wasmtime::Result<
            Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError>,
        > {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_normalize_state(store, states)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_normalize_state(store, states)
                        .await
                }
            }
        }

        // confinement-allow-return: Internal binding adapter visible only to its confined parent; raw results are decoded before runtime return.

        pub(super) async fn call_hydrate_read_state(
            &self,
            store: &mut Store<HostState>,
            states: &[(String, wit_types::PersistedState)],
            saved_attrs: &[(String, Vec<(String, wit_types::PersistedValue)>)],
        ) -> wasmtime::Result<
            Result<Vec<(String, wit_types::PersistedState)>, wit_types::ProviderError>,
        > {
            match &self.raw {
                RawWasmBindings::Basic(b) => {
                    b.carina_provider_provider()
                        .call_hydrate_read_state(store, states, saved_attrs)
                        .await
                }
                RawWasmBindings::Http(b) => {
                    b.carina_provider_provider()
                        .call_hydrate_read_state(store, states, saved_attrs)
                        .await
                }
            }
        }
    }
}

impl version_checked_bindings::aws_credentials::Host for HostState {
    async fn current_credentials(
        &mut self,
    ) -> Result<
        version_checked_bindings::aws_credentials::AwsCredentials,
        version_checked_bindings::aws_credentials::AwsCredentialError,
    > {
        use version_checked_bindings::aws_credentials::{
            AwsCredentialError as WitError, AwsCredentials as WitCredentials,
        };

        let source = match self.network() {
            GuestNetwork::SchemaOnly => {
                return Err(WitError::ProviderFailed(
                    "credentials are not available while loading schemas".to_string(),
                ));
            }
            GuestNetwork::Runtime {
                capabilities: RuntimeCapabilities::Aws { credentials },
                egress: _,
            } => Arc::clone(credentials),
            GuestNetwork::Runtime {
                capabilities: RuntimeCapabilities::GitHub | RuntimeCapabilities::Untrusted,
                egress: _,
            } => {
                return Err(WitError::ProviderFailed(
                    "AWS credentials are not available to this provider kind".to_string(),
                ));
            }
        };

        source
            .current()
            .await
            .and_then(|credentials| {
                let expires_at = credentials
                    .expires_at()
                    .map(|expires_at| {
                        expires_at
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|duration| duration.as_secs())
                            .map_err(|_| {
                                AwsCredentialError::ProviderFailed(
                                    "AWS credential expiry predates the Unix epoch".to_string(),
                                )
                            })
                    })
                    .transpose()?;
                Ok(WitCredentials {
                    access_key_id: guest_store::handoff_credential(
                        self,
                        credentials.access_key_id(),
                    ),
                    secret_access_key: guest_store::handoff_credential(
                        self,
                        credentials.secret_access_key(),
                    ),
                    session_token: credentials
                        .session_token()
                        .map(|value| guest_store::handoff_credential(self, value)),
                    expires_at,
                })
            })
            .map_err(|error| match error {
                AwsCredentialError::NotFound(message) => WitError::NotFound(message),
                AwsCredentialError::ProviderFailed(message) => WitError::ProviderFailed(message),
            })
    }
}

pub(super) fn add_aws_credentials_to_linker(
    linker: &mut Linker<HostState>,
    network: &GuestNetwork,
) -> wasmtime::Result<()> {
    let should_link = matches!(
        network,
        GuestNetwork::SchemaOnly
            | GuestNetwork::Runtime {
                capabilities: RuntimeCapabilities::Aws { credentials: _ },
                egress: _,
            }
    );
    if should_link {
        version_checked_bindings::aws_credentials::add_to_linker::<
            _,
            wasmtime::component::HasSelf<_>,
        >(linker, |state| state)?;
    }
    Ok(())
}

impl ValidationContextFingerprint {
    pub(super) fn validate_config(
        key: &[u8; 32],
        attributes: &[(String, wit_types::Value)],
    ) -> Self {
        let operation = ValidationOperation::ValidateConfig;
        let mut hasher = validation_fingerprint_hasher(key, operation);
        let mut attributes = attributes.iter().collect::<Vec<_>>();
        attributes.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for (name, value) in attributes {
            hash_validation_bytes(&mut hasher, name.as_bytes());
            hash_wit_validation_value(&mut hasher, value);
        }
        Self {
            operation,
            keyed_input_sha256: hasher.finalize().into_bytes().into(),
        }
    }

    pub(super) fn validate_custom_type(
        key: &[u8; 32],
        identity: &wit_types::TypeIdentity,
        value: &str,
    ) -> Self {
        let operation = ValidationOperation::ValidateCustomType;
        let mut hasher = validation_fingerprint_hasher(key, operation);
        hash_validation_bytes(&mut hasher, identity.provider.as_bytes());
        let segment_count = u64::try_from(identity.segments.len())
            .expect("validation type segment count must fit in u64");
        hasher.update(&segment_count.to_be_bytes());
        for segment in &identity.segments {
            hash_validation_bytes(&mut hasher, segment.as_bytes());
        }
        hash_validation_bytes(&mut hasher, identity.kind.as_bytes());
        hash_validation_bytes(&mut hasher, value.as_bytes());
        Self {
            operation,
            keyed_input_sha256: hasher.finalize().into_bytes().into(),
        }
    }

    // confinement-allow-return: Returns a host-authored static operation label.

    pub(super) const fn operation(&self) -> &'static str {
        self.operation.as_str()
    }

    pub(super) const fn suspended_validation_id(
        &self,
    ) -> carina_core::provider::SuspendedValidationId {
        carina_core::provider::SuspendedValidationId::from_keyed_fingerprint(
            self.keyed_input_sha256,
        )
    }
}

fn hash_wit_validation_value(hasher: &mut ValidationFingerprintHasher, value: &wit_types::Value) {
    match value {
        wit_types::Value::BoolVal(value) => {
            hasher.update(&[0]);
            hasher.update(&[u8::from(*value)]);
        }
        wit_types::Value::IntVal(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        }
        wit_types::Value::FloatVal(value) => {
            hasher.update(&[2]);
            hasher.update(&value.to_bits().to_be_bytes());
        }
        wit_types::Value::StrVal(value) => {
            hasher.update(&[3]);
            hash_validation_bytes(hasher, value.as_bytes());
        }
        wit_types::Value::StringListVal(value) => {
            hasher.update(&[4]);
            hash_validation_bytes(hasher, value.as_bytes());
        }
        wit_types::Value::ListVal(value) => {
            hasher.update(&[5]);
            hash_validation_bytes(hasher, value.as_bytes());
        }
        wit_types::Value::MapVal(value) => {
            hasher.update(&[6]);
            hash_validation_bytes(hasher, value.as_bytes());
        }
        wit_types::Value::SecretVal(value) => {
            hasher.update(&[7]);
            hash_validation_bytes(hasher, value.as_bytes());
        }
    }
}

impl WasmDiagnostic {
    pub(super) fn new(error: wasmtime::Error, inline: bool) -> Self {
        let text = Self::capture_chain(&error, inline);
        let redacted = DiagnosticText::new(
            MetadataDiagnostic::new(MetadataWasmChain { error, inline }).to_string(),
        );
        Self {
            text,
            redacted: Some(redacted),
        }
    }
    fn capture_chain(error: &wasmtime::Error, inline: bool) -> ProviderText {
        let mut text = ProviderText::default();
        for (index, cause) in error.chain().enumerate() {
            if index > 0 {
                text.append_host(if inline { ": " } else { "\n  caused by: " });
            }
            text.append(&ProviderText::new(cause.to_string()));
        }
        text
    }
}

impl PoisonReason {
    pub(super) fn from_trap<'a>(
        operation: &'static str,
        error: &wasmtime::Error,
        secrets: impl Into<Option<&'a RedactionSet>>,
    ) -> Self {
        let secrets = secrets.into();
        let root_cause = error.root_cause().to_string();
        let root_cause = root_cause.lines().next().unwrap_or("unknown WASM trap");
        let trap = error.downcast_ref::<wasmtime::Trap>().copied();
        let root_is_trap = trap.is_some_and(|trap| trap.to_string() == root_cause);
        Self::Trap {
            operation,
            trap,
            root_cause: root_cause.to_owned(),
            secrets: secrets.cloned(),
            before_request_root: secrets.is_none().then(|| DiagnosticText::new(root_cause)),
            root_is_trap,
        }
    }

    pub(super) fn opaque_text(&self) -> ProviderText {
        let text = ProviderText::new(self.capture_text());
        match self {
            Self::Trap {
                secrets: Some(secrets),
                ..
            } => text.with_redaction(secrets),
            _ => text,
        }
    }
    pub(super) fn fail_fast_error(&self, operation: &'static str) -> ProviderError {
        let mut text = self.opaque_text();
        text.append_host([" (operation '", operation, "'); re-run the command"].concat());
        ProviderError::internal(text)
    }
}

pub(super) fn wasm_trap_provider_error(
    operation: &'static str,
    error: wasmtime::Error,
) -> ProviderError {
    let error = match error.downcast::<EgressDenied>() {
        Ok(denial) => {
            return ProviderError::internal_caused_by(
                "WASM guest call stopped by the host egress policy",
                EgressDeniedCall { operation, denial },
            );
        }
        Err(error) => error,
    };
    let timed_out = error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt);
    let (context, call_context) = if timed_out {
        (
            "WASM plugin timed out",
            WasmGuestCallContext::ComputeBudgetExceeded { operation },
        )
    } else {
        (
            "WASM guest trapped",
            WasmGuestCallContext::Trap { operation },
        )
    };
    let cause = WasmGuestCallError {
        context: Some(call_context),
        source: error,
    };
    if timed_out {
        ProviderError::timeout_caused_by(context, cause)
    } else {
        ProviderError::internal_caused_by(context, cause)
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub fn decode_initialize_string_error_for_test(message: impl Into<String>) -> ProviderError {
    let wit_error = wit_types::ProviderError::InvalidInput(wit_types::ErrorDetail {
        message: message.into(),
        resource_id: None,
        cause: None,
        provider_name: None,
        operation: None,
        status: None,
        code: None,
        request_id: None,
    });
    let decoded = wasm_convert::decode_provider_initialization_error(
        wasm_convert::wit_to_wire_provider_error(wit_error),
    );
    let mapper: ProviderInstantiationErrorMapper =
        Arc::new(default_provider_instantiation_error_mapper);
    WasmProviderInstanceError::Initialize(decoded).into_provider_error_with(&mapper)
}

pub(super) fn decode_config_completions_json(
    json: &str,
    provider_name: &str,
    provider_version: &str,
) -> Result<ProviderConfigCompletions, WasmProviderLoadError> {
    serde_json::from_str(json).map_err(|source| {
        WasmProviderLoadError::metadata(
            [
                "provider '",
                provider_name,
                "' ",
                provider_version,
                " emitted malformed provider config completions JSON",
            ]
            .concat(),
            source,
        )
    })
}

impl WasmProviderFactory {
    /// Load provider metadata from WIT functions.
    ///
    /// Every export and decode is required to succeed. Substituting empty
    /// metadata here changes provider behavior, including resource identity.
    async fn load_metadata(
        bindings: &WasmBindings,
        store: &mut GuestStore,
        provider_name: &str,
        provider_version: &str,
    ) -> Result<
        (
            ProviderConfigCompletions,
            Vec<String>,
            ValidatedConfigSchema,
        ),
        WasmProviderLoadError,
    > {
        let config_completions_json = store
            .call(|store| bindings.call_provider_config_completions(store))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-completions", source))?
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "provider-config-completions",
                    source,
                )
            })?;
        let config_completions = decode_config_completions_json(
            &config_completions_json,
            provider_name,
            provider_version,
        )?;

        let identity_attributes = store
            .call(|store| bindings.call_identity_attributes(store))
            .await
            .map_err(|source| wall_budget_load_error("identity-attributes", source))?
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "identity-attributes",
                    source,
                )
            })?;

        let provider_config_types_json = store
            .call(|store| bindings.call_provider_config_attribute_types(store))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-attribute-types", source))?
            .map_err(|source| {
                metadata_call_error(
                    provider_name,
                    provider_version,
                    "provider-config-attribute-types",
                    source,
                )
            })?;
        let provider_config_types = wasm_convert::json_to_attribute_types(
            &provider_config_types_json,
        )
        .map_err(|source| {
            WasmProviderLoadError::metadata(
                [
                    "provider '",
                    provider_name,
                    "' ",
                    provider_version,
                    " emitted provider config attribute types this host cannot decode",
                ]
                .concat(),
                source,
            )
        })?;

        Ok((
            config_completions,
            identity_attributes,
            provider_config_types,
        ))
    }

    /// Instantiate a component and load all provider metadata shared by the
    /// cached and uncached acquisition paths.
    pub(super) async fn bring_up_component(
        engine: Engine,
        component: Component,
        epoch_ticker: EpochTicker,
        configured: ConfiguredProvider,
        network_request: GuestNetworkRequest<'_>,
    ) -> Result<Self, WasmProviderLoadError> {
        let budget = GuestCallBudget::PRODUCTION;
        let component_imports = ComponentImports::from_component(&engine, &component);
        let runtime_network = match network_request {
            GuestNetworkRequest::SchemaOnly => GuestNetwork::SchemaOnly,
            GuestNetworkRequest::Runtime { egress, aws } => {
                runtime_network(&configured, &component_imports, egress, aws).await?
            }
        };
        let mut rebuild = RebuildContext::new(
            engine.clone(),
            component.clone(),
            Arc::clone(&configured.label),
            budget,
        );
        let mut live = create_instance_auto(&mut rebuild).await?;
        let enable_http = live.world == InstantiationWorld::HttpEnabled;
        let info_json = live.bindings.info_json().to_owned();

        let (reported_name, display_name, version) =
            wasm_convert::json_to_provider_info(&info_json).map_err(|source| {
                WasmProviderLoadError::metadata("Failed to decode provider info", source)
            })?;
        if reported_name != configured.label.name {
            return Err(WasmProviderLoadError::ProviderNameMismatch {
                configured: configured.label.name.clone().into(),
                reported: reported_name.into(),
            });
        }
        let name = configured.label.name.as_str();

        let schemas_json = live
            .store
            .call(|store| live.bindings.call_schemas(store))
            .await
            .map_err(|source| wall_budget_load_error("schemas", source))?
            .map_err(|error| {
                WasmProviderLoadError::metadata(
                    "Failed to call schemas()",
                    WasmDiagnostic::new(error, true),
                )
            })?;

        let (cached_config_completions, cached_identity_attributes, provider_config_types) =
            Self::load_metadata(&live.bindings, &mut live.store, name, &version.to_string())
                .await?;
        let schema_witness =
            wasm_convert::json_to_provider_schema(&schemas_json, provider_config_types)
                .map_err(|error| provider_schema_decode_error(name, &version.to_string(), error))?;
        let init_instance = RebuildableInstance::new(live, rebuild);

        Ok(Self {
            engine,
            component,
            configured,
            runtime_network,
            budget,
            display_name,
            version,
            schema_witness: Arc::new(schema_witness),
            cached_config_completions,
            cached_identity_attributes,
            enable_http,
            http_test_route: None,
            init_instance,
            shared_instances: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "test-utils"))]
            poll_observer: None,
            instantiation_error_mapper: Arc::new(default_provider_instantiation_error_mapper),
            _epoch_ticker: epoch_ticker,
        })
    }

    pub(super) async fn create_initialized_instance(
        &self,
        config: &ProviderReadyConfig,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> Result<InitializedWasmInstance, WasmProviderInstanceError> {
        // This is the credentialed runtime instance (it calls
        // `initialize` and then read/create/update/delete). The kind was
        // classified from resolver-owned source identity before
        // guest metadata was read, so the guest receives only that partition.
        let world = if self.enable_http {
            InstantiationWorld::HttpEnabled
        } else {
            InstantiationWorld::Basic
        };
        let network = match &self.runtime_network {
            GuestNetwork::SchemaOnly => {
                return Err(WasmProviderInstanceError::MissingEgressConsent);
            }
            GuestNetwork::Runtime {
                capabilities,
                egress,
            } => GuestNetwork::Runtime {
                capabilities: capabilities.clone(),
                egress: egress.clone(),
            },
        };
        let mut live = create_runtime_instance(
            &self.engine,
            &self.component,
            Arc::clone(&self.configured.label),
            network,
            world,
            self.budget,
            self.http_test_route.clone(),
            text,
        )
        .await?;
        let wit_attrs = wasm_convert::checked_attributes_to_wit(config.wire_attributes());
        let initialize_result = live
            .store
            .call(|store| live.bindings.call_initialize(store, &wit_attrs))
            .await
            .map_err(|source| WasmProviderInstanceError::WallBudgetExceeded {
                operation: "initialize",
                source,
            })?
            .map_err(WasmProviderInstanceError::InitializeCall)?
            .map_err(|e| {
                WasmProviderInstanceError::Initialize(
                    wasm_convert::decode_provider_initialization_error(
                        wasm_convert::wit_to_wire_provider_error(e),
                    ),
                )
            })?;
        let wit_types::InitializeResult { reference_scope } = initialize_result;

        Ok(InitializedWasmInstance {
            instance: live,
            reference_scope: wasm_convert::wit_to_core_reference_scope(reference_scope)
                .map_err(WasmProviderInstanceError::InitializeReferenceScope)?,
        })
    }
}

impl WasmProviderFactory {
    pub(super) fn validate_config_in_context(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> ProviderResult<()> {
        let wit_attrs = wasm_convert::checked_attributes_to_wit(attributes.wire_attributes());
        let context = self.init_instance.validate_config_context(&wit_attrs);

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let instance = &self.init_instance;
                let locked = instance.acquire_in_session(context, text).await?;
                let result = locked
                    .call(async |bindings, store| {
                        bindings.call_validate_config(store, &wit_attrs).await
                    })
                    .await?;
                result.map_err(wasm_convert::wit_to_core_provider_error)
            })
        })
    }

    pub(super) fn validate_custom_type_in_context(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        text: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> ProviderResult<()> {
        let wit_identity = wasm_convert::core_type_identity_to_wit(identity);
        let context = self
            .init_instance
            .validate_custom_type_context(&wit_identity, value.as_str());
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let instance = &self.init_instance;
                let locked = instance.acquire_in_session(context, text).await?;
                let result = locked
                    .call(async |bindings, store| {
                        bindings
                            .call_validate_custom_type(store, &wit_identity, value.as_str())
                            .await
                    })
                    .await?;
                result.map_err(wasm_convert::wit_to_core_provider_error)
            })
        })
    }
}

impl SchemaOnlyWasmProviderFactory {
    pub(super) async fn bring_up_component(
        engine: Engine,
        component: Component,
        configured: ConfiguredProvider,
    ) -> Result<Self, WasmProviderLoadError> {
        let _epoch_ticker = EpochTicker::start(engine.clone());
        let (mut store, bindings) =
            create_schema_only_bindings(&engine, &component, Arc::clone(&configured.label)).await?;
        let info_json = bindings.info_json();
        let (reported_name, display_name, version) = wasm_convert::json_to_provider_info(info_json)
            .map_err(|source| {
                WasmProviderLoadError::metadata("Failed to decode provider info", source)
            })?;
        if reported_name != configured.label.name {
            return Err(WasmProviderLoadError::ProviderNameMismatch {
                configured: configured.label.name.clone().into(),
                reported: reported_name.into(),
            });
        }
        let name = configured.label.name.clone();

        let schemas_json = store
            .call(|store| bindings.call_string_export(store, "schemas"))
            .await
            .map_err(|source| wall_budget_load_error("schemas", source))?
            .map_err(|source| {
                metadata_call_error(&name, &version.to_string(), "schemas", source)
            })?;
        let config_completions_json = store
            .call(|store| bindings.call_string_export(store, "provider-config-completions"))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-completions", source))?
            .map_err(|source| {
                metadata_call_error(
                    &name,
                    &version.to_string(),
                    "provider-config-completions",
                    source,
                )
            })?;
        let cached_config_completions =
            decode_config_completions_json(&config_completions_json, &name, &version.to_string())?;
        let cached_identity_attributes = store
            .call(|store| bindings.call_identity_attributes(store))
            .await
            .map_err(|source| wall_budget_load_error("identity-attributes", source))?
            .map_err(|source| {
                metadata_call_error(&name, &version.to_string(), "identity-attributes", source)
            })?;
        let provider_config_types_json = store
            .call(|store| bindings.call_string_export(store, "provider-config-attribute-types"))
            .await
            .map_err(|source| wall_budget_load_error("provider-config-attribute-types", source))?
            .map_err(|source| {
                metadata_call_error(
                    &name,
                    &version.to_string(),
                    "provider-config-attribute-types",
                    source,
                )
            })?;
        let provider_config_types = wasm_convert::json_to_attribute_types(
            &provider_config_types_json,
        )
        .map_err(|source| {
            WasmProviderLoadError::metadata(
                [
                    "provider '",
                    name.as_str(),
                    "' ",
                    &version.to_string(),
                    " emitted provider config attribute types this host cannot decode",
                ]
                .concat(),
                source,
            )
        })?;
        let schema_witness =
            wasm_convert::json_to_provider_schema(&schemas_json, provider_config_types).map_err(
                |error| provider_schema_decode_error(&name, &version.to_string(), error),
            )?;

        Ok(Self {
            name,
            display_name,
            version,
            schema_witness: Arc::new(schema_witness),
            cached_config_completions,
            cached_identity_attributes,
        })
    }
}

impl RebuildableInstance {
    pub(super) fn validate_config_context(
        &self,
        attributes: &[(String, wit_types::Value)],
    ) -> ValidationContextFingerprint {
        ValidationContextFingerprint::validate_config(&self.fingerprint_key, attributes)
    }

    pub(super) fn validate_custom_type_context(
        &self,
        identity: &wit_types::TypeIdentity,
        value: &str,
    ) -> ValidationContextFingerprint {
        ValidationContextFingerprint::validate_custom_type(&self.fingerprint_key, identity, value)
    }
}

pub struct WasmProvider {
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) poll_observer: Option<PollObserverForTest>,
    pub(super) instance: Arc<FailFastInstance>,
    pub(super) name: String,
    pub(super) schema_witness: Arc<ValidatedProviderSchemaWitness>,
}

struct PendingCreate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    wit_request: wit_types::CreateRequest,
    pending: CorePendingCreate,
}

struct PendingCreateParts {
    token: proto::PendingToken,
    progress: proto::CreateProgress,
    sent: Sent,
    poll_after_secs: u32,
}

struct PendingCreateBoundaryError {
    safe_parts: PendingCreateParts,
    error: ProviderError,
}

enum PendingResume {
    Resume {
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        resume: CoreResume,
        poll_after_secs: u32,
        phase: proto::PendingPhase,
    },
    Resent(PendingResentCreate),
}

#[derive(Clone)]
struct ResumePoll {
    poll_after_secs: u32,
    phase: proto::PendingPhase,
    finality: proto::PollFinality,
}

struct PendingResentCreate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    wit_request: wit_types::CreateRequest,
    pending: ResentPendingCreate,
}

impl PendingResume {
    fn poll_after_secs(&self) -> u32 {
        match self {
            Self::Resume {
                id: _id,
                wit_id: _wit_id,
                wit_request: _wit_request,
                resume: _resume,
                poll_after_secs,
                phase: _phase,
            } => *poll_after_secs,
            Self::Resent(pending) => pending.pending.poll_after_secs(),
        }
    }

    fn phase(&self) -> proto::PendingPhase {
        match self {
            Self::Resume { phase, .. } => *phase,
            Self::Resent(pending) => pending.pending.phase(),
        }
    }

    fn into_failure(self, error: ProviderError) -> ResumeFailure {
        match self {
            Self::Resume {
                id: _id,
                wit_id: _wit_id,
                wit_request: _wit_request,
                resume,
                poll_after_secs: _poll_after_secs,
                phase: _phase,
            } => resume.preserve(error),
            Self::Resent(pending) => pending.pending.preserve(error),
        }
    }
}

#[derive(Clone)]
struct PendingUpdate {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    identifier: String,
    wit_request: wit_types::UpdateRequest,
    operation: wasm_convert::UpdatePendingOperation,
}

#[derive(Clone)]
struct PendingDelete {
    id: ResourceId,
    wit_id: wit_types::ResourceId,
    identifier: String,
    wit_request: wit_types::DeleteRequest,
    operation: wasm_convert::DeletePendingOperation,
}

enum UpdateProgress {
    Done(Box<UpdateOutcome>),
    Pending(Box<PendingUpdate>),
}

enum CreateProgress {
    Done(Box<CreateOutcome>),
    Verdict(Box<CreateFailure>),
    Pending(Box<PendingCreate>),
}

enum ResumeProgress {
    Done(ResumeOutcome),
    Verdict(Box<ResumeFailure>),
    Pending(Box<PendingResume>),
}

impl OperationProgress for ResumeProgress {
    type Output = ResumeOutcome;
    type Verdict = Box<ResumeFailure>;
    type PendingState = Box<PendingResume>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Box<ResumeFailure>, Self::PendingState>
    {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(outcome),
            Self::Verdict(verdict) => carina_provider_protocol::OperationStep::Verdict(verdict),
            Self::Pending(pending) => {
                let poll_after_secs = pending.poll_after_secs();
                let phase = pending.phase();
                carina_provider_protocol::OperationStep::Pending {
                    pending,
                    poll_after_secs,
                    phase,
                }
            }
        }
    }
}

impl OperationProgress for CreateProgress {
    type Output = CreateOutcome;
    type Verdict = Box<CreateFailure>;
    type PendingState = Box<PendingCreate>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Box<CreateFailure>, Self::PendingState>
    {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(*outcome),
            Self::Verdict(verdict) => carina_provider_protocol::OperationStep::Verdict(verdict),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.pending.poll_after_secs(),
                phase: pending.pending.phase(),
                pending,
            },
        }
    }
}

enum DeleteProgress {
    Deleted,
    Pending(Box<PendingDelete>),
}

trait OperationProgress {
    type Output;
    type Verdict;
    type PendingState;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Self::Verdict, Self::PendingState>;
}

impl OperationProgress for UpdateProgress {
    type Output = UpdateOutcome;
    type Verdict = std::convert::Infallible;
    type PendingState = Box<PendingUpdate>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<
        Self::Output,
        std::convert::Infallible,
        Self::PendingState,
    > {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(*outcome),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.operation.poll_after_secs(),
                phase: pending.operation.phase(),
                pending,
            },
        }
    }
}

impl OperationProgress for DeleteProgress {
    type Output = ();
    type Verdict = std::convert::Infallible;
    type PendingState = Box<PendingDelete>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<
        Self::Output,
        std::convert::Infallible,
        Self::PendingState,
    > {
        match self {
            Self::Deleted => carina_provider_protocol::OperationStep::Done(()),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.operation.poll_after_secs(),
                phase: proto::PendingPhase::Operation,
                pending,
            },
        }
    }
}

type GuestOperationFuture<'a, T> = Pin<
    Box<dyn Future<Output = wasmtime::Result<Result<T, wit_types::ProviderError>>> + Send + 'a>,
>;

#[derive(Debug, Clone, Copy)]
struct OperationBudgets {
    operation: std::time::Duration,
    read_back: std::time::Duration,
}

impl WasmProvider {
    #[cfg(any(test, feature = "test-utils"))]
    fn observe_poll(&self, event: PollEventForTest) {
        if let Some(observer) = &self.poll_observer {
            observer(event);
        }
    }

    fn poll_sleep(
        &self,
        _operation: &'static str,
        delay: std::time::Duration,
    ) -> tokio::time::Sleep {
        #[cfg(any(test, feature = "test-utils"))]
        self.observe_poll(PollEventForTest::Sleep {
            operation: _operation,
            delay,
        });
        tokio::time::sleep(delay)
    }

    fn operation_budgets(
        &self,
        id: &ResourceId,
        kind: carina_provider_protocol::OperationKind,
    ) -> OperationBudgets {
        let config = self
            .schema_witness
            .resources()
            .iter()
            .find(|schema| schema.as_schema().resource_type() == id.resource_type)
            .and_then(|schema| schema.as_schema().operation_config());
        OperationBudgets {
            operation: config.map_or(
                carina_provider_protocol::DEFAULT_OPERATION_BUDGET,
                |config| config.budget(kind),
            ),
            read_back: config.map_or(
                carina_provider_protocol::DEFAULT_READ_BACK_BUDGET,
                carina_provider_protocol::OperationConfig::read_back_budget,
            ),
        }
    }

    fn over_budget(operation: &'static str, id: ResourceId) -> ProviderError {
        carina_core::provider::HostError::timeout(
            [
                "WASM provider ",
                operation,
                " operation exceeded its schema budget",
            ]
            .concat(),
        )
        .for_resource(id)
        .into()
    }

    fn read_back_protocol_violation(operation: &'static str, id: ResourceId) -> ProviderError {
        carina_core::provider::HostError::internal(["WASM provider ", operation, " violated the read-back polling protocol by returning pending after a final-attempt sent to a read-back continuation or by regressing to operation"].concat())
        .for_resource(id)
        .into()
    }

    // In-flight verdicts retain a continuation but bypass the generic driver's
    // Pending branch, so validate their phase before converting them to failures.
    fn in_flight_poll_error(
        operation: &'static str,
        id: &ResourceId,
        previous_phase: proto::PendingPhase,
        next_phase: proto::PendingPhase,
        error: ProviderError,
    ) -> ProviderError {
        if previous_phase == proto::PendingPhase::ReadBack
            && next_phase == proto::PendingPhase::Operation
        {
            Self::read_back_protocol_violation(operation, id.clone())
        } else {
            error
        }
    }

    async fn call_operation<WitOutcome, Progress, DecodeError, GuestCall, Decode>(
        &self,
        operation: &'static str,
        id: ResourceId,
        secrets: Option<carina_core::provider::ProviderValidationInputs>,
        guest_call: GuestCall,
        decode: Decode,
    ) -> ProviderResult<Progress>
    where
        WitOutcome: 'static,
        GuestCall: for<'a> FnOnce(
            &'a WasmBindings,
            &'a mut Store<HostState>,
        ) -> GuestOperationFuture<'a, WitOutcome>,
        Decode: FnOnce(WitOutcome, ResourceId) -> Result<Progress, DecodeError>,
        DecodeError: std::error::Error + Send + Sync + 'static,
    {
        let locked = self.instance.acquire(operation).await?;
        let locked = if let Some(secrets) = secrets {
            locked.with_secrets(secrets)
        } else {
            locked
        };
        let outcome = locked
            .call(operation, async move |bindings, store| {
                guest_call(bindings, store).await
            })
            .await?;
        match outcome {
            Ok(outcome) => decode(outcome, id.clone()).map_err(|error| {
                wasm_value_decode_provider_error(operation, error).for_resource(id)
            }),
            Err(error) => Err(wasm_convert::wit_to_core_provider_error(error)),
        }
    }

    async fn drive_operation<Progress, Initial, Poll, PollFuture>(
        &self,
        operation: &'static str,
        id: ResourceId,
        budgets: OperationBudgets,
        initial: Initial,
        mut poll: Poll,
    ) -> ProviderResult<carina_provider_protocol::CompletedOperation<Progress::Output>>
    where
        Progress: OperationProgress<Verdict = std::convert::Infallible>,
        Progress::PendingState: Clone,
        Initial: Future<Output = ProviderResult<Progress>>,
        Poll: FnMut(Progress::PendingState, proto::PollFinality) -> PollFuture,
        PollFuture: Future<Output = ProviderResult<Progress>>,
    {
        let outcome = carina_provider_protocol::drive_pending(
            budgets.operation,
            budgets.read_back,
            async move { initial.await.map(OperationProgress::into_step) },
            |pending, finality| {
                #[cfg(any(test, feature = "test-utils"))]
                self.observe_poll(PollEventForTest::Attempt { operation });
                let next = poll(pending, finality);
                async move { next.await.map(OperationProgress::into_step) }
            },
            |delay| self.poll_sleep(operation, delay),
        )
        .await
        .map_err(|error| match error {
            carina_provider_protocol::DriveError::Initial(error) => error,
            carina_provider_protocol::DriveError::Poll {
                error,
                last_pending: _last_pending,
            } => error,
        })?;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(output) => Ok(output),
            carina_provider_protocol::DriveOutcome::Verdict(verdict) => match verdict {},
            carina_provider_protocol::DriveOutcome::OverBudget {
                last_pending: _last_pending,
            } => Err(Self::over_budget(operation, id)),
            carina_provider_protocol::DriveOutcome::ProtocolViolation {
                last_pending: _last_pending,
            } => Err(Self::read_back_protocol_violation(operation, id)),
        }
    }

    fn create_request_attribute_names(request: &wit_types::CreateRequest) -> BTreeSet<String> {
        request
            .res
            .attributes
            .iter()
            .map(|(name, _value)| name.clone())
            .collect()
    }

    fn create_boundary_error(id: &ResourceId, detail: &'static str) -> ProviderError {
        carina_core::provider::HostError::internal(
            ["invalid WASM create result: ", detail].concat(),
        )
        .for_resource(id.clone())
        .into()
    }

    fn create_boundary_error_with_source(
        id: &ResourceId,
        detail: &'static str,
        source: ProviderError,
    ) -> ProviderError {
        ProviderError::internal_caused_by(
            "invalid WASM create result",
            ProviderError::internal_caused_by(detail, source),
        )
        .for_resource(id.clone())
    }

    fn validate_and_unpack_pending_create(
        id: &ResourceId,
        wit_request: &wit_types::CreateRequest,
        pending: proto::PendingCreate<proto::PendingToken>,
        expected_sent: Option<&Sent>,
        detail: &'static str,
    ) -> Result<PendingCreateParts, PendingCreateBoundaryError> {
        let request_names = Self::create_request_attribute_names(wit_request);
        let (token, progress, mut sent, poll_after_secs) = pending.into_parts();
        let same_sent = match expected_sent {
            Some(expected) => &sent == expected,
            None => true,
        };
        let valid_subset = sent.is_valid_for(&request_names);
        if same_sent && valid_subset {
            return Ok(PendingCreateParts {
                token,
                progress,
                sent,
                poll_after_secs,
            });
        }

        sent.clip_to(&request_names);
        Err(PendingCreateBoundaryError {
            safe_parts: PendingCreateParts {
                token,
                progress,
                sent,
                poll_after_secs,
            },
            error: Self::create_boundary_error(id, detail),
        })
    }

    /// Turn a boundary decode failure that happened after a valid create
    /// identifier into the only safe classification: the identifier was
    /// reported, but its post-create observation could not be decoded.
    fn identifier_reported_read_decode_failure(
        operation: &'static str,
        id: &ResourceId,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> (AppliedIdentifier, Option<Sent>, Observation, ProviderError) {
        let (identifier, sent, declared_error, decode_error) = failure.into_parts();
        let decode_error = Arc::new(decode_error);
        let read_error = wasm_value_decode_provider_error(operation, Arc::clone(&decode_error))
            .for_resource(id.clone());
        let error = declared_error.unwrap_or_else(|| {
            wasm_value_decode_provider_error(operation, decode_error).for_resource(id.clone())
        });
        (identifier, sent, Observation::ReadFailed(read_error), error)
    }

    fn initial_identifier_reported_read_decode_failure(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        attempt: CreateAttempt,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, sent, observation, error) =
            Self::identifier_reported_read_decode_failure("create", &id, failure);
        Self::initial_create_verdict(
            id,
            wit_request,
            attempt,
            proto::CreateVerdict::IdentifierReported(proto::CreateIdentified {
                identifier,
                observation,
                sent: sent.unwrap_or(Sent::AllRequestAttributes),
                error,
            }),
        )
    }

    fn polled_step_identifier_reported_read_decode_failure(
        starting: PendingCreate,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, _sent, observation, error) =
            Self::identifier_reported_read_decode_failure("poll_create", &starting.id, failure);
        let poll = starting.pending.into_poll();
        CreateProgress::Verdict(Box::new(match poll {
            CorePoll::Unissued(attempt) => attempt
                .issued(identifier)
                .identifier_reported(observation, error),
            CorePoll::Issued(issued) => issued
                .canonicalize(identifier)
                .identifier_reported(observation, error),
        }))
    }

    fn polled_verdict_identifier_reported_read_decode_failure(
        starting: PendingCreate,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> CreateProgress {
        let (identifier, sent, observation, error) =
            Self::identifier_reported_read_decode_failure("poll_create", &starting.id, failure);
        let sent = sent.unwrap_or_else(|| starting.pending.sent().clone());
        Self::polled_create_verdict(
            starting,
            proto::PollVerdict::IdentifierReported(proto::CreateIdentified {
                identifier,
                observation,
                sent,
                error,
            }),
        )
    }

    fn resumed_identifier_reported_read_decode_failure(
        id: ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> ResumeProgress {
        let (identifier, sent, observation, error) =
            Self::identifier_reported_read_decode_failure("resume_create", &id, failure);
        match sent {
            None => Self::fail_resumed_create(starting, identifier, observation, error),
            Some(sent) => Self::resumed_create_verdict(
                id,
                &wit_request,
                starting,
                proto::PendingPhase::Operation,
                proto::ResumeVerdict::Resent(proto::CreateVerdict::IdentifierReported(
                    proto::CreateIdentified {
                        identifier,
                        observation,
                        sent,
                        error,
                    },
                )),
            ),
        }
    }

    fn map_initial_pending_create(
        id: &ResourceId,
        wit_request: &wit_types::CreateRequest,
        attempt: CreateAttempt,
        pending: proto::PendingCreate<proto::PendingToken>,
    ) -> Result<CorePendingCreate, CreateFailure> {
        let (parts, boundary_error) = match Self::validate_and_unpack_pending_create(
            id,
            wit_request,
            pending,
            None,
            "pending sent attributes are not a subset of the request",
        ) {
            Ok(parts) => (parts, None),
            Err(PendingCreateBoundaryError { safe_parts, error }) => (safe_parts, Some(error)),
        };
        let PendingCreateParts {
            token,
            progress,
            sent,
            poll_after_secs,
        } = parts;
        let pending = match progress {
            proto::CreateProgress::Operating { identifier: None } => {
                attempt.pending(token, sent, poll_after_secs)
            }
            proto::CreateProgress::Operating {
                identifier: Some(identifier),
            } => attempt
                .issued(identifier, sent)
                .pending(token, poll_after_secs),
            proto::CreateProgress::ReadingBack { identifier } => attempt
                .issued(identifier, sent)
                .pending_read_back(token, poll_after_secs),
        };
        if let Some(error) = boundary_error {
            return Err(pending.preserve(error));
        }
        Ok(pending)
    }

    fn initial_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        attempt: CreateAttempt,
        step: wasm_convert::DecodedCreateStep,
    ) -> CreateProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => CreateProgress::Done(Box::new(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .succeeded(attributes),
            )),
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => CreateProgress::Done(Box::new(
                attempt
                    .issued(identifier, Sent::AllRequestAttributes)
                    .partial(attributes, diagnostic),
            )),
            proto::CreateStep::Pending(pending) => {
                match Self::map_initial_pending_create(&id, &wit_request, attempt, pending) {
                    Ok(pending) => CreateProgress::Pending(Box::new(PendingCreate {
                        id,
                        wit_id,
                        wit_request,
                        pending,
                    })),
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn initial_create_verdict(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        attempt: CreateAttempt,
        verdict: wasm_convert::DecodedCreateVerdict,
    ) -> CreateProgress {
        match verdict {
            proto::CreateVerdict::NotCreated(error) | proto::CreateVerdict::NoResource(error) => {
                CreateProgress::Verdict(Box::new(attempt.not_created(error)))
            }
            proto::CreateVerdict::IdentifierReported(identified) => {
                let (identifier, observation, mut sent, mut error) = identified.into_parts();
                let request_names = Self::create_request_attribute_names(wit_request);
                if !sent.is_valid_for(&request_names) {
                    sent.clip_to(&request_names);
                    error = Self::create_boundary_error_with_source(
                        &id,
                        "identifier-reported verdict sent attributes are not a subset of the request",
                        error,
                    );
                }
                CreateProgress::Verdict(Box::new(
                    attempt
                        .issued(identifier, sent)
                        .identifier_reported(observation, error),
                ))
            }
            proto::CreateVerdict::InFlight(in_flight) => {
                let proto::CreateInFlight { pending, error } = in_flight;
                match Self::map_initial_pending_create(&id, wit_request, attempt, pending) {
                    Ok(pending) => CreateProgress::Verdict(Box::new(pending.preserve(error))),
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn preserve_polled_create(pending: PendingCreate, error: ProviderError) -> CreateProgress {
        CreateProgress::Verdict(Box::new(pending.pending.preserve(error)))
    }

    fn map_polled_pending_create(
        starting: PendingCreate,
        next: proto::PendingCreate<proto::PendingToken>,
    ) -> Result<PendingCreate, CreateFailure> {
        let parts = match Self::validate_and_unpack_pending_create(
            &starting.id,
            &starting.wit_request,
            next,
            Some(starting.pending.sent()),
            "pending changed its sent attributes",
        ) {
            Ok(parts) => parts,
            Err(boundary) => return Err(starting.pending.preserve(boundary.error)),
        };
        let poll = starting.pending.into_poll();
        let PendingCreateParts {
            token,
            progress,
            sent: _sent,
            poll_after_secs,
        } = parts;
        let pending = match poll {
            CorePoll::Unissued(attempt) => match progress {
                proto::CreateProgress::Operating { identifier: None } => {
                    attempt.pending(token, poll_after_secs)
                }
                proto::CreateProgress::Operating {
                    identifier: Some(identifier),
                } => attempt.issued(identifier).pending(token, poll_after_secs),
                proto::CreateProgress::ReadingBack { identifier } => attempt
                    .issued(identifier)
                    .pending_read_back(token, poll_after_secs),
            },
            CorePoll::Issued(issued) => match progress {
                proto::CreateProgress::Operating { .. } => issued.pending(token, poll_after_secs),
                proto::CreateProgress::ReadingBack { .. } => {
                    issued.pending_read_back(token, poll_after_secs)
                }
            },
        };
        Ok(PendingCreate {
            id: starting.id,
            wit_id: starting.wit_id,
            wit_request: starting.wit_request,
            pending,
        })
    }

    fn polled_create_step(
        starting: PendingCreate,
        step: wasm_convert::DecodedCreateStep,
    ) -> CreateProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => {
                let poll = starting.pending.into_poll();
                CreateProgress::Done(Box::new(match poll {
                    CorePoll::Unissued(attempt) => attempt.issued(identifier).succeeded(attributes),
                    CorePoll::Issued(issued) => {
                        issued.canonicalize(identifier).succeeded(attributes)
                    }
                }))
            }
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let poll = starting.pending.into_poll();
                CreateProgress::Done(Box::new(match poll {
                    CorePoll::Unissued(attempt) => {
                        attempt.issued(identifier).partial(attributes, diagnostic)
                    }
                    CorePoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .partial(attributes, diagnostic),
                }))
            }
            proto::CreateStep::Pending(next) => {
                match Self::map_polled_pending_create(starting, next) {
                    Ok(pending) => CreateProgress::Pending(Box::new(pending)),
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn polled_create_verdict(
        starting: PendingCreate,
        verdict: wasm_convert::DecodedPollVerdict,
    ) -> CreateProgress {
        match verdict {
            proto::PollVerdict::NotCreated(error) => {
                CreateProgress::Verdict(Box::new(starting.pending.not_created(error)))
            }
            proto::PollVerdict::IdentifierReported(identified) => {
                let (identifier, observation, sent, error) = identified.into_parts();
                if &sent != starting.pending.sent() {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "identifier-reported verdict changed the sent attributes",
                    );
                    return Self::preserve_polled_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                CreateProgress::Verdict(Box::new(match poll {
                    CorePoll::Unissued(attempt) => attempt
                        .issued(identifier)
                        .identifier_reported(observation, error),
                    CorePoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .identifier_reported(observation, error),
                }))
            }
            proto::PollVerdict::NoResource(error) => {
                let poll = starting.pending.into_poll();
                CreateProgress::Verdict(Box::new(match poll {
                    CorePoll::Unissued(attempt) => attempt.not_created(error),
                    CorePoll::Issued(issued) => issued.no_resource(error),
                }))
            }
            proto::PollVerdict::Unchanged(error) => Self::preserve_polled_create(starting, error),
            proto::PollVerdict::InFlight(in_flight) => {
                let proto::CreateInFlight { pending, error } = in_flight;
                let error = Self::in_flight_poll_error(
                    "create",
                    &starting.id,
                    starting.pending.phase(),
                    pending.phase(),
                    error,
                );
                match Self::map_polled_pending_create(starting, pending) {
                    Ok(pending) => {
                        CreateProgress::Verdict(Box::new(pending.pending.preserve(error)))
                    }
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn preserve_resent_create(
        starting: PendingResentCreate,
        error: ProviderError,
    ) -> ResumeProgress {
        ResumeProgress::Verdict(Box::new(starting.pending.preserve(error)))
    }

    fn map_polled_resent_pending_create(
        starting: PendingResentCreate,
        next: proto::PendingCreate<proto::PendingToken>,
    ) -> Result<PendingResentCreate, ResumeFailure> {
        let parts = match Self::validate_and_unpack_pending_create(
            &starting.id,
            &starting.wit_request,
            next,
            Some(starting.pending.sent()),
            "resent pending changed its sent attributes",
        ) {
            Ok(parts) => parts,
            Err(boundary) => return Err(starting.pending.preserve(boundary.error)),
        };
        let poll = starting.pending.into_poll();
        let PendingCreateParts {
            token,
            progress,
            sent: _sent,
            poll_after_secs,
        } = parts;
        let pending = match poll {
            CoreResentPoll::Unissued(attempt) => match progress {
                proto::CreateProgress::Operating { identifier: None } => {
                    attempt.pending(token, poll_after_secs)
                }
                proto::CreateProgress::Operating {
                    identifier: Some(identifier),
                } => attempt.issued(identifier).pending(token, poll_after_secs),
                proto::CreateProgress::ReadingBack { identifier } => attempt
                    .issued(identifier)
                    .pending_read_back(token, poll_after_secs),
            },
            CoreResentPoll::Issued(issued) => match progress {
                proto::CreateProgress::Operating { .. } => issued.pending(token, poll_after_secs),
                proto::CreateProgress::ReadingBack { .. } => {
                    issued.pending_read_back(token, poll_after_secs)
                }
            },
        };
        Ok(PendingResentCreate {
            id: starting.id,
            wit_id: starting.wit_id,
            wit_request: starting.wit_request,
            pending,
        })
    }

    fn polled_resent_step(
        starting: PendingResentCreate,
        step: wasm_convert::DecodedCreateStep,
    ) -> ResumeProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Done(match poll {
                    CoreResentPoll::Unissued(attempt) => {
                        attempt.issued(identifier).succeeded(attributes)
                    }
                    CoreResentPoll::Issued(issued) => {
                        issued.canonicalize(identifier).succeeded(attributes)
                    }
                })
            }
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Done(match poll {
                    CoreResentPoll::Unissued(attempt) => {
                        attempt.issued(identifier).partial(attributes, diagnostic)
                    }
                    CoreResentPoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .partial(attributes, diagnostic),
                })
            }
            proto::CreateStep::Pending(next) => {
                match Self::map_polled_resent_pending_create(starting, next) {
                    Ok(pending) => {
                        ResumeProgress::Pending(Box::new(PendingResume::Resent(pending)))
                    }
                    Err(failure) => ResumeProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn polled_resent_verdict(
        starting: PendingResentCreate,
        verdict: wasm_convert::DecodedPollVerdict,
    ) -> ResumeProgress {
        match verdict {
            proto::PollVerdict::NotCreated(error) => {
                ResumeProgress::Verdict(Box::new(starting.pending.not_created(error)))
            }
            proto::PollVerdict::IdentifierReported(identified) => {
                let (identifier, observation, sent, error) = identified.into_parts();
                if &sent != starting.pending.sent() {
                    let error = Self::create_boundary_error(
                        &starting.id,
                        "resent identifier-reported verdict changed the sent attributes",
                    );
                    return Self::preserve_resent_create(starting, error);
                }
                let poll = starting.pending.into_poll();
                ResumeProgress::Verdict(Box::new(match poll {
                    CoreResentPoll::Unissued(attempt) => attempt
                        .issued(identifier)
                        .identifier_reported(observation, error),
                    CoreResentPoll::Issued(issued) => issued
                        .canonicalize(identifier)
                        .identifier_reported(observation, error),
                }))
            }
            proto::PollVerdict::NoResource(error) => {
                let poll = starting.pending.into_poll();
                ResumeProgress::Verdict(Box::new(match poll {
                    CoreResentPoll::Unissued(attempt) => attempt.not_created(error),
                    CoreResentPoll::Issued(issued) => issued.no_resource(error),
                }))
            }
            proto::PollVerdict::Unchanged(error) => Self::preserve_resent_create(starting, error),
            proto::PollVerdict::InFlight(in_flight) => {
                let proto::CreateInFlight { pending, error } = in_flight;
                let error = Self::in_flight_poll_error(
                    "resume_create",
                    &starting.id,
                    starting.pending.phase(),
                    pending.phase(),
                    error,
                );
                match Self::map_polled_resent_pending_create(starting, pending) {
                    Ok(pending) => {
                        ResumeProgress::Verdict(Box::new(pending.pending.preserve(error)))
                    }
                    Err(failure) => ResumeProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn polled_resent_identifier_reported_read_decode_failure(
        starting: PendingResentCreate,
        failure: wasm_convert::IdentifierReportedReadDecodeFailure,
    ) -> ResumeProgress {
        let (identifier, sent, observation, error) =
            Self::identifier_reported_read_decode_failure("poll_create", &starting.id, failure);
        let sent = sent.unwrap_or_else(|| starting.pending.sent().clone());
        Self::polled_resent_verdict(
            starting,
            proto::PollVerdict::IdentifierReported(proto::CreateIdentified {
                identifier,
                observation,
                sent,
                error,
            }),
        )
    }

    fn preserve_resumed_create(resume: CoreResume, error: ProviderError) -> ResumeProgress {
        ResumeProgress::Verdict(Box::new(resume.preserve(error)))
    }

    fn fail_resumed_create(
        resume: CoreResume,
        identifier: AppliedIdentifier,
        observation: Observation,
        error: ProviderError,
    ) -> ResumeProgress {
        let failure = match resume {
            CoreResume::Unissued(attempt) => attempt
                .issued(identifier)
                .identifier_reported(observation, error),
            CoreResume::Issued(resumed) => resumed
                .canonicalize(identifier)
                .identifier_reported(observation, error),
        };
        ResumeProgress::Verdict(Box::new(failure))
    }

    fn map_resumed_pending_create(
        starting: CoreResume,
        next: proto::ResumePending<proto::PendingToken>,
    ) -> (CoreResume, u32, proto::PendingPhase) {
        let (token, progress, poll_after_secs) = next.into_parts();
        let phase = progress.phase();
        let resume = match starting {
            CoreResume::Unissued(attempt) => match progress.identifier().cloned() {
                Some(identifier) => attempt.issued(identifier).repend(token),
                None => attempt.repend(token),
            },
            CoreResume::Issued(resumed) => resumed.repend(token),
        };
        (resume, poll_after_secs, phase)
    }

    fn resumed_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        step: wasm_convert::DecodedResumeStep,
    ) -> ResumeProgress {
        match step {
            proto::ResumeStep::Completed {
                identifier,
                attributes,
            } => {
                let state = AppliedState::from_identifier(id, identifier.clone(), attributes);
                let outcome = match starting {
                    CoreResume::Unissued(attempt) => attempt.issued(identifier).completed(state),
                    CoreResume::Issued(resumed) => {
                        resumed.canonicalize(identifier).completed(state)
                    }
                };
                ResumeProgress::Done(outcome)
            }
            proto::ResumeStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let outcome = match starting {
                    CoreResume::Unissued(attempt) => {
                        attempt.issued(identifier).partial(attributes, diagnostic)
                    }
                    CoreResume::Issued(resumed) => resumed
                        .canonicalize(identifier)
                        .partial(attributes, diagnostic),
                };
                ResumeProgress::Done(outcome)
            }
            proto::ResumeStep::Pending(next) => {
                let (resume, poll_after_secs, phase) =
                    Self::map_resumed_pending_create(starting, next);
                ResumeProgress::Pending(Box::new(PendingResume::Resume {
                    id,
                    wit_id,
                    wit_request,
                    resume,
                    poll_after_secs,
                    phase,
                }))
            }
            proto::ResumeStep::Resent(step) => {
                Self::resent_create_step(id, wit_id, wit_request, starting, step)
            }
        }
    }

    fn map_resent_pending_create(
        id: &ResourceId,
        wit_request: &wit_types::CreateRequest,
        starting: CoreResume,
        pending: proto::PendingCreate<proto::PendingToken>,
    ) -> Result<ResentPendingCreate, ResumeFailure> {
        let attempt = match starting {
            CoreResume::Unissued(attempt) => attempt.resend(),
            CoreResume::Issued(resumed) => {
                return Err(resumed.preserve(Self::create_boundary_error(
                    id,
                    "an issued resume attempted to resend the create",
                )));
            }
        };
        let parts = match Self::validate_and_unpack_pending_create(
            id,
            wit_request,
            pending,
            None,
            "resent pending attributes are not a subset of the request",
        ) {
            Ok(parts) => parts,
            Err(boundary) => return Err(attempt.preserve(boundary.error)),
        };
        let PendingCreateParts {
            token,
            progress,
            sent,
            poll_after_secs,
        } = parts;
        Ok(match progress {
            proto::CreateProgress::Operating { identifier: None } => {
                attempt.pending(token, sent, poll_after_secs)
            }
            proto::CreateProgress::Operating {
                identifier: Some(identifier),
            } => attempt
                .issued(identifier, sent)
                .pending(token, poll_after_secs),
            proto::CreateProgress::ReadingBack { identifier } => attempt
                .issued(identifier, sent)
                .pending_read_back(token, poll_after_secs),
        })
    }

    fn resent_create_step(
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        starting: CoreResume,
        step: wasm_convert::DecodedCreateStep,
    ) -> ResumeProgress {
        match step {
            proto::CreateStep::Success {
                identifier,
                attributes,
            } => {
                let attempt = match starting {
                    CoreResume::Unissued(attempt) => attempt.resend(),
                    CoreResume::Issued(resumed) => {
                        return Self::preserve_resumed_create(
                            CoreResume::Issued(resumed),
                            Self::create_boundary_error(
                                &id,
                                "an issued resume attempted to resend the create",
                            ),
                        );
                    }
                };
                ResumeProgress::Done(
                    attempt
                        .issued(identifier, Sent::AllRequestAttributes)
                        .succeeded(attributes),
                )
            }
            proto::CreateStep::PartialSuccess {
                identifier,
                attributes,
                diagnostic,
            } => {
                let attempt = match starting {
                    CoreResume::Unissued(attempt) => attempt.resend(),
                    CoreResume::Issued(resumed) => {
                        return Self::preserve_resumed_create(
                            CoreResume::Issued(resumed),
                            Self::create_boundary_error(
                                &id,
                                "an issued resume attempted to resend the create",
                            ),
                        );
                    }
                };
                ResumeProgress::Done(
                    attempt
                        .issued(identifier, Sent::AllRequestAttributes)
                        .partial(attributes, diagnostic),
                )
            }
            proto::CreateStep::Pending(pending) => {
                match Self::map_resent_pending_create(&id, &wit_request, starting, pending) {
                    Ok(pending) => ResumeProgress::Pending(Box::new(PendingResume::Resent(
                        PendingResentCreate {
                            id,
                            wit_id,
                            wit_request,
                            pending,
                        },
                    ))),
                    Err(failure) => ResumeProgress::Verdict(Box::new(failure)),
                }
            }
        }
    }

    fn resumed_create_verdict(
        id: ResourceId,
        wit_request: &wit_types::CreateRequest,
        starting: CoreResume,
        previous_phase: proto::PendingPhase,
        verdict: wasm_convert::DecodedResumeVerdict,
    ) -> ResumeProgress {
        match verdict {
            proto::ResumeVerdict::NotCreated(error) => {
                ResumeProgress::Verdict(Box::new(starting.not_created(error)))
            }
            proto::ResumeVerdict::IdentifierReported(identified) => {
                let (identifier, observation, error) = identified.into_parts();
                Self::fail_resumed_create(starting, identifier, observation, error)
            }
            proto::ResumeVerdict::NoResource(error) => {
                ResumeProgress::Verdict(Box::new(starting.no_resource(error)))
            }
            proto::ResumeVerdict::Unchanged(error) => {
                Self::preserve_resumed_create(starting, error)
            }
            proto::ResumeVerdict::InFlight(in_flight) => {
                let proto::ResumeInFlight { pending, error } = in_flight;
                let error = Self::in_flight_poll_error(
                    "resume_create",
                    &id,
                    previous_phase,
                    pending.phase(),
                    error,
                );
                let (resume, _poll_after_secs, _phase) =
                    Self::map_resumed_pending_create(starting, pending);
                ResumeProgress::Verdict(Box::new(resume.preserve(error)))
            }
            proto::ResumeVerdict::Resent(verdict) => match verdict {
                proto::CreateVerdict::NotCreated(error)
                | proto::CreateVerdict::NoResource(error) => {
                    let attempt = match starting {
                        CoreResume::Unissued(attempt) => attempt.resend(),
                        CoreResume::Issued(resumed) => {
                            return Self::preserve_resumed_create(
                                CoreResume::Issued(resumed),
                                Self::create_boundary_error(
                                    &id,
                                    "an issued resume attempted to resend the create",
                                ),
                            );
                        }
                    };
                    ResumeProgress::Verdict(Box::new(attempt.not_created(error)))
                }
                proto::CreateVerdict::IdentifierReported(identified) => {
                    let attempt = match starting {
                        CoreResume::Unissued(attempt) => attempt.resend(),
                        CoreResume::Issued(resumed) => {
                            return Self::preserve_resumed_create(
                                CoreResume::Issued(resumed),
                                Self::create_boundary_error(
                                    &id,
                                    "an issued resume attempted to resend the create",
                                ),
                            );
                        }
                    };
                    let (identifier, observation, sent, error) = identified.into_parts();
                    let request_names = Self::create_request_attribute_names(wit_request);
                    if !sent.is_valid_for(&request_names) {
                        return ResumeProgress::Verdict(Box::new(attempt.preserve(
                            Self::create_boundary_error(
                                &id,
                                "resent verdict attributes are not a subset of the request",
                            ),
                        )));
                    }
                    ResumeProgress::Verdict(Box::new(
                        attempt
                            .issued(identifier, sent)
                            .identifier_reported(observation, error),
                    ))
                }
                proto::CreateVerdict::InFlight(in_flight) => {
                    let proto::CreateInFlight { pending, error } = in_flight;
                    match Self::map_resent_pending_create(&id, wit_request, starting, pending) {
                        Ok(pending) => ResumeProgress::Verdict(Box::new(pending.preserve(error))),
                        Err(failure) => ResumeProgress::Verdict(Box::new(failure)),
                    }
                }
            },
        }
    }

    async fn call_resume_record(
        &self,
        id: ResourceId,
        wit_id: wit_types::ResourceId,
        wit_request: wit_types::CreateRequest,
        resume: CoreResume,
        poll: ResumePoll,
        secrets: Option<carina_core::provider::ProviderValidationInputs>,
    ) -> ResumeProgress {
        let progress = match poll.phase {
            proto::PendingPhase::Operation => proto::CreateProgress::Operating {
                identifier: resume.identifier().cloned(),
            },
            proto::PendingPhase::ReadBack => proto::CreateProgress::ReadingBack {
                identifier: resume
                    .identifier()
                    .expect("a read-back continuation must have an identifier")
                    .clone(),
            },
        };
        let wit_pending = wasm_convert::core_to_wit_resume_pending(
            resume.token(),
            &progress,
            poll.poll_after_secs,
        );
        let wit_poll = wasm_convert::core_to_wit_poll_request(proto::PollRequest {
            finality: poll.finality,
        });
        let call_id = wit_id.clone();
        let call_request = wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("resume_create").await?;
            let locked = if let Some(secrets) = secrets {
                locked.with_secrets(secrets)
            } else {
                locked
            };
            locked
                .call("resume_create", async |bindings, store| {
                    bindings
                        .call_resume_create(store, &call_id, &call_request, &wit_pending, &wit_poll)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_resumed_create(resume, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_resume_step(step, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::resumed_create_step(id, wit_id, wit_request, resume, step)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::resumed_identifier_reported_read_decode_failure(
                        id,
                        wit_request,
                        resume,
                        failure,
                    )
                }
                Err(error) => Self::preserve_resumed_create(
                    resume,
                    wasm_value_decode_provider_error("resume_create", error).for_resource(id),
                ),
            },
            Err(verdict) => match wasm_convert::wit_to_core_resume_verdict(verdict, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::resumed_create_verdict(id, &wit_request, resume, poll.phase, verdict)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::resumed_identifier_reported_read_decode_failure(
                        id,
                        wit_request,
                        resume,
                        failure,
                    )
                }
                Err(error) => Self::preserve_resumed_create(
                    resume,
                    wasm_value_decode_provider_error("resume_create", error).for_resource(id),
                ),
            },
        }
    }

    async fn poll_resume_state(
        &self,
        pending: PendingResume,
        finality: proto::PollFinality,
    ) -> ResumeProgress {
        match pending {
            PendingResume::Resume {
                id,
                wit_id,
                wit_request,
                resume,
                poll_after_secs,
                phase,
            } => {
                self.call_resume_record(
                    id,
                    wit_id,
                    wit_request,
                    resume,
                    ResumePoll {
                        poll_after_secs,
                        phase,
                        finality,
                    },
                    None,
                )
                .await
            }
            PendingResume::Resent(pending) => self.call_poll_resent_create(pending, finality).await,
        }
    }

    async fn call_create(
        &self,
        id: ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateProgress, CreateFailure> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_create_request(&request);
        let call_id = wit_id.clone();
        let call_request = wit_request.clone();
        let locked = match self.instance.acquire("create").await {
            Ok(locked) => locked,
            Err(error) => return Err(attempt.not_created(error.for_resource(id))),
        };
        let started = attempt.start_guest_call();
        let returned = locked
            .call_create("create", id.clone(), started, async |bindings, store| {
                bindings.call_create(store, &call_id, &call_request).await
            })
            .await?;
        enum DecodedInitialCreateCall {
            Step(wasm_convert::DecodedIssued<wasm_convert::DecodedCreateStep>),
            Verdict(wasm_convert::DecodedIssued<wasm_convert::DecodedCreateVerdict>),
        }
        let decode_id = id.clone();
        let decoded = returned
            .decode(|result| match result {
                Ok(step) => wasm_convert::wit_to_core_create_step(step, &decode_id)
                    .map(DecodedInitialCreateCall::Step)
                    .map_err(|error| {
                        wasm_value_decode_provider_error("create", error)
                            .for_resource(decode_id.clone())
                    }),
                Err(verdict) => wasm_convert::wit_to_core_create_verdict(verdict, &decode_id)
                    .map(DecodedInitialCreateCall::Verdict)
                    .map_err(|error| {
                        wasm_value_decode_provider_error("create", error)
                            .for_resource(decode_id.clone())
                    }),
            })
            .await?;
        let (attempt, decoded) = decoded.into_parts();
        match decoded {
            DecodedInitialCreateCall::Step(wasm_convert::DecodedIssued::Decoded(step)) => Ok(
                Self::initial_create_step(id, wit_id, wit_request, attempt, step),
            ),
            DecodedInitialCreateCall::Step(
                wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure),
            ) => Ok(Self::initial_identifier_reported_read_decode_failure(
                id,
                &wit_request,
                attempt,
                failure,
            )),
            DecodedInitialCreateCall::Verdict(wasm_convert::DecodedIssued::Decoded(verdict)) => Ok(
                Self::initial_create_verdict(id, &wit_request, attempt, verdict),
            ),
            DecodedInitialCreateCall::Verdict(
                wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure),
            ) => Ok(Self::initial_identifier_reported_read_decode_failure(
                id,
                &wit_request,
                attempt,
                failure,
            )),
        }
    }

    async fn call_poll_create(
        &self,
        starting: PendingCreate,
        finality: proto::PollFinality,
    ) -> CreateProgress {
        let id = starting.id.clone();
        let wit_pending = wasm_convert::core_to_wit_pending_create(&starting.pending);
        let wit_poll = wasm_convert::core_to_wit_poll_request(proto::PollRequest { finality });
        let call_id = starting.wit_id.clone();
        let call_request = starting.wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("poll_create").await?;
            locked
                .call("poll_create", async |bindings, store| {
                    bindings
                        .call_poll_create(store, &call_id, &call_request, &wit_pending, &wit_poll)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_polled_create(starting, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_create_step(step, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::polled_create_step(starting, step)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::polled_step_identifier_reported_read_decode_failure(starting, failure)
                }
                Err(error) => Self::preserve_polled_create(
                    starting,
                    wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                ),
            },
            Err(verdict) => match wasm_convert::wit_to_core_poll_verdict(verdict, &id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::polled_create_verdict(starting, verdict)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::polled_verdict_identifier_reported_read_decode_failure(starting, failure)
                }
                Err(error) => Self::preserve_polled_create(
                    starting,
                    wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                ),
            },
        }
    }

    async fn call_poll_resent_create(
        &self,
        starting: PendingResentCreate,
        finality: proto::PollFinality,
    ) -> ResumeProgress {
        let wit_pending = wasm_convert::core_to_wit_pending_create_parts(
            starting.pending.token(),
            starting.pending.progress(),
            starting.pending.sent(),
            starting.pending.poll_after_secs(),
        );
        let wit_poll = wasm_convert::core_to_wit_poll_request(proto::PollRequest { finality });
        let call_id = starting.wit_id.clone();
        let call_request = starting.wit_request.clone();
        let call_result = async {
            let locked = self.instance.acquire("poll_create").await?;
            locked
                .call("poll_create", async |bindings, store| {
                    bindings
                        .call_poll_create(store, &call_id, &call_request, &wit_pending, &wit_poll)
                        .await
                })
                .await
        }
        .await;
        let result = match call_result {
            Ok(result) => result,
            Err(error) => return Self::preserve_resent_create(starting, error),
        };
        match result {
            Ok(step) => match wasm_convert::wit_to_core_create_step(step, &starting.id) {
                Ok(wasm_convert::DecodedIssued::Decoded(step)) => {
                    Self::polled_resent_step(starting, step)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::polled_resent_identifier_reported_read_decode_failure(starting, failure)
                }
                Err(error) => {
                    let id = starting.id.clone();
                    Self::preserve_resent_create(
                        starting,
                        wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                    )
                }
            },
            Err(verdict) => match wasm_convert::wit_to_core_poll_verdict(verdict, &starting.id) {
                Ok(wasm_convert::DecodedIssued::Decoded(verdict)) => {
                    Self::polled_resent_verdict(starting, verdict)
                }
                Ok(wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure)) => {
                    Self::polled_resent_identifier_reported_read_decode_failure(starting, failure)
                }
                Err(error) => {
                    let id = starting.id.clone();
                    Self::preserve_resent_create(
                        starting,
                        wasm_value_decode_provider_error("poll_create", error).for_resource(id),
                    )
                }
            },
        }
    }

    async fn drive_create(
        &self,
        id: ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
        budgets: OperationBudgets,
    ) -> Result<CreateOutcome, CreateFailure> {
        let initial = self.call_create(id.clone(), request, attempt);
        let outcome = carina_provider_protocol::drive_pending_infallible(
            budgets.operation,
            budgets.read_back,
            async move {
                match initial.await {
                    Ok(progress) => progress,
                    Err(failure) => CreateProgress::Verdict(Box::new(failure)),
                }
                .into_step()
            },
            |pending, finality| async move {
                #[cfg(any(test, feature = "test-utils"))]
                self.observe_poll(PollEventForTest::Attempt {
                    operation: "create",
                });
                self.call_poll_create(*pending, finality).await.into_step()
            },
            |delay| self.poll_sleep("create", delay),
        )
        .await;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(outcome) => {
                Ok(CreateOutcome::from_host_completion(outcome))
            }
            carina_provider_protocol::DriveOutcome::Verdict(failure) => Err(*failure),
            carina_provider_protocol::DriveOutcome::OverBudget { last_pending } => {
                Err(last_pending
                    .pending
                    .preserve(Self::over_budget("create", id)))
            }
            carina_provider_protocol::DriveOutcome::ProtocolViolation { last_pending } => {
                Err(last_pending
                    .pending
                    .preserve(Self::read_back_protocol_violation("create", id)))
            }
        }
    }

    async fn drive_resume_create(
        &self,
        request: ResumeCreateRequest,
        budgets: OperationBudgets,
    ) -> Result<ResumeOutcome, ResumeFailure> {
        let (resume, create_request) = request.into_parts();
        let id = resume.id().clone();
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_create_request(&create_request);
        // Persisted resume state has no scheduling metadata, while WIT requires
        // both fields. This operation-phase placeholder allows saved read-back
        // tokens to resume with an identifier: SDK dispatch only rejects a
        // backward phase transition. Returned pending or preserved progress
        // establishes the phase used by subsequent polls.
        let initial = self.call_resume_record(
            id.clone(),
            wit_id,
            wit_request,
            resume,
            ResumePoll {
                poll_after_secs: 1,
                phase: proto::PendingPhase::Operation,
                finality: proto::PollFinality::Continue,
            },
            None,
        );
        let outcome = carina_provider_protocol::drive_pending_infallible(
            budgets.operation,
            budgets.read_back,
            async move { initial.await.into_step() },
            |pending, finality| async move {
                self.poll_resume_state(*pending, finality).await.into_step()
            },
            |delay| self.poll_sleep("resume_create", delay),
        )
        .await;
        match outcome {
            carina_provider_protocol::DriveOutcome::Done(outcome) => {
                Ok(ResumeOutcome::from_host_completion(outcome))
            }
            carina_provider_protocol::DriveOutcome::Verdict(failure) => Err(*failure),
            carina_provider_protocol::DriveOutcome::OverBudget { last_pending } => {
                Err((*last_pending).into_failure(Self::over_budget("resume_create", id)))
            }
            carina_provider_protocol::DriveOutcome::ProtocolViolation { last_pending } => {
                Err((*last_pending)
                    .into_failure(Self::read_back_protocol_violation("resume_create", id)))
            }
        }
    }

    async fn call_update(
        &self,
        id: ResourceId,
        identifier: String,
        request: UpdateRequest,
    ) -> ProviderResult<UpdateProgress> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_update_request(&request).map_err(|error| {
            wasm_value_encode_provider_error("update", error).for_resource(id.clone())
        })?;
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request.clone();
        self.call_operation(
            "update",
            id,
            None,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_update(store, &call_id, &call_identifier, &call_request)
                        .await
                })
            },
            move |outcome, id| {
                let outcome = wasm_convert::wit_to_core_update_outcome(outcome, &id)?;
                Ok::<_, wasm_convert::WasmAppliedStateDecodeError>(match outcome {
                    wasm_convert::WasmUpdateOutcome::Done(outcome) => UpdateProgress::Done(outcome),
                    wasm_convert::WasmUpdateOutcome::Pending(operation) => {
                        UpdateProgress::Pending(Box::new(PendingUpdate {
                            id,
                            wit_id,
                            identifier,
                            wit_request,
                            operation,
                        }))
                    }
                })
            },
        )
        .await
    }

    async fn call_poll_update(
        &self,
        pending: PendingUpdate,
        finality: proto::PollFinality,
    ) -> ProviderResult<UpdateProgress> {
        let PendingUpdate {
            id,
            wit_id,
            identifier,
            wit_request,
            operation,
        } = pending;
        let wit_pending = operation.into_wit();
        let wit_poll = wasm_convert::core_to_wit_poll_request(proto::PollRequest { finality });
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request.clone();
        self.call_operation(
            "poll_update",
            id,
            None,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_poll_update(
                            store,
                            &call_id,
                            &call_identifier,
                            &call_request,
                            &wit_pending,
                            &wit_poll,
                        )
                        .await
                })
            },
            move |outcome, id| {
                let outcome = wasm_convert::wit_to_core_update_outcome(outcome, &id)?;
                Ok::<_, wasm_convert::WasmAppliedStateDecodeError>(match outcome {
                    wasm_convert::WasmUpdateOutcome::Done(outcome) => UpdateProgress::Done(outcome),
                    wasm_convert::WasmUpdateOutcome::Pending(operation) => {
                        UpdateProgress::Pending(Box::new(PendingUpdate {
                            id,
                            wit_id,
                            identifier,
                            wit_request,
                            operation,
                        }))
                    }
                })
            },
        )
        .await
    }

    async fn call_delete(
        &self,
        id: ResourceId,
        identifier: String,
        request: DeleteRequest,
    ) -> ProviderResult<DeleteProgress> {
        let wit_id = wasm_convert::core_to_wit_resource_id(&id);
        let wit_request = wasm_convert::core_to_wit_delete_request(&request);
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request;
        self.call_operation(
            "delete",
            id,
            None,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_delete(store, &call_id, &call_identifier, call_request)
                        .await
                })
            },
            move |outcome, id| {
                Ok::<_, std::convert::Infallible>(
                    match wasm_convert::wit_to_core_delete_outcome(outcome) {
                        wasm_convert::WasmDeleteOutcome::Deleted => DeleteProgress::Deleted,
                        wasm_convert::WasmDeleteOutcome::Pending(operation) => {
                            DeleteProgress::Pending(Box::new(PendingDelete {
                                id,
                                wit_id,
                                identifier,
                                wit_request,
                                operation,
                            }))
                        }
                    },
                )
            },
        )
        .await
    }

    async fn call_poll_delete(&self, pending: PendingDelete) -> ProviderResult<DeleteProgress> {
        let PendingDelete {
            id,
            wit_id,
            identifier,
            wit_request,
            operation,
        } = pending;
        let wit_pending = operation.into_wit();
        let call_id = wit_id.clone();
        let call_identifier = identifier.clone();
        let call_request = wit_request;
        self.call_operation(
            "poll_delete",
            id,
            None,
            move |bindings, store| {
                Box::pin(async move {
                    bindings
                        .call_poll_delete(
                            store,
                            &call_id,
                            &call_identifier,
                            call_request,
                            &wit_pending,
                        )
                        .await
                })
            },
            move |outcome, id| {
                Ok::<_, std::convert::Infallible>(
                    match wasm_convert::wit_to_core_delete_outcome(outcome) {
                        wasm_convert::WasmDeleteOutcome::Deleted => DeleteProgress::Deleted,
                        wasm_convert::WasmDeleteOutcome::Pending(operation) => {
                            DeleteProgress::Pending(Box::new(PendingDelete {
                                id,
                                wit_id,
                                identifier,
                                wit_request,
                                operation,
                            }))
                        }
                    },
                )
            },
        )
        .await
    }
}

impl Provider for WasmProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn resolve_references<'a>(
        &'a self,
        requests: &'a [carina_core::references::ReferenceRequest],
    ) -> BoxFuture<
        'a,
        ProviderResult<
            Vec<
                Result<
                    carina_core::references::ReferenceOutcome,
                    carina_core::references::ReferenceError,
                >,
            >,
        >,
    > {
        let encoded: Vec<_> = requests
            .iter()
            .map(|request| {
                let carina_core::references::ReferenceRequest { ty, value } = request;
                version_checked_bindings::wit_types::ReferenceRequest {
                    ty: wasm_convert::core_type_identity_to_wit(ty),
                    value: value.clone(),
                }
            })
            .collect();
        Box::pin(async move {
            let locked = self.instance.acquire("resolve_references").await?;
            let result = locked
                .call("resolve_references", async |bindings, store| {
                    bindings.call_resolve_references(store, &encoded).await
                })
                .await?;
            let results = result.map_err(wasm_convert::wit_to_core_provider_error)?;
            Ok(wasm_convert::decode_reference_results(results))
        })
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let wit_request = wasm_convert::core_to_wit_read_request(&request);
        let identifier = identifier.as_str().to_string();
        let id = id.clone();
        Box::pin(async move {
            let locked = self.instance.acquire("read").await?;
            let result = locked
                .call("read", async |bindings, store| {
                    bindings
                        .call_read(store, &wit_id, &identifier, wit_request)
                        .await
                })
                .await?;
            match result {
                Ok(wit_outcome) => wasm_convert::wit_to_core_read_outcome(wit_outcome, &id)
                    .map_err(|error| {
                        wasm_value_decode_provider_error("read", error).for_resource(id.clone())
                    }),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = wasm_convert::core_data_source_to_wit_resource(resource);

        let id = resource.id.clone();
        Box::pin(async move {
            let locked = self.instance.acquire("read_data_source").await?;
            let result = locked
                .call("read_data_source", async |bindings, store| {
                    bindings.call_read_data_source(store, &wit_resource).await
                })
                .await?;
            match result {
                Ok(wit_state) => {
                    wasm_convert::wit_to_core_state(&wit_state, &id).map_err(|error| {
                        wasm_value_decode_provider_error("read_data_source", error)
                            .for_resource(id.clone())
                    })
                }
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(wit_err)),
            }
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let id = id.clone();
        let budgets = self.operation_budgets(&id, carina_provider_protocol::OperationKind::Create);
        Box::pin(async move { self.drive_create(id, request, attempt, budgets).await })
    }

    fn resume_create(
        &self,
        request: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        let budgets = self.operation_budgets(
            request.id(),
            carina_provider_protocol::OperationKind::Create,
        );
        Box::pin(async move { self.drive_resume_create(request, budgets).await })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        _attempt: carina_core::provider::UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.as_str().to_string();
        let budgets = self.operation_budgets(&id, carina_provider_protocol::OperationKind::Update);
        Box::pin(async move {
            let initial = self.call_update(id.clone(), identifier, request);
            self.drive_operation("update", id, budgets, initial, |pending, finality| {
                self.call_poll_update(*pending, finality)
            })
            .await
            .map(UpdateOutcome::from_host_completion)
        })
    }

    fn delete(
        &self,
        attempt: carina_core::provider::DeleteAttempt,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = attempt.id().clone();
        let identifier = attempt.identifier().as_str().to_string();
        let request = attempt.request().clone();
        let budgets = self.operation_budgets(&id, carina_provider_protocol::OperationKind::Delete);
        Box::pin(async move {
            let _attempt = attempt;
            let initial = self.call_delete(id.clone(), identifier, request);
            self.drive_operation("delete", id, budgets, initial, |pending, _finality| {
                self.call_poll_delete(*pending)
            })
            .await
            .map(carina_provider_protocol::CompletedOperation::into_output)
        })
    }

    fn required_permissions(&self, id: &ResourceId, op: PlanOp) -> ProviderResult<Vec<String>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let locked = self.instance.acquire("required_permissions").await?;
                locked
                    .call("required_permissions", async |bindings, store| {
                        bindings.call_required_permissions(store, &wit_id, op).await
                    })
                    .await
                    .map(|permissions| permissions.0)
            })
        })
    }

    fn satisfier_hint(
        &self,
        target_id: &ResourceId,
        attr_path: &AttrPath,
    ) -> ProviderResult<Vec<BindingPattern>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(target_id);
        let wit_attr_path = attr_path.segments().to_vec();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let locked = self.instance.acquire("satisfier_hint").await?;
                let patterns = locked
                    .call("satisfier_hint", async |bindings, store| {
                        bindings
                            .call_satisfier_hint(store, &wit_id, &wit_attr_path)
                            .await
                    })
                    .await?;
                patterns
                    .into_iter()
                    .map(wasm_convert::wit_to_core_binding_pattern)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| {
                        wasm_value_decode_provider_error("satisfier_hint", error)
                            .for_resource(target_id.clone())
                    })
            })
        })
    }
}

// -- WasmProviderNormalizer --

pub struct WasmProviderNormalizer {
    pub(super) instance: Arc<FailFastInstance>,
}

type WitStateAttributeUpdate = (ResourceId, Vec<(String, wit_types::PersistedValue)>);

trait StateValueAccess {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut SnapshotState>;
}

impl StateValueAccess for carina_core::provider::StateTransformMap {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut SnapshotState> {
        self.get_mut(id)
    }
}

#[cfg(test)]
impl StateValueAccess for HashMap<ResourceId, SnapshotState> {
    fn state_mut(&mut self, id: &ResourceId) -> Option<&mut SnapshotState> {
        self.get_mut(id)
    }
}

fn validate_wit_state_result_keys(
    operation: &'static str,
    expected_ids_by_key: &HashMap<String, ResourceId>,
    results: Vec<(String, wit_types::PersistedState)>,
) -> ProviderResult<Vec<WitStateAttributeUpdate>> {
    let expected_keys = expected_ids_by_key.keys().cloned().collect::<BTreeSet<_>>();
    let mut returned = HashMap::with_capacity(results.len());
    let mut duplicates = BTreeSet::new();
    for (key, state) in results {
        if returned.insert(key.clone(), state).is_some() {
            duplicates.insert(key);
        }
    }
    let returned_keys = returned.keys().cloned().collect::<BTreeSet<_>>();
    let missing = expected_keys
        .difference(&returned_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let unexpected = returned_keys
        .difference(&expected_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() || !unexpected.is_empty() || !duplicates.is_empty() {
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned invalid state keys",
            WasmOperationError {
                operation,
                source: WasmNormalizerResultKeysError {
                    missing,
                    unexpected,
                    duplicates,
                },
            },
        ));
    }

    let mut updates = Vec::with_capacity(expected_ids_by_key.len());
    for (key, id) in expected_ids_by_key {
        let Some(state) = returned.remove(key) else {
            return Err(ProviderError::internal_caused_by(
                "WASM provider returned invalid state keys",
                WasmOperationError {
                    operation,
                    source: WasmNormalizerResultKeysError {
                        missing: BTreeSet::from([key.clone()]),
                        unexpected: BTreeSet::new(),
                        duplicates: BTreeSet::new(),
                    },
                },
            ));
        };
        updates.push((id.clone(), state.attributes));
    }
    Ok(updates)
}

fn take_single_wit_state_result(
    operation: &'static str,
    id: ResourceId,
    results: Vec<(String, wit_types::PersistedState)>,
) -> ProviderResult<WitStateAttributeUpdate> {
    if results.len() != 1 {
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned an invalid state count",
            WasmOperationError {
                operation,
                source: WasmNormalizerResultCountError {
                    actual: results.len(),
                },
            },
        )
        .for_resource(id));
    }

    let Some((_, state)) = results.into_iter().next() else {
        return Err(ProviderError::internal_caused_by(
            "WASM provider returned an invalid state count",
            WasmOperationError {
                operation,
                source: WasmNormalizerResultCountError { actual: 0 },
            },
        )
        .for_resource(id));
    };
    Ok((id, state.attributes))
}

fn apply_wit_state_attribute_updates<M: StateValueAccess>(
    operation: &'static str,
    current_states: &mut M,
    updates: Vec<WitStateAttributeUpdate>,
) -> ProviderResult<()> {
    let decoded = updates
        .into_iter()
        .map(|(id, attributes)| {
            wasm_convert::wit_to_core_persisted_value_map(&attributes)
                .map(|attributes| (id.clone(), attributes))
                .map_err(|error| {
                    wasm_value_decode_provider_error(operation, error).for_resource(id)
                })
        })
        .collect::<ProviderResult<Vec<_>>>()?;

    for (id, attributes) in decoded {
        if let Some(state) = current_states.state_mut(&id) {
            state.attributes = attributes;
        }
    }
    Ok(())
}

fn desired_normalization_provider_error<S, D>(
    resources: &[carina_core::resource::Resource<
        S,
        carina_core::resource::NoPlaceholder,
        carina_core::resource::AbsoluteBindingAddress,
        D,
    >],
    error: carina_provider_protocol::normalization::NormalizationError,
    convert: impl FnOnce(carina_provider_protocol::normalization::NormalizationError) -> ProviderError,
) -> ProviderError {
    let id = error
        .resource_index
        .and_then(|index| resources.get(index))
        .map(|resource| resource.id.clone());
    let error = convert(error);
    match id {
        Some(id) => error.for_resource(id),
        None => error,
    }
}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [carina_core::resource::ProviderResource],
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        let instance = Arc::clone(&self.instance);
        Box::pin(async move {
            let batch =
                carina_core::desired::NormalizationBatch::new(resources).map_err(|error| {
                    desired_normalization_provider_error(resources, error, |error| {
                        wasm_value_encode_provider_error("normalize_desired", error)
                    })
                })?;
            let sent = desired_normalization::encode(&batch);
            // Acquire and drop the guarded store in this future; never nest block_on.
            let result = {
                let locked = instance.acquire("normalize_desired").await?;
                locked
                    .call("normalize_desired", async |bindings, store| {
                        bindings.call_normalize_desired(store, &sent).await
                    })
                    .await?
                    .map_err(wasm_convert::wit_to_core_provider_error)
            }?;
            desired_normalization::apply(resources, batch, result).map_err(|error| {
                desired_normalization_provider_error(resources, error, |error| {
                    wasm_value_decode_provider_error("normalize_desired", error)
                })
            })
        })
    }

    fn normalize_state<'a>(
        &'a self,
        current_states: &'a mut carina_core::provider::StateTransformMap,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_wit_states = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit = wasm_convert::core_to_wit_persisted_state(state)
                    .map_err(|error| wasm_value_encode_provider_error("normalize_state", error))?;
                match id.identity_state() {
                    ResourceIdentityState::Pending(_) => {
                        single_wit_states.push((id.clone(), key, wit));
                    }
                    ResourceIdentityState::Resolved(_) => {
                        if colliding_keys.contains(&key) {
                            single_wit_states.push((id.clone(), key, wit));
                        } else if let Some((previous_id, previous_wit)) =
                            resolved_by_key.remove(&key)
                        {
                            colliding_keys.insert(key.clone());
                            single_wit_states.push((previous_id, key.clone(), previous_wit));
                            single_wit_states.push((id.clone(), key, wit));
                        } else {
                            resolved_by_key.insert(key, (id.clone(), wit));
                        }
                    }
                }
            }

            let mut resolved_ids_by_key = HashMap::with_capacity(resolved_by_key.len());
            let mut resolved_wit_states = Vec::with_capacity(resolved_by_key.len());
            for (key, (id, state)) in resolved_by_key {
                resolved_ids_by_key.insert(key.clone(), id);
                resolved_wit_states.push((key, state));
            }

            if !resolved_wit_states.is_empty() {
                // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
                let result = {
                    let locked = self.instance.acquire("normalize_state").await?;
                    let result = locked
                        .call("normalize_state", async |bindings, store| {
                            bindings
                                .call_normalize_state(store, &resolved_wit_states)
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                };
                let result = result?;
                updates.extend(validate_wit_state_result_keys(
                    "normalize_state",
                    &resolved_ids_by_key,
                    result,
                )?);
            }

            // Pending IDs and resolved IDs that differ only by provider
            // instance can share a legacy wire key. Send one per call so the
            // guest's key-parsing HashMap cannot collapse distinct host IDs,
            // and ignore the guest's re-rendered key when correlating the
            // single result.
            for (id, key, wit_state) in single_wit_states {
                let wit_states = [(key, wit_state)];
                let result = {
                    let locked = self.instance.acquire("normalize_state").await?;
                    let result = locked
                        .call("normalize_state", async |bindings, store| {
                            bindings.call_normalize_state(store, &wit_states).await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                };
                let result = result?;
                updates.push(take_single_wit_state_result("normalize_state", id, result)?);
            }

            apply_wit_state_attribute_updates("normalize_state", current_states, updates)?;
            Ok(())
        })
    }

    fn hydrate_read_state<'a>(
        &'a self,
        current_states: &'a mut carina_core::provider::StateTransformMap,
        saved_attrs: &'a SavedAttrs,
    ) -> carina_core::provider::BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            let mut resolved_by_key = HashMap::new();
            let mut colliding_keys = HashSet::new();
            let mut single_batches = Vec::new();
            let mut updates = Vec::new();

            for (id, state) in current_states.iter() {
                let key = wasm_convert::resource_id_wire_key(id);
                let wit_state =
                    wasm_convert::core_to_wit_persisted_state(state).map_err(|error| {
                        wasm_value_encode_provider_error("hydrate_read_state current state", error)
                    })?;
                let wit_saved =
                    wasm_convert::core_saved_attrs_to_wit(saved_attrs, id).map_err(|error| {
                        wasm_value_encode_provider_error(
                            "hydrate_read_state saved attributes",
                            error,
                        )
                    })?;

                match id.identity_state() {
                    ResourceIdentityState::Pending(_) => {
                        single_batches.push((id.clone(), key, wit_state, wit_saved));
                    }
                    ResourceIdentityState::Resolved(_) => {
                        if colliding_keys.contains(&key) {
                            single_batches.push((id.clone(), key, wit_state, wit_saved));
                        } else if let Some((previous_id, previous_state, previous_saved)) =
                            resolved_by_key.remove(&key)
                        {
                            colliding_keys.insert(key.clone());
                            single_batches.push((
                                previous_id,
                                key.clone(),
                                previous_state,
                                previous_saved,
                            ));
                            single_batches.push((id.clone(), key, wit_state, wit_saved));
                        } else {
                            resolved_by_key.insert(key, (id.clone(), wit_state, wit_saved));
                        }
                    }
                }
            }

            let mut resolved_ids_by_key = HashMap::with_capacity(resolved_by_key.len());
            let mut resolved_wit_states = Vec::with_capacity(resolved_by_key.len());
            let mut resolved_wit_saved = Vec::new();
            for (key, (id, state, saved)) in resolved_by_key {
                resolved_ids_by_key.insert(key.clone(), id);
                resolved_wit_states.push((key.clone(), state));
                if let Some(saved) = saved {
                    resolved_wit_saved.push((key, saved));
                }
            }

            if !resolved_wit_states.is_empty() {
                // Plain `.await`, not a nested `block_on` — see `normalize_desired`.
                let result = {
                    let locked = self.instance.acquire("hydrate_read_state").await?;
                    let result = locked
                        .call("hydrate_read_state", async |bindings, store| {
                            bindings
                                .call_hydrate_read_state(
                                    store,
                                    &resolved_wit_states,
                                    &resolved_wit_saved,
                                )
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                };
                let result = result?;
                updates.extend(validate_wit_state_result_keys(
                    "hydrate_read_state",
                    &resolved_ids_by_key,
                    result,
                )?);
            }

            // As in `normalize_state`, one-resource calls avoid both pending-ID
            // and provider-instance wire-key collisions. Correlate the single
            // result by position and ignore the guest's re-rendered key.
            for (id, key, wit_state, wit_saved) in single_batches {
                let wit_states = [(key.clone(), wit_state)];
                let wit_saved = wit_saved
                    .map(|attrs| vec![(key, attrs)])
                    .unwrap_or_default();
                let result = {
                    let locked = self.instance.acquire("hydrate_read_state").await?;
                    let result = locked
                        .call("hydrate_read_state", async |bindings, store| {
                            bindings
                                .call_hydrate_read_state(store, &wit_states, &wit_saved)
                                .await
                        })
                        .await?;
                    result.map_err(wasm_convert::wit_to_core_provider_error)
                };
                let result = result?;
                updates.push(take_single_wit_state_result(
                    "hydrate_read_state",
                    id,
                    result,
                )?);
            }

            apply_wit_state_attribute_updates("hydrate_read_state", current_states, updates)?;
            Ok(())
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    #[cfg(test)]
    mod round6 {
        use super::*;
        #[tokio::test]
        async fn validation_failed_capacity_rebuild_does_not_retain_unsent_inputs() {
            let mut factory = rebuildable_state_test_factory()
                .await
                .expect("mock WASM fixture is required");
            let previous = (0..256)
                .map(|index| format!("previousCapacity{index:04}"))
                .collect::<Vec<_>>();
            let previous_refs = previous.iter().map(String::as_str).collect::<Vec<_>>();
            let input = crate::provider_text_fixtures::context(&previous_refs);
            let attrs = vec![(
                "probe".into(),
                wit_types::Value::StrVal(previous[0].clone()),
            )];
            let context = factory.init_instance.validate_config_context(&attrs);
            factory
                .init_instance
                .acquire_in_session(context, Some(input.session().guest_inputs()))
                .await
                .unwrap()
                .call(async |bindings, store| bindings.call_validate_config(store, &attrs).await)
                .await
                .unwrap()
                .unwrap();
            factory.init_instance.rebuild.component =
                Component::new(&factory.init_instance.rebuild.engine, "(component)").unwrap();
            let empty = crate::provider_text_fixtures::context(&[]);
            for index in 0..3 {
                let secret = format!("neverSentCapacity{index:04}");
                let incoming = crate::provider_text_fixtures::context(&[&secret]);
                let attrs = [("probe".into(), wit_types::Value::StrVal(secret))];
                let context = factory.init_instance.validate_config_context(&attrs);
                let result = factory
                    .init_instance
                    .acquire_in_session(context, Some(incoming.session().guest_inputs()))
                    .await;
                assert!(
                    result.is_err(),
                    "the replacement component cannot instantiate"
                );
                drop(result);
                let state = factory.init_instance.state.lock().await;
                assert!(
                    !state
                        .inputs
                        .would_exceed_secret_limit(&empty.session().guest_inputs(), 256),
                    "a failed rebuild never hands current inputs to the old guest"
                );
            }
        }

        #[tokio::test]
        async fn validation_inventory_limit_rebuilds_once_and_forgets_prior_values() {
            let factory = rebuildable_state_test_factory()
                .await
                .expect("mock WASM fixture is required");
            let instance = &factory.init_instance;
            // Raw alphanumeric values have one distinct spelling each.
            for index in 0..256 {
                let secret = format!("inventoryPrivate{index:04}");
                let input = crate::provider_text_fixtures::context(&[&secret]);
                let attrs = vec![("probe".into(), wit_types::Value::StrVal(secret))];
                let context = instance.validate_config_context(&attrs);
                instance
                    .acquire_in_session(context, Some(input.session().guest_inputs()))
                    .await
                    .unwrap()
                    .call(async |bindings, store| {
                        bindings.call_validate_config(store, &attrs).await
                    })
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert_eq!(
                instance.additional_instantiations.load(Ordering::Relaxed),
                0
            );
            let current = ["currentPrivateFirst", "currentPrivateSecond"];
            let input = crate::provider_text_fixtures::context(&current);
            let attrs = vec![("probe".into(), wit_types::Value::StrVal(current[0].into()))];
            let context = instance.validate_config_context(&attrs);
            instance
                .acquire_in_session(context, Some(input.session().guest_inputs()))
                .await
                .unwrap()
                .call(async |bindings, store| bindings.call_validate_config(store, &attrs).await)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                instance.additional_instantiations.load(Ordering::Relaxed),
                1
            );
            let state = instance.state.lock().await;
            let previous = (0..256)
                .map(|index| format!("inventoryPrivate{index:04}"))
                .collect::<Vec<_>>()
                .join(" ");
            let mut output = Vec::new();
            let mut writer = state.inputs.line_writer(&mut output);
            writer
                .push_str(&format!("{previous} {} {}\n", current[0], current[1]))
                .unwrap();
            writer.finish().unwrap();
            drop(writer);
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output, format!("{previous} (secret) (secret)\n"));
        }

        #[tokio::test]
        async fn review_round6_failed_validation_rebuild_keeps_previous_inputs_redacted() {
            const SECRET: &str = "round6-previous-validation-rebuild-private";
            let mut factory = rebuildable_state_test_factory()
                .await
                .expect("mock WASM fixture is required");
            let previous = crate::provider_text_fixtures::context(&[SECRET]);
            let attrs = vec![("probe".into(), wit_types::Value::StrVal(SECRET.into()))];
            let context = factory.init_instance.validate_config_context(&attrs);
            let first = factory
                .init_instance
                .acquire_in_session(context, Some(previous.session().guest_inputs()))
                .await
                .unwrap()
                .call(async |bindings, store| {
                    bindings.call_validate_config(store, &attrs).await?.unwrap();
                    Err::<(), wasmtime::Error>(wasmtime::Error::msg(SECRET))
                })
                .await;
            assert!(first.is_err());
            factory.init_instance.rebuild.component =
                Component::new(&factory.init_instance.rebuild.engine, "(component)").unwrap();
            let incoming =
                crate::provider_text_fixtures::context(&["round6-later-validation-private"]);
            let context = factory
                .init_instance
                .validate_config_context(&[("probe".into(), wit_types::Value::IntVal(2))]);
            let error = match factory
                .init_instance
                .acquire_in_session(context, Some(incoming.session().guest_inputs()))
                .await
            {
                Ok(_) => panic!("an empty component must fail to rebuild"),
                Err(error) => error,
            }
            .with_secrets_for_test(&incoming)
            .to_string();
            assert!(
                !error.contains(SECRET),
                "failed rebuild forgot previous inputs: {error}"
            );
            assert!(error.contains("(secret)"), "{error}");
            assert!(
                error.contains("Failed to rebuild WASM provider validation instance"),
                "{error}"
            );
        }

        const ROUND6_SESSION: &str = "round6/aws+session=token";
        const ROUND6_GITHUB: &str = "round6/github+credential=token";
        struct Round6Credentials;
        impl AwsCredentialSource for Round6Credentials {
            fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>> {
                Box::pin(async {
                    Ok(AwsCredentials::new(
                        "ROUND6PUBLICID",
                        "round6/private+key=value",
                        Some(ROUND6_SESSION.into()),
                        None,
                    ))
                })
            }
        }

        fn round6_credential_child(github: bool) {
            use wasmtime_wasi::cli::WasiCliView as _;
            use wasmtime_wasi::p2::bindings::cli::environment::Host as _;
            use wasmtime_wasi::p2::bindings::cli::stderr::Host as _;
            tokio::runtime::Runtime::new().unwrap().block_on(async {
            let context = crate::provider_text_fixtures::context(&[]);
            let engine = Engine::new(&wasmtime::Config::new()).unwrap();
            let mut network = amazonaws_network(ProviderKind::Untrusted);
            if let GuestNetwork::Runtime { capabilities, .. } = &mut network {
                *capabilities = if github { RuntimeCapabilities::GitHub } else { RuntimeCapabilities::Aws { credentials: Arc::new(Round6Credentials) } };
            }
            let mut store = GuestStore::new(&engine, local_provider_label("round6"), network, None, build_store_limits(), GuestCallBudget::PRODUCTION);
            store.set_configuration_secrets(context.session().guest_inputs());
            let error = store.call(|store| async move {
                let token = if github {
                    store.data_mut().cli().get_environment().unwrap().into_iter().find(|(key, _)| key == "GITHUB_TOKEN").unwrap().1
                } else {
                    <HostState as version_checked_bindings::aws_credentials::Host>::current_credentials(store.data_mut()).await.unwrap().session_token.unwrap()
                };
                let mut cli = store.data_mut().cli();
                let handle = cli.get_stderr().unwrap();
                let stderr = cli.table.get_mut(&handle).unwrap();
                let line = format!("guest stderr credential={token}; public=visible\n");
                assert!(stderr.check_write().unwrap() >= line.len());
                stderr.write(hyper::body::Bytes::from(line)).unwrap();
                stderr.flush().unwrap();
                ProviderError::api_error(format!("guest error credential={token}; public=visible"))
            }).await.unwrap();
            let error = error.with_secrets_for_test(&context);
            eprintln!("{error}");
        });
        }
        fn round6_credential_parent(github: bool) {
            const CHILD: &str = "CARINA_4305_ROUND6_CREDENTIAL_CHILD";
            if std::env::var_os(CHILD).is_some() {
                round6_credential_child(github);
                return;
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    std::thread::current().name().unwrap(),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("GITHUB_TOKEN", ROUND6_GITHUB)
                .output()
                .unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{stderr}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let secret = if github {
                ROUND6_GITHUB
            } else {
                ROUND6_SESSION
            };
            assert!(
                !stderr.contains(secret),
                "host credential leaked through guest store: {stderr}"
            );
            assert!(
                stderr.contains("guest stderr credential=(secret); public=visible"),
                "{stderr}"
            );
            assert!(
                stderr.contains("guest error credential=(secret); public=visible"),
                "{stderr}"
            );
        }
        #[test]
        fn review_round6_guest_stderr_masks_session_token() {
            round6_credential_parent(false);
        }
        #[test]
        fn review_round6_guest_stderr_masks_github_token() {
            round6_credential_parent(true);
        }

        #[test]
        fn review_round6_guest_stderr_and_message_mask_percent_encoding() {
            const SECRET: &str = "round6/path+private=value";
            let context = crate::provider_text_fixtures::context(&[SECRET]);
            for encoded in [
                "round6%2Fpath%2Bprivate%3Dvalue",
                "round6%2fpath%2bprivate%3dvalue",
            ] {
                let capture = tempfile::NamedTempFile::new().unwrap();
                let stderr = guest_stderr::GuestStderr::new(capture.reopen().unwrap());
                let _call = stderr.begin_call(&context.session().guest_inputs());
                let uri = format!("https://example.com/{encoded}/public");
                stderr
                    .write_line(&http_trace_line("GET", TraceUri::new(&uri), 1, 2, "200"))
                    .unwrap();
                let text = std::fs::read_to_string(capture.path()).unwrap();
                assert!(!text.contains(encoded), "encoded HTTP path leaked: {text}");
                assert!(text.contains("/(secret)/public"), "{text}");
                let error =
                    ProviderError::api_error(format!("provider message {encoded}; public=visible"))
                        .with_secrets_for_test(&context)
                        .to_string();
                assert!(!error.contains(encoded), "encoded message leaked: {error}");
                assert!(
                    error.contains("provider message (secret); public=visible"),
                    "{error}"
                );
            }
        }

        #[tokio::test]
        async fn review_round6_validation_reuses_one_instance_and_remembers_all_inputs() {
            use wasmtime_wasi::cli::WasiCliView as _;
            use wasmtime_wasi::p2::bindings::cli::stderr::Host as _;
            const CHILD: &str = "CARINA_4305_ROUND6_VALIDATION_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        std::thread::current().name().unwrap(),
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .output()
                    .unwrap();
                let text = String::from_utf8(output.stderr).unwrap();
                assert!(
                    output.status.success(),
                    "{text}\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                for secret in [
                    "round6-validation-first",
                    "round6-validation-second",
                    "round6-validation-third",
                ] {
                    assert!(
                        !text.contains(secret),
                        "validator stderr forgot a prior value: {text}"
                    );
                }
                assert_eq!(text.matches("(secret)").count(), 10, "{text}");
                assert_eq!(text.matches("public=visible").count(), 4, "{text}");
                return;
            }
            let factory = rebuildable_state_test_factory()
                .await
                .expect("mock WASM fixture is required");
            let instance = &factory.init_instance;
            let mut previous = Vec::new();
            let mut rendered = Vec::new();
            for secret in [
                "round6-validation-first",
                "round6-validation-first",
                "round6-validation-second",
                "round6-validation-third",
            ] {
                previous.push(secret);
                let input = crate::provider_text_fixtures::context(&[secret]);
                let attrs = vec![("probe".into(), wit_types::Value::StrVal(secret.into()))];
                let context = instance.validate_config_context(&attrs);
                let message = previous.join(" / ");
                instance
                    .acquire_in_session(context, Some(input.session().guest_inputs()))
                    .await
                    .unwrap()
                    .call(async |bindings, store| {
                        let result = bindings.call_validate_config(store, &attrs).await?;
                        let mut cli = store.data_mut().cli();
                        let handle = cli.get_stderr().unwrap();
                        let stderr = cli.table.get_mut(&handle).unwrap();
                        let line = format!("remembered {message}; public=visible\n");
                        assert!(stderr.check_write().unwrap() >= line.len());
                        stderr.write(hyper::body::Bytes::from(line)).unwrap();
                        stderr.flush().unwrap();
                        Ok::<_, wasmtime::Error>(result)
                    })
                    .await
                    .unwrap()
                    .unwrap();
                rendered.push(
                    ProviderError::api_error(previous.join(" / "))
                        .with_secrets_for_test(&input)
                        .to_string(),
                );
            }
            assert_eq!(
                1 + instance.additional_instantiations.load(Ordering::Relaxed),
                1,
                "all validations must reuse the initial component instance"
            );
            for error in rendered {
                for secret in &previous {
                    assert!(
                        !error.contains(secret),
                        "validator forgot earlier secret: {error}"
                    );
                }
                assert!(error.contains("(secret)"), "{error}");
            }
        }
    }
    thread_local! { static REBUILD_LOAD_PROBE: std::cell::RefCell<Option<WasmProviderLoadError>> = const { std::cell::RefCell::new(None) }; }
    pub(in crate::wasm_factory) fn record_rebuild_load_probe(error: &WasmProviderLoadError) {
        let probe = match error {
            WasmProviderLoadError::Instantiation(source) => {
                Some(WasmProviderLoadError::Instantiation(source.clone()))
            }
            WasmProviderLoadError::Metadata(source) => {
                Some(WasmProviderLoadError::Metadata(source.clone()))
            }
            _ => None,
        };
        REBUILD_LOAD_PROBE.with(|slot| *slot.borrow_mut() = probe);
    }
    struct RebuildLoadProbe(WasmProviderLoadError);
    impl RebuildLoadProbe {
        fn take() -> Self {
            Self(
                REBUILD_LOAD_PROBE
                    .with(|slot| slot.borrow_mut().take())
                    .expect("actual rebuild emits a typed load probe"),
            )
        }
        fn project_redacted<T>(
            &self,
            _context: &crate::provider_text_fixtures::Context,
            inspect: impl FnOnce(&WasmProviderLoadError) -> String,
        ) -> Option<String> {
            let _type = std::marker::PhantomData::<T>;
            Some(inspect(&self.0))
        }
    }

    use crate::provider_text_fixtures::*;
    mod assignability_edges_stage_a {
        use super::*;
        use carina_core::parser::SchemaTypeOwner;
        use carina_core::schema::{
            AssignabilityEdge, AttributeSchema, AttributeType, ReferenceForms, ResourceSchema,
            SchemaRegistry,
        };

        #[test]
        fn sdk_json_host_decode_and_factory_preserve_edges_in_loaded_catalog() {
            let identity = |kind: &str| TypeIdentity::new(Some("semantic"), ["Bucket"], kind);
            let resource = ["A", "B", "C"]
                .into_iter()
                .fold(ResourceSchema::new("test.Bucket"), |schema, kind| {
                    schema.attribute(
                        AttributeSchema::optional(
                            kind,
                            AttributeType::refined_string(Some(identity(kind)), None, None, None),
                        )
                        .build(),
                    )
                })
                .validate()
                .unwrap();
            let edges = vec![
                AssignabilityEdge::new(identity("A"), identity("B")).unwrap(),
                AssignabilityEdge::new(identity("B"), identity("C")).unwrap(),
            ];
            let guest = ValidatedProviderSchemaWitness::new(
                vec![resource],
                ValidatedConfigSchema::default(),
                ReferenceForms::new(),
                edges.clone(),
            )
            .unwrap();
            let json = carina_plugin_sdk::provider_schema_to_json(&guest).unwrap();
            let decoded =
                wasm_convert::json_to_provider_schema(&json, ValidatedConfigSchema::default())
                    .unwrap();
            assert_eq!(decoded.assignability_edges(), edges.as_slice());

            // Use the host's real metadata factory to cross the final seam into
            // LoadedProviderSnapshot without instantiating a WASM runtime.
            let factory = SchemaOnlyWasmProviderFactory {
                name: "host-owner".into(),
                display_name: DiagnosticText::new("Host edge fixture"),
                version: ProviderKey::from("0.0.0"),
                schema_witness: Arc::new(decoded),
                cached_config_completions: Default::default(),
                cached_identity_attributes: Vec::new(),
            };
            let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(factory)];
            let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
                Arc::new(factories),
                SchemaRegistry::new(),
                carina_core::EnvironmentSnapshot::empty(),
            );
            let catalog = &loaded.provider_context().custom_types;
            let owner = SchemaTypeOwner::Provider("host-owner".into());
            assert!(catalog.reachable(&owner, &identity("A"), &identity("B")));
            assert!(catalog.reachable(&owner, &identity("A"), &identity("C")));
            assert!(!catalog.reachable(&owner, &identity("C"), &identity("A")));
            assert!(!catalog.reachable(
                &SchemaTypeOwner::Provider("semantic".into()),
                &identity("A"),
                &identity("C"),
            ));
        }
    }

    use super::*;
    use carina_core::provider::SentRecord;
    use carina_core::resource::{ConcreteValue, Current, DeferredValue, ModuleSource};
    use carina_core::test_utils::loopback::LoopbackListener;

    use std::borrow::Cow;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use wasm_encoder::{Component as EncoderComponent, CustomSection};
    use wasmtime::component::ResourceTable;
    use wasmtime_wasi::WasiCtxView;

    fn empty_ready_config(factory: &WasmProviderFactory) -> ProviderReadyConfig {
        prepare_test_config(factory, IndexMap::new())
            .expect("empty test configuration passes the provider boundary")
    }

    fn prepare_test_config(
        factory: &WasmProviderFactory,
        attributes: IndexMap<String, Value>,
    ) -> Result<ProviderReadyConfig, carina_core::executor::ProviderConfigPreparationError> {
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let mut parsed = carina_core::parser::ExpandedInferredFile::default();
                parsed.providers = vec![carina_core::parser::ProviderConfig {
                    name: factory.name().to_string(),
                    attributes,
                    default_tags: IndexMap::new(),
                    source: None,
                    version: None,
                    revision: None,
                    unresolved_attributes: IndexMap::new(),
                    binding: None,
                }];
                let configs = carina_core::parser::resolve_provider_attributes_with_remote(
                    &parsed,
                    &HashMap::new(),
                    type_checks,
                )
                .expect("test configuration resolves");
                carina_core::executor::prepare_provider_ready_config(
                    factory,
                    configs.iter().next().unwrap(),
                )
            },
        )
    }

    #[test]
    fn normalization_operation_wrapper_preserves_sources_and_renders_cause_once() {
        use carina_provider_protocol::normalization::{NormalizationError, NormalizationErrorKind};
        let id = carina_provider_protocol::types::ResourceId {
            provider: "mock".into(),
            resource_type: "example".into(),
            identity: "duration-error".into(),
        };
        let source = NormalizationError::new(id, "value", NormalizationErrorKind::Conversion)
            .with_source(carina_core::protocol_convert::ProtocolConvertError::DurationOutOfRange);
        let error =
            wasm_value_encode_provider_error("normalize_desired", source).into_host_for_test();
        let normalization = error
            .detail()
            .cause()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<NormalizationError>()
            .unwrap();
        assert!(
            normalization
                .source()
                .unwrap()
                .is::<carina_core::protocol_convert::ProtocolConvertError>()
        );
        let rendered = error.to_string();
        let reason =
            carina_core::protocol_convert::ProtocolConvertError::DurationOutOfRange.to_string();
        assert_eq!(rendered.matches(&reason).count(), 1, "{rendered}");
    }

    #[test]
    fn desired_normalization_errors_retain_the_original_provider_instance() {
        use carina_core::desired::NormalizationBatch;
        for encoding in [true, false] {
            let mut resources: Vec<carina_core::resource::Resource> = ["east", "west"]
                .into_iter()
                .map(|instance| {
                    carina_core::resource::Resource::with_provider(
                        "mock",
                        "example",
                        "same-id",
                        Some(instance.into()),
                    )
                    .with_attribute(
                        "value",
                        Value::Concrete(ConcreteValue::String("before".into())),
                    )
                })
                .collect();
            let error = if encoding {
                resources[1].attributes.insert(
                    "value".into(),
                    Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
                        u64::MAX,
                    ))),
                );
                NormalizationBatch::new(&resources).err().unwrap()
            } else {
                let batch = NormalizationBatch::new(&resources).unwrap();
                let mut returned = desired_normalization::encode(&batch);
                returned[1].attributes[0].1 = wit_types::DesiredValue::MapVal("[]".into());
                desired_normalization::apply(&mut resources, batch, returned).unwrap_err()
            };
            assert_eq!(error.resource_index, Some(1));
            let error = desired_normalization_provider_error(&resources, error, |error| {
                if encoding {
                    wasm_value_encode_provider_error("normalize_desired", error)
                } else {
                    wasm_value_decode_provider_error("normalize_desired", error)
                }
            });
            assert_eq!(
                error
                    .detail()
                    .resource_id
                    .as_deref()
                    .map(|id| id.as_resource_id()),
                Some(&resources[1].id)
            );
            assert_ne!(
                error
                    .detail()
                    .resource_id
                    .as_deref()
                    .map(|id| id.as_resource_id()),
                Some(&resources[0].id)
            );
            assert!(error.detail().cause().is_some());
        }
    }

    const HTTP_TRACE_SECRET_URI: &str = "https://user:PASSSECRET@bucket.s3.amazonaws.com/key?X-Amz-Credential=AKIDSECRET%2F...&X-Amz-Signature=SIGSECRET&X-Amz-Security-Token=TOKSECRET#FRAGSECRET";
    const HTTP_TRACE_SAFE_URI: &str = "https://bucket.s3.amazonaws.com/key?X-Amz-Credential=<redacted>&X-Amz-Signature=<redacted>&X-Amz-Security-Token=<redacted>";

    fn assert_http_trace_has_no_secrets(line: &str) {
        for secret in [
            "PASSSECRET",
            "AKIDSECRET",
            "SIGSECRET",
            "TOKSECRET",
            "FRAGSECRET",
        ] {
            assert!(!line.contains(secret), "trace leaked {secret}: {line}");
        }
    }

    #[test]
    fn http_trace_line_redacts_uri_credentials_and_keeps_metrics() {
        let line = http_trace_line("GET", TraceUri::new(HTTP_TRACE_SECRET_URI), 1, 2, "200");

        assert_http_trace_has_no_secrets(&line);
        assert_eq!(
            line,
            format!(
                "carina-host-http-trace method=GET uri={HTTP_TRACE_SAFE_URI} queue_ms=1 handler_ms=2 status=200"
            )
        );
    }

    #[test]
    fn http_trace_phases_line_redacts_uri_credentials_and_keeps_metrics() {
        let line =
            http_trace_phases_line("GET", TraceUri::new(HTTP_TRACE_SECRET_URI), [1, 2, 3, 4]);

        assert_http_trace_has_no_secrets(&line);
        assert_eq!(
            line,
            format!(
                "carina-host-http-trace-phases method=GET uri={HTTP_TRACE_SAFE_URI} tcp_connect_ms=1 tls_handshake_ms=2 http_handshake_ms=3 send_request_ms=4"
            )
        );
    }

    struct UnusedAwsCredentialSource;

    impl AwsCredentialSource for UnusedAwsCredentialSource {
        fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>> {
            Box::pin(async {
                panic!("this source is only a capability marker for network policy tests")
            })
        }
    }

    struct UnusedAwsCredentialProvisioner;

    impl AwsCredentialProvisioner for UnusedAwsCredentialProvisioner {
        fn provision(
            &self,
        ) -> BoxFuture<'_, Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>> {
            let source: Arc<dyn AwsCredentialSource> = Arc::new(UnusedAwsCredentialSource);
            Box::pin(async move { Ok(source) })
        }
    }

    struct FailingAwsCredentialProvisioner;

    impl AwsCredentialProvisioner for FailingAwsCredentialProvisioner {
        fn provision(
            &self,
        ) -> BoxFuture<'_, Result<Arc<dyn AwsCredentialSource>, AwsCredentialError>> {
            Box::pin(async {
                Err(AwsCredentialError::NotFound(
                    "host provisioning failed before instantiation".to_string(),
                ))
            })
        }
    }

    fn aws_capabilities() -> RuntimeCapabilities {
        RuntimeCapabilities::Aws {
            credentials: Arc::new(UnusedAwsCredentialSource),
        }
    }

    fn test_load_identity(name: &str) -> ProviderLoadIdentity {
        ProviderLoadIdentity::for_test(name, ProviderTrustClass::Untrusted)
    }

    fn local_provider_label(name: &str) -> Arc<ProviderLabel> {
        Arc::new(ProviderLabel {
            name: name.to_string(),
            source: ProviderLoadSource::local_file(
                &carina_core::paths::ResolvedPath::new(format!("/test-providers/{name}.wasm"))
                    .unwrap(),
                carina_core::paths::DisplayPath::new(format!("{name}.wasm")).unwrap(),
            ),
        })
    }

    fn remote_provider_label(name: &str, source: &str) -> Arc<ProviderLabel> {
        let source = match source.parse::<carina_core::provider_source::ProviderSource>() {
            Ok(carina_core::provider_source::ProviderSource::Remote(source)) => source.source_key(),
            Ok(carina_core::provider_source::ProviderSource::File(_)) => {
                panic!("test source must be remote")
            }
            Err(error) => panic!("invalid test provider source: {error:?}"),
        };
        Arc::new(ProviderLabel {
            name: name.to_string(),
            source: ProviderLoadSource::Remote { source },
        })
    }

    fn test_network(kind: ProviderKind, hosts: &[&str]) -> GuestNetwork {
        let declaration_json = serde_json::json!({
            "version": 1,
            "hosts": hosts,
        })
        .to_string();
        let mut component = EncoderComponent::new();
        component.section(&CustomSection {
            name: Cow::Borrowed("carina:egress"),
            data: Cow::Owned(declaration_json.into_bytes()),
        });
        let class = match kind {
            ProviderKind::Aws => ProviderTrustClass::Aws,
            ProviderKind::GitHub => ProviderTrustClass::GitHub,
            ProviderKind::Untrusted => ProviderTrustClass::Untrusted,
        };
        let load = VerifiedLoad::for_test(
            component.finish(),
            ProviderLoadIdentity::for_test("network-test", class),
        );
        let declaration = VerifiedEgressDeclaration::read(&load).unwrap();
        let locked = test_explicit_egress(&declaration);
        let egress =
            ConsentedEgress::from_lock(Some(&locked), declaration, load.carina_rs_signed())
                .unwrap();
        let capabilities = match kind {
            ProviderKind::Aws => aws_capabilities(),
            ProviderKind::GitHub => RuntimeCapabilities::GitHub,
            ProviderKind::Untrusted => RuntimeCapabilities::Untrusted,
        };
        GuestNetwork::Runtime {
            capabilities,
            egress,
        }
    }

    fn amazonaws_network(kind: ProviderKind) -> GuestNetwork {
        test_network(kind, &["**.amazonaws.com", "**.amazonaws.com.cn"])
    }

    #[tokio::test]
    async fn runtime_factory_binds_lock_record_to_the_same_loaded_artifact() {
        let component_with_host = |host: &str| {
            let declaration_json = serde_json::json!({
                "version": 1,
                "hosts": [host],
            })
            .to_string();
            let mut component = EncoderComponent::new();
            component.section(&CustomSection {
                name: Cow::Borrowed("carina:egress"),
                data: Cow::Owned(declaration_json.into_bytes()),
            });
            component.finish()
        };
        let first = VerifiedLoad::for_test(
            component_with_host("first.example.com"),
            ProviderLoadIdentity::for_test("network-test", ProviderTrustClass::Untrusted),
        );
        let first_declaration = VerifiedEgressDeclaration::read(&first).unwrap();
        let first_lock = test_explicit_egress(&first_declaration);
        let second = VerifiedLoad::for_test(
            component_with_host("second.example.com"),
            ProviderLoadIdentity::for_test("network-test", ProviderTrustClass::Untrusted),
        );
        let cache = tempfile::tempdir().unwrap();

        let result = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
            second,
            cache.path(),
            Some(&first_lock),
            &UnusedAwsCredentialProvisioner,
        )
        .await;

        assert!(matches!(
            result,
            Err(WasmProviderLoadError::EgressConsent(
                carina_provider_resolver::EgressConsentError::DeclarationMismatch
            ))
        ));
    }

    #[tokio::test]
    async fn aws_runtime_without_credential_import_stops_before_provisioning() {
        let declaration_json = serde_json::json!({
            "version": 1,
            "hosts": [],
        })
        .to_string();
        let mut component = EncoderComponent::new();
        component.section(&CustomSection {
            name: Cow::Borrowed("carina:egress"),
            data: Cow::Owned(declaration_json.into_bytes()),
        });
        let load = VerifiedLoad::for_test(
            component.finish(),
            ProviderLoadIdentity::for_test("aws", ProviderTrustClass::Aws),
        );
        let declaration = VerifiedEgressDeclaration::read(&load).unwrap();
        let locked = test_explicit_egress(&declaration);
        let cache = tempfile::tempdir().unwrap();

        let result = WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
            load,
            cache.path(),
            Some(&locked),
            &FailingAwsCredentialProvisioner,
        )
        .await;

        assert!(matches!(
            result,
            Err(WasmProviderLoadError::AwsCredentialsImportMissing)
        ));
    }

    #[test]
    fn aws_credential_load_error_is_a_terminal_diagnostic() {
        let error = WasmProviderLoadError::AwsCredentials(AwsCredentialError::NotFound(
            "credential diagnostic".to_string(),
        ));

        assert_eq!(error.to_string(), "credential diagnostic");
        assert!(std::error::Error::source(&error).is_none());
    }

    async fn rebuildable_state_test_factory() -> Option<WasmProviderFactory> {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return None;
        };
        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let private = tempfile::tempdir().expect("create fallback compiled cache");
        let cache = std::env::var_os("CARINA_TEST_WASM_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| private.path().to_path_buf());
        let load = VerifiedLoad::for_test(&wasm_bytes, test_load_identity("mock"));
        let declaration = VerifiedEgressDeclaration::read(&load).unwrap();
        let locked = test_explicit_egress(&declaration);
        Some(
            WasmProviderFactory::from_bytes_with_cache_dir_and_egress(
                load,
                &cache,
                Some(&locked),
                &UnusedAwsCredentialProvisioner,
            )
            .await
            .expect("mock WASM provider should load"),
        )
    }

    #[tokio::test]
    async fn schema_only_factory_is_uncached_and_cannot_create_a_runtime() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };
        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let user_dirs = carina_core::TestUserDirs::new();
        assert!(!user_dirs.compiled_wasm_cache_dir().exists());

        let factory = SchemaOnlyWasmProviderFactory::from_bytes(
            VerifiedLoad::for_test(wasm_bytes, test_load_identity("mock")),
            &user_dirs,
        )
        .await
        .expect("schema-only mock provider should load");

        assert_eq!(factory.name(), "mock");
        assert!(!factory.schemas().is_empty());
        assert!(
            !user_dirs.compiled_wasm_cache_dir().exists(),
            "metadata-only loading must not publish a precompile cache entry"
        );

        let saved_configs: carina_core::parser::SavedProviderConfigs =
            serde_json::from_str("[]").expect("empty saved provider config fixture");
        let configs = carina_core::parser::ResolvedProviderConfigs::try_from(saved_configs)
            .expect("empty provider config fixture crosses the checked saved seam");
        let config = carina_core::executor::prepare_provider_ready_config(
            &factory,
            configs.provider_or_implicit("mock").as_ref(),
        )
        .expect("empty mock config is provider-ready");
        let error = match factory.initialize_fixture(None, &config).await {
            Ok(_) => panic!("schema-only metadata must not create a runtime provider"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("schema-only provider metadata cannot initialize a provider runtime")
        );
    }

    #[tokio::test]
    async fn http_test_route_is_retained_for_validation_rebuilds() {
        let Some(factory) = rebuildable_state_test_factory().await else {
            return;
        };
        let destination = SocketAddr::from((Ipv4Addr::LOCALHOST, 43210));
        let factory = factory.with_http_test_route("phase-test.invalid", destination);

        let route = factory
            .init_instance
            .rebuild
            .http_test_route
            .as_ref()
            .expect("validation rebuild context must retain the test route");
        let retained_destination =
            match route.resolution_for("phase-test.invalid", destination.port(), false) {
                Some(ResolvedAddresses::HttpTestRoute(destination)) => destination.address(),
                Some(ResolvedAddresses::Standard(_)) => {
                    panic!("the loopback test route must carry the exemption witness")
                }
                None => panic!("the retained test route must match"),
            };
        assert_eq!(retained_destination, destination);
    }

    #[test]
    fn protocol_v20_wit_surface_compiles() {
        fn assert_type<T>() {}

        assert_type::<wit_types::ReferenceScopeValues>();
        assert_type::<wit_types::ReferenceScope>();
        assert_type::<wit_types::InitializeResult>();
        assert_type::<wit_types::Observation>();
        assert_type::<wit_types::SentAttributes>();
        assert_type::<wit_types::PendingCreate>();
        assert_type::<wit_types::CreateInFlight>();
        assert_type::<wit_types::CreateStep>();
        assert_type::<wit_types::CreateVerdict>();
        assert_type::<wit_types::PollVerdict>();
        assert_type::<wit_types::ResumePending>();
        assert_type::<wit_types::ResumeInFlight>();
        assert_type::<wit_types::ResumeStep>();
        assert_type::<wit_types::ResumeVerdict>();
    }

    fn test_string_value(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn boundary_resource() -> (ResourceId, wit_types::ResourceId, wit_types::CreateRequest) {
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let id = ResourceId::with_provider_identity("mock", "test.Resource", "main", None);
                let mut resource = Resource::from_id(id.clone());
                resource.set_attr("name", test_string_value("example"));
                let wit_id = wasm_convert::core_to_wit_resource_id(&id);
                let bindings = carina_core::binding_index::ResolvedBindings::default();
                let gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
                let schemas = carina_core::schema::SchemaRegistry::new();
                let context = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &carina_core::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );
                // This fixture uses a ready no-op normalizer, so preparation has no
                // pending I/O and can be polled once in both sync and async tests.
                let mut prepare = std::pin::pin!(carina_core::executor::prepare_create_request(
                    resource, &context
                ));
                let std::task::Poll::Ready(request) = prepare
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                else {
                    panic!("no-op fixture preparation must complete immediately")
                };
                let wit_request = wasm_convert::core_to_wit_create_request(
                    &request.expect("checked boundary fixture"),
                );
                (id, wit_id, wit_request)
            },
        )
    }

    fn sent_name_only() -> Sent {
        Sent::Only(["name".to_string()].into_iter().collect())
    }

    fn malformed_wit_attributes() -> Vec<(String, wit_types::Value)> {
        vec![(
            "broken".to_string(),
            wit_types::Value::ListVal("not-json".to_string()),
        )]
    }

    fn decode_failed_create_step(
        id: &ResourceId,
    ) -> wasm_convert::IdentifierReportedReadDecodeFailure {
        let step = wit_types::CreateStep::Success(wit_types::AppliedState {
            identifier: "decoded-created-id".to_string(),
            attributes: malformed_wit_attributes(),
        });
        match wasm_convert::wit_to_core_create_step(step, id)
            .expect("the valid identifier makes the attribute failure recoverable")
        {
            wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed attributes unexpectedly decoded")
            }
        }
    }

    fn decode_failed_identifier_reported_verdict(
        id: &ResourceId,
    ) -> wasm_convert::IdentifierReportedReadDecodeFailure {
        let error = ProviderError::internal("guest create failure");
        let verdict = wit_types::PollVerdict::IdentifierReported(wit_types::CreateIdentified {
            identifier: "decoded-created-id".to_string(),
            read: wit_types::Observation::Read(wit_types::ReadOutcome::Found(
                wit_types::AppliedState {
                    identifier: "decoded-created-id".to_string(),
                    attributes: malformed_wit_attributes(),
                },
            )),
            sent: wit_types::SentAttributes::Only(vec!["name".to_string()]),
            error: wasm_convert::wire_to_wit_provider_error(error.to_wire()),
        });
        match wasm_convert::wit_to_core_poll_verdict(verdict, id)
            .expect("the valid identifier makes the observation failure recoverable")
        {
            wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed observation unexpectedly decoded")
            }
        }
    }

    fn assert_identifier_reported_after_decode_failure(progress: CreateProgress) {
        let CreateProgress::Verdict(failure) = progress else {
            panic!("post-identifier decode failure must be an identifier-reported verdict")
        };
        match failure.into_kind() {
            CreateFailureKind::IdentifierReported {
                identified,
                error: _,
            } => {
                assert_eq!(identified.identifier().as_str(), "decoded-created-id");
                match identified.observation() {
                    CreateObservation::Observed(state) => {
                        panic!("decode failure became an observed create: {state:?}")
                    }
                    CreateObservation::Unobserved(reason) => match reason {
                        UnobservedReason::NotFound => {
                            panic!("decode failure became a not-found observation")
                        }
                        UnobservedReason::ReadFailed(error) => {
                            assert!(!error.to_string().is_empty())
                        }
                    },
                }
            }
            CreateFailureKind::NoResource(error) => {
                panic!("post-identifier decode failure became not_created: {error:?}")
            }
            CreateFailureKind::OutcomeUnknown { unknown, error } => {
                panic!(
                    "post-identifier decode failure lost identifier as unknown for {}: {error:?}",
                    unknown.id()
                )
            }
            CreateFailureKind::InFlight { error, .. } => {
                panic!("post-identifier decode failure stayed in flight: {error:?}")
            }
        }
    }

    fn assert_identifier_reported_after_resume_decode_failure(progress: ResumeProgress) {
        let ResumeProgress::Verdict(failure) = progress else {
            panic!("post-identifier resume decode failure must be an identifier-reported verdict")
        };
        match failure.into_kind() {
            ResumeFailureKind::IdentifierReported {
                identified,
                error: _,
            } => {
                assert_eq!(identified.identifier().as_str(), "decoded-created-id");
                match identified.observation() {
                    CreateObservation::Observed(state) => {
                        panic!("resume decode failure became an observed create: {state:?}")
                    }
                    CreateObservation::Unobserved(reason) => match reason {
                        UnobservedReason::NotFound => {
                            panic!("resume decode failure became a not-found observation")
                        }
                        UnobservedReason::ReadFailed(error) => {
                            assert!(!error.to_string().is_empty())
                        }
                    },
                }
            }
            ResumeFailureKind::NoResource(error) => {
                panic!("post-identifier resume decode failure became no-resource: {error:?}")
            }
            ResumeFailureKind::InFlight { error, .. } => {
                panic!("post-identifier resume decode failure stayed in flight: {error:?}")
            }
        }
    }

    struct PendingFixtureProvider {
        captured_attempt: Option<std::sync::Mutex<Option<CreateAttempt>>>,
    }

    impl Provider for PendingFixtureProvider {
        fn name(&self) -> &str {
            "pending-fixture"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            Box::pin(async move { Ok(ReadOutcome::not_found(id)) })
        }

        fn read_data_source(
            &self,
            resource: &carina_core::provider::ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(
                async move { Ok(State::<carina_core::resource::NoPlaceholder>::not_found(id)) },
            )
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            attempt: CreateAttempt,
        ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
            if let Some(captured) = &self.captured_attempt {
                *captured.lock().unwrap() = Some(attempt);
                return Box::pin(std::future::pending());
            }
            Box::pin(async move {
                let pending = attempt
                    .issued(
                        AppliedIdentifier::new("issued-id").expect("valid identifier"),
                        sent_name_only(),
                    )
                    .pending(proto::PendingToken::new("original-token"), 1);
                Err(pending.preserve(ProviderError::timeout("fixture remains pending")))
            })
        }

        fn resume_create(
            &self,
            request: ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
            Box::pin(async move {
                let (resume, create_request) = request.into_parts();
                drop(create_request);
                Err(resume.preserve(ProviderError::internal(
                    "resume is not used by this fixture",
                )))
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            _attempt: carina_core::provider::UpdateAttempt,
        ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
            Box::pin(async {
                Err(ProviderError::internal(
                    "update is not used by this fixture",
                ))
            })
        }

        fn delete(
            &self,
            _attempt: carina_core::provider::DeleteAttempt,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async {
                Err(ProviderError::internal(
                    "delete is not used by this fixture",
                ))
            })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn create_attempt_for_test(id: ResourceId) -> CreateAttempt {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = carina_core::schema::SchemaRegistry::new();
        let provider = std::sync::Arc::new(PendingFixtureProvider {
            captured_attempt: Some(std::sync::Mutex::new(None)),
        });
        let mut resource = Resource::from_id(id);
        resource.set_attr("name", test_string_value("example"));
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let preparation = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &[],
                    &carina_core::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );
                let mut prepare = std::pin::pin!(carina_core::executor::prepare_create_request(
                    resource,
                    &preparation
                ));
                let Poll::Ready(request) = prepare
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                else {
                    panic!("fixture preparation must complete synchronously");
                };
                let request = request.expect("fixture passes provider preparation");
                // Pure dispatch fixture with no schema input rules.
                let prepared = carina_core::replacement::create_permit_for_test(request);
                let provider_boundary = crate::provider_text_fixtures::boundary(Box::new(
                    std::sync::Arc::clone(&provider),
                ));
                let (create, _cancellation) =
                    carina_core::executor::dispatch_create(&provider_boundary, prepared);
                let mut create = std::pin::pin!(create);
                assert!(
                    create
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            },
        );
        provider
            .captured_attempt
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .take()
            .unwrap()
    }

    async fn pending_create_with_issued_identifier() -> PendingCreate {
        let (id, wit_id, wit_request) = boundary_resource();
        let mut resource = Resource::from_id(id.clone());
        resource.set_attr("name", test_string_value("example"));
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = carina_core::schema::SchemaRegistry::new();
        let provider_types = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(Vec::new()),
            schemas.clone(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        carina_core::validation::deferred_types::with_session_for_test_async(
            provider_types.provider_context(),
            async |type_checks| {
                let preparation = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &module_gate,
                    &[],
                    &carina_core::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );
                let request =
                    carina_core::executor::prepare_create_request(resource, &preparation).await;
                let request = request.expect("fixture resource passes provider preparation");
                let provider_boundary =
                    crate::provider_text_fixtures::boundary(Box::new(PendingFixtureProvider {
                        captured_attempt: None,
                    }));
                let prepared = carina_core::replacement::create_permit_for_test(request);
                let (create, _cancellation) =
                    carina_core::executor::dispatch_create(&provider_boundary, prepared);
                let failure = create.await.expect_err("fixture create remains pending");
                let pending = match failure.into_kind() {
                    CreateFailureKind::InFlight {
                        pending,
                        error: _error,
                    } => pending.into_fresh().unwrap_or_else(|_| {
                        panic!("fixture provider returned a persisted continuation")
                    }),
                    CreateFailureKind::IdentifierReported {
                        identified: _identified,
                        error,
                    } => {
                        panic!("fixture create was classified identifier-reported: {error:?}")
                    }
                    CreateFailureKind::NoResource(error) => {
                        panic!("fixture create was not pending: {error:?}")
                    }
                    CreateFailureKind::OutcomeUnknown { unknown, error } => {
                        panic!(
                            "fixture create outcome was unknown for {}: {error:?}",
                            unknown.id()
                        )
                    }
                };
                PendingCreate {
                    id,
                    wit_id,
                    wit_request,
                    pending,
                }
            },
        )
        .await
    }

    fn pending_create_without_identifier() -> PendingCreate {
        let (id, wit_id, wit_request) = boundary_resource();
        let pending = create_attempt_for_test(id.clone()).pending(
            proto::PendingToken::new("original-token"),
            sent_name_only(),
            1,
        );
        PendingCreate {
            id,
            wit_id,
            wit_request,
            pending,
        }
    }

    fn persisted_resume_with_identifier(identifier: Option<&str>) -> InFlightCreate {
        let (id, _wit_id, _wit_request) = boundary_resource();
        let identity = id
            .identity()
            .cloned()
            .expect("resume fixture identity is resolved");
        let mut row = serde_json::to_value(carina_state::ResourceState::new(
            &id.resource_type,
            identity,
            &id.provider,
        ))
        .expect("fixture row serializes");
        row["current"] = serde_json::json!({
            "kind": "create_in_flight",
            "envelope": {
                "token": "original-token",
                "identifier": identifier,
                "write_only": {},
                "secrets": {},
            },
            "started_at": "2026-10-23T12:34:56Z",
        });
        let mut document =
            serde_json::to_value(carina_state::StateFile::new()).expect("fixture state serializes");
        document["resources"] = serde_json::json!([row]);
        let loaded = carina_state::state::check_and_migrate(&document.to_string())
            .expect("v15 fixture passes the public state loader")
            .into_state();
        match loaded.build_state_for_resource(&id) {
            Current::CreateInFlight(pending) => pending,
            Current::Absent(state) => panic!("resume fixture loaded as absent: {state:?}"),
            Current::Present(state) => panic!("resume fixture loaded as present: {state:?}"),
            Current::MissingIdentifier(state) => {
                panic!("resume fixture loaded without an identifier: {state:?}")
            }
            Current::CreateOutcomeUnknown(unknown) => {
                panic!("resume fixture loaded as outcome unknown: {unknown:?}")
            }
        }
    }

    fn persisted_resume_with_issued_identifier() -> InFlightCreate {
        persisted_resume_with_identifier(Some("issued-id"))
    }

    fn persisted_resume_without_identifier() -> InFlightCreate {
        persisted_resume_with_identifier(None)
    }

    async fn resume_witness(pending: InFlightCreate) -> CoreResume {
        let mut resource = Resource::from_id(pending.id().clone());
        resource.set_attr("name", test_string_value("example"));
        let bindings = carina_core::binding_index::ResolvedBindings::default();
        let gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
        let provider_configs = [];
        let normalizer = carina_core::provider::NoopNormalizer;
        let factories = [];
        let schemas = carina_core::schema::SchemaRegistry::new();
        let provider_types = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(Vec::new()),
            schemas.clone(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        carina_core::validation::deferred_types::with_session_for_test_async(
            provider_types.provider_context(),
            async |type_checks| {
                let context = carina_core::executor::ProviderPreparationContext::new(
                    &bindings,
                    &gate,
                    &provider_configs,
                    &normalizer,
                    &factories,
                    &schemas,
                    type_checks,
                );
                let request =
                    carina_core::executor::prepare_create_request(resource, &context).await;
                let request =
                    request.expect("resume fixture request must pass provider preparation");
                let (resume, _request) = ResumeCreateRequest::for_test(pending, request)
                    .expect("resume fixture route must match")
                    .into_parts();
                resume
            },
        )
        .await
    }

    fn assert_original_polled_record(
        progress: CreateProgress,
        expected_id: &ResourceId,
        expected_started_at: &carina_core::provider::Timestamp,
    ) {
        match progress {
            CreateProgress::Verdict(failure) => match failure.into_kind() {
                CreateFailureKind::InFlight { pending, error } => {
                    assert_eq!(pending.id(), expected_id);
                    match pending.persistence() {
                        InFlightPersistence::Fresh(fresh) => {
                            assert_eq!(fresh.token().expose_for_provider_call(), "original-token");
                            assert_eq!(
                                fresh.identifier().map(AppliedIdentifier::as_str),
                                Some("issued-id")
                            );
                            assert_eq!(fresh.sent(), &sent_name_only());
                            assert_eq!(fresh.started_at(), expected_started_at);
                        }
                        InFlightPersistence::Persisted => {
                            panic!("fresh polling unexpectedly produced a persisted envelope")
                        }
                    }
                    assert_eq!(pending.token().expose_for_provider_call(), "original-token");
                    assert_eq!(
                        pending.identifier().map(AppliedIdentifier::as_str),
                        Some("issued-id")
                    );
                    match pending.sent() {
                        SentRecord::Request(Sent::Only(attributes)) => {
                            assert_eq!(attributes, ["name".to_string()].into_iter().collect());
                        }
                        SentRecord::Request(Sent::AllRequestAttributes) => {
                            panic!("the starting sent subset was replaced with all attributes")
                        }
                        SentRecord::Persisted(envelope) => {
                            panic!(
                                "fresh polling unexpectedly produced a persisted envelope: {envelope:?}"
                            )
                        }
                    }
                    assert!(!error.to_string().is_empty());
                }
                CreateFailureKind::NoResource(error) => {
                    panic!("issued create was downgraded to not_created: {error:?}")
                }
                CreateFailureKind::OutcomeUnknown { unknown, error } => {
                    panic!(
                        "issued create was downgraded to outcome unknown for {}: {error:?}",
                        unknown.id()
                    )
                }
                CreateFailureKind::IdentifierReported { identified, error } => {
                    panic!(
                        "invalid boundary response was accepted as identifier-reported for {}: {error:?}",
                        identified.identifier()
                    )
                }
            },
            CreateProgress::Done(outcome) => {
                panic!("invalid boundary response unexpectedly completed: {outcome:?}")
            }
            CreateProgress::Pending(pending) => {
                panic!(
                    "invalid boundary response unexpectedly replaced token with {:?}",
                    pending.pending.token()
                )
            }
        }
    }

    fn assert_original_resumed_record(progress: ResumeProgress, expected: &InFlightCreate) {
        match progress {
            ResumeProgress::Verdict(failure) => match failure.into_kind() {
                ResumeFailureKind::InFlight { pending, error } => {
                    assert_eq!(&pending, expected);
                    assert!(!error.to_string().is_empty());
                }
                ResumeFailureKind::NoResource(error) => {
                    panic!("issued resume was incorrectly classified no-resource: {error:?}")
                }
                ResumeFailureKind::IdentifierReported { identified, error } => {
                    panic!(
                        "invalid resume boundary response was accepted as identifier-reported for {}: {error:?}",
                        identified.identifier()
                    )
                }
            },
            ResumeProgress::Done(outcome) => {
                panic!("invalid resume boundary response unexpectedly completed: {outcome:?}")
            }
            ResumeProgress::Pending(pending) => match pending.as_ref() {
                PendingResume::Resume {
                    id,
                    wit_id,
                    wit_request,
                    resume,
                    poll_after_secs,
                    phase,
                } => panic!(
                    "invalid resume boundary response unexpectedly replaced the record for {id} ({wit_id:?}, {wit_request:?}, {resume:?}, {poll_after_secs}, {phase:?})"
                ),
                PendingResume::Resent(pending) => panic!(
                    "invalid resume boundary response unexpectedly resent create for {}",
                    pending.id
                ),
            },
        }
    }

    fn assert_issued_poll_repended_with_original_identifier(progress: CreateProgress) {
        let CreateProgress::Pending(pending) = progress else {
            panic!("issued poll must accept the replacement token")
        };
        assert_eq!(
            pending.pending.identifier().map(AppliedIdentifier::as_str),
            Some("issued-id")
        );
        assert_eq!(
            pending.pending.token().expose_for_provider_call(),
            "replacement-token"
        );
    }

    fn assert_issued_resume_repended_with_original_identifier(progress: ResumeProgress) {
        let ResumeProgress::Pending(pending) = progress else {
            panic!("issued resume must accept the replacement token")
        };
        let PendingResume::Resume { resume, .. } = *pending else {
            panic!("issued resume unexpectedly changed into a resent create")
        };
        assert_eq!(
            resume.identifier().map(AppliedIdentifier::as_str),
            Some("issued-id")
        );
        assert_eq!(
            resume.token().expose_for_provider_call(),
            "replacement-token"
        );
    }

    fn assert_fresh_create_in_flight(
        progress: CreateProgress,
        expected_token: &str,
        expected_identifier: Option<&str>,
        expected_sent: &Sent,
    ) -> String {
        let CreateProgress::Verdict(failure) = progress else {
            panic!("create in-flight verdict must preserve a continuation")
        };
        let CreateFailureKind::InFlight { pending, error } = failure.into_kind() else {
            panic!("create in-flight verdict changed failure classification")
        };
        let InFlightPersistence::Fresh(fresh) = pending.persistence() else {
            panic!("a boundary continuation must remain fresh until state persistence")
        };
        assert_eq!(fresh.token().expose_for_provider_call(), expected_token);
        assert_eq!(
            fresh.identifier().map(AppliedIdentifier::as_str),
            expected_identifier
        );
        assert_eq!(fresh.sent(), expected_sent);
        assert_eq!(
            fresh.phase(),
            if expected_identifier.is_some() {
                proto::PendingPhase::ReadBack
            } else {
                proto::PendingPhase::Operation
            }
        );
        error.into_host_for_test().to_string()
    }

    #[test]
    fn initial_create_read_back_in_flight_keeps_the_continuation() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let attempt = create_attempt_for_test(id.clone());
        let verdict: wasm_convert::DecodedCreateVerdict =
            proto::CreateVerdict::InFlight(proto::CreateInFlight {
                pending: proto::PendingCreate {
                    token: proto::PendingToken::new("initial-next-token"),
                    progress: proto::CreateProgress::ReadingBack {
                        identifier: AppliedIdentifier::new("initial-learned-id")
                            .expect("valid identifier"),
                    },
                    sent: sent_name_only(),
                    poll_after_secs: 3,
                },
                error: ProviderError::timeout("initial create interrupted"),
            });

        assert_fresh_create_in_flight(
            WasmProvider::initial_create_verdict(id, &wit_request, attempt, verdict),
            "initial-next-token",
            Some("initial-learned-id"),
            &sent_name_only(),
        );
    }

    #[test]
    fn poll_read_back_in_flight_keeps_an_identifier_learned_during_the_call() {
        let starting = pending_create_without_identifier();
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::InFlight(proto::CreateInFlight {
                pending: proto::PendingCreate {
                    token: proto::PendingToken::new("poll-next-token"),
                    progress: proto::CreateProgress::ReadingBack {
                        identifier: AppliedIdentifier::new("poll-learned-id")
                            .expect("valid identifier"),
                    },
                    sent: sent_name_only(),
                    poll_after_secs: 5,
                },
                error: ProviderError::timeout("poll interrupted"),
            });

        assert_fresh_create_in_flight(
            WasmProvider::polled_create_verdict(starting, verdict),
            "poll-next-token",
            Some("poll-learned-id"),
            &sent_name_only(),
        );
    }

    #[test]
    fn poll_in_flight_phase_regression_is_protocol_violation_and_preserves_continuation() {
        let (id, wit_id, wit_request) = boundary_resource();
        let starting = PendingCreate {
            pending: create_attempt_for_test(id.clone())
                .issued(
                    AppliedIdentifier::new("issued-id").unwrap(),
                    sent_name_only(),
                )
                .pending_read_back(proto::PendingToken::new("read-back-token"), 1),
            id,
            wit_id,
            wit_request,
        };
        let verdict = proto::PollVerdict::InFlight(proto::CreateInFlight {
            pending: proto::PendingCreate {
                token: proto::PendingToken::new("regressed-token"),
                progress: proto::CreateProgress::Operating { identifier: None },
                sent: sent_name_only(),
                poll_after_secs: 7,
            },
            error: ProviderError::timeout("poll interrupted"),
        });

        let CreateProgress::Verdict(failure) =
            WasmProvider::polled_create_verdict(starting, verdict)
        else {
            panic!("a phase regression must preserve a continuation")
        };
        let CreateFailureKind::InFlight { pending, error } = failure.into_kind() else {
            panic!("a phase regression must remain recoverable")
        };
        assert_eq!(
            pending.token().expose_for_provider_call(),
            "regressed-token"
        );
        assert_eq!(
            pending.identifier().map(AppliedIdentifier::as_str),
            Some("issued-id")
        );
        let InFlightPersistence::Fresh(fresh) = pending.persistence() else {
            panic!("the poll must retain its fresh send evidence")
        };
        let error = error.into_host_for_test();
        assert_eq!(fresh.sent(), &sent_name_only());
        assert_eq!(fresh.poll_after_secs(), 7);
        assert!(error.to_string().contains("polling protocol"), "{error}");
    }

    #[tokio::test]
    async fn resume_in_flight_phase_regression_is_protocol_violation_and_preserves_continuation() {
        for resent in [false, true] {
            for read_back_budget in [
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(30),
            ] {
                let (id, wit_id, wit_request) = boundary_resource();
                let resume = resume_witness(persisted_resume_without_identifier()).await;
                let progress = proto::CreateProgress::ReadingBack {
                    identifier: AppliedIdentifier::new("issued-id").unwrap(),
                };
                let token = proto::PendingToken::new("read-back-token");
                let step = if resent {
                    proto::ResumeStep::Resent(proto::CreateStep::Pending(proto::PendingCreate {
                        token,
                        progress,
                        sent: sent_name_only(),
                        poll_after_secs: 1,
                    }))
                } else {
                    proto::ResumeStep::Pending(proto::ResumePending {
                        token,
                        progress,
                        poll_after_secs: 1,
                    })
                };
                let initial =
                    WasmProvider::resumed_create_step(id, wit_id, wit_request, resume, step);
                let outcome = carina_provider_protocol::drive_pending_infallible(
                    std::time::Duration::from_secs(60),
                    read_back_budget,
                    std::future::ready(initial.into_step()),
                    |pending, finality| {
                        assert_eq!(pending.phase(), proto::PendingPhase::ReadBack);
                        assert_eq!(
                            finality,
                            if read_back_budget.is_zero() {
                                proto::PollFinality::FinalAttempt
                            } else {
                                proto::PollFinality::Continue
                            }
                        );
                        let token = proto::PendingToken::new("regressed-token");
                        let progress = proto::CreateProgress::Operating { identifier: None };
                        let error = ProviderError::timeout("resume interrupted");
                        let next = match *pending {
                            PendingResume::Resume {
                                id,
                                wit_request,
                                resume,
                                phase,
                                ..
                            } => WasmProvider::resumed_create_verdict(
                                id,
                                &wit_request,
                                resume,
                                phase,
                                proto::ResumeVerdict::InFlight(proto::ResumeInFlight {
                                    pending: proto::ResumePending {
                                        token,
                                        progress,
                                        poll_after_secs: 7,
                                    },
                                    error,
                                }),
                            ),
                            PendingResume::Resent(starting) => WasmProvider::polled_resent_verdict(
                                starting,
                                proto::PollVerdict::InFlight(proto::CreateInFlight {
                                    pending: proto::PendingCreate {
                                        token,
                                        progress,
                                        sent: sent_name_only(),
                                        poll_after_secs: 7,
                                    },
                                    error,
                                }),
                            ),
                        };
                        std::future::ready(next.into_step())
                    },
                    |_| std::future::ready(()),
                )
                .await;
                let carina_provider_protocol::DriveOutcome::Verdict(failure) = outcome else {
                    panic!("a phase regression must preserve its classified continuation")
                };
                let ResumeFailureKind::InFlight { pending, error } = failure.into_kind() else {
                    panic!("a phase regression must remain recoverable")
                };
                let error = error.into_host_for_test();
                assert_eq!(
                    pending.token().expose_for_provider_call(),
                    "regressed-token"
                );
                assert_eq!(
                    pending.identifier().map(AppliedIdentifier::as_str),
                    Some("issued-id")
                );
                assert!(error.to_string().contains("polling protocol"), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn resume_read_back_in_flight_keeps_an_identifier_learned_during_the_call() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let resume = resume_witness(persisted_resume_without_identifier()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::InFlight(proto::ResumeInFlight {
                pending: proto::ResumePending {
                    token: proto::PendingToken::new("resume-next-token"),
                    progress: proto::CreateProgress::ReadingBack {
                        identifier: AppliedIdentifier::new("resume-learned-id")
                            .expect("valid identifier"),
                    },
                    poll_after_secs: 7,
                },
                error: ProviderError::timeout("resume interrupted"),
            });

        let ResumeProgress::Verdict(failure) = WasmProvider::resumed_create_verdict(
            id,
            &wit_request,
            resume,
            proto::PendingPhase::Operation,
            verdict,
        ) else {
            panic!("resume in-flight verdict must preserve a continuation")
        };
        let ResumeFailureKind::InFlight { pending, error: _ } = failure.into_kind() else {
            panic!("resume in-flight verdict changed failure classification")
        };
        assert_eq!(
            pending.token().expose_for_provider_call(),
            "resume-next-token"
        );
        assert_eq!(
            pending.identifier().map(AppliedIdentifier::as_str),
            Some("resume-learned-id")
        );
    }

    #[tokio::test]
    async fn resent_create_read_back_in_flight_is_preserved() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let resume = resume_witness(persisted_resume_without_identifier()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::Resent(proto::CreateVerdict::InFlight(proto::CreateInFlight {
                pending: proto::PendingCreate {
                    token: proto::PendingToken::new("resent-next-token"),
                    progress: proto::CreateProgress::ReadingBack {
                        identifier: AppliedIdentifier::new("resent-learned-id")
                            .expect("valid identifier"),
                    },
                    sent: sent_name_only(),
                    poll_after_secs: 9,
                },
                error: ProviderError::timeout("resent create interrupted"),
            }));

        let ResumeProgress::Verdict(failure) = WasmProvider::resumed_create_verdict(
            id,
            &wit_request,
            resume,
            proto::PendingPhase::Operation,
            verdict,
        ) else {
            panic!("resent in-flight verdict must preserve a continuation")
        };
        let ResumeFailureKind::InFlight { pending, error: _ } = failure.into_kind() else {
            panic!("resent in-flight verdict changed failure classification")
        };
        assert_eq!(
            pending.token().expose_for_provider_call(),
            "resent-next-token"
        );
        let InFlightPersistence::Fresh(fresh) = pending.persistence() else {
            panic!("resent read-back must retain its fresh send evidence")
        };
        assert_eq!(fresh.phase(), proto::PendingPhase::ReadBack);
        assert_eq!(fresh.sent(), &sent_name_only());
        assert_eq!(
            pending.identifier().map(AppliedIdentifier::as_str),
            Some("resent-learned-id")
        );
    }

    #[tokio::test]
    async fn resent_poll_read_back_in_flight_keeps_the_new_token_and_learned_identifier() {
        let (id, wit_id, wit_request) = boundary_resource();
        let resume = resume_witness(persisted_resume_without_identifier()).await;
        let step: wasm_convert::DecodedResumeStep =
            proto::ResumeStep::Resent(proto::CreateStep::Pending(proto::PendingCreate {
                token: proto::PendingToken::new("resent-original-token"),
                progress: proto::CreateProgress::Operating { identifier: None },
                sent: sent_name_only(),
                poll_after_secs: 1,
            }));
        let ResumeProgress::Pending(pending) =
            WasmProvider::resumed_create_step(id, wit_id, wit_request, resume, step)
        else {
            panic!("resent pending step must retain a pollable continuation")
        };
        let PendingResume::Resent(starting) = *pending else {
            panic!("resent pending step changed into a resume continuation")
        };
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::InFlight(proto::CreateInFlight {
                pending: proto::PendingCreate {
                    token: proto::PendingToken::new("resent-poll-next-token"),
                    progress: proto::CreateProgress::ReadingBack {
                        identifier: AppliedIdentifier::new("resent-poll-learned-id")
                            .expect("valid identifier"),
                    },
                    sent: sent_name_only(),
                    poll_after_secs: 4,
                },
                error: ProviderError::timeout("resent poll interrupted"),
            });

        let ResumeProgress::Verdict(failure) =
            WasmProvider::polled_resent_verdict(starting, verdict)
        else {
            panic!("resent poll in-flight verdict must preserve a continuation")
        };
        let ResumeFailureKind::InFlight { pending, error: _ } = failure.into_kind() else {
            panic!("resent poll in-flight verdict changed failure classification")
        };
        assert_eq!(
            pending.token().expose_for_provider_call(),
            "resent-poll-next-token"
        );
        let InFlightPersistence::Fresh(fresh) = pending.persistence() else {
            panic!("resent read-back must retain its fresh send evidence")
        };
        assert_eq!(fresh.phase(), proto::PendingPhase::ReadBack);
        assert_eq!(fresh.sent(), &sent_name_only());
        assert_eq!(
            pending.identifier().map(AppliedIdentifier::as_str),
            Some("resent-poll-learned-id")
        );
    }

    #[test]
    fn in_flight_invalid_sent_attributes_follow_the_pending_boundary_error() {
        let invalid_sent = Sent::only(["not-in-request"]);
        let pending_payload = proto::PendingCreate {
            token: proto::PendingToken::new("invalid-sent-token"),
            progress: proto::CreateProgress::Operating { identifier: None },
            sent: invalid_sent,
            poll_after_secs: 2,
        };

        let (pending_id, pending_wit_id, pending_wit_request) = boundary_resource();
        let pending_error = assert_fresh_create_in_flight(
            WasmProvider::initial_create_step(
                pending_id.clone(),
                pending_wit_id,
                pending_wit_request,
                create_attempt_for_test(pending_id),
                proto::CreateStep::Pending(pending_payload.clone()),
            ),
            "invalid-sent-token",
            None,
            &Sent::Only(BTreeSet::new()),
        );

        let (verdict_id, _verdict_wit_id, verdict_wit_request) = boundary_resource();
        let in_flight_error = assert_fresh_create_in_flight(
            WasmProvider::initial_create_verdict(
                verdict_id.clone(),
                &verdict_wit_request,
                create_attempt_for_test(verdict_id),
                proto::CreateVerdict::InFlight(proto::CreateInFlight {
                    pending: pending_payload,
                    error: ProviderError::timeout("provider error must not hide boundary error"),
                }),
            ),
            "invalid-sent-token",
            None,
            &Sent::Only(BTreeSet::new()),
        );

        assert_eq!(in_flight_error, pending_error);
        assert!(
            in_flight_error.contains("not a subset of the request"),
            "{in_flight_error}"
        );
    }

    #[tokio::test]
    async fn issued_poll_boundary_never_drops_or_mutates_the_starting_record() {
        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::NotCreated(ProviderError::internal("not_created"));
        assert_original_polled_record(
            WasmProvider::polled_create_verdict(starting, verdict),
            &expected_id,
            &expected_started_at,
        );

        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let verdict: wasm_convert::DecodedPollVerdict =
            proto::PollVerdict::Unchanged(ProviderError::internal("unchanged"));
        assert_original_polled_record(
            WasmProvider::polled_create_verdict(starting, verdict),
            &expected_id,
            &expected_started_at,
        );

        let identifier_variants = [
            proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                progress: proto::CreateProgress::Operating { identifier: None },
                sent: sent_name_only(),
                poll_after_secs: 2,
            },
            proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                progress: proto::CreateProgress::Operating {
                    identifier: Some(
                        AppliedIdentifier::new("changed-id").expect("valid identifier"),
                    ),
                },
                sent: sent_name_only(),
                poll_after_secs: 2,
            },
        ];
        for next in identifier_variants {
            let starting = pending_create_with_issued_identifier().await;
            let step: wasm_convert::DecodedCreateStep = proto::CreateStep::Pending(next);
            assert_issued_poll_repended_with_original_identifier(WasmProvider::polled_create_step(
                starting, step,
            ));
        }

        let starting = pending_create_with_issued_identifier().await;
        let expected_id = starting.id.clone();
        let expected_started_at = starting.pending.started_at().clone();
        let changed_sent: wasm_convert::DecodedCreateStep =
            proto::CreateStep::Pending(proto::PendingCreate {
                token: proto::PendingToken::new("replacement-token"),
                progress: proto::CreateProgress::Operating {
                    identifier: Some(
                        AppliedIdentifier::new("issued-id").expect("valid identifier"),
                    ),
                },
                sent: Sent::AllRequestAttributes,
                poll_after_secs: 2,
            });
        assert_original_polled_record(
            WasmProvider::polled_create_step(starting, changed_sent),
            &expected_id,
            &expected_started_at,
        );
    }

    #[tokio::test]
    async fn poll_decode_failure_after_identifier_is_identifier_reported_not_no_resource() {
        let starting = pending_create_with_issued_identifier().await;
        let failure = decode_failed_create_step(&starting.id);
        assert_identifier_reported_after_decode_failure(
            WasmProvider::polled_step_identifier_reported_read_decode_failure(starting, failure),
        );

        let starting = pending_create_with_issued_identifier().await;
        let failure = decode_failed_identifier_reported_verdict(&starting.id);
        assert_identifier_reported_after_decode_failure(
            WasmProvider::polled_verdict_identifier_reported_read_decode_failure(starting, failure),
        );
    }

    #[tokio::test]
    async fn resume_decode_failure_after_identifier_is_identifier_reported_not_no_resource() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting).await;
        let step = wit_types::ResumeStep::Completed(wit_types::AppliedState {
            identifier: "decoded-created-id".to_string(),
            attributes: malformed_wit_attributes(),
        });
        let failure = match wasm_convert::wit_to_core_resume_step(step, &id)
            .expect("the valid identifier makes the attribute failure recoverable")
        {
            wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed attributes unexpectedly decoded")
            }
        };

        let progress = WasmProvider::resumed_identifier_reported_read_decode_failure(
            id,
            wit_request,
            resume,
            failure,
        );
        assert_identifier_reported_after_resume_decode_failure(progress);

        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting).await;
        let declared_error = ProviderError::internal("guest resume failure");
        let verdict = wit_types::ResumeVerdict::IdentifierReported(wit_types::ResumeIdentified {
            identifier: "decoded-created-id".to_string(),
            read: wit_types::Observation::Read(wit_types::ReadOutcome::Found(
                wit_types::AppliedState {
                    identifier: "decoded-created-id".to_string(),
                    attributes: malformed_wit_attributes(),
                },
            )),
            error: wasm_convert::wire_to_wit_provider_error(declared_error.to_wire()),
        });
        let failure = match wasm_convert::wit_to_core_resume_verdict(verdict, &id)
            .expect("the valid identifier makes the verdict observation failure recoverable")
        {
            wasm_convert::DecodedIssued::IdentifierReportedReadFailed(failure) => failure,
            wasm_convert::DecodedIssued::Decoded(_) => {
                panic!("malformed resume verdict observation unexpectedly decoded")
            }
        };
        let progress = WasmProvider::resumed_identifier_reported_read_decode_failure(
            id,
            wit_request,
            resume,
            failure,
        );
        assert_identifier_reported_after_resume_decode_failure(progress);
    }

    #[tokio::test]
    async fn issued_resume_boundary_never_drops_or_mutates_the_starting_record() {
        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting.clone()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::NotCreated(ProviderError::internal("not_created"));
        assert_original_resumed_record(
            WasmProvider::resumed_create_verdict(
                id,
                &wit_request,
                resume,
                proto::PendingPhase::Operation,
                verdict,
            ),
            &starting,
        );

        let (id, _wit_id, wit_request) = boundary_resource();
        let starting = persisted_resume_with_issued_identifier();
        let resume = resume_witness(starting.clone()).await;
        let verdict: wasm_convert::DecodedResumeVerdict =
            proto::ResumeVerdict::Unchanged(ProviderError::internal("unchanged"));
        assert_original_resumed_record(
            WasmProvider::resumed_create_verdict(
                id,
                &wit_request,
                resume,
                proto::PendingPhase::Operation,
                verdict,
            ),
            &starting,
        );

        let identifier_variants = [
            proto::ResumePending {
                token: proto::PendingToken::new("replacement-token"),
                progress: proto::CreateProgress::Operating { identifier: None },
                poll_after_secs: 2,
            },
            proto::ResumePending {
                token: proto::PendingToken::new("replacement-token"),
                progress: proto::CreateProgress::Operating {
                    identifier: Some(
                        AppliedIdentifier::new("changed-id").expect("valid identifier"),
                    ),
                },
                poll_after_secs: 2,
            },
        ];
        for next in identifier_variants {
            let (id, wit_id, wit_request) = boundary_resource();
            let resume = resume_witness(persisted_resume_with_issued_identifier()).await;
            let step: wasm_convert::DecodedResumeStep = proto::ResumeStep::Pending(next);
            assert_issued_resume_repended_with_original_identifier(
                WasmProvider::resumed_create_step(id, wit_id, wit_request, resume, step),
            );
        }
    }

    fn migrate_legacy_state_attribute(
        value: serde_json::Value,
        display_type: &str,
        identity: &str,
        key: &str,
    ) -> serde_json::Value {
        let mut attributes = serde_json::Map::new();
        attributes.insert(key.to_string(), value);
        let mut document = serde_json::json!({
            "version": 13,
            "resources": [{
                "provider": "",
                "resource_type": display_type,
                "identity": identity,
                "attributes": attributes,
                "deposed": [],
            }],
        });
        carina_core::resource::legacy_secret_migration::migrate_state_document_v13(&mut document)
            .expect("migrate legacy test state document");
        document["resources"][0]["attributes"]
            .as_object_mut()
            .and_then(|attributes| attributes.remove(key))
            .expect("migrated test document must retain its attribute")
    }

    #[test]
    fn malformed_config_completions_metadata_is_rejected() {
        let error = decode_config_completions_json("not json", "mock", "1.0.0")
            .expect_err("malformed config completions must fail provider loading");

        let rendered = error.to_string();
        assert!(
            rendered.contains("provider config completions"),
            "{rendered}"
        );
        assert!(rendered.contains("expected ident"), "{rendered}");
        assert!(rendered.contains("line 1 column 2"), "{rendered}");
        assert_eq!(rendered.matches("expected ident").count(), 1, "{rendered}");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn shared_trap_mapper_preserves_provider_operation_messages() {
        let trap = wasm_trap_provider_error_for_test(
            "read",
            wasmtime::Error::msg("low-level trap"),
            &crate::provider_text_fixtures::context(&[]),
        );
        assert_eq!(trap.detail().context(), "WASM guest trapped");
        assert_eq!(
            trap.to_string(),
            "WASM guest trapped: read call failed: low-level trap"
        );
        assert!(trap.detail().cause().is_some());
        assert!(std::error::Error::source(&trap).is_none());

        let timeout = wasm_trap_provider_error_for_test(
            "read",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
            &crate::provider_text_fixtures::context(&[]),
        );
        assert_eq!(timeout.detail().context(), "WASM plugin timed out");
        let rendered = timeout.to_string();
        assert!(rendered.contains("read exceeded the guest compute budget"));
        assert!(!rendered.to_lowercase().contains("credential"));

        let normalizer_timeout = wasm_trap_provider_error_for_test(
            "normalize_state",
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while executing at wasm backtrace:\n    0: test-frame"),
            &crate::provider_text_fixtures::context(&[]),
        );
        assert_eq!(
            normalizer_timeout.detail().context(),
            "WASM plugin timed out"
        );
        let rendered = normalizer_timeout.to_string();
        assert!(rendered.contains("normalize_state exceeded the guest compute budget"));
        assert!(!rendered.to_lowercase().contains("credential"));
    }

    #[test]
    fn shared_trap_mapper_preserves_typed_egress_denial_without_timeout_or_generic_wording() {
        let denial = EgressDenied::new(
            local_provider_label("mock_http"),
            Some(ProviderKind::Untrusted),
            "169.254.169.254".to_string(),
            DestinationDenied::MetadataWithheld(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
        );
        let expected = denial.to_string();
        let raw_call = EgressDeniedCall {
            operation: "create",
            denial,
        };
        {
            let call = &raw_call;
            assert_eq!(call.operation, "create");
            assert_eq!(call.denial().to_string(), expected);
            assert!(std::error::Error::source(call).is_none());
        }
        let denial = raw_call.denial;

        let mapped = wasm_trap_provider_error_for_test(
            "create",
            wasmtime::Error::new(denial),
            &crate::provider_text_fixtures::context(&[]),
        );

        assert_eq!(
            mapped.kind(),
            carina_core::provider::ProviderErrorKind::Internal
        );
        assert_eq!(
            mapped.detail().context(),
            "WASM guest call stopped by the host egress policy"
        );
        assert_eq!(
            mapped.to_string(),
            format!("WASM guest call stopped by the host egress policy: create call: {expected}")
        );
        let call = mapped
            .detail()
            .cause()
            .and_then(|cause| cause.downcast_ref::<EgressDeniedCall>())
            .expect("the ProviderError must retain the typed egress call");
        assert!(call.source().is_none());
    }

    #[test]
    fn timeout_classification_uses_the_typed_trap_not_message_text() {
        let nested = wasmtime::Error::msg("epoch deadline reached").context("outer guest trap");

        let error = wasm_trap_provider_error_for_test(
            "read",
            nested,
            &crate::provider_text_fixtures::context(&[]),
        );

        assert_eq!(error.variant_name(), "internal");
    }

    #[tokio::test]
    async fn configured_provider_name_must_match_guest_info() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let error = match WasmProviderFactory::new_uncached(
            &wasm_bytes,
            ProviderLoadIdentity::for_test("aws", ProviderTrustClass::Untrusted),
            &UnusedAwsCredentialProvisioner,
        )
        .await
        {
            Ok(_) => panic!("a guest-reported name must not override the configured name"),
            Err(error) => error,
        };
        match error {
            WasmProviderLoadError::ProviderNameMismatch {
                configured,
                reported,
            } => {
                assert_eq!(configured, "aws");
                assert_eq!(reported, "mock");
            }
            error => panic!("expected provider-name mismatch, got: {error:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn normalizer_guest_boundary_decode_error_is_structured_and_reentrant() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let factory = WasmProviderFactory::new_uncached(
            &wasm_bytes,
            test_load_identity("mock"),
            &UnusedAwsCredentialProvisioner,
        )
        .await
        .expect("mock WASM provider should load");
        let instance = factory
            .get_or_create_shared_instance(None, &empty_ready_config(&factory))
            .await
            .expect("mock WASM provider should initialize");
        let malformed = version_checked_bindings::WitDesiredResource {
            id: wit_types::ResourceId {
                provider: "mock".to_string(),
                resource_type: "test.resource".to_string(),
                identity: "malformed-boundary".to_string(),
            },
            attributes: vec![(
                "settings".to_string(),
                wit_types::DesiredValue::ListVal("not-json".to_string()),
            )],
        };
        let locked = instance
            .acquire("normalize_desired")
            .await
            .expect("fresh instance should be usable");
        let malformed = [malformed];
        let result = locked
            .call("normalize_desired", async |bindings, store| {
                bindings.call_normalize_desired(store, &malformed).await
            })
            .await
            .expect("the guest call should return its provider error");
        let error = result
            .map_err(wasm_convert::wit_to_core_provider_error)
            .expect_err("malformed guest input must return provider-error")
            .into_host_for_test();

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error
                .detail()
                .operation
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("normalize_desired"),
            "the guest boundary error must retain its export operation"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("normalize_desired"),
            "the guest boundary error message must name its export: {error}"
        );
        assert!(
            rendered.contains("WASM boundary decode error"),
            "the guest boundary error message must be preserved: {error}"
        );
        assert!(
            rendered.contains("invalid JSON"),
            "the guest decode detail must be preserved: {error}"
        );
        assert!(
            matches!(&*instance.slot.lock().await, FailFastSlot::Live(_)),
            "a returned boundary provider-error must not poison the instance"
        );

        let malformed_saved_attrs = [(
            "mock.test.resource.malformed-boundary".to_string(),
            vec![(
                "settings".to_string(),
                wit_types::PersistedValue::ListVal("not-json".to_string()),
            )],
        )];
        let locked = instance
            .acquire("hydrate_read_state")
            .await
            .expect("the decode error must leave the instance usable");
        let result = locked
            .call("hydrate_read_state", async |bindings, store| {
                bindings
                    .call_hydrate_read_state(store, &[], &malformed_saved_attrs)
                    .await
            })
            .await
            .expect("the guest call should return its provider error");
        let error = result
            .map_err(wasm_convert::wit_to_core_provider_error)
            .expect_err("malformed saved attributes must return provider-error")
            .into_host_for_test();

        assert_eq!(error.variant_name(), "internal");
        assert_eq!(
            error
                .detail()
                .operation
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("hydrate_read_state"),
            "the structured operation must be the bare export name"
        );
        assert!(
            error
                .to_string()
                .contains("hydrate_read_state saved attributes"),
            "the boundary error message must retain its failing sub-part: {error}"
        );
        assert!(
            matches!(&*instance.slot.lock().await, FailFastSlot::Live(_)),
            "a returned saved-attribute decode error must not poison the instance"
        );

        let normalizer = WasmProviderNormalizer { instance };
        let mut resources = vec![carina_core::resource::ProviderResource::from(
            Resource::with_provider("mock", "test.resource", "after-boundary-error", None),
        )];
        normalizer
            .normalize_desired(&mut resources)
            .await
            .expect("the same instance must accept a later normalizer call");
    }

    #[test]
    fn rendered_decode_provider_error_contains_serde_detail_once() {
        let decode_error =
            wasm_convert::wit_to_core_value(&wit_types::Value::ListVal("[".to_string()))
                .expect_err("malformed JSON must fail");
        let serde_detail = "EOF while parsing a list at line 1 column 1";
        let rendered = wasm_value_decode_provider_error("read", decode_error)
            .into_host_for_test()
            .to_string();

        assert!(rendered.contains("in operation read"), "{rendered}");
        assert_eq!(rendered.matches(&serde_detail).count(), 1, "{rendered}");
    }

    #[test]
    fn malformed_normalized_state_output_is_atomic() {
        use wit_types::PersistedValue as WitValue;

        let first_id =
            Resource::<ModuleSource>::with_provider("mock", "test.resource", "first", None).id;
        let second_id =
            Resource::<ModuleSource>::with_provider("mock", "test.resource", "second", None).id;
        let original_value = Value::Concrete(carina_core::resource::ConcreteValue::String(
            "original".into(),
        ));
        let mut states = HashMap::from([
            (
                first_id.clone(),
                SnapshotState::existing(
                    first_id.clone(),
                    HashMap::from([("value".to_string(), original_value.clone())]),
                ),
            ),
            (
                second_id.clone(),
                SnapshotState::existing(
                    second_id.clone(),
                    HashMap::from([("value".to_string(), original_value)]),
                ),
            ),
        ]);
        let before = states.clone();
        let updates = vec![
            (
                first_id,
                vec![("value".to_string(), WitValue::StrVal("changed".to_string()))],
            ),
            (
                second_id.clone(),
                vec![(
                    "settings".to_string(),
                    WitValue::MapVal("not json".to_string()),
                )],
            ),
        ];

        let error = apply_wit_state_attribute_updates("normalize_state", &mut states, updates)
            .expect_err("malformed guest output must fail normalization")
            .into_host_for_test();

        assert!(error.to_string().contains("map-val"));
        assert!(error.detail().cause().is_some());
        assert_eq!(
            error
                .detail()
                .resource_id
                .as_deref()
                .map(|id| id.as_resource_id()),
            Some(&second_id),
            "decode errors must identify the state whose attributes were malformed"
        );
        assert_eq!(
            states, before,
            "decode failure must not partially mutate state"
        );
    }

    #[test]
    fn normalized_state_roundtrip_preserves_self_contained_legacy_secret() {
        use argon2::Argon2;
        use wit_types::PersistedValue as WitValue;

        let id =
            Resource::<ModuleSource>::with_provider("mock", "test.resource", "legacy", None).id;
        let material = b"\"wire-legacy\"";
        let salt = b"carina:mock.test.resource:legacy:comment";
        let mut output = [0_u8; 32];
        Argon2::default()
            .hash_password_into(material, salt, &mut output)
            .expect("hash legacy state fixture");
        let encoded = migrate_legacy_state_attribute(
            serde_json::json!(format!("_secret:argon2:{}", hex::encode(output))),
            "mock.test.resource",
            "legacy",
            "comment",
        );
        let digest = encoded[carina_core::value::PERSISTED_SECRET_KEY]
            .as_str()
            .expect("migration produces a structural secret")
            .to_string();
        let hash = carina_core::resource::RedactedSecretHash::from_encoded_digest(&digest)
            .expect("migration produces a valid digest");
        let attributes = HashMap::from([(
            "comment".to_string(),
            carina_core::resource::SnapshotValue::redacted(hash),
        )]);
        let mut states =
            HashMap::from([(id.clone(), SnapshotState::existing(id.clone(), attributes))]);
        apply_wit_state_attribute_updates(
            "normalize_state",
            &mut states,
            vec![(
                id.clone(),
                vec![("comment".to_string(), WitValue::RedactedSecretVal(digest))],
            )],
        )
        .expect("apply normalized state");

        let Some(Value::Deferred(DeferredValue::Placeholder(
            carina_core::resource::SnapshotLeaf::RedactedSecret { hash },
        ))) = states[&id].attributes.get("comment")
        else {
            panic!("normalized legacy marker must remain a typed secret hash")
        };
        assert!(
            hash.verify(material),
            "the normalizer roundtrip must retain the marker's explicit salt"
        );
    }

    #[test]
    fn provider_text_verify_version_uses_raw_metadata_and_safe_error_labels() {
        let raw = format!("1.2.3+{}", "a".repeat(9000));
        let json =
            serde_json::json!({"name": "mock", "display_name": "Mock", "version": raw}).to_string();
        let (_, _, version) = wasm_convert::json_to_provider_info(&json).unwrap();
        assert!(verify_provider_version("mock", &version, "^1.2.0").is_ok());
        let mismatch = verify_provider_version("mock", &version, "^2").unwrap_err();
        assert!(mismatch.contains("truncated,"));
        assert!(mismatch.len() <= carina_core::provider_text::MAX_PROVIDER_TEXT_BYTES);
        let invalid = verify_provider_version("mock", &"\x1b[2J".into(), "*").unwrap_err();
        assert!(!invalid.contains('\x1b'));
        assert!(invalid.contains("\\u{1b}[2J"));
    }

    #[test]
    fn provider_text_component_import_matching_keeps_raw_names_and_versions() {
        use carina_core::provider_text::ProviderKey as ProviderString;
        let version = format!("0.2.6+{}", "a".repeat(9000));
        let name = format!("wasi:http/outgoing-handler@{version}");
        let imports = ComponentImports {
            names: vec![name.clone().into()],
        };
        assert_eq!(
            imports.iter().next().map(ProviderString::as_str),
            Some(name.as_str())
        );
        let http = WasiHttpImports::from_component_imports(&imports).unwrap();
        let WasiHttpImportVersion::Semver(actual) = &http.first.version else {
            panic!("valid long import version must remain parseable")
        };
        assert_eq!(actual, &semver::Version::parse(&version).unwrap());
        assert!(
            http.first.name.to_string().len()
                <= carina_core::provider_text::MAX_PROVIDER_TEXT_BYTES
        );
    }

    #[test]
    fn provider_text_inline_wasmtime_diagnostics_preserve_chain_format() {
        for (error, expected) in [
            (wasmtime::Error::msg("root"), "top: root"),
            (
                wasmtime::Error::msg("root\nsecond line")
                    .context("inner")
                    .context("outer"),
                "top: outer: inner: root\nsecond line",
            ),
            (
                wasmtime::Error::new(std::io::Error::other("root")).context("outer\x1b[2J"),
                "top: outer\\u{1b}[2J: root",
            ),
        ] {
            let error = error.context("top");
            assert_eq!(WasmDiagnostic::new(error, true).to_string(), expected);
        }
    }

    #[test]
    fn provider_text_poison_reason_sanitizes_rebuild_warning_at_construction() {
        let raw = format!("\x1b[2J\x07\u{9b}{}", "x".repeat(20 * 1024));
        let reason = PoisonReason::from_trap(
            "read",
            &wasmtime::Error::msg(raw),
            crate::provider_text_fixtures::context(&[])
                .session()
                .redaction_set(),
        );
        let reason = reason.fail_fast_error("read").into_host_for_test();
        let rendered = reason.to_string();
        assert!(!rendered.contains(['\x1b', '\x07', '\u{9b}']));
        assert!(rendered.contains("\\u{1b}[2J\\u{7}\\u{9b}"));
        assert!(rendered.contains("truncated,"));
    }

    #[test]
    fn provider_text_precompile_error_sanitizes_hostile_component_text() {
        let directory = tempfile::tempdir().unwrap();
        let component = format!(
            "(component (type $t (func)) (import \"bad\\1b[2J\\07{}\" (func (type $t))))",
            "x".repeat(20 * 1024)
        );
        let error = WasmProviderFactory::precompile(
            component.as_bytes(),
            &directory.path().join("test.cwasm"),
        )
        .unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains(['\x1b', '\x07']));
        assert!(rendered.len() <= carina_core::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert!(rendered.contains("\\u{1b}"));
    }

    #[test]
    fn provider_text_http_trace_sanitizes_guest_status_and_method() {
        let raw = format!("\x1b[2J\x07\u{9b}{}", "x".repeat(20 * 1024));
        let rendered = http_trace_line(&raw, TraceUri::new("https://example.com"), 0, 0, &raw);
        let mut captured = Vec::new();
        {
            let mut writer =
                carina_core::provider_text::RedactingLineWriter::for_metadata(&mut captured);
            writer.push_str(&rendered).unwrap();
            writer.finish().unwrap();
        }
        let rendered = String::from_utf8(captured)
            .unwrap()
            .trim_end_matches('\n')
            .to_owned();
        assert!(!rendered.contains(['\x1b', '\x07', '\u{9b}']));
        assert!(rendered.len() <= carina_core::provider_text::MAX_PROVIDER_TEXT_BYTES);
    }

    #[test]
    fn round5_guest_http_trace_redacts_secret_path() {
        const SECRET: &str = "trace-path-private-4305";
        const CHILD: &str = "CARINA_4305_HTTP_TRACE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let name = std::thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, "1")
                .env("CARINA_WASI_HTTP_TRACE", "1")
                .output()
                .unwrap();
            let text = String::from_utf8(output.stderr).unwrap();
            assert!(output.status.success(), "{text}");
            assert!(text.contains("carina-host-http-trace"), "{text}");
            assert!(!text.contains(SECRET), "guest HTTP path leaked: {text}");
            assert!(text.contains("/(secret)/public"), "{text}");
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use wasmtime_wasi::p2::Pollable;
            let session = crate::provider_text_fixtures::context(&[SECRET])
                .session()
                .guest_inputs();
            let stderr = guest_stderr::GuestStderr::new(std::io::stderr());
            let _call = stderr.begin_call(&session);
            let mut hooks = AllowListHttpHooks::with_resolver(
                local_provider_label("mock_http"),
                amazonaws_network(ProviderKind::Untrusted),
                Arc::new(StubResolver {
                    addresses: HashMap::from([("service.amazonaws.com".into(), Vec::new())]),
                }),
            );
            hooks.stderr = stderr;
            let request = hyper::Request::builder()
                .uri(format!("http://service.amazonaws.com/{SECRET}/public"))
                .body(empty_outgoing_body())
                .unwrap();
            let mut response = wasmtime_wasi_http::p2::WasiHttpHooks::send_request(
                &mut hooks,
                request,
                test_outgoing_request_config(false),
            )
            .unwrap();
            response.ready().await;
            assert!(response.unwrap_ready().unwrap().is_err());
        });
    }

    #[tokio::test]
    async fn round5_missing_configuration_returns_errors_without_panicking() {
        let factory = rebuildable_state_test_factory()
            .await
            .expect("mock WASM fixture is required");
        let session =
            carina_core::provider::ProviderPlan::state_only().session(factory.name(), None, None);
        assert!(session.configuration().is_none());
        let error = carina_core::provider::ProviderBoundary::initialize(&factory, &session)
            .await
            .err()
            .expect("configured factory requires configuration");
        assert!(error.to_string().contains("require checked configuration"));
        let error =
            carina_core::provider::ProviderBoundary::initialize_normalizer(&factory, &session)
                .await
                .err()
                .expect("configured normalizer requires configuration");
        assert!(error.to_string().contains("require checked configuration"));
    }

    #[test]
    fn round5_guest_stderr_http_trace_redacts_before_truncation() {
        const PREFIX: &str = "trace-secret-front-4305";
        let secret = format!("{PREFIX}{}", "s".repeat(1024));
        let session = crate::provider_text_fixtures::context(&[&secret])
            .session()
            .guest_inputs();
        for phases in [false, true] {
            let capture = tempfile::NamedTempFile::new().unwrap();
            let stderr = guest_stderr::GuestStderr::new(capture.reopen().unwrap());
            let _call = stderr.begin_call(&session);
            let uri = format!("https://example.com/{}/{secret}/public", "x".repeat(8000));
            let line = if phases {
                http_trace_phases_line("GET", TraceUri::new(&uri), [1, 2, 3, 4])
            } else {
                http_trace_line("GET", TraceUri::new(&uri), 1, 2, "200")
            };
            stderr.write_line(&line).unwrap();
            let text = std::fs::read_to_string(capture.path()).unwrap();
            assert!(
                !text.contains(PREFIX),
                "trace leaked secret prefix at truncation: {text}"
            );
            assert!(text.contains("(secret)"), "{text}");
        }
    }

    #[test]
    fn provider_text_egress_declaration_is_terminal_after_rendering_nested_sources() {
        #[derive(Debug)]
        struct Outer {
            source: std::io::Error,
        }
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("declaration outer")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }
        let error = WasmProviderLoadError::EgressDeclaration(MetadataDiagnostic::new(Outer {
            source: std::io::Error::other("nested guest \x1b]52;c;ZXZpbA==\x07\x1b[2J"),
        }));
        assert!(std::error::Error::source(&error).is_none());
        let rendered = carina_core::executor::render_error_chain(&error);
        assert_eq!(rendered.matches("declaration outer").count(), 1);
        assert_eq!(rendered.matches("nested guest").count(), 1);
        assert!(!rendered.contains(['\x1b', '\x07']));
        assert!(rendered.contains("\\u{1b}]52;c;ZXZpbA==\\u{7}\\u{1b}[2J"));
    }

    #[test]
    fn provider_text_plugin_load_and_trap_errors_are_sanitized() {
        let raw = format!("\x1b]52;c;ZXZpbA==\x07\x1b[2J{}", "x".repeat(20 * 1024));
        let errors = [
            WasmProviderLoadError::metadata(&raw, std::io::Error::other(raw.clone())),
            WasmProviderLoadError::Other(raw.clone().into()),
            WasmProviderLoadError::ProviderNameMismatch {
                configured: "test".into(),
                reported: raw.clone().into(),
            },
            WasmProviderLoadError::from(EgressSectionError::InvalidPattern {
                pattern: raw.clone(),
                source: carina_provider_protocol::HostPattern::parse(&raw).unwrap_err(),
            }),
            WasmProviderLoadError::metadata(
                "metadata call failed",
                WasmDiagnostic::new(wasmtime::Error::msg(raw.clone()), false),
            ),
        ];
        for error in errors {
            let metadata = matches!(error, WasmProviderLoadError::Metadata(..));
            assert_eq!(std::error::Error::source(&error).is_some(), metadata);
            let rendered = error.to_string();
            assert!(!rendered.contains(['\x1b', '\x07']));
            assert!(rendered.contains("\\u{1b}"));
            assert!(rendered.contains("truncated,"));
        }
        let trap = wasm_trap_provider_error_for_test(
            "read",
            wasmtime::Error::msg(raw),
            &crate::provider_text_fixtures::context(&[]),
        );
        assert!(!trap.to_string().contains(['\x1b', '\x07']));
        assert!(
            trap.detail()
                .cause()
                .unwrap()
                .downcast_ref::<WasmGuestCallError>()
                .is_some()
        );
    }

    #[test]
    fn provider_text_normalization_errors_preserve_sources_and_sanitize_guest_keys() {
        use carina_provider_protocol::normalization::{NormalizationError, NormalizationErrorKind};
        let raw = format!("\x1b[2J\x07{}", "x".repeat(20 * 1024));
        let id = Resource::<ModuleSource>::with_provider("mock", "test.resource", "only", None).id;
        let source = NormalizationError::new(
            carina_core::protocol_convert::resource_id_to_protocol(&id),
            &raw,
            NormalizationErrorKind::DuplicateKey { key: raw.clone() },
        )
        .with_source(std::io::Error::other(raw.clone()));
        {
            let source = &source;
            assert!(
                std::error::Error::source(source)
                    .unwrap()
                    .is::<std::io::Error>()
            );
        }
        let error = ProviderError::from(source).into_host_for_test();
        assert!(!error.to_string().contains(['\x1b', '\x07']));
        assert!(error.to_string().contains("truncated,"));
        assert!(
            error
                .detail()
                .cause()
                .unwrap()
                .source_is::<std::io::Error>()
        );

        let expected = HashMap::from([("expected".to_string(), id)]);
        let state = wit_types::PersistedState {
            identifier: None,
            attributes: Vec::new(),
            exists: true,
        };
        let error =
            validate_wit_state_result_keys("normalize_state", &expected, vec![(raw, state)])
                .unwrap_err()
                .into_host_for_test();
        assert!(!error.to_string().contains(['\x1b', '\x07']));
        assert!(error.to_string().contains("\\u{1b}[2J\\u{7}"));
    }

    #[test]
    fn normalizer_result_rejects_empty_unexpected_key() {
        let id = Resource::<ModuleSource>::with_provider("mock", "test.resource", "only", None).id;
        let expected = HashMap::from([("expected".to_string(), id)]);
        let state = wit_types::PersistedState {
            identifier: None,
            attributes: Vec::new(),
            exists: true,
        };
        let error = validate_wit_state_result_keys(
            "normalize_state",
            &expected,
            vec![("expected".into(), state.clone()), (String::new(), state)],
        )
        .expect_err("an unexpected empty key must not disappear during validation")
        .into_host_for_test();
        assert!(error.to_string().contains("unexpected: \"\""));
    }

    #[test]
    fn normalizer_result_rejects_duplicate_keys_including_empty() {
        for key in ["expected", ""] {
            let id =
                Resource::<ModuleSource>::with_provider("mock", "test.resource", "only", None).id;
            let expected = HashMap::from([(key.to_string(), id)]);
            let state = wit_types::PersistedState {
                identifier: None,
                attributes: Vec::new(),
                exists: true,
            };
            let error = validate_wit_state_result_keys(
                "normalize_state",
                &expected,
                vec![(key.into(), state.clone()), (key.into(), state)],
            )
            .expect_err("duplicate keys must be rejected before formatting")
            .into_host_for_test();
            assert!(error.to_string().contains("duplicate:"));
        }
    }

    #[test]
    fn invalid_state_key_error_names_the_operation_once() {
        let id = Resource::<ModuleSource>::with_provider("mock", "test.resource", "only", None).id;
        let expected = HashMap::from([("expected".to_string(), id)]);

        let error = validate_wit_state_result_keys("normalize_state", &expected, Vec::new())
            .expect_err("a missing state key must fail")
            .into_host_for_test();
        let rendered = error.to_string();

        assert_eq!(rendered.matches("normalize_state").count(), 1, "{rendered}");
    }

    #[test]
    fn single_state_result_rejects_more_than_one_entry() {
        let id = Resource::<ModuleSource>::with_provider("mock", "test.resource", "only", None).id;
        let state = wit_types::PersistedState {
            identifier: None,
            attributes: Vec::new(),
            exists: true,
        };

        let error = take_single_wit_state_result(
            "normalize_state",
            id,
            vec![
                ("first".to_string(), state.clone()),
                ("second".to_string(), state),
            ],
        )
        .expect_err("a single-resource call returning two states must fail")
        .into_host_for_test();

        assert!(error.to_string().contains("expected exactly one"));
    }

    #[test]
    fn aws_partition_contains_region_but_no_credential_inputs() {
        let capabilities = aws_capabilities();
        let keys = env_keys_for_capabilities(Some(&capabilities));
        assert!(keys.contains(&"AWS_REGION"));
        assert!(keys.contains(&"AWS_DEFAULT_REGION"));
        assert!(keys.contains(&"AWS_ENDPOINT_URL"));
        for credential_input in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_EC2_METADATA_DISABLED",
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        ] {
            assert!(
                !keys.contains(&credential_input),
                "{credential_input} leaked into the AWS guest"
            );
        }
    }

    #[test]
    fn aws_credentials_debug_redacts_secret_material() {
        let credentials = AwsCredentials::new(
            "AKIAEXAMPLE",
            "secret-debug-sentinel",
            Some("session-debug-sentinel".to_string()),
            None,
        );

        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("AKIAEXAMPLE"));
        assert!(!rendered.contains("secret-debug-sentinel"));
        assert!(!rendered.contains("session-debug-sentinel"));
    }

    #[tokio::test]
    async fn schema_only_credential_stub_returns_the_pinned_typed_error() {
        let config = wasmtime::Config::new();
        let engine = Engine::new(&config).expect("test engine should be valid");
        let mut store = GuestStore::new(
            &engine,
            local_provider_label("schema-only-aws"),
            GuestNetwork::SchemaOnly,
            None,
            build_store_limits(),
            GuestCallBudget::PRODUCTION,
        );

        let result = store
            .call(|store| {
                <HostState as version_checked_bindings::aws_credentials::Host>::current_credentials(
                    store.data_mut(),
                )
            })
            .await
            .expect("the schema-only credential import must fail promptly");

        assert!(matches!(
            result,
            Err(version_checked_bindings::aws_credentials::AwsCredentialError::ProviderFailed(
                ref message
            )) if message == "credentials are not available while loading schemas"
        ));
    }

    #[test]
    fn test_shared_vars_reach_every_kind() {
        // Provider-agnostic utility vars reach every guest, including
        // the kind-less (info/schemas) instance.
        for capabilities in [
            Some(aws_capabilities()),
            Some(RuntimeCapabilities::GitHub),
            Some(RuntimeCapabilities::Untrusted),
            None,
        ] {
            let keys = env_keys_for_capabilities(capabilities.as_ref());
            assert!(keys.contains(&"HOME"), "HOME missing for {capabilities:?}");
            assert!(
                keys.contains(&"RUST_LOG"),
                "RUST_LOG missing for {capabilities:?}"
            );
            assert!(
                keys.contains(&"CARINA_WASI_HTTP_TRACE"),
                "CARINA_WASI_HTTP_TRACE missing for {capabilities:?}"
            );
        }
    }

    #[test]
    fn test_github_token_is_partitioned_to_github_only() {
        // GITHUB_TOKEN reaches the github guest...
        assert!(
            env_keys_for_capabilities(Some(&RuntimeCapabilities::GitHub)).contains(&"GITHUB_TOKEN")
        );
        // ...and NEVER the aws / awscc guests (the partition is a real
        // isolation boundary, not a flat global list).
        assert!(!env_keys_for_capabilities(Some(&aws_capabilities())).contains(&"GITHUB_TOKEN"));
        // ...and not the kind-less info/schemas instance either.
        assert!(!env_keys_for_capabilities(None).contains(&"GITHUB_TOKEN"));
    }

    #[test]
    fn test_aws_creds_never_reach_github_guest() {
        // Symmetric isolation: AWS credentials must not leak into the
        // github guest.
        let github = env_keys_for_capabilities(Some(&RuntimeCapabilities::GitHub));
        assert!(!github.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(!github.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(!github.contains(&"AWS_SESSION_TOKEN"));
    }

    #[test]
    fn test_no_partition_leaks_sensitive_unrelated_vars() {
        // Common sensitive/unrelated variables are in NO partition.
        for capabilities in [
            Some(aws_capabilities()),
            Some(RuntimeCapabilities::GitHub),
            Some(RuntimeCapabilities::Untrusted),
            None,
        ] {
            let keys = env_keys_for_capabilities(capabilities.as_ref());
            for forbidden in ["PATH", "SHELL", "USER", "SSH_AUTH_SOCK", "DATABASE_URL"] {
                assert!(
                    !keys.contains(&forbidden),
                    "{forbidden} leaked into {capabilities:?} partition"
                );
            }
        }
    }

    #[test]
    fn untrusted_identity_gets_no_credential_or_metadata_partition() {
        // An untrusted source receives only the shared group and no metadata
        // endpoint access, even when its configured name is "aws".
        let configured = ConfiguredProvider::new(&ProviderLoadIdentity::for_test(
            "aws",
            ProviderTrustClass::Untrusted,
        ));
        assert_eq!(configured.kind, ProviderKind::Untrusted);
        let capabilities = RuntimeCapabilities::Untrusted;
        let keys = env_keys_for_capabilities(Some(&capabilities));
        assert!(keys.contains(&"HOME"));
        assert!(!keys.contains(&"AWS_ACCESS_KEY_ID"));
        assert!(!keys.contains(&"GITHUB_TOKEN"));
    }

    #[test]
    fn malformed_schema_json_is_reported_as_provider_load_error() {
        let detail = wasm_convert::json_to_schemas(
            r#"[{
                "resource_type":"s3.Bucket",
                "attributes":{},
                "unique_name_attribute":{"type":"attribute"}
            }]"#,
        )
        .unwrap_err();
        let error = provider_schema_decode_error("bad-provider", "9.9.9", detail);
        let err = error.to_string();

        assert!(err.contains("provider 'bad-provider' 9.9.9"), "{err}");
        assert!(err.contains("schema JSON parse error"), "{err}");
        assert!(err.contains("missing field `value`"), "{err}");
        assert!(
            err.contains("provider revision may predate this host"),
            "{err}"
        );
        assert!(!err.contains("carina init"), "{err}");
        assert!(!err.contains("bump the provider lock"), "{err}");
    }

    #[test]
    fn pattern_diagnostics_are_rendered_once_in_provider_load_error() {
        let json = r#"{
                "first":{"type":"String","pattern":{"regex":"(?=x)","mode":{"kind":"full"}}},
                "second":{"type":"String","pattern":{"regex":"[","mode":{"kind":"full"}}}
            }"#;
        let detail = wasm_convert::json_to_attribute_types(json).unwrap_err();
        let diagnostic = ProviderDiagnostic::new(detail)
            .with_secrets_for_test(&crate::provider_text_fixtures::context(&[]));
        let rendered = diagnostic.to_string();
        assert!(std::error::Error::source(&diagnostic).is_none());
        for pattern in ["(?=x)", "["] {
            let error = carina_core::schema::CompiledPattern::try_new(
                &carina_core::schema::PatternSpec::new(
                    pattern,
                    carina_core::schema::PatternMatch::Full,
                ),
            )
            .unwrap_err();
            assert_eq!(
                rendered.matches(&error.to_string()).count(),
                1,
                "{rendered}"
            );
            assert_eq!(
                rendered.matches(&error.reason.to_string()).count(),
                1,
                "{rendered}"
            );
        }
        let error = provider_schema_decode_error(
            "bad-provider",
            "9.9.9",
            wasm_convert::json_to_attribute_types(json).unwrap_err(),
        );
        assert_eq!(error.to_string().matches(&rendered).count(), 1);
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn provider_kind_comes_from_resolver_trust_identity() {
        let kind = |configured_name: &str, class| {
            ConfiguredProvider::new(&ProviderLoadIdentity::for_test(configured_name, class)).kind
        };

        assert_eq!(kind("renamed", ProviderTrustClass::Aws), ProviderKind::Aws);
        assert_eq!(
            kind("aws", ProviderTrustClass::GitHub),
            ProviderKind::GitHub
        );
        assert_eq!(
            kind("aws", ProviderTrustClass::Untrusted),
            ProviderKind::Untrusted
        );
    }

    #[test]
    fn initialized_instance_wasi_ctx_uses_runtime_capabilities() {
        use wasmtime_wasi::p2::bindings::cli::environment::Host as _;

        struct TestWasiState {
            ctx: WasiCtx,
            table: ResourceTable,
        }

        impl WasiView for TestWasiState {
            fn ctx(&mut self) -> WasiCtxView<'_> {
                WasiCtxView {
                    ctx: &mut self.ctx,
                    table: &mut self.table,
                }
            }
        }

        fn guest_env_keys(capabilities: &RuntimeCapabilities) -> HashSet<String> {
            let ctx = build_sandboxed_wasi_ctx_with(Some(capabilities), |_| {
                Some("test-value".to_string())
            });
            let mut state = TestWasiState {
                ctx,
                table: ResourceTable::new(),
            };
            state
                .cli()
                .get_environment()
                .unwrap()
                .into_iter()
                .map(|(key, _value)| key)
                .collect()
        }

        let aws = aws_capabilities();
        let untrusted = RuntimeCapabilities::Untrusted;

        assert!(guest_env_keys(&aws).contains("AWS_REGION"));
        assert!(!guest_env_keys(&aws).contains("AWS_ACCESS_KEY_ID"));
        assert!(!guest_env_keys(&untrusted).contains("AWS_ACCESS_KEY_ID"));
    }

    #[test]
    fn test_other_kind_has_empty_credential_partition() {
        // The fail-closed kind carries no credentials — only the shared
        // group reaches it (asserted via env_keys_for_capabilities above).
        assert!(
            RuntimeCapabilities::Untrusted
                .credential_partition()
                .is_empty()
        );
        assert_eq!(
            RuntimeCapabilities::Untrusted.private_address_policy(),
            PrivateAddressPolicy::Deny
        );
        assert_eq!(
            RuntimeCapabilities::GitHub.private_address_policy(),
            PrivateAddressPolicy::Deny
        );
        assert_eq!(
            aws_capabilities().private_address_policy(),
            PrivateAddressPolicy::Allow
        );
    }

    #[test]
    fn imds_disable_var_is_never_exposed_to_guests() {
        for capabilities in [
            Some(aws_capabilities()),
            Some(RuntimeCapabilities::GitHub),
            Some(RuntimeCapabilities::Untrusted),
            None,
        ] {
            assert!(
                !env_keys_for_capabilities(capabilities.as_ref())
                    .contains(&"AWS_EC2_METADATA_DISABLED")
            );
        }
    }

    #[test]
    fn test_build_sandboxed_wasi_ctx_does_not_panic() {
        // Verify that building the sandboxed context succeeds even when
        // allowlisted variables are not set in the environment.
        // This confirms the `if let Ok(val)` guard handles missing vars.
        let capabilities = aws_capabilities();
        let _ctx = build_sandboxed_wasi_ctx(Some(&capabilities));
        let _ctx_none = build_sandboxed_wasi_ctx(None);
    }

    #[test]
    fn provider_kind_does_not_grant_amazonaws_egress() {
        for kind in [
            ProviderKind::Aws,
            ProviderKind::GitHub,
            ProviderKind::Untrusted,
        ] {
            let network = test_network(kind, &[]);
            assert!(
                check_host(&network, "sts.amazonaws.com").is_err(),
                "{kind:?} reached an undeclared amazonaws.com host"
            );
        }
    }

    #[test]
    fn consented_egress_permits_amazonaws_com() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        assert!(check_host(&network, "s3.amazonaws.com").is_ok());
        assert!(check_host(&network, "ec2.us-east-1.amazonaws.com").is_ok());
        assert!(check_host(&network, "sts.amazonaws.com").is_ok());
        assert!(check_host(&network, "cloudformation.ap-northeast-1.amazonaws.com").is_ok());
    }

    #[test]
    fn consented_egress_permits_amazonaws_com_cn() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        assert!(check_host(&network, "s3.amazonaws.com.cn").is_ok());
        assert!(check_host(&network, "ec2.cn-north-1.amazonaws.com.cn").is_ok());
        assert!(check_host(&network, "sts.cn-northwest-1.amazonaws.com.cn").is_ok());
    }

    #[test]
    fn test_http_allowlist_permits_with_port() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        let standard: hyper::Uri = "https://s3.amazonaws.com:443/".parse().unwrap();
        let china: hyper::Uri = "https://ec2.cn-north-1.amazonaws.com.cn:443/"
            .parse()
            .unwrap();
        assert!(check_host(&network, standard.host().unwrap()).is_ok());
        assert!(check_host(&network, china.host().unwrap()).is_ok());
    }

    #[test]
    fn test_store_limits_are_configured() {
        // Verify that build_store_limits() returns sensible values by
        // exercising the ResourceLimiter trait methods on the result.
        use wasmtime::ResourceLimiter;

        let mut limits = build_store_limits();

        // memory_growing: requesting up to 256 MB should succeed
        assert!(limits.memory_growing(0, 256 * 1024 * 1024, None).unwrap());

        // memory_growing: requesting beyond 256 MB should be denied
        assert!(
            !limits
                .memory_growing(0, 256 * 1024 * 1024 + 1, None)
                .unwrap()
        );

        // table_growing: requesting up to 65_536 elements should succeed
        assert!(limits.table_growing(0, 65_536, None).unwrap());

        // table_growing: requesting beyond 65_536 should be denied
        assert!(!limits.table_growing(0, 65_537, None).unwrap());

        // instances: should be capped at 10
        assert_eq!(limits.instances(), 10);
    }

    #[test]
    fn test_http_allowlist_blocks_other_hosts() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        assert!(check_host(&network, "evil.example.com").is_err());
        assert!(check_host(&network, "attacker.io").is_err());
        assert!(check_host(&network, "localhost").is_err());
        assert!(check_host(&network, "").is_err());
        // Ensure partial matches don't pass
        assert!(check_host(&network, "not-amazonaws.com").is_err());
        assert!(check_host(&network, "amazonaws.com.evil.com").is_err());
        assert!(check_host(&network, "fakeamazonaws.com").is_err());
        // Bare domain without service prefix is not a valid AWS endpoint
        assert!(check_host(&network, "amazonaws.com").is_err());
    }

    #[test]
    fn request_policy_rejects_noncanonical_ip_literals() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        for host in ["127.1", "2130706433", "0x7f.1", "0177.0.0.1"] {
            assert!(check_host(&network, host).is_err(), "allowed {host}");
        }
    }

    #[test]
    fn request_policy_rejects_non_ldh_hostnames() {
        let unrelated = amazonaws_network(ProviderKind::Untrusted);
        for host in ["a_b.com", "x\"y.com", "host:443", "-a.com", "a-.com"] {
            assert!(check_host(&unrelated, host).is_err(), "allowed {host}");
        }

        let network = test_network(ProviderKind::Untrusted, &["**.example.com"]);
        for host in [
            "a_b.example.com",
            "x\"y.example.com",
            "host:443",
            "-a.example.com",
            "a-.example.com",
        ] {
            assert!(check_host(&network, host).is_err(), "allowed {host}");
        }
        assert!(check_host(&network, "a-b.example.com").is_ok());
        let exact = test_network(ProviderKind::Untrusted, &["a-b.com"]);
        assert!(check_host(&exact, "a-b.com").is_ok());
    }

    #[test]
    fn metadata_denials_have_a_distinct_variant_before_and_after_resolution() {
        let network = amazonaws_network(ProviderKind::Untrusted);
        let metadata = IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254));
        let literal_denial = match check_host(&network, "169.254.169.254") {
            Ok(_) => panic!("an untrusted provider must not reach an IP-literal metadata host"),
            Err(denial) => denial,
        };
        assert!(matches!(
            literal_denial,
            DestinationDenied::MetadataWithheld(address) if address == metadata
        ));

        let allowed = check_host(&network, "service.amazonaws.com")
            .expect("the AWS hostname should pass the pre-resolution allow-list");
        let resolved_denial =
            match vet_resolved_destination(allowed, SocketAddr::new(metadata, 443), &[]) {
                Ok(_destination) => {
                    panic!("an untrusted provider must not reach resolved metadata")
                }
                Err((denial, _denied_destination)) => denial,
            };
        assert!(matches!(
            resolved_denial,
            DestinationDenied::MetadataWithheld(address) if address == metadata
        ));

        let link_local = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
        let allowed = check_host(&network, "service.amazonaws.com")
            .expect("the AWS hostname should pass the pre-resolution allow-list");
        let link_local_denial =
            match vet_resolved_destination(allowed, SocketAddr::new(link_local, 443), &[]) {
                Ok(_destination) => panic!("a resolved link-local address must remain denied"),
                Err((denial, _denied_destination)) => denial,
            };
        assert!(matches!(
            link_local_denial,
            DestinationDenied::LinkLocalAddressDenied(address) if address == link_local
        ));
    }

    #[test]
    fn metadata_denial_never_offers_a_local_aws_grant() {
        let error = EgressDenied::new(
            Arc::new(ProviderLabel {
                name: "mock_http".to_string(),
                source: ProviderLoadSource::local_file(
                    &carina_core::paths::ResolvedPath::new("/opt/providers/mock_http.wasm")
                        .unwrap(),
                    carina_core::paths::DisplayPath::new("mock_http.wasm").unwrap(),
                ),
            }),
            Some(ProviderKind::Untrusted),
            "169.254.169.254".to_string(),
            DestinationDenied::MetadataWithheld(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
        );

        assert_eq!(
            error.to_string(),
            "provider 'mock_http' (file:///opt/providers/mock_http.wasm) request to \
             169.254.169.254 was denied: instance-metadata address 169.254.169.254 is withheld \
             from every provider"
        );
        assert!(!error.to_string().contains("--trust-local-provider"));
        assert!(!error.to_string().contains("CARINA_TRUST_LOCAL_PROVIDER"));
        assert!(!error.to_string().contains('\n'));
    }

    #[test]
    fn metadata_denial_for_remote_provider_has_no_trust_hint() {
        let error = EgressDenied::new(
            remote_provider_label("widget", "github.com/example/provider-widget"),
            Some(ProviderKind::Untrusted),
            "169.254.169.254".to_string(),
            DestinationDenied::MetadataWithheld(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
        );
        let rendered = error.to_string();

        assert_eq!(
            rendered,
            "provider 'widget' (github.com/example/provider-widget) request to 169.254.169.254 was \
             denied: instance-metadata address 169.254.169.254 is withheld from every provider"
        );
        assert!(!rendered.contains("--trust-local-provider"));
        assert!(!rendered.contains("CARINA_TRUST_LOCAL_PROVIDER"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn egress_denied_display_does_not_offer_aws_grant_for_other_cloud_metadata() {
        let address = IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200));
        let rendered = EgressDenied::new(
            local_provider_label("widget"),
            Some(ProviderKind::Untrusted),
            address.to_string(),
            DestinationDenied::MetadataWithheld(address),
        )
        .to_string();

        assert!(
            rendered.contains(
                "instance-metadata address 100.100.100.200 is withheld from every provider"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("--trust-local-provider"), "{rendered}");
    }

    #[test]
    fn egress_denied_display_names_every_generic_reason_and_validation_instance() {
        let label = remote_provider_label("aws", "carina-rs/aws");
        let cases = [
            (
                DestinationDenied::HostNotAllowed,
                "host is not in the provider's consented egress declaration",
            ),
            (
                DestinationDenied::LinkLocalAddressDenied(IpAddr::V4(Ipv4Addr::new(
                    169, 254, 1, 1,
                ))),
                "link-local resolved address 169.254.1.1 is denied",
            ),
            (
                DestinationDenied::LoopbackAddressDenied(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                "loopback resolved address 127.0.0.1 is denied",
            ),
            (
                DestinationDenied::UnspecifiedAddressDenied(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
                "unspecified resolved address :: is denied",
            ),
            (
                DestinationDenied::PrivateAddressDenied(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))),
                "private resolved address 10.0.0.5 is not available to this provider",
            ),
            (
                DestinationDenied::MetadataWithheld(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
                "instance-metadata address 169.254.169.254 is withheld from every provider",
            ),
        ];

        for (reason, expected_reason) in cases {
            let rendered = EgressDenied::new(
                Arc::clone(&label),
                Some(ProviderKind::Aws),
                "service.amazonaws.com".to_string(),
                reason,
            )
            .to_string();
            assert!(
                rendered.contains("provider 'aws' (carina-rs/aws)")
                    && rendered.contains("service.amazonaws.com")
                    && rendered.contains(expected_reason)
                    && rendered.contains("was denied:"),
                "unexpected generic denial: {rendered}"
            );
            assert!(!rendered.contains('\n'));
        }

        let validation_instance = EgressDenied::new(
            remote_provider_label("awscc", "carina-rs/awscc"),
            None,
            "example.invalid".to_string(),
            DestinationDenied::HostNotAllowed,
        )
        .to_string();
        assert_eq!(
            validation_instance,
            "provider 'awscc' (carina-rs/awscc) request to example.invalid was denied: host is not \
             in the provider's consented egress declaration"
        );
    }

    fn vet_address(
        kind: ProviderKind,
        address: IpAddr,
    ) -> Result<VettedDestination, DestinationDenied> {
        let capabilities = match kind {
            ProviderKind::Aws => aws_capabilities(),
            ProviderKind::GitHub => RuntimeCapabilities::GitHub,
            ProviderKind::Untrusted => RuntimeCapabilities::Untrusted,
        };
        vet_resolved_destination(
            AllowedHost {
                private_address_policy: Some(capabilities.private_address_policy()),
                host: "x.amazonaws.com".to_string(),
            },
            SocketAddr::new(address, 443),
            &[],
        )
        .map_err(|(reason, _host)| reason)
    }

    #[test]
    fn resolved_address_policy_matrix_is_exhaustive_per_kind() {
        let cases = [
            (
                "AWS metadata v4",
                "169.254.169.254".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "AWS metadata v6",
                "fd00:ec2::254".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "Alibaba metadata before CGNAT",
                "100.100.100.200".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "Azure metadata",
                "168.63.129.16".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "loopback v4",
                "127.0.0.1".parse().unwrap(),
                ResolvedAddressClass::Loopback,
                false,
                false,
            ),
            (
                "loopback v6",
                "::1".parse().unwrap(),
                ResolvedAddressClass::Loopback,
                false,
                false,
            ),
            (
                "unspecified v4",
                "0.0.0.0".parse().unwrap(),
                ResolvedAddressClass::Unspecified,
                false,
                false,
            ),
            (
                "unspecified v6",
                "::".parse().unwrap(),
                ResolvedAddressClass::Unspecified,
                false,
                false,
            ),
            (
                "link-local v4",
                "169.254.1.1".parse().unwrap(),
                ResolvedAddressClass::LinkLocal,
                false,
                false,
            ),
            (
                "link-local v6",
                "fe80::1".parse().unwrap(),
                ResolvedAddressClass::LinkLocal,
                false,
                false,
            ),
            (
                "private v4",
                "10.0.0.1".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
            (
                "private v6",
                "fd12::1".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
            (
                "public v4",
                "203.0.113.10".parse().unwrap(),
                ResolvedAddressClass::Public,
                true,
                true,
            ),
            (
                "public v6",
                "2001:db8::1".parse().unwrap(),
                ResolvedAddressClass::Public,
                true,
                true,
            ),
            (
                "IPv4-mapped private",
                "::ffff:10.0.0.1".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
            (
                "IPv4-mapped loopback",
                "::ffff:127.0.0.1".parse().unwrap(),
                ResolvedAddressClass::Loopback,
                false,
                false,
            ),
            (
                "IPv4-mapped AWS metadata",
                "::ffff:169.254.169.254".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "NAT64 private",
                "64:ff9b::a00:1".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
            (
                "NAT64 AWS metadata",
                "64:ff9b::a9fe:a9fe".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "local-use NAT64 AWS metadata",
                "64:ff9b:1::a9fe:a9fe".parse().unwrap(),
                ResolvedAddressClass::Metadata,
                false,
                false,
            ),
            (
                "RFC 1918 upper boundary",
                "172.31.255.255".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
            (
                "outside RFC 1918 boundary",
                "172.32.0.0".parse().unwrap(),
                ResolvedAddressClass::Public,
                true,
                true,
            ),
            (
                "outside CGNAT boundary",
                "100.63.255.255".parse().unwrap(),
                ResolvedAddressClass::Public,
                true,
                true,
            ),
            (
                "CGNAT lower boundary",
                "100.64.0.0".parse().unwrap(),
                ResolvedAddressClass::Private,
                true,
                false,
            ),
        ];

        for kind in [
            ProviderKind::Aws,
            ProviderKind::GitHub,
            ProviderKind::Untrusted,
        ] {
            for (name, address, expected_class, aws_allowed, other_allowed) in cases {
                let canonical = canonical_policy_address(address);
                assert_eq!(
                    ResolvedAddressClass::classify(canonical),
                    expected_class,
                    "wrong class for {name} ({address})"
                );
                let expected = if kind == ProviderKind::Aws {
                    aws_allowed
                } else {
                    other_allowed
                };
                assert_eq!(
                    vet_address(kind, address).is_ok(),
                    expected,
                    "wrong policy for {name} ({address}) and {kind:?}"
                );
            }
        }
    }

    #[test]
    fn ip_literal_policy_classifies_metadata_and_generically_denies_other_literals() {
        for kind in [
            ProviderKind::Aws,
            ProviderKind::GitHub,
            ProviderKind::Untrusted,
        ] {
            let network = amazonaws_network(kind);
            for address in ["100.100.100.200", "168.63.129.16"] {
                assert!(matches!(
                    check_host(&network, address),
                    Err(DestinationDenied::MetadataWithheld(_))
                ));
            }
            for address in [
                "127.0.0.1",
                "0.0.0.0",
                "169.254.1.1",
                "10.0.0.1",
                "203.0.113.10",
                "[::1]",
                "[::]",
                "[fe80::1]",
                "[fd12::1]",
                "[2001:db8::1]",
            ] {
                assert!(matches!(
                    check_host(&network, address),
                    Err(DestinationDenied::HostNotAllowed)
                ));
            }
        }
    }

    #[test]
    fn mixed_public_and_private_resolution_is_denied_except_for_aws() {
        let public = SocketAddr::new("203.0.113.10".parse().unwrap(), 443);
        let private = SocketAddr::new("10.0.0.5".parse().unwrap(), 443);

        assert!(
            check_destination(
                &amazonaws_network(ProviderKind::Aws),
                "x.amazonaws.com",
                &[public, private],
            )
            .is_ok()
        );
        for kind in [ProviderKind::GitHub, ProviderKind::Untrusted] {
            assert!(matches!(
                check_destination(
                    &amazonaws_network(kind),
                    "x.amazonaws.com",
                    &[public, private],
                ),
                Err(DestinationDenied::PrivateAddressDenied(address))
                    if address == private.ip()
            ));
        }
    }

    #[test]
    fn schema_only_network_denies_every_resolved_address_class() {
        for address in [
            "169.254.169.254",
            "127.0.0.1",
            "0.0.0.0",
            "169.254.1.1",
            "10.0.0.1",
            "203.0.113.10",
            "fd00:ec2::254",
            "::1",
            "::",
            "fe80::1",
            "fd12::1",
            "2001:db8::1",
        ] {
            assert!(matches!(
                check_destination(
                    &GuestNetwork::SchemaOnly,
                    "x.amazonaws.com",
                    &[SocketAddr::new(address.parse().unwrap(), 443)],
                ),
                Err(DestinationDenied::HostNotAllowed)
            ));
        }
    }

    #[test]
    fn normal_amazonaws_destination_is_allowed_for_every_consented_runtime_kind() {
        let public_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 443);
        for kind in [
            ProviderKind::Aws,
            ProviderKind::GitHub,
            ProviderKind::Untrusted,
        ] {
            let network = amazonaws_network(kind);
            assert!(
                check_destination(&network, "x.amazonaws.com", &[public_address]).is_ok(),
                "public AWS endpoint denied for {kind:?}"
            );
        }
        assert!(
            check_destination(
                &GuestNetwork::SchemaOnly,
                "x.amazonaws.com",
                &[public_address]
            )
            .is_err()
        );

        let denied_link_local = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)), 443);
        let network = amazonaws_network(ProviderKind::Aws);
        assert!(
            check_destination(
                &network,
                "x.amazonaws.com",
                &[public_address, denied_link_local],
            )
            .is_err(),
            "one denied address must reject the entire resolution set"
        );
    }

    #[test]
    fn bracketed_ipv6_authority_is_parsed_without_splitting_on_colons() {
        let uri: hyper::Uri = "http://[fd00:ec2::254]:80/latest/meta-data/"
            .parse()
            .unwrap();
        assert_eq!(uri.host(), Some("[fd00:ec2::254]"));
        for kind in [
            ProviderKind::Aws,
            ProviderKind::GitHub,
            ProviderKind::Untrusted,
        ] {
            assert!(matches!(
                check_host(&amazonaws_network(kind), uri.host().unwrap()),
                Err(DestinationDenied::MetadataWithheld(_))
            ));
        }
    }

    #[test]
    fn metadata_ip_literals_are_denied_without_dns() {
        let calls = Arc::new(AtomicUsize::new(0));
        for uri in [
            "http://169.254.169.254:0/latest/meta-data/",
            "http://[fd00:ec2::254]:0/latest/meta-data/",
        ] {
            let mut hooks = AllowListHttpHooks::with_resolver(
                local_provider_label("aws"),
                amazonaws_network(ProviderKind::Aws),
                Arc::new(CountingResolver {
                    calls: Arc::clone(&calls),
                }),
            );
            let request = hyper::Request::builder()
                .uri(uri)
                .body(empty_outgoing_body())
                .unwrap();
            let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
                use_tls: false,
                connect_timeout: std::time::Duration::ZERO,
                first_byte_timeout: std::time::Duration::ZERO,
                between_bytes_timeout: std::time::Duration::ZERO,
            };

            let error =
                wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                    .expect_err("an AWS metadata literal must be withheld");
            let trap = error
                .downcast()
                .expect_err("metadata withholding must trap before a request task starts");
            assert!(
                trap.downcast_ref::<EgressDenied>()
                    .is_some_and(|denial| matches!(
                        denial.reason,
                        DestinationDenied::MetadataWithheld(_)
                    ))
            );
        }

        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the hook must not send IP literals through the DNS resolver"
        );
    }

    #[test]
    fn metadata_classification_precedes_address_ranges() {
        for endpoint in METADATA_ENDPOINTS {
            assert_eq!(
                ResolvedAddressClass::classify(endpoint),
                ResolvedAddressClass::Metadata
            );
        }
        assert_eq!(
            ResolvedAddressClass::classify(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 1))),
            ResolvedAddressClass::LinkLocal
        );
    }

    struct StubResolver {
        addresses: HashMap<String, Vec<SocketAddr>>,
    }

    impl DestinationResolver for StubResolver {
        fn resolve(&self, host: String, _port: u16) -> ResolveFuture {
            let result = self.addresses.get(&host).cloned().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no stub resolution for {host}"),
                )
            });
            Box::pin(async move { result })
        }
    }

    struct CountingResolver {
        calls: Arc<AtomicUsize>,
    }

    impl DestinationResolver for CountingResolver {
        fn resolve(&self, _host: String, _port: u16) -> ResolveFuture {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Err(std::io::Error::other(
                    "IP literals must not reach the DNS resolver",
                ))
            })
        }
    }

    struct NeverResolver;

    impl DestinationResolver for NeverResolver {
        fn resolve(&self, _host: String, _port: u16) -> ResolveFuture {
            Box::pin(std::future::pending())
        }
    }

    fn empty_outgoing_body() -> wasmtime_wasi_http::p2::body::HyperOutgoingBody {
        use http_body_util::{BodyExt, Empty};

        Empty::<hyper::body::Bytes>::new()
            .map_err(|never| match never {})
            .boxed_unsync()
    }

    fn test_outgoing_request_config(
        use_tls: bool,
    ) -> wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
        wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
            use_tls,
            connect_timeout: std::time::Duration::from_secs(1),
            first_byte_timeout: std::time::Duration::from_secs(1),
            between_bytes_timeout: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn pre_spawn_policy_denial_traps_but_invalid_uri_remains_an_error_code() {
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let mut hooks = AllowListHttpHooks::with_resolver(
            local_provider_label("mock_http"),
            test_network(ProviderKind::Untrusted, &[]),
            Arc::new(StubResolver {
                addresses: HashMap::new(),
            }),
        );

        let denied = hyper::Request::builder()
            .uri("http://example.invalid/")
            .body(empty_outgoing_body())
            .unwrap();
        let denied = wasmtime_wasi_http::p2::WasiHttpHooks::send_request(
            &mut hooks,
            denied,
            test_outgoing_request_config(false),
        )
        .expect_err("a denied host should fail before spawning a request");
        let trap = denied
            .downcast()
            .expect_err("a host-policy denial must trap instead of returning an ErrorCode");
        let denial = trap
            .downcast_ref::<EgressDenied>()
            .expect("the trap must retain the typed egress denial");
        assert!(matches!(denial.reason, DestinationDenied::HostNotAllowed));

        let invalid = hyper::Request::builder()
            .uri("/relative")
            .body(empty_outgoing_body())
            .unwrap();
        let invalid = wasmtime_wasi_http::p2::WasiHttpHooks::send_request(
            &mut hooks,
            invalid,
            test_outgoing_request_config(false),
        )
        .expect_err("a URI without a host should fail before spawning a request");
        assert!(matches!(
            invalid.downcast_ref(),
            Some(ErrorCode::HttpRequestUriInvalid)
        ));
    }

    #[tokio::test]
    async fn post_resolution_policy_denial_is_an_outer_trap() {
        use wasmtime_wasi::p2::Pollable;

        let metadata = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), 443);
        let resolver = StubResolver {
            addresses: HashMap::from([("service.amazonaws.com".to_string(), vec![metadata])]),
        };
        let mut hooks = AllowListHttpHooks::with_resolver(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Arc::new(resolver),
        );
        let request = hyper::Request::builder()
            .uri("https://service.amazonaws.com/")
            .body(empty_outgoing_body())
            .unwrap();

        let mut response = wasmtime_wasi_http::p2::WasiHttpHooks::send_request(
            &mut hooks,
            request,
            test_outgoing_request_config(true),
        )
        .expect("the allow-listed hostname should reach the spawned resolver task");
        response.ready().await;
        let trap = response
            .unwrap_ready()
            .expect_err("post-resolution policy denial must be an outer trap");
        let denial = trap
            .downcast_ref::<EgressDenied>()
            .expect("the task trap must retain the typed egress denial");
        assert!(matches!(
            denial.reason,
            DestinationDenied::MetadataWithheld(address)
                if address == IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))
        ));
    }

    #[test]
    fn http_test_route_does_not_override_https_policy() {
        let destination = SocketAddr::from((Ipv4Addr::LOCALHOST, 4443));
        let mut hooks = AllowListHttpHooks::with_test_route(
            local_provider_label("mock_http"),
            test_network(ProviderKind::Untrusted, &[]),
            Some(HttpTestRoute::new(
                "phase-test.invalid".to_string(),
                destination,
            )),
        );
        let request = hyper::Request::builder()
            .uri("https://phase-test.invalid:4443/")
            .body(empty_outgoing_body())
            .unwrap();
        let config = test_outgoing_request_config(true);

        let error =
            wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                .expect_err("an HTTP-only test route must not bypass HTTPS host policy");
        let trap = error
            .downcast()
            .expect_err("the HTTPS policy denial must trap instead of returning an ErrorCode");
        assert!(trap.downcast_ref::<EgressDenied>().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn dns_resolution_respects_connect_timeout() {
        use wasmtime_wasi::p2::Pollable;
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let mut hooks = AllowListHttpHooks::with_resolver(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Arc::new(NeverResolver),
        );
        let request = hyper::Request::builder()
            .uri("http://service.amazonaws.com/")
            .body(empty_outgoing_body())
            .unwrap();
        let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
            connect_timeout: std::time::Duration::from_millis(10),
            ..test_outgoing_request_config(false)
        };

        let mut response =
            wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                .expect("an allow-listed hostname should pass the host policy");
        tokio::time::timeout(std::time::Duration::from_secs(1), response.ready())
            .await
            .expect("DNS resolution must be bounded by the connect timeout");
        let result = response
            .unwrap_ready()
            .expect("the request task should not trap");

        assert!(
            matches!(result, Err(ErrorCode::ConnectionTimeout)),
            "a DNS timeout returned the wrong result: {result:?}"
        );
    }

    #[tokio::test]
    async fn dns_failure_is_reported_as_pre_send() {
        use wasmtime_wasi::p2::Pollable;
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let mut hooks = AllowListHttpHooks::with_resolver(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Arc::new(StubResolver {
                addresses: HashMap::new(),
            }),
        );
        let request = hyper::Request::builder()
            .uri("http://service.amazonaws.com/")
            .body(empty_outgoing_body())
            .unwrap();
        let config = test_outgoing_request_config(false);

        let mut response =
            wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                .expect("an allow-listed hostname should reach resolution");
        response.ready().await;
        let result = response
            .unwrap_ready()
            .expect("a DNS failure should not trap the request task");

        assert!(matches!(result, Err(ErrorCode::DnsError(..))));
    }

    #[tokio::test]
    async fn empty_dns_resolution_is_reported_as_the_guest_dns_error() {
        use wasmtime_wasi::p2::Pollable;
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let mut hooks = AllowListHttpHooks::with_resolver(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Arc::new(StubResolver {
                addresses: HashMap::from([("service.amazonaws.com".to_string(), Vec::new())]),
            }),
        );
        let request = hyper::Request::builder()
            .uri("http://service.amazonaws.com/")
            .body(empty_outgoing_body())
            .unwrap();

        let mut response = wasmtime_wasi_http::p2::WasiHttpHooks::send_request(
            &mut hooks,
            request,
            test_outgoing_request_config(false),
        )
        .expect("an allow-listed hostname should reach resolution");
        response.ready().await;
        let result = response
            .unwrap_ready()
            .expect("an empty DNS answer must not trap the request task");

        assert!(matches!(result, Err(ErrorCode::DnsError(..))));
    }

    #[tokio::test]
    async fn closed_tcp_port_is_reported_as_pre_send() {
        use wasmtime_wasi::p2::Pollable;
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        // Keep the ephemeral port reserved by a TCP socket that never starts
        // listening. This removes the bind/drop/rebind race from the usual
        // "closed port" test. Kernels may either refuse the connection
        // immediately or leave it pending until the connect timeout expires.
        let socket = match tokio::net::TcpSocket::new_v4().and_then(|socket| {
            socket.bind((Ipv4Addr::LOCALHOST, 0).into())?;
            Ok(socket)
        }) {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot reserve a closed loopback port: {error}");
                return;
            }
        };
        let closed_address = socket.local_addr().unwrap();

        let mut hooks = AllowListHttpHooks::with_test_route(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Some(HttpTestRoute::new(
                "closed.amazonaws.com".to_string(),
                closed_address,
            )),
        );
        let request = hyper::Request::builder()
            .uri(format!(
                "http://closed.amazonaws.com:{}/",
                closed_address.port()
            ))
            .body(empty_outgoing_body())
            .unwrap();
        let config = test_outgoing_request_config(false);

        let mut response =
            wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                .expect("an allow-listed hostname should start a request task");
        response.ready().await;
        let result = response
            .unwrap_ready()
            .expect("a TCP-connect failure should not trap the request task");
        let code = match result {
            Err(code) => code,
            Ok(_) => panic!("a bound-but-not-listening socket unexpectedly accepted a request"),
        };

        assert!(
            matches!(
                &code,
                ErrorCode::ConnectionRefused | ErrorCode::ConnectionTimeout
            ),
            "a TCP-connect failure returned an unexpected code: {code:?}"
        );
    }

    #[tokio::test]
    async fn close_after_receiving_request_is_reported_as_maybe_sent() {
        use wasmtime_wasi_http::p2::bindings::http::types::ErrorCode;

        let listener = match LoopbackListener::bind() {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
                return;
            }
        };
        let server_address = listener.local_addr().unwrap();
        let request = hyper::Request::builder()
            .uri(format!(
                "http://phase-test.invalid:{}/",
                server_address.port()
            ))
            .body(empty_outgoing_body())
            .unwrap();
        let config = test_outgoing_request_config(false);
        let test_route = HttpTestRoute::new("phase-test.invalid".to_string(), server_address);
        let test_destination =
            match test_route.resolution_for("phase-test.invalid", server_address.port(), false) {
                Some(ResolvedAddresses::HttpTestRoute(destination)) => destination,
                Some(ResolvedAddresses::Standard(_)) => {
                    panic!("the loopback test route must carry the exemption witness")
                }
                None => panic!("the exact HTTP test route must select its destination"),
            };
        let destination = VettedDestination::from_http_test_route(
            AllowedHost {
                private_address_policy: Some(PrivateAddressPolicy::Deny),
                host: "phase-test.invalid".to_string(),
            },
            test_destination,
        );
        let server = std::thread::spawn(move || {
            let mut stream = listener
                .accept_within(std::time::Duration::from_secs(5))
                .expect("accept loopback client");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 4096];
            stream
                .read(&mut request)
                .expect("read request before close")
        });

        let error = send_request_handler(request, config, destination, None)
            .await
            .expect_err("closing before a response should fail the request");
        let bytes_read = server.join().expect("loopback server should finish");

        assert!(bytes_read > 0, "the server must receive request bytes");
        assert!(matches!(error, ErrorCode::HttpProtocolError));
    }

    #[tokio::test]
    async fn redirect_response_is_surfaced_and_follow_up_is_rechecked() {
        use wasmtime_wasi::p2::Pollable;

        let listener = match LoopbackListener::bind() {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
                return;
            }
        };
        let server_address = listener.local_addr().unwrap();

        let mut hooks = AllowListHttpHooks::with_test_route(
            local_provider_label("mock_http"),
            amazonaws_network(ProviderKind::Untrusted),
            Some(HttpTestRoute::new(
                "redirector.amazonaws.com".to_string(),
                server_address,
            )),
        );
        let request = hyper::Request::builder()
            .uri(format!(
                "http://redirector.amazonaws.com:{}/start",
                server_address.port()
            ))
            .body(empty_outgoing_body())
            .unwrap();
        let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: std::time::Duration::from_secs(2),
            first_byte_timeout: std::time::Duration::from_secs(2),
            between_bytes_timeout: std::time::Duration::from_secs(2),
        };

        let server = std::thread::spawn(move || {
            let mut stream = listener
                .accept_within(std::time::Duration::from_secs(5))
                .expect("accept loopback client");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });

        let mut response =
            wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                .expect("redirector host should pass the hook");
        response.ready().await;
        let response = response
            .unwrap_ready()
            .expect("request task should not trap")
            .expect("redirector request should succeed");
        assert_eq!(response.resp.status(), hyper::StatusCode::FOUND);
        let location = response
            .resp
            .headers()
            .get(hyper::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, "http://169.254.169.254/latest/meta-data/");
        server.join().unwrap();

        let follow_up: hyper::Uri = location.parse().unwrap();
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        for kind in [
            Some(ProviderKind::Aws),
            Some(ProviderKind::GitHub),
            Some(ProviderKind::Untrusted),
            None,
        ] {
            let network = match kind {
                Some(kind) => test_network(kind, &[]),
                None => GuestNetwork::SchemaOnly,
            };
            let mut hooks = AllowListHttpHooks::with_resolver(
                local_provider_label("redirect-test"),
                network,
                Arc::new(CountingResolver {
                    calls: Arc::clone(&resolver_calls),
                }),
            );
            let request = hyper::Request::builder()
                .uri(follow_up.clone())
                .body(empty_outgoing_body())
                .unwrap();
            let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
                use_tls: false,
                connect_timeout: std::time::Duration::from_secs(2),
                first_byte_timeout: std::time::Duration::from_secs(2),
                between_bytes_timeout: std::time::Duration::from_secs(2),
            };

            let error =
                wasmtime_wasi_http::p2::WasiHttpHooks::send_request(&mut hooks, request, config)
                    .expect_err("the follow-up metadata request must trap before resolution");
            let trap = error
                .downcast()
                .expect_err("the follow-up metadata denial must not reach the guest");
            let denial = trap
                .downcast_ref::<EgressDenied>()
                .expect("the trap must retain the typed metadata denial");
            assert!(
                matches!(denial.reason, DestinationDenied::MetadataWithheld(_)),
                "follow-up metadata request returned the wrong denial for {kind:?}: {denial}"
            );
        }
        assert_eq!(resolver_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_engine_config_enables_epoch_interruption() {
        // Verify that build_engine_config() produces an Engine that supports
        // epoch deadlines. If epoch_interruption were not enabled, setting a
        // deadline on the Store would panic.
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let mut store = GuestStore::new(
            &engine,
            local_provider_label("epoch-test"),
            GuestNetwork::SchemaOnly,
            None,
            build_store_limits(),
            GuestCallBudget::PRODUCTION,
        );
        // This will panic if epoch_interruption is not enabled on the engine.
        store.set_epoch_deadline_for_test(1);
    }

    #[test]
    fn test_epoch_ticker_increments_epoch() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let (ticked, ticks) = std::sync::mpsc::channel();
        let ticker = EpochTicker::start_with_tick_observer(engine, move || {
            let _ = ticked.send(());
        });
        for _ in 0..2 {
            // wall-clock-ok: hang guard for an actual increment from the real epoch ticker thread; no elapsed-speed assertion.
            ticks
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("epoch ticker must increment the engine");
        }
        drop(ticker);
    }

    #[test]
    fn test_epoch_ticker_stops_on_drop() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let ticker = EpochTicker::start(engine.clone());
        // Drop should signal shutdown and join the thread without hanging.
        drop(ticker);
    }

    #[test]
    fn test_guest_compute_budget_is_reasonable() {
        // The budget should be long enough for normal operations but short
        // enough to prevent indefinite hangs.
        const {
            assert!(
                GuestCallBudget::PRODUCTION.compute().as_secs() >= 10,
                "compute budget too short for normal operations"
            );
            assert!(
                GuestCallBudget::PRODUCTION.compute().as_secs() <= 120,
                "compute budget too long to be useful"
            );
        }
    }

    #[test]
    fn test_cache_key_changes_when_content_changes() {
        let engine = WasmProviderFactory::create_engine().unwrap();
        let key1 = WasmProviderFactory::cache_key(b"content_v1", &engine);
        let key2 = WasmProviderFactory::cache_key(b"content_v2", &engine);

        assert_ne!(
            key1, key2,
            "cache key should change when file content changes"
        );
    }

    #[test]
    fn test_cache_key_stable_for_same_content() {
        let engine = WasmProviderFactory::create_engine().unwrap();
        let key1 = WasmProviderFactory::cache_key(b"same_content", &engine);
        let key2 = WasmProviderFactory::cache_key(b"same_content", &engine);

        assert_eq!(key1, key2, "cache key should be stable for same content");
    }

    #[test]
    fn cache_key_covers_engine_configuration() {
        let standard = WasmProviderFactory::create_engine().unwrap();
        let equivalent = WasmProviderFactory::create_engine().unwrap();
        let mut config = build_engine_config();
        config.epoch_interruption(false);
        let different = Engine::new(&config).unwrap();
        assert_eq!(
            WasmProviderFactory::cache_key(b"component", &standard),
            WasmProviderFactory::cache_key(b"component", &equivalent),
        );
        assert_ne!(
            WasmProviderFactory::cache_key(b"component", &standard),
            WasmProviderFactory::cache_key(b"component", &different),
            "incompatible engines must not overwrite each other's compiled artifacts",
        );
    }

    #[test]
    fn cache_key_covers_wasmtime_serialization_version() {
        let standard = WasmProviderFactory::create_engine().unwrap();
        let mut config = build_engine_config();
        config
            .module_version(wasmtime::ModuleVersionStrategy::Custom(
                "other-version".into(),
            ))
            .unwrap();
        let different = Engine::new(&config).unwrap();
        assert_ne!(
            WasmProviderFactory::cache_key(b"component", &standard),
            WasmProviderFactory::cache_key(b"component", &different),
            "serialization version changes must select a different cache artifact",
        );
    }

    #[test]
    fn precompiled_deserialization_error_preserves_full_wasmtime_chain() {
        let dir = tempfile::tempdir().unwrap();
        let cwasm_path = dir.path().join("corrupt.cwasm");
        std::fs::write(&cwasm_path, b"not a precompiled component").unwrap();
        let engine = Engine::new(&build_engine_config()).unwrap();

        let error =
            match WasmProviderFactory::deserialize_precompiled_component(&engine, &cwasm_path) {
                Ok(_) => panic!("corrupt precompiled component must fail deserialization"),
                Err(error) => error,
            };
        let source_chain: Vec<_> = error
            .source
            .text
            .redact(&crate::provider_text_fixtures::context(&[]))
            .split("\n  caused by: ")
            .map(ToString::to_string)
            .collect();
        assert!(
            source_chain.len() >= 2,
            "fixture must produce a genuinely nested Wasmtime error: {source_chain:?}"
        );

        let mut expected_chain = source_chain[0].clone();
        for cause in &source_chain[1..] {
            expected_chain.push_str("\n  caused by: ");
            expected_chain.push_str(cause);
        }
        assert_eq!(
            error.to_string(),
            format!(
                "Failed to deserialize WASM component from {}: {expected_chain}",
                cwasm_path.display()
            )
        );
        assert!(
            std::error::Error::source(&error).is_none(),
            "the self-contained Display contract must not expose the rendered chain again"
        );
    }

    fn render_wasi_http_import_context(
        imports: WasiHttpImports,
    ) -> (WasiHttpCompatibility, String) {
        let host_version = semver::Version::parse(WASI_HTTP_HOST_VERSION).unwrap();
        let compatibility = wasi_http_compatibility(&host_version, &imports);
        let error = ProviderInstantiationError {
            primary_failure: WasmDiagnostic::new(
                wasmtime::Error::msg("fixture instantiation failure"),
                false,
            ),
            context: Box::new(InstantiationContext::SingleAttempt {
                world: InstantiationWorld::HttpEnabled,
                context: ComponentInstantiationContext::WasiHttpImported {
                    context: WasiHttpImportedContext {
                        imports,
                        host_version,
                        compatibility,
                    },
                },
            }),
        };
        (compatibility, error.to_string())
    }

    fn one_wasi_http_import(name: &str, version: WasiHttpImportVersion) -> WasiHttpImports {
        WasiHttpImports {
            first: WasiHttpImport {
                name: name.into(),
                version,
            },
            rest: Vec::new(),
        }
    }

    #[test]
    fn identical_wasi_http_versions_are_rendered_as_identical() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.2.6",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 2, 6)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Compatible);
        assert!(
            rendered.contains(
                "The component and host use the identical wasi:http version 0.2.6, so the wasi:http version is not the problem."
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("matches 0.2.x patch versions bidirectionally"),
            "{rendered}"
        );
    }

    #[test]
    fn differing_compatible_wasi_http_versions_explain_patch_matching() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.2.9",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 2, 9)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Compatible);
        assert!(
            rendered.contains(
                "These wasi:http versions are compatible but differ: Wasmtime's component linker matches 0.2.x patch versions bidirectionally, so the version difference is not the problem."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn incompatible_wasi_http_import_renders_component_linker_wording() {
        let imports = one_wasi_http_import(
            "wasi:http/outgoing-handler@0.3.0",
            WasiHttpImportVersion::Semver(semver::Version::new(0, 3, 0)),
        );

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Incompatible);
        assert!(
            rendered.contains(
                "These wasi:http versions are not semver-compatible under Wasmtime's component-linker rules."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn unknown_wasi_http_import_versions_render_uncertain_compatibility() {
        let imports = WasiHttpImports {
            first: WasiHttpImport {
                name: "wasi:http/types".into(),
                version: WasiHttpImportVersion::Unversioned,
            },
            rest: vec![WasiHttpImport {
                name: "wasi:http/outgoing-handler@not-semver".into(),
                version: WasiHttpImportVersion::Invalid,
            }],
        };

        let (compatibility, rendered) = render_wasi_http_import_context(imports);

        assert_eq!(compatibility, WasiHttpCompatibility::Unknown);
        assert!(
            rendered.contains(
                "Compatibility could not be determined because at least one wasi:http import has no valid semantic version."
            ),
            "{rendered}"
        );
    }

    /// Regression guard for carina#3688: the HTTP path is the real
    /// instantiation attempt. Its complete cause chain must lead the
    /// diagnostic, while the structurally-inapplicable basic fallback must
    /// not surface its predetermined "wasi:http not in linker" error as a
    /// peer cause.
    #[test]
    fn provider_instantiation_error_preserves_full_http_chain_and_demotes_fallback() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                  (import "wasi:http/outgoing-handler@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let http_error = wasmtime::Error::msg(
            "parameter 2 expected own<wasi:http/types.request> but found borrow",
        )
        .context("failed to convert function to given type")
        .context("Failed to instantiate WASM component (HTTP)");
        let basic_error = wasmtime::Error::msg(
            "component imports instance `wasi:http/types@0.2.9`, but a matching implementation was not found in the linker",
        )
        .context("Failed to instantiate WASM component");

        let error =
            ProviderInstantiationError::from_attempts(&engine, &component, http_error, basic_error);
        let msg = error.to_string();

        assert!(
            std::error::Error::source(&error).is_none(),
            "the self-contained Display contract must not expose the rendered chain again"
        );

        let outer = msg
            .find("Failed to instantiate WASM component (HTTP)")
            .unwrap();
        let middle = msg
            .find("failed to convert function to given type")
            .unwrap();
        let root = msg
            .find("parameter 2 expected own<wasi:http/types.request> but found borrow")
            .unwrap();
        assert!(
            outer < middle && middle < root,
            "cause chain out of order: {msg}"
        );
        assert!(
            msg.starts_with("Failed to instantiate WASM component (HTTP)"),
            "the real HTTP failure must lead: {msg}"
        );
        assert!(msg.contains("wasi:http/outgoing-handler@0.2.9"));
        assert!(msg.contains("wasi:http/types@0.2.9"));
        assert!(msg.contains("Host wasi:http version: 0.2.6"));
        assert!(
            msg.contains("These wasi:http versions are compatible"),
            "compatible versions must be ruled out affirmatively: {msg}"
        );
        assert!(msg.contains("matches 0.2.x patch versions bidirectionally"));
        assert!(msg.contains("Basic fallback diagnostic omitted"));
        assert!(
            !msg.contains("matching implementation was not found in the linker"),
            "the predetermined fallback error must not compete with the real failure: {msg}"
        );
        assert!(!msg.contains("HTTP-enabled world failed"));
        assert!(!msg.contains("basic fallback also failed"));
    }

    #[tokio::test]
    async fn selected_http_runtime_attempt_returns_structured_instantiation_error() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "bogus:runtime/iface@1.0.0" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();

        let error = match create_runtime_instance(
            &engine,
            &component,
            local_provider_label("runtime-test"),
            GuestNetwork::SchemaOnly,
            InstantiationWorld::HttpEnabled,
            GuestCallBudget::PRODUCTION,
            None,
            None,
        )
        .await
        {
            Ok(_) => panic!("the unresolved bogus import must reject instantiation"),
            Err(error) => error,
        };
        let WasmProviderInstanceError::Instantiation(instantiation) = error else {
            panic!("expected instantiation error")
        };
        let rendered = instantiation.to_string();

        assert!(rendered.contains("bogus:runtime/iface@1.0.0"));
        assert!(rendered.contains("Component wasi:http imports: none detected"));
        assert!(rendered.contains("Single runtime instantiation attempt: HTTP-enabled world"));
        assert!(rendered.contains("No basic fallback was attempted"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[tokio::test]
    async fn selected_basic_runtime_attempt_returns_structured_instantiation_error() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();

        let error = match create_runtime_instance(
            &engine,
            &component,
            local_provider_label("runtime-test"),
            GuestNetwork::SchemaOnly,
            InstantiationWorld::Basic,
            GuestCallBudget::PRODUCTION,
            None,
            None,
        )
        .await
        {
            Ok(_) => panic!("the basic linker must reject the wasi:http import"),
            Err(error) => error,
        };
        let WasmProviderInstanceError::Instantiation(instantiation) = error else {
            panic!("expected instantiation error")
        };
        let rendered = instantiation.to_string();

        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: basic world"));
        assert!(rendered.contains("No fallback attempt was made"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn single_http_instantiation_error_keeps_typed_context_without_claiming_a_fallback() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let failure = wasmtime::Error::msg("runtime HTTP type mismatch")
            .context("failed to convert function to given type")
            .context("Failed to instantiate WASM component (HTTP)");

        let error = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            failure,
        );
        let rendered = error.to_string();

        assert!(rendered.starts_with("Failed to instantiate WASM component (HTTP)"));
        assert!(rendered.contains("runtime HTTP type mismatch"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: HTTP-enabled world"));
        assert!(rendered.contains("No basic fallback was attempted"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn single_basic_instantiation_error_keeps_typed_context_without_demotion() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let failure = wasmtime::Error::msg(
            "component imports instance `wasi:http/types@0.2.9`, but a matching implementation was not found in the linker",
        )
        .context("Failed to instantiate WASM component");

        let error = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::Basic,
            failure,
        );
        let rendered = error.to_string();

        assert!(rendered.starts_with("Failed to instantiate WASM component"));
        assert!(rendered.contains("matching implementation was not found in the linker"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt: basic world"));
        assert!(rendered.contains("No fallback attempt was made"));
        assert!(!rendered.contains("Basic fallback diagnostic omitted"));
        assert!(!rendered.contains("Subordinate basic fallback attempt"));
    }

    #[test]
    fn runtime_instantiation_error_remains_typed_at_provider_error_boundary() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(
            &engine,
            r#"
                (component
                  (type $empty (instance))
                  (import "wasi:http/types@0.2.9" (instance (type $empty)))
                )
            "#,
        )
        .unwrap();
        let instantiation = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            wasmtime::Error::msg("runtime instantiation failed"),
        );

        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::Instantiation(instantiation)
            .into_provider_error_with(&mapper)
            .into_host_for_test();
        let cause = provider_error
            .detail()
            .cause()
            .expect("provider boundary must retain the typed instantiation cause");

        assert!(cause.downcast_ref::<ProviderInstantiationError>().is_some());
        let rendered = provider_error.to_string();
        assert!(rendered.contains("runtime instantiation failed"));
        assert!(rendered.contains("wasi:http/types@0.2.9"));
        assert!(rendered.contains("Host wasi:http version: 0.2.6"));
        assert!(rendered.contains("These wasi:http versions are compatible"));
        assert!(rendered.contains("Single runtime instantiation attempt"));
    }

    #[test]
    fn runtime_info_interrupt_uses_guest_timeout_classification() {
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::InfoCall(
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while calling provider info"),
        )
        .into_provider_error_with(&mapper)
        .into_host_for_test();

        assert_eq!(provider_error.variant_name(), "timeout");
        assert_eq!(provider_error.detail().context(), "WASM plugin timed out");
        let provider_rendered = provider_error.to_string();
        assert!(provider_rendered.contains("info exceeded the guest compute budget"));
        assert!(!provider_rendered.to_lowercase().contains("credential"));
        let cause = provider_error
            .detail()
            .cause()
            .expect("info timeout must retain the typed guest-call cause");
        assert!(cause.downcast_ref::<WasmGuestCallError>().is_some());
        let rendered = cause.to_string();
        assert!(
            rendered.contains("error while calling provider info"),
            "{rendered}"
        );
        assert_eq!(
            rendered
                .matches("error while calling provider info")
                .count(),
            1,
            "{rendered}"
        );
        assert!(cause.source().is_some());
        assert!(cause.source_is::<wasmtime::Trap>());
    }

    #[tokio::test]
    async fn runtime_wall_timeout_keeps_typed_budget_source_without_duplicate_text() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let mut store = GuestStore::new(
            &engine,
            local_provider_label("wall-budget-test"),
            GuestNetwork::SchemaOnly,
            None,
            build_store_limits(),
            GuestCallBudget::new(
                std::time::Duration::from_secs(30),
                std::time::Duration::ZERO,
            ),
        );
        let source = store
            .call(|_store| std::future::pending::<()>())
            .await
            .expect_err("a zero wall budget must time out");
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::WallBudgetExceeded {
            operation: "initialize",
            source,
        }
        .into_provider_error_with(&mapper)
        .into_host_for_test();

        assert_eq!(provider_error.variant_name(), "timeout");
        let operation_cause = provider_error
            .detail()
            .cause()
            .expect("wall timeout must retain an operation-aware typed cause");
        assert!(
            operation_cause
                .downcast_ref::<WasmWallBudgetError>()
                .is_some(),
            "unexpected cause type: {operation_cause:?}"
        );
        let budget_cause = operation_cause
            .source()
            .expect("operation-aware timeout must retain WallBudgetExceeded");
        assert!(budget_cause.downcast_ref::<WallBudgetExceeded>().is_some());

        let rendered = provider_error.to_string();
        assert!(
            rendered.contains("WASM guest call 'initialize' timed out"),
            "{rendered}"
        );
        assert!(rendered.contains("wall-clock budget"), "{rendered}");
        assert_eq!(
            rendered.matches("wall-clock budget").count(),
            1,
            "{rendered}"
        );
        assert!(
            rendered.contains("check network connectivity"),
            "{rendered}"
        );
        assert!(
            !rendered.to_lowercase().contains("credential"),
            "{rendered}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialization_values_fail_at_shared_wire_preparation_before_instance_creation() {
        let factory = rebuildable_state_test_factory()
            .await
            .expect("mock WASM fixture is required");
        let error = prepare_test_config(
            &factory,
            IndexMap::from([(
                "extra".into(),
                Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
                    u64::MAX,
                ))),
            )]),
        )
        .unwrap_err();
        let carina_core::executor::ProviderConfigPreparationError::Wire(wire) = &error else {
            panic!("expected shared wire rejection: {error:?}")
        };
        assert!(matches!(
            wire.conversion_error(),
            carina_core::protocol_convert::ProtocolConvertError::DurationOutOfRange
        ));
        assert_eq!(wire.attribute(), "extra");
        assert_eq!(
            wire.target(),
            &carina_core::provider::ProviderWireTarget::Configuration {
                provider: "mock".into()
            }
        );
        assert_eq!(
            error.to_string(),
            "provider mock configuration: attribute `extra` cannot cross the provider wire: duration in seconds does not fit the provider protocol integer domain"
        );
        assert!(std::error::Error::source(&error).is_none());
        assert!(factory.shared_instances.lock().await.is_empty());
    }

    #[test]
    fn initialization_call_interrupt_uses_guest_timeout_classification() {
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let provider_error = WasmProviderInstanceError::InitializeCall(
            wasmtime::Error::new(wasmtime::Trap::Interrupt)
                .context("error while calling provider initialize"),
        )
        .into_provider_error_with(&mapper)
        .into_host_for_test();

        assert_eq!(provider_error.variant_name(), "timeout");
        assert_eq!(provider_error.detail().context(), "WASM plugin timed out");
        assert_eq!(
            provider_error.to_string(),
            "WASM plugin timed out: initialize exceeded the guest compute budget: error while calling provider initialize: wasm trap: interrupt"
        );
        assert!(
            !provider_error
                .to_string()
                .to_lowercase()
                .contains("credential")
        );
    }

    #[test]
    fn initialization_error_preserves_decoded_provider_error_details() {
        let original = ProviderError::api_error_caused_by(
            "Initialization request failed",
            std::io::Error::other("credential service rejected the request"),
        )
        .with_operation("sts.GetCallerIdentity")
        .with_status(403)
        .with_code("AccessDenied")
        .with_request_id("request-123");
        let decoded = wasm_convert::decode_provider_initialization_error(original.to_wire());
        let mapper: ProviderInstantiationErrorMapper =
            Arc::new(default_provider_instantiation_error_mapper);
        let mapped = WasmProviderInstanceError::Initialize(decoded)
            .into_provider_error_with(&mapper)
            .into_host_for_test()
            .for_provider("awscc");

        {
            use carina_core::provider::HostError as ProviderError;
            assert!(matches!(mapped, ProviderError::ApiError(_)));
        }
        let detail = mapped.detail();
        assert_eq!(
            detail
                .provider_name
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("awscc")
        );
        assert_eq!(
            detail
                .operation
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("sts.GetCallerIdentity")
        );
        assert_eq!(detail.status, Some(403));
        assert_eq!(detail.code().as_deref(), Some("AccessDenied"));
        assert_eq!(
            detail
                .request_id
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("request-123")
        );
        assert_eq!(
            mapped.to_string(),
            "Initialization request failed\n  operation: sts.GetCallerIdentity\n  status: 403 AccessDenied\n  request_id: request-123\n  cause: credential service rejected the request"
        );
    }

    #[test]
    fn runtime_instantiation_error_accepts_a_typed_composition_mapper() {
        let config = build_engine_config();
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(&engine, "(component)").unwrap();
        let instantiation = ProviderInstantiationError::from_single_attempt(
            &engine,
            &component,
            InstantiationWorld::HttpEnabled,
            wasmtime::Error::msg("runtime instantiation failed"),
        );
        let mapper: ProviderInstantiationErrorMapper = Arc::new(|error| {
            ProviderError::internal_caused_by("artifact provenance attached", error)
        });

        let provider_error = WasmProviderInstanceError::Instantiation(instantiation)
            .into_provider_error_with(&mapper)
            .into_host_for_test();

        assert!(
            provider_error
                .to_string()
                .contains("artifact provenance attached")
        );
        assert!(
            provider_error
                .to_string()
                .contains("runtime instantiation failed")
        );
    }

    /// `WASI_HTTP_HOST_VERSION` is user-facing compatibility evidence, so a
    /// dependency bump must force an explicit review of the dependency's
    /// `wit/deps/http.wit` package version instead of silently leaving stale
    /// output behind.
    #[test]
    fn host_wasi_http_version_tracks_locked_wasmtime_wasi_http_pin() {
        let lock = include_str!("../../../Cargo.lock");
        let mut locked_versions: Vec<_> = lock
            .split("[[package]]")
            .filter(|package| {
                package
                    .lines()
                    .any(|line| line.trim() == "name = \"wasmtime-wasi-http\"")
            })
            .filter_map(|package| {
                package.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("version = \"")
                        .and_then(|version| version.strip_suffix('\"'))
                })
            })
            .collect();
        locked_versions.sort_unstable();
        locked_versions.dedup();

        assert_eq!(
            locked_versions,
            [WASMTIME_WASI_HTTP_CRATE_VERSION_WITH_KNOWN_WIT],
            "inspect wasmtime-wasi-http's wit/deps/http.wit and update WASI_HTTP_HOST_VERSION"
        );
    }

    #[test]
    fn cancellation_poison_fails_fast_naming_the_rejected_operation() {
        use carina_core::provider::HostError as ProviderError;
        let guard_err = PoisonReason::Cancelled
            .fail_fast_error("delete")
            .into_host_for_test();
        assert!(
            matches!(guard_err, ProviderError::Internal(_)),
            "poisoned fail-fast must be a hard error, got {guard_err:?}"
        );
        let msg = format!("{guard_err:?}");
        assert!(
            msg.contains("delete") && msg.contains("re-run"),
            "fail-fast error should name the rejected op and tell the user to re-run, got: {msg}"
        );
    }

    #[test]
    fn validation_context_fingerprint_is_canonical_and_secret_safe() {
        let key = [7_u8; 32];
        let other_key = [8_u8; 32];
        let attributes = [
            (
                "secret".to_string(),
                wit_types::Value::SecretVal("do-not-retain-me".to_string()),
            ),
            ("count".to_string(), wit_types::Value::IntVal(3)),
        ];
        let first = ValidationContextFingerprint::validate_config(&key, &attributes);
        let reordered = ValidationContextFingerprint::validate_config(
            &key,
            &[
                ("count".to_string(), wit_types::Value::IntVal(3)),
                (
                    "secret".to_string(),
                    wit_types::Value::SecretVal("do-not-retain-me".to_string()),
                ),
            ],
        );
        let changed = ValidationContextFingerprint::validate_config(
            &key,
            &[
                ("count".to_string(), wit_types::Value::IntVal(4)),
                (
                    "secret".to_string(),
                    wit_types::Value::SecretVal("do-not-retain-me".to_string()),
                ),
            ],
        );
        let different_factory =
            ValidationContextFingerprint::validate_config(&other_key, &attributes);

        assert_eq!(first, reordered);
        assert_ne!(first, changed);
        assert_ne!(first, different_factory);
        assert_eq!(
            first.suspended_validation_id(),
            reordered.suspended_validation_id()
        );
        assert_ne!(
            first.suspended_validation_id(),
            changed.suspended_validation_id()
        );
        assert_ne!(
            first.suspended_validation_id(),
            different_factory.suspended_validation_id()
        );
        let debug = format!("{first:?}");
        assert!(!debug.contains("do-not-retain-me"));
        assert!(debug.contains("<redacted>"), "fingerprint debug: {debug}");
        assert!(
            !debug.contains("input_sha256"),
            "the keyed digest must not be exposed through Debug: {debug}"
        );
    }

    #[test]
    fn validation_suspends_after_two_timeouts_despite_other_input_successes() {
        let context = ValidationContextFingerprint::validate_config(
            &[0; 32],
            &[("probe".to_string(), wit_types::Value::IntVal(1))],
        );
        let other = ValidationContextFingerprint::validate_config(
            &[0; 32],
            &[("probe".to_string(), wit_types::Value::IntVal(2))],
        );
        let reason = PoisonReason::WallClockTimeout {
            operation: "validate_config",
        };
        let mut state = RebuildableState {
            inputs: carina_core::provider::ProviderPlan::state_only()
                .session("", None, None)
                .guest_inventory(),
            instance: ValidationInstanceState::Poisoned {
                reason: reason.clone(),
            },
            input_health: IndexMap::new(),
        };

        for attempt in 1..=2 {
            state.record_wall_clock_timeout(context.clone(), reason.clone());
            state.record_success(&other);
            assert_eq!(state.suspended_reason(&context).is_some(), attempt == 2);
        }
        let error = suspended_validation_error(state.suspended_reason(&context).unwrap(), &context);
        assert_eq!(
            error.detail().host_cause(),
            Some(carina_core::provider::HostErrorCause::ValidationSuspended(
                context.suspended_validation_id()
            ))
        );
        assert!(state.suspended_reason(&other).is_none());
        assert_eq!(state.input_health.len(), 1);
    }

    #[tokio::test]
    async fn review_round4_validation_instance_does_not_retain_another_session() {
        let factory = rebuildable_state_test_factory()
            .await
            .expect("mock WASM fixture is required");
        let instance = &factory.init_instance;
        let first =
            instance.validate_config_context(&[("probe".into(), wit_types::Value::IntVal(1))]);
        let second =
            instance.validate_config_context(&[("probe".into(), wit_types::Value::IntVal(2))]);
        let session = crate::provider_text_fixtures::context(&["previous-validation-private"])
            .session()
            .guest_inputs();
        instance
            .acquire(first)
            .await
            .unwrap()
            .with_secrets(session)
            .call(async |_, _| Ok::<_, wasmtime::Error>(()))
            .await
            .unwrap();
        let other_factory = rebuildable_state_test_factory()
            .await
            .expect("mock WASM fixture is required");
        let locked = other_factory.init_instance.acquire(second).await.unwrap();
        let retained = match &locked.state.as_ref().unwrap().instance {
            ValidationInstanceState::Live(live) => live.store.known_secrets().is_some(),
            ValidationInstanceState::Poisoned { .. } => panic!("a successful acquisition is live"),
        };
        locked
            .call(async |_, _| Ok::<_, wasmtime::Error>(()))
            .await
            .unwrap();
        assert!(
            !retained,
            "a validator must not retain another input's plaintext or redaction session"
        );
        // Independent validation owners stay isolated even for equal guest values.
        let context =
            instance.validate_config_context(&[("probe".into(), wit_types::Value::IntVal(3))]);
        let first = crate::provider_text_fixtures::context(&["previous-validation-private"])
            .session()
            .guest_inputs();
        instance
            .acquire_in_session(context.clone(), Some(first))
            .await
            .unwrap()
            .call(async |_, _| Ok::<_, wasmtime::Error>(()))
            .await
            .unwrap();
        let second = crate::provider_text_fixtures::context(&[])
            .session()
            .guest_inputs();
        let other_factory = rebuildable_state_test_factory()
            .await
            .expect("mock WASM fixture is required");
        let locked = other_factory
            .init_instance
            .acquire_in_session(context, Some(second))
            .await
            .unwrap();
        let retained = match &locked.state.as_ref().unwrap().instance {
            ValidationInstanceState::Live(live) => live.store.known_secrets().is_some(),
            ValidationInstanceState::Poisoned { .. } => panic!("a successful acquisition is live"),
        };
        locked
            .call(async |_, _| Ok::<_, wasmtime::Error>(()))
            .await
            .unwrap();
        assert!(
            !retained,
            "equal guest values must not reuse a different session's instance"
        );
    }

    #[tokio::test]
    async fn rebuildable_state_retains_timeout_history_across_other_input_successes() {
        let Some(factory) = rebuildable_state_test_factory().await else {
            return;
        };
        let instance = &factory.init_instance;
        let context_x =
            instance.validate_config_context(&[("probe".to_string(), wit_types::Value::IntVal(1))]);
        let context_y =
            instance.validate_config_context(&[("probe".to_string(), wit_types::Value::IntVal(2))]);

        assert!(matches!(
            &instance.state.lock().await.instance,
            ValidationInstanceState::Live(_)
        ));

        instance
            .acquire(context_x.clone())
            .await
            .expect("fresh validation instance should be live")
            .wall_clock_timeout();
        let rebuilt_for_y = instance
            .acquire(context_y.clone())
            .await
            .expect("another input should rebuild the poisoned instance");
        rebuilt_for_y
            .call(async |_bindings, _store| Ok::<_, wasmtime::Error>(()))
            .await
            .expect("the other input should succeed");

        instance
            .acquire(context_x.clone())
            .await
            .expect("the second call for X should use the live instance")
            .wall_clock_timeout();

        let suspended = match instance.acquire(context_x.clone()).await {
            Ok(_) => panic!("X must be suspended after its second timeout"),
            Err(error) => error,
        };
        assert_eq!(
            suspended.detail().host_cause(),
            Some(carina_core::provider::HostErrorCause::ValidationSuspended(
                context_x.suspended_validation_id()
            ))
        );

        instance
            .acquire(context_y)
            .await
            .expect("Y should rebuild while X remains suspended")
            .call(async |_bindings, _store| Ok::<_, wasmtime::Error>(()))
            .await
            .expect("Y should remain usable");
        assert!(
            instance.acquire(context_x).await.is_err(),
            "Y's success must not clear X's suspension"
        );
    }

    #[tokio::test]
    async fn successful_input_clears_its_own_first_timeout() {
        let Some(factory) = rebuildable_state_test_factory().await else {
            return;
        };
        let instance = &factory.init_instance;
        let context =
            instance.validate_config_context(&[("probe".to_string(), wit_types::Value::IntVal(1))]);

        instance
            .acquire(context.clone())
            .await
            .unwrap()
            .wall_clock_timeout();
        instance
            .acquire(context.clone())
            .await
            .expect("one timeout still permits a retry")
            .call(async |_bindings, _store| Ok::<_, wasmtime::Error>(()))
            .await
            .expect("the retry should succeed");
        assert!(
            !instance
                .state
                .lock()
                .await
                .input_health
                .contains_key(&context)
        );

        instance
            .acquire(context.clone())
            .await
            .unwrap()
            .wall_clock_timeout();
        instance
            .acquire(context)
            .await
            .expect("a timeout after success must be treated as the first timeout again");
    }

    #[tokio::test]
    async fn validation_input_health_evicts_the_oldest_entry_at_the_bound() {
        let Some(factory) = rebuildable_state_test_factory().await else {
            return;
        };
        let instance = &factory.init_instance;
        let contexts = (0..=MAX_VALIDATION_INPUT_HEALTH_ENTRIES)
            .map(|value| {
                instance.validate_config_context(&[(
                    "probe".to_string(),
                    wit_types::Value::IntVal(i64::try_from(value).unwrap()),
                )])
            })
            .collect::<Vec<_>>();

        let mut state = instance.state.lock().await;
        for context in &contexts {
            state.record_wall_clock_timeout(
                context.clone(),
                PoisonReason::WallClockTimeout {
                    operation: "validate_config",
                },
            );
        }

        assert_eq!(
            state.input_health.len(),
            MAX_VALIDATION_INPUT_HEALTH_ENTRIES
        );
        assert!(!state.input_health.contains_key(&contexts[0]));
        assert!(state.input_health.contains_key(contexts.last().unwrap()));
    }

    #[tokio::test]
    async fn trap_after_timeout_rebuild_poisoned_but_does_not_suspend() {
        let Some(factory) = rebuildable_state_test_factory().await else {
            return;
        };
        let instance = &factory.init_instance;
        let context = instance.validate_config_context(&[]);

        instance
            .acquire(context.clone())
            .await
            .expect("fresh validation instance should be live")
            .wall_clock_timeout();
        let rebuilt = instance
            .acquire(context.clone())
            .await
            .expect("the timeout should permit one rebuild");
        let error = rebuilt
            .call(async |_bindings, _store| {
                Err::<(), _>(wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached))
            })
            .await
            .expect_err("the synthetic trap must fail");
        assert_eq!(error.variant_name(), "internal");
        assert!(matches!(
            &instance.state.lock().await.instance,
            ValidationInstanceState::Poisoned {
                reason: PoisonReason::Trap { .. }
            }
        ));

        let rebuilt_after_trap = instance
            .acquire(context)
            .await
            .expect("traps should continue to rebuild on every call");
        assert!(matches!(
            rebuilt_after_trap
                .state
                .as_deref()
                .map(|state| &state.instance),
            Some(ValidationInstanceState::Live(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn returned_trap_poison_is_classified_inside_call() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let factory = WasmProviderFactory::new_uncached(
            &wasm_bytes,
            test_load_identity("mock"),
            &UnusedAwsCredentialProvisioner,
        )
        .await
        .expect("mock WASM provider should load");
        let instance = factory
            .get_or_create_shared_instance(None, &empty_ready_config(&factory))
            .await
            .expect("mock WASM provider should initialize");

        let locked = instance
            .acquire("normalize_state")
            .await
            .expect("fresh instance should be usable");
        let call_error = locked
            .call("normalize_state", async |_bindings, _store| {
                Err::<(), _>(wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached))
            })
            .await
            .expect_err("the returned trap must fail the guest call")
            .into_host_for_test();
        assert_eq!(call_error.detail().context(), "WASM guest trapped");

        let next_error = instance
            .acquire("create")
            .await
            .err()
            .expect("the returned trap must poison the slot before call returns")
            .into_host_for_test();
        let rendered = next_error.to_string();
        assert!(
            rendered.contains("WASM provider instance unusable after trap in normalize_state"),
            "the fail-fast error must retain the trap origin: {rendered}"
        );
        assert!(
            rendered.contains("operation 'create'"),
            "the fail-fast error must name the rejected operation: {rendered}"
        );
    }

    #[test]
    fn trap_poison_names_the_origin_and_keeps_only_the_structured_cause() {
        let trap = wasmtime::Error::new(wasmtime::Trap::UnreachableCodeReached)
            .context("error while executing at wasm backtrace:\n    0: test-frame");
        let reason = PoisonReason::from_trap(
            "normalize_state",
            &trap,
            crate::provider_text_fixtures::context(&[])
                .session()
                .redaction_set(),
        );
        let error = reason.fail_fast_error("create").into_host_for_test();
        let rendered = error.to_string();
        assert!(
            rendered.contains(
                "WASM provider instance unusable after trap in normalize_state: wasm trap: wasm `unreachable` instruction executed"
            ),
            "the original trap must remain actionable: {rendered}"
        );
        assert!(
            !rendered.contains("wasm backtrace") && !rendered.contains("test-frame"),
            "later errors must not repeat the original trap backtrace: {rendered}"
        );
        assert!(
            rendered.contains("operation 'create'"),
            "the rejected follow-up operation should remain visible: {rendered}"
        );
        assert!(
            !rendered.contains("prior operation timed out"),
            "component traps must not be mislabeled as timeouts: {rendered}"
        );
    }

    #[tokio::test]
    async fn rebuild_uses_the_recorded_basic_world() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let wasm_path = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
            .into_iter()
            .map(|name| workspace_root.join("target/wasm32-wasip2/debug").join(name))
            .find(|path| path.exists());
        let Some(wasm_path) = wasm_path else {
            eprintln!(
                "SKIP: WASM binary not found. Build with: cargo build -p \
                 carina-provider-mock --target wasm32-wasip2"
            );
            return;
        };

        let config = build_engine_config();
        let engine = Engine::new(&config).expect("test engine should be valid");
        let wasm_bytes = std::fs::read(wasm_path).expect("read mock WASM provider");
        let component = Component::new(&engine, wasm_bytes)
            .expect("mock WASM provider component should compile");
        let initial = match create_instance_for_world(
            &engine,
            &component,
            local_provider_label("mock"),
            GuestNetwork::SchemaOnly,
            InstantiationWorld::Basic,
            GuestCallBudget::PRODUCTION,
            None,
            None,
        )
        .await
        {
            Ok(initial) => initial,
            Err(_) => panic!("the mock provider must support the basic world"),
        };
        assert_eq!(initial.instantiation_world(), InstantiationWorld::Basic);
        let instance = RebuildableInstance::new(
            initial,
            RebuildContext {
                engine,
                component,
                label: local_provider_label("mock"),
                network: GuestNetwork::SchemaOnly,
                world: InstantiationWorld::Basic,
                budget: GuestCallBudget::PRODUCTION,
                http_test_route: None,
            },
        );
        instance.state.lock().await.instance = ValidationInstanceState::Poisoned {
            reason: PoisonReason::Cancelled,
        };

        let context = instance.validate_config_context(&[]);
        let locked = instance
            .acquire(context)
            .await
            .expect("the poisoned basic-world instance should rebuild");
        locked
            .call(async |bindings, store| bindings.call_validate_config(store, &[]).await)
            .await
            .expect("the rebuilt basic-world guest call should not trap")
            .expect("the mock provider should accept an empty config");

        assert_eq!(
            instance.live_world().await,
            Some(InstantiationWorld::Basic),
            "the rebuilt live instance must record the basic world actually instantiated"
        );
    }

    #[tokio::test]
    async fn rebuild_failure_is_typed_leaves_slot_poisoned_and_retries() {
        let config = build_engine_config();
        let engine = Engine::new(&config).expect("test engine should be valid");
        let component =
            Component::new(&engine, "(component)").expect("empty test component should compile");
        let fingerprint_key = [9_u8; 32];
        let context = ValidationContextFingerprint::validate_config(&fingerprint_key, &[]);
        let instance = RebuildableInstance {
            state: Mutex::new(RebuildableState {
                inputs: carina_core::provider::ProviderPlan::state_only()
                    .session("", None, None)
                    .guest_inventory(),
                instance: ValidationInstanceState::Poisoned {
                    reason: PoisonReason::WallClockTimeout {
                        operation: "validate_config",
                    },
                },
                input_health: IndexMap::new(),
            }),
            rebuild: RebuildContext {
                engine,
                component,
                label: local_provider_label("rebuild-test"),
                network: GuestNetwork::SchemaOnly,
                world: InstantiationWorld::Basic,
                budget: GuestCallBudget::PRODUCTION,
                http_test_route: None,
            },
            fingerprint_key,
            additional_instantiations: std::sync::atomic::AtomicUsize::new(0),
        };

        for attempt in 1..=2 {
            let error = match instance.acquire(context.clone()).await {
                Ok(_) => panic!("the empty component rebuild must fail"),
                Err(error) => error,
            }
            .into_host_for_test();
            assert_eq!(error.variant_name(), "internal");
            assert_eq!(
                error.detail().context(),
                "Failed to rebuild WASM provider validation instance"
            );
            let rendered = error.to_string();
            assert!(
                rendered.contains(
                    "previous poison reason: WASM provider instance unusable after wall-clock \
                     timeout in validate_config"
                ),
                "rebuild attempt {attempt} must retain the original poison reason: {rendered}"
            );
            let cause = error
                .detail()
                .cause()
                .expect("rebuild failure must retain its typed rebuild error");
            let rebuild_error = cause
                .downcast_ref::<WasmProviderRebuildError>()
                .expect("rebuild failure cause must remain typed");
            let load_error = rebuild_error
                .source()
                .and_then(|source| source.downcast_ref::<WasmProviderLoadError>())
                .expect("rebuild failure must retain WasmProviderLoadError as its source");
            let _ = load_error;
            let load_error = RebuildLoadProbe::take();
            load_error.project_redacted::<WasmProviderLoadError>(&crate::provider_text_fixtures::context(&[]), |load_error| {
            let WasmProviderLoadError::Instantiation(instantiation) = load_error else {
                panic!("empty component must produce an instantiation error: {load_error:?}")
            };
            assert!(
                matches!(
                    instantiation.context.as_ref(),
                    InstantiationContext::SingleAttempt {
                        world: InstantiationWorld::Basic,
                        ..
                    }
                ),
                "rebuild attempt {attempt} must use only the recorded basic world"
            );
            let source_chain: Vec<_> = instantiation
                .primary_failure
                .text.redact(&crate::provider_text_fixtures::context(&[]))
                .split("\n  caused by: ")
                .map(ToString::to_string)
                .collect();
            assert!(
                source_chain.len() >= 2,
                "rebuild attempt {attempt} must retain Wasmtime's source chain: {source_chain:?}"
            );
                String::new()
            }).unwrap();
            assert!(
                matches!(
                    &instance.state.lock().await.instance,
                    ValidationInstanceState::Poisoned {
                        reason: PoisonReason::WallClockTimeout {
                            operation: "validate_config"
                        }
                    }
                ),
                "a failed rebuild must leave the original poisoned slot intact"
            );
        }
    }

    #[tokio::test]
    async fn wall_budget_rebuild_failure_is_timeout_and_keeps_load_error_source() {
        let config = build_engine_config();
        let engine = Engine::new(&config).expect("test engine should be valid");
        let mut store = GuestStore::new(
            &engine,
            local_provider_label("wall-budget-test"),
            GuestNetwork::SchemaOnly,
            None,
            build_store_limits(),
            GuestCallBudget::new(
                std::time::Duration::from_secs(30),
                std::time::Duration::ZERO,
            ),
        );
        let source = store
            .call(|_store| std::future::pending::<()>())
            .await
            .expect_err("a zero wall budget must time out");
        let load_error = wall_budget_load_error("instantiate", source);

        let error =
            rebuild_provider_error(PoisonReason::Cancelled, load_error).into_host_for_test();

        assert_eq!(error.variant_name(), "timeout");
        let rendered = error.to_string();
        assert!(
            rendered.contains(
                "previous poison reason: WASM provider is unusable: a prior operation was cancelled"
            ),
            "the prior poison reason must remain visible: {rendered}"
        );
        assert!(
            rendered.contains("WASM provider load timed out"),
            "the typed load failure must remain visible: {rendered}"
        );
        let cause = error
            .detail()
            .cause()
            .expect("timeout rebuild failure must retain its typed cause");
        let rebuild_error = cause
            .downcast_ref::<WasmProviderRebuildError>()
            .expect("timeout rebuild failure cause must remain typed");
        let load_error = rebuild_error
            .source()
            .and_then(|source| source.downcast_ref::<WasmProviderLoadError>())
            .expect("timeout rebuild failure must retain WasmProviderLoadError as its source");
        let _ = load_error;
        let load_error = RebuildLoadProbe::take();
        load_error
            .project_redacted::<WasmProviderLoadError>(
                &crate::provider_text_fixtures::context(&[]),
                |load_error| {
                    assert!(load_error.is_wall_budget_timeout());
                    String::new()
                },
            )
            .unwrap();
    }

    #[test]
    fn production_wall_budget_exceeds_each_provider_single_call_worst_case() {
        fn retry_backoff_total(
            max_attempts: u32,
            initial_delay: std::time::Duration,
            max_delay: std::time::Duration,
        ) -> std::time::Duration {
            (1..max_attempts).fold(std::time::Duration::ZERO, |total, attempt| {
                total
                    + initial_delay
                        .saturating_mul(2_u32.pow(attempt - 1))
                        .min(max_delay)
            })
        }

        // Mirrors carina-provider-aws/src/helpers.rs RetryPolicy::default()
        // on origin/main.
        // retry_aws_operation runs a complete SDK operation per outer attempt
        // and treats the SDK's TimeoutError as retryable.
        const AWS_RETRY_POLICY_MAX_ATTEMPTS: u32 = 8;
        const AWS_RETRY_POLICY_INITIAL_DELAY: std::time::Duration =
            std::time::Duration::from_secs(5);
        const AWS_RETRY_POLICY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(120);

        // Mirrors the bounded policy in both carina-provider-aws/src/sdk_config.rs
        // and carina-provider-awscc/src/sdk_config.rs on origin/main.
        const AWS_SDK_CONNECT_TIMEOUT: std::time::Duration =
            std::time::Duration::from_millis(3_100);
        const AWS_SDK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        const AWS_SDK_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

        // Mirrors carina-provider-aws/src/services/ec2/flow_log.rs on
        // origin/main: up to 12 send() attempts with a 5s IAM-propagation
        // sleep between attempts, followed by one read_ec2_flow_log SDK
        // operation in the same create call.
        const AWS_FLOW_LOG_SEND_ATTEMPTS: u32 = 12;
        const AWS_FLOW_LOG_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

        // Mirrors carina-provider-aws/src/services/acm/certificate.rs on
        // origin/main: one RequestCertificate send(), then six
        // wait_for_dvo_populated send() attempts at a 5s interval.
        const AWS_ACM_REQUEST_CERTIFICATE_SENDS: u32 = 1;
        const AWS_ACM_DVO_WAIT_ATTEMPTS: u32 = 6;
        const AWS_ACM_DVO_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

        let bounded_attempt_worst_case = AWS_SDK_CONNECT_TIMEOUT.min(HTTP_API_REQUEST_TIMEOUT)
            + AWS_SDK_READ_TIMEOUT.min(HTTP_API_REQUEST_TIMEOUT);
        assert_eq!(
            bounded_attempt_worst_case,
            std::time::Duration::from_millis(33_100)
        );
        // In WASM, the operation-attempt and operation deadlines are observed
        // only between attempts. The attempt deadline therefore does not
        // tighten this bound, and one SDK operation can overrun its operation
        // timeout by one bounded connect+read attempt.
        let bounded_sdk_operation_worst_case =
            AWS_SDK_OPERATION_TIMEOUT + bounded_attempt_worst_case;
        assert_eq!(
            bounded_sdk_operation_worst_case,
            std::time::Duration::from_millis(93_100)
        );

        let aws_outer_backoff = retry_backoff_total(
            AWS_RETRY_POLICY_MAX_ATTEMPTS,
            AWS_RETRY_POLICY_INITIAL_DELAY,
            AWS_RETRY_POLICY_MAX_DELAY,
        );
        assert_eq!(aws_outer_backoff, std::time::Duration::from_secs(395));
        let aws_retry_operation_worst_case =
            bounded_sdk_operation_worst_case * AWS_RETRY_POLICY_MAX_ATTEMPTS + aws_outer_backoff;

        let aws_flow_log_worst_case = bounded_sdk_operation_worst_case
            * (AWS_FLOW_LOG_SEND_ATTEMPTS + 1)
            + AWS_FLOW_LOG_RETRY_INTERVAL * (AWS_FLOW_LOG_SEND_ATTEMPTS - 1);
        assert_eq!(
            aws_flow_log_worst_case,
            std::time::Duration::from_millis(1_265_300)
        );

        let aws_acm_dns_validation_worst_case = bounded_sdk_operation_worst_case
            * AWS_ACM_REQUEST_CERTIFICATE_SENDS
            + bounded_sdk_operation_worst_case * AWS_ACM_DVO_WAIT_ATTEMPTS
            + AWS_ACM_DVO_WAIT_INTERVAL * (AWS_ACM_DVO_WAIT_ATTEMPTS - 1);

        // carina-provider-awscc/src/provider/cloudcontrol.rs `finish_issue`
        // and `poll_phase` on origin/main return retries as `poll_after_secs`
        // on a pending step, so one AWSCC guest call is one SDK operation.
        let awscc_guest_call_worst_case = bounded_sdk_operation_worst_case;

        // A host patch release must still run provider releases predating the
        // bounded SDK configuration. Their standard SDK policy used three
        // attempts with 1s + 2s backoff, while wasi:http could consume the
        // host cap once for connect and once for first byte per attempt.
        const LEGACY_AWS_SDK_MAX_ATTEMPTS: u32 = 3;
        const LEGACY_AWS_SDK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
        const LEGACY_AWS_SDK_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
        let legacy_sdk_backoff = retry_backoff_total(
            LEGACY_AWS_SDK_MAX_ATTEMPTS,
            LEGACY_AWS_SDK_INITIAL_DELAY,
            LEGACY_AWS_SDK_MAX_DELAY,
        );
        assert_eq!(legacy_sdk_backoff, std::time::Duration::from_secs(3));
        let legacy_attempt_worst_case = HTTP_API_REQUEST_TIMEOUT + HTTP_API_REQUEST_TIMEOUT;
        let legacy_sdk_operation_worst_case =
            legacy_attempt_worst_case * LEGACY_AWS_SDK_MAX_ATTEMPTS + legacy_sdk_backoff;
        assert_eq!(
            legacy_sdk_operation_worst_case,
            std::time::Duration::from_secs(183)
        );
        let legacy_aws_flow_log_worst_case = legacy_sdk_operation_worst_case
            * (AWS_FLOW_LOG_SEND_ATTEMPTS + 1)
            + AWS_FLOW_LOG_RETRY_INTERVAL * (AWS_FLOW_LOG_SEND_ATTEMPTS - 1);
        assert_eq!(
            legacy_aws_flow_log_worst_case,
            std::time::Duration::from_secs(2_434)
        );

        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_retry_operation_worst_case,
            "the production wall budget must exceed retry_aws_operation's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_flow_log_worst_case,
            "the production wall budget must exceed the VPC flow-log loop's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > aws_acm_dns_validation_worst_case,
            "the production wall budget must exceed the ACM DNS-validation wait's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > awscc_guest_call_worst_case,
            "the production wall budget must exceed one AWSCC guest call's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > legacy_aws_flow_log_worst_case,
            "the production wall budget must exceed the legacy VPC flow-log loop's worst case"
        );
        assert!(
            GuestCallBudget::PRODUCTION.wall() > GuestCallBudget::PRODUCTION.compute(),
            "the wall budget must outlast the compute budget so compute traps win"
        );
    }
    struct ReviewErrorFactory(std::sync::Mutex<Option<ProviderError>>);
    impl carina_core::provider::ProviderFactory for ReviewErrorFactory {
        fn name(&self) -> &str {
            "review"
        }
        fn display_name(&self) -> &str {
            "Review"
        }
        fn validate_config(
            &self,
            _: &carina_core::provider::CanonicalProviderConfigAttributes,
            _session: &carina_core::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Err(self.0.lock().unwrap().take().unwrap())
        }
        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            carina_core::schema::ValidatedConfigSchema::validate(std::collections::BTreeMap::from(
                [(
                    "password".into(),
                    carina_core::schema::PatternAttributeType::string(),
                )],
            ))
            .unwrap()
        }
        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            Vec::new()
        }
        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                Vec::new(),
                self.provider_config_attribute_types(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }
        fn create_provider(
            &self,
            _: &carina_core::provider::ProviderSession,
            _: carina_core::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
            unreachable!()
        }
    }

    fn review_bind_rebuild(error: ProviderError) -> carina_core::provider::HostError {
        let factory = ReviewErrorFactory(std::sync::Mutex::new(Some(error)));
        let attributes = IndexMap::from([(
            "password".into(),
            carina_core::resource::Value::Deferred(carina_core::resource::DeferredValue::Secret(
                Box::new(carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("guest-private-review".into()),
                ))
                .into(),
            )),
        )]);
        carina_core::executor::provider_ready::validate_and_canonicalize_provider_config(
            &factory,
            "review",
            &attributes,
            &factory.provider_config_attribute_types(),
        )
        .unwrap_err()
        .provider_error()
        .unwrap()
        .clone()
    }

    #[test]
    fn review_a2_guest_json_cause_is_bound_through_rebuild_and_config_validation() {
        let source = serde_json::from_str::<bool>(r#""guest-private-review""#).unwrap_err();
        let error = review_bind_rebuild(rebuild_provider_error(
            PoisonReason::Cancelled,
            WasmProviderLoadError::metadata("guest initialization", source),
        ));
        let text = error.to_string();
        assert!(
            !text.contains("guest-private-review"),
            "guest-derived cause leaked: {text}"
        );
        assert!(text.contains("(secret)"));
    }

    #[test]
    fn review_r1_rebuild_preserves_nonsecret_guest_initialization_reason() {
        let error = review_bind_rebuild(rebuild_provider_error(
            PoisonReason::Cancelled,
            WasmProviderLoadError::metadata(
                "guest initialization",
                ProviderError::invalid_input("invalid region").into_host_for_test(),
            ),
        ));
        let text = error.to_string();
        assert!(
            text.contains("invalid region"),
            "guest initialization reason was lost: {text}"
        );
        assert!(!text.contains("<provider text>"));
    }

    #[test]
    fn rebuild_metadata_source_reaches_the_wall_budget_error() {
        let error = rebuild_provider_error(
            PoisonReason::Cancelled,
            wall_budget_load_error(
                "instantiate",
                WallBudgetExceeded {
                    wall: std::time::Duration::from_secs(30),
                },
            ),
        );
        assert!(
            error
                .detail()
                .cause()
                .unwrap()
                .source_is::<WasmWallBudgetError>()
        );
        let host = error.into_host_for_test();
        assert!(
            host.detail()
                .cause()
                .unwrap()
                .source_is::<WasmWallBudgetError>()
        );
    }

    #[test]
    fn standalone_metadata_display_retains_its_cause() {
        let error = WasmProviderLoadError::metadata(
            "metadata failed",
            std::io::Error::other("useful reason"),
        );
        assert_eq!(error.to_string(), "metadata failed: useful reason");
    }

    #[test]
    fn load_metadata_renders_each_source_once_standalone_and_captured() {
        let error = WasmProviderLoadError::metadata(
            "metadata failed",
            std::io::Error::other("useful metadata reason"),
        );
        // This is the standalone text on main, including the metadata context.
        assert_eq!(error.to_string(), "metadata failed: useful metadata reason");
        assert!(std::error::Error::source(&error).is_some());
        let error = ProviderError::internal_caused_by(
            "Failed to create WASM provider validation instance",
            error,
        );
        assert!(
            error
                .detail()
                .cause()
                .unwrap()
                .source_is::<std::io::Error>()
        );
        let host = error.into_host_for_test();
        let wire = host.to_wire();
        assert_eq!(
            wire.cause.as_deref(),
            Some("metadata failed: useful metadata reason"),
        );
        assert_eq!(
            host.to_string().matches("useful metadata reason").count(),
            1
        );
        assert!(host.detail().cause().unwrap().source_is::<std::io::Error>());
    }

    #[test]
    fn load_egress_lock_renders_each_source_once_standalone_and_captured() {
        let error = WasmProviderLoadError::EgressLock(
            carina_provider_resolver::LockedEgressLoadError::LockFile(
                carina_provider_resolver::LockFileError::Read {
                    path: "carina-providers.lock".into(),
                    source: std::io::Error::other("lock read reason"),
                },
            ),
        );
        let expected = "Failed to read carina-providers.lock: lock read reason";
        assert_eq!(error.to_string(), expected);
        assert!(
            std::error::Error::source(&error).is_none(),
            "resolver errors render their sources inline"
        );
        let host = ProviderError::internal_caused_by("load failed", error).into_host_for_test();
        assert_eq!(host.to_wire().cause.as_deref(), Some(expected));
        assert_eq!(host.to_string().matches("lock read reason").count(), 1);
    }

    #[test]
    fn load_egress_consent_renders_once_standalone_and_captured() {
        let error = WasmProviderLoadError::EgressConsent(
            carina_provider_resolver::EgressConsentError::SourceMismatch {
                locked: "locked-source".into(),
                loaded: "loaded-source".into(),
            },
        );
        let expected = "locked egress source locked-source does not match loaded provider source loaded-source";
        assert_eq!(error.to_string(), expected);
        assert!(std::error::Error::source(&error).is_none());
        let host = ProviderError::internal_caused_by("load failed", error).into_host_for_test();
        assert_eq!(host.to_wire().cause.as_deref(), Some(expected));
        assert_eq!(host.to_string().matches("locked egress source").count(), 1);
    }

    #[test]
    fn rebuild_metadata_keeps_the_complete_opaque_diagnostic() {
        let message = format!("{}metadata-tail", "x".repeat(16_384));
        let error = rebuild_provider_error(
            PoisonReason::Cancelled,
            WasmProviderLoadError::metadata(
                "metadata failed",
                std::io::Error::other(message.clone()),
            ),
        )
        .into_host_for_test();
        let wire = error.to_wire();
        let cause = wire.cause.unwrap();
        assert!(
            cause.contains(&message),
            "full opaque metadata must survive before diagnostic truncation"
        );
        assert_eq!(cause.matches("metadata-tail").count(), 1);
        assert!(
            error
                .detail()
                .cause()
                .unwrap()
                .source_is::<std::io::Error>()
        );
    }

    #[test]
    fn rebuild_display_hides_guest_roots_and_binding_keeps_the_poison_inventory() {
        let secrets = crate::provider_text_fixtures::context(&["poison-private"]);
        let poison = PoisonReason::from_trap(
            "validate_config",
            &wasmtime::Error::msg("poison-private useful detail"),
            secrets.session().redaction_set(),
        );
        let rebuilt = WasmProviderRebuildError {
            previous_poison: poison.clone(),
            load_error: WasmProviderLoadError::Other("replacement failed".into()),
        };
        assert!(!rebuilt.to_string().contains("poison-private"));
        let error = rebuild_provider_error(poison, rebuilt.load_error).into_host_for_test();
        let wire = error.to_wire();
        let rendered = format!("{error} {error:?} {wire:?}");
        assert!(!rendered.contains("poison-private"), "{rendered}");
        assert!(wire.cause.unwrap().contains("(secret) useful detail"));
    }
}
