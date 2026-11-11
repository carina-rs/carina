//! Parser-owned locations and the sealed, read-only view used by analysis.

use std::{path::Path, sync::Arc};

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
/// ```compile_fail,E0624
/// use carina_core::parser::DeclarationLocation;
/// fn fabricate() { let _ = DeclarationLocation::block_header_from_pair; }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::parser::DeclarationLocation;
/// fn fabricate() { let _ = DeclarationLocation::reference_prefix_from_pair; }
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeclarationLocation {
    file: SourceFileId,
    span: SourceSpan,
}

impl DeclarationLocation {
    pub(in crate::parser) fn expression_node(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source_file: &Path,
    ) -> Self {
        Self {
            file: SourceFileId::new(source_file.to_path_buf()),
            span: SourceSpan::from_pest(pair, file),
        }
    }

    pub(in crate::parser) fn from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source_file: &Path,
    ) -> Self {
        assert!(matches!(
            pair.as_rule(),
            Rule::interpolation
                | Rule::let_binding
                | Rule::resource_expr
                | Rule::anonymous_resource
                | Rule::read_resource_expr
                | Rule::wait_expr
                | Rule::module_call
                | Rule::identifier
                | Rule::discard_pattern
                | Rule::string
                | Rule::expression
                | Rule::for_iterable
                | Rule::namespaced_id
                | Rule::reference_prefix
                | Rule::reference_field
                | Rule::variable_ref
                | Rule::postfix_expr
                | Rule::subscripted_id
                | Rule::validate_additive_expr
                | Rule::validate_multiplicative_expr
                | Rule::validate_primary
                | Rule::upstream_state_expr
                | Rule::primary
                | Rule::type_expr
                | Rule::attributes_param
                | Rule::exports_param
                | Rule::validate_expr
                | Rule::for_expr
                | Rule::if_expr
                | Rule::resource_address
                | Rule::function_call
                | Rule::nested_block
                | Rule::attribute
                | Rule::map_entry
                | Rule::module_call_arg
                | Rule::add_op
                | Rule::multiplicative_op
        ));
        Self {
            file: SourceFileId::new(source_file.to_path_buf()),
            span: SourceSpan::from_pest(pair, file),
        }
    }

    pub(in crate::parser) fn reference_prefix_from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        Self {
            file: SourceFileId::new(source.to_path_buf()),
            span: SourceSpan::from_reference_prefix(pair, file),
        }
    }

    pub(in crate::parser) fn block_header_from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        Self {
            file: SourceFileId::new(source.to_path_buf()),
            span: SourceSpan::from_block_header(pair, file),
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
pub struct DeclarationProvenance {
    location: Option<DeclarationLocation>,
    elements: Arc<super::elements::ElementLocations>,
}

/// One immutable index of parser-owned structured paths, shared by all
/// findings and analysis cursors borrowing this resource's provenance.
#[derive(Debug, Default)]
pub(super) struct AttributeLocationIndex {
    root: AttributeLocationNode,
}

#[derive(Debug, Default)]
struct AttributeLocationNode {
    fields: std::collections::HashMap<String, AttributeLocationNode>,
    indices: std::collections::HashMap<usize, AttributeLocationNode>,
    keys: Vec<usize>,
    first_block: Option<usize>,
    last_value: Option<usize>,
    replaceable: Option<usize>,
    list_literal: bool,
    contains_value: bool,
}

impl AttributeLocationIndex {
    pub(super) fn build(elements: &super::elements::ElementLocations) -> Self {
        let mut result = Self::default();
        for (index, key) in elements.keys().iter().enumerate() {
            let node = result.insert(key.path(), false);
            node.keys.push(index);
            if key.is_block() {
                node.first_block.get_or_insert(index);
            }
        }
        for (index, value) in elements.attribute_values().iter().enumerate() {
            let node = result.insert(&value.path, true);
            node.last_value = Some(index);
            if value.replaceable {
                node.replaceable = Some(index);
            }
            node.list_literal |= value.list_literal;
        }
        result
    }

    fn insert(
        &mut self,
        path: &[super::AttributePathSegment],
        value: bool,
    ) -> &mut AttributeLocationNode {
        let mut node = &mut self.root;
        node.contains_value |= value;
        for segment in path {
            #[cfg(test)]
            record_attribute_location_work(1);
            node = match segment {
                super::AttributePathSegment::Attribute(name)
                | super::AttributePathSegment::StructField(name)
                | super::AttributePathSegment::Key(name) => {
                    node.fields.entry(name.clone()).or_default()
                }
                super::AttributePathSegment::ListIndex(index) => {
                    node.indices.entry(*index).or_default()
                }
            };
            node.contains_value |= value;
        }
        node
    }

    fn attribute(&self, name: &str) -> Option<&AttributeLocationNode> {
        #[cfg(test)]
        record_attribute_location_work(1);
        self.root.fields.get(name)
    }

    fn at(&self, name: &str, path: &crate::schema::FieldPath) -> Option<&AttributeLocationNode> {
        let mut node = self.attribute(name)?;
        for step in path.steps() {
            node = node.child(step)?;
        }
        Some(node)
    }
}

impl AttributeLocationNode {
    fn child(&self, step: &crate::schema::FieldPathStep) -> Option<&Self> {
        #[cfg(test)]
        record_attribute_location_work(1);
        match step {
            crate::schema::FieldPathStep::Field(name) => self.fields.get(name),
            crate::schema::FieldPathStep::Index(index) => self.indices.get(index),
            crate::schema::FieldPathStep::Redacted(_) => None,
        }
    }
    fn key<'a>(
        &self,
        elements: &'a super::elements::ElementLocations,
    ) -> Option<&'a super::AttributeKeyLocation> {
        self.keys.last().map(|index| &elements.keys()[*index])
    }
    fn value<'a>(
        &self,
        elements: &'a super::elements::ElementLocations,
    ) -> Option<&'a super::elements::AttributeValueLocation> {
        self.last_value
            .map(|index| &elements.attribute_values()[index])
    }
}

struct AttributeDiagnosticLocations<'a> {
    elements: &'a super::elements::ElementLocations,
    root: &'a DeclarationLocation,
}

impl<'a> AttributeDiagnosticLocations<'a> {
    /// Computed containers have no authored child. Walk the indexed ancestors
    /// once and use the first computed expression's original parser location.
    fn computed_parent_location(
        &self,
        name: &str,
        path: &crate::schema::FieldPath,
    ) -> Option<&'a DeclarationLocation> {
        let mut node = self.elements.diagnostic_index().attribute(name);
        let mut containing = self.root;
        for (depth, step) in path.steps().iter().enumerate() {
            match node.and_then(|node| node.value(self.elements)) {
                Some(parent) if !parent.literal_container => {
                    return Some(if depth == 0 {
                        self.root
                    } else {
                        &parent.anchor
                    });
                }
                None if node.is_none_or(|node| node.first_block.is_none()) => {
                    return Some(containing);
                }
                Some(parent) if depth > 0 => {
                    containing = &parent.location;
                }
                Some(_) | None => {}
            }
            node = node.and_then(|node| node.child(step));
            if node.is_none() {
                return Some(containing);
            }
        }
        None
    }

    fn attribute_value_location(
        &self,
        name: &str,
        path: &crate::schema::FieldPath,
    ) -> &'a DeclarationLocation {
        if let Some(parent) = self.computed_parent_location(name, path) {
            return parent;
        }
        let mut nodes = Vec::new();
        let mut current = self.elements.diagnostic_index().attribute(name);
        if let Some(node) = current {
            nodes.push(node);
        }
        for step in path.steps() {
            current = current.and_then(|node| node.child(step));
            if let Some(node) = current {
                nodes.push(node);
            }
        }
        nodes
            .into_iter()
            .rev()
            .find_map(|node| node.value(self.elements))
            .map_or(self.root, |value| &value.location)
    }

    fn attribute_path_location(
        &self,
        name: &str,
        path: &crate::schema::FieldPath,
    ) -> &'a DeclarationLocation {
        if let Some(parent) = self.computed_parent_location(name, path) {
            return parent;
        }
        let Some(node) = self.elements.diagnostic_index().at(name, path) else {
            return self.root;
        };
        if let Some(key) = node.key(self.elements) {
            key.location()
        } else if let Some(value) = node.value(self.elements) {
            &value.anchor
        } else {
            self.root
        }
    }
}

impl DeclarationProvenance {
    /// Canonical lookup paths retain their original parser-owned spans after
    /// schema block aliases are resolved. Programmatic resources have no span.
    pub(crate) fn attribute_diagnostic_location(
        &self,
        path: &crate::schema::InputDiagnosticPath,
    ) -> Option<&DeclarationLocation> {
        let elements = self.elements();
        let root = elements
            .diagnostic_index()
            .attribute(path.root())
            .and_then(|node| node.key(elements));
        let Some(root) = root else {
            return elements
                .get(super::elements::ElementKind::ResourceType)
                .or(self.location());
        };
        let relative = path.relative();
        if relative.is_empty() {
            return Some(root.location());
        }
        Some(
            AttributeDiagnosticLocations {
                elements,
                root: root.location(),
            }
            .attribute_value_location(path.root(), &relative),
        )
    }

    pub(crate) fn for_composition(
        location: DeclarationLocation,
        declarations: &[Located<super::DeclarationRecord>],
    ) -> Self {
        Self {
            location: Some(location),
            elements: Arc::new(super::elements::ElementLocations::from_module_attributes(
                declarations,
            )),
        }
    }

    pub(in crate::parser) fn from_pair(
        pair: &pest::iterators::Pair<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        Self {
            location: Some(DeclarationLocation::from_pair(pair, file, source)),
            elements: Arc::new(super::elements::ElementLocations::from_pair(
                pair, file, source,
            )),
        }
    }

    pub(crate) fn elements(&self) -> &super::elements::ElementLocations {
        &self.elements
    }

    /// Follow a semantic attribute rename while retaining every authored span.
    /// This changes lookup paths only; it cannot create diagnostic provenance.
    pub(crate) fn rename_attribute_path(
        &mut self,
        from: &[super::AttributePathSegment],
        name: &str,
    ) {
        self.rename_attribute_paths([(from.to_vec(), name.to_owned())]);
    }

    pub(crate) fn rename_attribute_paths(
        &mut self,
        renames: impl IntoIterator<Item = (Vec<super::AttributePathSegment>, String)>,
    ) {
        let renames = renames.into_iter().collect::<Vec<_>>();
        if !renames.is_empty() {
            Arc::make_mut(&mut self.elements).rename_attribute_paths(renames);
        }
    }

    pub(in crate::parser) fn set_iterable(&mut self, location: DeclarationLocation) {
        Arc::make_mut(&mut self.elements)
            .push_element(super::elements::ElementKind::Iterable, location);
    }

    pub(crate) fn location(&self) -> Option<&DeclarationLocation> {
        self.location.as_ref()
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
/// Locations remain mandatory when consumed:
///
/// ```compile_fail,E0308
/// use carina_core::{parser::{DeclarationLocation, Located}, validation::TypeResolutionDiagnostic};
/// fn location(finding: &Located<TypeResolutionDiagnostic>) {
///     let _: Option<&DeclarationLocation> = finding.location();
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

    pub(in crate::parser) fn detail_mut(&mut self) -> &mut D {
        &mut self.detail
    }

    pub fn into_parts(self) -> (DeclarationLocation, D) {
        (self.location, self.detail)
    }
}

impl<D: std::fmt::Display> std::fmt::Display for Located<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let span = self.location.span();
        write!(
            f,
            "{}:{}:{}: {}",
            self.location.file().path().display(),
            span.start_line(),
            span.start_column(),
            self.detail
        )
    }
}

impl<D: std::error::Error + 'static> std::error::Error for Located<D> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.detail)
    }
}

/// A file-wide finding, or a parse failure for which no declaration exists.
/// Only the parser can attach a file. Never use this for a declaration or element finding.
///
/// ```compile_fail,E0451
/// use carina_core::parser::{FileScoped, SourceFileId};
/// fn fabricate(file: SourceFileId) -> FileScoped<&'static str> { FileScoped { file, detail: "error" } }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::parser::FileScoped;
/// let _ = FileScoped::new(std::path::Path::new("a.crn"), "error");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScoped<D> {
    file: SourceFileId,
    detail: D,
}
impl<D> FileScoped<D> {
    pub(in crate::parser) fn new(file: &Path, detail: D) -> Self {
        Self {
            file: SourceFileId::new(file.to_path_buf()),
            detail,
        }
    }
    pub fn file(&self) -> &SourceFileId {
        &self.file
    }
    pub fn detail(&self) -> &D {
        &self.detail
    }
    pub fn into_detail(self) -> D {
        self.detail
    }
}

/// A directory/graph finding with no responsible declaration or source file.
/// Use only for genuinely directory-wide failures; cycles belong to their members
/// and must be emitted as `Located` findings instead. There is no general
/// constructor: any directory-wide producer must expose a per-kind factory.
///
/// ```compile_fail,E0599
/// use carina_core::parser::DirectoryScoped;
/// let _ = DirectoryScoped::new("missing location");
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::parser::{DirectoryScoped, Located};
/// fn attach(finding: DirectoryScoped<&'static str>) -> Located<&'static str> { finding }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryScoped<D> {
    detail: D,
}
impl DirectoryScoped<super::ParseWarning> {
    pub(crate) fn provider_type_check(detail: super::ParseWarning) -> Self {
        Self { detail }
    }
}
impl<D> DirectoryScoped<D> {
    pub fn detail(&self) -> &D {
        &self.detail
    }
    pub fn into_detail(self) -> D {
        self.detail
    }
}

impl<D> std::ops::Deref for Located<D> {
    type Target = D;
    fn deref(&self) -> &D {
        &self.detail
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
///
/// Consumers cannot manufacture a directory finding from an arbitrary label:
///
/// ```compile_fail,E0451
/// use carina_core::parser::{AnalysisTreeError, AnalysisTreeErrorKind};
/// let _ = AnalysisTreeError {
///     kind: AnalysisTreeErrorKind::MissingSyntaxProvenance { element: "arbitrary".into() },
/// }.diagnostic();
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::parser::{AnalysisTreeError, AnalysisTreeErrorKind};
/// let _ = AnalysisTreeError::new(
///     AnalysisTreeErrorKind::MissingSyntaxProvenance { element: "arbitrary".into() },
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(transparent)]
pub struct AnalysisTreeError {
    kind: AnalysisTreeErrorKind,
}

/// Read-only details of a rejected analysis input. These details alone cannot
/// construct an error or mint a scoped diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalysisTreeErrorKind {
    #[error("{}", crate::resource::INSIDE_SECRET_MESSAGE)]
    InsideSecret,
    #[error("missing parser syntax provenance for {element}")]
    MissingSyntaxProvenance { element: String },
    #[error("missing declaration provenance for {kind:?} {id}")]
    MissingResourceProvenance {
        kind: AnalysisItemKind,
        id: ResourceId,
    },
    #[error("missing declaration provenance for wait {binding}")]
    MissingWaitProvenance { binding: String },
    #[error("missing {element} provenance in {}", declaration.file().path().display())]
    MissingElementProvenance {
        declaration: DeclarationLocation,
        element: String,
    },
}

impl crate::resource::InsideSecret for AnalysisTreeError {
    fn inside_secret(self) -> Self {
        Self::new(AnalysisTreeErrorKind::InsideSecret)
    }
}

pub enum AnalysisTreeDiagnostic<'a> {
    File(FileScoped<&'a AnalysisTreeError>),
    Directory(DirectoryScoped<&'a AnalysisTreeError>),
}

impl AnalysisTreeError {
    pub(crate) fn new(kind: AnalysisTreeErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> &AnalysisTreeErrorKind {
        &self.kind
    }

    pub fn diagnostic(&self) -> AnalysisTreeDiagnostic<'_> {
        match &self.kind {
            AnalysisTreeErrorKind::MissingElementProvenance { declaration, .. } => {
                AnalysisTreeDiagnostic::File(FileScoped::new(declaration.file().path(), self))
            }
            _ => AnalysisTreeDiagnostic::Directory(DirectoryScoped { detail: self }),
        }
    }
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
    in_project_walk: bool,
    location: &'a DeclarationLocation,
    attribute_locations: std::collections::HashMap<String, &'a super::AttributeKeyLocation>,
    elements: &'a super::elements::ElementLocations,
    type_location: &'a DeclarationLocation,
    ignore_changes_location: Option<&'a DeclarationLocation>,
    references: Vec<AnalysisReference<B>>,
    dependencies: Vec<AnalysisDependencyRef<'a, B>>,
    dependency_edges: DependencyEdges<B>,
    provider_location: &'a DeclarationLocation,
    iterable: AnalysisIterableRef<'a, P, B>,
}

enum AnalysisIterableRef<'a, P: ValueKind, B: BindingAddress> {
    Direct,
    Deferred {
        value: &'a Value<P, B>,
        location: &'a DeclarationLocation,
    },
}

/// A reference whose occurrence and prefix provenance were validated at the boundary.
#[derive(Clone)]
pub struct AnalysisReference<B: BindingAddress = RelativeBindingName> {
    path: crate::resource::AccessPath<B>,
    locations: Arc<super::ReferenceLocations>,
    diagnostic_scope: crate::resource::DiagnosticScope,
}

impl<B: BindingAddress> std::fmt::Debug for AnalysisReference<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.diagnostic_scope.is_secret() {
            return formatter.write_str("(secret reference)");
        }
        formatter
            .debug_struct("AnalysisReference")
            .field("path", &self.path)
            .field("locations", &self.locations)
            .finish()
    }
}

/// An authored reference occurrence in a value, with its consumer context.
pub struct AnalysisValueReference<B: BindingAddress = RelativeBindingName> {
    reference: AnalysisReference<B>,
    context: String,
    usage: crate::resource::ResourceRefUsage,
    scope: AnalysisValueScope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnalysisValueScope {
    Project,
    AttributeParameter,
    Variable,
    Function,
    ReadTemplate,
}
impl<B: BindingAddress> AnalysisValueReference<B> {
    pub fn reference(&self) -> &AnalysisReference<B> {
        &self.reference
    }
    pub fn context(&self) -> &str {
        &self.context
    }
    pub fn usage(&self) -> crate::resource::ResourceRefUsage {
        self.usage
    }
    pub fn typed_consumer(&self) -> bool {
        self.scope != AnalysisValueScope::Variable
    }
}

pub(crate) struct FunctionValidationValue<'a, P: ValueKind, B: BindingAddress> {
    pub value: &'a Value<P, B>,
    pub expected: Option<&'a super::TypeExpr>,
    pub context: Option<String>,
    pub check_return: bool,
}

/// A deferred iterable validated together with its parser-owned provenance.
pub struct AnalysisForRef<'a, P: ValueKind, B: BindingAddress> {
    site: super::DeferredForSite<'a, P, B>,
    location: &'a DeclarationLocation,
    references: Vec<AnalysisReference<B>>,
    reference_usages: Vec<crate::resource::ResourceRefUsage>,
    result_references: Vec<AnalysisReference<B>>,
}
impl<'a, P: ValueKind, B: BindingAddress> AnalysisForRef<'a, P, B> {
    pub fn location(&self) -> &'a DeclarationLocation {
        self.location
    }
    pub fn binding(&self) -> &'a super::ForBinding {
        self.site.binding
    }
    pub fn header(&self) -> &'a str {
        self.site.header
    }
    pub fn iterable(&self) -> &'a Value<P, B> {
        self.site.iterable
    }
    pub fn references(&self) -> &[AnalysisReference<B>] {
        &self.references
    }
    pub fn reference_occurrences(
        &self,
    ) -> impl Iterator<Item = (&AnalysisReference<B>, crate::resource::ResourceRefUsage)> {
        self.references
            .iter()
            .zip(self.reference_usages.iter().copied())
    }
    /// References constraining the iterable result, excluding function arguments.
    pub fn result_references(&self) -> &[AnalysisReference<B>] {
        &self.result_references
    }
    pub fn checks_scope(&self) -> bool {
        self.site.checks_scope
    }
}

fn collect_value_references<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    context: &str,
    scope: AnalysisValueScope,
    references: &mut Vec<AnalysisValueReference<B>>,
) -> Result<(), AnalysisTreeError> {
    require_integer_operator_provenance(value, context)?;
    let mut result = Ok(());
    value.visit_resource_refs_for_diagnostics(&mut |reference, usage| match reference
        .analysis_reference()
    {
        Some(reference) => references.push(AnalysisValueReference {
            reference,
            context: context.to_string(),
            usage,
            scope,
        }),
        None => {
            result = Err(AnalysisTreeError::new(
                AnalysisTreeErrorKind::MissingSyntaxProvenance {
                    element: format!(
                        "{context} reference {}",
                        reference.diagnostic_path().to_dot_string()
                    ),
                },
            ))
        }
    });
    result
}

/// Validate operator ownership before any analysis producer can inspect values.
fn require_integer_operator_provenance<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    context: &str,
) -> Result<(), AnalysisTreeError> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart};
    let check = |value: &Value<P, B>| require_integer_operator_provenance(value, context);
    match value {
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            if expression.operator_span().location().is_none() {
                return Err(AnalysisTreeError::new(
                    AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: format!("{context} integer operator"),
                    },
                ));
            }
            check(expression.lhs())?;
            check(expression.rhs())?;
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            for value in values {
                check(value)?;
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for value in values.values() {
                check(value)?;
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => check(access.base())?,
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            for value in deferred.values() {
                check(value)?;
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(value) = part {
                    check(value)?;
                }
            }
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for argument in args {
                check(argument.walker_value())?;
            }
        }
        Value::Deferred(DeferredValue::Secret(value)) => value.try_inspect(check)?,
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            check(value)?;
            check(default)?;
        }
        Value::Concrete(_)
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Placeholder(_)
            | DeferredValue::Unknown(_),
        ) => {}
    }
    Ok(())
}
fn collect_iterable_result_references<P, B>(
    value: &Value<P, B>,
    context: &str,
    references: &mut Vec<AnalysisValueReference<B>>,
) -> Result<(), AnalysisTreeError>
where
    P: ValueKind,
    B: BindingAddress,
{
    use crate::resource::DeferredValue;
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            for value in deferred.values() {
                collect_iterable_result_references(value, context, references)?;
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { .. }) => {
            collect_value_references(value, context, AnalysisValueScope::Project, references)?
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            collect_iterable_result_references(value, context, references)?;
            collect_iterable_result_references(default, context, references)?;
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            collect_iterable_result_references(expression.lhs(), context, references)?;
            collect_iterable_result_references(expression.rhs(), context, references)?;
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let first = references.len();
            inner.try_inspect(|secret_value| {
                collect_iterable_result_references(secret_value, context, references)
            })?;
            for reference in &mut references[first..] {
                reference.reference.diagnostic_scope = crate::resource::DiagnosticScope::Secret;
            }
        }
        // References inside a function argument constrain that argument,
        // not the function result used as the iterable.
        Value::Concrete(_)
        | Value::Deferred(
            DeferredValue::BindingRef { .. }
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => {}
    }
    Ok(())
}

impl<B: BindingAddress> AnalysisReference<B> {
    pub(crate) fn from_scoped(
        path: &crate::resource::AccessPath<B>,
        diagnostic_scope: crate::resource::DiagnosticScope,
    ) -> Option<Self> {
        Some(Self {
            path: path.clone(),
            locations: path.reference_locations_shared()?.clone(),
            diagnostic_scope,
        })
    }

    /// Protect a finding before any frontend formats or stores its payload.
    pub fn diagnostic<E>(&self, error: E) -> crate::resource::SecretDiagnosticError<E> {
        self.diagnostic_scope.protect(error)
    }

    /// Reference lookup errors cross their enclosing intrinsic boundary here.
    pub fn attribute_error(
        &self,
        bindings: &crate::binding_index::BindingIndex<'_, B>,
        usage: crate::resource::ResourceRefUsage,
    ) -> Option<crate::resource::SecretDiagnosticError<crate::binding_index::RefTypeError>> {
        match bindings.ref_type(&self.path) {
            crate::binding_index::RefType::UnknownAttribute(error)
                if !crate::validation::suppress_coalesce_existence_error(&error, usage) =>
            {
                Some(self.diagnostic(error))
            }
            _ => None,
        }
    }

    /// Diagnostic callers receive only the scoped spelling.
    pub fn diagnostic_path(&self) -> crate::resource::DiagnosticPath<B> {
        self.diagnostic_scope
            .path(crate::resource::ReferencePath::Access(self.path.clone()))
    }

    /// Identity comparisons do not expose the matched binding.
    pub fn binding_matches(
        &self,
        matches: impl FnOnce(crate::resource::DependencyKey<B>) -> bool,
    ) -> bool {
        matches(crate::resource::DependencyKey::new(
            self.path.binding_address().clone(),
        ))
    }

    pub fn render<T>(
        &self,
        render: impl FnOnce(&crate::resource::AccessPath<B>) -> T,
    ) -> crate::resource::SecretDiagnosticError<T> {
        if self.diagnostic_scope.is_secret() {
            crate::resource::SecretDiagnosticError::InsideSecret
        } else {
            crate::resource::SecretDiagnosticError::Public(render(&self.path))
        }
    }

    /// Check lookup identity and protect each finding before it leaves this boundary.
    pub(crate) fn collect_diagnostics<E: crate::resource::InsideSecret>(
        &self,
        check: impl FnOnce(&crate::resource::AccessPath<B>, &mut Vec<super::Located<E>>),
    ) -> Vec<super::Located<E>> {
        let mut findings = Vec::new();
        check(&self.path, &mut findings);
        findings
            .into_iter()
            .map(|finding| {
                let (location, error) = finding.into_parts();
                super::Located::new(location, self.diagnostic_scope.protect_error(error))
            })
            .collect()
    }
    pub fn location(&self) -> &DeclarationLocation {
        self.locations.occurrence()
    }
    pub fn prefix_location(&self) -> &DeclarationLocation {
        self.locations.prefix()
    }
    pub fn own_attribute_location(&self) -> &DeclarationLocation {
        self.locations
            .attribute_location(self.path.binding(), self.path.attribute())
    }
    pub fn attribute_location(&self, binding: &str, attribute: &str) -> &DeclarationLocation {
        self.locations.attribute_location(binding, attribute)
    }
}

/// One operand occurrence; order and duplicates are preserved.
pub struct AnalysisDependencyRef<'a, B: BindingAddress = RelativeBindingName> {
    operand: &'a super::UnexpandedDependencyOperand<B>,
    location: &'a DeclarationLocation,
}
impl<'a, B: BindingAddress> AnalysisDependencyRef<'a, B> {
    pub fn operand(&self) -> &'a super::UnexpandedDependencyOperand<B> {
        self.operand
    }
    pub fn location(&self) -> &'a DeclarationLocation {
        self.location
    }
}

fn dependency_refs<'a, B: BindingAddress>(
    dependencies: &'a super::UnexpandedDependencies<B>,
) -> Vec<AnalysisDependencyRef<'a, B>> {
    dependencies
        .located_operands()
        .iter()
        .map(|operand| AnalysisDependencyRef {
            operand: operand.detail(),
            location: operand.location(),
        })
        .collect()
}

fn append_reference_origins<B: BindingAddress>(
    references: &mut Vec<AnalysisReference<B>>,
    dependencies: &super::UnexpandedDependencies<B>,
    declaration: &DeclarationLocation,
) -> Result<(), AnalysisTreeError> {
    for origin in dependencies.reference_origins() {
        let scoped = origin.as_ref();
        let reference = scoped.analysis_reference().ok_or_else(|| {
            AnalysisTreeError::new(AnalysisTreeErrorKind::MissingElementProvenance {
                declaration: declaration.clone(),
                element: format!(
                    "reference edge {}",
                    scoped.diagnostic_path().to_dot_string()
                ),
            })
        })?;
        if let Some(existing) = references.iter_mut().find(|existing| {
            existing.path == reference.path && existing.location() == reference.location()
        }) {
            if scoped.is_secret() {
                existing.diagnostic_scope = crate::resource::DiagnosticScope::Secret;
            }
        } else {
            references.push(reference);
        }
    }
    Ok(())
}

fn required_element(
    provenance: &DeclarationProvenance,
    kind: super::elements::ElementKind,
) -> Result<&DeclarationLocation, AnalysisTreeError> {
    provenance.elements().get(kind).ok_or_else(|| {
        AnalysisTreeError::new(AnalysisTreeErrorKind::MissingElementProvenance {
            declaration: provenance
                .location()
                .expect("declaration checked before elements")
                .clone(),
            element: format!("{kind:?}"),
        })
    })
}

type AttributeProvenance<'a, B> = (
    std::collections::HashMap<String, &'a super::AttributeKeyLocation>,
    Vec<AnalysisReference<B>>,
);

fn attribute_provenance<'a, P: ValueKind, B: BindingAddress>(
    attributes: &indexmap::IndexMap<String, Value<P, B>>,
    provenance: &'a DeclarationProvenance,
    location: &'a DeclarationLocation,
) -> Result<AttributeProvenance<'a, B>, AnalysisTreeError> {
    let mut keys = std::collections::HashMap::new();
    let mut references = Vec::new();
    for (key, value) in attributes {
        let node = provenance.elements().diagnostic_index().attribute(key);
        // Blocks are collected after assignments; otherwise the parser keeps
        // the final authored assignment. Both facts are indexed once.
        let key_location = node
            .and_then(|node| {
                node.first_block
                    .map(|index| &provenance.elements().keys()[index])
                    .or_else(|| node.key(provenance.elements()))
            })
            .ok_or_else(|| {
                AnalysisTreeError::new(AnalysisTreeErrorKind::MissingElementProvenance {
                    declaration: location.clone(),
                    element: format!("attribute key {key}"),
                })
            })?;
        if node.is_none_or(|node| !node.contains_value) {
            return Err(AnalysisTreeError::new(
                AnalysisTreeErrorKind::MissingElementProvenance {
                    declaration: location.clone(),
                    element: format!("attribute value {key}"),
                },
            ));
        }
        keys.insert(key.clone(), key_location);
        let mut result = Ok(());
        value.visit_resource_refs_for_diagnostics(&mut |reference, _| match reference
            .analysis_reference()
        {
            Some(reference) => references.push(reference),
            None => {
                result = Err(AnalysisTreeError::new(
                    AnalysisTreeErrorKind::MissingElementProvenance {
                        declaration: location.clone(),
                        element: format!(
                            "reference {}",
                            reference.diagnostic_path().to_dot_string()
                        ),
                    },
                ))
            }
        });
        result?;
    }
    Ok((keys, references))
}

type DependencyEdges<B> =
    std::collections::HashMap<B, Located<crate::validation::depends_on::DependencyEdgeOrigin>>;

fn managed_dependency_origins<E, P, T, D, B>(
    file: &File<E, P, T, D, B>,
    managed: &crate::resource::Resource<ModuleSource, P, B>,
    dependencies: &[AnalysisDependencyRef<'_, B>],
    references: &[AnalysisReference<B>],
    location: &DeclarationLocation,
) -> Result<DependencyEdges<B>, AnalysisTreeError>
where
    E: super::ExportParamLike<P, B>,
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    use crate::validation::depends_on::DependencyEdgeOrigin;
    let mut dependency_edges = std::collections::HashMap::new();
    // Graph membership comes from the ordering pass's exact
    // dependency computation. Provenance only anchors those
    // edges; retained references never add graph edges.
    for binding in crate::deps::get_resource_dependencies(managed) {
        let explicit = dependencies
            .iter()
            .find(|dependency| dependency.operand().binding_reference() == binding);
        let (origin, edge_location) = if let Some(explicit) = explicit {
            (
                DependencyEdgeOrigin::ExplicitDependency,
                explicit.location(),
            )
        } else if let Some(reference) = references
            .iter()
            .find(|reference| reference.path.binding_address() == &binding)
        {
            (DependencyEdgeOrigin::ValueReference, reference.location())
        } else if let Some(origin) = managed
            .dependencies
            .depends_on
            .binding_reference_origins()
            .iter()
            .find(|origin| origin.detail() == &binding)
        {
            (DependencyEdgeOrigin::ValueReference, origin.location())
        } else if let Some(reference) = references.iter().find(|reference| {
            // Inlining a referenced attribute can introduce a
            // deferred call's bare handle dependencies. Keep
            // the authored reference that performed that inline.
            file.iter_top_level_resources()
                .find(|source| source.binding_address() == Some(reference.path.binding_address()))
                .is_some_and(|source| {
                    source.dependency_bindings().contains(&binding)
                        || source
                            .attributes()
                            .get(reference.path.attribute())
                            .is_some_and(|value| {
                                let mut found = false;
                                value.visit_binding_refs(&mut |target| {
                                    found |= target.matches(&binding)
                                });
                                found
                            })
                })
        }) {
            (DependencyEdgeOrigin::ValueReference, reference.location())
        } else {
            let bare = file
                .bare_binding_uses()
                .records()
                .iter()
                .find(|record| {
                    let owns_resource = match &record.authored.owner {
                        super::BindingUseOwner::Resource(owner) => {
                            owner.resource_id() == &managed.id
                        }
                        super::BindingUseOwner::ModuleArgument { .. } => false,
                    };
                    let references_binding = match &record.source {
                        super::BareBindingSource::Structural(handle) => {
                            handle.canonical_binding_name() == binding.as_str()
                        }
                        super::BareBindingSource::Value { value, .. } => {
                            let mut found = false;
                            value.visit_binding_refs(&mut |name| found |= name.matches(&binding));
                            found
                        }
                    };
                    owns_resource && references_binding
                })
                .ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingElementProvenance {
                        declaration: location.clone(),
                        element: format!("dependency edge to {binding}"),
                    })
                })?;
            (
                DependencyEdgeOrigin::ValueReference,
                bare.authored.location(),
            )
        };
        dependency_edges.insert(binding, Located::new(edge_location.clone(), origin));
    }
    Ok(dependency_edges)
}

impl<'a, P, T, D, B> AnalysisResourceRef<'a, P, T, D, B>
where
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    fn managed_resource(&self) -> Option<&'a crate::resource::Resource<ModuleSource, P, B>> {
        match self.resource {
            ResourceRef::Resource(resource) => Some(resource),
            _ => None,
        }
    }

    pub(crate) fn has_attribute_derived_conflict(
        &self,
        conflict: &crate::identifier::AttributeDerivedAnonymousResourceConflict,
        schemas: &crate::schema::SchemaRegistry,
    ) -> bool
    where
        P: crate::resource::PlaceholderKind,
    {
        self.managed_resource().is_some_and(|resource| {
            resource.binding.is_none()
                && resource.loop_provenance.is_empty()
                && conflict.includes(resource, schemas)
        })
    }

    pub(crate) fn module_identity_locations(&self) -> Vec<&'a DeclarationLocation> {
        match self
            .managed_resource()
            .and_then(|resource| resource.module_source.as_ref())
        {
            Some(ModuleSource::Module(module)) => module
                .module_call_path()
                .calls()
                .map(|call| call.id.declaration_location())
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn authored_declaration_id(&self) -> Option<&'a super::retained::SlotDeclarationId> {
        match self.resource {
            ResourceRef::Resource(resource) => resource.authored_slots.declaration_id(),
            ResourceRef::DataSource(resource) => resource.authored_slots.declaration_id(),
            ResourceRef::Deferred { resource, .. } => {
                resource.as_resource().authored_slots.declaration_id()
            }
            ResourceRef::LiveComposition(_) => None,
        }
    }

    pub(crate) fn projected_phase(&self, name: &str) -> Option<super::retained::ProjectedPhase> {
        match self.resource {
            ResourceRef::Resource(resource) => resource.authored_slots.projected_phase(name),
            ResourceRef::DataSource(resource) => resource.authored_slots.projected_phase(name),
            ResourceRef::Deferred { resource, .. } => {
                resource.as_resource().authored_slots.projected_phase(name)
            }
            ResourceRef::LiveComposition(_) => None,
        }
    }

    pub fn authored_slot(
        &self,
        name: &str,
    ) -> Result<&'a super::retained::JudgedSlot<B>, super::retained::MissingEvidence> {
        match self.resource {
            ResourceRef::Resource(resource) => resource.authored_slots.get(name),
            ResourceRef::DataSource(resource) => resource.authored_slots.get(name),
            ResourceRef::Deferred { resource, .. } => {
                resource.as_resource().authored_slots.get(name)
            }
            ResourceRef::LiveComposition(_) => Err(super::retained::MissingEvidence::new(
                "composition attribute has no authored resource slot",
            )),
        }
    }

    pub fn location(&self) -> &'a DeclarationLocation {
        self.location
    }

    pub fn id(&self) -> &'a ResourceId {
        self.resource.id()
    }

    pub(crate) fn diagnostic_label(&self) -> String {
        match self.resource.context() {
            super::ResourceContext::Direct => self.id().to_string(),
            super::ResourceContext::Deferred(deferred) => {
                format!("for-body `{}` {}", D::deferred_header(deferred), self.id())
            }
        }
    }

    pub fn binding_address(&self) -> Option<&'a B> {
        self.resource.binding_address()
    }
    pub fn dependencies(&self) -> &[AnalysisDependencyRef<'a, B>] {
        &self.dependencies
    }
    pub(crate) fn dependency_edges(
        &self,
    ) -> &std::collections::HashMap<B, Located<crate::validation::depends_on::DependencyEdgeOrigin>>
    {
        &self.dependency_edges
    }
    pub fn references(&self) -> &[AnalysisReference<B>] {
        &self.references
    }
    pub fn attribute_keys(&self) -> &'a [super::AttributeKeyLocation] {
        self.elements.keys()
    }
    pub fn type_location(&self) -> &'a DeclarationLocation {
        self.type_location
    }

    pub fn attribute_key_location(&self, name: &str) -> &'a DeclarationLocation {
        self.attribute_locations[name].location()
    }

    pub fn attribute_assignment_location(&self, name: &str) -> &'a DeclarationLocation {
        self.attribute_locations[name].assignment_location()
    }

    pub fn attribute_block_keys<'s>(
        &'s self,
        name: &'s str,
    ) -> impl Iterator<Item = &'a DeclarationLocation> + 's {
        self.elements
            .diagnostic_index()
            .attribute(name)
            .into_iter()
            .flat_map(|node| node.keys.iter())
            .map(|index| &self.elements.keys()[*index])
            .filter(|key| key.is_block())
            .map(|key| key.location())
    }

    fn diagnostic_locations(&self, name: &str) -> AttributeDiagnosticLocations<'a> {
        AttributeDiagnosticLocations {
            elements: self.elements,
            root: self.attribute_key_location(name),
        }
    }

    pub fn attribute_value_location(
        &self,
        name: &str,
        path: &crate::schema::FieldPath,
    ) -> &'a DeclarationLocation {
        self.diagnostic_locations(name)
            .attribute_value_location(name, path)
    }

    pub fn attribute_path_location(
        &self,
        name: &str,
        path: &crate::schema::FieldPath,
    ) -> &'a DeclarationLocation {
        self.diagnostic_locations(name)
            .attribute_path_location(name, path)
    }

    pub fn replaceable_attribute_value(&self, name: &str) -> Option<&'a DeclarationLocation> {
        self.elements
            .diagnostic_index()
            .attribute(name)
            .and_then(|node| node.replaceable)
            .map(|index| &self.elements.attribute_values()[index].location)
    }

    pub fn is_list_literal(&self, name: &str) -> bool {
        self.elements
            .diagnostic_index()
            .attribute(name)
            .is_some_and(|node| node.list_literal)
    }

    pub fn schema<'s>(
        &self,
        schemas: &'s crate::schema::SchemaRegistry,
    ) -> Option<crate::schema::OwnerBoundResourceSchema<'s>> {
        match self.resource {
            ResourceRef::Resource(resource) => schemas.get_for(resource),
            ResourceRef::Deferred { resource, .. } => schemas.get_for(resource.as_resource()),
            ResourceRef::DataSource(source) => schemas.get_for_data_source(source),
            ResourceRef::LiveComposition(_) => None,
        }
    }

    pub fn quoted_string_attrs(&self) -> &'a std::collections::HashSet<String> {
        self.resource.quoted_string_attrs()
    }
    pub fn provider_location(&self) -> &'a DeclarationLocation {
        self.provider_location
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

    pub fn deferred_iterable(&self) -> Option<Located<&'a Value<P, B>>> {
        match self.iterable {
            AnalysisIterableRef::Direct => None,
            AnalysisIterableRef::Deferred { value, location } => {
                Some(Located::new(location.clone(), value))
            }
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

    pub fn located_attributes(&self) -> Vec<Located<(String, Value<P, B>)>> {
        self.attributes()
            .into_owned()
            .into_iter()
            .map(|(key, value)| {
                Located::new(
                    self.attribute_locations[&key].location().clone(),
                    (key, value),
                )
            })
            .collect()
    }

    pub fn resolved_attributes(&self) -> std::collections::HashMap<String, Value<P, B>> {
        self.resource.resolved_attributes()
    }

    pub fn ignore_changes_findings(
        &self,
        schema: &crate::schema::ValidatedResourceSchema,
    ) -> Vec<Located<crate::resource::IgnoreChangeError>> {
        let resource = match self.resource {
            ResourceRef::Resource(resource) => resource,
            ResourceRef::Deferred { resource, .. } => resource.as_resource(),
            _ => return Vec::new(),
        };
        match crate::resource::resolve_ignore_changes(resource, schema) {
            Ok(_) => Vec::new(),
            Err(errors) => {
                let location = self.ignore_changes_location.expect(
                    "ignore_changes directive provenance validated at the analysis boundary",
                );
                errors
                    .into_iter()
                    .map(|error| Located::new(location.clone(), error))
                    .collect()
            }
        }
    }

    pub fn directives(&self) -> Option<&'a Directives<B, P>> {
        self.resource.directives()
    }
}

/// A sealed wait view with mandatory declaration provenance.
pub struct AnalysisWaitRef<'a, B: BindingAddress = RelativeBindingName> {
    wait: &'a WaitBinding<B>,
    location: &'a DeclarationLocation,
    target_location: &'a DeclarationLocation,
    lhs_location: &'a DeclarationLocation,
    rhs_location: &'a DeclarationLocation,
    dependencies: Vec<AnalysisDependencyRef<'a, B>>,
}

impl<'a, B: BindingAddress> AnalysisWaitRef<'a, B> {
    pub fn target_location(&self) -> &'a DeclarationLocation {
        self.target_location
    }
    pub fn lhs_location(&self) -> &'a DeclarationLocation {
        self.lhs_location
    }
    pub fn rhs_location(&self) -> &'a DeclarationLocation {
        self.rhs_location
    }
    pub fn dependencies(&self) -> &[AnalysisDependencyRef<'a, B>] {
        &self.dependencies
    }
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
/// fn fabricate(tree: AnalysisTree<'_>) {
///     let _ = AnalysisTree { resources: Vec::new(), ..tree };
/// }
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
    E = super::ParsedExportParam<P, B>,
> where
    D: DeferredForPhaseTraversal<P, T, B> + 'a,
{
    file: &'a File<E, P, T, D, B>,
    resources: Vec<AnalysisResourceRef<'a, P, T, D, B>>,
    top_level_resource_count: usize,
    waits: Vec<AnalysisWaitRef<'a, B>>,
    providers: &'a [super::ProviderConfig<P, B>],
    upstream_names: std::collections::HashSet<&'a str>,
    loop_groups: std::collections::HashSet<String>,
    syntax: &'a super::SyntaxInventory<B>,
    type_annotations: Vec<Located<crate::validation::TypeResolutionContext>>,
    provider_attribute_locations: Vec<std::collections::HashMap<String, DeclarationLocation>>,
    upstream_states: Vec<Located<&'a super::UpstreamState<B>>>,
    value_references: Vec<AnalysisValueReference<B>>,
    validation_values: Vec<Located<FunctionValidationValue<'a, P, B>>>,
    iterables: Vec<AnalysisForRef<'a, P, B>>,
    arguments: &'a [super::ArgumentParameter<super::TypeExpr, P, B>],
    functions: &'a std::collections::HashMap<String, super::UserFunction<B>>,
    module_calls: &'a [super::ModuleCall<P, B>],
    binding_names: std::collections::HashSet<String>,
    module_definition: bool,
    root_configuration: bool,
    backend_location: Option<&'a DeclarationLocation>,
}

impl<'a, P, T, D, B, E> AnalysisTree<'a, P, T, D, B, E>
where
    P: ValueKind,
    T: ValueKind,
    B: BindingAddress,
    D: DeferredForPhaseTraversal<P, T, B>,
{
    pub fn try_from_file(file: &'a File<E, P, T, D, B>) -> Result<Self, AnalysisTreeError>
    where
        E: super::ExportParamLike<P, B>,
    {
        let mut value_references = Vec::new();
        let resource_refs = file
            .resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(file.compositions.iter().map(ResourceRef::LiveComposition))
            .chain(file.data_sources.iter().map(ResourceRef::DataSource))
            .map(|resource| (resource, true))
            .chain(
                file.deferred_for_phase
                    .analysis_resource_refs()
                    .map(|resource| {
                        // File::iter_all_resources intentionally omits parser-side
                        // read templates. Other analyses still need their locations.
                        let in_project_walk = !matches!(resource, ResourceRef::DataSource(_));
                        (resource, in_project_walk)
                    }),
            )
            .chain(file.deferred_module_ifs.iter().flat_map(|deferred| {
                deferred
                    .branches()
                    .filter_map(|branch| match branch {
                        super::DeferredIfBody::Resource(resource) => {
                            Some(ResourceRef::Resource(resource))
                        }
                        super::DeferredIfBody::DataSource(source) => {
                            Some(ResourceRef::DataSource(source))
                        }
                        // Calls already have mandatory parser-owned locations by type.
                        super::DeferredIfBody::ModuleCall(_) => None,
                    })
                    .map(|resource| (resource, true))
            }));
        let resources: Vec<AnalysisResourceRef<'a, P, T, D, B>> = resource_refs
            .map(|(resource, in_project_walk)| {
                let value_scope = if in_project_walk {
                    AnalysisValueScope::Project
                } else {
                    AnalysisValueScope::ReadTemplate
                };
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
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingResourceProvenance {
                        kind,
                        id: resource.id().clone(),
                    })
                })?;
                let (attribute_locations, references, dependencies, provider_location) =
                    match resource {
                        ResourceRef::Resource(item) => {
                            for (key, value) in &item.attributes {
                                collect_value_references(
                                    value,
                                    &format!("{} attribute `{key}`", item.id),
                                    value_scope,
                                    &mut value_references,
                                )?;
                            }
                            let (keys, mut refs) =
                                attribute_provenance(&item.attributes, provenance, location)?;
                            append_reference_origins(
                                &mut refs,
                                &item.dependencies.depends_on,
                                location,
                            )?;
                            let provider = if item.directives.provider_instance.is_some() {
                                required_element(
                                    provenance,
                                    super::elements::ElementKind::Provider,
                                )?
                            } else {
                                location
                            };
                            (
                                keys,
                                refs,
                                dependency_refs(&item.dependencies.depends_on),
                                provider,
                            )
                        }
                        ResourceRef::DataSource(item) => {
                            for (key, value) in &item.attributes {
                                collect_value_references(
                                    value,
                                    &format!("{} attribute `{key}`", item.id),
                                    value_scope,
                                    &mut value_references,
                                )?;
                            }
                            let (keys, mut refs) =
                                attribute_provenance(&item.attributes, provenance, location)?;
                            append_reference_origins(
                                &mut refs,
                                &item.dependencies.depends_on,
                                location,
                            )?;
                            let provider = if item.directives.provider_instance.is_some() {
                                required_element(
                                    provenance,
                                    super::elements::ElementKind::Provider,
                                )?
                            } else {
                                location
                            };
                            (
                                keys,
                                refs,
                                dependency_refs(&item.dependencies.depends_on),
                                provider,
                            )
                        }
                        ResourceRef::LiveComposition(item) => {
                            let attrs = item
                                .signature
                                .attributes
                                .iter()
                                .filter_map(|(name, attr)| {
                                    attr.value().map(|value| (name.clone(), value))
                                })
                                .collect();
                            let (keys, refs) = attribute_provenance(&attrs, provenance, location)?;
                            for (key, value) in &attrs {
                                collect_value_references(
                                    value,
                                    &format!("{} attribute `{key}`", item.id),
                                    value_scope,
                                    &mut value_references,
                                )?;
                            }
                            (keys, refs, Vec::new(), location)
                        }
                        ResourceRef::Deferred { resource, deferred } => {
                            let item = resource.as_resource();
                            for (key, value) in &item.attributes {
                                collect_value_references(
                                    value,
                                    &format!(
                                        "for-body `{}` {} attribute `{key}`",
                                        D::deferred_header(deferred),
                                        item.id
                                    ),
                                    value_scope,
                                    &mut value_references,
                                )?;
                            }
                            let (keys, mut refs) =
                                attribute_provenance(&item.attributes, provenance, location)?;
                            append_reference_origins(
                                &mut refs,
                                &item.dependencies.depends_on,
                                location,
                            )?;
                            let provider = if item.directives.provider_instance.is_some() {
                                required_element(
                                    provenance,
                                    super::elements::ElementKind::Provider,
                                )?
                            } else {
                                location
                            };
                            (
                                keys,
                                refs,
                                dependency_refs(&item.dependencies.depends_on),
                                provider,
                            )
                        }
                    };
                let iterable = match resource {
                    ResourceRef::Deferred { deferred, .. } => AnalysisIterableRef::Deferred {
                        value: D::deferred_iterable(deferred),
                        location: required_element(
                            provenance,
                            super::elements::ElementKind::Iterable,
                        )?,
                    },
                    _ => AnalysisIterableRef::Direct,
                };
                let dependency_edges = match resource {
                    ResourceRef::Resource(managed) => managed_dependency_origins(
                        file,
                        managed,
                        &dependencies,
                        &references,
                        location,
                    )?,
                    _ => std::collections::HashMap::new(),
                };
                Ok(AnalysisResourceRef {
                    resource,
                    in_project_walk,
                    location,
                    attribute_locations,
                    references,
                    dependencies,
                    dependency_edges,
                    provider_location,
                    iterable,
                    elements: provenance.elements(),
                    type_location: if matches!(resource, ResourceRef::LiveComposition(_)) {
                        location
                    } else {
                        required_element(provenance, super::elements::ElementKind::ResourceType)?
                    },
                    ignore_changes_location: match resource {
                        ResourceRef::Resource(resource)
                            if !resource.directives.ignore_changes.is_empty() =>
                        {
                            Some(required_element(
                                provenance,
                                super::elements::ElementKind::IgnoreChanges,
                            )?)
                        }
                        ResourceRef::Deferred { resource, .. }
                            if !resource.as_resource().directives.ignore_changes.is_empty() =>
                        {
                            Some(required_element(
                                provenance,
                                super::elements::ElementKind::IgnoreChanges,
                            )?)
                        }
                        _ => None,
                    },
                })
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        let waits = file
            .wait_bindings
            .iter()
            .map(|wait| {
                let location = wait.declaration.location().ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingWaitProvenance {
                        binding: wait.binding.as_str().to_owned(),
                    })
                })?;
                Ok(AnalysisWaitRef {
                    wait,
                    location,
                    target_location: required_element(
                        &wait.declaration,
                        super::elements::ElementKind::WaitTarget,
                    )?,
                    lhs_location: required_element(
                        &wait.declaration,
                        super::elements::ElementKind::UntilLhs,
                    )?,
                    rhs_location: required_element(
                        &wait.declaration,
                        super::elements::ElementKind::UntilRhs,
                    )?,
                    dependencies: dependency_refs(&wait.depends_on),
                })
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        let mut loop_groups: std::collections::HashSet<String> = file
            .parser_binding_metadata
            .structural_handles()
            .iter()
            .filter(|(_, handle)| handle.is_for_binding())
            .map(|(binding, _)| binding.to_string())
            .collect();
        for resource in file.iter_all_resources() {
            if matches!(
                resource,
                ResourceRef::Resource(_) | ResourceRef::Deferred { .. }
            ) && let Some(binding) = resource.binding_address()
            {
                loop_groups.extend(
                    crate::resolved_graph::loop_group_bindings(binding)
                        .into_iter()
                        .map(|group| group.to_string()),
                );
            }
        }
        for export in &file.export_params {
            if let Some(value) = export.value() {
                collect_value_references(
                    value,
                    &format!("exports.{}", export.name()),
                    AnalysisValueScope::Project,
                    &mut value_references,
                )?;
            }
        }
        for attribute in &file.attribute_params {
            if let Some(super::BoundaryExpression::Value(value)) = &attribute.value {
                collect_value_references(
                    value,
                    &format!("attributes.{}", attribute.name),
                    AnalysisValueScope::AttributeParameter,
                    &mut value_references,
                )?;
            }
        }
        for (name, value) in &file.variables {
            collect_value_references(
                value,
                &format!("let {name}"),
                AnalysisValueScope::Variable,
                &mut value_references,
            )?;
        }
        for call in &file.module_calls {
            let caller = call
                .binding_name
                .as_ref()
                .map(BindingAddress::as_str)
                .unwrap_or(&call.module_name);
            for (name, value) in call.arguments() {
                collect_value_references(
                    value.walker_value(),
                    &format!("module `{caller}` argument `{name}`"),
                    AnalysisValueScope::Project,
                    &mut value_references,
                )?;
            }
        }
        let syntax = file.parser_binding_metadata.syntax();
        let require_syntax = |present: bool, element: String| {
            if present {
                Ok(())
            } else {
                Err(AnalysisTreeError::new(
                    AnalysisTreeErrorKind::MissingSyntaxProvenance { element },
                ))
            }
        };
        for item in &resources {
            let Some(resource) = item.managed_resource() else {
                continue;
            };
            if resource.binding.is_none() && resource.loop_provenance.is_empty() {
                require_syntax(
                    syntax.anonymous_headers().iter().any(|header| {
                        header.file() == item.location().file()
                            && item.location().span().contains(header.span())
                    }),
                    format!("anonymous resource header {}", item.id()),
                )?;
            }
            if let Some(origin) = resource.loop_provenance.display_origin() {
                require_syntax(
                    syntax.loops().iter().any(|location| {
                        Some(location.file()) == origin.source_file()
                            && Some(location.span()) == origin.source_span()
                    }),
                    format!("loop identity origin {}", item.id()),
                )?;
            }
        }
        require_syntax(
            syntax
                .configuration_blocks()
                .iter()
                .filter(|block| *block.detail() == super::ConfigurationBlock::State)
                .count()
                == file.state_blocks.len(),
            "state block declarations".into(),
        )?;
        for block in &file.state_blocks {
            let span = match block {
                super::StateBlock::Moved { addresses } => addresses.to_span(),
                super::StateBlock::Import { to_span, .. } => *to_span,
                super::StateBlock::Removed { .. } => continue,
            };
            require_syntax(
                syntax
                    .state_targets()
                    .iter()
                    .any(|location| Some(location.span()) == span),
                "state destination".into(),
            )?;
        }
        let backend_location = if let Some(backend) = &file.backend {
            let header = syntax
                .configuration_blocks()
                .iter()
                .rev()
                .find(|block| *block.detail() == super::ConfigurationBlock::Backend)
                .ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: "backend declaration".to_string(),
                    })
                })?;
            for key in backend.attributes.keys() {
                if !syntax.backend_keys().iter().any(|entry| {
                    entry.detail() == key && entry.location().file() == header.location().file()
                }) {
                    return Err(AnalysisTreeError::new(
                        AnalysisTreeErrorKind::MissingSyntaxProvenance {
                            element: format!("backend attribute {key}"),
                        },
                    ));
                }
            }
            Some(header.location())
        } else {
            None
        };
        use crate::validation::{
            TypeResolutionContext as Context, TypeResolutionDeclarationKind as Kind,
        };
        let mut type_annotations: Vec<_> = syntax
            .type_annotations()
            .iter()
            .filter(|annotation| {
                matches!(
                    annotation.detail(),
                    Context::Declaration {
                        kind: Kind::Argument,
                        ..
                    }
                )
            })
            .cloned()
            .collect();
        let declaration = |kind, name: &str, source: &Path, span: super::SourceSpan| {
            syntax
                .declarations()
                .iter()
                .find(|declaration| {
                    declaration.kind() == kind
                        && declaration.name() == name
                        && declaration.location().file().path() == source
                        && span.contains(declaration.location().span())
                })
                .ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: format!("{kind:?} declaration {name}"),
                    })
                })
        };
        for export in &file.export_params {
            let record = declaration(
                crate::config_loader::DeclarationKind::Export,
                export.name(),
                export.source_file(),
                export.source_span(),
            )?;
            require_syntax(
                export.value().is_none() || record.value_location().is_some(),
                format!("export {} value", export.name()),
            )?;
            let location = match record.annotation() {
                super::DeclarationAnnotation::Authored(location) => location,
                super::DeclarationAnnotation::Unannotated => record.location(),
            };
            type_annotations.push(Located::new(
                location.clone(),
                Context::Declaration {
                    kind: Kind::Export,
                    name: export.name().to_string(),
                },
            ));
        }
        for attribute in &file.attribute_params {
            let record = declaration(
                crate::config_loader::DeclarationKind::ModuleAttribute,
                &attribute.name,
                attribute.source_file(),
                attribute.source_span(),
            )?;
            require_syntax(
                attribute.value.is_none() || record.value_location().is_some(),
                format!("module attribute {} value", attribute.name),
            )?;
            if attribute.static_type.is_some() {
                let location = match record.annotation() {
                    super::DeclarationAnnotation::Authored(location) => location,
                    super::DeclarationAnnotation::Unannotated => record.location(),
                };
                type_annotations.push(Located::new(
                    location.clone(),
                    Context::Declaration {
                        kind: Kind::Attribute,
                        name: attribute.name.clone(),
                    },
                ));
            }
        }
        for function in file.user_functions.values() {
            type_annotations.extend(function.type_annotations.iter().cloned());
        }
        for function in file.user_functions.values() {
            require_syntax(
                syntax.declarations().iter().any(|record| {
                    record.kind() == crate::config_loader::DeclarationKind::UserFunction
                        && record.name() == function.name
                }),
                format!("function {} declaration", function.name),
            )?;
            for value in function
                .params
                .iter()
                .filter_map(|parameter| parameter.default.as_ref())
                .chain(function.local_lets.iter().map(|(_, value, _)| value))
                .chain(std::iter::once(&function.body.value))
            {
                collect_value_references(
                    value,
                    &format!("function {}", function.name),
                    AnalysisValueScope::Function,
                    &mut value_references,
                )?;
            }
        }
        let mut expected = file
            .arguments
            .iter()
            .map(|argument| Context::Declaration {
                kind: Kind::Argument,
                name: argument.name.clone(),
            })
            .collect::<Vec<_>>();
        for function in file.user_functions.values() {
            for parameter in function
                .params
                .iter()
                .filter(|parameter| parameter.param_type.is_some())
            {
                expected.push(Context::Declaration {
                    kind: Kind::FunctionParameter,
                    name: format!("{}' parameter '{}", function.name, parameter.name),
                });
            }
            if function.return_type.is_some() {
                expected.push(Context::Declaration {
                    kind: Kind::FunctionReturn,
                    name: function.name.clone(),
                });
            }
        }
        let mut type_occurrences = std::collections::HashMap::new();
        for context in expected {
            let occurrence = type_occurrences.entry(context.clone()).or_insert(0);
            if type_annotations
                .iter()
                .filter(|annotation| annotation.detail() == &context)
                .nth(*occurrence)
                .is_none()
            {
                return Err(AnalysisTreeError::new(
                    AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: format!("{context} annotation"),
                    },
                ));
            }
            *occurrence += 1;
        }
        let mut provider_occurrences = std::collections::HashMap::new();
        let mut provider_attribute_locations = Vec::new();
        for provider in &file.providers {
            let occurrence = provider_occurrences
                .entry((&provider.name, &provider.binding))
                .or_insert(0);
            let record = syntax
                .providers()
                .iter()
                .filter(|record| record.name == provider.name && record.binding == provider.binding)
                .nth(*occurrence);
            *occurrence += 1;
            let record = record.ok_or_else(|| {
                AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                    element: format!("provider {} declaration", provider.name),
                })
            })?;
            let mut locations = std::collections::HashMap::new();
            for key in provider.unresolved_attributes.keys() {
                let location = record
                    .attributes
                    .iter()
                    .rev()
                    .find(|attribute| attribute.detail() == key)
                    .ok_or_else(|| {
                        AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                            element: format!("provider {} attribute {key}", provider.name),
                        })
                    })?;
                locations.insert(key.clone(), location.location().clone());
            }
            provider_attribute_locations.push(locations);
        }
        let mut source_occurrences = std::collections::HashMap::<&B, usize>::new();
        let upstream_states = file
            .upstream_states
            .iter()
            .map(|state| {
                let occurrence = source_occurrences.entry(&state.binding).or_default();
                let source = syntax
                    .upstream_sources()
                    .iter()
                    .filter(|source| *source.detail() == state.binding)
                    .nth(*occurrence)
                    .ok_or_else(|| {
                        AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                            element: format!("upstream source {}", state.binding),
                        })
                    })?;
                *occurrence += 1;
                Ok(Located::new(source.location().clone(), state))
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        let iterables = file
            .deferred_for_sites()
            .map(|site| {
                let location = site.iterable_location.ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: format!("iterable {}", site.header),
                    })
                })?;
                let mut refs = Vec::new();
                collect_value_references(
                    site.iterable,
                    site.header,
                    AnalysisValueScope::Project,
                    &mut refs,
                )?;
                let reference_usages = refs.iter().map(|reference| reference.usage()).collect();
                let mut result_references = Vec::new();
                collect_iterable_result_references(
                    site.iterable,
                    site.header,
                    &mut result_references,
                )?;
                Ok(AnalysisForRef {
                    site,
                    location,
                    reference_usages,
                    result_references: result_references
                        .into_iter()
                        .map(|reference| reference.reference)
                        .collect(),
                    references: refs
                        .into_iter()
                        .map(|reference| reference.reference)
                        .collect(),
                })
            })
            .collect::<Result<_, AnalysisTreeError>>()?;
        let mut validation_values = Vec::new();
        let mut reference_sites: std::collections::HashSet<_> = value_references
            .iter()
            .map(|value| value.reference().location().clone())
            .collect();
        let mut add_value = |location: &DeclarationLocation,
                             value: &'a Value<P, B>,
                             expected,
                             context|
         -> Result<(), AnalysisTreeError> {
            let mut references = Vec::new();
            collect_value_references(
                value,
                "function boundary",
                AnalysisValueScope::Function,
                &mut references,
            )?;
            value_references.extend(
                references
                    .into_iter()
                    .filter(|value| reference_sites.insert(value.reference().location().clone())),
            );
            validation_values.push(Located::new(
                location.clone(),
                FunctionValidationValue {
                    value,
                    expected,
                    context,
                    check_return: false,
                },
            ));
            Ok(())
        };
        let value_location =
            |kind, name: &str| -> Result<&DeclarationLocation, AnalysisTreeError> {
                syntax
                    .declarations()
                    .iter()
                    .find(|record| record.kind() == kind && record.name() == name)
                    .map(|record| record.value_location().unwrap_or(record.location()))
                    .ok_or_else(|| {
                        AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                            element: format!("{kind:?} {name} value"),
                        })
                    })
            };
        use crate::config_loader::DeclarationKind;
        for parameter in &file.arguments {
            if let Some(value) = &parameter.default {
                add_value(
                    value_location(DeclarationKind::Argument, &parameter.name)?,
                    value,
                    Some(&parameter.static_type),
                    Some(format!("argument '{}' default", parameter.name)),
                )?;
            }
        }
        for parameter in &file.attribute_params {
            if let Some(super::BoundaryExpression::Value(value)) = &parameter.value {
                let record = declaration(
                    DeclarationKind::ModuleAttribute,
                    &parameter.name,
                    parameter.source_file(),
                    parameter.source_span(),
                )?;
                add_value(
                    record.value_location().expect("validated attribute value"),
                    value,
                    parameter.static_type.as_ref(),
                    Some(format!("attribute '{}'", parameter.name)),
                )?;
            }
        }
        for parameter in &file.export_params {
            if let Some(value) = parameter.value() {
                let record = declaration(
                    DeclarationKind::Export,
                    parameter.name(),
                    parameter.source_file(),
                    parameter.source_span(),
                )?;
                add_value(
                    record.value_location().expect("validated export value"),
                    value,
                    parameter.type_expr_opt(),
                    Some(format!("export '{}'", parameter.name())),
                )?;
            }
        }
        for (name, value) in &file.variables {
            add_value(
                value_location(DeclarationKind::Binding, name)?,
                value,
                None,
                None,
            )?;
        }
        for call in &file.module_calls {
            for (name, value) in call.arguments() {
                add_value(
                    call.id()
                        .argument_value_location(name)
                        .unwrap_or_else(|| call.id().declaration_location()),
                    value.walker_value(),
                    None,
                    None,
                )?;
            }
        }
        for (index, provider) in file.providers.iter().enumerate() {
            let header = syntax
                .providers()
                .iter()
                .filter(|record| {
                    record.detail().name == provider.name
                        && record.detail().binding == provider.binding
                })
                .nth(
                    file.providers[..index]
                        .iter()
                        .filter(|other| {
                            other.name == provider.name && other.binding == provider.binding
                        })
                        .count(),
                )
                .expect("validated provider header");
            for value in provider
                .attributes
                .values()
                .chain(provider.default_tags.values())
                .chain(provider.unresolved_attributes.values())
            {
                add_value(header.location(), value, None, None)?;
            }
        }
        if let Some(backend) = &file.backend {
            for value in backend.attributes.values() {
                add_value(
                    backend_location.expect("validated backend"),
                    value,
                    None,
                    None,
                )?;
            }
        }
        for state in &file.state_blocks {
            if let super::StateBlock::Import { id, .. } = state {
                let location = syntax.state_targets().first().ok_or_else(|| {
                    AnalysisTreeError::new(AnalysisTreeErrorKind::MissingSyntaxProvenance {
                        element: "state import".into(),
                    })
                })?;
                add_value(location, id, None, None)?;
            }
        }
        Ok(Self {
            file,
            resources,
            top_level_resource_count: file.resources.len()
                + file.compositions.len()
                + file.data_sources.len(),
            waits,
            providers: &file.providers,
            upstream_names: file
                .upstream_states
                .iter()
                .map(|state| state.binding.as_str())
                .collect(),
            loop_groups,
            syntax,
            type_annotations,
            provider_attribute_locations,
            upstream_states,
            value_references,
            validation_values,
            iterables,
            arguments: &file.arguments,
            functions: &file.user_functions,
            module_calls: &file.module_calls,
            module_definition: !file.arguments.is_empty() || !file.attribute_params.is_empty(),
            root_configuration: file.backend.is_some() || !file.providers.is_empty(),
            backend_location,
            binding_names: syntax
                .binding_names()
                .iter()
                .map(ToString::to_string)
                .chain(file.variables.keys().cloned())
                .chain(file.structural_bindings.iter().map(ToString::to_string))
                .collect(),
        })
    }

    /// Core transformations can only clone the same input that was sealed.
    pub(crate) fn file(&self) -> &'a File<E, P, T, D, B> {
        self.file
    }

    pub fn providers(&self) -> &'a [super::ProviderConfig<P, B>] {
        self.providers
    }

    pub fn provider_location(&self, index: usize) -> &DeclarationLocation {
        self.provider_declaration(index).location()
    }

    pub fn provider_attribute_location(&self, index: usize, name: &str) -> &DeclarationLocation {
        let declaration = self.provider_declaration(index);
        declaration
            .attributes
            .iter()
            .rev()
            .find(|attribute| attribute.detail() == name)
            .map_or(declaration.location(), |attribute| attribute.location())
    }

    fn provider_declaration(&self, index: usize) -> &Located<super::elements::ProviderDeclaration> {
        let provider = &self.providers[index];
        let occurrence = self.providers[..index]
            .iter()
            .filter(|other| other.name == provider.name && other.binding == provider.binding)
            .count();
        self.syntax
            .providers()
            .iter()
            .filter(|record| record.name == provider.name && record.binding == provider.binding)
            .nth(occurrence)
            .expect("provider inventory validated at the analysis boundary")
    }

    pub fn is_module_definition(&self) -> bool {
        self.module_definition
    }
    pub fn is_root_configuration(&self) -> bool {
        self.root_configuration
    }
    pub fn backend_location(&self) -> Option<&'a DeclarationLocation> {
        self.backend_location
    }
    pub fn backend_attribute_location(&self, name: &str) -> &'a DeclarationLocation {
        let header = self
            .backend_location
            .expect("backend must exist before validating its attributes");
        self.syntax
            .backend_keys()
            .iter()
            .rev()
            .find(|key| key.detail() == name && key.location().file() == header.file())
            .map(|key| key.location())
            .unwrap_or(header)
    }
    pub fn is_upstream(&self, binding: &str) -> bool {
        self.upstream_names.contains(binding)
    }
    pub fn is_loop_group(&self, binding: &str) -> bool {
        self.loop_groups.contains(binding)
    }
    pub(crate) fn provider_attribute_locations(
        &self,
    ) -> &[std::collections::HashMap<String, DeclarationLocation>] {
        &self.provider_attribute_locations
    }
    pub fn type_annotations(&self) -> &[Located<crate::validation::TypeResolutionContext>] {
        &self.type_annotations
    }
    pub fn syntax(&self) -> &'a super::SyntaxInventory<B> {
        self.syntax
    }
    pub fn module_calls(&self) -> impl Iterator<Item = &'a super::ModuleCall<P, B>> {
        self.module_calls.iter()
    }

    pub fn binding_names(&self) -> &std::collections::HashSet<String> {
        &self.binding_names
    }
    pub fn upstream_states(&self) -> &[Located<&'a super::UpstreamState<B>>] {
        &self.upstream_states
    }
    pub fn value_references(&self) -> &[AnalysisValueReference<B>] {
        &self.value_references
    }

    /// Resource attributes, exports, and module-call arguments, matching the
    /// project-value walk used by upstream shape validation.
    pub fn project_value_references(&self) -> impl Iterator<Item = &AnalysisValueReference<B>> {
        self.value_references
            .iter()
            .filter(|value| value.scope == AnalysisValueScope::Project)
    }

    pub(crate) fn upstream_field_references(
        &self,
    ) -> impl Iterator<Item = &AnalysisValueReference<B>> {
        self.value_references.iter().filter(|value| {
            matches!(
                value.scope,
                AnalysisValueScope::Project
                    | AnalysisValueScope::AttributeParameter
                    | AnalysisValueScope::Variable
            )
        })
    }
    /// Resolve an occurrence without weakening the scope carried by its visitor.
    pub fn reference_for_scoped(
        &self,
        reference: crate::resource::ScopedResourceRef<'_, B>,
    ) -> AnalysisReference<B> {
        let identity = reference.identity();
        let matched = self
            .value_references
            .iter()
            .map(|value| value.reference())
            .chain(
                self.resources
                    .iter()
                    .flat_map(|resource| resource.references()),
            )
            .find(|candidate| identity.matches_occurrence(&candidate.path))
            .expect("reference validated at the analysis boundary");
        AnalysisReference {
            path: matched.path.clone(),
            locations: matched.locations.clone(),
            diagnostic_scope: if reference.is_secret() {
                crate::resource::DiagnosticScope::Secret
            } else {
                matched.diagnostic_scope
            },
        }
    }
    pub fn reference_for_path(
        &self,
        path: &crate::resource::AccessPath<B>,
    ) -> &AnalysisReference<B> {
        self.value_references
            .iter()
            .map(|value| value.reference())
            .chain(
                self.resources
                    .iter()
                    .flat_map(|resource| resource.references()),
            )
            .find(|reference| {
                &reference.path == path
                    && reference.path.reference_locations_shared()
                        == path.reference_locations_shared()
            })
            .expect("reference validated at the analysis boundary")
    }

    pub fn for_iterables(&self) -> &[AnalysisForRef<'a, P, B>] {
        &self.iterables
    }

    pub fn resources(&self) -> impl Iterator<Item = &AnalysisResourceRef<'a, P, T, D, B>> {
        self.resources.iter()
    }

    /// The general project walk, matching `File::iter_all_resources`.
    pub fn project_resources(&self) -> impl Iterator<Item = &AnalysisResourceRef<'a, P, T, D, B>> {
        self.resources
            .iter()
            .filter(|resource| resource.in_project_walk)
    }

    /// Materialized resources, compositions and data sources in declaration
    /// order, matching `File::iter_top_level_resources` and excluding templates
    /// and deferred conditional branches.
    pub fn top_level_resources(
        &self,
    ) -> impl Iterator<Item = &AnalysisResourceRef<'a, P, T, D, B>> {
        self.resources.iter().take(self.top_level_resource_count)
    }

    pub fn waits(&self) -> impl Iterator<Item = &AnalysisWaitRef<'a, B>> {
        self.waits.iter()
    }
}

impl<'a, E, P, T, D, B> TryFrom<&'a File<E, P, T, D, B>> for AnalysisTree<'a, P, T, D, B, E>
where
    E: super::ExportParamLike<P, B>,
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

impl<D, B: BindingAddress, E> AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B, E>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn visit_function_values(
        &self,
        visit: &mut impl FnMut(Located<FunctionValidationValue<'_, ParserPlaceholder, B>>),
    ) {
        for resource in self.resources() {
            for value in resource.attributes().values() {
                visit(Located::new(
                    resource.location().clone(),
                    FunctionValidationValue {
                        value,
                        expected: None,
                        context: None,
                        check_return: false,
                    },
                ));
            }
        }
        for input in &self.validation_values {
            visit(Located::new(
                input.location().clone(),
                FunctionValidationValue {
                    value: input.value,
                    expected: input.expected,
                    context: input.context.clone(),
                    check_return: false,
                },
            ));
        }
        for function in self.functions.values() {
            let location = self
                .syntax
                .declarations()
                .iter()
                .find(|record| {
                    record.kind() == crate::config_loader::DeclarationKind::UserFunction
                        && record.name() == function.name
                })
                .expect("validated function declaration")
                .location();
            for parameter in &function.params {
                if let Some(value) = &parameter.default {
                    visit(Located::new(
                        location.clone(),
                        FunctionValidationValue {
                            value,
                            expected: parameter.param_type.as_ref(),
                            context: Some(format!(
                                "function '{}': parameter '{}' default",
                                function.name, parameter.name
                            )),
                            check_return: false,
                        },
                    ));
                }
            }
            for (_, value, _) in &function.local_lets {
                visit(Located::new(
                    location.clone(),
                    FunctionValidationValue {
                        value,
                        expected: None,
                        context: None,
                        check_return: false,
                    },
                ));
            }
            visit(Located::new(
                location.clone(),
                FunctionValidationValue {
                    value: &function.body.value,
                    expected: function.return_type.as_ref(),
                    context: Some(format!("function '{}': return value", function.name)),
                    check_return: true,
                },
            ));
        }
    }

    pub fn inference_bindings(
        &self,
    ) -> crate::validation::inference::InferenceBindings<ParserPlaceholder, B> {
        use crate::validation::inference::InferenceBinding;
        let mut bindings = std::collections::HashMap::new();
        for upstream in self.upstream_states() {
            bindings.insert(
                upstream.binding.to_string(),
                InferenceBinding::UpstreamState,
            );
        }
        for resource in self.resources() {
            if let Some(binding) = resource.binding_address() {
                let kind = if resource.kind() == AnalysisItemKind::Composition {
                    InferenceBinding::Virtual {
                        attributes: resource.attributes().into_owned(),
                    }
                } else {
                    InferenceBinding::Resource {
                        provider: resource.id().provider.clone(),
                        resource_type: resource.id().resource_type.clone(),
                    }
                };
                bindings.insert(binding.to_string(), kind);
            }
        }
        for call in self.module_calls {
            if let Some(binding) = &call.binding_name {
                bindings
                    .entry(binding.to_string())
                    .or_insert(InferenceBinding::ModuleCall);
            }
        }
        for argument in self.arguments {
            bindings
                .entry(argument.name.clone())
                .or_insert_with(|| InferenceBinding::Argument {
                    type_expr: argument.static_type.clone(),
                });
        }
        for function in self.functions.values() {
            bindings.entry(function.name.clone()).or_insert_with(|| {
                InferenceBinding::UserFunction {
                    return_type: function.return_type.clone(),
                }
            });
        }
        bindings
    }
}

#[cfg(test)]
thread_local! {
    static ATTRIBUTE_LOCATION_WORK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn record_attribute_location_work(steps: usize) {
    ATTRIBUTE_LOCATION_WORK.with(|work| {
        if let Some(count) = work.get() {
            work.set(Some(count + steps));
        }
    });
}

#[cfg(test)]
mod location_tests {
    use super::*;

    #[test]
    fn aliased_collection_input_construction_updates_locations_in_one_pass() {
        use crate::schema::{
            AttributeSchema, AttributeType, InputCondition, InputPath, InputPredicate, InputRule,
            InputRuleCode, InputRuleScope, ResourceSchema, SchemaRegistry, StructField,
        };
        const COUNT: usize = 128;
        let entry = AttributeType::struct_with_rules(
            "Entry",
            vec![
                StructField::optional(
                    "rules",
                    AttributeType::list(AttributeType::struct_("Rule", vec![])),
                )
                .with_block_name("rule")
                .build(),
                StructField::optional("target", AttributeType::bool()).build(),
            ],
            vec![],
            vec![
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("entry.target").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Present(InputPath::new("rules").unwrap())),
            ],
        );
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Review")
                .attribute(AttributeSchema::optional("items", AttributeType::map(entry)).build())
                .validate()
                .unwrap(),
        );
        let entries = (0..COUNT)
            .map(|index| format!("key{index} = {{ rule {{}} }}"))
            .collect::<Vec<_>>()
            .join(", ");
        let parsed = super::super::parse_file(
            &format!("let example = mock.Review {{ items = {{ {entries} }} }}"),
            Path::new("main.crn"),
            &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        ATTRIBUTE_LOCATION_WORK.set(Some(0));
        let report =
            crate::input_rules::check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas);
        assert_eq!(report.findings().len(), COUNT);
        for finding in report.findings() {
            assert!(finding.authored_location().is_some());
        }
        let work = ATTRIBUTE_LOCATION_WORK.replace(None).unwrap();
        assert!(
            work <= COUNT * 128,
            "{work} location visits for {COUNT} aliased instances, including construction"
        );
    }

    #[test]
    fn collection_input_findings_use_a_linear_authored_location_budget() {
        use crate::schema::{
            AttributeSchema, AttributeType, InputCondition, InputPath, InputPredicate, InputRule,
            InputRuleCode, InputRuleScope, ResourceSchema, SchemaRegistry, StructField,
        };
        const COUNT: usize = 256;
        for container in ["list", "map"] {
            let item = AttributeType::struct_with_rules(
                "Item",
                vec![
                    StructField::optional("guard", AttributeType::bool()).build(),
                    StructField::optional("target", AttributeType::string()).build(),
                ],
                vec![],
                vec![
                    InputRule::value(
                        InputRuleScope::EveryWrite,
                        InputPath::new("target").unwrap(),
                        InputRuleCode::new("item.target").unwrap(),
                        InputPredicate::Present,
                    )
                    .with_condition(InputCondition::Present(InputPath::new("guard").unwrap())),
                ],
            );
            let item_type = match container {
                "list" => AttributeType::list(item),
                "map" => AttributeType::map(item),
                _ => unreachable!(),
            };
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                ResourceSchema::new("Review")
                    .attribute(AttributeSchema::optional("items", item_type).build())
                    .validate()
                    .unwrap(),
            );
            let items = (0..COUNT)
                .map(|i| match container {
                    "list" => "{ guard = true }".into(),
                    "map" => format!("key{i} = {{ guard = true }}"),
                    _ => unreachable!(),
                })
                .collect::<Vec<String>>()
                .join(", ");
            let (open, close) = if container == "list" {
                ("[", "]")
            } else {
                ("{", "}")
            };
            let source = format!("let example = mock.Review {{ items = {open}{items}{close} }}");
            let parsed = super::super::parse_file(
                &source,
                Path::new("main.crn"),
                &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            let report = crate::input_rules::check_authored_inputs(
                parsed.authored_inputs().unwrap(),
                &schemas,
            );
            assert_eq!(report.findings().len(), COUNT);
            ATTRIBUTE_LOCATION_WORK.set(Some(0));
            for finding in report.findings().iter() {
                assert!(finding.authored_location().is_some());
            }
            let work = ATTRIBUTE_LOCATION_WORK.replace(None).unwrap();
            assert!(
                work <= COUNT * 48,
                "{container}: {work} location steps for {COUNT} findings"
            );
        }
    }

    #[test]
    fn secret_origin_4275_round2_iterable_provenance_masks_secret_path() {
        use crate::resource::{DeferredValue, ExpandedValue};
        let reference =
            ExpandedValue::root_resource_ref("private_producer", "private_attr", vec![]);
        let mut references = Vec::new();
        let public_error =
            collect_iterable_result_references(&reference, "iterable", &mut references)
                .unwrap_err();
        assert!(
            public_error
                .to_string()
                .contains("private_producer.private_attr")
        );
        let secret = ExpandedValue::Deferred(DeferredValue::Secret(Box::new(reference).into()));
        let error =
            collect_iterable_result_references(&secret, "iterable", &mut references).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(crate::resource::INSIDE_SECRET_MESSAGE)
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("private_"), "{rendered}");
        }
    }

    #[test]
    fn integer_operator_without_parser_provenance_fails_at_seal() {
        use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values};
        use crate::resource::{ConcreteValue, DeferredValue};

        for resource_value in [false, true] {
            let mut parsed = super::super::parse_file(
                "let count = 1\nmock.test.Resource { count = 1 }\n",
                Path::new("sibling.crn"),
                &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            let arithmetic = evaluate_integer_values(
                Value::Deferred(DeferredValue::BindingRef {
                    binding: RelativeBindingName::new("count"),
                }),
                IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                IntegerOperatorSpan::synthetic(),
            )
            .unwrap();
            if resource_value {
                parsed.resources[0]
                    .attributes
                    .insert("count".into(), arithmetic);
            } else {
                parsed.variables.insert("count".into(), arithmetic);
            }
            let error = match AnalysisTree::try_from_file(&parsed) {
                Ok(_) => panic!("integer operator without provenance entered analysis"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("integer operator"), "{error}");
        }
    }

    #[test]
    fn missing_attribute_value_provenance_fails_at_seal() {
        let mut parsed = super::super::parse_file(
            "mock.test.Resource { enabled = true }\n",
            Path::new("sibling.crn"),
            &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        Arc::make_mut(&mut parsed.resources[0].declaration.elements).clear_attribute_values();
        let error = match AnalysisTree::try_from_file(&parsed) {
            Ok(_) => panic!("missing value provenance must fail before any analysis runs"),
            Err(error) => error,
        };
        assert!(
            matches!(error.kind(), AnalysisTreeErrorKind::MissingElementProvenance { element, .. } if element == "attribute value enabled")
        );
    }
}
