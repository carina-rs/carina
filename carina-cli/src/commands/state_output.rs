use std::collections::{BTreeMap, BTreeSet};

use carina_core::provider::{NestedWriteOnlyRecord, PersistedAttributes};
use carina_core::resource::{ConcreteValue, DeferredValue, Value};
use carina_core::schema::{LeafPath, LeafPathSegment};
use carina_state::{CurrentInstance, ResourceState, StateFile};
use serde::Serialize;

const STATE_SHOW_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub(crate) struct StateShowDocument {
    pub(crate) schema_version: u32,
    pub(crate) state: StatePresence,
    pub(crate) resources: Vec<StateShowResource>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatePresence {
    Present,
    Absent,
}

#[derive(Debug, Serialize)]
pub(crate) struct StateShowResource {
    pub(crate) binding: Option<String>,
    pub(crate) identity: String,
    pub(crate) provider: String,
    pub(crate) resource_type: String,
    pub(crate) protected: bool,
    pub(crate) current: StateShowCurrent,
    pub(crate) deposed: Vec<StateShowDeposed>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StateShowCurrent {
    pub(crate) kind: StateShowCurrentKind,
    pub(crate) identifier: Option<String>,
    pub(crate) attributes: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateShowCurrentKind {
    Applied,
    None,
    CreateInFlight,
    CreateOutcomeUnknown,
    MissingIdentifier,
}

#[derive(Debug, Serialize)]
pub(crate) struct StateShowDeposed {
    pub(crate) identifier: String,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "attribute `{attribute}` on resource `{resource}` cannot be represented in state JSON output: {reason}"
)]
pub(crate) struct StateShowOutputError {
    resource: String,
    attribute: String,
    reason: &'static str,
}

impl StateShowDocument {
    pub(crate) fn from_state(state: &StateFile) -> Result<Self, StateShowOutputError> {
        let resources = state
            .resources()
            .iter()
            .map(StateShowResource::from_row)
            .collect::<Result<Vec<_>, StateShowOutputError>>()?;

        Ok(Self {
            schema_version: STATE_SHOW_SCHEMA_VERSION,
            state: StatePresence::Present,
            resources,
        })
    }

    pub(crate) fn absent() -> Self {
        Self {
            schema_version: STATE_SHOW_SCHEMA_VERSION,
            state: StatePresence::Absent,
            resources: Vec::new(),
        }
    }
}

impl StateShowResource {
    pub(crate) fn from_row(row: &ResourceState) -> Result<Self, StateShowOutputError> {
        let resource = row.binding.as_deref().unwrap_or(row.identity.as_str());
        let current = match row.current() {
            CurrentInstance::None => StateShowCurrent {
                kind: StateShowCurrentKind::None,
                identifier: None,
                attributes: BTreeMap::new(),
            },
            CurrentInstance::Applied {
                identifier,
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read: _partial_read,
            } => StateShowCurrent {
                kind: StateShowCurrentKind::Applied,
                identifier: Some(identifier.as_str().to_string()),
                attributes: public_attributes(
                    resource,
                    attributes,
                    write_only_attributes,
                    nested_write_only,
                )?,
            },
            CurrentInstance::CreateInFlight { pending } => StateShowCurrent {
                kind: StateShowCurrentKind::CreateInFlight,
                identifier: pending
                    .identifier()
                    .map(|identifier| identifier.as_str().to_string()),
                attributes: BTreeMap::new(),
            },
            CurrentInstance::CreateOutcomeUnknown { unknown: _unknown } => StateShowCurrent {
                kind: StateShowCurrentKind::CreateOutcomeUnknown,
                identifier: None,
                attributes: BTreeMap::new(),
            },
            CurrentInstance::MissingIdentifier {
                attributes,
                write_only_attributes,
                nested_write_only,
                partial_read: _partial_read,
            } => StateShowCurrent {
                kind: StateShowCurrentKind::MissingIdentifier,
                identifier: None,
                attributes: public_attributes(
                    resource,
                    attributes,
                    write_only_attributes,
                    nested_write_only,
                )?,
            },
        };

        Ok(Self {
            binding: row.binding.clone(),
            identity: row.identity.as_str().to_string(),
            provider: row.provider.clone(),
            resource_type: row.resource_type.clone(),
            protected: row.protected,
            current,
            deposed: row
                .deposed
                .iter()
                .map(|generation| StateShowDeposed {
                    identifier: generation.identifier.clone(),
                })
                .collect(),
        })
    }
}

fn public_attributes(
    resource: &str,
    attributes: &PersistedAttributes,
    write_only_attributes: &[String],
    nested_write_only: &NestedWriteOnlyRecord,
) -> Result<BTreeMap<String, serde_json::Value>, StateShowOutputError> {
    let write_only: BTreeSet<&str> = write_only_attributes.iter().map(String::as_str).collect();
    attributes
        .keys()
        .map(|attribute| {
            let mut value = if write_only.contains(attribute.as_str()) {
                redacted_marker()
            } else if let Some(value) = attributes.get(attribute) {
                public_json_value(&value).map_err(|reason| StateShowOutputError {
                    resource: resource.to_string(),
                    attribute: attribute.clone(),
                    reason,
                })?
            } else {
                attributes
                    .decoded_json_value(attribute)
                    .expect("an iterated persisted attribute must still exist")
            };
            if let Some(leaves) = nested_write_only.get(attribute) {
                for (path, _accepted_value) in leaves {
                    redact_nested_write_only_leaf(&mut value, path);
                }
            }
            Ok((attribute.clone(), value))
        })
        .collect()
}

fn public_json_value(value: &Value) -> Result<serde_json::Value, &'static str> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => {
            Err("the value contains an unresolved module-template conditional")
        }
        Value::Concrete(ConcreteValue::String(value)) => Ok(serde_json::Value::String(
            value.expose_plaintext().to_string(),
        )),
        Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            Ok(serde_json::Value::String(value.to_string()))
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
            Ok(serde_json::Value::String(value.api_value().to_string()))
        }
        Value::Concrete(ConcreteValue::Int(value)) => {
            Ok(serde_json::Value::Number((*value).into()))
        }
        Value::Concrete(ConcreteValue::Float(value)) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or("the value is a non-finite float"),
        Value::Concrete(ConcreteValue::Bool(value)) => Ok(serde_json::Value::Bool(*value)),
        Value::Concrete(ConcreteValue::Duration(value)) => Ok(serde_json::Value::Number(
            serde_json::Number::from(value.as_secs()),
        )),
        Value::Concrete(ConcreteValue::List(values)) => values
            .iter()
            .map(public_json_value)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        Value::Concrete(ConcreteValue::StringList(values)) => Ok(serde_json::Value::Array(
            values
                .iter()
                .map(|value| serde_json::Value::String(value.expose_plaintext().to_string()))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(values)) => {
            let sorted = values
                .iter()
                .map(|(key, value)| Ok((key.clone(), public_json_value(value)?)))
                .collect::<Result<BTreeMap<_, _>, &'static str>>()?;
            Ok(serde_json::Value::Object(sorted.into_iter().collect()))
        }
        Value::Deferred(DeferredValue::Secret(_))
        | Value::Deferred(DeferredValue::RedactedSecret { hash: _ }) => Ok(redacted_marker()),
        Value::Deferred(DeferredValue::ResourceRef { path: _path }) => {
            Err("the value contains an unresolved resource reference")
        }
        Value::Deferred(DeferredValue::BindingRef { binding: _binding }) => {
            Err("the value contains an unresolved binding reference")
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: _value,
            default: _default,
        }) => Err("the value contains an unresolved coalesce expression"),
        Value::Deferred(DeferredValue::IntegerArithmetic(_expression)) => {
            Err("the value contains unresolved integer arithmetic")
        }
        Value::Deferred(DeferredValue::Interpolation(_parts)) => {
            Err("the value contains an unresolved interpolation")
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name: _name,
            args: _args,
            source_line: _source_line,
        }) => Err("the value contains an unresolved function call"),
        Value::Deferred(DeferredValue::Unavailable {
            name: _name,
            args: _args,
            reason: _reason,
            source_line: _source_line,
        }) => Err("the value contains an unavailable function call"),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: _function,
            functions: _functions,
            args: _args,
            source_line: _source_line,
        }) => Err("the value contains an unresolved user function call"),
        Value::Deferred(DeferredValue::Access(_access)) => {
            Err("the value contains an unresolved access expression")
        }
        Value::Deferred(DeferredValue::Unknown(_reason)) => {
            Err("the value contains an unknown value")
        }
    }
}

fn redacted_marker() -> serde_json::Value {
    serde_json::json!({ "redacted": true })
}

fn redact_nested_write_only_leaf(value: &mut serde_json::Value, path: &LeafPath) {
    let mut current = value;
    for segment in path.segments() {
        current = match segment {
            LeafPathSegment::StructField(field) | LeafPathSegment::MapKey(field) => {
                let Some(value) = current
                    .as_object_mut()
                    .and_then(|object| object.get_mut(field))
                else {
                    return;
                };
                value
            }
            LeafPathSegment::ListIndex(index) => {
                let Some(value) = current
                    .as_array_mut()
                    .and_then(|values| values.get_mut(*index))
                else {
                    return;
                };
                value
            }
        };
    }
    *current = redacted_marker();
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use carina_core::provider::{
        CreateAttempt, CreateFailureKind, EgressSent, ProviderError, Sent,
    };
    use carina_core::resource::{ResolvedResource, Resource, ResourceId};
    use carina_core::schema::{LeafPath, LeafPathSegment};
    use carina_state::{DeposedInstance, DeposedKey, ResourceState, StateFile};
    use serde_json::{Value, json};

    use super::*;

    fn deposed_key(raw: &str) -> DeposedKey {
        serde_json::from_value(json!(raw)).expect("test deposed key must be valid")
    }

    fn with_nested_write_only(row: ResourceState) -> ResourceState {
        let mut encoded = serde_json::to_value(row).expect("fixture row must serialize");
        encoded["current"]["nested_write_only"] = json!({
            "oidc_config": [[
                [{"kind": "struct_field", "value": "client_secret"}],
                "accepted-nested-plaintext"
            ]]
        });
        let state = json!({
            "version": StateFile::CURRENT_VERSION,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 0,
            "lineage": "state-output-fixture",
            "carina_version": "test",
            "resources": [encoded],
            "exports": {}
        });
        carina_state::check_and_migrate(&state.to_string())
            .expect("fixture row with nested write-only provenance must load")
            .state()
            .resources()[0]
            .clone()
    }

    fn representative_state() -> StateFile {
        let digest = format!("argon2-legacy:73616c74:{}", "ab".repeat(32));
        let mut applied = ResourceState::try_applied(
            "test.resource",
            "bound-resource",
            "mock",
            "mock-applied-id",
        )
        .expect("fixture identifier must be non-empty")
        .with_attribute("name", json!("bound"))
        .with_attribute("count", json!(3))
        .with_attribute("enabled", json!(true))
        .with_attribute("items", json!(["a", 2, false]))
        .with_attribute("metadata", json!({"b": 2, "a": 1}))
        .with_attribute(
            "oidc_config",
            json!({
                "issuer": "https://issuer.example.com",
                "client_secret": "accepted-nested-plaintext"
            }),
        )
        .with_attribute("secret", json!({"$secret": digest}))
        .with_attribute("write_only", json!("accepted-plaintext"))
        .with_protected(true);
        applied.binding = Some("bound".to_string());
        applied.deposed.push(DeposedInstance::from_persisted_json(
            deposed_key("old-generation"),
            "mock-deposed-id".to_string(),
            Some("secondary".to_string()),
            HashMap::from([("name".to_string(), json!("old"))]),
            BTreeSet::from(["dependency".to_string()]),
        ));
        let applied = crate::fixture_resource_state_metadata(
            applied,
            Some(vec!["write_only".to_string()]),
            None,
        );
        let applied = with_nested_write_only(applied);

        let pending_id =
            ResourceId::with_provider_identity("mock", "test.resource", "pending-resource", None);
        let pending_resource = ResolvedResource::new(Resource::from_id(pending_id.clone()));
        let pending = crate::fixture_in_flight_create(
            &pending_id,
            "resume-token",
            Some("mock-pending-id"),
            Sent::AllRequestAttributes,
        );
        let mut pending = ResourceState::from_in_flight(
            &pending_resource,
            &pending,
            None,
            None,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        )
        .expect("in-flight fixture must be encodable");
        pending.binding = Some("pending".to_string());

        let unknown_id =
            ResourceId::with_provider_identity("mock", "test.resource", "unknown-resource", None);
        let unknown_resource = ResolvedResource::new(Resource::from_id(unknown_id.clone()));
        let failure = CreateAttempt::for_test(unknown_id).outcome_unknown(
            EgressSent::for_test(),
            ProviderError::api_error("guest trapped"),
        );
        let CreateFailureKind::OutcomeUnknown { unknown, .. } = failure.into_kind() else {
            panic!("the egress witness must mint an unknown outcome")
        };
        let mut unknown = ResourceState::from_unknown_create_outcome(
            &unknown_resource,
            &unknown,
            None,
            None,
            &carina_core::differ::exact_comparison_context_for_tests(),
        );
        unknown.binding = Some("unknown".to_string());

        let missing = ResourceState::new("test.resource", "anonymous-resource", "mock")
            .with_attribute("reason", json!("legacy"));
        let none = ResourceState::new("test.resource", "empty-resource", "mock");

        let mut state = StateFile::new();
        for row in [applied, pending, unknown, missing, none] {
            state
                .upsert_resource(row)
                .expect("representative row must be valid");
        }
        state
    }

    #[test]
    fn state_show_document_snapshot_pins_public_schema() {
        let document = StateShowDocument::from_state(&representative_state())
            .expect("representative state must convert");

        insta::assert_snapshot!(
            serde_json::to_string_pretty(&document).expect("document must serialize")
        );
    }

    #[test]
    fn absent_document_has_stable_compact_shape() {
        let encoded = serde_json::to_string(&StateShowDocument::absent())
            .expect("absent document must serialize");

        assert_eq!(
            encoded,
            r#"{"schema_version":1,"state":"absent","resources":[]}"#
        );
    }

    #[test]
    fn public_output_redacts_secrets_and_write_only_values_without_reserved_keys() {
        let document = StateShowDocument::from_state(&representative_state())
            .expect("representative state must convert");
        let encoded = serde_json::to_value(document).expect("document must serialize");

        assert_eq!(
            encoded.pointer("/resources/0/current/attributes/secret"),
            Some(&json!({"redacted": true}))
        );
        assert_eq!(
            encoded.pointer("/resources/0/current/attributes/write_only"),
            Some(&json!({"redacted": true}))
        );
        assert_eq!(
            encoded.pointer("/resources/0/current/attributes/oidc_config/client_secret"),
            Some(&json!({"redacted": true}))
        );
        assert_eq!(
            encoded.pointer("/resources/0/current/attributes/oidc_config/issuer"),
            Some(&json!("https://issuer.example.com"))
        );
        assert_no_reserved_keys(&encoded);
        assert!(!encoded.to_string().contains("accepted-plaintext"));
        assert!(!encoded.to_string().contains("accepted-nested-plaintext"));
        assert!(!encoded.to_string().contains("argon2-legacy"));
    }

    #[test]
    fn nested_write_only_redaction_preserves_public_siblings() {
        let mut value = json!({
            "issuer": "https://issuer.example.com",
            "clients": [{"name": "primary", "client_secret": "plaintext"}]
        });
        let path = LeafPath::from_segments([
            LeafPathSegment::StructField("clients".to_string()),
            LeafPathSegment::ListIndex(0),
            LeafPathSegment::StructField("client_secret".to_string()),
        ]);

        redact_nested_write_only_leaf(&mut value, &path);

        assert_eq!(value["issuer"], "https://issuer.example.com");
        assert_eq!(
            value.pointer("/clients/0/client_secret"),
            Some(&json!({"redacted": true}))
        );
        assert!(!value.to_string().contains("plaintext"));
    }

    fn assert_no_reserved_keys(value: &Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    assert!(!key.starts_with('$'), "reserved key leaked: {key}");
                    assert_no_reserved_keys(value);
                }
            }
            Value::Array(values) => values.iter().for_each(assert_no_reserved_keys),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}
