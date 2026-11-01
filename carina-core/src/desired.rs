//! Projection and restoration of opaque leaves during desired normalization.

use crate::protocol_convert::{
    protocol_to_core_value, resource_id_to_protocol, try_core_to_protocol_value,
};
use crate::resource::{ConcreteValue, Resource, Value, is_deferred_desired_leaf};
use carina_provider_protocol::normalization::{
    NormalizationError, NormalizationErrorKind as Kind, NormalizationPathSegment as Segment,
    NormalizationResource, NormalizationValue as Wire,
};
use indexmap::IndexMap;

impl From<NormalizationError> for crate::provider::ProviderError {
    fn from(error: NormalizationError) -> Self {
        let id = crate::protocol_convert::protocol_to_resource_id(&error.resource_id);
        Self::internal_caused_by("Failed to normalize desired resource", error).for_resource(id)
    }
}

/// Owns one call's original leaves. Resources are paired by position, never id.
pub struct NormalizationBatch {
    resources: Vec<NormalizationResource>,
    originals: Vec<IndexMap<String, Value>>,
    leaves: Vec<OriginalLeaf>,
}

struct OriginalLeaf {
    resource: usize,
    attribute: String,
    path: Vec<Segment>,
    value: Value,
}

impl NormalizationBatch {
    pub fn new<S>(resources: &[Resource<S>]) -> Result<Self, NormalizationError> {
        let mut batch = Self {
            resources: Vec::with_capacity(resources.len()),
            originals: Vec::with_capacity(resources.len()),
            leaves: Vec::new(),
        };
        for (index, resource) in resources.iter().enumerate() {
            let id = resource_id_to_protocol(&resource.id);
            let mut attributes = std::collections::BTreeMap::new();
            for (name, value) in &resource.attributes {
                let value = batch
                    .project(value, index, &id, name, &mut Vec::new())
                    .map_err(|error| error.with_resource_index(index))?;
                attributes.insert(name.clone(), value);
            }
            batch
                .resources
                .push(NormalizationResource { id, attributes });
            batch.originals.push(resource.attributes.clone());
        }
        Ok(batch)
    }

    pub fn resources(&self) -> &[NormalizationResource] {
        &self.resources
    }

    fn project(
        &mut self,
        value: &Value,
        resource: usize,
        id: &carina_provider_protocol::types::ResourceId,
        attribute: &str,
        path: &mut Vec<Segment>,
    ) -> Result<Wire, NormalizationError> {
        let error =
            |kind| NormalizationError::new(id.clone(), attribute, kind).with_path(path.clone());
        if is_deferred_desired_leaf(value) {
            let slot =
                u32::try_from(self.leaves.len()).map_err(|_| error(Kind::MalformedHandle))?;
            self.leaves.push(OriginalLeaf {
                resource,
                attribute: attribute.into(),
                path: path.clone(),
                value: value.clone(),
            });
            return Ok(Wire::Deferred(slot));
        }
        match value {
            Value::Concrete(ConcreteValue::List(values)) => {
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    path.push(Segment::Index(index));
                    result.push(self.project(value, resource, id, attribute, path)?);
                    path.pop();
                }
                Ok(Wire::List(result))
            }
            Value::Concrete(ConcreteValue::Map(values)) => {
                let mut result = std::collections::BTreeMap::new();
                for (key, value) in values {
                    path.push(Segment::Key(key.clone()));
                    result.insert(
                        key.clone(),
                        self.project(value, resource, id, attribute, path)?,
                    );
                    path.pop();
                }
                Ok(Wire::Map(result))
            }
            _ => {
                let value = try_core_to_protocol_value(value)
                    .map_err(|source| error(Kind::Conversion).with_source(source))?;
                Ok(match value {
                    carina_provider_protocol::types::Value::Bool(value) => Wire::Bool(value),
                    carina_provider_protocol::types::Value::Int(value) => Wire::Int(value),
                    carina_provider_protocol::types::Value::Float(value) => Wire::Float(value),
                    carina_provider_protocol::types::Value::String(value) => Wire::String(value),
                    carina_provider_protocol::types::Value::StringList(value) => {
                        Wire::StringList(value)
                    }
                    _ => unreachable!("containers were projected structurally"),
                })
            }
        }
    }

    /// Validate the response shape before decoding individual wire records.
    fn check_resource_count(&self, actual: usize) -> Result<(), NormalizationError> {
        if actual != self.resources.len() {
            let index = actual.min(self.resources.len().saturating_sub(1));
            let resource = self.resources.get(index);
            let id = resource
                .map(|resource| resource.id.clone())
                .unwrap_or_else(|| carina_provider_protocol::types::ResourceId {
                    provider: "<batch>".into(),
                    resource_type: "<resources>".into(),
                    identity: "<empty>".into(),
                });
            let attribute = resource
                .and_then(|resource| resource.attributes.keys().next())
                .map(String::as_str)
                .unwrap_or("<attributes>");
            return Err(NormalizationError::new(
                id,
                attribute,
                Kind::ResourceCountMismatch {
                    expected: self.resources.len(),
                    actual,
                },
            )
            .with_resource_index(index));
        }
        Ok(())
    }

    /// Check returned resource ids and every handle before restoring any values.
    pub fn finish(
        self,
        returned: Vec<NormalizationResource>,
    ) -> Result<Vec<IndexMap<String, Value>>, NormalizationError> {
        self.finish_with(returned, |_, resource| Ok(resource))
    }

    /// Check the count once, then decode each response against its original record.
    /// No attributes are published unless decoding and restoration both succeed.
    pub fn finish_with<T>(
        self,
        returned: Vec<T>,
        mut decode: impl FnMut(
            &NormalizationResource,
            T,
        ) -> Result<NormalizationResource, NormalizationError>,
    ) -> Result<Vec<IndexMap<String, Value>>, NormalizationError> {
        self.check_resource_count(returned.len())?;
        let returned = returned
            .into_iter()
            .zip(&self.resources)
            .enumerate()
            .map(|(index, (returned, sent))| {
                decode(sent, returned).map_err(|error| error.with_resource_index(index))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut seen = vec![false; self.leaves.len()];
        for (index, resource) in returned.iter().enumerate() {
            let sent = &self.resources[index];
            let id = &sent.id;
            if resource.id != *id {
                return Err(NormalizationError::new(
                    id.clone(),
                    sent.attributes
                        .keys()
                        .next()
                        .map(String::as_str)
                        .unwrap_or("<attributes>"),
                    Kind::ResourceIdMismatch {
                        expected: Box::new(id.clone()),
                        actual: Box::new(resource.id.clone()),
                    },
                )
                .with_resource_index(index));
            }
            for (name, value) in &resource.attributes {
                self.validate(value, index, id, name, &mut Vec::new(), &mut seen)
                    .map_err(|error| error.with_resource_index(index))?;
            }
        }
        for (slot, (leaf, seen)) in self.leaves.iter().zip(seen).enumerate() {
            if !seen {
                return Err(NormalizationError::new(
                    self.resources[leaf.resource].id.clone(),
                    &leaf.attribute,
                    Kind::MissingHandle { slot: slot as u32 },
                )
                .with_path(leaf.path.clone())
                .with_resource_index(leaf.resource));
            }
        }
        returned
            .into_iter()
            .enumerate()
            .map(|(index, returned)| {
                let original = &self.originals[index];
                let sent = &self.resources[index];
                let mut attributes = IndexMap::new();
                // Retain source order for existing keys, append insertions deterministically.
                for key in original.keys().chain(
                    returned
                        .attributes
                        .keys()
                        .filter(|key| !original.contains_key(*key)),
                ) {
                    if let Some(value) = returned.attributes.get(key) {
                        let mut context = AttributeRestoration {
                            id: &sent.id,
                            attribute: key,
                            known: Vec::new(),
                        };
                        if let (Some(original), Some(sent)) =
                            (original.get(key), sent.attributes.get(key))
                        {
                            collect_known_leaves(
                                original,
                                sent,
                                &mut Vec::new(),
                                &mut context.known,
                            );
                        }
                        context.reserve_same_paths(value);
                        let value = self
                            .restore(
                                value,
                                original.get(key),
                                sent.attributes.get(key),
                                &mut Vec::new(),
                                &mut context,
                            )
                            .map_err(|error| error.with_resource_index(index))?;
                        attributes.insert(key.clone(), value);
                    }
                }
                Ok(attributes)
            })
            .collect()
    }

    fn validate(
        &self,
        value: &Wire,
        resource: usize,
        id: &carina_provider_protocol::types::ResourceId,
        attribute: &str,
        path: &mut Vec<Segment>,
        seen: &mut [bool],
    ) -> Result<(), NormalizationError> {
        let error =
            |kind| NormalizationError::new(id.clone(), attribute, kind).with_path(path.clone());
        match value {
            Wire::Deferred(slot) => {
                let Some(leaf) = self.leaves.get(*slot as usize) else {
                    return Err(error(Kind::UnknownHandle { slot: *slot }));
                };
                if leaf.resource != resource || leaf.attribute != attribute {
                    return Err(error(Kind::ForeignHandle { slot: *slot }));
                }
                if seen[*slot as usize] {
                    return Err(error(Kind::DuplicateHandle { slot: *slot }));
                }
                seen[*slot as usize] = true;
            }
            Wire::List(values) => {
                for (index, value) in values.iter().enumerate() {
                    path.push(Segment::Index(index));
                    self.validate(value, resource, id, attribute, path, seen)?;
                    path.pop();
                }
            }
            Wire::Map(values) => {
                for (key, value) in values {
                    path.push(Segment::Key(key.clone()));
                    self.validate(value, resource, id, attribute, path, seen)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn restore(
        &self,
        value: &Wire,
        original: Option<&Value>,
        sent: Option<&Wire>,
        path: &mut Vec<Segment>,
        context: &mut AttributeRestoration<'_>,
    ) -> Result<Value, NormalizationError> {
        Ok(match value {
            Wire::Deferred(slot) => self.leaves[*slot as usize].value.clone(),
            Wire::List(values) => {
                let old = match (original, sent) {
                    (Some(Value::Concrete(ConcreteValue::List(old))), Some(Wire::List(sent))) => {
                        Some((old, sent))
                    }
                    _ => None,
                };
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    path.push(Segment::Index(index));
                    result.push(self.restore(
                        value,
                        old.and_then(|(values, _)| values.get(index)),
                        old.and_then(|(_, values)| values.get(index)),
                        path,
                        context,
                    )?);
                    path.pop();
                }
                // Nested JSON arrays have no StringList tag; retain the original kind.
                if matches!(
                    original,
                    Some(Value::Concrete(ConcreteValue::StringList(_)))
                ) {
                    string_list_or_list(result)
                } else {
                    Value::Concrete(ConcreteValue::List(result))
                }
            }
            Wire::StringList(values) => {
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    path.push(Segment::Index(index));
                    result.push(self.restore(
                        &Wire::String(value.clone()),
                        None,
                        None,
                        path,
                        context,
                    )?);
                    path.pop();
                }
                string_list_or_list(result)
            }
            Wire::Map(values) => {
                let old = match original {
                    Some(Value::Concrete(ConcreteValue::Map(values))) => Some(values),
                    _ => None,
                };
                let sent = match sent {
                    Some(Wire::Map(values)) => Some(values),
                    _ => None,
                };
                let mut result = IndexMap::new();
                let old_keys = old
                    .into_iter()
                    .flat_map(|values| values.keys())
                    .filter(|key| values.contains_key(*key));
                for key in old_keys.chain(
                    values
                        .keys()
                        .filter(|key| !old.is_some_and(|old| old.contains_key(*key))),
                ) {
                    path.push(Segment::Key(key.clone()));
                    result.insert(
                        key.clone(),
                        self.restore(
                            &values[key],
                            old.and_then(|old| old.get(key)),
                            sent.and_then(|sent| sent.get(key)),
                            path,
                            context,
                        )?,
                    );
                    path.pop();
                }
                Value::Concrete(ConcreteValue::Map(result))
            }
            _ => {
                if let Some(original) = context.reuse(value, path) {
                    return Ok(original);
                }
                let value = match value {
                    Wire::Bool(value) => carina_provider_protocol::types::Value::Bool(*value),
                    Wire::Int(value) => carina_provider_protocol::types::Value::Int(*value),
                    Wire::Float(value) => carina_provider_protocol::types::Value::Float(*value),
                    Wire::String(value) => {
                        carina_provider_protocol::types::Value::String(value.clone())
                    }
                    _ => unreachable!(),
                };
                protocol_to_core_value(&value)
                    .map_err(|source| context.error(path).with_source(source))?
            }
        })
    }
}

struct AttributeRestoration<'a> {
    id: &'a carina_provider_protocol::types::ResourceId,
    attribute: &'a str,
    known: Vec<OriginalKnownLeaf>,
}

struct OriginalKnownLeaf {
    path: Vec<Segment>,
    wire: Wire,
    value: Option<Value>,
    same_path: bool,
    movable: bool,
}

impl AttributeRestoration<'_> {
    fn reserve_same_paths(&mut self, returned: &Wire) {
        for leaf in &mut self.known {
            leaf.same_path = wire_matches_at_path(returned, &leaf.path, &leaf.wire);
            leaf.movable = leaf
                .path
                .iter()
                .any(|segment| matches!(segment, Segment::Index(_)))
                || !wire_contains_path(returned, &leaf.path);
        }
    }

    fn reuse(&mut self, value: &Wire, path: &[Segment]) -> Option<Value> {
        // Reserve all unchanged paths before a moved leaf can claim an equal value.
        let index = self
            .known
            .iter()
            .position(|leaf| leaf.value.is_some() && leaf.path == path && leaf.wire == *value)
            .or_else(|| {
                self.known.iter().position(|leaf| {
                    leaf.value.is_some()
                        && !leaf.same_path
                        && leaf.movable
                        && leaf.wire == *value
                        && leaf.path.len() == path.len()
                        && leaf.path.iter().zip(path).all(|(original, returned)| {
                            match (original, returned) {
                                (Segment::Index(_), Segment::Index(_)) => true,
                                (Segment::Key(original), Segment::Key(returned)) => {
                                    original == returned
                                }
                                _ => false,
                            }
                        })
                })
            })?;
        self.known[index].value.take()
    }

    fn error(&self, path: &[Segment]) -> NormalizationError {
        NormalizationError::new(self.id.clone(), self.attribute, Kind::Conversion)
            .with_path(path.to_vec())
    }
}

// Only known leaves enter this pool; deferred payloads stay in the host's table.
// Unchanged paths take priority; leaves below lists and removed map keys may move.
fn collect_known_leaves(
    original: &Value,
    sent: &Wire,
    path: &mut Vec<Segment>,
    known: &mut Vec<OriginalKnownLeaf>,
) {
    match (original, sent) {
        (_, Wire::Deferred(_)) => {}
        (Value::Concrete(ConcreteValue::List(original)), Wire::List(sent)) => {
            for (index, (original, sent)) in original.iter().zip(sent).enumerate() {
                path.push(Segment::Index(index));
                collect_known_leaves(original, sent, path, known);
                path.pop();
            }
        }
        (Value::Concrete(ConcreteValue::Map(original)), Wire::Map(sent)) => {
            for (key, original) in original {
                path.push(Segment::Key(key.clone()));
                collect_known_leaves(original, &sent[key], path, known);
                path.pop();
            }
        }
        (Value::Concrete(ConcreteValue::StringList(original)), Wire::StringList(sent)) => {
            for (index, (original, sent)) in original.iter().zip(sent).enumerate() {
                path.push(Segment::Index(index));
                known.push(OriginalKnownLeaf {
                    path: path.clone(),
                    wire: Wire::String(sent.clone()),
                    value: Some(Value::Concrete(ConcreteValue::String(original.clone()))),
                    same_path: false,
                    movable: false,
                });
                path.pop();
            }
        }
        _ => known.push(OriginalKnownLeaf {
            path: path.clone(),
            wire: sent.clone(),
            value: Some(original.clone()),
            same_path: false,
            movable: false,
        }),
    }
}

fn wire_contains_path(value: &Wire, path: &[Segment]) -> bool {
    match path.split_first() {
        None => true,
        Some((Segment::Key(key), rest)) => match value {
            Wire::Map(values) => values
                .get(key)
                .is_some_and(|value| wire_contains_path(value, rest)),
            _ => false,
        },
        Some((Segment::Index(index), rest)) => match value {
            Wire::List(values) => values
                .get(*index)
                .is_some_and(|value| wire_contains_path(value, rest)),
            Wire::StringList(values) => rest.is_empty() && *index < values.len(),
            _ => false,
        },
    }
}

fn wire_matches_at_path(value: &Wire, path: &[Segment], expected: &Wire) -> bool {
    match path.split_first() {
        None => value == expected,
        Some((Segment::Key(key), rest)) => match value {
            Wire::Map(values) => values
                .get(key)
                .is_some_and(|value| wire_matches_at_path(value, rest, expected)),
            _ => false,
        },
        Some((Segment::Index(index), rest)) => match value {
            Wire::List(values) => values
                .get(*index)
                .is_some_and(|value| wire_matches_at_path(value, rest, expected)),
            Wire::StringList(values) if rest.is_empty() => values.get(*index).is_some_and(
                |value| matches!(expected, Wire::String(expected) if value == expected),
            ),
            _ => false,
        },
    }
}

fn string_list_or_list(values: Vec<Value>) -> Value {
    if values
        .iter()
        .all(|value| matches!(value, Value::Concrete(ConcreteValue::String(_))))
    {
        Value::Concrete(ConcreteValue::StringList(
            values
                .into_iter()
                .map(|value| match value {
                    Value::Concrete(ConcreteValue::String(value)) => value,
                    _ => unreachable!(),
                })
                .collect(),
        ))
    } else {
        Value::Concrete(ConcreteValue::List(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ConcreteValue, DeferredValue};
    use carina_provider_protocol::normalization::{NormalizationErrorKind, NormalizationValue};

    fn fixture() -> Vec<Resource> {
        vec![Resource::new("mock", "example").with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("never-in-wire".into()),
                )))),
                Value::Concrete(ConcreteValue::String("known".into())),
            ])),
        )]
    }

    #[test]
    fn leaf_projection_restores_reordered_handles_without_exposing_secrets() {
        let original = fixture();
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        assert!(!format!("{returned:?}").contains("never-in-wire"));
        let NormalizationValue::List(items) = returned[0].attributes.get_mut("rules").unwrap()
        else {
            panic!()
        };
        assert!(matches!(items[0], NormalizationValue::Deferred(0)));
        items.swap(0, 1);
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = &restored[0]["rules"] else {
            panic!()
        };
        assert!(matches!(
            items[0],
            Value::Concrete(ConcreteValue::String(_))
        ));
        assert!(matches!(
            items[1],
            Value::Deferred(DeferredValue::Secret(_))
        ));
    }

    #[test]
    fn handles_must_occur_exactly_once_in_the_original_resource_and_attribute() {
        for case in 0..5 {
            let mut original = fixture();
            original.push(Resource::new("mock", "example")); // deliberately duplicate id
            let batch = NormalizationBatch::new(&original).unwrap();
            let mut returned = batch.resources().to_vec();
            match case {
                0 => {
                    returned[0].attributes.remove("rules");
                }
                1 => {
                    returned[0].attributes.insert(
                        "rules".into(),
                        NormalizationValue::List(vec![
                            NormalizationValue::Deferred(0),
                            NormalizationValue::Deferred(0),
                        ]),
                    );
                }
                2 => {
                    returned[0]
                        .attributes
                        .insert("rules".into(), NormalizationValue::Deferred(10));
                }
                3 => {
                    let value = returned[0].attributes.remove("rules").unwrap();
                    returned[0].attributes.insert("other".into(), value);
                }
                4 => {
                    let value = returned[0].attributes.remove("rules").unwrap();
                    returned[1].attributes.insert("rules".into(), value);
                }
                _ => unreachable!(),
            }
            let error = batch.finish(returned).unwrap_err();
            assert!(match case {
                0 => matches!(error.kind, NormalizationErrorKind::MissingHandle { .. }),
                1 => matches!(error.kind, NormalizationErrorKind::DuplicateHandle { .. }),
                2 => matches!(error.kind, NormalizationErrorKind::UnknownHandle { .. }),
                _ => matches!(error.kind, NormalizationErrorKind::ForeignHandle { .. }),
            });
            assert!(!error.attribute.is_empty());
            assert!(error.to_string().contains("example"));
            assert!(!format!("{error:?}").contains("never-in-wire"));
        }
    }

    #[test]
    fn reordered_known_leaves_retain_sensitivity_but_edits_are_plain() {
        use crate::resource::ValueString;
        let original = vec![Resource::new("test", "sensitive").with_attribute(
            "items",
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                    "sensitive",
                ))),
                Value::Concrete(ConcreteValue::String("plain".into())),
            ])),
        )];
        for edit in [false, true] {
            let batch = NormalizationBatch::new(&original).unwrap();
            let mut returned = batch.resources().to_vec();
            let Wire::List(items) = returned[0].attributes.get_mut("items").unwrap() else {
                panic!()
            };
            items.swap(0, 1);
            if edit {
                items[1] = Wire::String("edited".into());
            }
            let restored = batch.finish(returned).unwrap();
            let Value::Concrete(ConcreteValue::List(items)) = &restored[0]["items"] else {
                panic!()
            };
            let Value::Concrete(ConcreteValue::String(value)) = &items[1] else {
                panic!()
            };
            assert_eq!(value.is_diagnostic_sensitive(), !edit, "edit={edit}");
            assert_eq!(
                value.expose_plaintext(),
                if edit { "edited" } else { "sensitive" }
            );
        }
    }

    #[test]
    fn unchanged_sensitive_leaf_keeps_its_own_metadata_after_an_equal_sibling_is_edited() {
        use crate::resource::ValueString;
        for string_list in [false, true] {
            let values = vec!["x".into(), ValueString::diagnostic_sensitive("x")];
            let value = if string_list {
                ConcreteValue::StringList(values)
            } else {
                ConcreteValue::List(
                    values
                        .into_iter()
                        .map(|value| Value::Concrete(ConcreteValue::String(value)))
                        .collect(),
                )
            };
            let original = vec![
                Resource::new("test", "sensitivity")
                    .with_attribute("items", Value::Concrete(value)),
            ];
            let batch = NormalizationBatch::new(&original).unwrap();
            let mut returned = batch.resources().to_vec();
            match returned[0].attributes.get_mut("items").unwrap() {
                Wire::List(values) => values[0] = Wire::String("z".into()),
                Wire::StringList(values) => values[0] = "z".into(),
                _ => panic!(),
            }
            let restored = batch.finish(returned).unwrap();
            let value = match &restored[0]["items"] {
                Value::Concrete(ConcreteValue::List(values)) => match &values[1] {
                    Value::Concrete(ConcreteValue::String(value)) => value,
                    _ => panic!(),
                },
                Value::Concrete(ConcreteValue::StringList(values)) => &values[1],
                _ => panic!(),
            };
            assert!(value.is_diagnostic_sensitive(), "string_list={string_list}");
        }
    }

    #[test]
    fn an_edited_label_cannot_claim_an_unchanged_enums_metadata() {
        use crate::resource::EnumValueResolver;
        use crate::schema::{CompiledAttributeType, TypeIdentity};
        let ty = CompiledAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["Example"], "Mode"),
            Some(vec!["ENABLED".into()]),
            Vec::new(),
            None,
            None,
        );
        let mode = EnumValueResolver::new(&ty)
            .resolve_state_text("ENABLED")
            .unwrap();
        let original = vec![Resource::new("test", "enums").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "label".into(),
                    Value::Concrete(ConcreteValue::String("enabled".into())),
                ),
                (
                    "mode".into(),
                    Value::Concrete(ConcreteValue::CanonicalEnum(mode)),
                ),
            ]))),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::Map(values) = returned[0].attributes.get_mut("settings").unwrap() else {
            panic!()
        };
        values.insert("label".into(), Wire::String("ENABLED".into()));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = &restored[0]["settings"] else {
            panic!()
        };
        assert!(matches!(
            values["label"],
            Value::Concrete(ConcreteValue::String(_))
        ));
        assert!(matches!(
            values["mode"],
            Value::Concrete(ConcreteValue::CanonicalEnum(_))
        ));
    }

    #[test]
    fn an_unchanged_int_cannot_receive_an_edited_durations_metadata() {
        let original = vec![Resource::new("test", "durations").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "a".into(),
                    Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60))),
                ),
                ("b".into(), Value::Concrete(ConcreteValue::Int(60))),
            ]))),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::Map(values) = returned[0].attributes.get_mut("settings").unwrap() else {
            panic!()
        };
        values.insert("a".into(), Wire::Int(90));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = &restored[0]["settings"] else {
            panic!()
        };
        assert!(matches!(
            values["a"],
            Value::Concrete(ConcreteValue::Int(90))
        ));
        assert!(matches!(
            values["b"],
            Value::Concrete(ConcreteValue::Int(60))
        ));
    }

    #[test]
    fn original_known_leaf_metadata_is_consumed_once_and_scoped_to_its_attribute() {
        use crate::resource::ValueString;
        let original = vec![
            Resource::new("test", "sensitive")
                .with_attribute(
                    "items",
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String(ValueString::diagnostic_sensitive("same")),
                    )])),
                )
                .with_attribute(
                    "other",
                    Value::Concrete(ConcreteValue::String("before".into())),
                ),
        ];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        returned[0].attributes.insert(
            "items".into(),
            Wire::List(vec![
                Wire::String("same".into()),
                Wire::String("same".into()),
            ]),
        );
        returned[0]
            .attributes
            .insert("other".into(), Wire::String("same".into()));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = &restored[0]["items"] else {
            panic!()
        };
        let sensitive = |value: &Value| match value {
            Value::Concrete(ConcreteValue::String(value)) => value.is_diagnostic_sensitive(),
            _ => panic!(),
        };
        assert!(sensitive(&items[0]));
        assert!(!sensitive(&items[1]));
        assert!(!sensitive(&restored[0]["other"]));
    }

    #[test]
    fn finish_and_finish_with_reject_swapped_resource_ids_without_deferred_leaves() {
        let original = vec![
            Resource::new("test", "first")
                .with_attribute("known", Value::Concrete(ConcreteValue::Int(1))),
            Resource::new("test", "second")
                .with_attribute("known", Value::Concrete(ConcreteValue::Int(2))),
        ];
        for custom_decode in [false, true] {
            let batch = NormalizationBatch::new(&original).unwrap();
            let expected = batch.resources()[0].id.clone();
            let actual = batch.resources()[1].id.clone();
            let mut returned = batch.resources().to_vec();
            returned.swap(0, 1);
            let error = if custom_decode {
                batch.finish_with(returned, |_, resource| Ok(resource))
            } else {
                batch.finish(returned)
            }
            .unwrap_err();
            assert!(
                matches!(error.kind, Kind::ResourceIdMismatch { expected: ref sent, actual: ref returned } if **sent == expected && **returned == actual)
            );
            assert_eq!(*error.resource_id, expected);
            assert_eq!(error.resource_index, Some(0));
            assert_eq!(error.attribute, "known");
        }
    }

    #[test]
    fn a_renamed_known_map_key_does_not_inherit_original_scalar_metadata() {
        use crate::resource::ValueString;
        let original = vec![Resource::new("test", "map-move").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "old".into(),
                Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                    "moved",
                ))),
            )]))),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::Map(values) = returned[0].attributes.get_mut("settings").unwrap() else {
            panic!()
        };
        let value = values.remove("old").unwrap();
        values.insert("new".into(), value);
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = &restored[0]["settings"] else {
            panic!()
        };
        assert!(!values.contains_key("old"));
        let Value::Concrete(ConcreteValue::String(value)) = &values["new"] else {
            panic!()
        };
        assert!(!value.is_diagnostic_sensitive());
        assert_eq!(value.expose_plaintext(), "moved");
    }

    #[test]
    fn edited_map_values_cannot_donate_duration_metadata_to_another_key() {
        let original = vec![Resource::new("test", "durations").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "timeout".into(),
                    Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60))),
                ),
                ("retries".into(), Value::Concrete(ConcreteValue::Int(3))),
            ]))),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::Map(values) = returned[0].attributes.get_mut("settings").unwrap() else {
            panic!()
        };
        values.insert("timeout".into(), Wire::Int(90));
        values.insert("retries".into(), Wire::Int(60));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = &restored[0]["settings"] else {
            panic!()
        };
        assert!(matches!(
            values["timeout"],
            Value::Concrete(ConcreteValue::Int(90))
        ));
        assert!(matches!(
            values["retries"],
            Value::Concrete(ConcreteValue::Int(60))
        ));
    }

    #[test]
    fn edited_map_values_cannot_donate_enum_metadata_to_an_inserted_key() {
        use crate::resource::EnumValueResolver;
        use crate::schema::{CompiledAttributeType, TypeIdentity};
        let ty = CompiledAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["Example"], "Mode"),
            Some(vec!["ENABLED".into(), "DISABLED".into()]),
            Vec::new(),
            None,
            None,
        );
        let mode = EnumValueResolver::new(&ty)
            .resolve_state_text("ENABLED")
            .unwrap();
        let original = vec![Resource::new("test", "enums").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "mode".into(),
                Value::Concrete(ConcreteValue::CanonicalEnum(mode)),
            )]))),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::Map(values) = returned[0].attributes.get_mut("settings").unwrap() else {
            panic!()
        };
        values.insert("mode".into(), Wire::String("DISABLED".into()));
        values.insert("label".into(), Wire::String("ENABLED".into()));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = &restored[0]["settings"] else {
            panic!()
        };
        assert!(matches!(
            values["mode"],
            Value::Concrete(ConcreteValue::String(_))
        ));
        assert!(matches!(
            values["label"],
            Value::Concrete(ConcreteValue::String(_))
        ));
    }

    #[test]
    fn reordering_maps_in_a_list_preserves_nested_enum_and_sensitive_leaves() {
        use crate::resource::{EnumValueResolver, ValueString};
        use crate::schema::{CompiledAttributeType, TypeIdentity};
        let ty = CompiledAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["Rule"], "Status"),
            Some(vec!["Enabled".into(), "Disabled".into()]),
            Vec::new(),
            None,
            None,
        );
        let rules = [("Enabled", "a"), ("Disabled", "b")]
            .into_iter()
            .map(|(status, token)| {
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "status".into(),
                        Value::Concrete(ConcreteValue::CanonicalEnum(
                            EnumValueResolver::new(&ty)
                                .resolve_state_text(status)
                                .unwrap(),
                        )),
                    ),
                    (
                        "token".into(),
                        Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(
                            token,
                        ))),
                    ),
                ])))
            })
            .collect();
        let original = vec![
            Resource::new("test", "rules")
                .with_attribute("rules", Value::Concrete(ConcreteValue::List(rules))),
        ];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::List(rules) = returned[0].attributes.get_mut("rules").unwrap() else {
            panic!()
        };
        let moved = rules.remove(1);
        rules.insert(0, moved); // DesiredList::move_within(1, 0)'s wire result.
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::List(rules)) = &restored[0]["rules"] else {
            panic!()
        };
        for (rule, (status, token)) in rules.iter().zip([("Disabled", "b"), ("Enabled", "a")]) {
            let Value::Concrete(ConcreteValue::Map(rule)) = rule else {
                panic!()
            };
            let Value::Concrete(ConcreteValue::CanonicalEnum(value)) = &rule["status"] else {
                panic!("moved status lost its enum kind")
            };
            assert_eq!(value.api_value(), status);
            let Value::Concrete(ConcreteValue::String(value)) = &rule["token"] else {
                panic!()
            };
            assert_eq!(value.expose_plaintext(), token);
            assert!(
                value.is_diagnostic_sensitive(),
                "moved token lost its sensitivity"
            );
        }
    }

    #[test]
    fn native_question_mark_conversion_keeps_the_typed_cause_and_resource_id() {
        use crate::provider::ProviderResult;
        use std::error::Error;
        fn normalize(resource: &Resource) -> ProviderResult<()> {
            let batch = NormalizationBatch::new(std::slice::from_ref(resource))?;
            batch.finish(Vec::new())?;
            Ok(())
        }
        for invalid_scalar in [false, true] {
            let resource = Resource::new("test", "native-error").with_attribute(
                "value",
                Value::Concrete(if invalid_scalar {
                    ConcreteValue::Float(f64::NAN)
                } else {
                    ConcreteValue::Int(42)
                }),
            );
            let error = normalize(&resource).unwrap_err();
            let source = error
                .detail()
                .cause()
                .unwrap()
                .downcast_ref::<NormalizationError>()
                .expect("cause must retain the normalization error type");
            assert!(error.source().is_none(), "provider errors remain terminal");
            assert_eq!(error.detail().resource_id.as_deref(), Some(&resource.id));
            assert_eq!(source.attribute, "value");
            if invalid_scalar {
                assert!(
                    source
                        .source()
                        .unwrap()
                        .is::<crate::protocol_convert::ProtocolConvertError>()
                );
            } else {
                assert!(matches!(
                    source.kind,
                    Kind::ResourceCountMismatch {
                        expected: 1,
                        actual: 0
                    }
                ));
            }
        }
    }

    #[test]
    fn normalization_conversion_cause_is_rendered_exactly_once() {
        use crate::protocol_convert::ProtocolConvertError;
        let resource = Resource::new("test", "float-error")
            .with_attribute("value", Value::Concrete(ConcreteValue::Float(f64::NAN)));
        let error = || {
            NormalizationBatch::new(std::slice::from_ref(&resource))
                .err()
                .unwrap()
        };
        let rendered = crate::provider::ProviderError::from(error()).to_string();
        let float_message = ProtocolConvertError::NonFiniteFloat.to_string();
        assert_eq!(rendered.matches(&float_message).count(), 1, "{rendered}");
        let wire: carina_provider_protocol::types::ProviderError = error().into();
        let rendered = format!("{}: {}", wire.message, wire.cause.unwrap());
        assert_eq!(rendered.matches(&float_message).count(), 1, "{rendered}");
    }

    #[test]
    fn list_map_metadata_cannot_move_between_different_keys() {
        use crate::resource::EnumValueResolver;
        use crate::schema::{CompiledAttributeType, TypeIdentity};
        let ty = CompiledAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["Rule"], "Mode"),
            Some(vec!["ENABLED".into(), "DISABLED".into()]),
            Vec::new(),
            None,
            None,
        );
        let mode = EnumValueResolver::new(&ty)
            .resolve_state_text("ENABLED")
            .unwrap();
        let original = vec![Resource::new("test", "rules").with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([
                    (
                        "label".into(),
                        Value::Concrete(ConcreteValue::String("a".into())),
                    ),
                    (
                        "mode".into(),
                        Value::Concrete(ConcreteValue::CanonicalEnum(mode)),
                    ),
                ])),
            )])),
        )];
        let batch = NormalizationBatch::new(&original).unwrap();
        let mut returned = batch.resources().to_vec();
        let Wire::List(rules) = returned[0].attributes.get_mut("rules").unwrap() else {
            panic!()
        };
        let Wire::Map(rule) = &mut rules[0] else {
            panic!()
        };
        rule.insert("label".into(), Wire::String("ENABLED".into()));
        rule.insert("mode".into(), Wire::String("DISABLED".into()));
        let restored = batch.finish(returned).unwrap();
        let Value::Concrete(ConcreteValue::List(rules)) = &restored[0]["rules"] else {
            panic!()
        };
        let Value::Concrete(ConcreteValue::Map(rule)) = &rules[0] else {
            panic!()
        };
        assert!(matches!(
            rule["label"],
            Value::Concrete(ConcreteValue::String(_))
        ));
        assert!(matches!(
            rule["mode"],
            Value::Concrete(ConcreteValue::String(_))
        ));
    }

    #[test]
    fn resource_count_mismatch_is_typed_and_contextual() {
        let original = fixture();
        let batch = NormalizationBatch::new(&original).unwrap();
        let error = batch.finish(Vec::new()).unwrap_err();
        assert!(matches!(
            error.kind,
            NormalizationErrorKind::ResourceCountMismatch {
                expected: 1,
                actual: 0
            }
        ));
        assert_eq!(error.attribute, "rules");
        assert!(error.to_string().contains("example"));
    }

    #[test]
    fn secret_containers_and_other_deferred_expressions_remain_single_opaque_leaves() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "private-key".into(),
                Value::Concrete(ConcreteValue::String("private-content".into())),
            )])),
        ))));
        let expression = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::Concrete(ConcreteValue::String(
                "private-expression".into(),
            ))),
            default: Box::new(Value::Concrete(ConcreteValue::Bool(false))),
        });
        let resource = Resource::new("test", "opaque")
            .with_attribute("secret", secret)
            .with_attribute("expression", expression);
        let batch = NormalizationBatch::new(&[resource]).unwrap();
        assert!(
            batch.resources()[0]
                .attributes
                .values()
                .all(|value| matches!(value, NormalizationValue::Deferred(_)))
        );
        assert!(!format!("{:?}", batch.resources()).contains("private-"));
        let returned = batch.resources().to_vec();
        let restored = batch.finish(returned).unwrap();
        assert!(matches!(
            restored[0]["secret"],
            Value::Deferred(DeferredValue::Secret(_))
        ));
        assert!(matches!(
            restored[0]["expression"],
            Value::Deferred(DeferredValue::Coalesce { .. })
        ));
    }
}
