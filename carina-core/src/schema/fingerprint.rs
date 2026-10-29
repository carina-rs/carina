//! Canonical provider-schema fingerprints used by saved plans.
//!
//! This encoder is deliberately separate from the provider wire JSON. The
//! in-memory schema contains hash maps and host-only fields, so serializing it
//! incidentally would make fingerprints depend on insertion order or silently
//! omit fields. Every semantic schema struct and enum is destructured
//! exhaustively here; adding a field or variant forces this module to decide
//! whether it belongs in the fingerprint.

use sha2::{Digest, Sha256};

use super::{
    AttrTypeKind, AttributeSchema, AttributeType, DslTransform, InputMode, ResourceSchema,
    ResourceValidation, SchemaKind, StructField, TypeIdentity, UniqueNameSpec, ValidatorType,
};
use crate::resource::Value;

const ENCODING_VERSION: &str = "carina-resource-schema-semantic-v4";

#[derive(Default)]
struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn tag(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.tag(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) {
        self.u64(value.try_into().expect("schema collection length fits u64"));
    }

    fn bytes(&mut self, value: &[u8]) {
        self.usize(value.len());
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn option<T>(&mut self, value: Option<T>, encode: impl FnOnce(&mut Self, T)) {
        match value {
            Some(value) => {
                self.tag(1);
                encode(self, value);
            }
            None => self.tag(0),
        }
    }

    fn strings(&mut self, values: &[String]) {
        self.usize(values.len());
        for value in values {
            self.string(value);
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl ResourceSchema {
    /// SHA-256 of the canonical planning/apply semantics of this schema.
    ///
    /// Documentation and completion text are intentionally excluded. They do
    /// not affect planning or provider calls. Executable validator callbacks
    /// have no portable cross-process identity, so a fixed marker is encoded;
    /// all declarative validator inputs and refinements are encoded normally.
    pub fn semantic_fingerprint(&self) -> String {
        hex::encode(Sha256::digest(self.semantic_encoding()))
    }

    fn semantic_encoding(&self) -> Vec<u8> {
        let mut encoder = CanonicalEncoder::default();
        encoder.string(ENCODING_VERSION);
        encode_resource_schema(&mut encoder, self);
        encoder.finish()
    }
}

fn encode_resource_schema(encoder: &mut CanonicalEncoder, schema: &ResourceSchema) {
    let ResourceSchema {
        resource_type,
        attributes,
        description: _description,
        validation,
        kind,
        unique_name,
        operation_config,
        exclusive_required,
        default_wait_timeout,
        default_wait_interval,
        defs,
        duplicate_attributes: _duplicate_attributes,
    } = schema;

    encoder.string(resource_type);
    encode_schema_kind(encoder, *kind);

    encoder.usize(attributes.len());
    for (key, attribute) in attributes {
        encoder.string(key);
        encode_attribute_schema(encoder, attribute);
    }

    encode_resource_validation(encoder, validation);
    encode_unique_name(encoder, unique_name);
    encoder.option(operation_config.as_ref(), encode_operation_config);

    let mut exclusive_required = exclusive_required
        .iter()
        .map(|group| {
            let mut group = group.clone();
            group.sort();
            group
        })
        .collect::<Vec<_>>();
    exclusive_required.sort();
    encoder.usize(exclusive_required.len());
    for group in exclusive_required {
        encoder.strings(&group);
    }

    encoder.option(default_wait_timeout.as_ref(), |encoder, duration| {
        encode_duration(encoder, *duration);
    });
    encoder.option(default_wait_interval.as_ref(), |encoder, duration| {
        encode_duration(encoder, *duration);
    });

    encoder.usize(defs.len());
    for (name, attribute_type) in defs {
        encoder.string(name);
        encode_attribute_type(encoder, attribute_type);
    }
}

fn encode_resource_validation(encoder: &mut CanonicalEncoder, validation: &ResourceValidation) {
    match validation {
        ResourceValidation::None => encoder.tag(0),
        ResourceValidation::Declared(validators) => {
            encoder.tag(1);
            let mut validators = validators
                .iter()
                .map(|validator| {
                    let mut validator_encoder = CanonicalEncoder::default();
                    encode_validator_type(&mut validator_encoder, validator);
                    validator_encoder.finish()
                })
                .collect::<Vec<_>>();
            validators.sort();
            validators.dedup();
            encoder.usize(validators.len());
            for validator in validators {
                encoder.bytes(&validator);
            }
        }
    }
}

fn encode_validator_type(encoder: &mut CanonicalEncoder, validator: &ValidatorType) {
    match validator {
        ValidatorType::TagsKeyValueCheck => encoder.tag(0),
    }
}

fn encode_attribute_schema(encoder: &mut CanonicalEncoder, schema: &AttributeSchema) {
    let AttributeSchema {
        name,
        attr_type,
        input_mode,
        default,
        description: _description,
        completions: _completions,
        provider_name,
        create_only,
        removable,
        block_name,
        write_only,
        identity,
        deferred_populate,
    } = schema;

    encoder.string(name);
    encode_attribute_type(encoder, attr_type);
    encode_input_mode(encoder, *input_mode);
    encoder.option(default.as_ref(), encode_default_value);
    encoder.option(provider_name.as_deref(), CanonicalEncoder::string);
    encoder.bool(*create_only);
    encoder.option(*removable, CanonicalEncoder::bool);
    encoder.option(block_name.as_deref(), CanonicalEncoder::string);
    encoder.bool(*write_only);
    encoder.bool(*identity);
    encoder.bool(*deferred_populate);
}

fn encode_attribute_type(encoder: &mut CanonicalEncoder, attribute_type: &AttributeType) {
    let AttributeType { kind } = attribute_type;
    match kind {
        AttrTypeKind::String {
            binding,
            pattern,
            length,
            to_dsl,
        } => {
            encoder.tag(0);
            encoder.option(binding.identity(), encode_type_identity);
            encoder.option(pattern.as_deref(), CanonicalEncoder::string);
            encode_optional_u64_range(encoder, *length);
            encode_callback_presence(encoder, binding.callback().is_some());
            encoder.option(to_dsl.as_ref(), encode_dsl_transform);
        }
        AttrTypeKind::ResourceHandle {
            provider,
            resource_type,
        } => {
            encoder.tag(1);
            encoder.string(provider);
            encoder.string(resource_type);
            encode_callback_presence(encoder, false);
        }
        AttrTypeKind::StringLiteralSet { values } => {
            encoder.tag(2);
            let mut values = values.clone();
            values.sort();
            values.dedup();
            encoder.strings(&values);
            encode_callback_presence(encoder, false);
        }
        AttrTypeKind::Int { binding, range } => {
            encoder.tag(3);
            encoder.option(binding.identity(), encode_type_identity);
            encode_optional_i64_range(encoder, *range);
            encode_callback_presence(encoder, binding.callback().is_some());
        }
        AttrTypeKind::Float { binding, range } => {
            encoder.tag(4);
            encoder.option(binding.identity(), encode_type_identity);
            encoder.option(*range, |encoder, (min, max)| {
                encoder.option(min, |encoder, value| encoder.u64(value.to_bits()));
                encoder.option(max, |encoder, value| encoder.u64(value.to_bits()));
            });
            encode_callback_presence(encoder, binding.callback().is_some());
        }
        AttrTypeKind::Bool => encoder.tag(5),
        AttrTypeKind::Duration => encoder.tag(6),
        AttrTypeKind::Enum {
            identity,
            base,
            values,
            dsl_aliases,
            validate,
            to_dsl,
        } => {
            encoder.tag(7);
            encode_type_identity(encoder, identity);
            encode_attribute_type(encoder, base);
            encoder.option(values.as_ref(), |encoder, values| {
                let mut values = values.clone();
                values.sort();
                values.dedup();
                encoder.strings(&values);
            });
            let mut aliases = dsl_aliases.clone();
            aliases.sort();
            encoder.usize(aliases.len());
            for (api, dsl) in aliases {
                encoder.string(&api);
                encoder.string(&dsl);
            }
            encode_callback_presence(encoder, validate.is_some());
            encoder.option(to_dsl.as_ref(), encode_dsl_transform);
        }
        AttrTypeKind::List {
            element_type,
            ordered,
            length,
        } => {
            encoder.tag(8);
            encode_attribute_type(encoder, element_type);
            encoder.bool(*ordered);
            encode_optional_u64_range(encoder, *length);
            encode_callback_presence(encoder, false);
        }
        AttrTypeKind::Map { key, value } => {
            encoder.tag(9);
            encode_attribute_type(encoder, key);
            encode_attribute_type(encoder, value);
        }
        AttrTypeKind::Struct { name, fields } => {
            encoder.tag(10);
            encoder.string(name);
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by(|left, right| left.name.cmp(&right.name));
            encoder.usize(fields.len());
            for field in fields {
                encode_struct_field(encoder, field);
            }
        }
        AttrTypeKind::Union(members) => {
            encoder.tag(11);
            encoder.usize(members.as_raw_slice().len());
            for member in members.as_raw_slice() {
                encode_attribute_type(encoder, member);
            }
        }
        AttrTypeKind::Ref(name) => {
            encoder.tag(12);
            encoder.string(name);
        }
    }
}

fn encode_callback_presence(encoder: &mut CanonicalEncoder, present: bool) {
    match present {
        false => encoder.tag(0),
        true => encoder.tag(1),
    }
}

fn encode_struct_field(encoder: &mut CanonicalEncoder, field: &StructField) {
    let StructField {
        name,
        field_type,
        declared_input_mode,
        description: _description,
        provider_name,
        block_name,
        deferred_populate,
        write_only,
    } = field;
    encoder.string(name);
    encode_attribute_type(encoder, field_type);
    encode_input_mode(encoder, *declared_input_mode);
    encoder.option(provider_name.as_deref(), CanonicalEncoder::string);
    encoder.option(block_name.as_deref(), CanonicalEncoder::string);
    encoder.bool(*deferred_populate);
    encoder.bool(*write_only);
}

fn encode_schema_kind(encoder: &mut CanonicalEncoder, kind: SchemaKind) {
    match kind {
        SchemaKind::Resource => encoder.tag(0),
        SchemaKind::DataSource => encoder.tag(1),
    }
}

fn encode_input_mode(encoder: &mut CanonicalEncoder, mode: InputMode) {
    match mode {
        InputMode::Optional => encoder.tag(0),
        InputMode::Required => encoder.tag(1),
        InputMode::ProviderPopulated => encoder.tag(2),
    }
}

fn encode_unique_name(encoder: &mut CanonicalEncoder, unique_name: &UniqueNameSpec) {
    match unique_name {
        UniqueNameSpec::Attribute(attribute) => {
            encoder.tag(0);
            encoder.string(attribute);
        }
        UniqueNameSpec::Coexisting => encoder.tag(1),
        UniqueNameSpec::Conflicting => encoder.tag(2),
    }
}

fn encode_operation_config(
    encoder: &mut CanonicalEncoder,
    config: &carina_provider_protocol::OperationConfig,
) {
    let carina_provider_protocol::OperationConfig {
        delete_timeout_secs,
        delete_max_retries,
        create_timeout_secs,
        create_max_retries,
        update_timeout_secs,
    } = config;
    encoder.option(*delete_timeout_secs, CanonicalEncoder::u64);
    encoder.option(*delete_max_retries, CanonicalEncoder::u32);
    encoder.option(*create_timeout_secs, CanonicalEncoder::u64);
    encoder.option(*create_max_retries, CanonicalEncoder::u32);
    encoder.option(*update_timeout_secs, CanonicalEncoder::u64);
}

fn encode_type_identity(encoder: &mut CanonicalEncoder, identity: &TypeIdentity) {
    let TypeIdentity {
        provider,
        segments,
        kind,
    } = identity;
    encoder.option(provider.as_deref(), CanonicalEncoder::string);
    encoder.strings(segments);
    encoder.string(kind);
}

fn encode_dsl_transform(encoder: &mut CanonicalEncoder, transform: &DslTransform) {
    match transform {
        DslTransform::Identity => encoder.tag(0),
        DslTransform::HyphenToUnderscore => encoder.tag(1),
        DslTransform::StripSuffix(suffix) => {
            encoder.tag(2);
            encoder.string(suffix);
        }
        DslTransform::ReplaceTable(table) => {
            encoder.tag(3);
            encoder.usize(table.len());
            for (source, replacement) in table {
                encoder.string(source);
                encoder.string(replacement);
            }
        }
        DslTransform::Unknown(value) => {
            encoder.tag(4);
            encode_json(encoder, value);
        }
    }
}

fn encode_optional_u64_range(
    encoder: &mut CanonicalEncoder,
    range: Option<(Option<u64>, Option<u64>)>,
) {
    encoder.option(range, |encoder, (min, max)| {
        encoder.option(min, CanonicalEncoder::u64);
        encoder.option(max, CanonicalEncoder::u64);
    });
}

fn encode_optional_i64_range(
    encoder: &mut CanonicalEncoder,
    range: Option<(Option<i64>, Option<i64>)>,
) {
    encoder.option(range, |encoder, (min, max)| {
        encoder.option(min, CanonicalEncoder::i64);
        encoder.option(max, CanonicalEncoder::i64);
    });
}

fn encode_duration(encoder: &mut CanonicalEncoder, duration: std::time::Duration) {
    encoder.u64(duration.as_secs());
    encoder.u32(duration.subsec_nanos());
}

fn encode_default_value(encoder: &mut CanonicalEncoder, value: &Value) {
    match serde_json::to_value(value) {
        Ok(value) => {
            encoder.tag(0);
            encode_json(encoder, &value);
        }
        Err(error) => {
            // Invalid schema defaults should already be rejected elsewhere.
            // Keep the fingerprint total and deterministic so diagnostics do
            // not turn into a panic at plan-save time.
            encoder.tag(1);
            encoder.string(&error.to_string());
        }
    }
}

fn encode_json(encoder: &mut CanonicalEncoder, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => encoder.tag(0),
        serde_json::Value::Bool(value) => {
            encoder.tag(1);
            encoder.bool(*value);
        }
        serde_json::Value::Number(value) => {
            encoder.tag(2);
            encoder.string(&value.to_string());
        }
        serde_json::Value::String(value) => {
            encoder.tag(3);
            encoder.string(value);
        }
        serde_json::Value::Array(values) => {
            encoder.tag(4);
            encoder.usize(values.len());
            for value in values {
                encode_json(encoder, value);
            }
        }
        serde_json::Value::Object(values) => {
            encoder.tag(5);
            let mut values = values.iter().collect::<Vec<_>>();
            values.sort_by_key(|(key, _)| *key);
            encoder.usize(values.len());
            for (key, value) in values {
                encoder.string(key);
                encode_json(encoder, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute_type_encoding(attribute_type: AttributeType) -> Vec<u8> {
        let mut encoder = CanonicalEncoder::default();
        encode_attribute_type(&mut encoder, &attribute_type);
        encoder.finish()
    }

    #[test]
    fn callback_presence_markers_reflect_the_new_binding_model() {
        assert_eq!(
            attribute_type_encoding(AttributeType::string()),
            vec![0, 0, 0, 0, 0, 0],
            "String encodes validate before to_dsl"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::resource_handle("", "")),
            [vec![1], vec![0; 16], vec![0]].concat(),
            "ResourceHandle has no callback"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::string_literal_set(Vec::new())),
            [vec![2], vec![0; 8], vec![0]].concat(),
            "StringLiteralSet has no callback"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::int()),
            vec![3, 0, 0, 0],
            "Int encodes validate"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::float()),
            vec![4, 0, 0, 0],
            "Float encodes validate"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::list(AttributeType::bool())),
            vec![8, 5, 1, 0, 0],
            "List has no callback"
        );
    }

    #[test]
    fn duplicate_declared_validators_do_not_change_the_fingerprint() {
        let once = ResourceSchema::new("test.Widget")
            .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck]);
        let repeated = ResourceSchema::new("test.Widget").with_declared_validators(vec![
            ValidatorType::TagsKeyValueCheck,
            ValidatorType::TagsKeyValueCheck,
        ]);

        assert_eq!(once.semantic_fingerprint(), repeated.semantic_fingerprint());
    }
}
