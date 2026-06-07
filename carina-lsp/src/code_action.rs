//! Code-action support for enum-mismatch diagnostics. See #2309.
//!
//! Diagnostics produced for `TypeError::InvalidEnumVariant` and
//! `TypeError::StringLiteralExpectedEnum` carry a serialized
//! [`EnumDiagnosticData`] payload on `Diagnostic.data`. The LSP's
//! `textDocument/codeAction` handler reads that payload back, dedupes
//! the candidate list, and returns one `CodeAction` per remaining
//! candidate that replaces the offending range with the canonical
//! identifier form.
//!
//! The kind tag distinguishes the two diagnostic shapes:
//!
//! - [`EnumDiagnosticKind::BareInvalid`] — the user typed a bare or
//!   namespaced identifier that didn't match any variant. The
//!   replacement is the new identifier alone (no quotes); the
//!   diagnostic range covers the value text.
//! - [`EnumDiagnosticKind::StringLiteral`] — the user wrote a quoted
//!   string literal on an enum-typed attribute. The diagnostic range
//!   covers the literal *including* both quote characters; the
//!   replacement is the canonical identifier form (no quotes), so
//!   applying the action drops the quotes too.

use carina_core::schema::ExpectedEnumVariant;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{CodeAction, CodeActionKind, Diagnostic, TextEdit, Url, WorkspaceEdit};

/// Whether the diagnostic was emitted for a bare-identifier mismatch
/// or a quoted string literal in enum position. Determines how the
/// code-action `WorkspaceEdit` overwrites the offending range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnumDiagnosticKind {
    /// The user typed an identifier (bare or namespaced) that didn't
    /// match any variant. Replace with the canonical identifier.
    BareInvalid,
    /// The user wrote a quoted string literal where an enum identifier
    /// was expected. Replace including the surrounding quotes.
    StringLiteral,
}

/// Payload attached to enum-mismatch `Diagnostic.data`. Round-trips
/// through JSON so the LSP client can hand it back unchanged on a
/// `textDocument/codeAction` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumDiagnosticData {
    /// Marker discriminator so future payloads on `Diagnostic.data`
    /// (e.g. for `UnknownAttribute` quick-fixes) won't be mistaken
    /// for an enum payload after deserialization.
    pub tag: EnumDiagnosticTag,
    pub kind: EnumDiagnosticKind,
    pub expected: Vec<ExpectedEnumVariant>,
}

/// Static tag value used as a structural marker. `serde` rejects
/// anything else when deserializing, so a stray `Diagnostic.data`
/// from another source can't masquerade as an enum payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnumDiagnosticTag {
    #[serde(rename = "carina_enum_mismatch")]
    EnumMismatch,
}

impl EnumDiagnosticData {
    pub fn new(kind: EnumDiagnosticKind, expected: Vec<ExpectedEnumVariant>) -> Self {
        Self {
            tag: EnumDiagnosticTag::EnumMismatch,
            kind,
            expected,
        }
    }

    /// Try to read an enum-mismatch payload off a `Diagnostic`.
    /// Returns `None` when `data` is missing, was emitted by a
    /// different feature, or fails to deserialize.
    pub fn from_diagnostic(diag: &Diagnostic) -> Option<Self> {
        let data = diag.data.as_ref()?;
        serde_json::from_value(data.clone()).ok()
    }
}

/// Build the `CodeAction` list for one diagnostic. Returns an empty
/// vec when the payload is absent or no candidates apply.
pub fn code_actions_for_diagnostic(uri: &Url, diag: &Diagnostic) -> Vec<CodeAction> {
    let Some(payload) = EnumDiagnosticData::from_diagnostic(diag) else {
        return Vec::new();
    };

    // Drop alias entries when at least one canonical entry is present.
    // The issue's spec: "skipping `is_alias = true` unless no canonical
    // entry exists". Aliases (e.g. `enabled` for `Enabled`) are valid
    // forms but the canonical name is the preferred fix.
    let has_canonical = payload.expected.iter().any(|e| !e.is_alias);
    let candidates: Vec<&ExpectedEnumVariant> = if has_canonical {
        payload.expected.iter().filter(|e| !e.is_alias).collect()
    } else {
        payload.expected.iter().collect()
    };

    candidates
        .into_iter()
        .map(|variant| {
            let new_text = variant.to_string();
            let title = format!("Replace with `{}`", new_text);
            let edit = TextEdit {
                range: diag.range,
                new_text,
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diag.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                is_preferred: Some(!variant.is_alias),
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeError, enum_identity,
    };
    use std::collections::HashMap;
    use tower_lsp::lsp_types::{Position, Range};

    /// Build a versioning-bucket fixture variant — keep the `aws.s3.Bucket
    /// .VersioningStatus` shape stable across these tests so `Display`
    /// renders `aws.s3.Bucket.VersioningStatus.<value>` and the
    /// snapshot assertions stay readable.
    fn s3_versioning_variant(
        provider: Option<&str>,
        value: &str,
        is_alias: bool,
    ) -> ExpectedEnumVariant {
        let namespace = provider.map(|provider| format!("{provider}.s3.Bucket"));
        ExpectedEnumVariant::from_namespaced(
            namespace.as_deref(),
            "VersioningStatus",
            value,
            is_alias,
        )
    }

    fn diag_with_payload(payload: EnumDiagnosticData) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 5,
                },
                end: Position {
                    line: 0,
                    character: 12,
                },
            },
            data: Some(serde_json::to_value(payload).unwrap()),
            ..Default::default()
        }
    }

    fn dummy_uri() -> Url {
        Url::parse("file:///tmp/main.crn").unwrap()
    }

    #[test]
    fn payload_round_trips_via_diagnostic_data() {
        let original = EnumDiagnosticData::new(
            EnumDiagnosticKind::BareInvalid,
            vec![s3_versioning_variant(Some("aws"), "Enabled", false)],
        );
        let diag = diag_with_payload(original.clone());
        let read = EnumDiagnosticData::from_diagnostic(&diag).expect("payload present");
        assert_eq!(read, original);
    }

    #[test]
    fn from_diagnostic_returns_none_when_data_is_absent() {
        let diag = Diagnostic::default();
        assert!(EnumDiagnosticData::from_diagnostic(&diag).is_none());
    }

    #[test]
    fn from_diagnostic_returns_none_for_unrelated_payload() {
        // A diagnostic from another feature (or a future tag) must not
        // be mistakenly routed to the enum code-action handler.
        let diag = Diagnostic {
            data: Some(serde_json::json!({ "tag": "something_else" })),
            ..Default::default()
        };
        assert!(EnumDiagnosticData::from_diagnostic(&diag).is_none());
    }

    #[test]
    fn aliases_are_skipped_when_canonical_entry_exists() {
        let payload = EnumDiagnosticData::new(
            EnumDiagnosticKind::BareInvalid,
            vec![
                s3_versioning_variant(Some("aws"), "Enabled", false),
                s3_versioning_variant(Some("aws"), "enabled", true),
                s3_versioning_variant(Some("aws"), "Suspended", false),
                s3_versioning_variant(Some("aws"), "suspended", true),
            ],
        );
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        let titles: Vec<String> = actions.iter().map(|a| a.title.clone()).collect();
        assert_eq!(
            titles,
            vec![
                "Replace with `aws.s3.Bucket.VersioningStatus.Enabled`",
                "Replace with `aws.s3.Bucket.VersioningStatus.Suspended`",
            ],
            "alias entries should be filtered out when canonicals are present"
        );
    }

    #[test]
    fn aliases_are_kept_when_no_canonical_entry_exists() {
        // Defensive: if a malformed payload contains only aliases, the
        // user should still get *something* actionable.
        let payload = EnumDiagnosticData::new(
            EnumDiagnosticKind::BareInvalid,
            vec![s3_versioning_variant(Some("aws"), "enabled", true)],
        );
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0].title,
            "Replace with `aws.s3.Bucket.VersioningStatus.enabled`"
        );
    }

    #[test]
    fn workspace_edit_replaces_diagnostic_range_with_new_text() {
        // The applied edit's `new_text` is the variant's `Display`
        // form — for `StringLiteral` kind the diagnostic range
        // already covers the surrounding quotes, so writing the bare
        // identifier in that range drops the quotes too.
        let payload = EnumDiagnosticData::new(
            EnumDiagnosticKind::StringLiteral,
            vec![s3_versioning_variant(Some("aws"), "Enabled", false)],
        );
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        let action = &actions[0];
        let edit = action.edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        let edits = changes.get(&dummy_uri()).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range, diag.range);
        assert_eq!(edits[0].new_text, "aws.s3.Bucket.VersioningStatus.Enabled");
    }

    #[test]
    fn non_namespaced_variant_renders_bare_value() {
        let payload = EnumDiagnosticData::new(
            EnumDiagnosticKind::BareInvalid,
            vec![ExpectedEnumVariant::from_namespaced(
                None, "Mode", "fast", false,
            )],
        );
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        assert_eq!(actions[0].title, "Replace with `fast`");
    }

    #[test]
    fn canonical_action_is_marked_preferred() {
        let payload = EnumDiagnosticData::new(
            EnumDiagnosticKind::BareInvalid,
            vec![s3_versioning_variant(Some("aws"), "Enabled", false)],
        );
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        assert_eq!(actions[0].is_preferred, Some(true));
    }

    #[test]
    fn version_quickfix_uses_dsl_spelling_from_core_candidates() {
        let schema = ResourceSchema::new("aws.iam.PolicyDocument").attribute(
            AttributeSchema::new(
                "Version",
                AttributeType::enum_(
                    enum_identity("Version", Some("aws.iam.PolicyDocument")),
                    Some(vec!["2012-10-17".to_string(), "2008-10-17".to_string()]),
                    vec![
                        ("2012-10-17".to_string(), "2012_10_17".to_string()),
                        ("2008-10-17".to_string(), "2008_10_17".to_string()),
                    ],
                    None,
                    None,
                ),
            )
            .required(),
        );
        let mut attrs = HashMap::new();
        attrs.insert(
            "Version".to_string(),
            Value::Concrete(ConcreteValue::EnumIdentifier("bad_version".to_string())),
        );
        let mut errs = schema.validate(&attrs).unwrap_err();
        let err = errs.remove(0);
        let TypeError::InvalidEnumVariant { expected, .. } = err else {
            panic!("expected InvalidEnumVariant, got {err:?}");
        };
        let payload = EnumDiagnosticData::new(EnumDiagnosticKind::BareInvalid, expected);
        let diag = diag_with_payload(payload);
        let actions = code_actions_for_diagnostic(&dummy_uri(), &diag);
        let replacements: Vec<String> = actions
            .iter()
            .flat_map(|action| {
                action
                    .edit
                    .as_ref()
                    .and_then(|edit| edit.changes.as_ref())
                    .and_then(|changes| changes.get(&dummy_uri()))
                    .into_iter()
                    .flatten()
                    .map(|edit| edit.new_text.clone())
            })
            .collect();

        assert!(
            replacements.contains(&"aws.iam.PolicyDocument.Version.2012_10_17".to_string()),
            "quickfixes must include DSL spelling, got: {replacements:?}"
        );
        assert!(
            replacements
                .iter()
                .all(|replacement| !replacement.contains('-')),
            "quickfixes must not offer API hyphen spelling, got: {replacements:?}"
        );
        let preferred = actions
            .iter()
            .find(|action| action.title.contains("2012_10_17"))
            .expect("2012_10_17 quickfix");
        assert_eq!(preferred.is_preferred, Some(true));
    }
}
