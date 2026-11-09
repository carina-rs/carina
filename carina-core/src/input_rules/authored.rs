//! Parser/admission-owned authored snapshots. Public mutable ASTs carry only a
//! read-only snapshot of the source boundary; mutating the AST cannot reseal it.
use super::*;

/// The file has not crossed a parser/admission boundary that grants authored
/// input authority. This cannot be interpreted as a successful empty check.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("authored input authority is unavailable")]
pub struct AuthoredInputUnavailable;

impl From<AuthoredInputUnavailable> for crate::provider::HostError {
    fn from(source: AuthoredInputUnavailable) -> Self {
        Self::invalid_input_caused_by("authored input authority is unavailable", source)
    }
}

/// Immutable authored evidence. Parser and admission owners mint this snapshot;
/// public AST edits cannot change it or manufacture another complete input.
#[derive(Clone)]
pub struct AuthoredInputs {
    parser_source: Option<Arc<crate::parser::ParsedFile>>,
    resources: Vec<crate::resource::Resource<ModuleSource, ParserPlaceholder>>,
    defaults: Vec<type_context::DefaultTagEvidence>,
}
impl std::fmt::Debug for AuthoredInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthoredInputs(<parser/admission snapshot>)")
    }
}

pub(super) struct AuthoredResource<'a>(
    &'a crate::resource::Resource<ModuleSource, ParserPlaceholder>,
);
impl<'a> AuthoredResource<'a> {
    pub(super) fn resource(
        &self,
    ) -> &'a crate::resource::Resource<ModuleSource, ParserPlaceholder> {
        self.0
    }
}

impl AuthoredInputs {
    fn snapshot<E, P, T, D, B>(file: &crate::parser::File<E, P, T, D, B>) -> Self
    where
        P: crate::resource::PlaceholderKind,
        T: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    {
        use crate::parser::ResourceRef;
        let resources = file
            .iter_all_resources()
            .filter_map(|source| {
                let (id, attributes, declaration) = match source {
                    ResourceRef::Resource(resource) => (
                        &resource.id,
                        project_attributes(&resource.attributes),
                        resource.declaration.clone(),
                    ),
                    ResourceRef::Deferred { resource, .. } => (
                        resource.id(),
                        project_attributes(resource.attributes()),
                        resource.as_resource().declaration.clone(),
                    ),
                    ResourceRef::DataSource(_) | ResourceRef::LiveComposition(_) => return None,
                };
                let mut projected = crate::resource::Resource::from_id(id.clone());
                projected.attributes = attributes;
                projected.declaration = declaration;
                Some(projected)
            })
            .collect();
        let defaults = file
            .providers
            .iter()
            .map(type_context::DefaultTagEvidence::for_config)
            .collect();
        Self {
            resources,
            defaults,
            parser_source: None,
        }
    }

    pub(crate) fn for_parser_output(source: &crate::parser::AuthoredParse<'_>) -> Self {
        let mut inputs = Self::snapshot(source.file());
        // The parser calls this before attaching the snapshot to its public AST.
        // This retained source is immutable and contains no snapshot back-edge.
        inputs.parser_source = Some(Arc::new(source.file().clone()));
        inputs
    }

    pub fn resolved_in_session(
        &self,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        self.parser_source
            .as_ref()
            .and_then(|source| {
                let tree = crate::parser::AnalysisTree::try_from_file(source).ok()?;
                crate::parser::resolve_resource_refs_from_analysis(&tree, session).ok()
            })
            .as_ref()
            .map(Self::snapshot)
            .unwrap_or_else(|| self.clone())
    }

    pub fn for_directory<E>(source: &crate::config_loader::RootDirectoryParse<E>) -> Self {
        Self::snapshot(source.parsed())
    }

    pub fn for_directory_in_session<E>(
        source: &crate::config_loader::RootDirectoryParse<E>,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self
    where
        E: crate::parser::ExportParamLike + Clone,
    {
        let mut resolved = source.clone();
        match resolved.resolve_resource_refs_in_session(session) {
            Ok(()) => Self::for_directory(&resolved),
            Err(_) => Self::for_directory(source),
        }
    }

    pub(crate) fn for_module_admission<E>(
        source: &crate::module_resolver::AuthoredModuleGraph<'_, E>,
        config: &crate::parser::ProviderContext,
    ) -> Self
    where
        E: crate::parser::ExportParamLike<
                ParserPlaceholder,
                crate::resource::AbsoluteBindingAddress,
            > + Clone,
    {
        let mut inputs = Self::snapshot(source.file());
        if source
            .file()
            .providers
            .iter()
            .any(|provider| !provider.unresolved_attributes.is_empty())
        {
            inputs.defaults =
                crate::parser::preview_expanded_provider_inputs(source.file(), config)
                    .iter()
                    .map(type_context::DefaultTagEvidence::for_preview)
                    .collect();
        }
        inputs
    }

    pub(super) fn resources(&self) -> impl Iterator<Item = AuthoredResource<'_>> {
        self.resources.iter().map(AuthoredResource)
    }
    pub(super) fn defaults(&self) -> &[type_context::DefaultTagEvidence] {
        &self.defaults
    }

    #[cfg(test)]
    pub(super) fn for_test<E, P, T, D, B>(file: &crate::parser::File<E, P, T, D, B>) -> Self
    where
        P: crate::resource::PlaceholderKind,
        T: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
    {
        Self::snapshot(file)
    }

    #[cfg(test)]
    pub(super) fn resource_for_test<
        S,
        P: crate::resource::PlaceholderKind,
        B: crate::resource::BindingAddress,
        D,
    >(
        resource: &crate::resource::Resource<S, P, B, D>,
    ) -> Self {
        let mut projected = crate::resource::Resource::from_id(resource.id.clone());
        projected.attributes = project_attributes(&resource.attributes);
        projected.declaration = resource.declaration.clone();
        Self {
            resources: vec![projected],
            defaults: Vec::new(),
            parser_source: None,
        }
    }
}
