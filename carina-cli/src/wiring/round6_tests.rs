use super::*;
use carina_core::provider::*;
use carina_core::resource::{ConcreteValue, Current, DeferredValue, Resource, State, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SECRET: &str = "round6-named-desired-private";
struct NamedFactory(Arc<Mutex<Vec<Option<String>>>>);
struct NamedNormalizer;
impl ProviderFactory for NamedFactory {
    fn name(&self) -> &str {
        "round6"
    }
    fn display_name(&self) -> &str {
        "Round 6"
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            Vec::new(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        unreachable!()
    }
    fn create_normalizer(
        &self,
        session: &ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        self.0
            .lock()
            .unwrap()
            .push(session.binding().map(str::to_owned));
        Box::pin(async { Ok(Box::new(NamedNormalizer) as Box<dyn ProviderNormalizer>) })
    }
}
impl ProviderNormalizer for NamedNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _: &'a mut [carina_core::resource::ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }
    fn normalize_state<'a>(
        &'a self,
        states: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async move {
            if states.is_empty() {
                return Ok(());
            }
            Err(ProviderError::api_error(format!(
                "named state normalizer {SECRET}; public=visible"
            )))
        })
    }
    fn hydrate_read_state<'a>(
        &'a self,
        _: &'a mut StateTransformMap,
        _: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        ready_noop()
    }
}
fn named_state_normalizer(include_default: bool) {
    let bindings = Arc::new(Mutex::new(Vec::new()));
    let configs: Vec<carina_core::parser::ExpandedProviderConfig> =
        serde_json::from_value(serde_json::json!([
            {"name":"round6", "attributes":{}}, {"name":"round6", "attributes":{}, "binding":"east"}
        ]))
        .unwrap();
    let saved: carina_core::parser::SavedProviderConfigs =
        serde_json::from_value(serde_json::to_value(configs).unwrap()).unwrap();
    let saved = if include_default {
        saved
    } else {
        serde_json::from_value(
            serde_json::json!([{"name":"round6", "attributes":{}, "binding":"east"}]),
        )
        .unwrap()
    };
    let configs = carina_core::parser::ResolvedProviderConfigs::try_from(saved).unwrap();
    let ctx = WiringContext::new_for_test_with_provider_phase(
        &crate::test_support::TestCommandRuntime::new(),
        vec![Box::new(NamedFactory(bindings.clone()))],
        configs.as_provider_configs(),
    );
    let mut desired = Resource::with_provider("round6", "Resource", "bucket", None);
    desired.id.provider_instance = Some("east".into());
    desired.attributes.insert(
        "password".into(),
        Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(SECRET.into()))).into(),
        )),
    );
    let plan = crate::provider_text_fixtures::plan_for_desired(std::slice::from_ref(&desired));
    let state =
        State::<carina_core::resource::NoPlaceholder>::existing(desired.id.clone(), HashMap::new())
            .with_identifier("named-id");
    let mut states = HashMap::from([(desired.id.clone(), Current::from_state(state))]);
    let error = normalize_state_with_ctx(&ctx, &configs, &mut states, &plan)
        .expect_err("named instance must receive its state row");
    let text = error.to_string();
    assert!(
        !text.contains(SECRET),
        "named desired secret leaked: {text}"
    );
    assert!(
        text.contains("named state normalizer (secret); public=visible"),
        "{text}"
    );
    let expected = if include_default {
        vec![None, Some("east".into())]
    } else {
        vec![Some("east".into())]
    };
    assert_eq!(*bindings.lock().unwrap(), expected);
}

#[test]
fn review_round6_named_state_normalizer_uses_whole_plan_and_binding() {
    named_state_normalizer(true);
}
#[test]
fn review_round6_named_only_state_normalizer_uses_whole_plan_and_binding() {
    named_state_normalizer(false);
}
