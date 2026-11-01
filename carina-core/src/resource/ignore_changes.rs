use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use super::{
    BindingAddress, CanonicalComparisonAttributes, ConcreteValue, PlaceholderKind, Resource, Value,
};
use crate::schema::{
    EffectiveType, LeafPath, LeafPathSegment, ResourceSchema, Shape, ShapeWalkBudget,
    ValidatedResourceSchema,
};

/// A syntactic attribute path read from `directives.ignore_changes`.
///
/// This type deliberately carries no schema meaning. It may only enter the
/// differ after schema resolution has converted it to a `ResolvedIgnorePath`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParsedIgnorePath {
    segments: Vec<ParsedIgnorePathSegment>,
}

impl ParsedIgnorePath {
    pub(crate) fn new(segments: Vec<ParsedIgnorePathSegment>) -> Self {
        debug_assert!(!segments.is_empty());
        Self { segments }
    }

    pub fn segments(&self) -> &[ParsedIgnorePathSegment] {
        &self.segments
    }
}

impl Display for ParsedIgnorePath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.segments.iter().enumerate() {
            match segment {
                ParsedIgnorePathSegment::Attribute(name) => {
                    if index > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(name)?;
                }
                ParsedIgnorePathSegment::MapKey(key) => {
                    let key = serde_json::to_string(key).map_err(|_| fmt::Error)?;
                    write!(f, "[{key}]")?;
                }
                ParsedIgnorePathSegment::ListIndex(value) => write!(f, "[{value}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ParsedIgnorePathSegment {
    Attribute(String),
    MapKey(String),
    ListIndex(usize),
}

/// A schema-resolved ignored leaf. Its fields are private so only
/// [`resolve_ignore_changes`] can create one from parser syntax.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedIgnorePath {
    attribute: String,
    leaf: LeafPath,
    authored: ParsedIgnorePath,
    write_only: bool,
}

impl ResolvedIgnorePath {
    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    pub fn leaf(&self) -> &LeafPath {
        &self.leaf
    }

    pub fn authored(&self) -> &ParsedIgnorePath {
        &self.authored
    }

    pub fn is_write_only(&self) -> bool {
        self.write_only
    }
}

/// The only ignored-path collection accepted by planning and apply code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedIgnoreChanges(Vec<ResolvedIgnorePath>);

impl ResolvedIgnoreChanges {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ResolvedIgnorePath> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn for_attribute<'a>(
        &'a self,
        attribute: &'a str,
    ) -> impl Iterator<Item = &'a ResolvedIgnorePath> + 'a {
        self.0
            .iter()
            .filter(move |path| path.attribute == attribute)
    }

    pub fn ignores_whole_attribute(&self, attribute: &str) -> bool {
        self.for_attribute(attribute)
            .any(|path| path.leaf.segments().is_empty())
    }

    pub fn ignores_leaf(&self, attribute: &str, leaf: &LeafPath) -> bool {
        self.for_attribute(attribute).any(|path| &path.leaf == leaf)
    }

    pub fn has_ignored_write_only_leaf(&self, attribute: &str) -> bool {
        self.for_attribute(attribute)
            .any(ResolvedIgnorePath::is_write_only)
    }

    pub(crate) fn with_observed_readable_leaves(
        &self,
        attribute: &str,
        desired: &Value,
        observed: Option<&Value>,
    ) -> Value {
        let mut value = desired.clone();
        for path in self
            .for_attribute(attribute)
            .filter(|path| !path.write_only && !path.leaf.segments().is_empty())
        {
            let observed_leaf = observed.and_then(|value| value_at_leaf(value, &path.leaf));
            set_leaf(&mut value, path.leaf.segments(), observed_leaf.cloned());
        }
        value
    }

    /// Apply readable ignore masks and restore the canonical comparison form.
    pub(crate) fn mask_comparison_readable_leaves(
        &self,
        desired: &CanonicalComparisonAttributes,
        observed: &CanonicalComparisonAttributes,
        schema: Option<&ResourceSchema>,
    ) -> CanonicalComparisonAttributes {
        if self.is_empty() {
            return desired.clone();
        }
        let mut masked = desired.attributes.clone();
        let attributes = self
            .iter()
            .filter(|path| !path.write_only)
            .map(|path| path.attribute.clone())
            .collect::<HashSet<_>>();
        for attribute in attributes {
            if self
                .for_attribute(&attribute)
                .any(|path| !path.write_only && path.leaf.segments().is_empty())
            {
                match observed.get(&attribute) {
                    Some(value) => {
                        masked.insert(attribute, value.clone());
                    }
                    None => {
                        masked.remove(&attribute);
                    }
                }
                continue;
            }
            let Some(value) = masked.get(&attribute).cloned() else {
                continue;
            };
            let value =
                self.with_observed_readable_leaves(&attribute, &value, observed.get(&attribute));
            masked.insert(attribute, value);
        }
        match schema {
            Some(schema) => CanonicalComparisonAttributes::canonicalize(masked, schema),
            None => CanonicalComparisonAttributes::from_unschematized(
                super::UnschematizedComparisonAttributes::new(masked),
            ),
        }
    }

    /// Make an historical comparison baseline agree with a fresh observation
    /// at every readable ignored path.
    ///
    /// Saved-plan drift detection uses this before comparing the two states;
    /// write-only paths are deliberately untouched because reads cannot reveal
    /// their current values.
    pub fn mask_observed_readable_leaves(
        &self,
        baseline: &mut HashMap<String, Value>,
        observed: &HashMap<String, Value>,
    ) {
        let attributes = self
            .iter()
            .filter(|path| !path.write_only)
            .map(|path| path.attribute.clone())
            .collect::<HashSet<_>>();
        for attribute in attributes {
            if self
                .for_attribute(&attribute)
                .any(|path| !path.write_only && path.leaf.segments().is_empty())
            {
                match observed.get(&attribute) {
                    Some(value) => {
                        baseline.insert(attribute, value.clone());
                    }
                    None => {
                        baseline.remove(&attribute);
                    }
                }
                continue;
            }
            let Some(value) = baseline.get(&attribute).cloned() else {
                continue;
            };
            let masked =
                self.with_observed_readable_leaves(&attribute, &value, observed.get(&attribute));
            baseline.insert(attribute, masked);
        }
    }
}

pub(crate) fn value_at_leaf<'a>(value: &'a Value, leaf: &LeafPath) -> Option<&'a Value> {
    value_at_segments(value, leaf.segments())
}

fn value_at_segments<'a>(mut value: &'a Value, segments: &[LeafPathSegment]) -> Option<&'a Value> {
    for segment in segments {
        value = match (segment, value) {
            (
                LeafPathSegment::StructField(name) | LeafPathSegment::MapKey(name),
                Value::Concrete(ConcreteValue::Map(values)),
            ) => values.get(name)?,
            (LeafPathSegment::ListIndex(index), Value::Concrete(ConcreteValue::List(values))) => {
                values.get(*index)?
            }
            _ => return None,
        };
    }
    Some(value)
}

fn set_leaf(value: &mut Value, segments: &[LeafPathSegment], replacement: Option<Value>) {
    let Some((segment, rest)) = segments.split_first() else {
        if let Some(replacement) = replacement {
            *value = replacement;
        }
        return;
    };
    match (segment, value) {
        (
            LeafPathSegment::StructField(name) | LeafPathSegment::MapKey(name),
            Value::Concrete(ConcreteValue::Map(values)),
        ) => {
            if rest.is_empty() {
                match replacement {
                    Some(replacement) => {
                        values.insert(name.clone(), replacement);
                    }
                    None => {
                        values.shift_remove(name);
                    }
                }
            } else if let Some(child) = values.get_mut(name) {
                set_leaf(child, rest, replacement);
            }
        }
        (LeafPathSegment::ListIndex(index), Value::Concrete(ConcreteValue::List(values))) => {
            if let Some(child) = values.get_mut(*index) {
                set_leaf(child, rest, replacement);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreChangeError {
    path: ParsedIgnorePath,
    kind: IgnoreChangeErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IgnoreChangeErrorKind {
    NotInSchema,
    ReadOnly,
    ListIndex,
    ThroughList,
    Union,
    NotWritten,
    Duplicate,
}

impl Display for IgnoreChangeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match self.kind {
            IgnoreChangeErrorKind::NotInSchema => write!(
                f,
                "ignore_changes path `{path}` does not exist in the resource schema"
            ),
            IgnoreChangeErrorKind::ReadOnly => write!(
                f,
                "ignore_changes path `{path}` reaches a read-only or provider-populated node"
            ),
            IgnoreChangeErrorKind::ListIndex => write!(
                f,
                "ignore_changes path `{path}` contains a list index, which is not supported"
            ),
            IgnoreChangeErrorKind::ThroughList => write!(
                f,
                "ignore_changes path `{path}` passes through a list, which is not supported"
            ),
            IgnoreChangeErrorKind::Union => {
                write!(f, "ignore_changes path `{path}` passes through a union")
            }
            IgnoreChangeErrorKind::NotWritten => write!(
                f,
                "ignore_changes path `{path}` is not written in this resource block"
            ),
            IgnoreChangeErrorKind::Duplicate => {
                write!(f, "ignore_changes path `{path}` is listed more than once")
            }
        }
    }
}

impl std::error::Error for IgnoreChangeError {}

struct ValueCursor<'a, P: PlaceholderKind, B: BindingAddress> {
    leaf: Vec<LeafPathSegment>,
    value: &'a Value<P, B>,
}

/// Resolve authored ignore paths against the effective schema and the values
/// written in the same resource block.
pub fn resolve_ignore_changes<S, P: PlaceholderKind, B: BindingAddress, D>(
    resource: &Resource<S, P, B, D>,
    schema: &ValidatedResourceSchema,
) -> Result<ResolvedIgnoreChanges, Vec<IgnoreChangeError>> {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();

    for path in &resource.directives.ignore_changes {
        if !seen.insert(path.clone()) {
            errors.push(IgnoreChangeError {
                path: path.clone(),
                kind: IgnoreChangeErrorKind::Duplicate,
            });
            continue;
        }
        match resolve_one_ignore_change(resource, schema, path) {
            Ok(mut paths) => resolved.append(&mut paths),
            Err(error) => errors.push(error),
        }
    }

    if errors.is_empty() {
        Ok(ResolvedIgnoreChanges(resolved))
    } else {
        Err(errors)
    }
}

fn resolve_one_ignore_change<S, P: PlaceholderKind, B: BindingAddress, D>(
    resource: &Resource<S, P, B, D>,
    schema: &ValidatedResourceSchema,
    path: &ParsedIgnorePath,
) -> Result<Vec<ResolvedIgnorePath>, IgnoreChangeError> {
    if path
        .segments()
        .iter()
        .any(|segment| matches!(segment, ParsedIgnorePathSegment::ListIndex(_)))
    {
        return Err(ignore_error(path, IgnoreChangeErrorKind::ListIndex));
    }

    let Some((ParsedIgnorePathSegment::Attribute(root), rest)) = path.segments().split_first()
    else {
        return Err(ignore_error(path, IgnoreChangeErrorKind::NotInSchema));
    };
    let Some(attribute) = schema.effective_attribute_by_authored_name(root) else {
        return Err(ignore_error(path, IgnoreChangeErrorKind::NotInSchema));
    };
    if attribute.is_read_only() {
        return Err(ignore_error(path, IgnoreChangeErrorKind::ReadOnly));
    }

    let canonical_attribute = attribute.name().to_string();
    let Some(root_value) = resource
        .attributes
        .get(root)
        .or_else(|| resource.attributes.get(&canonical_attribute))
    else {
        return Err(ignore_error(path, IgnoreChangeErrorKind::NotWritten));
    };
    let mut cursors = vec![ValueCursor {
        leaf: Vec::new(),
        value: root_value,
    }];
    let mut value_type = attribute.value_type();
    let mut write_only = attribute.is_write_only();

    for segment in rest {
        normalize_authored_struct_block(&value_type, &mut cursors);
        if value_type.list_element().is_some() {
            return Err(ignore_error(path, IgnoreChangeErrorKind::ThroughList));
        }
        match segment {
            ParsedIgnorePathSegment::ListIndex(_) => {
                unreachable!("list indices are rejected before traversal")
            }
            ParsedIgnorePathSegment::Attribute(authored_name) => {
                if matches!(value_type.shape(), Shape::Union) {
                    return Err(ignore_error(path, IgnoreChangeErrorKind::Union));
                }
                let mut budget = ShapeWalkBudget::new(64);
                let Some(field) = value_type.find_field_with_budget(authored_name, &mut budget)
                else {
                    return Err(ignore_error(path, IgnoreChangeErrorKind::NotInSchema));
                };
                if field.is_read_only() {
                    return Err(ignore_error(path, IgnoreChangeErrorKind::ReadOnly));
                }
                write_only |= field.is_write_only();
                let canonical_name = field.name().to_string();
                let block_name = field.block_name().map(str::to_string);
                cursors = cursors
                    .into_iter()
                    .filter_map(|cursor| {
                        let Value::Concrete(ConcreteValue::Map(values)) = cursor.value else {
                            return None;
                        };
                        let value = values
                            .get(authored_name)
                            .or_else(|| values.get(&canonical_name))
                            .or_else(|| block_name.as_deref().and_then(|name| values.get(name)))?;
                        let mut leaf = cursor.leaf;
                        leaf.push(LeafPathSegment::StructField(canonical_name.clone()));
                        Some(ValueCursor { leaf, value })
                    })
                    .collect();
                value_type = field.value_type();
            }
            ParsedIgnorePathSegment::MapKey(key) => {
                if matches!(value_type.shape(), Shape::Union) {
                    return Err(ignore_error(path, IgnoreChangeErrorKind::Union));
                }
                let Some(next_type) = value_type.map_value() else {
                    return Err(ignore_error(path, IgnoreChangeErrorKind::NotInSchema));
                };
                cursors = cursors
                    .into_iter()
                    .filter_map(|cursor| {
                        let Value::Concrete(ConcreteValue::Map(values)) = cursor.value else {
                            return None;
                        };
                        let value = values.get(key)?;
                        let mut leaf = cursor.leaf;
                        leaf.push(LeafPathSegment::MapKey(key.clone()));
                        Some(ValueCursor { leaf, value })
                    })
                    .collect();
                value_type = next_type;
            }
        }

        if cursors.is_empty() {
            return Err(ignore_error(path, IgnoreChangeErrorKind::NotWritten));
        }
    }

    Ok(cursors
        .into_iter()
        .map(|cursor| ResolvedIgnorePath {
            attribute: canonical_attribute.clone(),
            leaf: LeafPath::from_segments(cursor.leaf),
            authored: path.clone(),
            write_only,
        })
        .collect())
}

fn normalize_authored_struct_block<'a, P: PlaceholderKind, B: BindingAddress>(
    value_type: &EffectiveType<'_>,
    cursors: &mut Vec<ValueCursor<'a, P, B>>,
) {
    if !matches!(value_type.shape(), Shape::Struct { .. }) {
        return;
    }
    let old = std::mem::take(cursors);
    *cursors = old
        .into_iter()
        .flat_map(|cursor| match cursor.value {
            Value::Concrete(ConcreteValue::List(values)) => values
                .iter()
                .map(|value| ValueCursor {
                    leaf: cursor.leaf.clone(),
                    value,
                })
                .collect::<Vec<_>>(),
            _ => vec![cursor],
        })
        .collect();
}

fn ignore_error(path: &ParsedIgnorePath, kind: IgnoreChangeErrorKind) -> IgnoreChangeError {
    IgnoreChangeError {
        path: path.clone(),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::resource::{ConcreteValue, Directives, Resource, Value};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    fn path(segments: impl IntoIterator<Item = ParsedIgnorePathSegment>) -> ParsedIgnorePath {
        ParsedIgnorePath::new(segments.into_iter().collect())
    }

    fn attr(name: &str) -> ParsedIgnorePathSegment {
        ParsedIgnorePathSegment::Attribute(name.to_string())
    }

    fn resource_with(paths: Vec<ParsedIgnorePath>) -> Resource {
        let mut config = IndexMap::new();
        config.insert(
            "desired".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        );
        config.insert(
            "secret".to_string(),
            Value::Concrete(ConcreteValue::String("secret".into())),
        );
        let mut resource = Resource::with_provider("test", "Widget", "example", None)
            .with_attribute("config", Value::Concrete(ConcreteValue::Map(config)))
            .with_attribute(
                "labels",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "Owner".to_string(),
                    Value::Concrete(ConcreteValue::String("platform".into())),
                )]))),
            )
            .with_attribute(
                "create_key",
                Value::Concrete(ConcreteValue::String("initial".into())),
            )
            .with_attribute(
                "rule",
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(IndexMap::from([(
                        "action".to_string(),
                        Value::Concrete(ConcreteValue::String("allow".into())),
                    )])),
                )])),
            );
        resource.directives = Directives {
            ignore_changes: paths,
            ..Directives::default()
        };
        resource
    }

    fn schema() -> crate::schema::ValidatedResourceSchema {
        ResourceSchema::new("Widget")
            .attribute(
                AttributeSchema::optional(
                    "configuration",
                    AttributeType::struct_(
                        "Configuration",
                        vec![
                            StructField::optional("desired", AttributeType::string()).build(),
                            StructField::optional("secret", AttributeType::string())
                                .write_only()
                                .build(),
                            StructField::provider_populated("observed", AttributeType::string())
                                .build(),
                        ],
                    ),
                )
                .with_block_name("config")
                .build(),
            )
            .attribute(
                AttributeSchema::optional("labels", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("create_key", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "rule",
                    AttributeType::list(AttributeType::struct_(
                        "Rule",
                        vec![StructField::optional("action", AttributeType::string()).build()],
                    )),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("generated", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "choice",
                    AttributeType::union(vec![
                        AttributeType::struct_(
                            "A",
                            vec![StructField::optional("value", AttributeType::string()).build()],
                        ),
                        AttributeType::struct_(
                            "B",
                            vec![StructField::optional("value", AttributeType::string()).build()],
                        ),
                    ]),
                )
                .build(),
            )
            .validate()
            .expect("valid ignore_changes test schema")
    }

    #[test]
    fn resolve_ignore_changes_canonicalizes_authored_fields_and_map_keys() {
        let resource = resource_with(vec![
            path([attr("config"), attr("secret")]),
            path([
                attr("labels"),
                ParsedIgnorePathSegment::MapKey("Owner".to_string()),
            ]),
        ]);

        let resolved = resolve_ignore_changes(&resource, &schema()).expect("valid paths");
        let paths = resolved
            .iter()
            .map(|path| {
                (
                    path.attribute(),
                    path.leaf().segments(),
                    path.is_write_only(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].0, "configuration");
        assert!(paths[0].2);
        assert_eq!(paths[1].0, "labels");
        assert!(!paths[1].2);
    }

    #[test]
    fn resolve_ignore_changes_normalizes_authored_struct_blocks_without_list_leaves() {
        let mut resource = resource_with(vec![path([attr("config"), attr("desired")])]);
        resource.attributes.insert(
            "config".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([(
                    "desired".to_string(),
                    Value::Concrete(ConcreteValue::String("value".into())),
                )])),
            )])),
        );

        let resolved = resolve_ignore_changes(&resource, &schema())
            .expect("a struct-typed attribute authored as a block remains traversable");
        let resolved = resolved.iter().next().expect("one resolved path");

        assert_eq!(resolved.attribute(), "configuration");
        assert_eq!(
            resolved.leaf().segments(),
            [LeafPathSegment::StructField("desired".to_string())]
        );
        assert!(
            resolved
                .leaf()
                .segments()
                .iter()
                .all(|segment| !matches!(segment, LeafPathSegment::ListIndex(_)))
        );
    }

    #[test]
    fn resolve_ignore_changes_reports_every_validation_rule() {
        let cases = [
            (
                resource_with(vec![path([attr("missing")])]),
                "ignore_changes path `missing` does not exist in the resource schema",
            ),
            (
                resource_with(vec![path([attr("generated")])]),
                "ignore_changes path `generated` reaches a read-only or provider-populated node",
            ),
            (
                resource_with(vec![path([
                    attr("config"),
                    ParsedIgnorePathSegment::ListIndex(0),
                ])]),
                "ignore_changes path `config[0]` contains a list index, which is not supported",
            ),
            (
                resource_with(vec![path([attr("choice"), attr("value")])]).with_attribute(
                    "choice",
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "value".to_string(),
                        Value::Concrete(ConcreteValue::String("x".into())),
                    )]))),
                ),
                "ignore_changes path `choice.value` passes through a union",
            ),
            (
                resource_with(vec![path([attr("config"), attr("observed")])]),
                "ignore_changes path `config.observed` reaches a read-only or provider-populated node",
            ),
            (
                resource_with(vec![path([attr("config"), attr("desired")])]).with_attribute(
                    "config",
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                ),
                "ignore_changes path `config.desired` is not written in this resource block",
            ),
            (
                resource_with(vec![path([attr("create_key")]), path([attr("create_key")])]),
                "ignore_changes path `create_key` is listed more than once",
            ),
        ];

        for (resource, expected) in cases {
            let messages = resolve_ignore_changes(&resource, &schema())
                .expect_err("invalid path")
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            assert!(
                messages.iter().any(|message| message == expected),
                "expected {expected:?}, got {messages:?}"
            );
        }
    }

    #[test]
    fn resolve_ignore_changes_rejects_path_through_list() {
        let resource = resource_with(vec![path([attr("rule"), attr("action")])]);

        let errors = resolve_ignore_changes(&resource, &schema())
            .expect_err("paths through lists must be rejected");

        assert_eq!(
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>(),
            ["ignore_changes path `rule.action` passes through a list, which is not supported"]
        );
    }
}
