//! The file that crossed prefix canonicalization is admission's authority.
use super::{
    AttributePrefixError, ResourcePrefixSource, generate_random_suffix, prefix_companion_attribute,
};
use crate::parser::retained::JudgedFile;
use crate::parser::{
    AnalysisResourceRef, AnalysisTree, AnalysisTreeError, DeferredForPhaseTraversal,
    ExportParamLike, File, ProviderContext,
};
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, ModuleSource, ParserPlaceholder, PlaceholderKind,
    Resource, ResourceId, Value, ValueString,
};
use crate::schema::{OwnerBoundResourceSchema, SchemaKind, SchemaRegistry};
use crate::validation::admission::{DeclarationIdentity, attributes::declaration_identity};
use indexmap::IndexMap;
use std::{ops::Deref, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuffixSource {
    Random,
    Preview,
}
impl SuffixSource {
    fn suffix(self) -> String {
        match self {
            Self::Random => generate_random_suffix(),
            Self::Preview => "00000000".to_owned(),
        }
    }
}

/// An immutable file and the exact registered catalog that canonicalized it.
/// Only the canonicalizer and its concrete phase transitions can produce this.
/// A raw clone's tree cannot authorize admission:
/// ```compile_fail,E0308
/// use carina_core::{parser::{AnalysisTree, ParsedFile}, validation::admission::attributes::ResourceAttributeInputs};
/// fn bypass(file: &ParsedFile) {
///     let raw = file.clone();
///     let tree = AnalysisTree::try_from_file(&raw).unwrap();
///     let _ = ResourceAttributeInputs::new(tree.resources().next().unwrap());
/// }
/// ```
#[derive(Clone)]
pub struct PrefixCanonicalized<F> {
    file: F,
    schemas: Arc<SchemaRegistry>,
    errors: Vec<AttributePrefixError>,
    rejected: Vec<usize>,
}
impl<F: std::fmt::Debug> std::fmt::Debug for PrefixCanonicalized<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixCanonicalized")
            .field("file", &self.file)
            .field("errors", &self.errors)
            .finish_non_exhaustive()
    }
}
impl<F> Deref for PrefixCanonicalized<F> {
    type Target = F;
    fn deref(&self) -> &F {
        &self.file
    }
}
impl<F> PrefixCanonicalized<F> {
    pub fn prefix_errors(&self) -> &[AttributePrefixError] {
        &self.errors
    }
    pub fn schemas(&self) -> &SchemaRegistry {
        &self.schemas
    }
}

pub fn canonicalize_file_prefixes<
    X,
    P: PlaceholderKind,
    T: PlaceholderKind,
    D,
    B: BindingAddress,
>(
    mut file: File<X, P, T, D, B>,
    schemas: &SchemaRegistry,
    suffix: SuffixSource,
) -> PrefixCanonicalized<File<X, P, T, D, B>> {
    let schemas = Arc::new(schemas.clone());
    let (errors, rejected) = resolve_resources(&mut file.resources, &schemas, suffix);
    PrefixCanonicalized {
        file,
        schemas,
        errors,
        rejected,
    }
}

impl<X, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>
    PrefixCanonicalized<File<X, P, T, D, B>>
{
    pub fn canonicalize_values(
        &mut self,
    ) -> Result<
        crate::value::CanonicalizedResources<'_, ModuleSource, P, B>,
        crate::value::EnumCanonicalizationError,
    > {
        crate::value::canonicalize_resources_with_schemas(&mut self.file.resources, &self.schemas)
    }
}
impl<X, D, B: BindingAddress>
    PrefixCanonicalized<File<X, ParserPlaceholder, ParserPlaceholder, D, B>>
where
    X: Clone + ExportParamLike<ParserPlaceholder, B>,
    D: Clone + DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn judge<'s>(
        &self,
        schemas: &'s SchemaRegistry,
        context: &ProviderContext,
    ) -> PrefixCanonicalized<JudgedFile<'s, X, D, B>> {
        PrefixCanonicalized {
            file: JudgedFile::new(self.file.clone(), schemas, context),
            schemas: self.schemas.clone(),
            errors: self.errors.clone(),
            rejected: self.rejected.clone(),
        }
    }
}

/// Values whose authored keys crossed the same per-owner canonicalizer.
#[derive(Clone)]
pub struct CanonicalAttributeValues<P: PlaceholderKind, B: BindingAddress> {
    values: IndexMap<String, Value<P, B>>,
    pub(crate) declaration: DeclarationIdentity,
}
impl<P: PlaceholderKind, B: BindingAddress> Deref for CanonicalAttributeValues<P, B> {
    type Target = IndexMap<String, Value<P, B>>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

/// A resource produced by a canonical file's own analysis tree.
/// Its schema and file cannot be replaced independently.
pub struct CanonicalResource<'f, X, D, B: BindingAddress>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resource: AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>,
    file: &'f File<X, ParserPlaceholder, ParserPlaceholder, D, B>,
    schemas: &'f SchemaRegistry,
    schema: OwnerBoundResourceSchema<'f>,
}
impl<'f, X, D, B: BindingAddress> Deref for CanonicalResource<'f, X, D, B>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    type Target = AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>;
    fn deref(&self) -> &Self::Target {
        &self.resource
    }
}
impl<'f, X, D, B: BindingAddress> CanonicalResource<'f, X, D, B>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    /// Syntax-only guidance reads the retained source, never a resolved value.
    pub fn authored_attribute_has_function_call(
        &self,
        name: &str,
    ) -> Result<bool, crate::parser::retained::MissingEvidence> {
        Ok(self
            .resource
            .authored_slot(name)?
            .cursor()?
            .contains_function_call())
    }
    pub fn module_source(&self) -> Option<&'f ModuleSource> {
        self.resource.module_source()
    }
    pub fn resource_schema(&self) -> OwnerBoundResourceSchema<'f> {
        self.schema
    }
    pub fn file(&self) -> &'f File<X, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file
    }
    pub fn schemas(&self) -> &'f SchemaRegistry {
        self.schemas
    }
    pub fn report(&self) -> &'f crate::parser::retained::ExpressionReport<B> {
        self.file.expression_report()
    }
    pub fn values(&self) -> CanonicalAttributeValues<ParserPlaceholder, B> {
        CanonicalAttributeValues {
            values: self.resource.attributes().into_owned(),
            declaration: declaration_identity(self.schema),
        }
    }
    /// Caller projections stay open for #4400; their keys must still be resolved.
    pub fn with_values(
        &self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) -> Result<CanonicalAttributeValues<ParserPlaceholder, B>, Vec<AttributePrefixError>> {
        canonicalize_prefix_values(
            self.schema,
            self.id(),
            values.clone(),
            SuffixSource::Preview,
        )
    }
}

enum CanonicalItem<'f, X, D, B: BindingAddress>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    Bound(CanonicalResource<'f, X, D, B>),
    Rejected(AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>),
    Unavailable(AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>),
}

pub struct CanonicalAnalysisTree<'f, X, D, B: BindingAddress>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resources: Vec<CanonicalItem<'f, X, D, B>>,
    top_level_resource_count: usize,
}
impl<'f, X, D, B: BindingAddress> CanonicalAnalysisTree<'f, X, D, B>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn resources(&self) -> impl Iterator<Item = &CanonicalResource<'f, X, D, B>> {
        self.resources.iter().filter_map(|item| match item {
            CanonicalItem::Bound(resource) => Some(resource),
            CanonicalItem::Rejected(_resource) | CanonicalItem::Unavailable(_resource) => None,
        })
    }
    /// The materialized managed members of File::resources, matching the CLI.
    pub fn managed_resources(&self) -> impl Iterator<Item = &CanonicalResource<'f, X, D, B>> {
        self.resources
            .iter()
            .take(self.top_level_resource_count)
            .filter_map(|item| match item {
                CanonicalItem::Bound(resource)
                    if resource.kind() == crate::parser::AnalysisItemKind::Resource =>
                {
                    Some(resource)
                }
                CanonicalItem::Bound(_)
                | CanonicalItem::Rejected(_)
                | CanonicalItem::Unavailable(_) => None,
            })
    }
    pub fn project_resources(&self) -> impl Iterator<Item = &CanonicalResource<'f, X, D, B>> {
        self.resources()
            .filter(|resource| resource.is_project_resource())
            .inspect(|_| record_project_resource())
    }
    pub fn project_resource_refs(
        &self,
    ) -> impl Iterator<Item = &AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>>
    {
        self.resources
            .iter()
            .map(|item| match item {
                CanonicalItem::Bound(resource) => &resource.resource,
                CanonicalItem::Rejected(resource) | CanonicalItem::Unavailable(resource) => {
                    resource
                }
            })
            .filter(|resource| resource.is_project_resource())
    }
    pub fn rejected_resources(
        &self,
    ) -> impl Iterator<Item = &AnalysisResourceRef<'f, ParserPlaceholder, ParserPlaceholder, D, B>>
    {
        self.resources.iter().filter_map(|item| match item {
            CanonicalItem::Rejected(resource) => Some(resource),
            CanonicalItem::Bound(_) | CanonicalItem::Unavailable(_) => None,
        })
    }
    pub fn into_project_resources(self) -> impl Iterator<Item = CanonicalResource<'f, X, D, B>> {
        self.resources.into_iter().filter_map(|item| match item {
            CanonicalItem::Bound(resource) if resource.is_project_resource() => {
                record_project_resource();
                Some(resource)
            }
            CanonicalItem::Bound(_resource) => None,
            CanonicalItem::Rejected(_resource) | CanonicalItem::Unavailable(_resource) => None,
        })
    }
}
impl<'s, X, D, B: BindingAddress> PrefixCanonicalized<JudgedFile<'s, X, D, B>>
where
    X: ExportParamLike<ParserPlaceholder, B>,
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn admission_report(&self) -> &crate::parser::retained::ExpressionReport<B> {
        self.file
            .file()
            .parser_binding_metadata
            .syntax()
            .expressions()
            .admission_report()
    }
    pub fn parsed(&self) -> &File<X, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file.file()
    }
    pub fn judged(&self) -> &Self {
        self
    }
    pub fn prefix_failures(
        &self,
    ) -> impl Iterator<
        Item = (
            &Resource<ModuleSource, ParserPlaceholder, B>,
            &AttributePrefixError,
        ),
    > {
        self.rejected
            .iter()
            .zip(&self.errors)
            .map(|(index, error)| (&self.file.file().resources[*index], error))
    }
    pub fn analysis_tree(&self) -> Result<CanonicalAnalysisTree<'_, X, D, B>, AnalysisTreeError> {
        let file = self.file.file();
        let tree = AnalysisTree::try_from_file(file)?;
        let top_level_resource_count = tree.top_level_resources().count();
        let resources = tree
            .into_resources()
            .into_iter()
            .map(|resource| {
                let rejected = self
                    .rejected
                    .iter()
                    .any(|index| std::ptr::eq(&file.resources[*index].id, resource.id()));
                match resource.schema(&self.schemas) {
                    Some(schema) if !rejected => CanonicalItem::Bound(CanonicalResource {
                        resource,
                        file,
                        schemas: &self.schemas,
                        schema,
                    }),
                    Some(_schema) => CanonicalItem::Rejected(resource),
                    None => CanonicalItem::Unavailable(resource),
                }
            })
            .collect();
        Ok(CanonicalAnalysisTree {
            resources,
            top_level_resource_count,
        })
    }
}

struct PrefixRewrite {
    base: String,
    authored: String,
    prefix: ValueString,
}

pub(super) fn resolve_resources<P: PlaceholderKind, B: BindingAddress>(
    resources: &mut [Resource<ModuleSource, P, B>],
    schemas: &SchemaRegistry,
    suffix: SuffixSource,
) -> (Vec<AttributePrefixError>, Vec<usize>) {
    let mut errors = Vec::new();
    let mut rejected = Vec::new();
    for (index, resource) in resources.iter_mut().enumerate() {
        let Some(schema) = schemas.get_for(resource) else {
            continue;
        };
        let (rewrites, mut findings) = rewrite_prefix_values(
            &mut resource.attributes,
            schema,
            &resource.id,
            resource.declaration.location().cloned(),
            suffix,
        );
        if !findings.is_empty() {
            rejected.extend(std::iter::repeat_n(index, findings.len()));
        }
        errors.append(&mut findings);
        for rewrite in rewrites {
            resource.declaration.rename_attribute_path(
                &[crate::parser::AttributePathSegment::Attribute(
                    rewrite.authored,
                )],
                &rewrite.base,
            );
            resource.prefixes.insert(
                rewrite.base.clone(),
                rewrite.prefix.expose_plaintext().to_owned(),
            );
            resource.authored_slots.record_synthetic(
                rewrite.base,
                crate::parser::retained::SyntheticSource::ResourcePrefix(ResourcePrefixSource(())),
            );
        }
    }
    (errors, rejected)
}

pub(crate) fn canonicalize_prefix_values<P: PlaceholderKind, B: BindingAddress>(
    schema: OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    mut values: IndexMap<String, Value<P, B>>,
    suffix: SuffixSource,
) -> Result<CanonicalAttributeValues<P, B>, Vec<AttributePrefixError>> {
    let (_rewrites, errors) = rewrite_prefix_values(&mut values, schema, id, None, suffix);
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(CanonicalAttributeValues {
        values,
        declaration: declaration_identity(schema),
    })
}

fn rewrite_prefix_values<P: PlaceholderKind, B: BindingAddress>(
    values: &mut IndexMap<String, Value<P, B>>,
    bound: OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    location: Option<crate::parser::DeclarationLocation>,
    suffix: SuffixSource,
) -> (Vec<PrefixRewrite>, Vec<AttributePrefixError>) {
    let schema = bound.schema().as_schema();
    if schema.kind != SchemaKind::Resource {
        return (Vec::new(), Vec::new());
    }
    let prefixes = values
        .iter()
        .filter_map(|(key, value)| {
            let base = key.strip_suffix(super::PREFIX_COMPANION_SUFFIX)?;
            let prefix = concrete_prefix(value)?;
            let attribute = schema.attributes.get(base)?;
            crate::validation::is_string_compatible_type(&attribute.attr_type, &schema.defs)
                .then(|| (base.to_owned(), prefix.clone()))
        })
        .collect::<Vec<_>>();
    let mut rewrites = Vec::new();
    let mut errors = Vec::new();
    for (base, prefix) in prefixes {
        let authored = prefix_companion_attribute(&base);
        if prefix.expose_plaintext().is_empty() || values.contains_key(&base) {
            errors.push(AttributePrefixError {
                resource: id.clone(),
                prefix: authored,
                conflict: if prefix.expose_plaintext().is_empty() {
                    None
                } else {
                    Some(base)
                },
                location: location.clone(),
            });
            continue;
        }
        values.shift_remove(&authored);
        let generated = ValueString::derived_from(
            format!("{}{}", prefix.expose_plaintext(), suffix.suffix()),
            [&prefix],
        );
        values.insert(
            base.clone(),
            Value::Concrete(ConcreteValue::String(generated)),
        );
        rewrites.push(PrefixRewrite {
            base,
            authored,
            prefix,
        });
    }
    (rewrites, errors)
}

// Exhaustive on both axes: adding a reference-bearing representation requires
// deciding its eligibility here, rather than silently treating it as a String.
fn concrete_prefix<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> Option<&ValueString> {
    match value {
        Value::Concrete(concrete) => match concrete {
            ConcreteValue::String(prefix) => Some(prefix),
            ConcreteValue::EnumIdentifier(_identifier) => None,
            ConcreteValue::CanonicalEnum(_enumeration) => None,
            ConcreteValue::Int(_integer) => None,
            ConcreteValue::Float(_float) => None,
            ConcreteValue::Bool(_boolean) => None,
            ConcreteValue::Duration(_duration) => None,
            ConcreteValue::List(_list) => None,
            ConcreteValue::StringList(_strings) => None,
            ConcreteValue::Map(_map) => None,
        },
        Value::Deferred(deferred) => match deferred {
            DeferredValue::ModuleIf(_condition) => None,
            DeferredValue::ResourceRef { path: _path } => None,
            DeferredValue::BindingRef { binding: _binding } => None,
            DeferredValue::Coalesce {
                value: _value,
                default: _default,
            } => None,
            DeferredValue::IntegerArithmetic(_arithmetic) => None,
            DeferredValue::Interpolation(_parts) => None,
            DeferredValue::FunctionCall {
                name: _name,
                args: _args,
                source_line: _line,
            } => None,
            DeferredValue::Unavailable {
                name: _name,
                args: _args,
                reason: _reason,
                source_line: _line,
            } => None,
            DeferredValue::ResolvedFunctionCall {
                function: _function,
                functions: _functions,
                args: _args,
                source_line: _line,
            } => None,
            DeferredValue::Access(_access) => None,
            DeferredValue::Secret(_secret) => None,
            DeferredValue::Unknown(_unknown) => None,
            DeferredValue::Placeholder(_placeholder) => None,
        },
    }
}

impl<E: crate::parser::IntoExpandedExportParam<crate::resource::AbsoluteBindingAddress>>
    PrefixCanonicalized<crate::parser::ModuleResolvedFile<E>>
{
    /// Consume the authored witness at the existing parser-placeholder boundary.
    pub fn resolve_binding_uses(
        self,
        schemas: &SchemaRegistry,
        context: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        signatures: &crate::module_resolver::ResolvedModuleSignatures,
    ) -> Result<crate::validation::ResolvedFile<E::Expanded>, crate::validation::ResolveFileError>
    {
        crate::validation::resolve_bare_binding_uses_in_session(
            self.file, schemas, context, signatures,
        )
    }
    pub fn into_expanded(
        self,
        schemas: &SchemaRegistry,
        context: &ProviderContext,
    ) -> Result<crate::parser::ExpandedFile<E::Expanded>, crate::resource::ExpansionError> {
        self.file.into_expanded(schemas, context)
    }
}

fn record_project_resource() {
    #[cfg(any(test, feature = "test-utils"))]
    crate::test_work::record(crate::test_work::Operation::ProjectResource, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    fn schemas() -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
        schemas
    }

    #[test]
    fn prefix_canonicalization_preview_retains_authored_reference_slot() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve(
            "let p = 'lit'\nlet sink = mock.Sink { name_prefix = p }\n",
        )
        .unwrap();
        let original = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &context);
        let original_tree = AnalysisTree::try_from_file(original.parsed()).unwrap();
        let original_slot = original_tree
            .resources()
            .next()
            .unwrap()
            .authored_slot("name_prefix")
            .unwrap();
        let canonical = canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview);
        assert!(canonical.prefix_errors().is_empty());
        let judged = canonical.judge(&schemas, &context);
        let tree = judged.analysis_tree().unwrap();
        let owner = tree.resources().next().unwrap();
        assert_eq!(
            owner.attributes()["name"],
            Value::Concrete(ConcreteValue::String("lit00000000".into()))
        );
        assert!(!owner.attributes().contains_key("name_prefix"));
        let slot = owner.authored_slot("name_prefix").unwrap();
        assert_eq!(slot.occurrence(), original_slot.occurrence());
        assert_eq!(slot.instance(), original_slot.instance());
        assert_eq!(
            slot.cursor().unwrap().form(),
            original_slot.cursor().unwrap().form()
        );
        assert!(owner.projected_phase("name").is_some());
        assert!(owner.authored_slot("name").is_err());
        let again = canonical.judge(&schemas, &context);
        assert_eq!(
            again.file().resources[0].attributes,
            judged.file().resources[0].attributes
        );
    }

    #[test]
    fn prefix_canonicalization_random_and_preview_have_equivalent_provenance() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve(
            "let p = 'lit'\nlet sink = mock.Sink { name_prefix = p }\n",
        )
        .unwrap();
        let cli = canonicalize_file_prefixes(parsed.clone(), &schemas, SuffixSource::Random);
        let lsp = canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview);
        let cli = cli.judge(&schemas, &context);
        let lsp = lsp.judge(&schemas, &context);
        let cli_tree = cli.analysis_tree().unwrap();
        let lsp_tree = lsp.analysis_tree().unwrap();
        let cli_owner = cli_tree.resources().next().unwrap();
        let lsp_owner = lsp_tree.resources().next().unwrap();
        assert_eq!(cli_owner.id(), lsp_owner.id());
        assert_eq!(
            cli_owner.attributes().keys().collect::<Vec<_>>(),
            lsp_owner.attributes().keys().collect::<Vec<_>>()
        );
        assert_eq!(
            cli.file().resources[0].prefixes,
            lsp.file().resources[0].prefixes
        );
        assert_eq!(
            cli_owner
                .projected_phase("name")
                .unwrap()
                .no_conversion()
                .is_some(),
            lsp_owner
                .projected_phase("name")
                .unwrap()
                .no_conversion()
                .is_some()
        );
        let cli_slot = cli_owner.authored_slot("name_prefix").unwrap();
        let lsp_slot = lsp_owner.authored_slot("name_prefix").unwrap();
        assert_eq!(cli_slot.occurrence(), lsp_slot.occurrence());
        assert_eq!(cli_slot.instance(), lsp_slot.instance());
        assert_eq!(cli_slot.generation(), lsp_slot.generation());
        assert_eq!(
            cli_slot.cursor().unwrap().form(),
            lsp_slot.cursor().unwrap().form()
        );
        assert_eq!(
            cli_owner.attribute_key_location("name"),
            lsp_owner.attribute_key_location("name")
        );
        assert_eq!(
            cli_owner.attribute_assignment_location("name"),
            lsp_owner.attribute_assignment_location("name")
        );
        // The generated value alone differs: both suffix sources have eight lowercase hex characters.
        for owner in [cli_owner, lsp_owner] {
            let values = owner.attributes();
            let Value::Concrete(ConcreteValue::String(generated)) = &values["name"] else {
                panic!("generated string");
            };
            let suffix = generated.expose_plaintext().strip_prefix("lit").unwrap();
            assert_eq!(suffix.len(), 8);
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
        }
        let inputs =
            crate::validation::admission::attributes::ResourceAttributeInputs::for_resource(
                lsp_owner,
                lsp.report(),
            )
            .unwrap();
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(lsp.file(), &schemas, &context);
        crate::validation::deferred_types::with_session_for_test(&context, |session| {
            let request = crate::validation::admission::attributes::ResourceAdmissionRequest::new(
                lsp_owner,
                lsp_owner.quoted_string_attrs(),
                inputs,
                crate::validation::admission::attributes::ValidationEvaluation::None,
                &bindings,
                session,
            )
            .unwrap();
            let mut validation =
                crate::validation::ConversionValidation::<std::convert::Infallible>::new(
                    lsp.report(),
                );
            let outcome = validation.admit_resource(request);
            assert_eq!(
                outcome.values.errors().count(),
                0,
                "the consumed authored slot has no value and stays silent"
            );
        });
    }

    #[test]
    fn prefix_canonicalization_rejects_only_the_failed_owner() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve(
            "mock.Sink { name_prefix = '' }\nmock.Sink { name_prefix = 'ok' }\n",
        )
        .unwrap();
        let canonical = canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview);
        assert_eq!(canonical.prefix_errors().len(), 1);
        let judged = canonical.judge(&schemas, &context);
        let tree = judged.analysis_tree().unwrap();
        assert_eq!(tree.project_resource_refs().count(), 2);
        let owners = tree.resources().collect::<Vec<_>>();
        assert_eq!(owners.len(), 1);
        assert_eq!(
            owners[0].attributes()["name"],
            Value::Concrete(ConcreteValue::String("ok00000000".into()))
        );
    }
    #[test]
    fn prefix_canonicalization_keeps_raw_project_walk_scope() {
        let directory = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("providers.crn", "provider mock {}"),
            (
                "backend.crn",
                "backend local { path = 'carina.state.json' }",
            ),
            (
                "main.crn",
                "let src = mock.test.resource { name = 'source' }\nlet reads = for item in src.domain_validation_options { read mock.iam.Roles { name_regex = item } }\n",
            ),
        ] {
            std::fs::write(directory.path().join(name), source).unwrap();
        }
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::config_loader::parse_directory(
            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
            &context,
        )
        .unwrap()
        .into_parsed();
        let raw = AnalysisTree::try_from_file(&parsed).unwrap();
        assert_eq!(
            raw.resources().count(),
            2,
            "includes a parser-side read template"
        );
        let expected = raw
            .project_resources()
            .map(|resource| resource.location().clone())
            .collect::<Vec<_>>();
        assert_eq!(
            expected.len(),
            1,
            "the established project walk excludes that read template"
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("iam.Roles")
                .as_data_source()
                .attribute(AttributeSchema::optional("name_regex", AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
        let canonical = canonicalize_file_prefixes(parsed.clone(), &schemas, SuffixSource::Preview);
        let judged = canonical.judge(&schemas, &context);
        let tree = judged.analysis_tree().unwrap();
        assert_eq!(
            tree.resources().count(),
            2,
            "both references have registered schemas"
        );
        assert_eq!(
            tree.managed_resources().count(),
            1,
            "module admission stays on materialized managed members"
        );
        assert_eq!(
            tree.project_resource_refs()
                .map(|resource| resource.location().clone())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
