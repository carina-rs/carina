//! Schema findings retain the target that the parser, rather than a client, locates.

use super::{
    FieldPath, FieldPathDirection, FieldPathStep, LeafPath, LeafPathSegment, RedactedPathField,
    SECRET_VALUE_MASK, TypeError,
};
use crate::resource::InsideSecret;

/// A schema diagnostic with an explicit location contract for unknown keys.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaDiagnostic {
    UnknownKey(KeyedSchemaFinding),
    Other(OtherSchemaFinding),
}

/// An unknown input key. Only the parser resolves its opaque target.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyedSchemaFinding {
    pub(crate) key: SchemaDiagnosticKey,
    error: TypeError,
}

#[derive(Clone, PartialEq)]
pub(crate) enum SchemaDiagnosticKey {
    Unlocated,
    Attribute {
        name: String,
    },
    Value {
        root: String,
        relative: SchemaKeyPath,
        coverage: Option<LeafPath>,
    },
}

impl std::fmt::Debug for SchemaDiagnosticKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SchemaDiagnosticKey(..)")
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum SchemaKeyPath {
    Key(FieldPath),
    SecretParent(FieldPath),
}

impl SchemaKeyPath {
    fn new(path: &FieldPath) -> Self {
        // A protected map key cannot be entered by the location index. Retain
        // only its public prefix and a marker, never its runtime bytes/suffix.
        let canonical = path.canonical();
        let mut prefix = FieldPath::new();
        for (step, segment) in path.steps().iter().zip(canonical.segments()) {
            if matches!(step, FieldPathStep::Redacted(_)) {
                return Self::SecretParent(secret_parent(&prefix));
            }
            prefix = prefix.pushed(step.clone(), segment.clone(), FieldPathDirection::After);
        }
        Self::Key(path.clone())
    }

    pub(crate) fn path(&self) -> &FieldPath {
        match self {
            Self::Key(path) | Self::SecretParent(path) => path,
        }
    }
}

fn secret_parent(path: &FieldPath) -> FieldPath {
    // An unenterable step lets the existing parser walk select the authored
    // parent, including the anchor of a nested secret() expression.
    path.pushed(
        FieldPathStep::Redacted(RedactedPathField::SecretValue),
        LeafPathSegment::StructField(SECRET_VALUE_MASK.into()),
        FieldPathDirection::After,
    )
}

/// A diagnostic whose established location policy remains with its adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherSchemaFinding {
    error: TypeError,
    path: Option<(String, LeafPath)>,
}

impl OtherSchemaFinding {
    pub fn error(&self) -> &TypeError {
        &self.error
    }

    pub fn path(&self) -> Option<(&str, &LeafPath)> {
        self.path.as_ref().map(|(root, path)| (root.as_str(), path))
    }
}

impl KeyedSchemaFinding {
    pub fn error(&self) -> &TypeError {
        &self.error
    }
}

/// A diagnostic produced by the walk before its attribute is known.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RelativeSchemaDiagnostic {
    target: RelativeDiagnosticTarget,
    error: TypeError,
}

#[derive(Clone, PartialEq)]
enum RelativeDiagnosticTarget {
    Attribute,
    Value {
        relative: SchemaKeyPath,
        coverage: Option<LeafPath>,
    },
    Other {
        path: Option<LeafPath>,
    },
}

impl std::fmt::Debug for RelativeDiagnosticTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelativeDiagnosticTarget(..)")
    }
}

impl RelativeSchemaDiagnostic {
    pub(super) fn new(path: &FieldPath, error: TypeError) -> Self {
        let target = if matches!(error, TypeError::UnknownAttribute { .. }) && path.is_empty() {
            RelativeDiagnosticTarget::Attribute
        } else if is_unknown_key(&error) {
            RelativeDiagnosticTarget::Value {
                relative: SchemaKeyPath::new(path),
                coverage: Some(path.canonical()),
            }
        } else {
            RelativeDiagnosticTarget::Other {
                path: Some(path.canonical()),
            }
        };
        Self { target, error }
    }

    pub(crate) fn error(&self) -> &TypeError {
        &self.error
    }
    fn into_error(self) -> TypeError {
        self.error
    }

    pub(super) fn at_attribute(self, root: impl Into<String>) -> SchemaDiagnostic {
        let root = root.into();
        match self.target {
            RelativeDiagnosticTarget::Attribute => {
                SchemaDiagnostic::UnknownKey(KeyedSchemaFinding {
                    key: SchemaDiagnosticKey::Attribute { name: root },
                    error: self.error,
                })
            }
            RelativeDiagnosticTarget::Value { relative, coverage } => {
                SchemaDiagnostic::UnknownKey(KeyedSchemaFinding {
                    key: SchemaDiagnosticKey::Value {
                        root,
                        relative,
                        coverage,
                    },
                    error: self.error,
                })
            }
            RelativeDiagnosticTarget::Other { path } => {
                SchemaDiagnostic::Other(OtherSchemaFinding {
                    error: self.error,
                    path: path.map(|path| (root, path)),
                })
            }
        }
    }

    fn with_error(self, error: TypeError, source: &FieldPath) -> Self {
        let target = match (self.target, is_unknown_key(&error)) {
            (RelativeDiagnosticTarget::Attribute, true)
                if matches!(error, TypeError::UnknownAttribute { .. }) =>
            {
                RelativeDiagnosticTarget::Attribute
            }
            (
                RelativeDiagnosticTarget::Attribute | RelativeDiagnosticTarget::Other { .. },
                true,
            ) => return Self::new(source, error),
            (RelativeDiagnosticTarget::Value { relative, coverage }, true) => {
                if matches!(error, TypeError::UnknownAttribute { .. }) && relative.path().is_empty()
                {
                    RelativeDiagnosticTarget::Attribute
                } else {
                    RelativeDiagnosticTarget::Value { relative, coverage }
                }
            }
            (RelativeDiagnosticTarget::Attribute, false) => {
                RelativeDiagnosticTarget::Other { path: None }
            }
            (RelativeDiagnosticTarget::Value { coverage, .. }, false) => {
                RelativeDiagnosticTarget::Other { path: coverage }
            }
            (RelativeDiagnosticTarget::Other { path }, false) => {
                RelativeDiagnosticTarget::Other { path }
            }
        };
        Self { target, error }
    }

    fn with_coverage(mut self, path: Option<&LeafPath>) -> Self {
        match &mut self.target {
            RelativeDiagnosticTarget::Attribute => {}
            RelativeDiagnosticTarget::Value { coverage, .. } => *coverage = path.cloned(),
            RelativeDiagnosticTarget::Other { path: stored } => {
                *stored = stored.take().and_then(|_| path.cloned());
            }
        }
        self
    }

    fn at_secret_boundary(&mut self, boundary: &FieldPath) {
        if let RelativeDiagnosticTarget::Value { relative, .. } = &mut self.target {
            *relative = SchemaKeyPath::SecretParent(secret_parent(boundary));
        }
    }
}

impl SchemaDiagnostic {
    pub fn error(&self) -> &TypeError {
        match self {
            Self::UnknownKey(finding) => finding.error(),
            Self::Other(finding) => &finding.error,
        }
    }

    pub(crate) fn unlocated(error: TypeError) -> Self {
        if is_unknown_key(&error) {
            return Self::UnknownKey(KeyedSchemaFinding {
                key: SchemaDiagnosticKey::Unlocated,
                error,
            });
        }
        Self::Other(OtherSchemaFinding { error, path: None })
    }

    pub(crate) fn from_value(
        root: impl Into<String>,
        relative: LeafPath,
        error: TypeError,
    ) -> Self {
        RelativeSchemaDiagnostic::new(
            &FieldPath::from_canonical_segments(relative.segments().iter().cloned()),
            error,
        )
        .at_attribute(root)
    }

    /// Ordinary paths never expose an unknown key's location target.
    pub(crate) fn path(&self) -> Option<(&str, &LeafPath)> {
        match self {
            Self::UnknownKey(_) => None,
            Self::Other(finding) => finding.path(),
        }
    }

    pub(crate) fn coverage_path(&self) -> Option<(&str, &LeafPath)> {
        match self {
            Self::UnknownKey(finding) => match &finding.key {
                SchemaDiagnosticKey::Unlocated | SchemaDiagnosticKey::Attribute { .. } => None,
                SchemaDiagnosticKey::Value { root, coverage, .. } => {
                    coverage.as_ref().map(|path| (root.as_str(), path))
                }
            },
            Self::Other(finding) => finding.path(),
        }
    }

    pub(super) fn with_coverage(mut self, path: Option<&LeafPath>) -> Self {
        match &mut self {
            Self::UnknownKey(finding) => {
                if let SchemaDiagnosticKey::Value { coverage, .. } = &mut finding.key {
                    *coverage = path.cloned();
                }
            }
            Self::Other(finding) => {
                finding.path = finding
                    .path
                    .take()
                    .and_then(|(root, _)| path.cloned().map(|path| (root, path)));
            }
        }
        self
    }

    pub(super) fn attribute_root(&self) -> Option<&str> {
        match self {
            Self::UnknownKey(finding) => match &finding.key {
                SchemaDiagnosticKey::Attribute { name } => Some(name),
                SchemaDiagnosticKey::Value { root, .. } => Some(root),
                SchemaDiagnosticKey::Unlocated => None,
            },
            Self::Other(finding) => finding.path().map(|(root, _)| root),
        }
    }
}

impl InsideSecret for SchemaKeyPath {
    fn inside_secret(self) -> Self {
        match self {
            Self::Key(_) => Self::SecretParent(secret_parent(&FieldPath::new())),
            Self::SecretParent(path) => Self::SecretParent(path),
        }
    }
}

impl InsideSecret for RelativeSchemaDiagnostic {
    fn inside_secret(self) -> Self {
        let target = match self.target {
            RelativeDiagnosticTarget::Attribute => RelativeDiagnosticTarget::Attribute,
            RelativeDiagnosticTarget::Value { relative, coverage } => {
                RelativeDiagnosticTarget::Value {
                    relative: relative.inside_secret(),
                    coverage: coverage.map(InsideSecret::inside_secret),
                }
            }
            RelativeDiagnosticTarget::Other { path } => RelativeDiagnosticTarget::Other {
                path: path.map(InsideSecret::inside_secret),
            },
        };
        Self {
            target,
            error: self.error.inside_secret(),
        }
    }
}

impl InsideSecret for SchemaDiagnostic {
    fn inside_secret(self) -> Self {
        match self {
            Self::UnknownKey(mut finding) => {
                finding.error = finding.error.inside_secret();
                if let SchemaDiagnosticKey::Value {
                    relative, coverage, ..
                } = &mut finding.key
                {
                    *relative = relative.clone().inside_secret();
                    *coverage = coverage.take().map(InsideSecret::inside_secret);
                }
                Self::UnknownKey(finding)
            }
            Self::Other(finding) => Self::Other(OtherSchemaFinding {
                error: finding.error.inside_secret(),
                path: finding
                    .path
                    .map(|(root, path)| (root, path.inside_secret())),
            }),
        }
    }
}

fn is_unknown_key(error: &TypeError) -> bool {
    match error {
        TypeError::UnknownAttribute { .. } | TypeError::UnknownStructField { .. } => true,
        TypeError::StructFieldError { inner, .. }
        | TypeError::ListItemError { inner, .. }
        | TypeError::MapValueError { inner, .. } => is_unknown_key(inner),
        _ => false,
    }
}

/// One walk-produced record, with a separate container/display path. Wrapping
/// a message or selecting a union arm moves its record, never rebuilds a key.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PrimaryValueFinding {
    pub(super) path: FieldPath,
    diagnostic: RelativeSchemaDiagnostic,
}

impl PrimaryValueFinding {
    pub(super) fn new(path: FieldPath, error: TypeError) -> Self {
        Self {
            diagnostic: RelativeSchemaDiagnostic::new(&path, error),
            path,
        }
    }
    pub(super) fn error(&self) -> &TypeError {
        self.diagnostic.error()
    }
    pub(super) fn into_error(self) -> TypeError {
        self.diagnostic.into_error()
    }
    pub(super) fn map_error(mut self, wrap: impl FnOnce(TypeError) -> TypeError) -> Self {
        let error = wrap(self.error().clone());
        self.diagnostic = self.diagnostic.with_error(error, &self.path);
        self
    }
    pub(super) fn in_container(mut self, path: &FieldPath) -> Self {
        self.path = path.clone();
        self.diagnostic = self.diagnostic.with_coverage(Some(&path.canonical()));
        self
    }
    pub(super) fn union_error(self, path: &FieldPath, error: TypeError) -> Self {
        if matches!(
            error,
            TypeError::UnionAlternativesMismatch { .. } | TypeError::UnionStructMismatch { .. }
        ) {
            Self::new(path.clone(), error)
        } else {
            self.map_error(|_| error).in_container(path)
        }
    }
    pub(super) fn diagnostic(&self, root: &str, error: TypeError) -> SchemaDiagnostic {
        self.diagnostic
            .clone()
            .with_error(error, &self.path)
            .at_attribute(root)
    }
    pub(super) fn into_diagnostic(self, root: &str) -> (FieldPath, SchemaDiagnostic) {
        (self.path, self.diagnostic.at_attribute(root))
    }
    pub(super) fn at_secret_boundary(&mut self, boundary: &FieldPath) {
        self.diagnostic.at_secret_boundary(boundary);
    }
}

impl InsideSecret for PrimaryValueFinding {
    fn inside_secret(self) -> Self {
        Self {
            path: self.path.inside_secret(),
            diagnostic: self.diagnostic.inside_secret(),
        }
    }
}

impl std::fmt::Display for PrimaryValueFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error().fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::InsideSecret;

    fn target(finding: &PrimaryValueFinding) -> &FieldPath {
        let RelativeDiagnosticTarget::Value { relative, .. } = &finding.diagnostic.target else {
            panic!("nested unknown key")
        };
        relative.path()
    }

    #[test]
    fn unlocated_unknown_keys_keep_their_diagnostic_variant() {
        let field = || TypeError::StructFieldError {
            field: "nested".into(),
            inner: Box::new(unknown_field()),
        };
        let errors = [
            unknown_field(),
            field(),
            TypeError::ListItemError {
                index: 1,
                inner: Box::new(field()),
            },
            TypeError::MapValueError {
                key: "entry".into(),
                inner: Box::new(field()),
            },
        ];
        let mut misclassified = Vec::new();
        for (index, error) in errors.into_iter().enumerate() {
            let finding = SchemaDiagnostic::unlocated(error.clone());
            assert_eq!(finding.error(), &error);
            assert!(finding.path().is_none());
            if !matches!(finding, SchemaDiagnostic::UnknownKey(_)) {
                misclassified.push(index);
            }
        }
        assert!(
            misclassified.is_empty(),
            "unlocated unknowns classified as Other: {misclassified:?}"
        );
    }

    #[test]
    fn protected_key_keeps_its_public_prefix_after_inside_secret() {
        let finding = PrimaryValueFinding::new(
            FieldPath::from_canonical_segments([
                LeafPathSegment::StructField("public".into()),
                LeafPathSegment::MapKey(crate::resource::ValueString::diagnostic_sensitive(
                    "PRIVATE_KEY",
                )),
                LeafPathSegment::StructField("PRIVATE_SUFFIX".into()),
            ]),
            unknown_field(),
        )
        .inside_secret();
        assert_eq!(
            target(&finding).steps().first(),
            Some(&FieldPathStep::Field("public".into()))
        );
        assert_eq!(target(&finding).steps().len(), 2);
        assert!(!target(&finding).to_string().contains("PRIVATE"));
    }

    fn unknown_field() -> TypeError {
        TypeError::UnknownStructField {
            struct_name: "B".into(),
            field: "unknown".into(),
            suggestion: None,
        }
    }

    #[test]
    fn replacement_errors_reclassify_the_location_contract() {
        let path = FieldPath::new().push_field("unknown");
        let ordinary = || TypeError::value_type_mismatch("Bool".into(), "String".into());
        let mut failures = Vec::new();
        for (label, old, replacement) in [
            ("unknown_to_other", unknown_field(), ordinary()),
            ("other_to_unknown", ordinary(), unknown_field()),
        ] {
            let expected_unknown = is_unknown_key(&replacement);
            let expected_error = replacement.clone();
            for (operation, diagnostic) in [
                (
                    "with_error",
                    RelativeSchemaDiagnostic::new(&path, old.clone())
                        .with_error(replacement.clone(), &path)
                        .at_attribute("direct"),
                ),
                (
                    "map_error",
                    PrimaryValueFinding::new(path.clone(), old.clone())
                        .map_error(|_| replacement.clone())
                        .diagnostic
                        .at_attribute("direct"),
                ),
                (
                    "diagnostic",
                    PrimaryValueFinding::new(path.clone(), old).diagnostic("direct", replacement),
                ),
            ] {
                assert_eq!(diagnostic.error(), &expected_error, "{label}/{operation}");
                if matches!(diagnostic, SchemaDiagnostic::UnknownKey(_)) != expected_unknown {
                    failures.push((label, operation));
                }
            }
        }
        assert!(failures.is_empty(), "replacement drift: {failures:?}");
    }

    #[test]
    fn protected_map_key_target_drops_runtime_key_and_suffix() {
        use super::super::LeafPathSegment;
        let finding = PrimaryValueFinding::new(
            FieldPath::from_canonical_segments([
                LeafPathSegment::MapKey(crate::resource::ValueString::diagnostic_sensitive(
                    "PRIVATE_KEY",
                )),
                LeafPathSegment::StructField("PRIVATE_FIELD".into()),
            ]),
            TypeError::UnknownStructField {
                struct_name: "Entry".into(),
                field: "PRIVATE_FIELD".into(),
                suggestion: None,
            },
        );
        let path = target(&finding).canonical();
        assert!(
            path.segments().iter().all(|segment| match segment {
                LeafPathSegment::MapKey(key) => !key.expose_plaintext().contains("PRIVATE"),
                LeafPathSegment::StructField(field) => !field.contains("PRIVATE"),
                LeafPathSegment::ListIndex(_) => true,
            }),
            "target retains protected runtime bytes"
        );
        assert!(target(&finding).steps().len() <= 1);
    }

    #[test]
    fn secret_unknown_key_target_does_not_retain_plaintext() {
        let finding = PrimaryValueFinding::new(
            FieldPath::new()
                .push_map_key("PRIVATE_KEY")
                .push_field("PRIVATE_FIELD"),
            TypeError::UnknownStructField {
                struct_name: "Entry".into(),
                field: "PRIVATE_FIELD".into(),
                suggestion: None,
            },
        )
        .inside_secret();
        assert!(!target(&finding).to_string().contains("PRIVATE"));
        assert!(
            target(&finding).steps().len() <= 1,
            "truncate at the protected boundary"
        );
    }
}
