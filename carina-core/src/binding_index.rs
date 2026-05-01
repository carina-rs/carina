//! Single source of truth for binding-name lookups (#2231 / #2251).
//!
//! Two sibling views live here:
//!
//! - [`BindingIndex`] — the schema-aware view, `binding_name → (resource,
//!   schema)`. Used by validation and the LSP.
//! - [`ResolvedBindings`] — the value-aware view, `binding_name →
//!   merged attributes`. Used by the resolver (#2299), and will be used
//!   by the executor (#2300).
//!
//! They are separate types because their invariants differ: every
//! `BindingIndex` entry has a `Resource` and a `ResourceSchema`, while
//! `ResolvedBindings` includes upstream-state bindings that have neither.
//! Callers that need both views (e.g. validation that wants the schema
//! *and* the resolved value) hold both indexes side by side.
//!
//! Parser is still out of scope and will be folded in by #2301.
//!
//! Built once at the parse → validate boundary, then borrowed. The walk
//! is **top-level only** (`parsed.resources`) on purpose: for-body
//! template resources carry a parser-synthesised binding name used for
//! address derivation, but those names are an internal detail and were
//! never visible to validation's `binding_map` pre-#2231. Surfacing them
//! through `BindingIndex` would let ResourceRefs name them, which is a
//! behaviour change neither validation nor the LSP wants. The LSP still
//! walks `iter_all_resources` separately for its own checks (so for-body
//! type/enum diagnostics fire); only the binding-name table is scoped.
//!
//! ```ignore
//! let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_fn);
//! if let Some(entry) = index.get("vpc") {
//!     // entry.resource and entry.schema are both available
//! }
//! ```

use crate::parser::ParsedFile;
use crate::resource::{Resource, ResourceId, State, Value};
use crate::schema::ResourceSchema;
use std::collections::HashMap;

/// One entry in the binding index. Both fields are non-`Option` because the
/// builder skips bindings whose schema cannot be resolved — callers never
/// have to defend against half-populated entries.
#[derive(Debug)]
pub struct BindingEntry<'a> {
    pub resource: &'a Resource,
    pub schema: &'a ResourceSchema,
}

/// Index of `binding_name → (resource, schema)` for every named binding
/// declared in `parsed`. Lifetime `'a` ties the index to its inputs so
/// callers can keep it borrowed without cloning.
///
/// `entries` only contains bindings whose schema resolved successfully.
/// `known_names` records every named binding regardless of schema status,
/// so callers can tell "unknown binding" apart from "binding exists but
/// its schema is missing" — those are separate diagnostics.
#[derive(Debug, Default)]
pub struct BindingIndex<'a> {
    entries: HashMap<String, BindingEntry<'a>>,
    known_names: std::collections::HashSet<String>,
}

impl<'a> BindingIndex<'a> {
    /// Build the index from a parsed file and a schema map. `schema_key_fn`
    /// converts a `Resource` to the key under which its schema is stored
    /// (e.g. `"aws.s3.Bucket" -> "s3.Bucket"`); validation and the LSP both
    /// already pass such a function around, so the contract here mirrors
    /// theirs.
    ///
    /// Bindings whose schema is missing from `schemas` are silently skipped
    /// — callers (validation / LSP) treat unknown resource types as a
    /// separate diagnostic, so reporting them again here would double-count.
    pub fn from_parsed(
        parsed: &'a ParsedFile,
        schemas: &'a HashMap<String, ResourceSchema>,
        schema_key_fn: &dyn Fn(&Resource) -> String,
    ) -> Self {
        let mut entries = HashMap::new();
        let mut known_names = std::collections::HashSet::new();
        // Walk top-level resources only. The parser auto-generates a
        // synthetic `binding` for anonymous for-body templates (used for
        // resource address derivation), but those names are an internal
        // detail — they were never visible to validation's binding map
        // pre-#2231 and surfacing them here would be an unintended
        // behaviour change for ResourceRef lookups. The LSP and
        // validation both still walk `iter_all_resources` *separately*
        // for their own checks; only the binding-name table is scoped to
        // top-level here.
        for resource in &parsed.resources {
            let Some(binding_name) = resource.binding.as_ref() else {
                continue;
            };
            known_names.insert(binding_name.clone());
            let Some(schema) = schemas.get(&schema_key_fn(resource)) else {
                continue;
            };
            entries.insert(binding_name.clone(), BindingEntry { resource, schema });
        }
        Self {
            entries,
            known_names,
        }
    }

    pub fn get(&self, name: &str) -> Option<&BindingEntry<'a>> {
        self.entries.get(name)
    }

    /// True iff a binding by this name was declared anywhere in the parsed
    /// file, *even if its schema could not be resolved*. Used to
    /// distinguish "unknown binding" diagnostics from "known binding but
    /// schema missing" (which is a different diagnostic surface).
    pub fn is_declared(&self, name: &str) -> bool {
        self.known_names.contains(name)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Borrow-valued projection — `binding_name → &ResourceSchema`. Lets
    /// callers that previously took a `HashMap<String, ResourceSchema>` of
    /// schema clones switch to a borrow-only map without changing their
    /// signatures' shape (just the value type). Saves a per-binding,
    /// per-keystroke `ResourceSchema::clone()` on the LSP path.
    pub fn schemas_by_name(&self) -> HashMap<&str, &'a ResourceSchema> {
        self.entries
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.schema))
            .collect()
    }
}

/// Where the values for a binding came from. `Local` means a `let` binding
/// in the current configuration; `Upstream` means an `upstream_state` data
/// source bringing values in from another state file.
///
/// Marked `#[non_exhaustive]` because #2301 will introduce structural
/// sources (`for` / `if` / module) — flagging the intent now keeps
/// downstream `match` arms from becoming exhaustive against the current
/// two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindingValueSource {
    Local,
    Upstream,
}

/// Value-aware sibling of [`BindingIndex`]. See the module-level doc for
/// why this is a separate type.
///
/// Owned, not borrowed: building the merged map requires combining
/// `Resource.attributes` with `State.attributes`, and there is no single
/// upstream `HashMap<String, HashMap<String, Value>>` already on hand to
/// borrow from. Owning the merged map avoids a self-referential
/// structure.
///
/// The two internal maps are populated in lockstep by
/// `from_resources_with_state` (the only writer) so a `name` is always
/// present in both or neither. Once `resolve_ref_value` and the
/// executor stop taking a raw `&HashMap<String, HashMap<String, Value>>`
/// (#2300), the storage will fold into a single `HashMap<String,
/// ResolvedBinding>`; until then the parallel shape lets `as_map()`
/// hand out a borrow of the legacy view without an extra allocation.
#[derive(Debug, Default)]
pub struct ResolvedBindings {
    attrs: HashMap<String, HashMap<String, Value>>,
    sources: HashMap<String, BindingValueSource>,
}

impl ResolvedBindings {
    /// Build the resolved view from the same three inputs the resolver
    /// already takes: top-level resources (DSL), the last-known state map,
    /// and upstream-state bindings.
    ///
    /// DSL keys win over state keys on conflict. Upstream bindings are
    /// inserted last and overwrite any local binding of the same name.
    /// Both rules match the pre-existing resolver behaviour so this
    /// refactor is a pure internal change.
    pub fn from_resources_with_state(
        resources: &[Resource],
        current_states: &HashMap<ResourceId, State>,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
    ) -> Self {
        let mut attrs: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut sources: HashMap<String, BindingValueSource> = HashMap::new();

        for resource in resources.iter() {
            let Some(binding_name) = resource.binding.as_ref() else {
                continue;
            };
            let mut merged: HashMap<String, Value> = resource.resolved_attributes();
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    merged.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            attrs.insert(binding_name.clone(), merged);
            sources.insert(binding_name.clone(), BindingValueSource::Local);
        }

        for (remote_binding, remote_attrs) in remote_bindings {
            attrs.insert(remote_binding.clone(), remote_attrs.clone());
            sources.insert(remote_binding.clone(), BindingValueSource::Upstream);
        }

        Self { attrs, sources }
    }

    pub fn get(&self, name: &str) -> Option<&HashMap<String, Value>> {
        self.attrs.get(name)
    }

    pub fn source(&self, name: &str) -> Option<BindingValueSource> {
        self.sources.get(name).copied()
    }

    /// Borrow the legacy `binding_name → attributes` view.
    ///
    /// `resolve_ref_value` (and the executor, via #2300) still take
    /// `&HashMap<String, HashMap<String, Value>>` — exposing it
    /// directly avoids both an extra allocation and a public-API change
    /// in `carina-cli` while #2299 lands. Slated to disappear once
    /// #2300 threads `&ResolvedBindings` all the way through.
    pub fn as_map(&self) -> &HashMap<String, HashMap<String, Value>> {
        &self.attrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn schema_key_aws(r: &Resource) -> String {
        format!("{}.{}", r.id.provider, r.id.resource_type)
    }

    fn vpc_schema() -> ResourceSchema {
        ResourceSchema::new("aws.ec2.Vpc")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
    }

    #[test]
    fn build_indexes_named_let_binding() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut schemas = HashMap::new();
        schemas.insert("aws.ec2.Vpc".to_string(), vpc_schema());

        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);
        let entry = index.get("vpc").expect("vpc binding present");
        assert_eq!(entry.schema.resource_type, "aws.ec2.Vpc");
        assert_eq!(entry.resource.binding.as_deref(), Some("vpc"));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn build_skips_anonymous_resources() {
        // No `let` binding — anonymous resources never appear in the index
        // because they cannot be referenced by name.
        let src = r#"
aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut schemas = HashMap::new();
        schemas.insert("aws.ec2.Vpc".to_string(), vpc_schema());

        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);
        assert!(index.is_empty());
    }

    #[test]
    fn build_skips_bindings_with_unknown_schema() {
        // The Vpc binding exists but its schema is not registered. Callers
        // (validation / LSP) raise a separate "unknown resource type"
        // diagnostic, so `get` returns None — but `contains_name` still
        // says yes, so a "unknown binding" diagnostic is not double-fired
        // on top of the "unknown resource type" one.
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let schemas: HashMap<String, ResourceSchema> = HashMap::new();

        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);
        assert!(index.get("vpc").is_none());
        assert!(
            index.is_declared("vpc"),
            "binding declared in source must show up in `known_names`",
        );
    }

    #[test]
    fn build_includes_only_named_top_level_bindings_not_for_body_templates() {
        // For-body template resources never carry a `binding`, so they
        // must not appear in the index — the iter walks all resources
        // (top-level and for-body) for parity with `iter_all_resources`,
        // but the binding filter weeds out the unnamed ones.
        let src = r#"
let net = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

for _, n in some_iterable {
    aws.ec2.Vpc {
        name = n
        cidr_block = "10.0.0.0/16"
    }
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut schemas = HashMap::new();
        schemas.insert("aws.ec2.Vpc".to_string(), vpc_schema());

        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);
        assert!(index.get("net").is_some(), "named let binding indexed");
        assert!(
            !index.is_declared("n"),
            "for-body iteration variable is not a binding the index should know about",
        );
        // For-body templates carry parser-synthesised internal bindings
        // (used for address derivation) — those names never surfaced in
        // the pre-#2231 validation map and `BindingIndex::from_parsed`
        // preserves that contract by walking top-level only.
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn schemas_by_name_returns_borrowed_schemas() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut schemas = HashMap::new();
        schemas.insert("aws.ec2.Vpc".to_string(), vpc_schema());
        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);

        let by_name = index.schemas_by_name();
        let schema = by_name.get("vpc").expect("vpc projection present");
        assert_eq!(schema.resource_type, "aws.ec2.Vpc");
        // The projection borrows from the index — pointer-equal to
        // `index.get(...).schema`, which is the whole point.
        assert!(std::ptr::eq(*schema, index.get("vpc").unwrap().schema));
    }

    #[test]
    fn get_returns_none_for_unknown_name() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parse(src, &Default::default()).expect("parse");
        let mut schemas = HashMap::new();
        schemas.insert("aws.ec2.Vpc".to_string(), vpc_schema());
        let index = BindingIndex::from_parsed(&parsed, &schemas, &schema_key_aws);
        assert!(index.get("missing").is_none());
    }
}

#[cfg(test)]
mod resolved_bindings_tests {
    use super::*;
    use crate::resource::{Resource, ResourceId, State, Value};
    use std::collections::BTreeSet;

    fn make_resource(name: &str, binding: Option<&str>, attrs: Vec<(&str, Value)>) -> Resource {
        let mut r = Resource::new("test.resource", name);
        r.attributes = attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        r.binding = binding.map(|b| b.to_string());
        r
    }

    #[test]
    fn local_binding_carries_dsl_attributes() {
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(resolved.source("vpc"), Some(BindingValueSource::Local));
    }

    #[test]
    fn local_binding_merges_state_attributes_when_dsl_missing_them() {
        let rid = ResourceId::new("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: true,
                attributes: vec![
                    ("id".to_string(), Value::String("vpc-abc".to_string())),
                    // conflicting key — DSL value should win
                    ("cidr_block".to_string(), Value::String("WRONG".to_string())),
                ]
                .into_iter()
                .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("id"),
            Some(&Value::String("vpc-abc".to_string())),
            "state-only attribute must be merged in",
        );
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string())),
            "DSL value must win when both sides define a key",
        );
    }

    #[test]
    fn upstream_state_binding_is_first_class() {
        // Upstream-state bindings have no `Resource` and no `ResourceSchema`,
        // which is the case `BindingIndex` cannot represent.
        let resources: Vec<Resource> = Vec::new();
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut network_attrs = HashMap::new();
        network_attrs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        remote.insert("network".to_string(), network_attrs);

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);

        let attrs = resolved.get("network").expect("upstream binding present");
        assert_eq!(
            attrs.get("vpc_id"),
            Some(&Value::String("vpc-123".to_string()))
        );
        assert_eq!(
            resolved.source("network"),
            Some(BindingValueSource::Upstream)
        );
    }

    #[test]
    fn unbound_resources_are_excluded() {
        let resources = vec![make_resource(
            "anonymous",
            None,
            vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);
        assert!(resolved.get("anonymous").is_none());
    }

    #[test]
    fn upstream_overrides_local_on_name_collision() {
        // The pre-#2299 resolver inserted remote bindings *after* local
        // ones, so upstream wins when both define the same name. This is
        // load-bearing behaviour — preserving it makes #2299 a no-op for
        // configurations that already rely on it.
        let resources = vec![make_resource(
            "my-local",
            Some("shared"),
            vec![("kind", Value::String("local".to_string()))],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let mut remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        remote.insert(
            "shared".to_string(),
            vec![("kind".to_string(), Value::String("upstream".to_string()))]
                .into_iter()
                .collect(),
        );

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);
        let attrs = resolved.get("shared").expect("shared binding present");
        assert_eq!(
            attrs.get("kind"),
            Some(&Value::String("upstream".to_string())),
            "upstream binding must override local one with the same name",
        );
        assert_eq!(
            resolved.source("shared"),
            Some(BindingValueSource::Upstream),
        );
    }

    #[test]
    fn destroyed_state_does_not_contribute_attributes() {
        // `State.exists = false` represents a tombstone (resource was
        // destroyed since the last apply). The resolver must not merge
        // its attributes — they describe a no-longer-real resource.
        let rid = ResourceId::new("test.resource", "my-vpc");
        let resources = vec![make_resource(
            "my-vpc",
            Some("vpc"),
            vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
        )];
        let mut states: HashMap<ResourceId, State> = HashMap::new();
        states.insert(
            rid.clone(),
            State {
                id: rid,
                identifier: None,
                exists: false,
                attributes: vec![("id".to_string(), Value::String("vpc-stale".to_string()))]
                    .into_iter()
                    .collect(),
                dependency_bindings: BTreeSet::new(),
            },
        );
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();

        let resolved = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);
        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert!(
            attrs.get("id").is_none(),
            "tombstoned state must not contribute attributes",
        );
    }
}
