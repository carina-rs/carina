//! Parser-owned locations and the sealed, read-only view used by analysis.

use std::path::Path;

use super::{
    DeferredForPhaseTraversal, File, ParsedDeferredForPhase, PreprocessedFile, ResourceRef, Rule,
    SourceFileId, SourceSpan, WaitBinding,
};
use crate::resource::{
    BindingAddress, Directives, ModuleSource, ParserPlaceholder, RelativeBindingName, ResourceId,
    Value, ValueKind,
};

/// The source file and span of a declaration, minted only by the parser.
///
/// The fields cannot be assembled by consumers:
///
/// ```compile_fail,E0451
/// use carina_core::parser::{DeclarationLocation, SourceFileId, SourceSpan};
/// fn fabricate(file: SourceFileId, span: SourceSpan) -> DeclarationLocation {
///     DeclarationLocation { file, span }
/// }
/// ```
///
/// Even the parser's syntax-based constructor is inaccessible outside it:
///
/// ```compile_fail,E0624
/// use carina_core::parser::DeclarationLocation;
/// fn fabricate() { let _ = DeclarationLocation::from_pair; }
/// ```
///
/// There is no constructor from offsets, line numbers, or a file/span tuple:
///
/// ```compile_fail,E0599
/// use carina_core::parser::DeclarationLocation;
/// let _ = DeclarationLocation::from_offsets("sibling.crn", 0, 12);
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::parser::DeclarationLocation;
/// let _ = DeclarationLocation::from_line("sibling.crn", 3);
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::parser::{DeclarationLocation, SourceFileId, SourceSpan};
/// fn fabricate(file: SourceFileId, span: SourceSpan) -> DeclarationLocation {
///     (file, span).into()
/// }
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::parser::DeclarationLocation;
/// let _ = DeclarationLocation::default();
/// ```
///
/// Deserialization cannot mint parser provenance either:
///
/// ```compile_fail,E0277
/// use carina_core::parser::DeclarationLocation;
/// let _: DeclarationLocation = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclarationLocation {
    file: SourceFileId,
    span: SourceSpan,
}

impl DeclarationLocation {
    pub(in crate::parser) fn from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source_file: &Path,
    ) -> Self {
        assert!(matches!(
            pair.as_rule(),
            Rule::resource_expr
                | Rule::anonymous_resource
                | Rule::read_resource_expr
                | Rule::wait_expr
                | Rule::module_call
        ));
        Self {
            file: SourceFileId::new(source_file.to_path_buf()),
            span: SourceSpan::from_pest(pair, file),
        }
    }

    pub fn file(&self) -> &SourceFileId {
        &self.file
    }

    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

/// Declaration metadata on runtime nodes. Programmatic and deserialized nodes
/// may lack it; [`AnalysisTree::try_from_file`] rejects those nodes. This field
/// is skipped by each owning node's serializer and never affects semantics.
#[derive(Debug, Clone, Default)]
pub struct DeclarationProvenance(Option<DeclarationLocation>);

impl DeclarationProvenance {
    pub(crate) fn from_location(location: DeclarationLocation) -> Self {
        Self(Some(location))
    }

    pub(crate) fn location(&self) -> Option<&DeclarationLocation> {
        self.0.as_ref()
    }
}

impl PartialEq for DeclarationProvenance {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for DeclarationProvenance {}

impl std::hash::Hash for DeclarationProvenance {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

/// An analysis finding with its mandatory parser-owned location.
///
/// ```
/// use carina_core::parser::{DeclarationLocation, Located};
/// fn report(location: DeclarationLocation) -> Located<&'static str> {
///     Located::new(location, "invalid dependency")
/// }
/// ```
///
/// A location cannot be omitted or supplied later:
///
/// ```compile_fail,E0061
/// use carina_core::parser::Located;
/// let _ = Located::new("invalid dependency");
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::parser::Located;
/// let _ = Located::new(None, "invalid dependency");
/// ```
///
/// ```compile_fail,E0451
/// use carina_core::parser::{DeclarationLocation, Located};
/// fn report(location: DeclarationLocation) {
///     let _ = Located { location, detail: "invalid dependency" };
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Located<D> {
    location: DeclarationLocation,
    detail: D,
}

impl<D> Located<D> {
    pub fn new(location: DeclarationLocation, detail: D) -> Self {
        Self { location, detail }
    }

    pub fn location(&self) -> &DeclarationLocation {
        &self.location
    }

    pub fn detail(&self) -> &D {
        &self.detail
    }

    pub fn into_parts(self) -> (DeclarationLocation, D) {
        (self.location, self.detail)
    }
}

/// The declaration kind of a resource in the analysis view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisItemKind {
    Resource,
    DataSource,
    Composition,
    DeferredResource,
    Wait,
}

/// Invariant failure at the boundary between parsing/expansion and analysis.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalysisTreeError {
    #[error("missing declaration provenance for {kind:?} {id}")]
    MissingResourceProvenance {
        kind: AnalysisItemKind,
        id: ResourceId,
    },
    #[error("missing declaration provenance for wait {binding}")]
    MissingWaitProvenance { binding: String },
}

/// A sealed resource view: every item has a location, independent of its id.
///
/// ```compile_fail,E0451
/// use carina_core::parser::{AnalysisResourceRef, DeclarationLocation, ResourceRef};
/// fn fabricate<'a>(resource: ResourceRef<'a>, location: &'a DeclarationLocation) {
///     let _ = AnalysisResourceRef { resource, location };
/// }
/// ```
///
/// The optional runtime metadata is inaccessible through this view:
///
/// ```compile_fail,E0616
/// use carina_core::parser::AnalysisResourceRef;
/// fn bypass(item: &AnalysisResourceRef<'_>) { let _ = &item.resource; }
/// ```
pub struct AnalysisResourceRef<
    'a,
    P: ValueKind = ParserPlaceholder,
    T: ValueKind = P,
    D = ParsedDeferredForPhase<P, RelativeBindingName>,
    B: BindingAddress = RelativeBindingName,
> where
    D: DeferredForPhaseTraversal<P, T, B> + 'a,
{
    resource: ResourceRef<'a, ModuleSource, P, T, D, B>,
    location: &'a DeclarationLocation,
}

impl<'a, P, T, D, B> AnalysisResourceRef<'a, P, T, D, B>
where
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    pub fn location(&self) -> &'a DeclarationLocation {
        self.location
    }

    pub fn id(&self) -> &'a ResourceId {
        self.resource.id()
    }

    pub fn binding(&self) -> Option<&'a str> {
        self.resource.binding()
    }

    pub fn kind(&self) -> AnalysisItemKind {
        match self.resource {
            ResourceRef::Resource(_) => AnalysisItemKind::Resource,
            ResourceRef::DataSource(_) => AnalysisItemKind::DataSource,
            ResourceRef::LiveComposition(_) => AnalysisItemKind::Composition,
            ResourceRef::Deferred { .. } => AnalysisItemKind::DeferredResource,
        }
    }

    pub fn dependency_bindings(&self) -> &'a std::collections::BTreeSet<B> {
        match self.resource {
            ResourceRef::Resource(resource) => &resource.dependencies.value_bindings,
            ResourceRef::DataSource(source) => &source.dependencies.value_bindings,
            ResourceRef::LiveComposition(composition) => &composition.dependency_bindings,
            ResourceRef::Deferred { resource, .. } => {
                &resource.as_resource().dependencies.value_bindings
            }
        }
    }

    pub fn deferred_iterable(&self) -> Option<&'a Value<P, B>> {
        match self.resource {
            ResourceRef::Deferred { deferred, .. } => Some(D::deferred_iterable(deferred)),
            ResourceRef::Resource(_)
            | ResourceRef::DataSource(_)
            | ResourceRef::LiveComposition(_) => None,
        }
    }
}

impl<'a, P, D, B> AnalysisResourceRef<'a, P, P, D, B>
where
    P: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, P, B>,
{
    pub fn attributes(&self) -> std::borrow::Cow<'a, indexmap::IndexMap<String, Value<P, B>>> {
        self.resource.attributes()
    }

    pub fn directives(&self) -> Option<&'a Directives<B, P>> {
        self.resource.directives()
    }
}

/// A sealed wait view with mandatory declaration provenance.
pub struct AnalysisWaitRef<'a, B: BindingAddress = RelativeBindingName> {
    wait: &'a WaitBinding<B>,
    location: &'a DeclarationLocation,
}

impl<'a, B: BindingAddress> AnalysisWaitRef<'a, B> {
    pub fn location(&self) -> &'a DeclarationLocation {
        self.location
    }

    pub fn binding(&self) -> &'a B {
        &self.wait.binding
    }

    pub fn target(&self) -> &'a B {
        &self.wait.target
    }

    pub fn until_predicate(&self) -> &'a super::UntilPredicateAst<B> {
        &self.wait.until_predicate
    }

    pub fn depends_on(&self) -> &'a super::UnexpandedDependencies<B> {
        &self.wait.depends_on
    }

    pub fn timeout_secs(&self) -> Option<u64> {
        self.wait.timeout_secs
    }
}

/// A sealed analysis snapshot. Construction validates declaration provenance at
/// the parse/expansion boundary; producers cannot accept a raw tree instead.
///
/// ```compile_fail,E0451
/// use carina_core::parser::AnalysisTree;
/// let _: AnalysisTree<'_> = AnalysisTree { resources: Vec::new(), waits: Vec::new() };
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::parser::{AnalysisTree, ParsedFile};
/// fn analyze(_: &AnalysisTree<'_>) {}
/// analyze(&ParsedFile::default());
/// ```
pub struct AnalysisTree<
    'a,
    P: ValueKind = ParserPlaceholder,
    T: ValueKind = P,
    D = ParsedDeferredForPhase<P, RelativeBindingName>,
    B: BindingAddress = RelativeBindingName,
> where
    D: DeferredForPhaseTraversal<P, T, B> + 'a,
{
    resources: Vec<AnalysisResourceRef<'a, P, T, D, B>>,
    waits: Vec<AnalysisWaitRef<'a, B>>,
}

impl<'a, P, T, D, B> AnalysisTree<'a, P, T, D, B>
where
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    pub fn try_from_file<E>(file: &'a File<E, P, T, D, B>) -> Result<Self, AnalysisTreeError> {
        let resource_refs = file
            .resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(file.compositions.iter().map(ResourceRef::LiveComposition))
            .chain(file.data_sources.iter().map(ResourceRef::DataSource))
            .chain(file.deferred_for_phase.analysis_resource_refs())
            .chain(file.deferred_module_ifs.iter().flat_map(|deferred| {
                deferred.branches().filter_map(|branch| match branch {
                    super::DeferredIfBody::Resource(resource) => {
                        Some(ResourceRef::Resource(resource))
                    }
                    super::DeferredIfBody::DataSource(source) => {
                        Some(ResourceRef::DataSource(source))
                    }
                    // Calls already have mandatory parser-owned locations by type.
                    super::DeferredIfBody::ModuleCall(_) => None,
                })
            }));
        let resources = resource_refs
            .map(|resource| {
                let (provenance, kind) = match resource {
                    ResourceRef::Resource(item) => (&item.declaration, AnalysisItemKind::Resource),
                    ResourceRef::DataSource(item) => {
                        (&item.declaration, AnalysisItemKind::DataSource)
                    }
                    ResourceRef::LiveComposition(item) => {
                        (&item.declaration, AnalysisItemKind::Composition)
                    }
                    ResourceRef::Deferred { resource, .. } => (
                        &resource.as_resource().declaration,
                        AnalysisItemKind::DeferredResource,
                    ),
                };
                let location = provenance.location().ok_or_else(|| {
                    AnalysisTreeError::MissingResourceProvenance {
                        kind,
                        id: resource.id().clone(),
                    }
                })?;
                Ok(AnalysisResourceRef { resource, location })
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        let waits = file
            .wait_bindings
            .iter()
            .map(|wait| {
                let location = wait.declaration.location().ok_or_else(|| {
                    AnalysisTreeError::MissingWaitProvenance {
                        binding: wait.binding.as_str().to_owned(),
                    }
                })?;
                Ok(AnalysisWaitRef { wait, location })
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        Ok(Self { resources, waits })
    }

    pub fn resources(&self) -> impl Iterator<Item = &AnalysisResourceRef<'a, P, T, D, B>> {
        self.resources.iter()
    }

    pub fn waits(&self) -> impl Iterator<Item = &AnalysisWaitRef<'a, B>> {
        self.waits.iter()
    }
}

impl<'a, E, P, T, D, B> TryFrom<&'a File<E, P, T, D, B>> for AnalysisTree<'a, P, T, D, B>
where
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    type Error = AnalysisTreeError;

    fn try_from(file: &'a File<E, P, T, D, B>) -> Result<Self, Self::Error> {
        Self::try_from_file(file)
    }
}
