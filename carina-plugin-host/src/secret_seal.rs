//! Opaque secret transport for guest calls that return desired resources.
//!
//! CRUD calls intentionally use the WIT `secret-val` representation because
//! providers need the plaintext. Desired-state round trips use this module
//! instead: secrets become per-call tokens, and the guest response remains an
//! opaque [`GuestDesired`] until the matching [`Unsealer`] restores known
//! tokens, rejects attributes the host marked deferred, and isolates
//! attributes containing mangled tokens.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;

use carina_core::resource::{ConcreteValue, DeferredValue, Resource, Value};
use carina_core::value::SerializationError;

use crate::wasm_convert;
use crate::wasm_factory::version_checked_bindings::wit_types as wit;

const TOKEN_NAMESPACE: &str = "carina-sealed-secret";

pub(crate) struct SealedDesired {
    resources: Vec<wit::ResourceDef>,
    deferred: Vec<Vec<String>>,
}

impl SealedDesired {
    pub(crate) async fn send<'a, E, P, F, Fut>(
        &'a self,
        call: F,
    ) -> Result<Result<GuestDesired, P>, E>
    where
        F: FnOnce(&'a [wit::ResourceDef], &'a [Vec<String>]) -> Fut,
        Fut: Future<Output = Result<Result<Vec<wit::ResourceDef>, P>, E>>,
    {
        let result = call(&self.resources, &self.deferred).await?;
        Ok(result.map(GuestDesired::new))
    }
}

// The field stays private so a desired-state caller cannot feed raw guest
// attributes to `wasm_convert`; only `Unsealer::try_restore` can consume them.
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
    host_attributes: Vec<indexmap::IndexMap<String, Value>>,
    sent_attributes: Vec<HashMap<String, wit::Value>>,
    deferred: Vec<Vec<String>>,
    expected_resource_count: usize,
}

#[derive(Debug)]
pub(crate) enum SealError {
    ResourceValue(SerializationError),
    DeferredResourceCountMismatch { resources: usize, deferred: usize },
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceValue(error) => {
                write!(f, "failed to encode sealed resource value: {error}")
            }
            Self::DeferredResourceCountMismatch {
                resources,
                deferred,
            } => write!(
                f,
                "received deferred attribute names for {deferred} resources; expected {resources}"
            ),
        }
    }
}

impl std::error::Error for SealError {}

impl From<SerializationError> for SealError {
    fn from(source: SerializationError) -> Self {
        Self::ResourceValue(source)
    }
}

#[derive(Debug)]
pub(crate) enum RestoreError {
    ResourceCountMismatch { expected: usize, actual: usize },
    DeferredAttributeReturned { resource_index: usize, name: String },
    ValueDecode(wasm_convert::WasmValueDecodeError),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceCountMismatch { expected, actual } => write!(
                f,
                "WASM guest returned {actual} normalized resources; expected {expected}"
            ),
            Self::DeferredAttributeReturned {
                resource_index,
                name,
            } => write!(
                f,
                "WASM guest returned host-deferred attribute '{name}' for normalized resource at index {resource_index}"
            ),
            Self::ValueDecode(error) => {
                write!(f, "failed to decode normalized resource values: {error}")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<wasm_convert::WasmValueDecodeError> for RestoreError {
    fn from(error: wasm_convert::WasmValueDecodeError) -> Self {
        Self::ValueDecode(error)
    }
}

pub(crate) struct RestoredDesired {
    resources: Vec<RestoredResource>,
}

struct RestoredResource {
    attributes: HashMap<String, Value>,
    skipped_attributes: Vec<String>,
}

impl RestoredDesired {
    pub(crate) fn apply_to<S>(self, resources: &mut [Resource<S>]) {
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

pub(crate) fn seal_with_deferred<S: Clone>(
    resources: &[Resource<S>],
    deferred: &[Vec<String>],
) -> Result<(SealedDesired, Unsealer), SealError> {
    if resources.len() != deferred.len() {
        return Err(SealError::DeferredResourceCountMismatch {
            resources: resources.len(),
            deferred: deferred.len(),
        });
    }
    let nonce_prefix = format!("{TOKEN_NAMESPACE}:{}:", uuid::Uuid::new_v4().simple());
    let host_attributes = resources
        .iter()
        .map(|resource| resource.attributes.clone())
        .collect();
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
    let expected_resource_count = resources.len();
    let sent_attributes = resources
        .iter()
        .map(|resource| resource.attributes.iter().cloned().collect())
        .collect();

    Ok((
        SealedDesired {
            resources,
            deferred: deferred.to_vec(),
        },
        Unsealer {
            nonce_prefix,
            secrets: sealer.secrets,
            host_attributes,
            sent_attributes,
            deferred: deferred.to_vec(),
            expected_resource_count,
        },
    ))
}

#[cfg(test)]
fn seal<S: Clone>(resources: &[Resource<S>]) -> Result<(SealedDesired, Unsealer), SealError> {
    seal_with_deferred(resources, &vec![Vec::new(); resources.len()])
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
                Value::Concrete(ConcreteValue::String(
                    format!("{}{index}", self.nonce_prefix).into(),
                ))
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
    pub(crate) fn try_restore(self, guest: GuestDesired) -> Result<RestoredDesired, RestoreError> {
        if guest.resources.len() != self.expected_resource_count {
            return Err(RestoreError::ResourceCountMismatch {
                expected: self.expected_resource_count,
                actual: guest.resources.len(),
            });
        }
        let mut resources = Vec::with_capacity(guest.resources.len());

        for (resource_index, resource) in guest.resources.into_iter().enumerate() {
            if let Some((name, _value)) = resource.attributes.iter().find(|(name, _value)| {
                self.deferred[resource_index]
                    .iter()
                    .any(|deferred| deferred == name)
            }) {
                return Err(RestoreError::DeferredAttributeReturned {
                    resource_index,
                    name: name.clone(),
                });
            }
            let host_attributes = &self.host_attributes[resource_index];
            let sent_attributes = &self.sent_attributes[resource_index];
            let guest_wire_by_key: HashMap<&str, &wit::Value> = resource
                .attributes
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect();
            let guest_attributes = wasm_convert::wit_to_core_value_map(&resource.attributes)?;
            let mut attributes = HashMap::with_capacity(guest_attributes.len());
            let mut skipped_attributes = Vec::new();
            for (key, mut value) in guest_attributes {
                if let (Some(host_value), Some(sent_value), Some(guest_value)) = (
                    host_attributes.get(&key),
                    sent_attributes.get(&key),
                    guest_wire_by_key.get(key.as_str()),
                ) {
                    preserve_wire_equal_host_values(
                        host_value,
                        sent_value,
                        guest_value,
                        &mut value,
                    );
                }
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

        Ok(RestoredDesired { resources })
    }

    #[cfg(test)]
    fn restore(self, guest: GuestDesired) -> RestoredDesired {
        self.try_restore(guest)
            .expect("test guest values must be valid WIT boundary payloads")
    }

    fn restore_value(&self, value: &mut Value) -> bool {
        if let Value::Concrete(ConcreteValue::String(candidate)) = value {
            return match self.token_index(candidate.expose_plaintext()) {
                TokenIndex::NotSealed => true,
                TokenIndex::Known(index) => {
                    *value = self.secrets[index].clone();
                    true
                }
                TokenIndex::Mangled => false,
            };
        }

        match value {
            Value::Deferred(DeferredValue::ModuleIf(_)) => false,
            Value::Concrete(ConcreteValue::List(items)) => {
                items.iter_mut().all(|item| self.restore_value(item))
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                if items
                    .iter()
                    .any(|item| item.expose_plaintext().contains(&self.nonce_prefix))
                {
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
                        self.token_index(candidate.expose_plaintext())
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
            Value::Deferred(DeferredValue::Coalesce { value, default }) => {
                self.restore_value(value) && self.restore_value(default)
            }
            Value::Deferred(DeferredValue::RedactedSecret { .. }) => false,
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
                | DeferredValue::Unavailable { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Access(_)
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

fn preserve_wire_equal_host_values(
    host: &Value,
    sent: &wit::Value,
    returned: &wit::Value,
    guest: &mut Value,
) {
    if same_wire_value(sent, returned) {
        *guest = host.clone();
        return;
    }

    let (sent_json, returned_json) = match (sent, returned) {
        (wit::Value::StringListVal(sent), wit::Value::StringListVal(returned))
        | (wit::Value::ListVal(sent), wit::Value::ListVal(returned))
        | (wit::Value::MapVal(sent), wit::Value::MapVal(returned))
        | (wit::Value::SecretVal(sent), wit::Value::SecretVal(returned)) => (
            serde_json::from_str::<serde_json::Value>(sent),
            serde_json::from_str::<serde_json::Value>(returned),
        ),
        (
            wit::Value::StrVal(_)
            | wit::Value::IntVal(_)
            | wit::Value::FloatVal(_)
            | wit::Value::BoolVal(_),
            _,
        )
        | (
            _,
            wit::Value::StrVal(_)
            | wit::Value::IntVal(_)
            | wit::Value::FloatVal(_)
            | wit::Value::BoolVal(_),
        ) => return,
        _ => return,
    };
    if let (Ok(sent_json), Ok(returned_json)) = (sent_json, returned_json) {
        preserve_equal_json_subtrees(host, &sent_json, &returned_json, guest);
    }
}

fn preserve_equal_json_subtrees(
    host: &Value,
    sent: &serde_json::Value,
    returned: &serde_json::Value,
    guest: &mut Value,
) {
    if sent == returned {
        *guest = host.clone();
        return;
    }

    match (host, sent, returned, guest) {
        (
            Value::Concrete(ConcreteValue::Map(host_entries)),
            serde_json::Value::Object(sent_entries),
            serde_json::Value::Object(returned_entries),
            Value::Concrete(ConcreteValue::Map(guest_entries)),
        ) => {
            for (key, guest_value) in guest_entries {
                let (Some(host_value), Some(sent_value), Some(returned_value)) = (
                    host_entries.get(key),
                    sent_entries.get(key),
                    returned_entries.get(key),
                ) else {
                    continue;
                };
                preserve_equal_json_subtrees(host_value, sent_value, returned_value, guest_value);
            }
        }
        (
            Value::Concrete(ConcreteValue::List(host_items)),
            serde_json::Value::Array(sent_items),
            serde_json::Value::Array(returned_items),
            Value::Concrete(ConcreteValue::List(guest_items)),
        ) if sent_items.len() == returned_items.len()
            && host_items.len() == sent_items.len()
            && guest_items.len() == returned_items.len() =>
        {
            for (((host_item, sent_item), returned_item), guest_item) in host_items
                .iter()
                .zip(sent_items)
                .zip(returned_items)
                .zip(guest_items)
            {
                preserve_equal_json_subtrees(host_item, sent_item, returned_item, guest_item);
            }
        }
        (
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_)
                | ConcreteValue::StringList(_),
            )
            | Value::Deferred(
                DeferredValue::ModuleIf(_)
                | DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::Coalesce { .. }
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Access(_)
                | DeferredValue::Secret(_)
                | DeferredValue::RedactedSecret { .. }
                | DeferredValue::Unknown(_),
            ),
            _,
            _,
            _,
        )
        | (Value::Concrete(ConcreteValue::Map(_) | ConcreteValue::List(_)), _, _, _) => {}
    }
}

/// Compare the exact provider wire forms so an echoed enum string can retain
/// the richer host-side type that was intentionally erased for transport.
fn same_wire_value(left: &wit::Value, right: &wit::Value) -> bool {
    match (left, right) {
        (wit::Value::StrVal(left), wit::Value::StrVal(right)) => left == right,
        (wit::Value::StringListVal(left), wit::Value::StringListVal(right))
        | (wit::Value::ListVal(left), wit::Value::ListVal(right))
        | (wit::Value::MapVal(left), wit::Value::MapVal(right))
        | (wit::Value::SecretVal(left), wit::Value::SecretVal(right)) => {
            same_json_wire_value(left, right)
        }
        (wit::Value::IntVal(left), wit::Value::IntVal(right)) => left == right,
        (wit::Value::FloatVal(left), wit::Value::FloatVal(right)) => {
            left.to_bits() == right.to_bits()
        }
        (wit::Value::BoolVal(left), wit::Value::BoolVal(right)) => left == right,
        _ => false,
    }
}

/// JSON-backed wire variants use semantic JSON equality because guests may
/// reserialize an unchanged subtree with different insignificant formatting.
fn same_json_wire_value(left: &str, right: &str) -> bool {
    left == right
        || matches!(
            (
                serde_json::from_str::<serde_json::Value>(left),
                serde_json::from_str::<serde_json::Value>(right),
            ),
            (Ok(left), Ok(right)) if left == right
        )
}

#[cfg(test)]
mod tests {
    use carina_core::resource::{ConcreteValue, DeferredValue};
    use carina_core::schema::{AttributeType, TypeIdentity};
    use indexmap::IndexMap;

    use super::*;

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn secret(value: Value) -> Value {
        Value::Deferred(DeferredValue::Secret(Box::new(value)))
    }

    fn resource_with_attribute(name: &str, value: Value) -> Resource {
        let mut resource = Resource::with_provider("mock", "test.resource", "test", None);
        resource.attributes.insert(name.to_string(), value);
        resource
    }

    fn mode_enum_type() -> AttributeType {
        AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["test", "resource"], "Mode"),
            Some(vec!["Enabled".to_string(), "Disabled".to_string()]),
            Vec::new(),
            None,
            None,
        )
    }

    fn canonical_mode(mode: &str) -> Value {
        let canonical = carina_core::resource::EnumValueResolver::new(&mode_enum_type())
            .resolve_state_text(mode)
            .expect("the test mode must resolve");
        Value::Concrete(ConcreteValue::CanonicalEnum(canonical))
    }

    fn rewrite_json_attribute(
        payload: &mut SealedDesired,
        attribute: &str,
        rewrite: impl FnOnce(&mut serde_json::Value),
    ) {
        let value = guest_attribute_mut(payload, attribute);
        let json = match value {
            wit::Value::StringListVal(json)
            | wit::Value::ListVal(json)
            | wit::Value::MapVal(json)
            | wit::Value::SecretVal(json) => json,
            other => panic!("{attribute} must use a JSON-backed wire value, got {other:?}"),
        };
        let mut decoded = serde_json::from_str(json).expect("test wire JSON must decode");
        rewrite(&mut decoded);
        *json = serde_json::to_string(&decoded).expect("test wire JSON must encode");
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
        resource
            .attributes
            .insert("other".to_string(), secret(string("different-secret")));
        let mut resources = vec![resource];
        // The second secret makes index 1 valid, so non-canonical spellings
        // exercise exact-format validation rather than only bounds checking.
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
    fn restore_rejects_a_different_guest_resource_count_before_mutation() {
        for return_extra_resource in [false, true] {
            let resources = vec![
                resource_with_attribute("value", string("first")),
                resource_with_attribute("value", string("second")),
            ];
            let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");

            if return_extra_resource {
                payload.resources.push(payload.resources[0].clone());
            } else {
                payload.resources.pop();
            }

            let result = unsealer.try_restore(take_guest_resources(&mut payload));

            assert!(
                result.is_err(),
                "a guest resource count mismatch must be a boundary error"
            );
        }
    }

    #[test]
    fn restore_rejects_a_guest_attribute_marked_deferred_by_the_host() {
        let resources = vec![resource_with_attribute("known", string("host-known"))];
        let deferred = vec![vec!["pending".to_string()]];
        let (mut payload, unsealer) =
            seal_with_deferred(&resources, &deferred).expect("seal should succeed");
        payload.resources[0].attributes.push((
            "pending".to_string(),
            wit::Value::StrVal("guest-value".to_string()),
        ));

        let error = match unsealer.try_restore(take_guest_resources(&mut payload)) {
            Ok(_) => panic!("the guest must not synthesize a host-deferred attribute"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            RestoreError::DeferredAttributeReturned {
                resource_index: 0,
                name,
            } if name == "pending"
        ));
    }

    #[test]
    fn echoed_enum_wire_value_keeps_host_typing_and_plan_display() {
        let enum_type = AttributeType::enum_(
            TypeIdentity::new(
                Some("aws"),
                ["route53", "RecordSet", "AliasTarget"],
                "HostedZoneId",
            ),
            Some(vec!["Z2FDTNDATAQYW2".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let canonical = carina_core::resource::EnumValueResolver::new(&enum_type)
            .resolve_raw(&carina_core::resource::RawEnumIdentifier::parse(
                "Z2FDTNDATAQYW2",
            ))
            .expect("the identifier must resolve through the Enum arm of the union");
        let original = Value::Concrete(ConcreteValue::CanonicalEnum(canonical));
        let alias_target = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "hosted_zone_id".to_string(),
            original.clone(),
        )])));
        let mut resources = vec![resource_with_attribute("alias_target", alias_target)];
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");

        // The guest echoes the exact StrVal wire representation it received.
        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        let Value::Concrete(ConcreteValue::Map(normalized_alias_target)) = resources[0]
            .get_attr("alias_target")
            .expect("the normalized attribute must remain present")
        else {
            panic!("alias_target must remain a typed map");
        };
        let normalized = normalized_alias_target
            .get("hosted_zone_id")
            .expect("the nested enum must remain present");
        assert_eq!(normalized, &original);
        assert_eq!(
            carina_core::value::format_value(normalized),
            "Z2FDTNDATAQYW2",
            "plan-value display must retain identifier rendering rather than adding quotes"
        );
    }

    #[test]
    fn partial_nested_change_preserves_unchanged_enum_typing_and_relifts_changed_leaf() {
        let unchanged = canonical_mode("Enabled");
        let changed = canonical_mode("Enabled");
        let settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("unchanged".to_string(), unchanged.clone()),
            ("changed".to_string(), changed),
        ])));
        let mut resources = vec![resource_with_attribute("settings", settings)];
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
        rewrite_json_attribute(&mut payload, "settings", |json| {
            json["changed"] = serde_json::Value::String("Disabled".to_string());
        });

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        let Some(Value::Concrete(ConcreteValue::Map(settings))) = resources[0].get_attr("settings")
        else {
            panic!("settings must remain a map");
        };
        assert_eq!(settings.get("unchanged"), Some(&unchanged));
        assert_eq!(
            carina_core::value::format_value(settings.get("unchanged").unwrap()),
            "Enabled",
            "an unchanged enum leaf must keep unquoted plan rendering"
        );
        assert_eq!(settings.get("changed"), Some(&string("Disabled")));

        let mut registry = carina_core::schema::SchemaRegistry::new();
        registry.insert(
            "mock",
            carina_core::schema::ResourceSchema::new("test.resource")
                .attribute(
                    carina_core::schema::AttributeSchema::optional(
                        "settings",
                        AttributeType::map(mode_enum_type()),
                    )
                    .build(),
                )
                .validate()
                .expect("test schema must be valid"),
        );
        carina_core::value::canonicalize_normalized_resources_with_schemas(
            &mut resources,
            &registry,
        )
        .expect("the changed guest string must be re-lifted by the normalizer pipeline");
        let Some(Value::Concrete(ConcreteValue::Map(settings))) = resources[0].get_attr("settings")
        else {
            panic!("settings must remain a map after re-lifting");
        };
        assert!(matches!(
            settings.get("changed"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "Disabled"
        ));
    }

    #[test]
    fn length_changed_list_uses_the_complete_guest_list() {
        let mut resources = vec![resource_with_attribute(
            "modes",
            Value::Concrete(ConcreteValue::List(vec![canonical_mode("Enabled")])),
        )];
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
        rewrite_json_attribute(&mut payload, "modes", |json| {
            json.as_array_mut()
                .expect("modes must encode as an array")
                .push(serde_json::Value::String("Disabled".to_string()));
        });

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(
            resources[0].get_attr("modes"),
            Some(&Value::Concrete(ConcreteValue::List(vec![
                string("Enabled"),
                string("Disabled"),
            ]))),
            "a changed list length makes the whole list guest-owned"
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");

        let restored = unsealer.restore(take_guest_resources(&mut payload));
        restored.apply_to(&mut resources);

        assert_eq!(resources[0].get_attr("nested"), Some(&original));
    }

    #[test]
    fn restore_follows_tokens_when_list_items_are_reordered() {
        let original = Value::Concrete(ConcreteValue::List(vec![
            secret(string("first-secret")),
            secret(string("second-secret")),
        ]));
        let mut resources = vec![resource_with_attribute("items", original)];
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
                "first".into(),
                "second".into(),
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (mut payload, unsealer) = seal(&resources).expect("seal should succeed");
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
        let (payload, _) = seal(&resources).expect("seal should succeed");
        let wire_strings = payload_strings(&payload).join("\n");

        assert!(!wire_strings.contains("resource-plaintext-must-not-cross"));
    }
}
