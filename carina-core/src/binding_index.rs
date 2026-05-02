//! Single source of truth for binding-name lookups (#2231 / #2251).
//!
//! Three sibling views live here, each answering a different question
//! about the same set of names declared in a parsed Carina configuration:
//!
//! - [`BindingNameSet`] — "is this identifier in scope at all?" View
//!   for the parser and LSP scope-check pass. Records all eight
//!   declaration kinds the parser tracks (resource, argument, module
//!   call, upstream state, import alias, user function, variable,
//!   structural binding from let-of-if/for/read).
//! - [`BindingIndex`] — "what schema and which `Resource` does this
//!   binding point at?" Schema-aware view. Used by validation and the
//!   LSP. Each entry has both a `Resource` and a `ResourceSchema`.
//! - [`ResolvedBindings`] — "what attribute values does this binding
//!   actually carry?" Value-aware view. Used by the resolver and
//!   executor. Includes upstream-state bindings that have no `Resource`
//!   or `ResourceSchema`.
//!
//! The three are separate types rather than fields on one because their
//! invariants differ — `BindingIndex` requires both `Resource` and
//! `ResourceSchema`, `ResolvedBindings` requires only attribute values
//! (no schema), `BindingNameSet` requires only the name and an origin
//! tag. Callers that need more than one view hold them side by side.
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
//! let index = BindingIndex::from_parsed(&parsed, &registry);
//! if let Some(entry) = index.get("vpc") {
//!     // entry.resource and entry.schema are both available
//! }
//! ```

use crate::parser::ParsedFile;
use crate::resource::{Resource, ResourceId, State, Value};
use crate::schema::{ResourceSchema, SchemaRegistry};
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
    /// Build the index from a parsed file and a [`SchemaRegistry`]. The
    /// registry handles the `(provider, resource_type, kind) -> schema`
    /// lookup, including the kind-aware split between managed resources
    /// and data sources.
    ///
    /// Bindings whose schema is missing from the registry are silently
    /// skipped — callers (validation / LSP) treat unknown resource types
    /// as a separate diagnostic, so reporting them again here would
    /// double-count.
    pub fn from_parsed(parsed: &'a ParsedFile, registry: &'a SchemaRegistry) -> Self {
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
            let Some(schema) = registry.get_for(resource) else {
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

/// Origin of a binding name in [`BindingNameSet`]. Eight kinds, one per
/// declaration form a parsed Carina configuration can introduce.
///
/// Lives on `BindingNameSet` (the scope-only view) rather than on
/// [`BindingValueSource`] (the value-aware view) because not every kind
/// has a value: `Use` (import alias) and `UserFunction` are pure
/// identifiers that the resolver/executor never need to look up
/// attributes on.
///
/// `#[non_exhaustive]` so adding a new declaration form (a future
/// `data` block, etc.) does not break downstream `match` arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BindingNameKind {
    /// `let <name> = ...` resource binding.
    Resource,
    /// `argument <name> { ... }` module argument.
    Argument,
    /// `module <name> "..." { ... }` call site.
    ModuleCall,
    /// `upstream_state <name> "..." { ... }` data source.
    UpstreamState,
    /// `use <alias> from "..."` import.
    Use,
    /// `function <name>(...) { ... }` user-defined function.
    UserFunction,
    /// `variable <name> { ... }` config variable.
    Variable,
    /// Iteration / condition binding from `for <name> in ...` or
    /// equivalent structural form. Recorded for scope checks but not
    /// addressable by `ResourceRef` (see the module doc for the
    /// pre-existing invariant).
    Structural,
}

/// Name-only scope view: every identifier a parsed Carina configuration
/// has brought into scope, tagged with its declaration kind.
///
/// This is the canonical replacement for the parser's
/// `collect_known_bindings_merged` helper (#2104 / #2301). The two
/// views ([`BindingIndex`] and [`ResolvedBindings`]) carry richer data
/// (schema; resolved values) but are restricted to bindings that
/// actually carry such data. `BindingNameSet` is the broadest of the
/// three — it answers "is this identifier in scope at all?" for
/// parser- and LSP-side identifier-scope diagnostics.
///
/// The set is built once from a [`ParsedFile`] (the merged
/// directory-level view) and borrowed thereafter; names are owned
/// `String`s so the set survives independent of the source.
#[derive(Debug, Default, Clone)]
pub struct BindingNameSet {
    by_name: HashMap<String, BindingNameKind>,
}

impl BindingNameSet {
    /// Build the set from a merged [`ParsedFile`], populating every
    /// declaration form the parser tracks.
    ///
    /// **Kind precedence on collisions**: a single name can appear in
    /// more than one parser-side map by design — for example, a `let
    /// vpc = aws.ec2.Vpc { ... }` registers `vpc` in both
    /// `parsed.resources` (as a resource binding) *and*
    /// `parsed.variables` (as a let-value binding, holding a
    /// placeholder ref). To keep the kind label meaningful, sources are
    /// inserted in **most-specific-first** order and the first
    /// `insert` wins (later sources call `entry().or_insert`).
    ///
    /// The order — `Resource` → `ModuleCall` → `UpstreamState` →
    /// `Argument` → `Use` → `UserFunction` → `Structural` → `Variable`
    /// — keeps `Variable` last because `parsed.variables` is the
    /// catch-all "any `let`-RHS placeholder lives here" map, while the
    /// preceding sources are narrower declaration forms.
    pub fn from_parsed(parsed: &ParsedFile) -> Self {
        let mut by_name: HashMap<String, BindingNameKind> = HashMap::new();

        for resource in &parsed.resources {
            if let Some(name) = resource.binding.as_deref() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::Resource);
            }
        }
        for call in &parsed.module_calls {
            if let Some(name) = call.binding_name.as_deref() {
                by_name
                    .entry(name.to_string())
                    .or_insert(BindingNameKind::ModuleCall);
            }
        }
        for upstream in &parsed.upstream_states {
            by_name
                .entry(upstream.binding.clone())
                .or_insert(BindingNameKind::UpstreamState);
        }
        for arg in &parsed.arguments {
            by_name
                .entry(arg.name.clone())
                .or_insert(BindingNameKind::Argument);
        }
        for use_decl in &parsed.uses {
            by_name
                .entry(use_decl.alias.clone())
                .or_insert(BindingNameKind::Use);
        }
        for fn_name in parsed.user_functions.keys() {
            by_name
                .entry(fn_name.clone())
                .or_insert(BindingNameKind::UserFunction);
        }
        for structural in &parsed.structural_bindings {
            by_name
                .entry(structural.clone())
                .or_insert(BindingNameKind::Structural);
        }
        for var_name in parsed.variables.keys() {
            by_name
                .entry(var_name.clone())
                .or_insert(BindingNameKind::Variable);
        }

        Self { by_name }
    }

    /// `true` iff `name` is declared somewhere in the parsed file.
    pub fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Declaration kind for `name`, or `None` if the name is not
    /// in scope.
    pub fn kind(&self, name: &str) -> Option<BindingNameKind> {
        self.by_name.get(name).copied()
    }

    /// Iterate every in-scope name. Order is unspecified.
    pub fn iter_names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// Iterate every (name, kind) pair. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, BindingNameKind)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
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

/// One entry in [`ResolvedBindings`]: the merged attribute map and the
/// source it came from. Stored as a single struct (rather than two
/// parallel maps) so the type system enforces "every name has both
/// attributes and a source".
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub attributes: HashMap<String, Value>,
    pub source: BindingValueSource,
}

/// Value-aware sibling of [`BindingIndex`]. See the module-level doc for
/// why this is a separate type.
///
/// Owned, not borrowed: building the merged map requires combining
/// `Resource.attributes` with `State.attributes`, and there is no single
/// upstream `HashMap<String, HashMap<String, Value>>` already on hand to
/// borrow from. Owning the merged map avoids a self-referential
/// structure.
#[derive(Debug, Default, Clone)]
pub struct ResolvedBindings {
    by_name: HashMap<String, ResolvedBinding>,
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
        let mut by_name: HashMap<String, ResolvedBinding> = HashMap::new();

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
            by_name.insert(
                binding_name.clone(),
                ResolvedBinding {
                    attributes: merged,
                    source: BindingValueSource::Local,
                },
            );
        }

        for (remote_binding, remote_attrs) in remote_bindings {
            by_name.insert(
                remote_binding.clone(),
                ResolvedBinding {
                    attributes: remote_attrs.clone(),
                    source: BindingValueSource::Upstream,
                },
            );
        }

        Self { by_name }
    }

    pub fn get(&self, name: &str) -> Option<&HashMap<String, Value>> {
        self.by_name.get(name).map(|b| &b.attributes)
    }

    pub fn source(&self, name: &str) -> Option<BindingValueSource> {
        self.by_name.get(name).map(|b| b.source)
    }

    /// Record (or refresh) a binding after a `Create` / `Update` effect
    /// returns its post-apply state.
    ///
    /// `resource_attrs` is the resolved DSL attribute map; `state` is
    /// what the provider just reported. **State wins on key collision** —
    /// the provider's freshly-returned values (e.g. an auto-assigned
    /// `id`) are by definition the source of truth for the binding's
    /// downstream consumers. This is the inverse of `from_resources_with_state`'s
    /// "DSL wins" pre-apply rule: pre-apply we trust the DSL, post-apply
    /// we trust the provider.
    ///
    /// Replaces the executor's `update_binding_map` helper (#2300).
    pub fn record_applied(
        &mut self,
        binding: Option<&str>,
        resource_attrs: &HashMap<String, Value>,
        state: &State,
    ) {
        let Some(name) = binding else {
            return;
        };
        let mut merged = resource_attrs.clone();
        for (k, v) in &state.attributes {
            merged.insert(k.clone(), v.clone());
        }
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: merged,
                source: BindingValueSource::Local,
            },
        );
    }

    /// Set a binding directly to the given attribute map and source.
    /// Replaces any existing entry with the same name.
    ///
    /// Used by call sites that already hold a fully-resolved attribute
    /// map and don't need the `record_applied` state-merge path — for
    /// example, post-apply state writeback that hydrates bindings
    /// straight out of `StateFile.resources[].attributes`.
    ///
    /// The name is `set` rather than `insert` to avoid sounding like
    /// `HashMap::insert` (which returns the previous value); this method
    /// is fire-and-forget.
    pub fn set(&mut self, name: &str, attrs: HashMap<String, Value>, source: BindingValueSource) {
        self.by_name.insert(
            name.to_string(),
            ResolvedBinding {
                attributes: attrs,
                source,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn vpc_schema() -> ResourceSchema {
        ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
    }

    fn registry_with_vpc() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.insert("aws", vpc_schema());
        r
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
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
        let entry = index.get("vpc").expect("vpc binding present");
        assert_eq!(entry.schema.resource_type, "ec2.Vpc");
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
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
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
        let registry = SchemaRegistry::new();

        let index = BindingIndex::from_parsed(&parsed, &registry);
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
        let registry = registry_with_vpc();

        let index = BindingIndex::from_parsed(&parsed, &registry);
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
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);

        let by_name = index.schemas_by_name();
        let schema = by_name.get("vpc").expect("vpc projection present");
        assert_eq!(schema.resource_type, "ec2.Vpc");
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
        let registry = registry_with_vpc();
        let index = BindingIndex::from_parsed(&parsed, &registry);
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

    #[test]
    fn record_applied_inserts_state_winning_merge() {
        // Post-apply semantics: the freshly returned `State.attributes`
        // must override the DSL `resource_attrs` on conflict — the
        // provider just told us the real value (e.g. an auto-assigned
        // `id` differs from the placeholder, or `arn` materialises). The
        // executor relied on this in `update_binding_map`, so the
        // `ResolvedBindings` API has to preserve it byte-for-byte.
        let mut resolved = ResolvedBindings::default();
        let resource_attrs: HashMap<String, Value> = vec![
            ("name".to_string(), Value::String("vpc-dsl".to_string())),
            (
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            ),
        ]
        .into_iter()
        .collect();
        let state = State {
            id: ResourceId::new("test.resource", "my-vpc"),
            identifier: None,
            exists: true,
            attributes: vec![
                // overrides DSL value
                ("name".to_string(), Value::String("vpc-applied".to_string())),
                // adds a state-only key
                ("id".to_string(), Value::String("vpc-abc".to_string())),
            ]
            .into_iter()
            .collect(),
            dependency_bindings: BTreeSet::new(),
        };

        resolved.record_applied(Some("vpc"), &resource_attrs, &state);

        let attrs = resolved.get("vpc").expect("vpc binding present");
        assert_eq!(
            attrs.get("name"),
            Some(&Value::String("vpc-applied".to_string())),
            "state must win over resource_attrs on conflict",
        );
        assert_eq!(attrs.get("id"), Some(&Value::String("vpc-abc".to_string())));
        assert_eq!(
            attrs.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string())),
        );
        assert_eq!(resolved.source("vpc"), Some(BindingValueSource::Local));
    }

    #[test]
    fn record_applied_with_no_binding_is_a_noop() {
        let mut resolved = ResolvedBindings::default();
        let attrs: HashMap<String, Value> = HashMap::new();
        let state = State {
            id: ResourceId::new("test.resource", "anon"),
            identifier: None,
            exists: true,
            attributes: HashMap::new(),
            dependency_bindings: BTreeSet::new(),
        };

        resolved.record_applied(None, &attrs, &state);
        assert!(resolved.get("anon").is_none());
    }

    #[test]
    fn cloned_view_does_not_share_storage_with_original() {
        // The phased / replace executor paths snapshot the binding map
        // for cascade frames: `local_binding_map = binding_snapshot.clone()`.
        // After #2300 the same pattern relies on `ResolvedBindings: Clone`,
        // and the clone must be independent so frame-local mutations
        // don't leak back into the parent snapshot.
        let resources = vec![make_resource(
            "vpc",
            Some("vpc"),
            vec![("cidr_block", Value::String("10.0.0.0/16".to_string()))],
        )];
        let states: HashMap<ResourceId, State> = HashMap::new();
        let remote: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let parent = ResolvedBindings::from_resources_with_state(&resources, &states, &remote);

        let mut child = parent.clone();
        let extra_state = State {
            id: ResourceId::new("test.resource", "subnet"),
            identifier: None,
            exists: true,
            attributes: vec![("id".to_string(), Value::String("subnet-1".to_string()))]
                .into_iter()
                .collect(),
            dependency_bindings: BTreeSet::new(),
        };
        child.record_applied(Some("subnet"), &HashMap::new(), &extra_state);

        assert!(child.get("subnet").is_some());
        assert!(
            parent.get("subnet").is_none(),
            "child mutation must not leak into the parent snapshot",
        );
    }

    #[test]
    fn set_replaces_entry_and_records_source() {
        // `set` is the no-merge counterpart to `record_applied` — used
        // when the caller already holds a fully-resolved attribute map
        // (e.g. post-apply state-writeback hydrating exports). Verify
        // it overwrites any existing entry and records the given source.
        let mut resolved = ResolvedBindings::default();
        let initial: HashMap<String, Value> =
            vec![("kind".to_string(), Value::String("first".to_string()))]
                .into_iter()
                .collect();
        resolved.set("registry", initial, BindingValueSource::Upstream);
        assert_eq!(
            resolved.source("registry"),
            Some(BindingValueSource::Upstream)
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::String("first".to_string()))
        );

        let replacement: HashMap<String, Value> =
            vec![("kind".to_string(), Value::String("second".to_string()))]
                .into_iter()
                .collect();
        resolved.set("registry", replacement, BindingValueSource::Local);
        assert_eq!(
            resolved.source("registry"),
            Some(BindingValueSource::Local),
            "set must replace the source as well as the attributes",
        );
        assert_eq!(
            resolved.get("registry").and_then(|a| a.get("kind")),
            Some(&Value::String("second".to_string()))
        );
    }
}

#[cfg(test)]
mod binding_name_set_tests {
    use super::*;
    use crate::parser::parse;

    fn parsed_with(src: &str) -> ParsedFile {
        parse(src, &Default::default()).expect("parse")
    }

    #[test]
    fn records_resource_let_bindings_as_resource_kind() {
        let src = r#"
let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("vpc"));
        assert_eq!(names.kind("vpc"), Some(BindingNameKind::Resource));
    }

    #[test]
    fn records_upstream_state_with_upstream_kind() {
        let src = r#"
let network = upstream_state {
    source = "../network"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("network"));
        assert_eq!(names.kind("network"), Some(BindingNameKind::UpstreamState));
    }

    #[test]
    fn records_top_level_let_value_with_variable_kind() {
        // `let <name> = <Value>` (non-resource RHS) is captured under
        // `parsed.variables` and surfaces as `Variable` in the name set.
        let src = r#"
let region = "ap-northeast-1"
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("region"));
        assert_eq!(names.kind("region"), Some(BindingNameKind::Variable));
    }

    #[test]
    fn records_arguments_block_entries_with_argument_kind() {
        let src = r#"
arguments {
    env: String
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("env"));
        assert_eq!(names.kind("env"), Some(BindingNameKind::Argument));
    }

    #[test]
    fn records_fn_def_with_user_function_kind() {
        let src = r#"
fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(names.contains("double"));
        assert_eq!(names.kind("double"), Some(BindingNameKind::UserFunction));
    }

    #[test]
    fn records_module_call_with_module_call_kind() {
        let src = r#"
let cluster = module {
    source = "../cluster"
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        assert!(
            names.contains("cluster"),
            "module-call binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("cluster"), Some(BindingNameKind::ModuleCall));
    }

    #[test]
    fn matches_collect_known_bindings_merged_byte_for_byte() {
        // BindingNameSet is the canonical replacement for
        // `collect_known_bindings_merged`. The set of names must be
        // identical so the replacement is a pure refactor.
        let src = r#"
arguments {
    env: String
}

let region = "ap-northeast-1"

let vpc = aws.ec2.Vpc {
    name = "v"
    cidr_block = "10.0.0.0/16"
}

let network = upstream_state {
    source = "../network"
}

fn double(x: Int) {
    x
}
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);
        let legacy = crate::parser::collect_known_bindings_merged(&parsed);

        let from_set: std::collections::HashSet<&str> = names.iter_names().collect();
        let legacy_set: std::collections::HashSet<&str> = legacy.into_iter().collect();
        assert_eq!(from_set, legacy_set);
    }

    #[test]
    fn structural_binding_is_in_scope_but_not_addressable_by_resource_ref() {
        // The pre-#2301 invariant: when a `let` binds the result of an
        // `if` / `for` / `read` expression, the parser flags the
        // binding `is_structural` and records the name in
        // `parsed.structural_bindings` (so unused-binding warnings
        // don't fire). Such names are visible to scope checks but not
        // surfaced through `ResolvedBindings` — the executor /
        // resolver address bindings by their attribute map, and a
        // structural binding's "value" is the entire if/for branch,
        // not an attribute set.
        let src = r#"
let chosen = if true { "primary" } else { "fallback" }
"#;
        let parsed = parsed_with(src);
        let names = BindingNameSet::from_parsed(&parsed);

        // Scope-side: `chosen` is in scope as a Structural kind.
        assert!(
            names.contains("chosen"),
            "structural binding must be in scope; got {:?}",
            names.iter_names().collect::<Vec<_>>()
        );
        assert_eq!(names.kind("chosen"), Some(BindingNameKind::Structural));

        // Value-side: `ResolvedBindings` does NOT carry an entry for
        // `chosen`, so a `ResourceRef` to `chosen.foo` cannot resolve.
        let resolved = ResolvedBindings::from_resources_with_state(
            &parsed.resources,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            resolved.get("chosen").is_none(),
            "structural bindings must stay invisible to ResourceRef resolution",
        );
    }
}
