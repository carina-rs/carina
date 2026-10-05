//! Opaque secret transport for guest calls that return desired resources.
//!
//! CRUD calls intentionally use the WIT `secret-val` representation because
//! providers need the plaintext. Desired-state round trips use this module
//! instead: secrets become per-call tokens, and the guest response remains an
//! opaque [`GuestDesired`] until the matching [`Unsealer`] restores known
//! tokens and isolates attributes containing mangled ones.

use std::collections::HashMap;
use std::future::Future;

use carina_core::resource::{ConcreteValue, DeferredValue, Resource, Value};
use carina_core::value::SerializationError;
use indexmap::IndexMap;

use crate::wasm_bindings::carina::provider::types as wit;
use crate::wasm_convert;

const TOKEN_NAMESPACE: &str = "carina-sealed-secret";

pub(crate) struct SealedDesired {
    resources: Vec<wit::ResourceDef>,
    default_tags: Vec<(String, wit::Value)>,
}

impl SealedDesired {
    pub(crate) async fn send<'a, E, F, Fut>(&'a self, call: F) -> Result<GuestDesired, E>
    where
        F: FnOnce(&'a [wit::ResourceDef], &'a [(String, wit::Value)]) -> Fut,
        Fut: Future<Output = Result<Vec<wit::ResourceDef>, E>>,
    {
        let resources = call(&self.resources, &self.default_tags).await?;
        Ok(GuestDesired::new(resources))
    }
}

// The field stays private so a desired-state caller cannot feed raw guest
// attributes to `wasm_convert`; only `Unsealer::restore` can consume them.
pub(crate) struct GuestDesired {
    resources: Vec<wit::ResourceDef>,
}

impl GuestDesired {
    fn new(resources: Vec<wit::ResourceDef>) -> Self {
        Self { resources }
    }
}

pub(crate) struct Unsealer {
    nonce_prefix: String,
    secrets: Vec<Value>,
}

pub(crate) struct RestoredDesired {
    resources: Vec<RestoredResource>,
}

struct RestoredResource {
    attributes: HashMap<String, Value>,
    skipped_attributes: Vec<String>,
}

impl RestoredDesired {
    pub(crate) fn apply_to(self, resources: &mut [Resource]) {
        for (resource, restored) in resources.iter_mut().zip(self.resources) {
            for key in restored.skipped_attributes {
                log::error!(
                    "Ignoring mangled sealed secret in resource '{}' attribute '{key}'",
                    resource.id
                );
            }
            for (key, value) in restored.attributes {
                resource.attributes.insert(key, value);
            }
        }
    }
}

pub(crate) fn seal(
    resources: &[Resource],
    default_tags: Option<&IndexMap<String, Value>>,
) -> Result<(SealedDesired, Unsealer), SerializationError> {
    let nonce_prefix = format!("{TOKEN_NAMESPACE}:{}:", uuid::Uuid::new_v4().simple());
    let mut sealer = Sealer {
        nonce_prefix: nonce_prefix.clone(),
        secrets: Vec::new(),
    };
    let resources = resources
        .iter()
        .map(|resource| {
            let mut sealed = resource.clone();
            for value in sealed.attributes.values_mut() {
                *value = sealer.seal_value(value);
            }
            wasm_convert::core_to_wit_resource(&sealed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let default_tags = default_tags
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let checkpoint = sealer.secrets.len();
            let sealed = sealer.seal_value(value);
            match wasm_convert::core_to_wit_value(&sealed) {
                Ok(value) => Some((key.clone(), value)),
                Err(error) => {
                    sealer.secrets.truncate(checkpoint);
                    log::error!("Skipping default_tag '{key}' with unresolvable value: {error}");
                    None
                }
            }
        })
        .collect();

    Ok((
        SealedDesired {
            resources,
            default_tags,
        },
        Unsealer {
            nonce_prefix,
            secrets: sealer.secrets,
        },
    ))
}

struct Sealer {
    nonce_prefix: String,
    secrets: Vec<Value>,
}

impl Sealer {
    fn seal_value(&mut self, value: &Value) -> Value {
        match value {
            Value::Deferred(DeferredValue::Secret(_)) => {
                let index = self.secrets.len();
                self.secrets.push(value.clone());
                Value::Concrete(ConcreteValue::String(format!(
                    "{}{index}",
                    self.nonce_prefix
                )))
            }
            Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
                items.iter().map(|item| self.seal_value(item)).collect(),
            )),
            Value::Concrete(ConcreteValue::Map(entries)) => Value::Concrete(ConcreteValue::Map(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), self.seal_value(value)))
                    .collect(),
            )),
            _ => value.clone(),
        }
    }
}

enum TokenIndex {
    NotSealed,
    Known(usize),
    Mangled,
}

impl Unsealer {
    pub(crate) fn restore(self, guest: GuestDesired) -> RestoredDesired {
        let mut resources = Vec::with_capacity(guest.resources.len());

        for resource in guest.resources {
            let guest_attributes = wasm_convert::wit_to_core_value_map(&resource.attributes);
            let mut attributes = HashMap::with_capacity(guest_attributes.len());
            let mut skipped_attributes = Vec::new();
            for (key, mut value) in guest_attributes {
                // A token cannot be restored into a string key, so any token
                // material in a key is structurally mangled.
                if key.contains(&self.nonce_prefix) || !self.restore_value(&mut value) {
                    skipped_attributes.push(key);
                } else {
                    attributes.insert(key, value);
                }
            }
            resources.push(RestoredResource {
                attributes,
                skipped_attributes,
            });
        }

        RestoredDesired { resources }
    }

    fn restore_value(&self, value: &mut Value) -> bool {
        if let Value::Concrete(ConcreteValue::String(candidate)) = value {
            return match self.token_index(candidate) {
                TokenIndex::NotSealed => true,
                TokenIndex::Known(index) => {
                    *value = self.secrets[index].clone();
                    true
                }
                TokenIndex::Mangled => false,
            };
        }

        match value {
            Value::Concrete(ConcreteValue::List(items)) => {
                items.iter_mut().all(|item| self.restore_value(item))
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                if items.iter().any(|item| item.contains(&self.nonce_prefix)) {
                    let mut expanded: Vec<Value> = std::mem::take(items)
                        .into_iter()
                        .map(|item| Value::Concrete(ConcreteValue::String(item)))
                        .collect();
                    if !expanded.iter_mut().all(|item| self.restore_value(item)) {
                        return false;
                    }
                    *value = Value::Concrete(ConcreteValue::List(expanded));
                }
                true
            }
            Value::Concrete(ConcreteValue::Map(entries)) => {
                if entries.keys().any(|key| key.contains(&self.nonce_prefix)) {
                    return false;
                }
                entries.values_mut().all(|value| self.restore_value(value))
            }
            Value::Deferred(DeferredValue::Secret(inner)) => {
                let token = match inner.as_ref() {
                    Value::Concrete(ConcreteValue::String(candidate)) => {
                        self.token_index(candidate)
                    }
                    _ => return self.restore_value(inner),
                };
                match token {
                    TokenIndex::NotSealed => true,
                    TokenIndex::Known(index) => {
                        *value = self.secrets[index].clone();
                        true
                    }
                    TokenIndex::Mangled => false,
                }
            }
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_),
            )
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::Unknown(_),
            ) => true,
        }
    }

    fn token_index(&self, candidate: &str) -> TokenIndex {
        if !candidate.contains(&self.nonce_prefix) {
            return TokenIndex::NotSealed;
        }
        let Some(index) = candidate
            .strip_prefix(&self.nonce_prefix)
            .and_then(|suffix| suffix.parse::<usize>().ok())
            .filter(|index| *index < self.secrets.len())
            .filter(|index| candidate == format!("{}{index}", self.nonce_prefix))
        else {
            return TokenIndex::Mangled;
        };
        TokenIndex::Known(index)
    }
}

#[cfg(test)]
mod tests {
    use carina_core::resource::{ConcreteValue, DeferredValue};

    use super::*;

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string()))
    }

    fn secret(value: Value) -> Value {
        Value::Deferred(DeferredValue::Secret(Box::new(value)))
    }

    fn resource_with_attribute(name: &str, value: Value) -> Resource {
        let mut resource = Resource::with_provider("mock", "test.resource", "test", None);
        resource.attributes.insert(name.to_string(), value);
        resource
    }

    fn payload_strings(payload: &SealedDesired) -> Vec<&str> {
        fn collect<'a>(value: &'a wit::Value, strings: &mut Vec<&'a str>) {
            match value {
                wit::Value::StrVal(value)
                | wit::Value::StringListVal(value)
                | wit::Value::ListVal(value)
                | wit::Value::MapVal(value)
                | wit::Value::SecretVal(value) => strings.push(value),
                wit::Value::BoolVal(_) | wit::Value::IntVal(_) | wit::Value::FloatVal(_) => {}
            }
        }

        let mut strings = Vec::new();
        for resource in &payload.resources {
            for (_, value) in &resource.attributes {
                collect(value, &mut strings);
            }
        }
        for (_, value) in &payload.default_tags {
            collect(value, &mut strings);
        }
        strings
    }

    fn take_guest_resources(payload: &mut SealedDesired) -> GuestDesired {
        GuestDesired::new(std::mem::take(&mut payload.resources))
    }

    fn guest_attribute_mut<'a>(
        payload: &'a mut SealedDesired,
        attribute: &str,
    ) -> &'a mut wit::Value {
        &mut payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == attribute)
            .unwrap_or_else(|| panic!("{attribute} attribute should be present"))
            .1
    }

    fn assert_mangled_guest_value_is_skipped(
        rewrite: impl FnOnce(&str, &str, usize) -> wit::Value,
    ) {
        let original_secret = secret(string("host-secret"));
        let mut resource = resource_with_attribute("affected", original_secret.clone());
        resource
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let mut resources = vec![resource];
        // The second secret makes index 1 valid, so non-canonical spellings
        // exercise exact-format validation rather than only bounds checking.
        let default_tags =
            IndexMap::from([("Other".to_string(), secret(string("different-secret")))]);
        let (mut payload, unsealer) =
            seal(&resources, Some(&default_tags)).expect("seal should succeed");
        let wit::Value::StrVal(token) = guest_attribute_mut(&mut payload, "affected") else {
            panic!("expected a sealed token");
        };
        let token = token.clone();
        let rewritten = rewrite(&token, &unsealer.nonce_prefix, unsealer.secrets.len());
        *guest_attribute_mut(&mut payload, "affected") = rewritten;
        *guest_attribute_mut(&mut payload, "sibling") =
            wit::Value::StrVal("guest-sibling".to_string());

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        assert_eq!(
            restored.resources[0].skipped_attributes,
            vec!["affected".to_string()]
        );
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("affected"), Some(&original_secret));
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
    }

    #[test]
    fn seal_restore_preserves_nested_map_and_list_secrets() {
        let original = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                string("public"),
                secret(string("nested-secret")),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "deep".to_string(),
                    secret(Value::Concrete(ConcreteValue::Int(42))),
                )]))),
            ])),
        )])));
        let mut resources = vec![resource_with_attribute("nested", original.clone())];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("nested"), Some(&original));
    }

    #[test]
    fn seal_restore_preserves_secret_from_default_tags() {
        let original_secret = secret(string("default-tag-secret"));
        let default_tags = IndexMap::from([("Token".to_string(), original_secret.clone())]);
        let resources = vec![Resource::with_provider(
            "mock",
            "test.resource",
            "test",
            None,
        )];
        let (mut payload, unsealer) =
            seal(&resources, Some(&default_tags)).expect("seal should succeed");
        let (_, sealed_tag) = payload
            .default_tags
            .pop()
            .expect("sealed default tag should be present");
        let wit::Value::StrVal(token) = &sealed_tag else {
            panic!("default-tag secret must be represented by an opaque string token");
        };
        assert!(!token.contains("default-tag-secret"));
        payload.resources[0]
            .attributes
            .push(("copied_tag".to_string(), sealed_tag));
        let mut restored_resources = resources;

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut restored_resources);

        assert_eq!(
            restored_resources[0].get_attr("copied_tag"),
            Some(&original_secret)
        );
    }

    #[test]
    fn restore_allows_default_tag_secret_token_in_two_resources() {
        let original_secret = secret(string("shared-default-tag-secret"));
        let default_tags = IndexMap::from([("Token".to_string(), original_secret.clone())]);
        let resources = vec![
            Resource::with_provider("mock", "test.resource", "first", None),
            Resource::with_provider("mock", "test.resource", "second", None),
        ];
        let (mut payload, unsealer) =
            seal(&resources, Some(&default_tags)).expect("seal should succeed");
        let (_, wit::Value::StrVal(token)) = payload
            .default_tags
            .pop()
            .expect("sealed default tag should be present")
        else {
            panic!("default-tag secret must be represented by an opaque string token");
        };
        for resource in &mut payload.resources {
            resource
                .attributes
                .push(("copied_tag".to_string(), wit::Value::StrVal(token.clone())));
        }
        let mut restored_resources = resources;

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut restored_resources);

        for resource in &restored_resources {
            assert_eq!(resource.get_attr("copied_tag"), Some(&original_secret));
        }
    }

    #[test]
    fn restore_allows_unused_default_tag_secret_token() {
        let original_secret = secret(string("unused-default-tag-secret"));
        let default_tags = IndexMap::from([("Token".to_string(), original_secret)]);
        let resources = vec![Resource::with_provider(
            "mock",
            "test.resource",
            "untaggable",
            None,
        )];
        let (mut payload, unsealer) =
            seal(&resources, Some(&default_tags)).expect("seal should succeed");
        let mut restored_resources = resources;

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut restored_resources);

        assert!(restored_resources[0].attributes.is_empty());
    }

    #[test]
    fn restore_follows_tokens_when_list_items_are_reordered() {
        let original = Value::Concrete(ConcreteValue::List(vec![
            secret(string("first-secret")),
            secret(string("second-secret")),
        ]));
        let mut resources = vec![resource_with_attribute("items", original)];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let wit::Value::ListVal(json) = &mut payload.resources[0].attributes[0].1 else {
            panic!("expected a sealed list");
        };
        let mut items: Vec<serde_json::Value> =
            serde_json::from_str(json).expect("sealed list should be JSON");
        items.swap(0, 1);
        *json = serde_json::to_string(&items).expect("JSON serialization should succeed");

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(
            resources[0].get_attr("items"),
            Some(&Value::Concrete(ConcreteValue::List(vec![
                secret(string("second-secret")),
                secret(string("first-secret")),
            ])))
        );
    }

    #[test]
    fn restore_flattens_secret_val_wrapped_token() {
        let original_secret = secret(string("secret-val-wrapped-token"));
        let mut resources = vec![resource_with_attribute("secret", original_secret.clone())];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let wit::Value::StrVal(token) = &payload.resources[0].attributes[0].1 else {
            panic!("expected a sealed token");
        };
        let encoded_token =
            serde_json::to_string(token).expect("token JSON serialization should succeed");
        payload.resources[0].attributes[0].1 = wit::Value::SecretVal(encoded_token);

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("secret"), Some(&original_secret));
    }

    #[test]
    fn restore_expands_string_list_token_to_secret_value() {
        let original_secret = secret(string("string-list-token"));
        let mut resources = vec![resource_with_attribute("items", original_secret.clone())];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let wit::Value::StrVal(token) = &payload.resources[0].attributes[0].1 else {
            panic!("expected a sealed token");
        };
        let encoded_items = serde_json::to_string(&vec!["public", token])
            .expect("string list JSON serialization should succeed");
        payload.resources[0].attributes[0].1 = wit::Value::StringListVal(encoded_items);

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(
            resources[0].get_attr("items"),
            Some(&Value::Concrete(ConcreteValue::List(vec![
                string("public"),
                original_secret,
            ])))
        );
    }

    #[test]
    fn restore_skips_attribute_with_nonce_in_map_key() {
        let original_secret = secret(string("map-key-secret"));
        let mut resource = resource_with_attribute("mapped", original_secret.clone());
        resource
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let mut resources = vec![resource];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let (_, wit::Value::StrVal(token)) = payload.resources[0]
            .attributes
            .iter()
            .find(|(key, _)| key == "mapped")
            .expect("mapped attribute should be present")
        else {
            panic!("expected a sealed token");
        };
        let map = serde_json::Map::from_iter([(
            format!("{token}-mangled"),
            serde_json::Value::String("guest-value".to_string()),
        )]);
        payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "mapped")
            .expect("mapped attribute should be present")
            .1 = wit::Value::MapVal(
            serde_json::to_string(&map).expect("map JSON serialization should succeed"),
        );
        payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "sibling")
            .expect("sibling attribute should be present")
            .1 = wit::Value::StrVal("guest-sibling".to_string());

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("mapped"), Some(&original_secret));
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
    }

    #[test]
    fn restore_skips_top_level_attribute_with_nonce_in_key() {
        let original_secret = secret(string("attribute-key-secret"));
        let mut resource = resource_with_attribute("secret", original_secret.clone());
        resource
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let mut resources = vec![resource];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let secret_attribute = payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "secret")
            .expect("secret attribute should be present");
        let wit::Value::StrVal(token) = &secret_attribute.1 else {
            panic!("expected a sealed token");
        };
        let mangled_key = format!("mangled-{token}");
        secret_attribute.0.clone_from(&mangled_key);
        payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "sibling")
            .expect("sibling attribute should be present")
            .1 = wit::Value::StrVal("guest-sibling".to_string());

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("secret"), Some(&original_secret));
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
        assert!(!resources[0].attributes.contains_key(&mangled_key));
    }

    #[test]
    fn restore_skips_list_with_mangled_token_item() {
        assert_mangled_guest_value_is_skipped(|token, _, _| {
            wit::Value::ListVal(
                serde_json::to_string(&vec![format!("{token}-mangled")])
                    .expect("list JSON serialization should succeed"),
            )
        });
    }

    #[test]
    fn restore_skips_map_with_mangled_token_value() {
        assert_mangled_guest_value_is_skipped(|token, _, _| {
            wit::Value::MapVal(
                serde_json::to_string(&serde_json::json!({
                    "nested": format!("{token}-mangled"),
                }))
                .expect("map JSON serialization should succeed"),
            )
        });
    }

    #[test]
    fn restore_skips_string_list_with_mangled_token_item() {
        assert_mangled_guest_value_is_skipped(|token, _, _| {
            wit::Value::StringListVal(
                serde_json::to_string(&vec![format!("{token}-mangled")])
                    .expect("string list JSON serialization should succeed"),
            )
        });
    }

    #[test]
    fn restore_skips_secret_val_with_mangled_token() {
        assert_mangled_guest_value_is_skipped(|token, _, _| {
            wit::Value::SecretVal(
                serde_json::to_string(&format!("{token}-mangled"))
                    .expect("secret JSON serialization should succeed"),
            )
        });
    }

    #[test]
    fn restore_skips_secret_val_wrapping_list_with_mangled_token() {
        assert_mangled_guest_value_is_skipped(|token, _, _| {
            wit::Value::SecretVal(
                serde_json::to_string(&vec![format!("{token}-mangled")])
                    .expect("secret list JSON serialization should succeed"),
            )
        });
    }

    #[test]
    fn restore_skips_out_of_range_token_index_without_panicking() {
        assert_mangled_guest_value_is_skipped(|_, nonce_prefix, secret_count| {
            wit::Value::StrVal(format!("{nonce_prefix}{secret_count}"))
        });
    }

    #[test]
    fn restore_skips_non_canonical_token_indices() {
        for suffix in ["01", "+0"] {
            assert_mangled_guest_value_is_skipped(|_, nonce_prefix, _| {
                wit::Value::StrVal(format!("{nonce_prefix}{suffix}"))
            });
        }
    }

    #[test]
    fn restore_preserves_token_free_string_list_variant() {
        let original_secret = secret(string("host-secret"));
        let mut resource = resource_with_attribute("affected", original_secret);
        resource
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let mut resources = vec![resource];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        *guest_attribute_mut(&mut payload, "affected") = wit::Value::StringListVal(
            serde_json::to_string(&vec!["first", "second"])
                .expect("string list JSON serialization should succeed"),
        );
        *guest_attribute_mut(&mut payload, "sibling") =
            wit::Value::StrVal("guest-sibling".to_string());

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        assert!(restored.resources[0].skipped_attributes.is_empty());
        restored.apply_to(&mut resources);

        assert_eq!(
            resources[0].get_attr("affected"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "first".to_string(),
                "second".to_string(),
            ])))
        );
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
    }

    #[test]
    fn restore_skips_only_attribute_with_mangled_token() {
        let original_secret = secret(string("mangled-secret"));
        let mut first = resource_with_attribute("secret", original_secret.clone());
        first
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let second = resource_with_attribute("other", string("host-other"));
        let mut resources = vec![first, second];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let (_, wit::Value::StrVal(token)) = payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "secret")
            .expect("secret attribute should be present")
        else {
            panic!("expected a sealed token");
        };
        token.push_str("-mangled");
        payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "sibling")
            .expect("sibling attribute should be present")
            .1 = wit::Value::StrVal("guest-sibling".to_string());
        payload.resources[1]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "other")
            .expect("other resource attribute should be present")
            .1 = wit::Value::StrVal("guest-other".to_string());

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("secret"), Some(&original_secret));
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
        assert_eq!(resources[1].get_attr("other"), Some(&string("guest-other")));
    }

    #[test]
    fn restore_replaces_duplicated_resource_attribute_token_everywhere() {
        let original_secret = secret(string("duplicated-secret"));
        let mut resources = vec![resource_with_attribute("secret", original_secret.clone())];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        let wit::Value::StrVal(token) = &payload.resources[0].attributes[0].1 else {
            panic!("expected a sealed token");
        };
        let token = token.clone();
        payload.resources[0]
            .attributes
            .push(("duplicate".to_string(), wit::Value::StrVal(token)));

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("secret"), Some(&original_secret));
        assert_eq!(resources[0].get_attr("duplicate"), Some(&original_secret));
    }

    #[test]
    fn restore_allows_dropped_secret_attribute_and_applies_other_attributes() {
        let original_secret = secret(string("dropped-secret"));
        let mut resource = resource_with_attribute("secret", original_secret.clone());
        resource
            .attributes
            .insert("sibling".to_string(), string("host-sibling"));
        let mut resources = vec![resource];
        let (mut payload, unsealer) = seal(&resources, None).expect("seal should succeed");
        payload.resources[0]
            .attributes
            .retain(|(key, _)| key != "secret");
        payload.resources[0]
            .attributes
            .iter_mut()
            .find(|(key, _)| key == "sibling")
            .expect("sibling attribute should be present")
            .1 = wit::Value::StrVal("guest-sibling".to_string());
        payload.resources[0].attributes.push((
            "guest-added".to_string(),
            wit::Value::StrVal("guest-value".to_string()),
        ));

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("secret"), Some(&original_secret));
        assert_eq!(
            resources[0].get_attr("sibling"),
            Some(&string("guest-sibling"))
        );
        assert_eq!(
            resources[0].get_attr("guest-added"),
            Some(&string("guest-value"))
        );
    }

    #[test]
    fn sealed_wit_payload_contains_no_secret_plaintext() {
        let resources = vec![resource_with_attribute(
            "nested",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "value".to_string(),
                secret(string("resource-plaintext-must-not-cross")),
            )]))),
        )];
        let default_tags = IndexMap::from([(
            "Token".to_string(),
            secret(string("default-tag-plaintext-must-not-cross")),
        )]);

        let (payload, _) = seal(&resources, Some(&default_tags)).expect("seal should succeed");
        let wire_strings = payload_strings(&payload).join("\n");

        assert!(!wire_strings.contains("resource-plaintext-must-not-cross"));
        assert!(!wire_strings.contains("default-tag-plaintext-must-not-cross"));
    }
}
