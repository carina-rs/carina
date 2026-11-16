//! Schema findings retain the target that the parser, rather than a client, locates.

use super::{FieldPath, LeafPath, TypeError};

/// A schema diagnostic with an explicit location contract for unknown keys.
#[derive(Debug, Clone)]
pub enum SchemaDiagnostic {
    UnknownKey(KeyedSchemaFinding),
    Other(OtherSchemaFinding),
}

/// An unknown input key. Only the parser resolves its opaque target.
#[derive(Debug, Clone)]
pub struct KeyedSchemaFinding {
    pub(crate) key: SchemaDiagnosticKey,
    error: TypeError,
}

#[derive(Clone)]
pub(crate) struct SchemaDiagnosticKey {
    pub(crate) root: String,
    pub(crate) relative: LeafPath,
}

impl std::fmt::Debug for SchemaDiagnosticKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Location provenance can retain a key whose diagnostic text is redacted.
        formatter.write_str("SchemaDiagnosticKey(..)")
    }
}

/// A diagnostic whose established location policy remains with its adapter.
#[derive(Debug, Clone)]
pub struct OtherSchemaFinding {
    error: TypeError,
    path: Option<(String, LeafPath)>,
}

impl OtherSchemaFinding {
    pub fn path(&self) -> Option<(&str, &LeafPath)> {
        self.path.as_ref().map(|(root, path)| (root.as_str(), path))
    }
}

impl KeyedSchemaFinding {
    pub fn error(&self) -> &TypeError {
        &self.error
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
        if let TypeError::UnknownAttribute { name, .. } = &error {
            return Self::from_value(name.clone(), LeafPath::from_segments([]), error);
        }
        Self::Other(OtherSchemaFinding { error, path: None })
    }

    pub(crate) fn from_value(
        root: impl Into<String>,
        relative: LeafPath,
        error: TypeError,
    ) -> Self {
        let root = root.into();
        if is_unknown_key(&error) {
            Self::UnknownKey(KeyedSchemaFinding {
                key: SchemaDiagnosticKey { root, relative },
                error,
            })
        } else {
            Self::Other(OtherSchemaFinding {
                error,
                path: Some((root, relative)),
            })
        }
    }

    pub(crate) fn path(&self) -> Option<(&str, &LeafPath)> {
        match self {
            Self::UnknownKey(finding) => Some((&finding.key.root, &finding.key.relative)),
            Self::Other(finding) => finding.path(),
        }
    }

    pub(super) fn from_container(root: &str, path: &FieldPath, error: TypeError) -> Self {
        // Map summaries retain a container display path. Their structured error
        // carries the selected value's suffix, independently of that presentation.
        let target = if matches!(error, TypeError::MapValueError { .. }) && is_unknown_key(&error) {
            unknown_key_path(path.clone(), &error)
        } else {
            path.clone()
        };
        Self::from_value(root, target.canonical(), error)
    }
}

fn unknown_key_path(path: FieldPath, error: &TypeError) -> FieldPath {
    match error {
        TypeError::UnknownStructField { field, .. } => path.push_field(field),
        TypeError::StructFieldError { field, inner } => {
            unknown_key_path(path.push_field(field), inner)
        }
        TypeError::ListItemError { index, inner } => {
            unknown_key_path(path.push_index(*index), inner)
        }
        TypeError::MapValueError { key, inner } => unknown_key_path(path.push_map_key(key), inner),
        _ => path,
    }
}

pub(super) fn is_unknown_key(error: &TypeError) -> bool {
    match error {
        TypeError::UnknownAttribute { .. } | TypeError::UnknownStructField { .. } => true,
        TypeError::StructFieldError { inner, .. }
        | TypeError::ListItemError { inner, .. }
        | TypeError::MapValueError { inner, .. } => is_unknown_key(inner),
        _ => false,
    }
}

/// Keep the selected error attached to its leaf as container messages wrap it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PrimaryValueFinding {
    pub(super) path: FieldPath,
    pub(super) error: TypeError,
    key_path: Option<SchemaKeyPath>,
}

#[derive(Clone, PartialEq)]
struct SchemaKeyPath(FieldPath);

impl std::fmt::Debug for SchemaKeyPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SchemaKeyPath(..)")
    }
}

impl PrimaryValueFinding {
    pub(super) fn new(path: FieldPath, error: TypeError) -> Self {
        let key_path = is_unknown_key(&error).then(|| SchemaKeyPath(path.clone()));
        Self {
            path,
            error,
            key_path,
        }
    }

    pub(super) fn from_container(path: FieldPath, error: TypeError) -> Self {
        let key_path =
            is_unknown_key(&error).then(|| SchemaKeyPath(unknown_key_path(path.clone(), &error)));
        Self {
            path,
            error,
            key_path,
        }
    }

    pub(super) fn key_path(&self) -> &FieldPath {
        self.key_path.as_ref().map_or(&self.path, |key| &key.0)
    }

    pub(super) fn diagnostic(&self, root: &str, error: TypeError) -> SchemaDiagnostic {
        SchemaDiagnostic::from_value(root, self.key_path().canonical(), error)
    }
}

impl crate::resource::InsideSecret for PrimaryValueFinding {
    fn inside_secret(self) -> Self {
        Self {
            path: self.path.inside_secret(),
            error: self.error.inside_secret(),
            key_path: self.key_path,
        }
    }
}

impl std::fmt::Display for PrimaryValueFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}
