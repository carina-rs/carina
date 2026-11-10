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
    AssignabilityEdge, AttrTypeKind, CharSet, Compiled, CompiledAttributeSchema as AttributeSchema,
    CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema,
    CompiledStructField as StructField, DslTransform, FieldGroup, FieldGroupKind, IdentityMode,
    InputCondition, InputLiteral, InputMode, InputPredicate, InputRule, InputRuleParts,
    InputRuleScope, InputRuleTarget, OwnerBoundResourceSchema, ReferenceForm, ReferenceForms,
    ResourceValidation, SchemaKind, TypeIdentity, UniqueNameSpec, ValidatorType,
};
use crate::resource::Value;

const ENCODING_VERSION: &str = "carina-resource-schema-semantic-v13";

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

fn encode_pattern(encoder: &mut CanonicalEncoder, pattern: &super::PatternSpec) {
    let super::PatternSpecParts { regex, mode } = pattern.parts();
    encoder.string(regex);
    match mode {
        super::PatternMatch::Full => encoder.tag(0),
        super::PatternMatch::Substring { reason } => {
            encoder.tag(1);
            encoder.string(reason.as_str());
        }
    }
}

impl ResourceSchema {
    /// SHA-256 of the canonical planning/apply semantics of this schema.
    ///
    /// Documentation and completion text are intentionally excluded. They do
    /// not affect planning or provider calls. Executable validator callbacks
    /// have no portable cross-process identity, so a fixed marker is encoded;
    /// all declarative validator inputs and refinements are encoded normally.
    #[cfg(test)]
    pub(crate) fn semantic_fingerprint(&self) -> String {
        hex::encode(Sha256::digest(semantic_encoding(
            self,
            &ReferenceForms::new(),
            &[],
        )))
    }
}

impl AttributeType {
    /// Portable provenance for rebinding an exact scalar projection. The
    /// encoding retains enum/reference semantics and callback presence; the
    /// catalog must still reject indistinguishable declarations with different
    /// executable callbacks.
    pub(crate) fn declaration_fingerprint(&self) -> String {
        let mut encoder = CanonicalEncoder::default();
        encoder.string(ENCODING_VERSION);
        encoder.string("exact-scalar-projection-v1");
        encode_attribute_type(&mut encoder, self);
        hex::encode(Sha256::digest(encoder.finish()))
    }
}

impl OwnerBoundResourceSchema<'_> {
    /// SHA-256 of resource semantics, owner reference forms, and declared edges.
    pub fn semantic_fingerprint(self) -> String {
        hex::encode(Sha256::digest(semantic_encoding(
            self.schema().as_schema(),
            self.reference_forms(),
            self.assignability_edges(),
        )))
    }
}

fn semantic_encoding(
    schema: &ResourceSchema,
    reference_forms: &ReferenceForms,
    assignability_edges: &[AssignabilityEdge],
) -> Vec<u8> {
    let mut encoder = CanonicalEncoder::default();
    encoder.string(ENCODING_VERSION);
    encode_resource_schema(&mut encoder, schema);
    encode_reference_forms(&mut encoder, reference_forms);
    encode_assignability_edges(&mut encoder, assignability_edges);
    encoder.finish()
}

fn encode_assignability_edges(encoder: &mut CanonicalEncoder, edges: &[AssignabilityEdge]) {
    let mut edges = edges.iter().collect::<Vec<_>>();
    edges.sort_unstable();
    encoder.usize(edges.len());
    for edge in edges {
        let (source, sink) = edge.parts();
        encode_type_identity(encoder, source);
        encode_type_identity(encoder, sink);
    }
}

fn encode_reference_forms(encoder: &mut CanonicalEncoder, forms: &ReferenceForms) {
    encoder.usize(forms.len());
    for (identity, form) in forms {
        encode_type_identity(encoder, identity);
        match form {
            ReferenceForm::Template { long } => {
                encoder.tag(0);
                encoder.string(long.as_str());
            }
        }
    }
}

fn encode_resource_schema(encoder: &mut CanonicalEncoder, schema: &ResourceSchema) {
    let ResourceSchema {
        resource_type,
        attributes,
        description: _description,
        validation,
        input_rules,
        kind,
        unique_name,
        operation_config,
        field_groups,
        default_prevent_destroy,
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
    encode_input_rules(encoder, input_rules);
    encode_unique_name(encoder, unique_name);
    encoder.option(operation_config.as_ref(), encode_operation_config);

    encode_field_groups(encoder, field_groups);
    encoder.bool(*default_prevent_destroy);

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

fn encode_field_groups(encoder: &mut CanonicalEncoder, groups: &[FieldGroup]) {
    let mut encoded = groups
        .iter()
        .map(|group| {
            let mut group_encoder = CanonicalEncoder::default();
            group_encoder.tag(match group.kind() {
                FieldGroupKind::ExactlyOne => 0,
                FieldGroupKind::AtLeastOne => 1,
                FieldGroupKind::AtMostOne => 2,
            });
            let mut fields = group.fields().to_vec();
            fields.sort();
            group_encoder.strings(&fields);
            group_encoder.finish()
        })
        .collect::<Vec<_>>();
    encoded.sort();
    encoder.usize(encoded.len());
    for group in encoded {
        encoder.bytes(&group);
    }
}

fn encode_input_charset(encoder: &mut CanonicalEncoder, set: &CharSet) {
    encoder.usize(set.ranges().len());
    for (start, end) in set.ranges() {
        encoder.u32(u32::from(*start));
        encoder.u32(u32::from(*end));
    }
}

fn encode_input_literal(encoder: &mut CanonicalEncoder, literal: &InputLiteral) {
    match literal {
        InputLiteral::Bool(value) => {
            encoder.tag(0);
            encoder.bool(*value);
        }
        InputLiteral::Int(value) => {
            encoder.tag(1);
            encoder.i64(*value);
        }
        InputLiteral::Float(value) => {
            encoder.tag(2);
            encoder.u64(if value.get() == 0.0 {
                0
            } else {
                value.get().to_bits()
            });
        }
        InputLiteral::String(value) => {
            encoder.tag(3);
            encoder.string(value);
        }
        InputLiteral::Enum {
            identity,
            api_value,
        } => {
            encoder.tag(4);
            encode_type_identity(encoder, identity);
            encoder.string(api_value);
        }
    }
}
fn encode_input_condition(encoder: &mut CanonicalEncoder, condition: &InputCondition) {
    match condition {
        InputCondition::Always => encoder.tag(0),
        InputCondition::Present(path) => {
            encoder.tag(1);
            encoder.string(path.as_str());
        }
        InputCondition::Equals { path, literal } => {
            encoder.tag(2);
            encoder.string(path.as_str());
            encode_input_literal(encoder, literal);
        }
        InputCondition::Not(inner) => {
            encoder.tag(3);
            encode_input_condition(encoder, inner);
        }
    }
}

fn encode_input_rules(encoder: &mut CanonicalEncoder, rules: &[InputRule<Compiled>]) {
    let mut encoded: Vec<_> = rules
        .iter()
        .map(|rule| {
            let InputRuleParts {
                scope,
                target,
                code,
                condition,
                predicate,
            } = rule.parts();
            let mut encoder = CanonicalEncoder::default();
            encoder.tag(match scope {
                InputRuleScope::CreateOnly => 0,
                InputRuleScope::EveryWrite => 1,
            });
            match target {
                InputRuleTarget::Value(path) => {
                    encoder.tag(0);
                    encoder.string(path.as_str());
                }
                InputRuleTarget::MapKeys(path) => {
                    encoder.tag(1);
                    encoder.string(path.as_str());
                }
            }
            encoder.string(code.as_str());
            encode_input_condition(&mut encoder, condition);
            match predicate {
                InputPredicate::Present => encoder.tag(12),
                InputPredicate::Absent => encoder.tag(13),
                InputPredicate::Equals(literal) => {
                    encoder.tag(14);
                    encode_input_literal(&mut encoder, literal);
                }
                InputPredicate::Length { min, max } => {
                    encoder.tag(0);
                    encoder.option(*min, CanonicalEncoder::u64);
                    encoder.option(*max, CanonicalEncoder::u64);
                }
                InputPredicate::AllowedChars(set) => {
                    encoder.tag(1);
                    encode_input_charset(&mut encoder, set);
                }
                InputPredicate::FirstChar(set) => {
                    encoder.tag(2);
                    encode_input_charset(&mut encoder, set);
                }
                InputPredicate::LastChar(set) => {
                    encoder.tag(3);
                    encode_input_charset(&mut encoder, set);
                }
                InputPredicate::NoSubstring(token) => {
                    encoder.tag(4);
                    encoder.string(token.as_str());
                }
                InputPredicate::NoPrefix(token) => {
                    encoder.tag(5);
                    encoder.string(token.as_str());
                }
                InputPredicate::ForbiddenPrefix {
                    prefix,
                    ignore_ascii_case,
                } => {
                    encoder.tag(11);
                    encoder.string(prefix.as_str());
                    encoder.bool(*ignore_ascii_case);
                }
                InputPredicate::NoSuffix(token) => {
                    encoder.tag(6);
                    encoder.string(token.as_str());
                }
                InputPredicate::NoAdjacent { left, right } => {
                    encoder.tag(7);
                    encode_input_charset(&mut encoder, left);
                    encode_input_charset(&mut encoder, right);
                }
                InputPredicate::NotIpv4 => encoder.tag(8),
                InputPredicate::Matches(pattern) => {
                    encoder.tag(9);
                    encode_pattern(&mut encoder, pattern.as_ref());
                }
                InputPredicate::NotMatches(pattern) => {
                    encoder.tag(10);
                    encode_pattern(&mut encoder, pattern.as_ref());
                }
            }
            encoder.finish()
        })
        .collect();
    encoded.sort();
    encoder.usize(encoded.len());
    for rule in encoded {
        encoder.bytes(&rule);
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
        keyed_partial_update,
        identity_mode,
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
    encoder.option(keyed_partial_update.as_ref(), |encoder, keyed| {
        encoder.string(keyed.key_field().as_str());
    });
    encode_identity_mode(encoder, *identity_mode);
    encoder.bool(*deferred_populate);
}

fn encode_attribute_type(encoder: &mut CanonicalEncoder, attribute_type: &AttributeType) {
    let AttributeType { kind } = attribute_type;
    match kind {
        AttrTypeKind::UnloadedSchemaType(unloaded) => {
            encoder.tag(13);
            encode_type_identity(encoder, unloaded.identity());
        }
        AttrTypeKind::Reference {
            identity,
            pattern,
            length,
        } => {
            encoder.tag(15);
            encode_type_identity(encoder, identity);
            encoder.option(pattern.as_ref().map(AsRef::as_ref), encode_pattern);
            encode_optional_u64_range(encoder, *length);
        }
        AttrTypeKind::String {
            identity_layer:
                crate::schema::StringIdentityLayer {
                    binding,
                    pattern,
                    length,
                    to_dsl,
                },
            member_layer,
        } => {
            let super::StringMemberParts {
                pattern: member_pattern,
                length: member_length,
            } = member_layer.parts();
            encoder.tag(0);
            encoder.option(binding.identity(), encode_type_identity);
            encoder.option(pattern.as_ref().map(AsRef::as_ref), encode_pattern);
            encode_optional_u64_range(encoder, *length);
            encoder.option(member_pattern, encode_pattern);
            encode_optional_u64_range(encoder, member_length);
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
        AttrTypeKind::Int {
            identity_layer: crate::schema::IntIdentityLayer { binding, range },
            member_layer,
        } => {
            encoder.tag(3);
            encoder.option(binding.identity(), encode_type_identity);
            encode_optional_i64_range(encoder, *range);
            let super::IntMemberParts {
                range: member_range,
            } = member_layer.parts();
            encode_optional_i64_range(encoder, member_range);
            encode_callback_presence(encoder, binding.callback().is_some());
        }
        AttrTypeKind::Float { binding, range } => {
            encoder.tag(4);
            encoder.option(binding.identity(), encode_type_identity);
            encoder.option(*range, |encoder, (min, max)| {
                encoder.option(min, |encoder, value| encoder.u64(value.get().to_bits()));
                encoder.option(max, |encoder, value| encoder.u64(value.get().to_bits()));
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
            constraints,
        } => {
            encoder.tag(8);
            encode_attribute_type(encoder, element_type);
            encoder.bool(*ordered);
            encode_optional_u64_range(encoder, constraints.length());
            encoder.tag(match constraints.elements() {
                super::ListElements::AllowDuplicates => 0,
                super::ListElements::Unique => 1,
            });
            encode_callback_presence(encoder, false);
        }
        AttrTypeKind::Map {
            key,
            value,
            constraints,
        } => {
            encoder.tag(9);
            encode_attribute_type(encoder, key);
            encode_attribute_type(encoder, value);
            encode_optional_u64_range(encoder, constraints.length());
        }
        AttrTypeKind::Struct {
            name,
            fields,
            field_groups,
            input_rules,
        } => {
            encoder.tag(10);
            encoder.string(name);
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by(|left, right| left.name.cmp(&right.name));
            encoder.usize(fields.len());
            for field in fields {
                encode_struct_field(encoder, field);
            }
            encode_field_groups(encoder, field_groups);
            encode_input_rules(encoder, input_rules);
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
        create_only,
        identity_mode,
    } = field;
    encoder.string(name);
    encode_attribute_type(encoder, field_type);
    encode_input_mode(encoder, *declared_input_mode);
    encoder.option(provider_name.as_deref(), CanonicalEncoder::string);
    encoder.option(block_name.as_deref(), CanonicalEncoder::string);
    encoder.bool(*deferred_populate);
    encoder.bool(*write_only);
    encoder.bool(*create_only);
    encode_identity_mode(encoder, *identity_mode);
}

fn encode_identity_mode(encoder: &mut CanonicalEncoder, mode: IdentityMode) {
    match mode {
        IdentityMode::None => encoder.tag(0),
        IdentityMode::Required => encoder.tag(1),
        IdentityMode::WhenPresent => encoder.tag(2),
    }
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
        read_back,
    } = config;
    encoder.option(*delete_timeout_secs, CanonicalEncoder::u64);
    encoder.option(*delete_max_retries, CanonicalEncoder::u32);
    encoder.option(*create_timeout_secs, CanonicalEncoder::u64);
    encoder.option(*create_max_retries, CanonicalEncoder::u32);
    encoder.option(*update_timeout_secs, CanonicalEncoder::u64);
    encoder.option(
        read_back.map(|budget| budget.timeout_secs().get()),
        CanonicalEncoder::u64,
    );
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
    use crate::schema::{PatternAttributeSchema, PatternAttributeType, PatternResourceSchema};

    use super::*;
    use crate::schema::ReferenceTemplate;

    fn attribute_type_encoding(attribute_type: AttributeType) -> Vec<u8> {
        let mut encoder = CanonicalEncoder::default();
        encode_attribute_type(&mut encoder, &attribute_type);
        encoder.finish()
    }

    #[test]
    fn callback_presence_markers_reflect_the_new_binding_model() {
        assert_eq!(
            attribute_type_encoding(AttributeType::string()),
            vec![0, 0, 0, 0, 0, 0, 0, 0],
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
            vec![3, 0, 0, 0, 0],
            "Int encodes validate"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::float()),
            vec![4, 0, 0, 0],
            "Float encodes validate"
        );
        assert_eq!(
            attribute_type_encoding(AttributeType::list(AttributeType::bool())),
            vec![8, 5, 1, 0, 0, 0],
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

    #[test]
    fn list_elements_refinement_changes_the_fingerprint() {
        let schema = |elements| {
            ResourceSchema::new("test.Subject").attribute(
                AttributeSchema::optional(
                    "claims",
                    AttributeType::refined_list(AttributeType::string(), true, None, elements),
                )
                .build(),
            )
        };
        assert_ne!(
            schema(super::super::ListElements::AllowDuplicates).semantic_fingerprint(),
            schema(super::super::ListElements::Unique).semantic_fingerprint()
        );
    }

    #[test]
    fn map_length_changes_the_fingerprint() {
        let schema = |length| {
            ResourceSchema::new("test.Subject").attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::refined_map(
                        AttributeType::string(),
                        AttributeType::string(),
                        length,
                    ),
                )
                .build(),
            )
        };
        let unbounded = schema(None).semantic_fingerprint();
        let bounded = schema(Some((Some(0), Some(10)))).semantic_fingerprint();
        let different_max = schema(Some((Some(0), Some(11)))).semantic_fingerprint();
        let different_min = schema(Some((Some(1), Some(10)))).semantic_fingerprint();
        assert_ne!(unbounded, bounded);
        assert_ne!(bounded, different_max);
        assert_ne!(bounded, different_min);
    }

    #[test]
    fn owner_reference_forms_change_the_resource_fingerprint() {
        let identity = TypeIdentity::new(Some("aws"), ["lambda", "Function"], "Name");
        let schema = PatternResourceSchema::new("aws.lambda.Function")
            .attribute(
                PatternAttributeSchema::optional(
                    "name",
                    PatternAttributeType::refined_string(Some(identity.clone()), None, None, None),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let empty = ReferenceForms::new();
        let first = ReferenceForms::from([(
            identity.clone(),
            ReferenceForm::Template {
                long: ReferenceTemplate::new(
                    "arn:{partition}:lambda:{region}:{account}:function:{name}",
                )
                .unwrap(),
            },
        )]);
        let changed = ReferenceForms::from([(
            identity,
            ReferenceForm::Template {
                long: ReferenceTemplate::new(
                    "arn:{partition}:lambda:{region}:{account}:function-alias:{name}",
                )
                .unwrap(),
            },
        )]);

        let owner = crate::parser::SchemaTypeOwner::Provider("aws".into());
        let without =
            OwnerBoundResourceSchema::new(&owner, &schema, &empty, &[]).semantic_fingerprint();
        let with =
            OwnerBoundResourceSchema::new(&owner, &schema, &first, &[]).semantic_fingerprint();
        let changed =
            OwnerBoundResourceSchema::new(&owner, &schema, &changed, &[]).semantic_fingerprint();

        assert_ne!(without, with, "adding a form must change the fingerprint");
        assert_ne!(with, changed, "changing a form must change the fingerprint");
        assert_ne!(
            without, changed,
            "removing a form must change the fingerprint"
        );
    }
    #[test]
    fn creation_fingerprints_encode_every_payload_and_ignore_rule_order() {
        use super::super::{
            CharSet, InputPath, InputPredicate, InputRule, InputRuleCode, NonEmptyString,
        };
        let rule = |path: &str, code: &str, predicate| {
            InputRule::value(
                crate::schema::InputRuleScope::CreateOnly,
                InputPath::new(path).unwrap(),
                InputRuleCode::new(code).unwrap(),
                predicate,
            )
        };
        let fingerprint = |rules: Vec<InputRule>| {
            rules
                .into_iter()
                .fold(
                    PatternResourceSchema::new("test.Widget")
                        .attribute(
                            PatternAttributeSchema::optional(
                                "name",
                                PatternAttributeType::string(),
                            )
                            .build(),
                        )
                        .attribute(
                            PatternAttributeSchema::optional(
                                "other",
                                PatternAttributeType::string(),
                            )
                            .build(),
                        ),
                    |schema, rule| schema.input_rule(rule),
                )
                .validate()
                .unwrap()
                .as_schema()
                .semantic_fingerprint()
        };
        let token = |value: &str| NonEmptyString::new(value).unwrap();
        let predicates = vec![
            InputPredicate::Length {
                min: Some(1),
                max: Some(63),
            },
            InputPredicate::Length {
                min: Some(2),
                max: Some(63),
            },
            InputPredicate::Length {
                min: Some(1),
                max: Some(64),
            },
            InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            InputPredicate::AllowedChars(CharSet::ascii_digits()),
            InputPredicate::FirstChar(CharSet::ascii_lowercase()),
            InputPredicate::LastChar(CharSet::ascii_lowercase()),
            InputPredicate::NoSubstring(token("ab")),
            InputPredicate::NoSubstring(token("ac")),
            InputPredicate::NoPrefix(token("ab")),
            InputPredicate::NoSuffix(token("ab")),
            InputPredicate::NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            InputPredicate::NoAdjacent {
                left: CharSet::ascii_digits(),
                right: CharSet::ascii_lowercase(),
            },
            InputPredicate::NotIpv4,
            InputPredicate::Matches(crate::schema::PatternSpec::new(
                "^a$",
                crate::schema::PatternMatch::Full,
            )),
            InputPredicate::Matches(crate::schema::PatternSpec::new(
                "^b$",
                crate::schema::PatternMatch::Full,
            )),
            InputPredicate::NotMatches(crate::schema::PatternSpec::new(
                "^a$",
                crate::schema::PatternMatch::Full,
            )),
        ];
        let mut encodings = std::collections::BTreeSet::new();
        encodings.insert(fingerprint(vec![]));
        for predicate in predicates {
            assert!(encodings.insert(fingerprint(vec![rule("name", "test.rule", predicate)])));
        }
        let a = rule("name", "test.first", InputPredicate::NotIpv4);
        let b = rule("other", "test.second", InputPredicate::NotIpv4);
        assert_eq!(
            fingerprint(vec![a.clone(), b.clone()]),
            fingerprint(vec![b.clone(), a.clone()])
        );
        assert_ne!(
            fingerprint(vec![a.clone()]),
            fingerprint(vec![rule("other", "test.first", InputPredicate::NotIpv4)])
        );
        assert_ne!(
            fingerprint(vec![a]),
            fingerprint(vec![rule("name", "test.changed", InputPredicate::NotIpv4)])
        );
        let canonical = rule(
            "name",
            "test.chars",
            InputPredicate::AllowedChars(CharSet::new([('a', 'f')]).unwrap()),
        );
        let reordered = rule(
            "name",
            "test.chars",
            InputPredicate::AllowedChars(CharSet::new([('d', 'f'), ('a', 'd')]).unwrap()),
        );
        assert_eq!(fingerprint(vec![canonical]), fingerprint(vec![reordered]));
    }
}

#[cfg(test)]
mod pattern_mode_tests {
    use super::*;
    use crate::schema::{
        AttributeSchema, AttributeType, PatternMatch, PatternReason, PatternSpec, ResourceSchema,
    };
    use std::collections::HashSet;

    #[test]
    fn reference_and_input_rule_fingerprints_include_pattern_mode_and_reason() {
        for kind in ["reference", "matches", "not_matches"] {
            let fingerprints: HashSet<_> = [
                PatternMatch::Full,
                PatternMatch::Substring {
                    reason: PatternReason::new("first contract").unwrap(),
                },
                PatternMatch::Substring {
                    reason: PatternReason::new("second contract").unwrap(),
                },
            ]
            .into_iter()
            .map(|mode| {
                let spec = PatternSpec::new("abc", mode);
                let ty = if kind == "reference" {
                    AttributeType::reference(
                        TypeIdentity::new(Some("test"), ["Pattern"], "Id"),
                        Some(spec.clone()),
                        None,
                    )
                } else {
                    AttributeType::string()
                };
                let mut schema = ResourceSchema::new("test.Pattern")
                    .attribute(AttributeSchema::optional("value", ty).build());
                if kind != "reference" {
                    let predicate = if kind == "matches" {
                        InputPredicate::Matches(spec)
                    } else {
                        InputPredicate::NotMatches(spec)
                    };
                    schema = schema.input_rule(InputRule::value(
                        InputRuleScope::EveryWrite,
                        crate::schema::InputPath::new("value").unwrap(),
                        crate::schema::InputRuleCode::new("test.pattern").unwrap(),
                        predicate,
                    ));
                }
                schema
                    .validate()
                    .unwrap()
                    .as_schema()
                    .semantic_fingerprint()
            })
            .collect();
            assert_eq!(fingerprints.len(), 3, "{kind}");
        }
    }
}

#[cfg(test)]
mod conditional_input_rule_tests {
    use crate::schema::{
        AttributeSchema, AttributeType, InputCondition, InputLiteral, InputPath, InputPredicate,
        InputRule, InputRuleCode, InputRuleScope, ResourceSchema as DraftSchema, StructField,
    };

    #[test]
    fn guards_and_nested_rules_change_semantic_fingerprints() {
        let rule = |condition| {
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("test.condition").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(condition)
        };
        let schema = |condition, nested: bool| {
            let mut draft = DraftSchema::new("test.Conditional")
                .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
                .attribute(AttributeSchema::optional("target", AttributeType::string()).build());
            if nested {
                draft = draft.attribute(
                    AttributeSchema::optional(
                        "nested",
                        AttributeType::struct_with_rules(
                            "Nested",
                            vec![
                                StructField::optional("guard", AttributeType::bool()).build(),
                                StructField::optional("target", AttributeType::string()).build(),
                            ],
                            vec![],
                            vec![rule(condition)],
                        ),
                    )
                    .build(),
                );
            } else {
                draft = draft.input_rule(rule(condition));
            }
            draft.validate().unwrap().as_schema().semantic_fingerprint()
        };
        for nested in [false, true] {
            let present = InputCondition::Present(InputPath::new("guard").unwrap());
            let equal = InputCondition::Equals {
                path: InputPath::new("guard").unwrap(),
                literal: InputLiteral::Bool(true),
            };
            let negated = InputCondition::Not(Box::new(equal.clone()));
            let fingerprints = [
                schema(InputCondition::Always, nested),
                schema(present, nested),
                schema(equal, nested),
                schema(negated, nested),
            ];
            assert_eq!(
                fingerprints
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                4
            );
        }
    }
}
