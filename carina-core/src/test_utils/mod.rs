//! Fixtures available to downstream tests with the `test-utils` feature.

pub mod configuration;
#[cfg(not(target_arch = "wasm32"))]
pub mod loopback;

// Host boundary fixtures must not become guest display authority through
// Cargo feature unification in a provider's wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod provider_text;

/// Adapt a real parser invariant for downstream terminal-rendering tests.
pub fn resource_analysis_finding_for_test(
    error: crate::parser::AnalysisTreeError,
) -> crate::validation::ResourceAdmissionFinding {
    crate::validation::ResourceAdmissionFinding::analysis(error)
}

pub fn load_canonical_buffer_configuration<'a, E, D, B: crate::resource::BindingAddress>(
    parsed: &crate::parser::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &'a crate::schema::SchemaRegistry,
    context: &crate::parser::ProviderContext,
) -> crate::identifier::PrefixCanonicalized<'a, crate::parser::retained::JudgedFile<'a, E, D, B>>
where
    E: Clone + crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B>,
    D: Clone
        + crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    crate::identifier::canonicalize_file_prefixes(
        parsed.clone(),
        schemas,
        crate::identifier::SuffixSource::Preview,
    )
    .judge(context)
}

/// Observe authored coverage using the same witnessed request as production.
pub fn admit_resource_with_coverage_for_test<E, D, B: crate::resource::BindingAddress>(
    request: crate::validation::admission::attributes::ResourceAdmissionRequest<'_, '_, E, D, B>,
    coverage: &mut crate::validation::admission::WalkCoverage,
) -> crate::validation::admission::attributes::ResourceAdmissionOutcome<B>
where
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
{
    crate::validation::admission::attributes::resource_attribute_admissions(request, coverage)
}

/// Run resource value fixtures through production validation admission.
pub fn admit_resource_attribute_report<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
    is_string_literal: &dyn Fn(&str) -> bool,
    custom_types: &crate::parser::ProviderCustomTypes,
) -> crate::schema::ResourceValueValidation {
    let catalog = crate::parser::ProviderContext {
        custom_types: custom_types.clone(),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let id = crate::resource::ResourceId::pending_with_provider(
        schema.owner(),
        schema.schema().parts().resource_type,
        None,
    );
    let attributes = match crate::identifier::canonicalize_prefix_values(
        schema,
        &id,
        attributes.clone(),
        crate::identifier::SuffixSource::Preview,
    ) {
        Ok(values) => values,
        Err(errors) => {
            return crate::schema::ResourceValueValidation::from_errors(
                errors
                    .into_iter()
                    .map(|error| crate::schema::TypeError::AttributePrefix(Box::new(error)))
                    .collect(),
            );
        }
    };
    let quoted = attributes
        .keys()
        .filter(|name| is_string_literal(name))
        .cloned()
        .collect();
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        crate::validation::admission::attributes::admit_value_only_attributes::<
            crate::validation::admission::kind::ResourceAttribute,
            P,
            B,
        >(schema, &id, &attributes, &quoted, session)
    })
}

/// Validate a resource's authored attributes through production admission.
///
/// Available with the `test-utils` feature for provider test dev-dependencies.
/// The schema walk checks required attributes and field groups; each authored
/// value enters `admit()`, including local and provider custom-type validation
/// from `custom_types`. Use the schema and custom types from the same catalog.
///
/// A VPC gateway attachment must specify exactly one gateway. Supplying both
/// is rejected even though each attribute is individually a valid String:
///
/// ```
/// use indexmap::IndexMap;
/// use carina_core::{
///     parser::ProviderCustomTypes,
///     resource::{ConcreteValue, Value},
///     schema::{AttributeSchema, AttributeType, FieldGroup, ResourceSchema, SchemaKind, SchemaRegistry},
///     test_utils::admit_resource_attributes,
/// };
///
/// let schema = ResourceSchema::new("ec2.vpc_gateway_attachment")
///     .attribute(AttributeSchema::required("vpc_id", AttributeType::string()).build())
///     .attribute(AttributeSchema::optional("internet_gateway_id", AttributeType::string()).build())
///     .attribute(AttributeSchema::optional("vpn_gateway_id", AttributeType::string()).build())
///     .field_group(FieldGroup::exactly_one(["internet_gateway_id", "vpn_gateway_id"]).unwrap())
///     .validate().unwrap();
/// let mut schemas = SchemaRegistry::new();
/// schemas.insert("awscc", schema);
/// let schema = schemas.get("awscc", "ec2.vpc_gateway_attachment", SchemaKind::Resource).unwrap();
/// let values: IndexMap<String, Value> = IndexMap::from([
///     ("vpc_id".into(), Value::Concrete(ConcreteValue::String("vpc-12345678".into()))),
///     ("internet_gateway_id".into(), Value::Concrete(ConcreteValue::String("igw-12345678".into()))),
///     ("vpn_gateway_id".into(), Value::Concrete(ConcreteValue::String("vgw-12345678".into()))),
/// ]);
/// let errors = admit_resource_attributes(
///     schema, &values, &|_| true, &ProviderCustomTypes::default(),
/// ).unwrap_err();
/// assert_eq!(errors.len(), 1);
/// assert!(errors[0].to_string().contains(
///     "exactly one of [internet_gateway_id, vpn_gateway_id] must be set"
/// ));
/// ```
pub fn admit_resource_attributes<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
    is_string_literal: &dyn Fn(&str) -> bool,
    custom_types: &crate::parser::ProviderCustomTypes,
) -> Result<(), Vec<crate::schema::TypeError>> {
    let report =
        admit_resource_attribute_report(schema, attributes, is_string_literal, custom_types);
    let errors: Vec<_> = report.errors().cloned().collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A registered, empty schema for boundary fixtures that exercise wire checks
/// rather than provider declarations. Its attributes remain unschematized.
pub fn preparation_schemas(
    id: &crate::resource::ResourceId,
    kind: crate::schema::SchemaKind,
) -> crate::schema::SchemaRegistry {
    let mut registry = crate::schema::SchemaRegistry::new();
    let schema = crate::schema::ResourceSchema::new(&id.resource_type);
    let schema = match kind {
        crate::schema::SchemaKind::Resource => schema,
        crate::schema::SchemaKind::DataSource => schema.as_data_source(),
    };
    registry.insert(&id.provider, schema.validate().unwrap());
    registry
}

/// Derive unconstrained fixture shapes through the shared structural walker.
/// This builds declarations for transport/scheduler fixtures; it does not admit
/// values, and all declared production constraints remain in the real schemas.
pub fn fixture_value_type<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
    value: &crate::resource::Value<P, B>,
) -> crate::schema::PatternAttributeType {
    if let crate::resource::Value::Concrete(crate::resource::ConcreteValue::CanonicalEnum(value)) =
        value
    {
        return crate::schema::PatternAttributeType::enum_(
            value.identity().clone(),
            Some(vec![value.api_value().into()]),
            vec![],
            None,
            None,
        );
    }
    use crate::resource::{BindingAddress, ValueKind};
    use crate::value_walk::{
        self, Algebra, At, Children, ExprChildren, Inspect, ScopedLeaf, ScopedScalar, SecretAt,
        SecretChild, SecretStep, Step,
    };
    #[derive(Default)]
    struct Shape {
        keys: Vec<String>,
    }
    impl Shape {
        fn key(&mut self, at: &At<'_>) -> usize {
            let Some(crate::schema::LeafPathSegment::MapKey(key)) =
                at.public_path().and_then(|path| path.segments().last())
            else {
                return usize::MAX;
            };
            let key = key.expose_plaintext();
            self.keys
                .iter()
                .position(|name| name == key)
                .unwrap_or_else(|| {
                    self.keys.push(key.into());
                    self.keys.len() - 1
                })
        }
    }
    fn container(parts: Vec<Vec<usize>>, tag: usize, key: usize) -> Vec<usize> {
        let mut unique = Vec::new();
        for part in parts {
            if !unique.contains(&part) {
                unique.push(part);
            }
        }
        let count = unique.len();
        let mut tokens = unique.into_iter().flatten().collect::<Vec<_>>();
        tokens.extend([count, tag, key]);
        tokens
    }
    impl Algebra<Inspect> for Shape {
        type Output = Vec<usize>;
        fn scalar<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &At<'_>,
            value: ScopedScalar<'_, Inspect, P, B>,
        ) -> Step<Inspect, Self::Output, P, B> {
            use value_walk::ScalarKind;
            let tag = match value.kind() {
                ScalarKind::Bool => 1,
                ScalarKind::Int => 2,
                ScalarKind::Float => 3,
                ScalarKind::Duration => 4,
                ScalarKind::StringList => 7,
                _ => 0,
            };
            value.finish(vec![tag, self.key(at)])
        }
        fn list<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &At<'_>,
            items: Children<'_, Inspect, P, B>,
        ) -> Step<Inspect, Self::Output, P, B> {
            let key = self.key(at);
            items.inspect(self, |parts| container(parts, 5, key))
        }
        fn map<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &At<'_>,
            items: Children<'_, Inspect, P, B>,
        ) -> Step<Inspect, Self::Output, P, B> {
            let key = self.key(at);
            items.inspect(self, |parts| {
                let tag = if parts.iter().all(|part| part.last() != Some(&usize::MAX)) {
                    8
                } else {
                    6
                };
                container(parts, tag, key)
            })
        }
        fn expression<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &At<'_>,
            value: ExprChildren<'_, Inspect, P, B>,
        ) -> Step<Inspect, Self::Output, P, B> {
            let key = self.key(at);
            value.inspect(self, |_| vec![0, key])
        }
        fn placeholder<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &At<'_>,
            value: ScopedLeaf<'_, Inspect, P, B>,
        ) -> Step<Inspect, Self::Output, P, B> {
            value.finish(vec![0, self.key(at)])
        }
        fn secret<P: ValueKind, B: BindingAddress>(
            &mut self,
            at: &SecretAt<'_>,
            value: SecretChild<'_, Inspect, P, B>,
        ) -> SecretStep<Inspect, Self::Output, P, B> {
            let key = self.key(at.at());
            value.inspect(self).map(|mut tokens| {
                *tokens.last_mut().unwrap() = key;
                tokens
            })
        }
    }
    fn declaration(
        tokens: &mut Vec<usize>,
        keys: &[String],
    ) -> (Option<String>, crate::schema::PatternAttributeType) {
        use crate::schema::{PatternAttributeType as Ty, PatternStructField};
        let key = tokens.pop().unwrap();
        let ty = match tokens.pop().unwrap() {
            0 => Ty::string(),
            1 => Ty::bool(),
            2 => Ty::int(),
            3 => Ty::float(),
            4 => Ty::duration(),
            7 => Ty::list(Ty::string()),
            8 => {
                let count = tokens.pop().unwrap();
                let mut fields = (0..count)
                    .map(|_| {
                        let (key, ty) = declaration(tokens, keys);
                        PatternStructField::optional(key.expect("public fixture field"), ty).build()
                    })
                    .collect::<Vec<_>>();
                fields.reverse();
                Ty::struct_("FixtureObject", fields)
            }
            tag @ (5 | 6) => {
                let count = tokens.pop().unwrap();
                let mut types = (0..count)
                    .map(|_| declaration(tokens, keys).1)
                    .collect::<Vec<_>>();
                // An anonymous protected map must preserve its wire values.
                // Avoid the two-arm String/List canonicalization shorthand.
                if tag == 6 && types.len() > 1 {
                    types.extend([Ty::bool(), Ty::int()]);
                }
                let inner = match types.len() {
                    0 => Ty::string(),
                    1 => types.pop().unwrap(),
                    _ => Ty::union(types),
                };
                if tag == 5 {
                    Ty::list(inner)
                } else {
                    Ty::map(inner)
                }
            }
            _ => unreachable!("fixture shape tag"),
        };
        ((key != usize::MAX).then(|| keys[key].clone()), ty)
    }
    let mut shape = Shape::default();
    let mut tokens = value_walk::inspect(value_walk::WalkRoot::syntactic(), value, &mut shape)
        .expect("fixture value walk")
        .into_output();
    declaration(&mut tokens, &shape.keys).1
}

/// Register the fields supplied by a transport fixture before normalization.
pub fn preparation_schemas_with_attributes<
    P: crate::resource::ValueKind,
    B: crate::resource::BindingAddress,
>(
    id: &crate::resource::ResourceId,
    kind: crate::schema::SchemaKind,
    attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
) -> crate::schema::SchemaRegistry {
    let mut schemas = preparation_schemas(id, kind);
    complete_fixture_schema(&mut schemas, id, kind, attributes);
    schemas
}

/// Preserve existing constraints and add only the fixture's undeclared fields.
pub fn complete_fixture_schema<
    P: crate::resource::ValueKind,
    B: crate::resource::BindingAddress,
>(
    schemas: &mut crate::schema::SchemaRegistry,
    id: &crate::resource::ResourceId,
    kind: crate::schema::SchemaKind,
    attributes: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
) {
    let Some(schema) = schemas.get(&id.provider, &id.resource_type, kind) else {
        return;
    };
    let completed = schema.schema().with_fixture_attributes(attributes);
    schemas.insert(&id.provider, completed);
}

/// Load fixture declaration authority from the same schema snapshot used by
/// preparation, including qualified enum/custom-type identities.
#[cfg(not(target_arch = "wasm32"))]
pub fn loaded_fixture_snapshot(
    schemas: crate::schema::SchemaRegistry,
) -> crate::provider::LoadedProviderSnapshot {
    struct Factory {
        name: String,
        schemas: Vec<crate::schema::ValidatedResourceSchema>,
        witness: crate::schema::ValidatedProviderSchemaWitness,
    }
    impl crate::provider::ProviderFactory for Factory {
        fn name(&self) -> &str {
            &self.name
        }
        fn display_name(&self) -> &str {
            "Schema fixture"
        }
        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            self.schemas.clone()
        }
        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            self.witness.clone()
        }

        fn validate_config(
            &self,
            _: &crate::provider::CanonicalProviderConfigAttributes,
            _: &crate::provider::ProviderValidationInputs,
        ) -> crate::provider::ProviderResult<()> {
            Ok(())
        }
        fn create_provider(
            &self,
            _: &crate::provider::ProviderSession,
            _: crate::provider::InitializeOutcomeMint,
        ) -> crate::provider::BoxFuture<
            '_,
            crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
        > {
            Box::pin(async {
                Err(crate::provider::ProviderError::internal(
                    "schema fixture has no provider instance",
                ))
            })
        }
    }
    let mut by_owner = std::collections::BTreeMap::<String, Vec<_>>::new();
    for (owner, _, _, schema) in schemas.iter() {
        by_owner
            .entry(owner.into())
            .or_default()
            .push(schema.clone());
    }
    let factories: Vec<Box<dyn crate::provider::ProviderFactory>> = by_owner
        .into_iter()
        .map(|(name, declarations)| {
            let first = &declarations[0];
            let bound = schemas
                .get(
                    &name,
                    first.as_schema().resource_type(),
                    first.as_schema().kind(),
                )
                .unwrap();
            let witness = crate::schema::ValidatedProviderSchemaWitness::new(
                declarations.clone(),
                Default::default(),
                bound.reference_forms().clone(),
                bound.assignability_edges().to_vec(),
            )
            .expect("fixture schema authority");
            Box::new(Factory {
                name,
                schemas: declarations,
                witness,
            }) as Box<dyn crate::provider::ProviderFactory>
        })
        .collect();
    crate::provider::loaded_provider_snapshot_from_factories(
        std::sync::Arc::new(factories),
        schemas,
        crate::EnvironmentSnapshot::empty(),
    )
}

/// Build known strict provider inputs for execution fixtures through the same
/// invariant constructor as live and decoded configurations.
pub fn resolved_provider_configs(
    configs: &[crate::parser::ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
) -> crate::parser::ResolvedProviderConfigs {
    crate::parser::ResolvedProviderConfigs::from_test_configs(configs.to_vec())
        .expect("execution fixture provider configs must be known")
}

/// An actual conversion-free Judge result for programmatic admission fixtures.
pub fn no_conversion_proof() -> crate::parser::retained::NoConversion {
    static PROOF: std::sync::OnceLock<crate::parser::retained::NoConversion> =
        std::sync::OnceLock::new();
    *PROOF.get_or_init(|| {
        let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed =
            crate::parser::parse_file("let proof = 0", std::path::Path::new("proof.crn"), &catalog)
                .expect("plain proof fixture");
        let schemas = crate::schema::SchemaRegistry::new();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        configuration
            .judged()
            .report()
            .binding(&crate::resource::RelativeBindingName::new("proof"))
            .expect("proof judgement")
            .no_conversion()
            .expect("no conversion")
    })
}

/// Final test renderer after the consuming presentation boundary. Production
/// core APIs cannot stringify the mixed findings aggregate.
pub fn render_presented_for_test<E: crate::validation::ValidationClientError>(
    findings: &crate::validation::PresentedFindings<E>,
) -> String {
    findings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
#[derive(Debug)]
pub struct LegacyPresentedFindings(pub(crate) crate::validation::PresentedFindings, String);
impl LegacyPresentedFindings {
    pub fn lines(&self) -> std::str::Lines<'_> {
        self.1.lines()
    }
}
impl std::ops::Deref for LegacyPresentedFindings {
    type Target = crate::validation::PresentedFindings;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::fmt::Display for LegacyPresentedFindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}
impl std::error::Error for LegacyPresentedFindings {}
#[derive(Debug, thiserror::Error)]
pub enum LegacyValidationError {
    #[error(transparent)]
    AnalysisInvariant(#[from] crate::parser::AnalysisTreeError),
    #[error(transparent)]
    Validation(LegacyPresentedFindings),
}
fn legacy_findings_result(
    result: Result<crate::validation::PresentedFindings, crate::parser::AnalysisTreeError>,
) -> Result<(), LegacyValidationError> {
    let findings = result?;
    let rendered = render_presented_for_test(&findings);
    if findings.is_empty() {
        Ok(())
    } else {
        Err(LegacyValidationError::Validation(LegacyPresentedFindings(
            findings, rendered,
        )))
    }
}
pub fn legacy_validate_resources<
    E: Clone,
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B> + Clone,
>(
    parsed: &crate::parser::File<E, P, T, D, B>,
    registry: &crate::schema::SchemaRegistry,
    known: &std::collections::HashSet<String>,
    context: &crate::parser::ProviderContext,
    diagnostics: &mut dyn crate::validation::deferred_types::ProviderTypeCheckSink,
) -> Result<(), String> {
    crate::validation::validate_resources(parsed, registry, known, context, diagnostics)
        .map_err(|findings| render_presented_for_test(&findings))
}
pub fn legacy_validate_resource_ref_types<
    E: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B> + Clone,
    B: crate::resource::BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        > + Clone,
>(
    judged: &crate::identifier::PrefixCanonicalized<
        '_,
        crate::parser::retained::JudgedFile<'_, E, D, B>,
    >,
    _: &std::collections::HashSet<String>,
    bindings: &crate::binding_index::BindingIndex<'_, B>,
    diagnostics: &mut dyn crate::validation::deferred_types::ProviderTypeCheckSink,
) -> Result<(), LegacyValidationError> {
    legacy_findings_result(crate::validation::resource_reference_findings(
        judged,
        bindings,
        diagnostics,
    ))
}
pub fn legacy_validate_resource_admissions<
    E: crate::parser::ExportParamLike<crate::resource::ParserPlaceholder, B> + Clone,
    B: crate::resource::BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        > + Clone,
>(
    judged: &crate::identifier::PrefixCanonicalized<
        '_,
        crate::parser::retained::JudgedFile<'_, E, D, B>,
    >,
    bindings: &crate::binding_index::BindingIndex<'_, B>,
    known: &std::collections::HashSet<String>,
    diagnostics: &mut dyn crate::validation::deferred_types::ProviderTypeCheckSink,
) -> Result<(), LegacyValidationError> {
    legacy_findings_result(crate::validation::resource_admission_findings(
        judged,
        bindings,
        known,
        diagnostics,
    ))
}

/// Adapt an ordinary-only fixture to the source-proof API. Conversion cases
/// must exercise the production run and presentation driver instead.
pub fn ordinary_sink_findings_for_test<T>(findings: crate::validation::SinkFindings<T>) -> T {
    findings.expect_ordinary_for_test()
}

pub fn ordinary_sink_result_for_test<T, E>(
    result: Result<T, crate::parser::retained::SinkRejection<E>>,
) -> Result<T, E> {
    match result {
        Ok(value) => Ok(value),
        Err(crate::parser::retained::SinkRejection::Ordinary(error)) => Err(error),
        Err(crate::parser::retained::SinkRejection::Conversion(_)) => {
            panic!("conversion fixture must use the production validation driver")
        }
    }
}

/// Start an isolated collector for fixtures that do not own a parsed document.
/// Production clients start from a directory, Judge report, or parse failure.
pub fn conversion_validation<E: crate::validation::ValidationClientError>()
-> crate::validation::ConversionValidation<E> {
    crate::validation::ConversionValidation::for_test()
}

/// Record a synchronous fixture with fixed secret-hash entropy on this thread.
/// Restores the prior source on return or panic. Ordinary hashing uses OsRng.
pub fn with_secret_hash_salt_for_test<T>(salt: [u8; 16], record: impl FnOnce() -> T) -> T {
    crate::resource::secret_hash_test_entropy::with_salt(salt, record)
}
