//! The normalization-only WIT bridge. Ordinary value codecs remain separate.

use crate::wasm_factory::version_checked_bindings::{WitDesiredResource, wit_types as wit};
use carina_core::desired::NormalizationBatch;
use carina_core::resource::{AbsoluteBindingAddress, NoPlaceholder, Resource};
use carina_provider_protocol::normalization::{
    NormalizationError, NormalizationErrorKind as Kind, NormalizationResource,
    NormalizationValue as Wire,
};
use std::collections::BTreeMap;

fn id_to_wit(id: &carina_provider_protocol::types::ResourceId) -> wit::ResourceId {
    wit::ResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        identity: id.identity.clone(),
    }
}

pub(crate) fn encode(
    batch: &NormalizationBatch,
) -> Result<Vec<WitDesiredResource>, NormalizationError> {
    batch
        .resources()
        .iter()
        .enumerate()
        .map(|(index, resource)| {
            let attributes = resource
                .attributes
                .iter()
                .map(|(name, value)| {
                    to_wit(value, &resource.id, name)
                        .map(|value| (name.clone(), value))
                        .map_err(|error| error.with_resource_index(index))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(WitDesiredResource {
                id: id_to_wit(&resource.id),
                attributes,
            })
        })
        .collect()
}

fn to_wit(
    value: &Wire,
    id: &carina_provider_protocol::types::ResourceId,
    attribute: &str,
) -> Result<wit::DesiredValue, NormalizationError> {
    Ok(match value {
        Wire::Bool(value) => wit::DesiredValue::BoolVal(*value),
        Wire::Int(value) => wit::DesiredValue::IntVal(*value),
        Wire::Float(value) => wit::DesiredValue::FloatVal(*value),
        Wire::String(value) => wit::DesiredValue::StrVal(value.clone()),
        Wire::Deferred(slot) => wit::DesiredValue::DeferredVal(*slot),
        Wire::StringList(_) => wit::DesiredValue::StringListVal(value.to_json(id, attribute)?),
        Wire::List(_) => wit::DesiredValue::ListVal(value.to_json(id, attribute)?),
        Wire::Map(_) => wit::DesiredValue::MapVal(value.to_json(id, attribute)?),
    })
}

fn from_wit(
    value: &wit::DesiredValue,
    id: &carina_provider_protocol::types::ResourceId,
    attribute: &str,
) -> Result<Wire, NormalizationError> {
    Ok(match value {
        wit::DesiredValue::BoolVal(value) => Wire::Bool(*value),
        wit::DesiredValue::IntVal(value) => Wire::Int(*value),
        wit::DesiredValue::FloatVal(value) => Wire::Float(*value),
        wit::DesiredValue::StrVal(value) => Wire::String(value.clone()),
        wit::DesiredValue::DeferredVal(slot) => Wire::Deferred(*slot),
        wit::DesiredValue::ListVal(json) => Wire::from_json_list(json, id, attribute)?,
        wit::DesiredValue::MapVal(json) => Wire::from_json_map(json, id, attribute)?,
        wit::DesiredValue::StringListVal(json) => Wire::from_json_string_list(json, id, attribute)?,
    })
}

pub(crate) fn apply<S, D>(
    resources: &mut [Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>],
    batch: NormalizationBatch,
    returned: Vec<WitDesiredResource>,
) -> Result<(), NormalizationError> {
    let updates = batch.finish_with(returned, |sent, resource| {
        let actual = carina_provider_protocol::types::ResourceId {
            provider: resource.id.provider,
            resource_type: resource.id.resource_type,
            identity: resource.id.identity,
        };
        let mut attributes = BTreeMap::new();
        for (name, value) in resource.attributes {
            if attributes.contains_key(&name) {
                return Err(NormalizationError::new(
                    sent.id.clone(),
                    &name,
                    Kind::DuplicateKey { key: name.clone() },
                ));
            }
            attributes.insert(name.clone(), from_wit(&value, &sent.id, &name)?);
        }
        Ok(NormalizationResource {
            id: actual,
            attributes,
        })
    })?;
    for (resource, attributes) in resources.iter_mut().zip(updates) {
        resource.attributes = attributes;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    use carina_plugin_sdk::{DesiredInputs, DesiredValue};

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }
    fn fixture() -> Vec<Resource> {
        let nested = Value::Deferred(DeferredValue::Secret(Box::new(string("private-nested"))));
        vec![
            Resource::new("test", "example")
                .with_attribute(
                    "top",
                    Value::Deferred(DeferredValue::Secret(Box::new(string("private-top")))),
                )
                .with_attribute(
                    "rules",
                    Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::Map(
                            [
                                ("$carina_deferred".into(), string("literal")),
                                ("secret".into(), nested),
                            ]
                            .into_iter()
                            .collect(),
                        )),
                        string("known"),
                    ])),
                ),
        ]
    }

    #[test]
    fn native_and_wasm_normalization_preserve_both_dependency_phases() {
        fn normalize<D>(
            resource: &mut Resource<
                carina_core::resource::ModuleSource,
                NoPlaceholder,
                AbsoluteBindingAddress,
                D,
            >,
        ) {
            let original_id = resource.id.clone();
            let original_binding = resource.binding.clone();
            let original_secret = resource.attributes["secret"].clone();
            let batch = NormalizationBatch::new(std::slice::from_ref(resource)).unwrap();
            let mut returned = encode(&batch).unwrap();
            returned[0]
                .attributes
                .iter_mut()
                .find(|(name, _)| name == "known")
                .unwrap()
                .1 = wit::DesiredValue::StrVal("wasm".into());
            apply(std::slice::from_mut(resource), batch, returned).unwrap();
            assert_eq!(resource.attributes["known"], string("wasm"));
            assert!(matches!(
                DesiredInputs::from_core(resource)
                    .unwrap()
                    .attribute("secret"),
                Some(DesiredValue::Deferred(_)),
            ));

            let mut native =
                carina_plugin_sdk::NativeDesiredNormalization::from_core(resource).unwrap();
            {
                let mut view = native.view();
                let Some(carina_plugin_sdk::DesiredValueMut::Known(value)) =
                    view.attribute_mut("known")
                else {
                    panic!("known sibling stays editable beside the opaque secret");
                };
                *value = carina_plugin_sdk::DesiredScalar::String("native".into());
            }
            native.finish().unwrap();
            assert_eq!(resource.attributes["known"], string("native"));
            assert_eq!(resource.attributes["secret"], original_secret);
            assert_eq!(resource.id, original_id);
            assert_eq!(resource.binding, original_binding);
        }

        let binding = |name| carina_core::resource::absolute_binding_address_for_test(name);
        let mut resource =
            Resource::with_provider("test", "test.Widget", "example", Some("named".into()))
                .with_absolute_binding(binding("example"))
                .with_absolute_dependency_bindings([binding("upstream")].into_iter().collect())
                .with_attribute("known", string("before"))
                .with_attribute(
                    "secret",
                    Value::Deferred(DeferredValue::Secret(Box::new(string("private")))),
                );
        let dependencies = resource.dependencies.clone();
        normalize(&mut resource);
        assert_eq!(resource.dependencies, dependencies);
        normalize(&mut resource.into_value_snapshot());
    }

    #[test]
    fn host_projection_and_sdk_view_agree_on_leaf_classification() {
        let mut resources = fixture();
        let batch = NormalizationBatch::new(&resources).unwrap();
        let wire = encode(&batch).unwrap();
        assert!(matches!(
            wire[0]
                .attributes
                .iter()
                .find(|(name, _)| name == "top")
                .unwrap()
                .1,
            wit::DesiredValue::DeferredVal(0)
        ));
        let text = format!("{wire:?}");
        assert!(!text.contains("private-top") && !text.contains("private-nested"));
        assert!(text.contains("$$carina_deferred"));
        let inputs = DesiredInputs::from_core(&resources[0]).unwrap();
        assert!(matches!(
            inputs.attribute("top"),
            Some(DesiredValue::Deferred(_))
        ));
        assert!(matches!(
            inputs.attribute("rules"),
            Some(DesiredValue::List(_))
        ));
        let (_, encoded) = wire[0]
            .attributes
            .iter()
            .find(|(name, _)| name == "rules")
            .unwrap();
        let Wire::List(encoded) = from_wit(encoded, &batch.resources()[0].id, "rules").unwrap()
        else {
            panic!()
        };
        let Wire::Map(encoded) = &encoded[0] else {
            panic!()
        };
        assert!(matches!(encoded["secret"], Wire::Deferred(1)));
        let Some(DesiredValue::List(native)) = inputs.attribute("rules") else {
            panic!()
        };
        let Some(DesiredValue::Map(native)) = native.iter().next() else {
            panic!()
        };
        assert!(matches!(
            native.get("secret"),
            Some(DesiredValue::Deferred(_))
        ));
        apply(&mut resources, batch, wire).unwrap();
        assert!(matches!(
            resources[0].attributes["top"],
            Value::Deferred(DeferredValue::Secret(_))
        ));
    }

    #[test]
    fn swapped_resource_ids_without_handles_are_rejected_atomically() {
        let mut resources = vec![
            Resource::new("test", "first").with_attribute("known", string("first-value")),
            Resource::new("test", "second").with_attribute("known", string("second-value")),
        ];
        let original = resources.clone();
        let batch = NormalizationBatch::new(&resources).unwrap();
        let mut returned = encode(&batch).unwrap();
        returned.swap(0, 1);
        let error = apply(&mut resources, batch, returned).unwrap_err();
        assert!(
            matches!(error.kind, Kind::ResourceIdMismatch { ref expected, ref actual }
            if **expected == carina_core::protocol_convert::resource_id_to_protocol(&original[0].id)
            && **actual == carina_core::protocol_convert::resource_id_to_protocol(&original[1].id))
        );
        assert_eq!(error.resource_index, Some(0));
        assert_eq!(
            *error.resource_id,
            carina_core::protocol_convert::resource_id_to_protocol(&original[0].id)
        );
        assert_eq!(resources[0].attributes, original[0].attributes);
        assert_eq!(resources[1].attributes, original[1].attributes);
    }

    #[test]
    fn malformed_response_count_handles_and_keys_never_publish_partial_updates() {
        for case in 0..8 {
            let mut resources = fixture();
            resources
                .push(Resource::new("test", "example").with_attribute("known", string("before")));
            let batch = NormalizationBatch::new(&resources).unwrap();
            let mut returned = encode(&batch).unwrap();
            returned[1]
                .attributes
                .push(("added".into(), wit::DesiredValue::BoolVal(true)));
            match case {
                0 => {
                    returned.pop();
                }
                1 => returned[0].attributes.retain(|(name, _)| name != "top"),
                2 => returned[0]
                    .attributes
                    .push(("other".into(), wit::DesiredValue::DeferredVal(0))),
                3 => {
                    returned[0]
                        .attributes
                        .iter_mut()
                        .find(|(name, _)| name == "top")
                        .unwrap()
                        .1 = wit::DesiredValue::DeferredVal(999)
                }
                4 => returned[0]
                    .attributes
                    .push(("rules".into(), wit::DesiredValue::MapVal("{}".into()))),
                5 => {
                    returned[0]
                        .attributes
                        .iter_mut()
                        .find(|(name, _)| name == "rules")
                        .unwrap()
                        .1 = wit::DesiredValue::ListVal(
                        r#"[{"$carina_deferred":1},{"$carina_deferred":1}]"#.into(),
                    )
                }
                6 => {
                    returned[0]
                        .attributes
                        .iter_mut()
                        .find(|(name, _)| name == "rules")
                        .unwrap()
                        .1 =
                        wit::DesiredValue::ListVal(r#"[{"$carina_deferred":"private-bad"}]"#.into())
                }
                7 => returned.push(returned[1].clone()),
                _ => unreachable!(),
            }
            let error = apply(&mut resources, batch, returned).unwrap_err();
            assert!(match case {
                0 | 7 => matches!(error.kind, Kind::ResourceCountMismatch { .. }),
                1 => matches!(error.kind, Kind::MissingHandle { .. }),
                2 => matches!(error.kind, Kind::ForeignHandle { .. }),
                3 => matches!(error.kind, Kind::UnknownHandle { .. }),
                4 => matches!(error.kind, Kind::DuplicateKey { .. }),
                5 => matches!(error.kind, Kind::DuplicateHandle { .. }),
                _ => matches!(error.kind, Kind::MalformedHandle),
            });
            assert!(!error.attribute.is_empty());
            assert!(error.to_string().contains("example"));
            assert!(!format!("{error:?}").contains("private-"));
            assert!(!resources[1].attributes.contains_key("added"));
        }
    }

    #[test]
    fn late_json_decode_error_keeps_source_and_original_attributes() {
        use std::error::Error;
        let mut resources = vec![
            Resource::new("test", "example").with_attribute("value", string("before")),
            Resource::new("test", "second").with_attribute("value", string("before")),
        ];
        let batch = NormalizationBatch::new(&resources).unwrap();
        let mut returned = encode(&batch).unwrap();
        returned[0].attributes[0].1 = wit::DesiredValue::StrVal("changed".into());
        returned[1].attributes[0].1 = wit::DesiredValue::MapVal("private-bad-json".into());
        let error = apply(&mut resources, batch, returned).unwrap_err();
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(error.to_string().contains("second") && error.to_string().contains("value"));
        assert_eq!(resources[0].attributes["value"], string("before"));
    }
}
