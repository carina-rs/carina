//! The file that crossed prefix canonicalization is admission's authority.
mod read;
use super::{
    AttributePrefixError, AttributePrefixFailure, DeferredPrefixDependencies, PrefixDependency,
    ResourcePrefixSource, generate_random_suffix, prefix_companion_attribute,
};
use crate::parser::retained::JudgedFile;
use crate::parser::{
    AnalysisResourceRef, AnalysisTree, AnalysisTreeError, DeferredForPhaseTraversal,
    ExportParamLike, File, ProviderContext, ResourceRef,
};
use crate::resource::{
    BindingAddress, ConcreteValue, ModuleSource, ParserPlaceholder, PlaceholderKind, Resource,
    ResourceId, Value, ValueString,
};
use crate::schema::{OwnerBoundResourceSchema, SchemaKind, SchemaRegistry};
use crate::validation::admission::{DeclarationIdentity, attributes::declaration_identity};
use indexmap::IndexMap;
pub use read::{ModuleResolvedFileRead, PrefixFileRead};
use std::ops::Deref;

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
/// The witness cannot outlive the catalog that authorized its keys:
/// ```compile_fail,E0515
/// use carina_core::{identifier::{canonicalize_file_prefixes, PrefixCanonicalized, SuffixSource}, parser::ParsedFile, schema::SchemaRegistry};
/// fn escape(file: ParsedFile) -> PrefixCanonicalized<'static, ParsedFile> {
///     let schemas = SchemaRegistry::new();
///     canonicalize_file_prefixes(file, &schemas, SuffixSource::Preview)
/// }
/// ```
/// The inner judgement cannot be detached from its canonicalization witness:
/// ```compile_fail,E0614
/// use carina_core::{identifier::PrefixCanonicalized, parser::{retained::JudgedFile, ParsedExportParam, ParsedDeferredForPhase}, resource::{ParserPlaceholder, RelativeBindingName}};
/// type Judged<'s> = JudgedFile<'s, ParsedExportParam, ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>>;
/// fn detach<'f, 's>(wrapper: &'f PrefixCanonicalized<'s, Judged<'s>>) -> &'f Judged<'s> {
///     &**wrapper
/// }
/// ```
/// A detached judgement cannot enter the resource admission driver:
/// ```compile_fail,E0308
/// use carina_core::{parser::{retained::JudgedFile, ParsedExportParam, ParsedDeferredForPhase}, resource::{ParserPlaceholder, RelativeBindingName}, binding_index::BindingIndex, validation::{ConversionValidation, FindingSubject, deferred_types::ProviderTypeCheckContext}};
/// type Judged<'s> = JudgedFile<'s, ParsedExportParam, ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>>;
/// fn bypass(file: &Judged<'_>, bindings: &BindingIndex<'_>, session: &dyn ProviderTypeCheckContext) {
///     let mut run = ConversionValidation::<std::convert::Infallible>::new(file.report());
///     let _ = run.admitted_resources(file, bindings, session, FindingSubject::Root);
/// }
/// ```
/// Read views cannot be cloned into a file and canonicalized against another catalog:
/// ```compile_fail,E0308
/// use carina_core::{identifier::{PrefixCanonicalized, canonicalize_file_prefixes, SuffixSource}, parser::{retained::JudgedFile, ParsedExportParam, ParsedDeferredForPhase}, resource::{ParserPlaceholder, RelativeBindingName}, schema::SchemaRegistry};
/// type Judged<'s> = JudgedFile<'s, ParsedExportParam, ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>>;
/// fn reroute(wrapper: &PrefixCanonicalized<'_, Judged<'_>>, other: &SchemaRegistry) {
///     let _ = canonicalize_file_prefixes(wrapper.parsed().clone(), other, SuffixSource::Preview);
/// }
/// ```
#[derive(Clone)]
pub struct PrefixCanonicalized<'s, F> {
    file: F,
    schemas: &'s SchemaRegistry,
    errors: Vec<AttributePrefixError>,
    visits: Vec<OwnerVisit>,
}
// These addresses are minted only while visiting the owning file. Judging
// clones that file without adding/reordering owners, so addresses survive the
// phase transition without treating a projected ResourceId as membership.
#[derive(Clone, Copy)]
enum OwnerAddress {
    Resource(usize),
    DataSource(usize),
    DeferredFor(usize),
    Conditional { conditional: usize, branch: usize },
}
#[derive(Clone)]
struct OwnerVisit {
    owner: OwnerAddress,
    state: PrefixAdmission,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrefixAdmission {
    Ready,
    AwaitingArguments,
    Rejected,
}
impl OwnerAddress {
    fn managed_resource<X, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>(
        self,
        file: &File<X, P, T, D, B>,
    ) -> Option<&Resource<ModuleSource, P, B>> {
        match self {
            Self::Resource(index) => file.resources.get(index),
            Self::Conditional {
                conditional,
                branch,
            } => {
                match file
                    .deferred_module_ifs
                    .get(conditional)?
                    .branches()
                    .nth(branch)?
                {
                    crate::parser::DeferredIfBody::Resource(resource) => Some(resource),
                    crate::parser::DeferredIfBody::DataSource(_)
                    | crate::parser::DeferredIfBody::ModuleCall(_) => None,
                }
            }
            Self::DataSource(_) | Self::DeferredFor(_) => None,
        }
    }
}

// Pointer keys are local to this borrow, rebuilt from recorded addresses after
// cloning/judging. Pending/anonymous IDs may be equal for different owners.
fn visited_members<X, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>(
    file: &File<X, P, T, D, B>,
    visits: &[OwnerVisit],
) -> std::collections::HashMap<*const ResourceId, bool>
where
    D: DeferredForPhaseTraversal<P, T, B>,
{
    let templates: Vec<_> = file
        .deferred_for_phase
        .analysis_resource_refs()
        .map(|resource| std::ptr::from_ref(resource.id()))
        .collect();
    visits
        .iter()
        .filter_map(|visit| {
            let owner = match visit.owner {
                OwnerAddress::Resource(_) => visit
                    .owner
                    .managed_resource(file)
                    .map(|resource| std::ptr::from_ref(&resource.id)),
                OwnerAddress::Conditional {
                    conditional,
                    branch,
                } => file
                    .deferred_module_ifs
                    .get(conditional)
                    .and_then(|deferred| deferred.branches().nth(branch))
                    .and_then(|body| match body {
                        crate::parser::DeferredIfBody::Resource(resource) => {
                            Some(std::ptr::from_ref(&resource.id))
                        }
                        crate::parser::DeferredIfBody::DataSource(resource) => {
                            Some(std::ptr::from_ref(&resource.id))
                        }
                        crate::parser::DeferredIfBody::ModuleCall(_) => None,
                    }),
                OwnerAddress::DataSource(index) => file
                    .data_sources
                    .get(index)
                    .map(|resource| std::ptr::from_ref(&resource.id)),
                OwnerAddress::DeferredFor(index) => templates.get(index).copied(),
            }?;
            Some((owner, visit.state != PrefixAdmission::Ready))
        })
        .collect()
}

fn membership_status(
    members: &std::collections::HashMap<*const ResourceId, bool>,
    id: &ResourceId,
    schema: Option<OwnerBoundResourceSchema<'_>>,
    declaration: Option<&crate::parser::DeclarationLocation>,
) -> Result<Option<bool>, AnalysisTreeError> {
    let status = members.get(&std::ptr::from_ref(id)).copied();
    if status.is_none() && schema.is_some() {
        return Err(AnalysisTreeError::new(
            crate::parser::AnalysisTreeErrorKind::UncanonicalizedResource {
                id: Box::new(id.clone()),
                declaration: declaration.cloned(),
            },
        ));
    }
    Ok(status)
}

impl<F: std::fmt::Debug> std::fmt::Debug for PrefixCanonicalized<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixCanonicalized")
            .field("file", &self.file)
            .field("errors", &self.errors)
            .finish_non_exhaustive()
    }
}
impl<'s, F> PrefixCanonicalized<'s, F> {
    pub fn prefix_errors(&self) -> &[AttributePrefixError] {
        &self.errors
    }
    pub fn schemas(&self) -> &'s SchemaRegistry {
        self.schemas
    }
}

pub fn canonicalize_file_prefixes<
    's,
    X,
    P: PlaceholderKind,
    T: PlaceholderKind,
    D: DeferredForPhaseTraversal<P, T, B>,
    B: BindingAddress,
>(
    mut file: File<X, P, T, D, B>,
    schemas: &'s SchemaRegistry,
    suffix: SuffixSource,
) -> PrefixCanonicalized<'s, File<X, P, T, D, B>> {
    let mut errors = Vec::new();
    let mut visits = Vec::new();
    let mut record = |owner, findings: Vec<AttributePrefixError>| {
        let state = if findings.is_empty() {
            PrefixAdmission::Ready
        } else if findings
            .iter()
            .all(|finding| finding.owner().awaiting_arguments())
        {
            PrefixAdmission::AwaitingArguments
        } else {
            PrefixAdmission::Rejected
        };
        // Unbound parameters are obligations, not findings. Their owners stay
        // unavailable to generic admission until a concrete caller is expanded.
        errors.extend(
            findings
                .into_iter()
                .filter(|finding| !finding.owner().awaiting_arguments()),
        );
        visits.push(OwnerVisit { owner, state });
    };
    let expressions = file.parser_binding_metadata.syntax().expressions();
    let variables = &file.variables;
    for (index, resource) in file.resources.iter_mut().enumerate() {
        record(
            OwnerAddress::Resource(index),
            resolve_resource(
                resource,
                schemas,
                suffix,
                Some(expressions),
                Some(variables),
            ),
        );
    }
    // Reads never generate names. Deferred-for templates retain their existing
    // literal-prefix behavior, but a deferred companion must own its rejection.
    for index in 0..file.data_sources.len() {
        record(OwnerAddress::DataSource(index), Vec::new());
    }
    for (index, resource) in file.deferred_for_phase.analysis_resource_refs().enumerate() {
        let mut findings = Vec::new();
        if let ResourceRef::Deferred { resource, .. } = resource {
            let resource = resource.as_resource();
            if let Some(schema) = schemas.get_for(resource) {
                let source = PrefixSource {
                    id: &resource.id,
                    declaration: Some(&resource.declaration),
                    slots: Some(&resource.authored_slots),
                    expressions: Some(expressions),
                    variables: None,
                    module_source: resource.module_source.as_ref(),
                };
                for (key, value) in &resource.attributes {
                    if let CompanionRecognition::Deferred { base, dependencies } =
                        recognize_companion(key, value, schema.schema().as_schema(), &source)
                    {
                        findings.push(deferred_companion_error(base, dependencies, &source));
                    }
                }
            }
        }
        record(OwnerAddress::DeferredFor(index), findings);
    }
    for (conditional, deferred) in file.deferred_module_ifs.iter_mut().enumerate() {
        for (branch, body) in deferred.branches_mut().enumerate() {
            let findings = match body {
                crate::parser::DeferredIfBody::Resource(resource) => resolve_resource(
                    resource,
                    schemas,
                    suffix,
                    Some(expressions),
                    Some(variables),
                ),
                crate::parser::DeferredIfBody::DataSource(_) => Vec::new(),
                crate::parser::DeferredIfBody::ModuleCall(_) => continue,
            };
            record(
                OwnerAddress::Conditional {
                    conditional,
                    branch,
                },
                findings,
            );
        }
    }
    PrefixCanonicalized {
        file,
        schemas,
        errors,
        visits,
    }
}

impl<X, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>
    PrefixCanonicalized<'_, File<X, P, T, D, B>>
{
    pub fn parsed(&self) -> PrefixFileRead<'_, X, P, T, D, B> {
        PrefixFileRead::new(&self.file, self.schemas)
    }
    pub fn canonicalize_values(
        &mut self,
    ) -> Result<
        crate::value::CanonicalizedResources<'_, ModuleSource, P, B>,
        crate::value::EnumCanonicalizationError,
    > {
        crate::value::canonicalize_resources_with_schemas(&mut self.file.resources, self.schemas)
    }
}
impl<'s, X, D, B: BindingAddress>
    PrefixCanonicalized<'s, File<X, ParserPlaceholder, ParserPlaceholder, D, B>>
where
    X: Clone + ExportParamLike<ParserPlaceholder, B>,
    D: Clone + DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn judge(
        &self,
        context: &ProviderContext,
    ) -> PrefixCanonicalized<'s, JudgedFile<'s, X, D, B>> {
        PrefixCanonicalized {
            file: JudgedFile::new(self.file.clone(), self.schemas, context),
            schemas: self.schemas,
            errors: self.errors.clone(),
            visits: self.visits.clone(),
        }
    }
}

/// Values whose authored keys crossed the same per-owner canonicalizer.
#[derive(Clone)]
pub struct CanonicalAttributeValues<P: PlaceholderKind, B: BindingAddress> {
    values: IndexMap<String, Value<P, B>>,
    pub(crate) declaration: DeclarationIdentity,
}

/// Correspondence between a canonical owner and a complete per-file view.
pub enum ResourceProjection<B: BindingAddress> {
    Matched(MatchedResourceProjection<B>),
    NoMatch,
    Ambiguous,
}

/// A unique source projection, constructed only by the core matcher.
///
/// A caller-owned value map cannot claim correspondence:
/// ```compile_fail,E0451
/// use carina_core::{identifier::{CanonicalAttributeValues, MatchedResourceProjection}, resource::{ParserPlaceholder, RelativeBindingName}};
/// fn forge(values: CanonicalAttributeValues<ParserPlaceholder, RelativeBindingName>) -> MatchedResourceProjection<RelativeBindingName> {
///     MatchedResourceProjection { values }
/// }
/// ```
pub struct MatchedResourceProjection<B: BindingAddress> {
    values: CanonicalAttributeValues<ParserPlaceholder, B>,
}

impl<B: BindingAddress> MatchedResourceProjection<B> {
    pub fn values(&self) -> &CanonicalAttributeValues<ParserPlaceholder, B> {
        &self.values
    }

    pub fn into_values(self) -> CanonicalAttributeValues<ParserPlaceholder, B> {
        self.values
    }
}
pub(crate) struct CanonicalSchemaInputs<
    'f,
    P: PlaceholderKind,
    T: PlaceholderKind,
    B: BindingAddress,
> {
    pub is_module_definition: bool,
    pub resources: Vec<CanonicalSchemaResource<'f, P, T, B>>,
}

// Whole-file schema validation also visits data sources and deferred templates.
// Only the canonical file can project these inputs; excluded kinds retain their
// original keys, exactly as in the CLI canonicalization pass.
pub(crate) enum CanonicalSchemaResource<
    'f,
    P: PlaceholderKind,
    T: PlaceholderKind,
    B: BindingAddress,
> {
    Resource(&'f Resource<ModuleSource, P, B>, SchemaValues<'f, P, B>),
    DataSource(
        &'f crate::resource::DataSource<ModuleSource, P, B>,
        SchemaValues<'f, P, B>,
    ),
    Deferred(&'f Resource<ModuleSource, T, B>, SchemaValues<'f, T, B>),
}
type SchemaValues<'f, P, B> =
    Option<(OwnerBoundResourceSchema<'f>, CanonicalAttributeValues<P, B>)>;

fn schema_validation_resources<
    'f,
    X,
    P: PlaceholderKind,
    T: PlaceholderKind,
    D,
    B: BindingAddress,
>(
    file: &'f File<X, P, T, D, B>,
    schemas: &'f SchemaRegistry,
    visits: &[OwnerVisit],
) -> Result<CanonicalSchemaInputs<'f, P, T, B>, AnalysisTreeError>
where
    D: DeferredForPhaseTraversal<P, T, B>,
{
    fn values<P: PlaceholderKind, B: BindingAddress>(
        schema: OwnerBoundResourceSchema<'_>,
        attributes: &IndexMap<String, Value<P, B>>,
    ) -> CanonicalAttributeValues<P, B> {
        CanonicalAttributeValues {
            values: attributes.clone(),
            declaration: declaration_identity(schema),
        }
    }
    let members = visited_members(file, visits);
    let mut resources = Vec::new();
    for resource in file.iter_all_resources() {
        match resource {
            ResourceRef::LiveComposition(_) => {}
            ResourceRef::Resource(resource) => {
                let schema = schemas.get_for(resource);
                if membership_status(
                    &members,
                    &resource.id,
                    schema,
                    resource.declaration.location(),
                )? == Some(false)
                {
                    resources.push(CanonicalSchemaResource::Resource(
                        resource,
                        schema.map(|schema| (schema, values(schema, &resource.attributes))),
                    ));
                }
            }
            ResourceRef::DataSource(resource) => {
                let schema = schemas.get_for_data_source(resource);
                if membership_status(
                    &members,
                    &resource.id,
                    schema,
                    resource.declaration.location(),
                )? == Some(false)
                {
                    resources.push(CanonicalSchemaResource::DataSource(
                        resource,
                        schema.map(|schema| (schema, values(schema, &resource.attributes))),
                    ));
                }
            }
            ResourceRef::Deferred { resource, .. } => {
                let schema = schemas.get_for(resource.as_resource());
                if membership_status(
                    &members,
                    resource.id(),
                    schema,
                    resource.as_resource().declaration.location(),
                )? == Some(false)
                {
                    resources.push(CanonicalSchemaResource::Deferred(
                        resource.as_resource(),
                        schema.map(|schema| (schema, values(schema, resource.attributes()))),
                    ));
                }
            }
        }
    }
    Ok(CanonicalSchemaInputs {
        is_module_definition: crate::validation::is_module_definition(file),
        resources,
    })
}

impl<X, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>
    PrefixCanonicalized<'_, File<X, P, T, D, B>>
where
    D: DeferredForPhaseTraversal<P, T, B>,
{
    pub(crate) fn schema_validation_resources(
        &self,
    ) -> Result<CanonicalSchemaInputs<'_, P, T, B>, AnalysisTreeError> {
        schema_validation_resources(&self.file, self.schemas, &self.visits)
    }
    pub(crate) fn validation_report(
        &self,
        context: &ProviderContext,
    ) -> crate::parser::retained::ExpressionReport<B> {
        let existing = self
            .file
            .parser_binding_metadata
            .syntax()
            .expressions()
            .admission_report();
        if existing.has_authored_judgement() {
            existing.clone()
        } else {
            crate::parser::retained::judge_expressions(&self.file, self.schemas, context)
        }
    }
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
    pub fn module_source(&self) -> Option<&'f ModuleSource> {
        self.resource.module_source()
    }
    pub fn resource_schema(&self) -> OwnerBoundResourceSchema<'f> {
        self.schema
    }
    pub(crate) fn parsed(
        &self,
    ) -> PrefixFileRead<'f, X, ParserPlaceholder, ParserPlaceholder, D, B> {
        PrefixFileRead::new(self.file, self.schemas)
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
    /// Project ordinary values onto this owner's canonical keys. Generated
    /// values retain their producer's projection, including its prefix suffix.
    /// A projection cannot introduce keys or restore consumed companions.
    /// Exact same-phase instance addresses include their binding address. When
    /// only the merged view materialized a loop, its unique per-file deferred
    /// declaration supplies the original body projection instead.
    ///
    /// Arbitrary maps must use the separate caller-value path:
    /// ```compile_fail,E0308
    /// use carina_core::{identifier::CanonicalResource, parser::{ParsedDeferredForPhase, ParsedExportParam}, resource::{ParserPlaceholder, RelativeBindingName, Value}};
    /// fn raw(owner: &CanonicalResource<'_, ParsedExportParam, ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>, RelativeBindingName>, values: &indexmap::IndexMap<String, Value<ParserPlaceholder, RelativeBindingName>>) {
    ///     let _ = owner.project_values(values);
    /// }
    /// ```
    pub fn project_values<E, S>(
        &self,
        source: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, S, B, E>,
    ) -> ResourceProjection<B>
    where
        S: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        let Some(key) = self.authored_instance_key() else {
            return ResourceProjection::NoMatch;
        };
        let mut exact = source
            .project_resources()
            .filter(|source| source.authored_instance_key().as_ref() == Some(&key));
        let selected = match (exact.next(), exact.next()) {
            (Some(_), Some(_)) => return ResourceProjection::Ambiguous,
            (Some(source), None) => Some(source),
            (None, _) => None,
        };
        let selected = match selected {
            Some(source) => Some(source),
            None => {
                let mut templates = source.project_resources().filter(|source| {
                    source.kind() == crate::parser::AnalysisItemKind::DeferredResource
                        && source
                            .authored_instance_key()
                            .is_some_and(|source| key.same_source_declaration(&source))
                });
                match (templates.next(), templates.next()) {
                    (Some(_), Some(_)) => return ResourceProjection::Ambiguous,
                    (Some(source), None) => Some(source),
                    (None, _) => None,
                }
            }
        };
        match selected {
            Some(source) => ResourceProjection::Matched(MatchedResourceProjection {
                values: self.project_selected_values(&source.attributes()),
            }),
            None => ResourceProjection::NoMatch,
        }
    }

    fn project_selected_values(
        &self,
        values: &IndexMap<String, Value<ParserPlaceholder, B>>,
    ) -> CanonicalAttributeValues<ParserPlaceholder, B> {
        CanonicalAttributeValues {
            values: self
                .resource
                .attributes()
                .iter()
                .map(|(name, value)| {
                    let projected = if self.resource.projected_phase(name).is_some() {
                        value
                    } else {
                        values.get(name).unwrap_or(value)
                    };
                    (name.clone(), projected.clone())
                })
                .collect(),
            declaration: declaration_identity(self.schema),
        }
    }
    /// Caller projections stay open for #4400; their keys must still be resolved.
    /// This projection uses the deterministic preview suffix (`00000000`),
    /// and must not supply the CLI's generated resource names.
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
impl<'s, X, D, B: BindingAddress> PrefixCanonicalized<'s, JudgedFile<'s, X, D, B>>
where
    X: ExportParamLike<ParserPlaceholder, B>,
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn schema_validation_resources(
        &self,
    ) -> Result<CanonicalSchemaInputs<'_, ParserPlaceholder, ParserPlaceholder, B>, AnalysisTreeError>
    {
        schema_validation_resources(self.file.file(), self.schemas, &self.visits)
    }
    pub(crate) fn admission_report(&self) -> &crate::parser::retained::ExpressionReport<B> {
        self.file
            .file()
            .parser_binding_metadata
            .syntax()
            .expressions()
            .admission_report()
    }
    pub fn parsed(&self) -> PrefixFileRead<'_, X, ParserPlaceholder, ParserPlaceholder, D, B> {
        PrefixFileRead::new(self.file.file(), self.schemas)
    }
    #[cfg(test)]
    pub(crate) fn edit_judgement_for_test(
        &mut self,
        edit: impl FnOnce(&mut JudgedFile<'s, X, D, B>),
    ) {
        edit(&mut self.file);
    }
    #[cfg(test)]
    pub(crate) fn file(&self) -> &File<X, ParserPlaceholder, ParserPlaceholder, D, B> {
        self.file.file()
    }
    pub fn report(&self) -> &crate::parser::retained::ExpressionReport<B> {
        self.file.report()
    }
    pub fn secrecy(&self) -> &crate::secrecy::SecrecyReport {
        self.file.secrecy()
    }
    pub fn validation_context_in_session<'f>(
        &'f self,
        session: &'f dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        signatures: &'f crate::module_resolver::ResolvedModuleSignatures,
    ) -> crate::validation::ValidationTypeContext<'f, B> {
        crate::validation::ValidationTypeContext::in_session(&self.file, session, signatures)
    }
    pub fn validation_context<'f>(
        &'f self,
        provider: &'f crate::parser::LoadedProviderContext,
        signatures: &'f crate::module_resolver::ResolvedModuleSignatures,
    ) -> crate::validation::ValidationTypeContext<'f, B> {
        crate::validation::ValidationTypeContext::new(&self.file, provider, signatures)
    }
    pub fn providerless_validation_context<'f>(
        &'f self,
        provider: &'f ProviderContext,
        signatures: &'f crate::module_resolver::ResolvedModuleSignatures,
    ) -> crate::validation::ValidationTypeContext<'f, B> {
        crate::validation::ValidationTypeContext::new_providerless(&self.file, provider, signatures)
    }
    pub fn integer_arithmetic_findings(
        &self,
        bindings: &crate::binding_index::BindingIndex<'_, B>,
    ) -> Vec<crate::parser::Located<crate::validation::IntegerArithmeticTypeFinding>> {
        crate::validation::check_file_integer_arithmetic_values(&self.file, bindings)
    }
    pub fn judged(&self) -> &Self {
        self
    }
    pub fn prefix_failures(&self) -> impl Iterator<Item = &AttributePrefixError> {
        self.errors.iter()
    }

    pub fn analysis_tree(&self) -> Result<CanonicalAnalysisTree<'_, X, D, B>, AnalysisTreeError> {
        let file = self.file.file();
        let tree = AnalysisTree::try_from_file(file)?;
        let members = visited_members(file, &self.visits);
        let top_level_resource_count = tree.top_level_resources().count();
        let resources = tree
            .into_resources()
            .into_iter()
            .map(|resource| {
                let schema = resource.schema(self.schemas);
                let status =
                    membership_status(&members, resource.id(), schema, Some(resource.location()))?;
                Ok(match (status, schema) {
                    (Some(false), Some(schema)) => CanonicalItem::Bound(CanonicalResource {
                        resource,
                        file,
                        schemas: self.schemas,
                        schema,
                    }),
                    (Some(true), Some(_schema)) => CanonicalItem::Rejected(resource),
                    (None, _) | (_, None) => CanonicalItem::Unavailable(resource),
                })
            })
            .collect::<Result<Vec<_>, AnalysisTreeError>>()?;
        Ok(CanonicalAnalysisTree {
            resources,
            top_level_resource_count,
        })
    }
}

impl<'s, D, B: BindingAddress>
    PrefixCanonicalized<
        's,
        JudgedFile<'s, crate::parser::ParsedExportParam<ParserPlaceholder, B>, D, B>,
    >
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn inferred_export_params(
        &self,
    ) -> crate::validation::inference::SchemaCheckedExportParams<B> {
        crate::validation::inference::infer_export_params(&self.file)
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
) -> Vec<AttributePrefixError> {
    resources
        .iter_mut()
        .flat_map(|resource| resolve_resource(resource, schemas, suffix, None, None))
        .collect()
}

fn resolve_resource<P: PlaceholderKind, B: BindingAddress>(
    resource: &mut Resource<ModuleSource, P, B>,
    schemas: &SchemaRegistry,
    suffix: SuffixSource,
    expressions: Option<&crate::parser::retained::Expressions<B>>,
    variables: Option<&IndexMap<String, Value<P, B>>>,
) -> Vec<AttributePrefixError> {
    let Some(schema) = schemas.get_for(resource) else {
        return Vec::new();
    };
    let (rewrites, findings) = rewrite_prefix_values(
        &mut resource.attributes,
        schema,
        PrefixSource {
            id: &resource.id,
            declaration: Some(&resource.declaration),
            slots: Some(&resource.authored_slots),
            expressions,
            variables,
            module_source: resource.module_source.as_ref(),
        },
        suffix,
    );
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
    findings
}

pub(crate) fn canonicalize_prefix_values<P: PlaceholderKind, B: BindingAddress>(
    schema: OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    mut values: IndexMap<String, Value<P, B>>,
    suffix: SuffixSource,
) -> Result<CanonicalAttributeValues<P, B>, Vec<AttributePrefixError>> {
    let (_rewrites, errors) = rewrite_prefix_values(
        &mut values,
        schema,
        PrefixSource {
            id,
            declaration: None,
            slots: None,
            expressions: None,
            variables: None,
            module_source: None,
        },
        suffix,
    );
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(CanonicalAttributeValues {
        values,
        declaration: declaration_identity(schema),
    })
}

struct PrefixSource<'a, P: PlaceholderKind, B: BindingAddress> {
    id: &'a ResourceId,
    declaration: Option<&'a crate::parser::DeclarationProvenance>,
    slots: Option<&'a crate::parser::retained::AuthoredSlots<B>>,
    expressions: Option<&'a crate::parser::retained::Expressions<B>>,
    variables: Option<&'a IndexMap<String, Value<P, B>>>,
    module_source: Option<&'a ModuleSource>,
}

impl<P: PlaceholderKind, B: BindingAddress> PrefixSource<'_, P, B> {
    fn declaration_owner(
        &self,
        authored: Option<crate::parser::DeclarationLocation>,
    ) -> crate::finding::FindingOwner {
        crate::finding::FindingOwner::Declaration {
            authored,
            module_call: self
                .module_source
                .and_then(ModuleSource::module_call)
                .map(|source| source.module_call_path().root().id.clone()),
        }
    }
    fn argument_call(&self) -> Option<&crate::parser::ModuleCallId> {
        self.module_source
            .and_then(ModuleSource::module_call)
            .map(|source| &source.module_call_path().immediate().id)
    }
}

/// Recognition owns deferred companions too. Only NotCompanion can proceed to
/// ordinary unknown-key/required-attribute admission; Deferred rejects the owner.
enum CompanionRecognition {
    Known {
        base: String,
        prefix: ValueString,
    },
    Deferred {
        base: String,
        dependencies: DeferredPrefixDependencies,
    },
    NotCompanion,
}

fn recognize_companion<P: PlaceholderKind, B: BindingAddress>(
    key: &str,
    value: &Value<P, B>,
    schema: &crate::schema::CompiledResourceSchema,
    source: &PrefixSource<'_, P, B>,
) -> CompanionRecognition {
    let Some(base) = key.strip_suffix(super::PREFIX_COMPANION_SUFFIX) else {
        return CompanionRecognition::NotCompanion;
    };
    let Some(attribute) = schema.attributes.get(base) else {
        return CompanionRecognition::NotCompanion;
    };
    if !crate::validation::is_string_compatible_type(&attribute.attr_type, &schema.defs) {
        return CompanionRecognition::NotCompanion;
    }
    match value {
        Value::Concrete(ConcreteValue::String(prefix)) => CompanionRecognition::Known {
            base: base.to_owned(),
            prefix: prefix.clone(),
        },
        Value::Concrete(_) => CompanionRecognition::NotCompanion,
        Value::Deferred(_) => CompanionRecognition::Deferred {
            base: base.to_owned(),
            dependencies: deferred_prefix_dependencies(value, key, source),
        },
    }
}

fn rewrite_prefix_values<P: PlaceholderKind, B: BindingAddress>(
    values: &mut IndexMap<String, Value<P, B>>,
    bound: OwnerBoundResourceSchema<'_>,
    source: PrefixSource<'_, P, B>,
    suffix: SuffixSource,
) -> (Vec<PrefixRewrite>, Vec<AttributePrefixError>) {
    let schema = bound.schema().as_schema();
    if schema.kind != SchemaKind::Resource {
        return (Vec::new(), Vec::new());
    }
    let companions = values
        .iter()
        .map(|(key, value)| recognize_companion(key, value, schema, &source))
        .collect::<Vec<_>>();
    let mut rewrites = Vec::new();
    let mut errors = Vec::new();
    for companion in companions {
        let (base, prefix) = match companion {
            CompanionRecognition::NotCompanion => continue,
            CompanionRecognition::Deferred { base, dependencies } => {
                errors.push(deferred_companion_error(base, dependencies, &source));
                continue;
            }
            CompanionRecognition::Known { base, prefix } => (base, prefix),
        };
        let authored = prefix_companion_attribute(&base);
        if prefix.expose_plaintext().is_empty() || values.contains_key(&base) {
            errors.push(AttributePrefixError {
                resource: source.id.clone(),
                prefix: authored,
                failure: if prefix.expose_plaintext().is_empty() {
                    AttributePrefixFailure::Empty
                } else {
                    AttributePrefixFailure::Conflict(base)
                },
                owner: source.declaration_owner(
                    source
                        .declaration
                        .and_then(|declaration| declaration.location().cloned()),
                ),
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

fn deferred_companion_error<P: PlaceholderKind, B: BindingAddress>(
    base: String,
    dependencies: DeferredPrefixDependencies,
    source: &PrefixSource<'_, P, B>,
) -> AttributePrefixError {
    let authored = prefix_companion_attribute(&base);
    let owner = if std::iter::once(&dependencies.first)
        .chain(&dependencies.rest)
        .all(|dependency| matches!(dependency, PrefixDependency::UnboundModuleArgument(_)))
    {
        crate::finding::FindingOwner::AwaitingModuleArguments
    } else if let Some(call) = &dependencies.argument_call {
        crate::finding::FindingOwner::ModuleArgument(call.clone())
    } else {
        source.declaration_owner(
            source
                .declaration
                .and_then(|declaration| declaration.attribute_key_finding_location(&authored)),
        )
    };
    AttributePrefixError {
        resource: source.id.clone(),
        owner,
        prefix: authored,
        failure: AttributePrefixFailure::Deferred { base, dependencies },
    }
}

fn binding_reference_paths<P: PlaceholderKind, B: BindingAddress>(
    binding: &B,
    source: &PrefixSource<'_, P, B>,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(value) = source
        .variables
        .and_then(|variables| variables.get(binding.as_str()))
    {
        value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
            paths.push(reference.diagnostic_path().to_dot_string())
        });
    } else if let Some(value) = source
        .expressions
        .and_then(|expressions| expressions.binding_projection(binding))
    {
        value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
            paths.push(reference.diagnostic_path().to_dot_string())
        });
    }
    paths
}

fn deferred_prefix_dependencies<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    authored: &str,
    source: &PrefixSource<'_, P, B>,
) -> DeferredPrefixDependencies {
    use crate::resource::{DiagnosticScope, ReferencePath};

    // Source names survive substitution independently of the resolved leaves.
    // Keep diagnostic paths scoped so a reference inside secret(...) stays opaque.
    let mut origins = Vec::new();
    if let Some(slots) = source.slots
        && let Ok(slot) = slots.get(authored)
    {
        slot.visit_authored_bindings(&mut |binding, scope| {
            let name = scope.path(ReferencePath::Binding(binding.clone())).render(
                |path| path.to_dot_string().rsplit('.').next().unwrap().to_owned(),
                || "<secret>".to_owned(),
            );
            let module_argument = source
                .expressions
                .is_some_and(|expressions| expressions.is_module_argument(binding));
            let paths = binding_reference_paths(binding, source);
            let argument_location = if module_argument {
                source.argument_call().and_then(|call| {
                    call.argument_value_location(binding.as_str().rsplit('.').next().unwrap())
                })
            } else {
                None
            };
            origins.push((name, module_argument, paths, argument_location));
        });
    }
    let mut dependencies = Vec::new();
    let mut argument_call = None;
    let mut push = |dependency| {
        if !dependencies.contains(&dependency) {
            dependencies.push(dependency);
        }
    };
    value.visit_resource_refs_for_diagnostics(&mut |reference, _| {
        let path = reference.diagnostic_path().to_dot_string();
        let origin = origins
            .iter()
            .find(|(_, _, paths, _)| paths.contains(&path));
        let through = match origin {
            Some((_, true, _, argument_location)) => {
                argument_call = source.argument_call().cloned();
                argument_location
                    .and_then(|location| {
                        source
                            .expressions
                            .map(|expressions| expressions.bindings_at(location))
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .find(|(binding, _)| binding_reference_paths(binding, source).contains(&path))
                    .map(|(binding, scope)| {
                        scope.path(ReferencePath::Binding(binding)).to_dot_string()
                    })
            }
            origin => origin.map(|(name, _, _, _)| name.clone()),
        };
        push(PrefixDependency::Reference { path, through });
    });
    value.visit_binding_refs_for_diagnostics(&mut |binding| {
        push(PrefixDependency::Reference {
            path: binding.diagnostic_path().to_dot_string(),
            through: None,
        });
    });
    value.visit_placeholders(&mut |placeholder, scope| {
        if let Some(name) = placeholder.module_argument_name() {
            push(PrefixDependency::UnboundModuleArgument(
                if scope == DiagnosticScope::Secret {
                    "<secret>".to_owned()
                } else {
                    name.to_owned()
                },
            ));
        }
    });
    let mut dependencies = dependencies.into_iter();
    DeferredPrefixDependencies {
        first: dependencies.next().unwrap_or(PrefixDependency::Expression),
        rest: dependencies.collect(),
        argument_call,
    }
}

impl<E: crate::parser::IntoExpandedExportParam<crate::resource::AbsoluteBindingAddress>>
    PrefixCanonicalized<'_, crate::parser::ModuleResolvedFile<E>>
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
    fn argument_owner_renders_every_prefix_failure_kind() {
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            "let src = mock.Sink { name = 'source' }\nlet child = use { source = './child' }\nlet instance = child {}\n",
            std::path::Path::new("main.crn"), &context,
        ).unwrap();
        let call = parsed.module_calls[0].id();
        // #4492 will choose this owner for argument-dependent empty/conflict
        // findings. Their actual classification remains unchanged in #4461.
        for (failure, message) in [
            (
                AttributePrefixFailure::Empty,
                "'name_prefix' cannot be empty",
            ),
            (
                AttributePrefixFailure::Conflict("name".into()),
                "cannot specify both 'name_prefix' and 'name'",
            ),
        ] {
            let finding = AttributePrefixError {
                resource: parsed.resources[0].id.clone(),
                prefix: "name_prefix".into(),
                failure,
                owner: crate::finding::FindingOwner::ModuleArgument(call.clone()),
            };
            assert_eq!(finding.diagnostic_message(), message);
            assert_eq!(
                finding.to_string(),
                crate::parser::Located::new(call.declaration_location().clone(), message)
                    .to_string()
            );
        }
    }

    #[test]
    fn deferred_prefix_rejects_owner_before_attribute_admission() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        for (value, dependency) in [
            ("src.generated", "'src.generated'"),
            ("\"${src.generated}-x\"", "'src.generated'"),
            ("p", "'src.generated' (through 'p')"),
        ] {
            let parsed = crate::parser::parse_and_resolve(&format!(
                "let src = mock.Sink {{ name = 'source' }}\nlet p = src.generated\nmock.Sink {{ name_prefix = {value} }}\n"
            ))
            .unwrap();
            let canonical =
                canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview).judge(&context);
            assert_eq!(canonical.prefix_errors().len(), 1);
            let error = &canonical.prefix_errors()[0];
            assert!(matches!(
                error.failure,
                AttributePrefixFailure::Deferred { .. }
            ));
            assert_eq!(
                error.diagnostic_message(),
                format!(
                    "'name_prefix' must be known at plan time to generate 'name', but it depends on {dependency}, which is known only after apply"
                )
            );
            assert_eq!(
                crate::schema::TypeError::AttributePrefix(Box::new(error.clone())).to_string(),
                error.diagnostic_message(),
                "preflight wrappers must retain the same semantic message"
            );
            let owners = canonical.analysis_tree().unwrap();
            assert_eq!(
                owners.resources().count(),
                1,
                "only src can enter admission"
            );
            assert_eq!(owners.rejected_resources().count(), 1);
        }
    }

    #[test]
    fn prefix_read_view_admits_only_through_its_witness() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve("mock.Sink { name_prefix = 'ok' }").unwrap();
        let canonical =
            canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview).judge(&context);
        let read = canonical.parsed();
        assert!(read.resources[0].attributes.contains_key("name"));
        assert!(!read.resources[0].attributes.contains_key("name_prefix"));
        assert!(std::ptr::eq(read.expression_report(), canonical.report()));
        let bindings = read.binding_index(&context);
        crate::validation::deferred_types::with_session_for_test(&context, |session| {
            let mut run = crate::validation::ConversionValidation::<std::convert::Infallible>::new(
                canonical.report(),
            );
            let steps = run
                .admitted_resources(
                    &canonical,
                    &bindings,
                    session,
                    crate::validation::FindingSubject::Root,
                )
                .unwrap();
            let mut count = 0;
            for step in steps {
                let (_, values) = step.into_parts();
                assert_eq!(values.unwrap().unwrap().errors().count(), 0);
                count += 1;
            }
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn prefix_canonicalization_does_not_authorize_unvisited_owners() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        // Keep both declarations in the syntax inventory, but hide one from
        // the canonicalizer to simulate a future, unvisited tree source.
        let mut parsed = crate::parser::parse_and_resolve(
            "mock.Sink { name_prefix = 'ok' }\nmock.Sink { name_prefix = 'raw' }",
        )
        .unwrap();
        let unvisited = parsed.resources.pop().unwrap(); // allow: direct — fixture test inspection
        let location = unvisited.declaration.location().unwrap().clone();
        assert_eq!(location.span().start_line(), 2);
        let mut canonical = canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview);
        canonical.file.resources.push(unvisited);
        let judged = canonical.judge(&context);
        let error = match judged.analysis_tree() {
            Err(error) => error,
            Ok(_) => panic!("unvisited owner was silently accepted"),
        };
        assert!(matches!(
            error.kind(),
            crate::parser::AnalysisTreeErrorKind::UncanonicalizedResource {
                declaration: Some(_),
                ..
            }
        ));
        match error.diagnostic() {
            crate::parser::AnalysisTreeDiagnostic::Located(finding) => {
                assert_eq!(finding.location(), &location);
                assert_eq!(
                    finding.to_string(),
                    format!("{}:2:1: {error}", location.file().path().display())
                );
                assert_eq!(error.diagnostic().to_string(), finding.to_string());
            }
            crate::parser::AnalysisTreeDiagnostic::File(_)
            | crate::parser::AnalysisTreeDiagnostic::Directory(_) => {
                panic!("the unvisited owner's declaration must anchor its diagnostic")
            }
        }
        let error = match judged.schema_validation_resources() {
            Err(error) => error,
            Ok(_) => panic!("unvisited owner received canonical values"),
        };
        assert!(matches!(
            error.kind(),
            crate::parser::AnalysisTreeErrorKind::UncanonicalizedResource {
                declaration: Some(_),
                ..
            }
        ));
        match error.diagnostic() {
            crate::parser::AnalysisTreeDiagnostic::Located(finding) => {
                assert_eq!(finding.location(), &location);
            }
            crate::parser::AnalysisTreeDiagnostic::File(_)
            | crate::parser::AnalysisTreeDiagnostic::Directory(_) => {
                panic!("schema input projection must retain the owner's diagnostic location")
            }
        }
    }

    #[test]
    fn prefix_preflight_rejections_are_typed_and_located() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("arguments.crn"),
            "arguments { enabled: Bool }\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("main.crn"),
            "let r = if enabled {\n  mock.Sink { name_prefix = '' }\n}\n",
        )
        .unwrap();
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let loaded = crate::module_resolver::load_module_with_diagnostics_and_schemas(
            &path, &context, &schemas,
        )
        .unwrap();
        let template = loaded.preflight().judge(&schemas, &context);
        assert_eq!(
            template
                .judged()
                .analysis_tree()
                .unwrap()
                .resources()
                .count(),
            0
        );
        let bindings = template.judged().parsed().binding_index(&context);
        let mut run = crate::validation::ConversionValidation::<std::convert::Infallible>::new(
            template.judged().report(),
        );
        crate::validation::resource_reference_preflight_findings_in(
            &mut run,
            &template,
            &bindings,
            &mut Vec::new(),
            crate::validation::FindingSubject::Root,
        )
        .unwrap();
        let findings = run.complete().into_presented();
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = match findings.iter().next().unwrap() {
            crate::validation::PresentedFinding::Ordinary(
                crate::validation::OrdinaryFinding::Resource(finding),
            ) => finding,
            other => panic!("a typed resource rejection is required: {other:?}"),
        };
        assert!(matches!(
            finding.type_error(),
            Some(crate::schema::TypeError::AttributePrefix(_))
        ));
        let location = finding.location().expect("authored rejection location");
        assert_eq!(location.file().path(), directory.path().join("main.crn"));
        assert_eq!(location.span().start_line(), 2);
    }

    #[test]
    fn prefix_canonicalization_uses_one_borrowed_catalog() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve("mock.Sink { name_prefix = 'ok' }").unwrap();
        let canonical = canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview);
        assert!(
            std::ptr::eq(canonical.schemas(), &schemas),
            "the witness must borrow the caller's catalog"
        );
        let judged = canonical.judge(&context);
        assert!(std::ptr::eq(judged.schemas(), &schemas));
        assert!(
            std::ptr::eq(judged.file.schemas(), &schemas),
            "the inner Judge uses that same catalog"
        );
        let tree = judged.analysis_tree().unwrap();
        let owner = tree.resources().next().unwrap();
        assert!(std::ptr::eq(owner.schemas(), &schemas));
        let expected = schemas.get("mock", "Sink", SchemaKind::Resource).unwrap();
        assert!(std::ptr::eq(
            owner.resource_schema().schema(),
            expected.schema()
        ));
    }

    #[test]
    fn prefix_projection_second_file_keeps_declaration_address() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("a.crn"),
            "let input = 'second'\nlet first = mock.Sink { name = 'first' }\n",
        )
        .unwrap();
        let source = "let second = mock.Sink { name = upper(input) }\n";
        let path = directory.path().join("z.crn");
        std::fs::write(&path, source).unwrap();
        let root = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let merged = crate::config_loader::parse_directory(&root, &context)
            .unwrap()
            .into_parsed();
        let canonical =
            canonicalize_file_prefixes(merged, &schemas, SuffixSource::Preview).judge(&context);
        let owners = canonical.analysis_tree().unwrap();
        let owner = owners
            .resources()
            .find(|resource| {
                resource
                    .binding_address()
                    .is_some_and(|binding| binding.as_str() == "second")
            })
            .unwrap();
        let file = crate::parser::parse_file(source, &path, &context).unwrap();
        let tree = AnalysisTree::try_from_file(&file).unwrap();
        let per_file = tree.project_resources().next().unwrap();
        assert!(
            owner
                .authored_instance_key()
                .unwrap()
                .same_source_instance(&per_file.authored_instance_key().unwrap()),
            "declaration addresses must be local to each source file"
        );
    }

    #[test]
    fn prefix_projection_preserves_named_loop_instances() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve(
            "let sinks = for item in ['a', 'b'] { mock.Sink { name = item } }\n",
        )
        .unwrap();
        let canonical = canonicalize_file_prefixes(parsed.clone(), &schemas, SuffixSource::Preview)
            .judge(&context);
        let owners = canonical.analysis_tree().unwrap();
        let source = AnalysisTree::try_from_file(&parsed).unwrap();
        assert_eq!(owners.resources().count(), 2);
        for owner in owners.resources() {
            let projected = match owner.project_values(&source) {
                ResourceProjection::Matched(projection) => projection.into_values(),
                ResourceProjection::NoMatch | ResourceProjection::Ambiguous => {
                    panic!("a materialized element has one per-file source")
                }
            };
            assert_eq!(
                projected["name"],
                owner.values()["name"],
                "source matching must preserve the element's own value"
            );
        }
    }

    #[test]
    fn prefix_projection_reports_missing_source() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve("mock.Sink { name = 'folded' }").unwrap();
        let canonical =
            canonicalize_file_prefixes(parsed, &schemas, SuffixSource::Preview).judge(&context);
        let owners = canonical.analysis_tree().unwrap();
        let owner = owners.resources().next().unwrap();
        let source = crate::parser::parse_file(
            "mock.Sink { name = 'foreign' }",
            std::path::Path::new("other.crn"),
            &context,
        )
        .unwrap();
        let source = AnalysisTree::try_from_file(&source).unwrap();
        assert!(matches!(
            owner.project_values(&source),
            ResourceProjection::NoMatch
        ));
    }

    #[test]
    fn prefix_projection_reports_ambiguous_source() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve("mock.Sink { name = 'folded' }").unwrap();
        let canonical = canonicalize_file_prefixes(parsed.clone(), &schemas, SuffixSource::Preview)
            .judge(&context);
        let owners = canonical.analysis_tree().unwrap();
        let owner = owners.resources().next().unwrap();
        let mut source = parsed;
        let duplicate = source.resources[0].clone();
        source.resources.push(duplicate); // allow: direct — fixture test inspection
        let source = AnalysisTree::try_from_file(&source).unwrap();
        assert!(
            matches!(owner.project_values(&source), ResourceProjection::Ambiguous),
            "two source occurrences cannot be reduced to the first"
        );
    }

    #[test]
    fn prefix_projection_uses_unique_deferred_template() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("values.crn"),
            "let items = ['a', 'b']\n",
        )
        .unwrap();
        let source = "let sinks = for item in items { mock.Sink { name = item } }\n";
        let path = directory.path().join("main.crn");
        std::fs::write(&path, source).unwrap();
        let root = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let merged = crate::config_loader::parse_directory(&root, &context)
            .unwrap()
            .into_parsed();
        let canonical =
            canonicalize_file_prefixes(merged, &schemas, SuffixSource::Preview).judge(&context);
        let owners = canonical.analysis_tree().unwrap();
        assert_eq!(owners.resources().count(), 2);
        let source = crate::parser::parse_file(source, &path, &context).unwrap();
        let source = AnalysisTree::try_from_file(&source).unwrap();
        let template = source.project_resources().next().unwrap();
        assert_eq!(
            template.kind(),
            crate::parser::AnalysisItemKind::DeferredResource
        );
        for owner in owners.resources() {
            let projected = match owner.project_values(&source) {
                ResourceProjection::Matched(projection) => projection.into_values(),
                ResourceProjection::NoMatch | ResourceProjection::Ambiguous => {
                    panic!("a unique deferred declaration supplies the per-file value")
                }
            };
            assert_eq!(projected["name"], template.attributes()["name"]);
            assert_ne!(projected["name"], owner.values()["name"]);
        }
    }

    #[test]
    fn prefix_canonicalization_preview_retains_authored_reference_slot() {
        let schemas = schemas();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_and_resolve(
            "let p = 'lit'\nlet sink = mock.Sink { name_prefix = p\n other = 'resolved' }\n",
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
        let judged = canonical.judge(&context);
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
        let reference = Value::resource_ref(
            crate::resource::RelativeBindingName::new("source"),
            "name",
            vec![],
        );
        let source = crate::parser::parse_file(
            "let sink = mock.Sink { name_prefix = 'raw'\n name = 'replacement'\n other = source.name\n foreign = 'not an owner key' }",
            owner.location().file().path(),
            &context,
        ).unwrap();
        let source = AnalysisTree::try_from_file(&source).unwrap();
        let projected = match owner.project_values(&source) {
            ResourceProjection::Matched(projection) => projection.into_values(),
            ResourceProjection::NoMatch | ResourceProjection::Ambiguous => {
                panic!("per-file source must match")
            }
        };
        assert_eq!(projected["name"], owner.attributes()["name"]);
        assert_eq!(projected["other"], reference);
        assert_eq!(projected.len(), 2);
        assert!(!projected.contains_key("name_prefix"));
        assert!(!projected.contains_key("foreign"));
        let again = canonical.judge(&context);
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
        let cli = cli.judge(&context);
        let lsp = lsp.judge(&context);
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
        let judged = canonical.judge(&context);
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
        let judged = canonical.judge(&context);
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
