//! Borrowed read views: no judgement/file extraction or admission constructor.
use super::*;
use crate::parser::{AttributeParameter, ProviderConfig, TypeExpr};
use crate::resource::{AbsoluteBindingAddress, RelativeBindingName};

/// Read-only collections from a canonicalized file. Cloning this view copies
/// references, never a file or its judgement. It cannot authorize admission.
pub struct PrefixFileRead<'f, E, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress> {
    file: &'f File<E, P, T, D, B>,
    schemas: &'f SchemaRegistry,
    pub providers: &'f [ProviderConfig<P, B>],
    pub resources: &'f [Resource<ModuleSource, P, B>],
    pub compositions: &'f [crate::resource::LiveComposition<P, B>],
    pub variables: &'f IndexMap<String, Value<P, B>>,
    pub arguments: &'f [crate::parser::ArgumentParameter<TypeExpr, P, B>],
    pub attribute_params: &'f [AttributeParameter<TypeExpr, P, B>],
    pub export_params: &'f [E],
    pub backend: Option<&'f crate::parser::BackendConfig<P, B>>,
    pub state_blocks: &'f [crate::parser::StateBlock<P, B>],
    pub wait_bindings: &'f [crate::parser::WaitBinding<B>],
}
impl<E, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress> Copy
    for PrefixFileRead<'_, E, P, T, D, B>
{
}
impl<E, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress> Clone
    for PrefixFileRead<'_, E, P, T, D, B>
{
    fn clone(&self) -> Self {
        *self
    }
}
impl<'f, E, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>
    PrefixFileRead<'f, E, P, T, D, B>
{
    pub(crate) fn new(file: &'f File<E, P, T, D, B>, schemas: &'f SchemaRegistry) -> Self {
        Self {
            file,
            schemas,
            providers: &file.providers,
            resources: &file.resources,
            compositions: &file.compositions,
            variables: &file.variables,
            arguments: &file.arguments,
            attribute_params: &file.attribute_params,
            export_params: &file.export_params,
            backend: file.backend.as_ref(),
            state_blocks: &file.state_blocks,
            wait_bindings: &file.wait_bindings,
        }
    }
    #[cfg(test)]
    pub(crate) fn for_test(file: &'f File<E, P, T, D, B>, schemas: &'f SchemaRegistry) -> Self {
        Self::new(file, schemas)
    }
    pub fn bare_binding_uses(&self) -> &'f crate::parser::ParsedBindingUses<B> {
        self.file.bare_binding_uses()
    }
    pub fn is_module_definition(&self) -> bool {
        crate::validation::is_module_definition(self.file)
    }
    pub fn authored_inputs(
        &self,
    ) -> Result<&'f crate::input_rules::AuthoredInputs, crate::input_rules::AuthoredInputUnavailable>
    {
        self.file.authored_inputs()
    }
}
impl<E, P: PlaceholderKind, T: PlaceholderKind, D, B: BindingAddress>
    PrefixFileRead<'_, E, P, T, D, B>
where
    D: DeferredForPhaseTraversal<P, T, B>,
{
    pub fn iter_all_resources(
        &self,
    ) -> impl Iterator<Item = crate::parser::ResourceRef<'_, ModuleSource, P, T, D, B>> {
        self.file.iter_all_resources()
    }
    pub fn deferred_for_sites(
        &self,
    ) -> impl Iterator<Item = crate::parser::DeferredForSite<'_, P, B>> {
        self.file.deferred_for_sites()
    }
}
impl<'f, E, D, B: BindingAddress> PrefixFileRead<'f, E, ParserPlaceholder, ParserPlaceholder, D, B>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub fn analysis_tree(
        &self,
    ) -> Result<AnalysisTree<'f, ParserPlaceholder, ParserPlaceholder, D, B, E>, AnalysisTreeError>
    where
        E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    {
        AnalysisTree::try_from_file(self.file)
    }
    pub fn expression_report(&self) -> &'f crate::parser::retained::ExpressionReport<B> {
        self.file.expression_report()
    }
    pub(crate) fn inference_bindings(
        &self,
    ) -> crate::validation::inference::InferenceBindings<ParserPlaceholder, B> {
        crate::validation::inference::bindings_from_parsed(self.file)
    }
    pub(crate) fn module_resource_ref_declaration(
        &self,
        id: &crate::resource::ResourceId,
        name: &str,
        value: &Value<ParserPlaceholder, B>,
        bindings: &crate::binding_index::BindingIndex<'_, B>,
    ) -> Option<(&'f str, crate::binding_index::DeclaredValueSource)> {
        crate::validation::module_resource_ref_declaration(*self, id, name, value, bindings)
    }
    pub fn binding_index(
        &self,
        context: &ProviderContext,
    ) -> crate::binding_index::BindingIndex<'f, B> {
        crate::binding_index::BindingIndex::from_parsed(self.file, self.schemas, context)
    }
    pub fn binding_index_in_session(
        &self,
        upstream: Option<&'f crate::upstream_exports::UpstreamExports<B>>,
        signatures: &'f crate::module_resolver::ResolvedModuleSignatures,
        session: &'f dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> crate::binding_index::BindingIndex<'f, B> {
        crate::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
            self.file, self.schemas, upstream, signatures, session,
        )
    }
    pub fn binding_use_authority_owns_use(
        &self,
        context: &crate::validation::ValidationTypeContext<'_, B>,
        report: &crate::validation::BindingUseReport,
        use_site: &crate::parser::BareBindingUse,
    ) -> bool {
        crate::validation::binding_use_authority_owns_use(self.file, context, report, use_site)
    }
    pub fn directive_resolution_errors(
        &self,
        boundary: crate::parser::ParserPlaceholderBoundary,
        context: &ProviderContext,
    ) -> Result<
        Vec<crate::resource::DirectiveResolutionError>,
        crate::parser::retained::MissingEvidence,
    > {
        self.file
            .directive_resolution_errors(boundary, self.schemas, context)
    }
    pub fn module_signatures(
        &self,
        base: &crate::paths::ResolvedPath,
        context: &ProviderContext,
    ) -> crate::module_resolver::ResolvedModuleSignatures {
        crate::module_resolver::load_resolved_module_signatures(self.file, base, context)
    }
}
impl<E, D> PrefixFileRead<'_, E, ParserPlaceholder, ParserPlaceholder, D, RelativeBindingName>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, RelativeBindingName>,
{
    pub fn resolved_bindings(
        &self,
    ) -> crate::binding_index::ResolvedBindings<ParserPlaceholder, RelativeBindingName> {
        crate::binding_index::ResolvedBindings::for_authored_validation(self.file)
    }
}
impl<E, D> PrefixFileRead<'_, E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, AbsoluteBindingAddress>,
{
    pub fn ensure_expanded_root_graph(&self) -> Result<(), crate::module_resolver::ModuleError>
    where
        E: crate::parser::ExportParamLike<ParserPlaceholder, AbsoluteBindingAddress>,
        D: crate::parser::ParserDeferredForPhaseAudit<AbsoluteBindingAddress>,
    {
        crate::module_resolver::ensure_expanded_root_graph(self.file)
    }
    pub fn resolved_bindings(
        &self,
    ) -> crate::binding_index::ResolvedBindings<ParserPlaceholder, AbsoluteBindingAddress> {
        crate::binding_index::ResolvedBindings::for_expanded_validation(self.file)
    }
}
impl<'f, E, B: BindingAddress>
    PrefixFileRead<
        'f,
        E,
        ParserPlaceholder,
        ParserPlaceholder,
        crate::parser::ResolvedDeferredForPhase<ParserPlaceholder, ParserPlaceholder, B>,
        B,
    >
where
    E: crate::parser::IntoExpandedExportParam<B>,
{
    pub fn deferred_for_expressions(
        &self,
    ) -> &'f crate::parser::ResolvedDeferredForExpressions<ParserPlaceholder, ParserPlaceholder, B>
    {
        self.file.deferred_for_expressions()
    }
    pub fn validate_expansion_boundary(
        &self,
        boundary: crate::parser::ParserPlaceholderBoundary,
    ) -> Result<(), crate::resource::LeftoverPlaceholders> {
        self.file.validate_expansion_boundary(boundary)
    }
}
pub type ModuleResolvedFileRead<'f, E> = PrefixFileRead<
    'f,
    E,
    ParserPlaceholder,
    ParserPlaceholder,
    crate::parser::ResolvedDeferredForPhase<
        ParserPlaceholder,
        ParserPlaceholder,
        AbsoluteBindingAddress,
    >,
    AbsoluteBindingAddress,
>;
