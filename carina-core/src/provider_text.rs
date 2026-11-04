//! Text supplied by providers for host diagnostics.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum stored diagnostic size, including a truncation marker.
pub const MAX_PROVIDER_TEXT_BYTES: usize = 8 * 1024;

/// Provider-supplied data with a safe diagnostic representation.
///
/// Matching, lookup and serialization use the original value through explicit
/// accessors. Formatting always passes through [`ProviderText`].
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderString(String);

impl ProviderString {
    /// Original data for semantic operations; use Display for diagnostics.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper without changing the original data.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for ProviderString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ProviderString {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for ProviderString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ProviderText::new(&self.0).fmt(f)
    }
}

impl fmt::Debug for ProviderString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ProviderString")
            .field(&ProviderText::new(&self.0).as_str())
            .finish()
    }
}

impl PartialEq<str> for ProviderString {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ProviderString {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for ProviderString {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

/// Provider diagnostic text, sanitized before it can be displayed.
/// Use [`ProviderString`] when the original string also has semantic meaning.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderText(String);

impl ProviderText {
    pub fn new(text: impl Into<String>) -> Self {
        let mut builder = ProviderTextBuilder::default();
        builder.push_str(&text.into());
        builder.finish()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Incremental form of the same sanitizer, retaining only a bounded prefix.
/// The final marker includes bytes omitted across every supplied chunk.
#[derive(Default)]
pub struct ProviderTextBuilder {
    prefix: String,
    sanitized_bytes: usize,
    truncated: bool,
}

impl ProviderTextBuilder {
    pub fn push_str(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\0'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {
                    for escaped in c.escape_unicode() {
                        self.push_sanitized_char(escaped);
                    }
                }
                c => self.push_sanitized_char(c),
            }
        }
    }

    fn push_sanitized_char(&mut self, c: char) {
        self.sanitized_bytes = self.sanitized_bytes.saturating_add(c.len_utf8());
        if !self.truncated && self.prefix.len() + c.len_utf8() <= MAX_PROVIDER_TEXT_BYTES {
            self.prefix.push(c);
        } else {
            self.truncated = true;
        }
    }

    pub(crate) fn retained_len(&self) -> usize {
        self.prefix.len()
    }

    pub fn finish(self) -> ProviderText {
        self.finish_with_prefix_len().0
    }

    pub(crate) fn finish_with_prefix_len(mut self) -> (ProviderText, usize) {
        let mut retained = self.prefix.len();
        if self.sanitized_bytes > MAX_PROVIDER_TEXT_BYTES {
            let mut end = self.prefix.len();
            loop {
                let marker = format!("… (truncated, {} bytes)", self.sanitized_bytes - end);
                if end + marker.len() <= MAX_PROVIDER_TEXT_BYTES {
                    retained = end;
                    self.prefix.truncate(end);
                    self.prefix.push_str(&marker);
                    break;
                }
                end = MAX_PROVIDER_TEXT_BYTES - marker.len();
                while !self.prefix.is_char_boundary(end) {
                    end -= 1;
                }
            }
        }
        (ProviderText(self.prefix), retained)
    }
}

impl From<String> for ProviderText {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for ProviderText {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

// Borrowing exposes only the already sanitized, immutable text.
impl Deref for ProviderText {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ProviderText {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<String> for ProviderText {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ProviderText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for ProviderText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<'de> Deserialize<'de> for ProviderText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl fmt::Display for ProviderText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A provider's typed error and its sanitized, bounded diagnostic.
///
/// The complete source chain is captured at construction. Display is terminal;
/// callers can still inspect the original error through [`Self::error`].
pub struct ProviderDiagnostic {
    error: Box<dyn std::error::Error + Send + Sync>,
    text: ProviderText,
}

impl fmt::Debug for ProviderDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderDiagnostic")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

impl ProviderDiagnostic {
    pub fn new(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        struct Chain<'a>(&'a (dyn std::error::Error + 'static));
        impl fmt::Display for Chain<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)?;
                let mut source = self.0.source();
                while let Some(error) = source {
                    write!(f, ": {error}")?;
                    source = error.source();
                }
                Ok(())
            }
        }
        let text = ProviderText::new(Chain(&error).to_string());
        Self {
            error: Box::new(error),
            text,
        }
    }

    pub fn error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.error.as_ref()
    }
}

impl fmt::Display for ProviderDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt(f)
    }
}

impl std::error::Error for ProviderDiagnostic {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_text_builder_bounds_storage_and_counts_the_entire_tail() {
        let mut builder = ProviderTextBuilder::default();
        for _ in 0..32 {
            builder.push_str(&"x".repeat(1024));
            assert!(builder.prefix.len() <= MAX_PROVIDER_TEXT_BYTES);
            assert!(builder.prefix.capacity() <= MAX_PROVIDER_TEXT_BYTES);
        }
        builder.push_str("\x1b[2J");
        assert!(builder.prefix.len() <= MAX_PROVIDER_TEXT_BYTES);
        let text = builder.finish();
        assert!(text.len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(!text.contains('\x1b'));
        let (prefix, marker) = text.split_once("… (truncated, ").unwrap();
        let omitted: usize = marker.strip_suffix(" bytes)").unwrap().parse().unwrap();
        assert_eq!(prefix.len() + omitted, 32 * 1024 + "\\u{1b}[2J".len());
    }

    #[test]
    fn provider_text_escapes_terminal_sequences() {
        let text = ProviderText::new("\x1b]52;c;ZXZpbA==\x07 \x1b[2J");
        assert_eq!(text.as_str(), "\\u{1b}]52;c;ZXZpbA==\\u{7} \\u{1b}[2J");
        assert!(!text.to_string().contains(['\x1b', '\x07']));
    }

    #[test]
    fn provider_text_neutralizes_sgr_cursor_and_osc_sequences() {
        for raw in [
            "\x1b[31mred\x1b[0m",
            "\x1b[5A",
            "\x1b[H",
            "\x1b]0;evil\x07",
            "\x1b]0;evil\x1b\\",
        ] {
            let text = ProviderText::new(raw);
            assert!(!text.as_bytes().contains(&0x1b));
            assert!(!text.as_bytes().contains(&0x07));
            assert!(text.contains("\\u{1b}"), "{text}");
        }
    }

    #[test]
    fn provider_text_utf8_boundary_drops_three_and_four_byte_chars_whole() {
        // Exercise both the initial byte limit and the cut after reserving
        // space for the truncation marker.
        let marker_size = "… (truncated, 12317 bytes)".len();
        for c in ['あ', '🦀'] {
            for leading in [
                MAX_PROVIDER_TEXT_BYTES - 1,
                MAX_PROVIDER_TEXT_BYTES - marker_size - 1,
            ] {
                let raw = format!(
                    "{}{c}{}",
                    "x".repeat(leading),
                    "y".repeat(20 * 1024 - leading - c.len_utf8())
                );
                let expected_prefix =
                    "x".repeat(leading.min(MAX_PROVIDER_TEXT_BYTES - marker_size));
                let omitted = raw.len() - expected_prefix.len();
                let text = ProviderText::new(raw);
                assert!(std::str::from_utf8(text.as_bytes()).is_ok());
                assert!(!text.contains(c));
                assert!(text.len() <= MAX_PROVIDER_TEXT_BYTES);
                assert_eq!(
                    text.as_str(),
                    format!("{expected_prefix}… (truncated, {omitted} bytes)")
                );
            }
        }
    }

    #[test]
    fn provider_text_preserves_newlines_and_tabs_but_escapes_other_controls() {
        let text = ProviderText::new("line\n\tindent\r\x7f\u{009b}\0");
        assert_eq!(text.as_str(), "line\n\tindent\\u{d}\\u{7f}\\u{9b}\\u{0}");
        for control in ('\0'..='\u{9f}').filter(|c| c.is_control()) {
            let text = ProviderText::new(control.to_string());
            let expected = match control {
                '\n' | '\t' => control.to_string(),
                c => c.escape_unicode().to_string(),
            };
            assert_eq!(text.as_str(), expected);
        }
    }

    #[test]
    fn provider_text_caps_twenty_kib_with_exact_omitted_byte_count() {
        let text = ProviderText::new("x".repeat(20 * 1024));
        let marker = "… (truncated, 12316 bytes)";
        assert_eq!(text.as_str(), format!("{}{marker}", "x".repeat(8164)));
        assert_eq!(text.as_str().len(), MAX_PROVIDER_TEXT_BYTES);
    }

    #[test]
    fn provider_text_truncates_sanitized_utf8_at_a_character_boundary() {
        let raw = format!("{}{}", "\x1b".repeat(1360), "界".repeat(5000));
        let sanitized = format!("{}{}", "\\u{1b}".repeat(1360), "界".repeat(5000));
        let text = ProviderText::new(raw);
        let (prefix, marker) = text.as_str().split_once("… (truncated, ").unwrap();
        assert!(text.as_str().len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(sanitized.starts_with(prefix));
        assert!(prefix.ends_with('界'));
        assert_eq!(marker, format!("{} bytes)", sanitized.len() - prefix.len()));
        assert!(std::str::from_utf8(text.as_str().as_bytes()).is_ok());
    }

    #[test]
    fn provider_text_caps_twenty_kib_on_a_multibyte_boundary() {
        let raw = format!("{}é", "界".repeat(6826));
        assert_eq!(raw.len(), 20 * 1024);
        let text = ProviderText::new(raw);
        assert_eq!(
            text.as_str(),
            format!("{}… (truncated, 12317 bytes)", "界".repeat(2721))
        );
        assert_eq!(text.as_str().len(), MAX_PROVIDER_TEXT_BYTES - 1);
    }

    #[test]
    fn provider_text_leaves_text_at_the_limit_unchanged() {
        let raw = "x".repeat(MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(ProviderText::new(&raw).as_str(), raw);
    }

    #[test]
    fn provider_text_all_construction_paths_sanitize() {
        let raw = "\x1b[2J";
        let json = serde_json::to_string(raw).unwrap();
        let decoded: ProviderText = serde_json::from_str(&json).unwrap();
        for text in [
            ProviderText::from(raw),
            ProviderText::from(raw.to_string()),
            decoded,
        ] {
            assert_eq!(text.as_str(), "\\u{1b}[2J");
            assert_eq!(serde_json::to_string(&text).unwrap(), r#""\\u{1b}[2J""#);
        }
    }

    #[test]
    fn provider_text_native_errors_sanitize_messages_metadata_and_typed_causes() {
        use crate::provider::ProviderError;
        let raw = "\x1b]52;c;ZXZpbA==\x07\x1b[2J";
        for error in [
            ProviderError::invalid_input(raw),
            ProviderError::api_error(raw),
            ProviderError::not_found(raw),
            ProviderError::timeout(raw),
            ProviderError::internal(raw),
            ProviderError::internal_caused_by("native error", std::io::Error::other(raw)),
        ] {
            let error = error
                .for_provider(raw)
                .with_operation(raw)
                .with_code(raw)
                .with_request_id(raw);
            assert!(!error.to_string().contains(['\x1b', '\x07']));
            assert_eq!(
                error.detail().provider_name.as_deref(),
                Some("\\u{1b}]52;c;ZXZpbA==\\u{7}\\u{1b}[2J")
            );
            if let Some(cause) = error.detail().cause() {
                assert!(cause.downcast_ref::<std::io::Error>().is_some());
            }
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn provider_text_semantic_strings_keep_raw_equality_and_serialization() {
        let raw = format!("key\x1b[2J{}", "x".repeat(9000));
        let data = ProviderString::from(raw.clone());
        let label = data.to_string();
        assert!(!label.contains('\x1b'));
        assert!(label.len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(label.contains("\\u{1b}[2J"));
        assert_ne!(data, ProviderString::from(label));
        assert_eq!(data.as_str(), raw);
        let encoded = serde_json::to_string(&data).unwrap();
        assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), raw);
        let decoded: ProviderString = serde_json::from_str(&encoded).unwrap();
        assert_eq!(data, decoded);
        assert_eq!(decoded.into_string(), raw);
    }

    #[test]
    fn provider_text_partial_read_preserves_raw_keys_for_lookup_and_writeback() {
        use crate::binding_index::{AppliedOperation, PartialReadBack};
        use crate::provider::{PartialReadDiagnostic, ReadBackEnd};
        use crate::resource::{ResourceId, State};

        for raw in ["computed\x1b[2J".to_string(), "x".repeat(9000)] {
            let diagnostic =
                PartialReadDiagnostic::new("incomplete".into(), vec![raw.clone()]).unwrap();
            let read_back = PartialReadBack::from_diagnostic(
                AppliedOperation::Create,
                &diagnostic,
                ReadBackEnd::ProviderReported,
            );
            assert!(read_back.attribute_is_missing(&raw));
            let mut state = diagnostic.into_state_for_writeback(State::existing(
                ResourceId::with_identity("test", "example"),
                Default::default(),
            ));
            assert!(
                state
                    .partial_read
                    .as_ref()
                    .unwrap()
                    .missing_attributes()
                    .contains(&raw)
            );
            state.materialize_partial_read_unknowns();
            assert!(state.attributes.contains_key(&raw));
        }
    }

    #[test]
    fn provider_text_error_resource_identity_stays_raw() {
        use crate::provider::ProviderError;
        use crate::resource::ResourceId;
        let raw = "example\x1b[2J";
        let id = ResourceId::with_identity("test", raw);
        let error = ProviderError::internal("failed").for_resource(id.clone());
        assert_eq!(
            error
                .detail()
                .resource_id
                .as_ref()
                .unwrap()
                .as_resource_id(),
            &id
        );
        assert!(error.to_string().contains("example\\u{1b}[2J"));
        assert!(!error.to_string().contains('\x1b'));
        assert_eq!(error.to_wire().resource_id.unwrap().identity, raw);
    }

    #[test]
    fn provider_text_enum_metadata_stays_raw_and_displays_safely() {
        use crate::schema::{
            CompiledAttributeType, DiagnosticValue, ExpectedEnumVariant, InvalidEnumValueKind,
            TypeError, TypeIdentity,
        };
        let raw = "bad\x1b[2J\x07";
        let expected = ExpectedEnumVariant::from_namespaced(Some(raw), raw, raw, false);
        assert_eq!(expected.provider.as_deref(), Some(raw));
        assert_eq!(expected.value.as_str(), raw);
        assert!(!expected.to_string().contains(['\x1b', '\x07']));
        assert!(expected.to_string().contains("\\u{1b}[2J\\u{7}"));
        let schema = CompiledAttributeType::enum_(
            TypeIdentity::new(None::<&str>, Vec::<String>::new(), raw),
            Some(vec![raw.into()]),
            vec![],
            None,
            None,
        );
        let value: crate::resource::Value =
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::enum_identifier(raw));
        assert!(schema.validate(&value).is_ok());
        let invalid: crate::resource::Value = crate::resource::Value::Concrete(
            crate::resource::ConcreteValue::enum_identifier("missing"),
        );
        assert!(
            !schema
                .validate(&invalid)
                .unwrap_err()
                .to_string()
                .contains(['\x1b', '\x07'])
        );
        for error in [
            TypeError::InvalidEnumVariant {
                value: DiagnosticValue::from("missing"),
                value_kind: InvalidEnumValueKind::Identifier,
                attribute: Some(raw.into()),
                type_name: Some(raw.into()),
                expected: vec![expected],
            },
            TypeError::value_type_mismatch(raw.into(), "Int".into()),
            TypeError::MissingRequired { name: raw.into() },
            TypeError::UnknownAttribute {
                name: "unknown".into(),
                suggestion: Some(raw.into()),
            },
            TypeError::UnknownStructField {
                struct_name: raw.into(),
                field: "unknown".into(),
                suggestion: Some(raw.into()),
            },
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains(['\x1b', '\x07']), "{rendered:?}");
            assert!(rendered.contains("\\u{1b}[2J\\u{7}"));
        }
    }

    #[test]
    fn provider_text_schema_locations_preserve_raw_attribute_paths() {
        use crate::schema::SchemaValidationIssueLocation;
        let raw = "name\x1b[2J";
        let location = SchemaValidationIssueLocation::Attribute(raw.into());
        assert_eq!(location.attribute_path(), Some(raw));
        assert_eq!(location.to_string(), "name\\u{1b}[2J");
    }

    #[test]
    fn provider_text_partial_read_deserialization_cannot_bypass_sanitizing() {
        use crate::provider::PartialReadDiagnostic;
        use crate::resource::{PendingReRead, ReReadReason};
        let raw = "\x1b[2J\x07";
        let json = serde_json::json!({"reason": raw, "missing_attributes": [raw]});
        let decoded: PartialReadDiagnostic = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.reason(), "\\u{1b}[2J\\u{7}");
        assert_eq!(decoded.raw_reason(), raw);
        assert_eq!(decoded.missing_attributes()[0].as_str(), raw);
        assert_eq!(
            decoded.missing_attributes()[0].to_string(),
            "\\u{1b}[2J\\u{7}"
        );
        let decoded: PendingReRead = serde_json::from_value(serde_json::json!({
            "reason": {"kind": "incomplete", "detail": raw}
        }))
        .unwrap();
        let ReReadReason::Incomplete { detail } = decoded.reason() else {
            panic!("expected an incomplete re-read reason");
        };
        assert_eq!(detail.as_str(), raw);
        let rendered = detail.to_string();
        assert_eq!(rendered, "\\u{1b}[2J\\u{7}");
    }

    #[test]
    fn provider_text_pattern_mismatch_keeps_regex_data_and_sanitizes_diagnostic() {
        use crate::schema::{CompiledAttributeType, CompiledPattern, TypeError};
        let raw = format!("^\x1b{}$", "x".repeat(20 * 1024));
        let pattern = CompiledPattern::try_new(&raw).unwrap();
        let schema = CompiledAttributeType::refined_string(None, Some(pattern.clone()), None, None);
        let value: crate::resource::Value =
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::String("safe".into()));
        let error = schema.validate(&value).unwrap_err();
        assert!(!error.to_string().contains('\x1b'));
        let TypeError::PatternMismatch {
            pattern: diagnostic,
            ..
        } = error
        else {
            panic!("expected a pattern mismatch");
        };
        assert!(diagnostic.to_string().len() <= MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(diagnostic.as_str(), raw);
        assert_eq!(pattern.as_str(), raw);
    }

    #[test]
    fn provider_text_invalid_schema_metadata_stays_raw_and_displays_safely() {
        use crate::protocol_convert::SchemaDecodeError;
        use crate::schema::{CompiledPattern, ProviderSchemaValidationError, TypeIdentity};
        let raw = format!("name\x1b[2J{}", "x".repeat(9000));
        let invalid_pattern = format!("{raw}(");
        let error = CompiledPattern::try_new(&invalid_pattern).unwrap_err();
        assert_eq!(error.pattern.as_str(), invalid_pattern);
        assert!(!error.to_string().contains('\x1b'));
        let identity = TypeIdentity::new(Some("mock"), [raw.as_str()], "Mode");
        let error = ProviderSchemaValidationError::DanglingReferenceForm {
            identity: identity.clone(),
        };
        assert!(
            matches!(&error, ProviderSchemaValidationError::DanglingReferenceForm { identity: stored } if stored == &identity)
        );
        assert!(!error.to_string().contains('\x1b'));
        let error = SchemaDecodeError::DuplicateReferenceForm { identity };
        assert!(!error.to_string().contains('\x1b'));
        assert!(error.to_string().contains("\\u{1b}[2J"));
        let error = crate::parser::CustomTypeValidationError::ConflictingEnumSpelling {
            identity: Box::new(TypeIdentity::new(None::<&str>, Vec::<String>::new(), &raw)),
            first_owner: "mock".into(),
            first_spelling: raw.clone(),
            second_owner: "mock2".into(),
            second_spelling: "other".into(),
        };
        assert!(!error.to_string().contains('\x1b'));
        assert!(
            matches!(error, crate::parser::CustomTypeValidationError::ConflictingEnumSpelling { first_spelling, .. } if first_spelling == raw)
        );
    }

    #[test]
    fn provider_text_schema_descriptions_and_validation_messages_are_sanitized() {
        use crate::schema::{CompletionValue, PatternResourceSchema, TypeError, legacy_validator};
        let raw = format!("\x1b[2J\x07{}", "x".repeat(20 * 1024));
        let schema: PatternResourceSchema =
            PatternResourceSchema::new("test.Resource").with_description(&raw);
        let description = schema.parts().description.unwrap();
        assert!(!description.contains(['\x1b', '\x07']));
        assert!(description.len() <= MAX_PROVIDER_TEXT_BYTES);
        let completion: CompletionValue = serde_json::from_value(serde_json::json!({
            "value": "data", "description": raw
        }))
        .unwrap();
        assert_eq!(completion.value, "data");
        assert!(!completion.description.contains(['\x1b', '\x07']));
        let validator = legacy_validator(move |_| Err(raw.clone()));
        let error = validator(&crate::resource::Value::Concrete(
            crate::resource::ConcreteValue::Bool(true),
        ))
        .unwrap_err();
        let TypeError::ValidationFailed { message } = error else {
            panic!("expected a validation failure");
        };
        assert!(!message.contains(['\x1b', '\x07']));
        assert!(message.len() <= MAX_PROVIDER_TEXT_BYTES);
    }
}
