//! Single source of truth for the `binding_name → (resource, schema)` lookup
//! that validation and the LSP both need (#2231).
//!
//! The four pre-existing call sites that hand-rolled their own binding map
//! (parser, resolver, validation, LSP) historically drifted in subtle ways —
//! the LSP saw a different set of bindings than the parser, structural
//! `for`/`if` bindings were missed in some checks, and adding a new binding
//! source meant chasing four files. This type collapses **the schema-lookup
//! shape** (validation + LSP) into one builder.
//!
//! Resolver and parser are deliberately out of scope here: resolver's map
//! holds owned merged attribute values (DSL + AWS state), which is a
//! different shape, and parser's notion of "in-scope names" runs before
//! schemas are known. Both can share `BindingIndex` later by extending
//! `BindingEntry`; for now the smaller surface keeps the refactor reviewable.
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
use crate::resource::Resource;
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
