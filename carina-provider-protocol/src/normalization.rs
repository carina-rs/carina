//! Values used exclusively by desired normalization.
//!
//! Ordinary [`crate::types::Value`] and its JSON representation are unchanged.
//! Only this codec recognizes handles and escapes user keys beginning with `$`.
//!
//! Ordinary values cannot contain normalization handles:
//! ```compile_fail
//! use carina_provider_protocol::{types::Value, normalization::NormalizationValue};
//! let value: Value = NormalizationValue::Deferred(0);
//! ```

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::types::ResourceId;

/// A normalization wire value. Handles are untrusted per-call slot numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizationValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    StringList(Vec<String>),
    List(Vec<Self>),
    Map(BTreeMap<String, Self>),
    Deferred(u32),
}

/// One complete normalization input/output record, paired by slice position.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizationResource {
    pub id: ResourceId,
    pub attributes: BTreeMap<String, NormalizationValue>,
}

/// A location inside one top-level attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizationPathSegment {
    Key(String),
    Index(usize),
}

/// Structural normalization failures. These never carry deferred payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizationErrorKind {
    MissingHandle {
        slot: u32,
    },
    DuplicateHandle {
        slot: u32,
    },
    UnknownHandle {
        slot: u32,
    },
    ForeignHandle {
        slot: u32,
    },
    ResourceCountMismatch {
        expected: usize,
        actual: usize,
    },
    ResourceIdMismatch {
        expected: Box<ResourceId>,
        actual: Box<ResourceId>,
    },
    MalformedHandle,
    DuplicateKey {
        key: String,
    },
    Conversion,
}

/// A typed failure with resource, top-level attribute and nested path context.
#[derive(Debug)]
pub struct NormalizationError {
    pub resource_id: Box<ResourceId>,
    /// Original batch position, when the error originates at the host boundary.
    pub resource_index: Option<usize>,
    pub attribute: String,
    pub path: Vec<NormalizationPathSegment>,
    pub kind: NormalizationErrorKind,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl NormalizationError {
    pub fn new(
        resource_id: ResourceId,
        attribute: impl Into<String>,
        kind: NormalizationErrorKind,
    ) -> Self {
        Self {
            resource_id: Box::new(resource_id),
            resource_index: None,
            attribute: attribute.into(),
            path: Vec::new(),
            kind,
            source: None,
        }
    }

    pub fn with_path(mut self, path: Vec<NormalizationPathSegment>) -> Self {
        self.path = path;
        self
    }

    pub fn with_resource_index(mut self, index: usize) -> Self {
        self.resource_index = Some(index);
        self
    }

    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}

impl fmt::Display for NormalizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "desired normalization of '{}.{}.{}' attribute '{}'",
            self.resource_id.provider,
            self.resource_id.resource_type,
            self.resource_id.identity,
            self.attribute
        )?;
        for segment in &self.path {
            match segment {
                NormalizationPathSegment::Key(key) => write!(f, "[{key:?}]")?,
                NormalizationPathSegment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        match &self.kind {
            NormalizationErrorKind::MissingHandle { slot } => {
                write!(f, ": missing deferred handle {slot}")
            }
            NormalizationErrorKind::DuplicateHandle { slot } => {
                write!(f, ": duplicate deferred handle {slot}")
            }
            NormalizationErrorKind::UnknownHandle { slot } => {
                write!(f, ": unknown deferred handle {slot}")
            }
            NormalizationErrorKind::ForeignHandle { slot } => {
                write!(f, ": foreign deferred handle {slot}")
            }
            NormalizationErrorKind::ResourceCountMismatch { expected, actual } => {
                write!(f, ": returned {actual} resources; expected {expected}")
            }
            NormalizationErrorKind::ResourceIdMismatch { expected, actual } => {
                write!(
                    f,
                    ": returned resource id {actual:?}; expected {expected:?}"
                )
            }
            NormalizationErrorKind::MalformedHandle => f.write_str(": malformed deferred handle"),
            NormalizationErrorKind::DuplicateKey { key } => write!(f, ": duplicate key {key:?}"),
            NormalizationErrorKind::Conversion => {
                if self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.is::<serde_json::Error>())
                {
                    f.write_str(": invalid JSON")?;
                } else {
                    f.write_str(": value conversion failed")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for NormalizationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn Error + 'static))
    }
}

// The provider-error transport has a string-only cause channel. Preserve typed
// errors until this terminal conversion, then render each source once.
impl From<NormalizationError> for crate::types::ProviderError {
    fn from(error: NormalizationError) -> Self {
        let mut causes: Vec<String> = Vec::new();
        let mut source = error.source();
        while let Some(current) = source {
            causes.push(current.to_string());
            source = current.source();
        }
        let message = error.to_string();
        let cause = causes.join(": ");
        let cause = (!cause.is_empty()).then_some(cause);
        Self {
            kind: crate::types::ProviderErrorKind::Internal,
            message,
            resource_id: Some(*error.resource_id),
            cause,
            provider_name: None,
            operation: Some("normalize_desired".into()),
            status: None,
            code: None,
            request_id: None,
        }
    }
}

impl NormalizationValue {
    /// Encode a complex normalization value; ordinary JSON codecs do not use this.
    pub fn to_json(&self, id: &ResourceId, attribute: &str) -> Result<String, NormalizationError> {
        fn encode(
            value: &NormalizationValue,
            id: &ResourceId,
            attribute: &str,
            path: &mut Vec<NormalizationPathSegment>,
        ) -> Result<serde_json::Value, NormalizationError> {
            Ok(match value {
                NormalizationValue::Bool(value) => (*value).into(),
                NormalizationValue::Int(value) => (*value).into(),
                NormalizationValue::Float(value) => serde_json::Number::from_f64(*value)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| {
                        NormalizationError::new(
                            id.clone(),
                            attribute,
                            NormalizationErrorKind::Conversion,
                        )
                        .with_path(path.clone())
                    })?,
                NormalizationValue::String(value) => value.clone().into(),
                NormalizationValue::StringList(values) => {
                    serde_json::Value::Array(values.iter().cloned().map(Into::into).collect())
                }
                NormalizationValue::Deferred(slot) => serde_json::json!({"$carina_deferred": slot}),
                NormalizationValue::List(values) => {
                    let mut result = Vec::with_capacity(values.len());
                    for (index, value) in values.iter().enumerate() {
                        path.push(NormalizationPathSegment::Index(index));
                        result.push(encode(value, id, attribute, path)?);
                        path.pop();
                    }
                    serde_json::Value::Array(result)
                }
                NormalizationValue::Map(values) => {
                    let mut result = serde_json::Map::new();
                    for (key, value) in values {
                        path.push(NormalizationPathSegment::Key(key.clone()));
                        let encoded = encode(value, id, attribute, path)?;
                        path.pop();
                        let key = if key.starts_with('$') {
                            format!("${key}")
                        } else {
                            key.clone()
                        };
                        result.insert(key, encoded);
                    }
                    serde_json::Value::Object(result)
                }
            })
        }
        Ok(encode(self, id, attribute, &mut Vec::new())?.to_string())
    }

    /// Decode normalization JSON without collapsing duplicate object keys.
    pub fn from_json(
        json: &str,
        id: &ResourceId,
        attribute: &str,
    ) -> Result<Self, NormalizationError> {
        let raw: RawJson = serde_json::from_str(json).map_err(|source| {
            NormalizationError::new(id.clone(), attribute, NormalizationErrorKind::Conversion)
                .with_source(source)
        })?;
        raw.decode(id, attribute, &mut Vec::new())
    }

    /// Decode a WIT `list-val`, checking its JSON root kind.
    pub fn from_json_list(
        json: &str,
        id: &ResourceId,
        attribute: &str,
    ) -> Result<Self, NormalizationError> {
        match Self::from_json(json, id, attribute)? {
            value @ Self::List(_) => Ok(value),
            _ => Err(NormalizationError::new(
                id.clone(),
                attribute,
                NormalizationErrorKind::Conversion,
            )),
        }
    }

    /// Decode a WIT `map-val`, checking its JSON root kind.
    pub fn from_json_map(
        json: &str,
        id: &ResourceId,
        attribute: &str,
    ) -> Result<Self, NormalizationError> {
        match Self::from_json(json, id, attribute)? {
            value @ Self::Map(_) => Ok(value),
            _ => Err(NormalizationError::new(
                id.clone(),
                attribute,
                NormalizationErrorKind::Conversion,
            )),
        }
    }

    /// Decode a WIT `string-list-val`; every element must be a known string.
    pub fn from_json_string_list(
        json: &str,
        id: &ResourceId,
        attribute: &str,
    ) -> Result<Self, NormalizationError> {
        let Self::List(values) = Self::from_json_list(json, id, attribute)? else {
            unreachable!()
        };
        let values = values
            .into_iter()
            .enumerate()
            .map(|(index, value)| match value {
                Self::String(value) => Ok(value),
                _ => Err(NormalizationError::new(
                    id.clone(),
                    attribute,
                    NormalizationErrorKind::Conversion,
                )
                .with_path(vec![NormalizationPathSegment::Index(index)])),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::StringList(values))
    }
}

// Objects retain their entries until duplicate keys have been rejected. Parsing
// into serde_json::Value first would silently discard duplicate handle markers.
enum RawJson {
    Scalar(serde_json::Value),
    List(Vec<Self>),
    Map(Vec<(String, Self)>),
}

impl<'de> serde::Deserialize<'de> for RawJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RawJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a normalization JSON value")
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|value| RawJson::Scalar(serde_json::Value::Number(value)))
                    .ok_or_else(|| E::custom("unsupported number"))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(value.into()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(value.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(RawJson::Scalar(serde_json::Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut values: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(value) = values.next_element()? {
                    result.push(value);
                }
                Ok(RawJson::List(result))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<Self::Value, A::Error> {
                let mut result = Vec::new();
                while let Some(entry) = entries.next_entry()? {
                    result.push(entry);
                }
                Ok(RawJson::Map(result))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl RawJson {
    fn decode(
        self,
        id: &ResourceId,
        attribute: &str,
        path: &mut Vec<NormalizationPathSegment>,
    ) -> Result<NormalizationValue, NormalizationError> {
        let error =
            |kind| NormalizationError::new(id.clone(), attribute, kind).with_path(path.clone());
        Ok(match self {
            Self::Scalar(serde_json::Value::Bool(value)) => NormalizationValue::Bool(value),
            Self::Scalar(serde_json::Value::String(value)) => NormalizationValue::String(value),
            Self::Scalar(serde_json::Value::Number(value)) => {
                if let Some(value) = value.as_i64() {
                    NormalizationValue::Int(value)
                } else if let Some(value) = value.as_f64() {
                    NormalizationValue::Float(value)
                } else {
                    return Err(error(NormalizationErrorKind::Conversion));
                }
            }
            Self::Scalar(_) => return Err(error(NormalizationErrorKind::Conversion)),
            Self::List(values) => {
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.into_iter().enumerate() {
                    path.push(NormalizationPathSegment::Index(index));
                    result.push(value.decode(id, attribute, path)?);
                    path.pop();
                }
                NormalizationValue::List(result)
            }
            Self::Map(entries) => {
                let mut keys = std::collections::BTreeSet::new();
                for (key, _) in &entries {
                    if !keys.insert(key) {
                        return Err(error(NormalizationErrorKind::DuplicateKey {
                            key: key.clone(),
                        }));
                    }
                }
                if entries.iter().any(|(key, _)| key == "$carina_deferred") {
                    if entries.len() != 1 {
                        return Err(error(NormalizationErrorKind::MalformedHandle));
                    }
                    let (_, value) = entries.into_iter().next().expect("one entry");
                    let Self::Scalar(serde_json::Value::Number(number)) = value else {
                        return Err(error(NormalizationErrorKind::MalformedHandle));
                    };
                    let Some(slot) = number.as_u64().and_then(|value| u32::try_from(value).ok())
                    else {
                        return Err(error(NormalizationErrorKind::MalformedHandle));
                    };
                    return Ok(NormalizationValue::Deferred(slot));
                }
                let mut result = BTreeMap::new();
                for (key, value) in entries {
                    let key = if key.starts_with("$$") {
                        key[1..].to_string()
                    } else if key.starts_with('$') {
                        return Err(NormalizationError::new(
                            id.clone(),
                            attribute,
                            NormalizationErrorKind::MalformedHandle,
                        )
                        .with_path(path.clone()));
                    } else {
                        key
                    };
                    path.push(NormalizationPathSegment::Key(key.clone()));
                    let value = value.decode(id, attribute, path)?;
                    path.pop();
                    result.insert(key, value);
                }
                NormalizationValue::Map(result)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_checked_json_decoders_share_shape_validation_and_error_sources() {
        assert_eq!(
            NormalizationValue::from_json_list("[]", &id(), "rules").unwrap(),
            NormalizationValue::List(vec![])
        );
        assert_eq!(
            NormalizationValue::from_json_map("{}", &id(), "rules").unwrap(),
            NormalizationValue::Map(BTreeMap::new())
        );
        assert_eq!(
            NormalizationValue::from_json_string_list(r#"["known"]"#, &id(), "rules").unwrap(),
            NormalizationValue::StringList(vec!["known".into()])
        );
        type Decoder =
            fn(&str, &ResourceId, &str) -> Result<NormalizationValue, NormalizationError>;
        for (decode, json) in [
            (NormalizationValue::from_json_list as Decoder, "{}"),
            (NormalizationValue::from_json_map as Decoder, "[]"),
            (NormalizationValue::from_json_string_list as Decoder, "{}"),
            (
                NormalizationValue::from_json_string_list as Decoder,
                r#"["known",{"$carina_deferred":7}]"#,
            ),
        ] {
            let error = decode(json, &id(), "rules").unwrap_err();
            assert!(matches!(error.kind, NormalizationErrorKind::Conversion));
            assert_eq!(*error.resource_id, id());
            assert_eq!(error.attribute, "rules");
            if json.starts_with('[') && json != "[]" {
                assert_eq!(error.path, vec![NormalizationPathSegment::Index(1)]);
            }
        }
        for decode in [
            NormalizationValue::from_json_list as Decoder,
            NormalizationValue::from_json_map,
            NormalizationValue::from_json_string_list,
        ] {
            let error = decode("[", &id(), "rules").unwrap_err();
            assert!(error.source().unwrap().is::<serde_json::Error>());
        }
    }

    fn id() -> ResourceId {
        ResourceId {
            provider: "mock".into(),
            resource_type: "normalization".into(),
            identity: "example".into(),
        }
    }

    #[test]
    fn normalization_json_round_trips_handles_and_literal_reserved_keys() {
        let value = NormalizationValue::Map(BTreeMap::from([
            ("$carina_deferred".into(), NormalizationValue::Int(12)),
            ("$$or".into(), NormalizationValue::String("literal".into())),
            (
                "items".into(),
                NormalizationValue::List(vec![
                    NormalizationValue::Deferred(0),
                    NormalizationValue::Deferred(u32::MAX),
                ]),
            ),
        ]));
        let json = value.to_json(&id(), "rules").unwrap();
        assert!(json.contains("\"$$carina_deferred\""));
        assert_eq!(
            NormalizationValue::from_json(&json, &id(), "rules").unwrap(),
            value
        );
        // Marker-shaped ordinary maps retain their literal meaning.
        let ordinary: crate::types::Value =
            serde_json::from_str(r#"{"$carina_deferred":0}"#).unwrap();
        assert!(matches!(ordinary, crate::types::Value::Map(_)));
        assert_eq!(
            serde_json::to_string(&ordinary).unwrap(),
            r#"{"$carina_deferred":0}"#
        );
    }

    #[test]
    fn normalization_json_seeded_trees_round_trip() {
        const KEYS: &[&str] = &[
            "$carina_deferred",
            "$$carina_deferred",
            "$",
            "$$",
            "$secret",
            "$$$$$$$$carina_deferred",
            "$$$$$$$$",
            "$$$$$$$$secret",
            "ordinary",
            "items",
            "",
            "quote\"and\\slash",
            "日本語",
        ];
        const STRINGS: &[&str] = &[
            "",
            "ordinary",
            "$carina_deferred",
            r#"{"$carina_deferred":0}"#,
            "line\nbreak\t\0",
            "quote\"and\\slash",
            "日本語🦀",
        ];

        struct SplitMix64(u64);
        impl SplitMix64 {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut value = self.0;
                value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                value ^ (value >> 31)
            }

            fn choose(&mut self, count: usize) -> usize {
                (self.next() % count as u64) as usize
            }

            // StringList's discriminator belongs to WIT, not JSON. JSON arrays
            // use List here, including empty arrays and arrays of strings.
            fn tree(&mut self, depth: usize) -> NormalizationValue {
                match self.choose(if depth == 0 { 5 } else { 7 }) {
                    0 => NormalizationValue::Bool(self.next() & 1 != 0),
                    1 => NormalizationValue::Int(match self.choose(4) {
                        0 => i64::MIN,
                        1 => i64::MAX,
                        2 => 0,
                        _ => self.next() as i64,
                    }),
                    2 => {
                        // Finite dyadic fractions avoid unrelated decimal-parser
                        // precision differences while exercising float scalars.
                        let value = if self.choose(4) == 0 {
                            -0.0
                        } else {
                            f64::from(self.next() as i16) / 8.0
                        };
                        NormalizationValue::Float(value)
                    }
                    3 => {
                        let text = STRINGS[self.choose(STRINGS.len())];
                        let text = if self.choose(2) == 0 {
                            text.to_string()
                        } else {
                            format!("{text}-{:016x}", self.next())
                        };
                        NormalizationValue::String(text)
                    }
                    4 => NormalizationValue::Deferred(match self.choose(4) {
                        0 => 0,
                        1 => u32::MAX,
                        _ => self.next() as u32,
                    }),
                    5 => {
                        let values = (0..self.choose(5)).map(|_| self.tree(depth - 1)).collect();
                        NormalizationValue::List(values)
                    }
                    6 => {
                        let mut values = BTreeMap::new();
                        for _ in 0..self.choose(5) {
                            let key = KEYS[self.choose(KEYS.len())];
                            values.insert(key.to_string(), self.tree(depth - 1));
                        }
                        NormalizationValue::Map(values)
                    }
                    _ => unreachable!(),
                }
            }
        }

        fn handles(value: &NormalizationValue) -> Vec<u32> {
            match value {
                NormalizationValue::Deferred(slot) => vec![*slot],
                NormalizationValue::List(values) => values.iter().flat_map(handles).collect(),
                NormalizationValue::Map(values) => values.values().flat_map(handles).collect(),
                _ => Vec::new(),
            }
        }

        let resource_id = id();
        for case in 0..5_000_u64 {
            let seed = 0x4194_5eed_d011_a425_u64.wrapping_add(case);
            let original = SplitMix64(seed).tree(4);
            let json = original
                .to_json(&resource_id, "generated")
                .unwrap_or_else(|error| panic!("seed={seed:#018x}: encode failed: {error}"));
            let decoded = NormalizationValue::from_json(&json, &resource_id, "generated")
                .unwrap_or_else(|error| {
                    panic!("seed={seed:#018x}: decode failed: {error}; JSON={json}")
                });
            assert_eq!(
                handles(&decoded),
                handles(&original),
                "seed={seed:#018x}: deferred slots changed; JSON={json}"
            );
            assert_eq!(decoded, original, "seed={seed:#018x}; JSON={json}");
        }
    }

    #[test]
    fn normalization_json_rejects_malformed_handles_and_duplicate_keys() {
        for json in [
            r#"{"$carina_deferred":-1}"#,
            r#"{"$carina_deferred":4294967296}"#,
            r#"{"$carina_deferred":1.5}"#,
            r#"{"$carina_deferred":"0"}"#,
            r#"{"$carina_deferred":null}"#,
            r#"{"$carina_deferred":true}"#,
            r#"{"$carina_deferred":[]}"#,
            r#"{"$carina_deferred":0,"extra":1}"#,
        ] {
            assert!(matches!(
                NormalizationValue::from_json(json, &id(), "rules")
                    .unwrap_err()
                    .kind,
                NormalizationErrorKind::MalformedHandle
            ));
        }
        for json in [
            r#"{"key":0,"key":1}"#,
            r#"[{"$carina_deferred":0,"$carina_deferred":1}]"#,
        ] {
            assert!(matches!(
                NormalizationValue::from_json(json, &id(), "rules")
                    .unwrap_err()
                    .kind,
                NormalizationErrorKind::DuplicateKey { .. }
            ));
        }
    }

    #[test]
    fn normalization_json_error_has_context_source_and_no_payload() {
        let payload = r#"{"private-value"#;
        let error = NormalizationValue::from_json(payload, &id(), "rules").unwrap_err();
        assert_eq!(*error.resource_id, id());
        assert_eq!(error.attribute, "rules");
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(!format!("{error:?}").contains("private-value"));
    }
}
