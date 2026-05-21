//! Reject chained references to schema-flagged "deferred-populate"
//! attributes that lack a synchronizing `wait` block (carina#3034).
//!
//! ACM `Certificate.domain_validation_options[*].resource_record_value`
//! is the canonical example: AWS does not populate the inner
//! `resource_record_*` fields until *after* `RequestCertificate`
//! returns, so a downstream `route53.RecordSet` that reads
//! `cert.domain_validation_options[0].resource_record_value` directly
//! will fail at apply time. The provider author has marked the
//! offending struct fields with `.deferred_populate()`; this pass
//! flags any chained access that traverses such a field unless the
//! user has declared `wait <binding> { until = ... }` against the
//! same target binding (or a transitive dependency thereof).
//!
//! Synchronization model: existence of *any* `wait` block on the
//! binding satisfies the rule. We do not require the wait predicate
//! to mention the specific accessed attribute — by the time a user
//! has declared a wait, they have asserted "this resource has reached
//! a steady state", which transitively guarantees the populated-
//! attribute set. Tightening the rule to "the wait predicate must
//! reference *this exact attribute*" would force users into one wait
//! per accessed attribute, which is impractical for nested structs
//! (the cert's wait predicate is `cert.status == ISSUED`, which
//! transitively guarantees DVO is populated, but the rule wouldn't
//! see the connection). See
//! `notes/specs/2026-05-14-deferred-populate-attribute-design.md`.
//!
//! Defense-in-depth: the apply-time fail-fast in
//! `executor/basic.rs::assert_fully_resolved` (carina#3032 / #3033)
//! still catches misannotated attributes at apply time. This pass
//! moves the *common* case to validate time so the user gets the
//! error in milliseconds, with an actionable suggestion, rather than
//! after a multi-minute apply.

use std::collections::{HashMap, HashSet};

use crate::parser::File;
use crate::resource::{
    AccessPath, ConcreteValue, DeferredValue, InterpolationPart, PathSegment, Value,
};
use crate::schema::{AttributeSchema, AttributeType, SchemaKind, SchemaRegistry};

/// One diagnostic for a deferred-populate-bound chained reference
/// that lacks a synchronizing `wait` block.
///
/// `binding` and `attribute_path` carry structured location hints so
/// the LSP can resolve a per-span anchor without re-parsing the
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredPopulateDiagnostic {
    /// The error message (same wording for `carina validate` and LSP).
    pub message: String,
    /// The DSL binding name of the resource holding the offending
    /// attribute (e.g. the route53 RecordSet's binding).
    pub holder_binding: Option<String>,
    /// The attribute key on the holder resource that contains the
    /// offending chained reference (e.g. `resource_records`).
    pub attribute_key: String,
    /// The full unresolved path as the user wrote it (e.g.
    /// `cert.domain_validation_options[0].resource_record_value`).
    pub unresolved_path: String,
    /// The target binding that needs a `wait` (e.g. `cert`).
    pub target_binding: String,
}

/// Run the deferred-populate diagnostic against a parsed file +
/// schema registry.
pub fn validate_deferred_populate_refs<E>(
    parsed: &File<E>,
    schemas: &SchemaRegistry,
) -> Vec<DeferredPopulateDiagnostic> {
    // binding name → (provider, resource_type) for schema lookup.
    // Walks both top-level resources and for-expression bodies so a
    // chained ref inside a `for` body still finds its target binding.
    let mut by_binding: HashMap<String, (String, String)> = HashMap::new();
    for rref in parsed.iter_all_resources() {
        if let Some(b) = rref.binding() {
            let id = rref.id();
            by_binding.insert(
                b.to_string(),
                (id.provider.clone(), id.resource_type.clone()),
            );
        }
    }
    if by_binding.is_empty() {
        return Vec::new();
    }

    // Set of binding names that have a `wait` block declared against
    // them in the same directory. A wait on binding `cert` satisfies
    // the rule for ANY chained access on `cert` (see module doc).
    let synchronized: HashSet<&str> = parsed
        .wait_bindings
        .iter()
        .map(|wb| wb.target.as_str())
        .collect();

    let mut out: Vec<DeferredPopulateDiagnostic> = Vec::new();
    for rref in parsed.iter_all_resources() {
        for (key, value) in rref.attributes() {
            collect_unsynchronized_refs(
                value,
                key,
                rref.binding(),
                &by_binding,
                &synchronized,
                schemas,
                &mut out,
            );
        }
    }
    out
}

/// Walk `value` recursively, emitting one diagnostic per chained ref
/// that targets a deferred-populate-flagged attribute on an
/// unsynchronized binding.
fn collect_unsynchronized_refs(
    value: &Value,
    attribute_key: &str,
    holder_binding: Option<&str>,
    by_binding: &HashMap<String, (String, String)>,
    synchronized: &HashSet<&str>,
    schemas: &SchemaRegistry,
    out: &mut Vec<DeferredPopulateDiagnostic>,
) {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            check_ref(
                path,
                attribute_key,
                holder_binding,
                by_binding,
                synchronized,
                schemas,
                out,
            );
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                collect_unsynchronized_refs(
                    item,
                    attribute_key,
                    holder_binding,
                    by_binding,
                    synchronized,
                    schemas,
                    out,
                );
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for v in map.values() {
                collect_unsynchronized_refs(
                    v,
                    attribute_key,
                    holder_binding,
                    by_binding,
                    synchronized,
                    schemas,
                    out,
                );
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(v) = part {
                    collect_unsynchronized_refs(
                        v,
                        attribute_key,
                        holder_binding,
                        by_binding,
                        synchronized,
                        schemas,
                        out,
                    );
                }
            }
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            for arg in args {
                collect_unsynchronized_refs(
                    arg,
                    attribute_key,
                    holder_binding,
                    by_binding,
                    synchronized,
                    schemas,
                    out,
                );
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            collect_unsynchronized_refs(
                inner,
                attribute_key,
                holder_binding,
                by_binding,
                synchronized,
                schemas,
                out,
            );
        }
        _ => {}
    }
}

/// Check one `ResourceRef` against the schema and the wait set.
fn check_ref(
    path: &AccessPath,
    attribute_key: &str,
    holder_binding: Option<&str>,
    by_binding: &HashMap<String, (String, String)>,
    synchronized: &HashSet<&str>,
    schemas: &SchemaRegistry,
    out: &mut Vec<DeferredPopulateDiagnostic>,
) {
    let target = path.binding();
    if synchronized.contains(target) {
        return;
    }
    let Some((provider, resource_type)) = by_binding.get(target) else {
        // Unknown binding (typo, cross-file ref the parser couldn't
        // resolve, …). Other passes report that — staying silent here
        // avoids spurious double diagnostics.
        return;
    };
    let Some(schema) = schemas.get(provider, resource_type, SchemaKind::Managed) else {
        return;
    };
    let Some(attr_schema) = schema.attributes.get(path.attribute()) else {
        return;
    };

    if path_traverses_deferred(attr_schema, path.segments()) {
        out.push(DeferredPopulateDiagnostic {
            message: format!(
                "attribute `{attribute_key}` references `{}`, which is populated asynchronously by the provider after Create. \
                 Add a `wait {target} {{ until = ... }}` block in this directory before downstream resources read this attribute.",
                path.to_dot_string(),
            ),
            holder_binding: holder_binding.map(str::to_string),
            attribute_key: attribute_key.to_string(),
            unresolved_path: path.to_dot_string(),
            target_binding: target.to_string(),
        });
    }
}

/// Walk `(top_attr, segments)` over the schema graph; return true if
/// any hop traverses a `deferred_populate=true` attribute or struct
/// field. The top-level attribute itself counts (an
/// `AttributeSchema.deferred_populate=true` value is deferred-bound
/// for *every* downstream chained access on it, even with no
/// segments).
fn path_traverses_deferred(top_attr: &AttributeSchema, segments: &[PathSegment]) -> bool {
    if top_attr.deferred_populate {
        return true;
    }
    let mut current_type = &top_attr.attr_type;
    for segment in segments {
        // Peel List wrappers introduced by `[idx]` segments. A
        // `Subscript::Int` traverses `List<T>` to `T`; a
        // `Subscript::Str` traverses `Map<_, V>` to `V`. Either way
        // the resulting `current_type` is the element type — no
        // deferred flag lives on List/Map themselves (they have no
        // such field on their builders).
        match (current_type, segment) {
            (AttributeType::List { inner, .. }, PathSegment::Subscript { .. }) => {
                current_type = inner;
            }
            (AttributeType::Map { value, .. }, PathSegment::Subscript { .. }) => {
                current_type = value;
            }
            (AttributeType::Struct { fields, .. }, PathSegment::Field { name }) => {
                let Some(field) = fields.iter().find(|f| &f.name == name) else {
                    return false;
                };
                if field.deferred_populate {
                    return true;
                }
                current_type = &field.field_type;
            }
            // `[idx]` against a Struct, `.field` against a List/Map,
            // etc. — the type-narrowing pass (carina#3028) catches
            // these as separate diagnostics. Bailing here matches the
            // resolver's bail-on-mismatch behaviour and avoids
            // surfacing a confusing double-diagnostic for the same
            // typo.
            _ => return false,
        }
        // After traversing a List<Struct> via [idx], the next field
        // hop looks up against the inner Struct directly because
        // `current_type` was just rebound to the list's element type.
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ParsedFile, WaitBinding};
    use crate::resource::{
        AccessPath, ConcreteValue, DeferredValue, PathSegment, Resource, ResourceId, Subscript,
        Value,
    };
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    };

    fn cert_schema_with_dvo_inner_field_deferred() -> ResourceSchema {
        ResourceSchema::new("acm.Certificate").attribute(AttributeSchema::new(
            "domain_validation_options",
            AttributeType::list(AttributeType::Struct {
                name: "DomainValidationOption".to_string(),
                fields: vec![
                    StructField::new("domain_name", AttributeType::String),
                    StructField::new("resource_record_value", AttributeType::String)
                        .deferred_populate(),
                ],
            }),
        ))
    }

    fn rrset_schema() -> ResourceSchema {
        ResourceSchema::new("route53.RecordSet")
            .attribute(AttributeSchema::new(
                "resource_records",
                AttributeType::list(AttributeType::String),
            ))
            .attribute(AttributeSchema::new("name", AttributeType::String))
    }

    fn dvo_chained_ref(field: &str) -> Value {
        let path = AccessPath::with_segments(
            "cert",
            "domain_validation_options",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: field.to_string(),
                },
            ],
        );
        Value::Deferred(DeferredValue::ResourceRef { path })
    }

    /// Build a `ParsedFile` with a cert binding + a route53 RecordSet
    /// that references the deferred field.
    fn parsed_with_unsynchronized_chained_ref() -> ParsedFile {
        let mut cert = Resource::new("acm.Certificate", "cert");
        cert.id = ResourceId::new("acm.Certificate", "cert");
        cert.id.provider = "aws".to_string();
        cert.binding = Some("cert".to_string());

        let mut record = Resource::new("route53.RecordSet", "record");
        record.id = ResourceId::new("route53.RecordSet", "record");
        record.id.provider = "aws".to_string();
        record.binding = Some("record".to_string());
        record.set_attr(
            "resource_records",
            Value::Concrete(ConcreteValue::List(vec![dvo_chained_ref(
                "resource_record_value",
            )])),
        );

        ParsedFile {
            resources: vec![cert, record],
            ..ParsedFile::default()
        }
    }

    fn registry_for_acm() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.insert("aws", cert_schema_with_dvo_inner_field_deferred());
        r.insert("aws", rrset_schema());
        r
    }

    #[test]
    fn list_wrapped_chained_ref_to_deferred_inner_field_is_flagged() {
        let parsed = parsed_with_unsynchronized_chained_ref();
        let diags = validate_deferred_populate_refs(&parsed, &registry_for_acm());
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        let d = &diags[0];
        assert_eq!(d.target_binding, "cert");
        assert_eq!(d.attribute_key, "resource_records");
        assert!(
            d.unresolved_path
                .contains("domain_validation_options[0].resource_record_value"),
            "got: {}",
            d.unresolved_path
        );
        assert!(d.message.contains("wait cert"), "got: {}", d.message);
    }

    #[test]
    fn wait_block_on_target_binding_satisfies_the_rule() {
        let mut parsed = parsed_with_unsynchronized_chained_ref();
        // A wait on the cert binding — predicate text is irrelevant
        // for this pass; presence is the contract.
        parsed.wait_bindings.push(WaitBinding {
            binding: "cert_issued".into(),
            target: "cert".into(),
            until_raw: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            until_predicate: crate::parser::UntilPredicateAst {
                lhs_segments: vec!["cert".to_string(), "status".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            timeout_secs: None,
            depends_on: vec![],
            line: 0,
        });
        let diags = validate_deferred_populate_refs(&parsed, &registry_for_acm());
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn chained_ref_to_non_deferred_inner_field_is_not_flagged() {
        let mut record = Resource::new("route53.RecordSet", "record");
        record.id = ResourceId::new("route53.RecordSet", "record");
        record.id.provider = "aws".to_string();
        record.binding = Some("record".to_string());
        record.set_attr("name", dvo_chained_ref("domain_name"));

        let mut cert = Resource::new("acm.Certificate", "cert");
        cert.id = ResourceId::new("acm.Certificate", "cert");
        cert.id.provider = "aws".to_string();
        cert.binding = Some("cert".to_string());

        let p = ParsedFile {
            resources: vec![cert, record],
            ..ParsedFile::default()
        };
        let diags = validate_deferred_populate_refs(&p, &registry_for_acm());
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn unknown_target_binding_does_not_emit_double_diagnostic() {
        // A typo in the binding name produces an undefined-identifier
        // error elsewhere; this pass must not pile on.
        let mut record = Resource::new("route53.RecordSet", "record");
        record.id = ResourceId::new("route53.RecordSet", "record");
        record.id.provider = "aws".to_string();
        record.binding = Some("record".to_string());
        let path = AccessPath::with_segments(
            "typo_cert", // <-- unknown binding
            "domain_validation_options",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "resource_record_value".to_string(),
                },
            ],
        );
        record.set_attr(
            "resource_records",
            Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                DeferredValue::ResourceRef { path },
            )])),
        );

        let p = ParsedFile {
            resources: vec![record],
            ..ParsedFile::default()
        };
        let diags = validate_deferred_populate_refs(&p, &registry_for_acm());
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn top_level_attribute_marked_deferred_flags_any_segment_chain() {
        // Mirror RDS DBInstance.endpoint shape: the whole attribute
        // is undefined immediately post-Create.
        let schema = ResourceSchema::new("rds.DBInstance").attribute(
            AttributeSchema::new(
                "endpoint",
                AttributeType::Struct {
                    name: "Endpoint".to_string(),
                    fields: vec![StructField::new("address", AttributeType::String)],
                },
            )
            .deferred_populate(),
        );
        let consumer = ResourceSchema::new("ec2.Instance")
            .attribute(AttributeSchema::new("user_data", AttributeType::String));
        let mut r = SchemaRegistry::new();
        r.insert("aws", schema);
        r.insert("aws", consumer);

        let mut db = Resource::new("rds.DBInstance", "db");
        db.id = ResourceId::new("rds.DBInstance", "db");
        db.id.provider = "aws".to_string();
        db.binding = Some("db".to_string());

        let mut inst = Resource::new("ec2.Instance", "i");
        inst.id = ResourceId::new("ec2.Instance", "i");
        inst.id.provider = "aws".to_string();
        inst.binding = Some("i".to_string());
        let path = AccessPath::with_segments(
            "db",
            "endpoint",
            vec![PathSegment::Field {
                name: "address".to_string(),
            }],
        );
        inst.set_attr(
            "user_data",
            Value::Deferred(DeferredValue::ResourceRef { path }),
        );

        let p = ParsedFile {
            resources: vec![db, inst],
            ..ParsedFile::default()
        };
        let diags = validate_deferred_populate_refs(&p, &r);
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert_eq!(diags[0].target_binding, "db");
    }

    #[test]
    fn schema_lookup_miss_does_not_panic_or_emit() {
        // Resource type not in the registry — pass should bail
        // silently. Other passes report unknown resource types.
        let mut record = Resource::new("unknown.thing", "x");
        record.id = ResourceId::new("unknown.thing", "x");
        record.id.provider = "aws".to_string();
        record.binding = Some("x".to_string());
        record.set_attr(
            "resource_records",
            Value::Concrete(ConcreteValue::List(vec![dvo_chained_ref(
                "resource_record_value",
            )])),
        );
        let p = ParsedFile {
            resources: vec![record],
            ..ParsedFile::default()
        };
        let diags = validate_deferred_populate_refs(&p, &registry_for_acm());
        // Schema for "unknown.thing" missing means nothing to check;
        // crucially no panic.
        assert!(diags.is_empty(), "got: {diags:?}");
    }
}
