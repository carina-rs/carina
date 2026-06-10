use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::schema::{
    AttributeType, DslMap, EnumParts, ExpectedEnumVariant, TypeError, TypeIdentity,
    enum_value_matches,
};
use crate::utils::{NamespacedId, extract_enum_value_with_values, validate_enum_namespace};

/// Parser-surface enum identifier text plus schema-free syntax classification.
#[derive(Clone, Eq)]
pub struct RawEnumIdentifier {
    text: String,
    parsed: RawEnumIdentifierParts,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RawEnumIdentifierParts {
    Bare {
        value: String,
    },
    TypeQualified {
        type_name: String,
        value: String,
    },
    ProviderQualified {
        provider: String,
        type_name: String,
        value: String,
    },
    FullyQualified {
        provider: String,
        segments: Vec<String>,
        type_name: String,
        value: String,
    },
    Unclassified,
}

impl RawEnumIdentifier {
    pub fn parse(text: impl Into<String>) -> Self {
        let text = text.into();
        let parsed = match NamespacedId::parse(&text) {
            Some(NamespacedId::TypeQualified { type_name, value }) => {
                RawEnumIdentifierParts::TypeQualified {
                    type_name: type_name.to_string(),
                    value: value.to_string(),
                }
            }
            Some(NamespacedId::ProviderQualified {
                provider,
                type_name,
                value,
            }) => RawEnumIdentifierParts::ProviderQualified {
                provider: provider.to_string(),
                type_name: type_name.to_string(),
                value: value.to_string(),
            },
            Some(NamespacedId::FullyQualified {
                provider,
                segments_str,
                type_name,
                value,
            }) => RawEnumIdentifierParts::FullyQualified {
                provider: provider.to_string(),
                segments: segments_str.split('.').map(String::from).collect(),
                type_name: type_name.to_string(),
                value: value.to_string(),
            },
            None if !text.contains('.') => RawEnumIdentifierParts::Bare {
                value: text.clone(),
            },
            None => RawEnumIdentifierParts::Unclassified,
        };
        Self { text, parsed }
    }

    pub fn parsed(&self) -> &RawEnumIdentifierParts {
        &self.parsed
    }

    /// TODO(carina#3438): remove in chain PR 5.
    /// Temporary accessor for call sites that still consume enum identifiers
    /// as parser-surface strings during the PR chain.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for RawEnumIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl fmt::Debug for RawEnumIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.text, f)
    }
}

// TODO(carina#3438): remove in chain PR 5.
// Temporary string-like shim for existing display/formatter call sites.
impl Deref for RawEnumIdentifier {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl PartialEq for RawEnumIdentifier {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

// TODO(carina#3438): remove in chain PR 5.
// Temporary equality shim preserving old String/EnumIdentifier comparisons.
impl PartialEq<RawEnumIdentifier> for String {
    fn eq(&self, other: &RawEnumIdentifier) -> bool {
        *self == other.text
    }
}

impl Hash for RawEnumIdentifier {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
    }
}

impl Serialize for RawEnumIdentifier {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for RawEnumIdentifier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::parse)
    }
}

/// Schema-resolved enum identity plus provider API value.
///
/// This type intentionally has no equality implementation with
/// [`RawEnumIdentifier`]. Raw source spelling and canonical API semantics are
/// different phases; compare canonical values only after resolver construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalEnumValue {
    identity: TypeIdentity,
    api_value: String,
}

impl CanonicalEnumValue {
    pub fn identity(&self) -> &TypeIdentity {
        &self.identity
    }

    pub fn api_value(&self) -> &str {
        &self.api_value
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(identity: TypeIdentity, api_value: impl Into<String>) -> Self {
        Self {
            identity,
            api_value: api_value.into(),
        }
    }

    fn new_resolved(identity: TypeIdentity, api_value: String) -> Self {
        Self {
            identity,
            api_value,
        }
    }
}

impl fmt::Display for CanonicalEnumValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.identity, self.api_value)
    }
}

pub struct EnumValueResolver<'a> {
    attr_type: &'a AttributeType,
    defs: Option<&'a BTreeMap<String, AttributeType>>,
}

impl<'a> EnumValueResolver<'a> {
    pub fn new(attr_type: &'a AttributeType) -> Self {
        Self {
            attr_type,
            defs: None,
        }
    }

    pub fn with_defs(
        attr_type: &'a AttributeType,
        defs: &'a BTreeMap<String, AttributeType>,
    ) -> Self {
        Self {
            attr_type,
            defs: Some(defs),
        }
    }

    pub fn resolve_raw(&self, raw: &RawEnumIdentifier) -> Result<CanonicalEnumValue, TypeError> {
        self.resolve_text(raw.as_str(), EnumInputPhase::RawDsl)
    }

    pub fn resolve_state_text(&self, text: &str) -> Result<CanonicalEnumValue, TypeError> {
        self.resolve_text(text, EnumInputPhase::StateText)
    }

    fn parts(&self) -> Result<EnumParts<'_>, TypeError> {
        let attr_type = match self.defs {
            Some(defs) => self.attr_type.resolve_refs_with_defs(defs).as_attr(),
            None => self.attr_type,
        };
        attr_type
            .enum_parts()
            .ok_or_else(|| TypeError::TypeMismatch {
                expected: "Enum".to_string(),
                got: attr_type.type_name(),
            })
    }

    fn resolve_text(
        &self,
        text: &str,
        phase: EnumInputPhase,
    ) -> Result<CanonicalEnumValue, TypeError> {
        let (identity, values, dsl_aliases, validate, dsl_map) = self.parts()?;
        let valid: Vec<&str> = values.into_iter().flatten().map(String::as_str).collect();
        let direct_match = valid.iter().any(|v| enum_value_matches(text, v));
        let variant = if direct_match {
            text
        } else {
            extract_enum_value_with_values(text, &valid)
        };

        if phase == EnumInputPhase::RawDsl
            && !direct_match
            && let Err(message) = validate_enum_namespace(text, identity)
        {
            return Err(TypeError::ValidationFailed {
                message: format!("Invalid {} '{}': {}", identity.kind, text, message),
            });
        }

        if phase == EnumInputPhase::RawDsl && api_spelling_rejected_in_dsl(variant, dsl_aliases) {
            return Err(invalid_enum_variant(
                text,
                identity,
                values,
                dsl_aliases,
                dsl_map,
            ));
        }

        let api_value = match phase {
            EnumInputPhase::RawDsl => dsl_map.api_for_hash_feature(variant),
            EnumInputPhase::StateText => {
                if valid.contains(&text) {
                    text.to_string()
                } else {
                    dsl_map.api_for_hash_feature(variant)
                }
            }
        };

        let enumerated = values.is_some();
        let valid_value = values
            .into_iter()
            .flatten()
            .any(|v| enum_value_matches(&api_value, v));
        let validation_result = validate.map(|validate| {
            validate(&crate::resource::Value::Concrete(
                crate::resource::ConcreteValue::String(api_value.clone()),
            ))
        });
        let validation_ok = validation_result
            .as_ref()
            .map_or(!enumerated, Result::is_ok);

        if valid_value || validation_ok {
            Ok(CanonicalEnumValue::new_resolved(
                identity.clone(),
                api_value,
            ))
        } else {
            Err(invalid_enum_variant(
                text,
                identity,
                values,
                dsl_aliases,
                dsl_map,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumInputPhase {
    RawDsl,
    StateText,
}

fn api_spelling_rejected_in_dsl(variant: &str, dsl_aliases: &[(String, String)]) -> bool {
    dsl_aliases
        .iter()
        .any(|(api, dsl)| api != dsl && variant == api)
}

fn invalid_enum_variant(
    value: &str,
    identity: &TypeIdentity,
    values: Option<&[String]>,
    dsl_aliases: &[(String, String)],
    dsl_map: DslMap<'_>,
) -> TypeError {
    TypeError::InvalidEnumVariant {
        value: value.to_string(),
        attribute: None,
        type_name: Some(identity.kind.clone()),
        expected: expected_variants(identity, values.unwrap_or_default(), dsl_aliases, dsl_map),
    }
}

fn expected_variants(
    identity: &TypeIdentity,
    values: &[String],
    dsl_aliases: &[(String, String)],
    dsl_map: DslMap<'_>,
) -> Vec<ExpectedEnumVariant> {
    let namespace = identity.dotted_prefix();
    let namespace = namespace.as_deref();
    let mut expected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut canonical_dsl_values = std::collections::HashSet::new();
    for value in values {
        let dsl_value = dsl_map.dsl_for(value).into_owned();
        canonical_dsl_values.insert(dsl_value.clone());
        if seen.insert(dsl_value.clone()) {
            expected.push(ExpectedEnumVariant::from_namespaced(
                namespace,
                &identity.kind,
                &dsl_value,
                false,
            ));
        }
    }
    for (_api, dsl_value) in dsl_aliases {
        if !canonical_dsl_values.contains(dsl_value) && seen.insert(dsl_value.clone()) {
            expected.push(ExpectedEnumVariant::from_namespaced(
                namespace,
                &identity.kind,
                dsl_value,
                true,
            ));
        }
    }
    expected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ConcreteValue;
    use crate::schema::{DslTransform, enum_identity};

    #[test]
    fn raw_parse_classifies_identifier_shapes() {
        assert_eq!(
            RawEnumIdentifier::parse("enabled").parsed(),
            &RawEnumIdentifierParts::Bare {
                value: "enabled".to_string()
            }
        );
        assert_eq!(
            RawEnumIdentifier::parse("Region.ap_northeast_1").parsed(),
            &RawEnumIdentifierParts::TypeQualified {
                type_name: "Region".to_string(),
                value: "ap_northeast_1".to_string()
            }
        );
        assert_eq!(
            RawEnumIdentifier::parse("aws.Region.ap_northeast_1").parsed(),
            &RawEnumIdentifierParts::ProviderQualified {
                provider: "aws".to_string(),
                type_name: "Region".to_string(),
                value: "ap_northeast_1".to_string()
            }
        );
        assert_eq!(
            RawEnumIdentifier::parse("awscc.ec2.vpn_gateway.Type.ipsec.1").parsed(),
            &RawEnumIdentifierParts::FullyQualified {
                provider: "awscc".to_string(),
                segments: vec!["ec2".to_string(), "vpn_gateway".to_string()],
                type_name: "Type".to_string(),
                value: "ipsec.1".to_string()
            }
        );
        assert_eq!(
            RawEnumIdentifier::parse("some.random.string").parsed(),
            &RawEnumIdentifierParts::Unclassified
        );
    }

    #[test]
    fn debug_output_is_transparent_to_text() {
        let v = ConcreteValue::enum_identifier("dedicated");
        assert_eq!(format!("{:?}", v), r#"EnumIdentifier("dedicated")"#);
    }

    #[test]
    fn resolve_raw_maps_alias_and_transform_to_same_canonical_value() {
        let region = AttributeType::enum_(
            enum_identity("Region", Some("aws")),
            Some(vec!["ap-northeast-1".to_string()]),
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let canonical_from_aws = EnumValueResolver::new(&region)
            .resolve_raw(&RawEnumIdentifier::parse("aws.Region.ap_northeast_1"));
        let awscc_region = AttributeType::enum_(
            enum_identity("Region", Some("awscc")),
            Some(vec!["ap-northeast-1".to_string()]),
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let canonical_from_awscc = EnumValueResolver::new(&awscc_region)
            .resolve_raw(&RawEnumIdentifier::parse("awscc.Region.ap_northeast_1"));
        assert_eq!(canonical_from_aws.unwrap().api_value(), "ap-northeast-1");
        assert_eq!(canonical_from_awscc.unwrap().api_value(), "ap-northeast-1");

        let effect = AttributeType::enum_(
            enum_identity("Effect", Some("aws.iam.PolicyDocument")),
            Some(vec!["Allow".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let canonical = EnumValueResolver::new(&effect)
            .resolve_raw(&RawEnumIdentifier::parse(
                "aws.iam.PolicyDocument.Effect.allow",
            ))
            .unwrap();
        assert_eq!(canonical.api_value(), "Allow");
    }

    #[test]
    fn resolve_raw_rejects_namespace_mismatch_and_invalid_value() {
        let attr = AttributeType::enum_(
            enum_identity("Region", Some("aws")),
            Some(vec!["ap-northeast-1".to_string()]),
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        assert!(matches!(
            EnumValueResolver::new(&attr)
                .resolve_raw(&RawEnumIdentifier::parse("awscc.Region.ap_northeast_1")),
            Err(TypeError::ValidationFailed { .. })
        ));
        assert!(matches!(
            EnumValueResolver::new(&attr)
                .resolve_raw(&RawEnumIdentifier::parse("aws.Region.eu_west_1")),
            Err(TypeError::InvalidEnumVariant { .. })
        ));
    }

    #[test]
    fn resolve_state_text_accepts_api_and_dsl_alias() {
        let attr = AttributeType::enum_(
            enum_identity("Effect", Some("aws.iam.PolicyDocument")),
            Some(vec!["Allow".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let resolver = EnumValueResolver::new(&attr);
        assert_eq!(
            resolver.resolve_state_text("Allow").unwrap().api_value(),
            "Allow"
        );
        assert_eq!(
            resolver.resolve_state_text("allow").unwrap().api_value(),
            "Allow"
        );
    }

    #[test]
    fn canonical_equality_uses_identity_and_api_value() {
        let aws_region = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region");
        let gcp_region = TypeIdentity::new(Some("gcp"), Vec::<String>::new(), "Region");
        assert_eq!(
            CanonicalEnumValue::new_for_test(aws_region.clone(), "us-east-1"),
            CanonicalEnumValue::new_for_test(aws_region, "us-east-1")
        );
        assert_ne!(
            CanonicalEnumValue::new_for_test(gcp_region, "us-east-1"),
            CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "us-east-1"
            )
        );
    }

    #[test]
    fn serde_round_trips_raw_as_plain_string_and_canonical_as_object() {
        let raw = RawEnumIdentifier::parse("aws.Region.ap_northeast_1");
        let encoded = serde_json::to_string(&raw).unwrap();
        assert_eq!(encoded, r#""aws.Region.ap_northeast_1""#);
        let decoded: RawEnumIdentifier = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, raw);
        assert_eq!(decoded.parsed(), raw.parsed());

        let canonical = CanonicalEnumValue::new_for_test(
            TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
            "ap-northeast-1",
        );
        let encoded = serde_json::to_string(&canonical).unwrap();
        assert!(encoded.contains(r#""identity""#));
        assert!(encoded.contains(r#""api_value":"ap-northeast-1""#));
        let decoded: CanonicalEnumValue = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, canonical);
    }
}
