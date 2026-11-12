//! Authored key identity and its independently admitted diagnostic spelling.

use super::{AuthoredExpressionText, Rule};
use pest::iterators::Pair;
use std::hash::{Hash, Hasher};

/// Equality and map membership use authored identity. Formatting can only use
/// the admitted spelling; there is no raw-text accessor or string conversion.
#[derive(Clone)]
pub struct AttributeKey {
    identity: String,
    diagnostic: KeyPresentation,
}
#[derive(Clone)]
enum KeyPresentation {
    Authored(AuthoredExpressionText),
    SecretOwned,
}
impl KeyPresentation {
    fn text(&self) -> &str {
        match self {
            Self::Authored(text) => text.as_str(),
            Self::SecretOwned => "<redacted>",
        }
    }
    fn protected(&self) -> bool {
        match self {
            Self::Authored(text) => text.has_redactions(),
            Self::SecretOwned => true,
        }
    }
}

impl AttributeKey {
    pub(crate) fn literal(identity: String) -> Self {
        let diagnostic = AuthoredExpressionText::literal_value(&identity);
        Self {
            identity,
            diagnostic: KeyPresentation::Authored(diagnostic),
        }
    }
    fn authored(pair: Pair<'_, Rule>) -> Self {
        Self {
            identity: pair.as_str().to_owned(),
            diagnostic: KeyPresentation::Authored(AuthoredExpressionText::redact(pair, None)),
        }
    }

    fn admit_in_file(mut self, pair: &Pair<'_, Rule>, file: &super::PreprocessedFile) -> Self {
        if file
            .protected_regions()
            .owns(pair.as_span().start()..pair.as_span().end())
        {
            self.diagnostic = KeyPresentation::SecretOwned;
        }
        self
    }
    pub(super) fn authored_in_file(pair: Pair<'_, Rule>, file: &super::PreprocessedFile) -> Self {
        Self::authored(pair.clone()).admit_in_file(&pair, file)
    }
    pub(super) fn literal_in_file(pair: Pair<'_, Rule>, file: &super::PreprocessedFile) -> Self {
        let identity = super::context::extract_key_string(pair.clone()).unwrap_or_default();
        Self::provenance(identity, &pair, file)
    }
    /// Provenance identity retains its legacy decoding, then enters the same
    /// file-owned admission as configuration and retained-expression keys.
    pub(super) fn provenance(
        identity: String,
        pair: &Pair<'_, Rule>,
        file: &super::PreprocessedFile,
    ) -> Self {
        let authored = AuthoredExpressionText::redact(pair.clone(), None);
        let key = if authored.has_redactions() {
            Self {
                identity,
                diagnostic: KeyPresentation::Authored(authored),
            }
        } else {
            Self::literal(identity)
        };
        key.admit_in_file(pair, file)
    }
    pub(super) fn into_provenance_segment(
        self,
        kind: super::ProtectedPathKind,
    ) -> super::AttributePathSegment {
        if self.diagnostic.protected() {
            super::AttributePathSegment::ProtectedKey { kind, key: self }
        } else {
            match kind {
                super::ProtectedPathKind::Attribute => {
                    super::AttributePathSegment::Attribute(self.identity)
                }
                super::ProtectedPathKind::Key => super::AttributePathSegment::Key(self.identity),
                super::ProtectedPathKind::StructField => {
                    super::AttributePathSegment::StructField(self.identity)
                }
            }
        }
    }
    pub(super) fn renamed(&self, identity: String) -> Self {
        if self.diagnostic.protected() {
            Self {
                identity,
                diagnostic: KeyPresentation::SecretOwned,
            }
        } else {
            Self::literal(identity)
        }
    }
    pub(super) fn lookup_hash<'a, V>(
        &self,
        values: &'a std::collections::HashMap<String, V>,
    ) -> Option<&'a V> {
        values.get(&self.identity)
    }

    pub(super) fn matches_terminal(&self, name: &str) -> bool {
        self.identity.rsplit('.').next() == Some(name)
    }

    pub(crate) fn transport_identity(&self) -> &str {
        &self.identity
    }

    pub fn matches(&self, name: &str) -> bool {
        self.identity == name
    }

    pub fn diagnostic_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.diagnostic.text().cmp(other.diagnostic.text())
    }

    pub(super) fn with_diagnostic_prefix(self, prefix: &str) -> Self {
        Self {
            identity: format!("{prefix}{}", self.identity),
            diagnostic: match self.diagnostic {
                KeyPresentation::Authored(text) => {
                    KeyPresentation::Authored(text.with_diagnostic_prefix(prefix))
                }
                KeyPresentation::SecretOwned => KeyPresentation::SecretOwned,
            },
        }
    }

    pub fn schema_type(
        &self,
        schema: &crate::schema::ValidatedConfigSchema,
    ) -> Option<crate::schema::RootlessType> {
        schema.rootless_type(&self.identity)
    }

    pub fn lookup<'a, V>(
        &self,
        values: &'a std::collections::BTreeMap<String, V>,
    ) -> Option<&'a V> {
        values.get(&self.identity)
    }

    pub(super) fn effective_attribute<'a>(
        &self,
        schema: &'a crate::schema::ValidatedResourceSchema,
    ) -> Option<crate::schema::EffectiveAttribute<'a>> {
        schema.effective_attribute_by_authored_name(&self.identity)
    }

    pub(super) fn lifecycle_directive(&self) -> Option<crate::resource::LifecycleDirective> {
        crate::resource::LifecycleDirective::from_name(&self.identity)
    }
}

impl From<String> for AttributeKey {
    fn from(identity: String) -> Self {
        Self::literal(identity)
    }
}
impl From<&str> for AttributeKey {
    fn from(identity: &str) -> Self {
        identity.to_owned().into()
    }
}
impl serde::Serialize for AttributeKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.identity)
    }
}
impl<'de> serde::Deserialize<'de> for AttributeKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <String as serde::Deserialize>::deserialize(deserializer).map(Self::literal)
    }
}
impl PartialOrd for AttributeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for AttributeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.identity.cmp(&other.identity)
    }
}
impl indexmap::Equivalent<String> for AttributeKey {
    fn equivalent(&self, key: &String) -> bool {
        self.matches(key)
    }
}
impl indexmap::Equivalent<AttributeKey> for str {
    fn equivalent(&self, key: &AttributeKey) -> bool {
        key.matches(self)
    }
}
impl indexmap::Equivalent<AttributeKey> for String {
    fn equivalent(&self, key: &AttributeKey) -> bool {
        key.matches(self)
    }
}

/// Configuration entries retain their diagnostic keys through all value stages.
/// String identity is available only to serializers at a transport boundary.
// Presentation belongs to the live parse. Persistence retains main's exact
// string-keyed map representation; every decoded key is literal data.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ConfigurationAttributes<V>(indexmap::IndexMap<AttributeKey, V>);

impl<V> Default for ConfigurationAttributes<V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<V: std::fmt::Debug> std::fmt::Debug for ConfigurationAttributes<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl<V> ConfigurationAttributes<V> {
    pub(super) fn deserialize_literal<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, D::Error>
    where
        V: serde::Deserialize<'de>,
    {
        <Self as serde::Deserialize>::deserialize(deserializer)
    }
    pub fn new() -> Self {
        Self(indexmap::IndexMap::new())
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self(indexmap::IndexMap::with_capacity(capacity))
    }
    pub fn insert(&mut self, key: impl Into<AttributeKey>, value: V) -> Option<V> {
        self.0.insert(key.into(), value)
    }
    pub fn entry(
        &mut self,
        key: impl Into<AttributeKey>,
    ) -> indexmap::map::Entry<'_, AttributeKey, V> {
        self.0.entry(key.into())
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&AttributeKey, &mut V) -> bool) {
        self.0.retain(|key, value| keep(key, value));
    }
    pub fn try_map<W, E>(
        &self,
        mut map: impl FnMut(&V) -> Result<W, E>,
    ) -> Result<ConfigurationAttributes<W>, E> {
        self.iter()
            .map(|(key, value)| map(value).map(|value| (key.clone(), value)))
            .collect()
    }
    pub fn into_try_map<W, E>(
        self,
        mut map: impl FnMut(V) -> Result<W, E>,
    ) -> Result<ConfigurationAttributes<W>, E> {
        self.into_iter()
            .map(|(key, value)| map(value).map(|value| (key, value)))
            .collect()
    }
    pub(crate) fn from_literal_map(values: impl IntoIterator<Item = (String, V)>) -> Self {
        values
            .into_iter()
            .map(|(key, value)| (AttributeKey::literal(key), value))
            .collect()
    }

    pub fn public_value_map(
        &self,
    ) -> Result<indexmap::IndexMap<String, V>, ProtectedConfigurationKey>
    where
        V: Clone,
    {
        self.iter()
            .map(|(key, value)| {
                if key.diagnostic.protected() {
                    Err(ProtectedConfigurationKey(key.clone()))
                } else {
                    Ok((key.identity.clone(), value.clone()))
                }
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("configuration key '{0}' cannot be projected as public literal text")]
pub struct ProtectedConfigurationKey(AttributeKey);
impl<V> std::ops::Deref for ConfigurationAttributes<V> {
    type Target = indexmap::IndexMap<AttributeKey, V>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<V> std::ops::DerefMut for ConfigurationAttributes<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<V, K: Into<AttributeKey>> FromIterator<(K, V)> for ConfigurationAttributes<V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(entries: T) -> Self {
        Self(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }
}
impl<V, K: Into<AttributeKey>> From<indexmap::IndexMap<K, V>> for ConfigurationAttributes<V> {
    fn from(entries: indexmap::IndexMap<K, V>) -> Self {
        entries.into_iter().collect()
    }
}
impl<V, K: Into<AttributeKey>> From<std::collections::HashMap<K, V>>
    for ConfigurationAttributes<V>
{
    fn from(entries: std::collections::HashMap<K, V>) -> Self {
        entries.into_iter().collect()
    }
}
impl<V> IntoIterator for ConfigurationAttributes<V> {
    type Item = (AttributeKey, V);
    type IntoIter = indexmap::map::IntoIter<AttributeKey, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl<'a, V> IntoIterator for &'a ConfigurationAttributes<V> {
    type Item = (&'a AttributeKey, &'a V);
    type IntoIter = indexmap::map::Iter<'a, AttributeKey, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
impl<'a, V> IntoIterator for &'a mut ConfigurationAttributes<V> {
    type Item = (&'a AttributeKey, &'a mut V);
    type IntoIter = indexmap::map::IterMut<'a, AttributeKey, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

impl PartialEq for AttributeKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl Eq for AttributeKey {}
impl Hash for AttributeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}
impl PartialEq<str> for AttributeKey {
    fn eq(&self, other: &str) -> bool {
        self.matches(other)
    }
}
impl PartialEq<&str> for AttributeKey {
    fn eq(&self, other: &&str) -> bool {
        self.matches(other)
    }
}
impl PartialEq<String> for &AttributeKey {
    fn eq(&self, other: &String) -> bool {
        self.matches(other)
    }
}

impl<V: PartialEq> PartialEq<indexmap::IndexMap<String, V>> for ConfigurationAttributes<V> {
    fn eq(&self, other: &indexmap::IndexMap<String, V>) -> bool {
        self.len() == other.len()
            && other
                .iter()
                .all(|(key, value)| self.get(key) == Some(value))
    }
}
impl PartialEq<String> for AttributeKey {
    fn eq(&self, other: &String) -> bool {
        self.matches(other)
    }
}
impl std::fmt::Display for AttributeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.diagnostic.text())
    }
}
impl std::fmt::Debug for AttributeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.diagnostic.text(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round14_provider_and_backend_bytes_match_main_string_key_maps() {
        use crate::resource::{
            AbsoluteBindingAddress, ConcreteValue, ExpandedValue, NoPlaceholder,
        };
        use indexmap::IndexMap;
        let name = r#""${secret('CANARY')}""#;
        let input = format!("let hidden = secret({{ {name} = 1 }})");
        let file = super::super::preprocess_file(&input).unwrap();
        let pair = file
            .parse()
            .unwrap()
            .flatten()
            .find(|pair| pair.as_rule() == Rule::map_entry)
            .unwrap()
            .into_inner()
            .next()
            .unwrap();
        let key = AttributeKey::authored_in_file(pair, &file);
        assert_eq!(key.to_string(), "<redacted>");
        assert_eq!(
            serde_json::to_vec(&key).unwrap(),
            serde_json::to_vec(name).unwrap()
        );
        let value = |n| ExpandedValue::Concrete(ConcreteValue::Int(n));
        let attributes: ConfigurationAttributes<_> = [
            (AttributeKey::from("first"), value(1)),
            (key.clone(), value(2)),
            (AttributeKey::from("last"), value(3)),
        ]
        .into_iter()
        .collect();
        let raw: IndexMap<String, ExpandedValue> = [
            ("first".into(), value(1)),
            (name.into(), value(2)),
            ("last".into(), value(3)),
        ]
        .into_iter()
        .collect();
        #[derive(serde::Serialize, serde::Deserialize)]
        struct MainProviderConfig {
            name: String,
            attributes: IndexMap<String, ExpandedValue>,
        }
        #[derive(serde::Serialize, serde::Deserialize)]
        struct MainBackendConfig {
            backend_type: String,
            attributes: std::collections::HashMap<String, ExpandedValue>,
        }
        let raw_backend = raw
            .clone()
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        // Use the same main-map instance's iteration order for both views;
        // independent HashMaps deliberately have independently seeded order.
        let backend_attributes = raw_backend
            .iter()
            .map(|(name, value)| {
                (
                    if key.matches(name) {
                        key.clone()
                    } else {
                        AttributeKey::from(name.clone())
                    },
                    value.clone(),
                )
            })
            .collect();
        let provider: super::super::ProviderConfig<NoPlaceholder, AbsoluteBindingAddress> =
            super::super::ProviderConfig {
                name: "mock".into(),
                attributes: attributes.clone(),
                default_tags: Default::default(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: Default::default(),
                binding: None,
            };
        let backend: super::super::BackendConfig<NoPlaceholder, AbsoluteBindingAddress> =
            super::super::BackendConfig {
                backend_type: "local".into(),
                attributes: backend_attributes,
                attribute_spans: Default::default(),
            };
        let provider_bytes = serde_json::to_vec(&provider).unwrap();
        let main_provider_bytes = serde_json::to_vec(&MainProviderConfig {
            name: "mock".into(),
            attributes: raw.clone(),
        })
        .unwrap();
        assert_eq!(provider_bytes, main_provider_bytes);
        serde_json::from_slice::<MainProviderConfig>(&provider_bytes).unwrap();
        let backend_bytes = serde_json::to_vec(&backend).unwrap();
        assert_eq!(
            backend_bytes,
            serde_json::to_vec(&MainBackendConfig {
                backend_type: "local".into(),
                attributes: raw_backend,
            })
            .unwrap()
        );
        serde_json::from_slice::<MainBackendConfig>(&backend_bytes).unwrap();
        let saved_bytes = [b"[".as_slice(), provider_bytes.as_slice(), b"]".as_slice()].concat();
        let saved: super::super::SavedProviderConfigs =
            serde_json::from_slice(&saved_bytes).unwrap();
        assert_eq!(serde_json::to_vec(&saved).unwrap(), saved_bytes);
    }

    #[test]
    fn round14_deserialization_cannot_mint_authored_keys() {
        assert!(
            serde_json::from_str::<AttributeKey>(r#"{"authored":"x","secret_owned":true}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ConfigurationAttributes<u32>>(
                r#"[[{"authored":"x","secret_owned":true},1]]"#
            )
            .is_err()
        );
        for name in ["x", "secret", "secret(", r#""${secret('CANARY')}""#] {
            let encoded = serde_json::to_string(name).unwrap();
            let key: AttributeKey = serde_json::from_str(&encoded).unwrap();
            assert_eq!(key.to_string(), name);
            assert!(!key.diagnostic.protected());
            let encoded = serde_json::to_string(&indexmap::IndexMap::from([(name, 1u32)])).unwrap();
            let attrs: ConfigurationAttributes<u32> = serde_json::from_str(&encoded).unwrap();
            assert!(attrs.public_value_map().is_ok());
        }
    }

    #[test]
    fn round13_plain_string_conversions_and_deserialization_are_literal() {
        for name in [
            "app-secret",
            "secret",
            "x-secret-y",
            "secret(",
            r#""${secret('PUBLIC')}""#,
        ] {
            let key = AttributeKey::from(name);
            assert_eq!(key.to_string(), name);
            assert_eq!(AttributeKey::from(name.to_owned()).to_string(), name);
            let json = serde_json::to_string(&key).unwrap();
            let decoded: AttributeKey = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.to_string(), name);
            let attrs: ConfigurationAttributes<u32> =
                indexmap::IndexMap::from([(name.to_owned(), 1)]).into();
            let json = serde_json::to_string(&attrs).unwrap();
            let decoded: ConfigurationAttributes<u32> = serde_json::from_str(&json).unwrap();
            assert_eq!(
                decoded.public_value_map().unwrap(),
                indexmap::IndexMap::from([(name.to_owned(), 1)])
            );
        }
    }

    fn authored_key(source: &str) -> AttributeKey {
        let input = format!("provider mock {{ {source} = 1 }}");
        let file = super::super::preprocess_file(&input).unwrap();
        let key = file
            .parse()
            .unwrap()
            .flatten()
            .find(|pair| pair.as_rule() == Rule::attribute)
            .unwrap()
            .into_inner()
            .next()
            .unwrap();
        AttributeKey::authored_in_file(key, &file)
    }

    #[test]
    fn round14_secret_owned_key_round_trip_keeps_identity_as_literal_data() {
        let input = "let hidden = secret({ CANARY = 1 })";
        let file = super::super::preprocess_file(input).unwrap();
        let key = file
            .parse()
            .unwrap()
            .flatten()
            .find(|pair| pair.as_rule() == Rule::map_entry)
            .unwrap()
            .into_inner()
            .next()
            .unwrap();
        let key = AttributeKey::authored_in_file(key, &file);
        let attrs: ConfigurationAttributes<_> = [(key, 42u32)].into_iter().collect();
        let encoded = serde_json::to_string(&attrs).unwrap();
        let restored: ConfigurationAttributes<u32> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.get("CANARY"), Some(&42));
        assert_eq!(restored.keys().next().unwrap().to_string(), "CANARY");
        assert!(restored.public_value_map().is_ok());
        assert_eq!(encoded, r#"{"CANARY":42}"#);
        assert_eq!(serde_json::to_string(&restored).unwrap(), encoded);
    }

    #[test]
    fn round14_authored_key_presentation_is_not_persisted() {
        let names = [r#""${secret('CANARY_A')}""#, r#""${secret('CANARY_B')}""#];
        let values = names
            .into_iter()
            .enumerate()
            .map(|(value, key)| (authored_key(key), value as u32))
            .collect::<ConfigurationAttributes<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values.get(names[0]), Some(&0));
        assert_eq!(values.get(names[1]), Some(&1));
        assert!(!format!("{values:?}").contains("CANARY"));
        assert!(values.public_value_map().is_err());
        let encoded = serde_json::to_string(&values).unwrap();
        assert!(encoded.contains("CANARY_A")); // Raw identity is required on the wire.
        let decoded: ConfigurationAttributes<u32> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(values, decoded);
        assert!(decoded.public_value_map().is_ok());
        assert_eq!(
            decoded.keys().map(ToString::to_string).collect::<Vec<_>>(),
            names
        );
        assert_eq!(encoded, serde_json::to_string(&decoded).unwrap());
    }

    #[test]
    fn round7_literal_keys_keep_public_secret_like_spelling() {
        let names = [
            "my-secret",
            "secret('PUBLIC')",
            "\"${secret('PUBLIC')}\"",
            "a\nb",
            "\0",
        ];
        let values = ConfigurationAttributes::from_literal_map(
            names
                .iter()
                .enumerate()
                .map(|(value, key)| (key.to_string(), value as u32)),
        );
        assert_eq!(
            values.keys().map(ToString::to_string).collect::<Vec<_>>(),
            names
        );
        assert_eq!(
            values
                .public_value_map()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            names
        );
        let encoded = serde_json::to_string(&values).unwrap();
        let mut decoder = serde_json::Deserializer::from_str(&encoded);
        let decoded = ConfigurationAttributes::<u32>::deserialize_literal(&mut decoder).unwrap();
        assert_eq!(decoded, values);
        assert_eq!(
            decoded.keys().map(ToString::to_string).collect::<Vec<_>>(),
            names
        );
    }
}
