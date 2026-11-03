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

use crate::parser::{AnalysisTree, DeclarationLocation, Located};
use crate::resource::{AccessPath, BindingAddress, ParserPlaceholder, PathSegment};
use crate::schema::{EffectiveAttribute, SchemaKind, SchemaRegistry, Shape};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredPopulateDiagnostic {
    pub message: String,
    pub attribute_key: String,
    pub unresolved_path: String,
    pub target_binding: String,
}
impl std::fmt::Display for DeferredPopulateDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, schema::SchemaRegistry, validation::deferred_populate::validate_deferred_populate_refs};
/// fn analyze(file: &ParsedFile) {
///     validate_deferred_populate_refs(file, &SchemaRegistry::new());
/// }
/// ```
pub fn validate_deferred_populate_refs<B: BindingAddress, D>(
    tree: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &SchemaRegistry,
) -> Vec<Located<DeferredPopulateDiagnostic>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let by_binding: HashMap<_, _> = tree
        .resources()
        .filter_map(|resource| {
            resource.binding_address().map(|binding| {
                (
                    binding.clone(),
                    (&resource.id().provider, &resource.id().resource_type),
                )
            })
        })
        .collect();
    let synchronized: HashSet<_> = tree.waits().map(|wait| wait.target().clone()).collect();
    let mut out = Vec::new();
    for resource in tree.resources() {
        for attribute in resource.located_attributes() {
            let (key, value) = attribute.detail();
            value.visit_resource_refs(&mut |path| {
                check_ref(
                    path,
                    key,
                    attribute.location(),
                    &by_binding,
                    &synchronized,
                    schemas,
                    &mut out,
                )
            });
        }
    }
    out
}

fn check_ref<B: BindingAddress>(
    path: &AccessPath<B>,
    key: &str,
    location: &DeclarationLocation,
    bindings: &HashMap<B, (&String, &String)>,
    synchronized: &HashSet<B>,
    schemas: &SchemaRegistry,
    out: &mut Vec<Located<DeferredPopulateDiagnostic>>,
) {
    let target = path.binding_address();
    if synchronized.contains(target) {
        return;
    }
    let Some((provider, resource_type)) = bindings.get(target) else {
        return;
    };
    let Some(schema) = schemas.get(provider, resource_type, SchemaKind::Resource) else {
        return;
    };
    let Some(attribute) = schema.effective_attribute_by_authored_name(path.attribute()) else {
        return;
    };
    if path_traverses_deferred(attribute, path.segments()) {
        out.push(Located::new(location.clone(), DeferredPopulateDiagnostic {
            message: format!("attribute `{key}` references `{}`, which is populated asynchronously by the provider after Create. Add a `wait {target} {{ until = ... }}` block in this directory before downstream resources read this attribute.", path.to_dot_string()),
            attribute_key: key.to_string(), unresolved_path: path.to_dot_string(), target_binding: target.to_string(),
        }));
    }
}

/// Walk `(top_attr, segments)` over the schema graph; return true if
/// any hop traverses a `deferred_populate=true` attribute or struct
/// field. The top-level attribute itself counts (an
/// `AttributeSchema.deferred_populate=true` value is deferred-bound
/// for *every* downstream chained access on it, even with no
/// segments).
fn path_traverses_deferred(top_attr: EffectiveAttribute<'_>, segments: &[PathSegment]) -> bool {
    if top_attr.is_deferred_populate() {
        return true;
    }
    let mut current_type = top_attr.value_type();
    for segment in segments {
        // Peel List wrappers introduced by `[idx]` segments. A
        // `Subscript::Int` traverses `List<T>` to `T`; a
        // `Subscript::Str` traverses `Map<_, V>` to `V`. Either way
        // the resulting `current_type` is the element type — no
        // deferred flag lives on List/Map themselves (they have no
        // such field on their builders).
        match segment {
            PathSegment::Subscript { .. } => {
                let next = current_type
                    .list_element()
                    .or_else(|| current_type.map_value());
                let Some(next) = next else {
                    return false;
                };
                current_type = next;
            }
            PathSegment::Field { name } => {
                match current_type.shape() {
                    Shape::UnloadedSchemaType(_) => return false,
                    Shape::Struct { .. } | Shape::Union => {}
                    Shape::Reference { .. }
                    | Shape::String { .. }
                    | Shape::Int { .. }
                    | Shape::Float { .. }
                    | Shape::Bool
                    | Shape::Duration
                    | Shape::Enum { .. }
                    | Shape::List { .. }
                    | Shape::Map { .. } => return false,
                }
                // Each path hop receives a fresh bounded transparent-shape
                // search through `field_candidates`; earlier segments cannot
                // exhaust the budget and turn a later deferred field into a
                // silent `None`.
                let Some(field) = current_type
                    .field_candidates()
                    .into_iter()
                    .find(|field| field.name() == name || field.block_name() == Some(name))
                else {
                    return false;
                };
                if field.is_deferred_populate() {
                    return true;
                }
                current_type = field.value_type();
            }
        }
        // After traversing a List<Struct> via [idx], the next field
        // hop looks up against the inner Struct directly because
        // `current_type` was just rebound to the list's element type.
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;
    use crate::parser::{ParsedFile, ProviderContext, parse};
    use crate::schema::SchemaRegistry;

    fn cert_schema_with_dvo_inner_field_deferred<R: crate::schema::PatternForm>()
    -> crate::schema::PatternResourceSchema<R> {
        PatternResourceSchema::new("acm.Certificate").attribute(
            PatternAttributeSchema::provider_populated(
                "domain_validation_options",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "DomainValidationOption".to_string(),
                    vec![
                        PatternStructField::provider_populated(
                            "domain_name",
                            PatternAttributeType::string(),
                        )
                        .build(),
                        PatternStructField::provider_populated(
                            "resource_record_value",
                            PatternAttributeType::string(),
                        )
                        .deferred_populate()
                        .build(),
                    ],
                )),
            )
            .build(),
        )
    }

    #[test]
    fn deep_deferred_path_does_not_exhaust_a_shared_shape_budget() {
        let mut attr_type = PatternAttributeType::struct_(
            "Leaf",
            vec![
                PatternStructField::optional("deferred", PatternAttributeType::string())
                    .deferred_populate()
                    .build(),
            ],
        );
        let mut segments = vec![PathSegment::Field {
            name: "deferred".to_string(),
        }];
        for index in (0..80).rev() {
            let name = format!("field_{index}");
            attr_type = PatternAttributeType::struct_(
                format!("Level{index}"),
                vec![PatternStructField::required(name.clone(), attr_type).build()],
            );
            segments.insert(0, PathSegment::Field { name });
        }
        let schema = PatternResourceSchema::new("test.Deep")
            .attribute(PatternAttributeSchema::provider_populated("output", attr_type).build())
            .validate()
            .expect("deep schema must be valid");
        let output = schema
            .effective_attribute("output")
            .expect("output attribute must exist");

        assert!(path_traverses_deferred(output, &segments));
    }

    fn rrset_schema<R: crate::schema::PatternForm>() -> crate::schema::PatternResourceSchema<R> {
        PatternResourceSchema::new("route53.RecordSet")
            .attribute(
                PatternAttributeSchema::optional(
                    "resource_records",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
    }

    fn fixture(source: &str) -> ParsedFile {
        parse(
            source,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("authored diagnostic fixture")
    }

    fn parsed_with_unsynchronized_chained_ref() -> ParsedFile {
        fixture(
            "let cert = aws.acm.Certificate {}\nlet record = aws.route53.RecordSet { resource_records = [cert.domain_validation_options[0].resource_record_value] }\n",
        )
    }

    fn registry_for_acm() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.insert(
            "aws",
            (cert_schema_with_dvo_inner_field_deferred())
                .validate()
                .expect("schema fixture must be valid"),
        );
        r.insert(
            "aws",
            (rrset_schema())
                .validate()
                .expect("schema fixture must be valid"),
        );
        r
    }

    #[test]
    fn list_wrapped_chained_ref_to_deferred_inner_field_is_flagged() {
        let parsed = parsed_with_unsynchronized_chained_ref();
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&parsed).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
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
    fn parser_side_deferred_resource_body_is_checked() {
        let parsed = fixture(
            "arguments { names: list(String) }\nlet cert = aws.acm.Certificate {}\nlet records = for name in names { aws.route53.RecordSet { name = name resource_records = [cert.domain_validation_options[0].resource_record_value] } }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&parsed).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert_eq!(diags[0].target_binding, "cert");
    }

    #[test]
    fn wait_block_on_target_binding_satisfies_the_rule() {
        let parsed = fixture(
            "let cert = aws.acm.Certificate {}\nlet record = aws.route53.RecordSet { resource_records = [cert.domain_validation_options[0].resource_record_value] }\nlet ready = wait cert { until = cert.status == 'ISSUED' }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&parsed).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn chained_ref_to_non_deferred_inner_field_is_not_flagged() {
        let p = fixture(
            "let cert = aws.acm.Certificate {}\nlet record = aws.route53.RecordSet { name = cert.domain_validation_options[0].domain_name }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&p).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn unknown_target_binding_does_not_emit_double_diagnostic() {
        let p = fixture(
            "let record = aws.route53.RecordSet { resource_records = [typo_cert.domain_validation_options[0].resource_record_value] }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&p).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn top_level_attribute_marked_deferred_flags_any_segment_chain() {
        // Mirror RDS DBInstance.endpoint shape: the whole attribute
        // is undefined immediately post-Create.
        let schema = PatternResourceSchema::new("rds.DBInstance").attribute(
            PatternAttributeSchema::provider_populated(
                "endpoint",
                PatternAttributeType::struct_(
                    "Endpoint".to_string(),
                    vec![
                        PatternStructField::provider_populated(
                            "address",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ],
                ),
            )
            .deferred_populate()
            .build(),
        );
        let consumer = PatternResourceSchema::new("ec2.Instance").attribute(
            PatternAttributeSchema::optional("user_data", PatternAttributeType::string()).build(),
        );
        let mut r = SchemaRegistry::new();
        r.insert(
            "aws",
            schema.validate().expect("schema fixture must be valid"),
        );
        r.insert(
            "aws",
            consumer.validate().expect("schema fixture must be valid"),
        );

        let p = fixture(
            "let db = aws.rds.DBInstance {}\nlet i = aws.ec2.Instance { user_data = db.endpoint.address }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&p).expect("parsed deferred-populate provenance"),
            &r,
        );
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert_eq!(diags[0].target_binding, "db");
    }

    #[test]
    fn schema_lookup_miss_does_not_panic_or_emit() {
        let p = fixture(
            "let x = aws.unknown.thing {}\nlet record = aws.route53.RecordSet { resource_records = [x.domain_validation_options[0].resource_record_value] }\n",
        );
        let diags = validate_deferred_populate_refs(
            &AnalysisTree::try_from_file(&p).expect("parsed deferred-populate provenance"),
            &registry_for_acm(),
        );
        // Schema for "unknown.thing" missing means nothing to check;
        // crucially no panic.
        assert!(diags.is_empty(), "got: {diags:?}");
    }
}
