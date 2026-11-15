//! Native schema fixtures enter the server's reload and document-analysis path.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use carina_core::{provider::*, schema::*};
use tower_lsp::lsp_types::Diagnostic;

use crate::{
    backend::{FactoryBuilder, ProviderStates, analyze_document, reload_provider_states},
    document::Document,
};

pub struct Diagnostics {
    root: PathBuf,
    snapshot: Arc<LoadedProviderSnapshot>,
    providers: BTreeSet<String>,
}

impl Diagnostics {
    pub fn new(root: &Path, snapshot: Arc<LoadedProviderSnapshot>, providers: Vec<String>) -> Self {
        Self {
            root: root.to_path_buf(),
            snapshot,
            providers: providers.into_iter().collect(),
        }
    }

    pub fn from_schemas(root: &Path, schemas: SchemaRegistry) -> Self {
        let providers = schemas
            .iter()
            .map(|(owner, _, _, _)| owner.to_owned())
            .collect::<BTreeSet<_>>();
        Self::new(
            root,
            Arc::new(LoadedProviderSnapshot::for_test(
                schemas,
                Arc::new(Vec::new()),
            )),
            providers.into_iter().collect(),
        )
    }

    pub fn analyze(
        &self,
        document: &Document,
        current_file_path: Option<&Path>,
    ) -> Vec<Diagnostic> {
        // Fixture helpers are also called by async CLI tests. A scoped runtime
        // keeps provider reload on the production async path without nesting
        // block_on inside the caller's Tokio runtime.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            let snapshot = self.snapshot.clone();
                            let providers = self.providers.clone();
                            let builder: FactoryBuilder = Arc::new(move |configs| {
                                let mut seen = BTreeSet::new();
                                let mut factories = Vec::new();
                                for (_, config) in configs {
                                    let name = &config.name;
                                    if providers.contains(name) && seen.insert(name.clone()) {
                                        factories.push(Box::new(SnapshotFactory {
                                            name: name.clone(),
                                            snapshot: snapshot.clone(),
                                        })
                                            as Box<dyn ProviderFactory>);
                                    }
                                }
                                (factories, Default::default(), Vec::new())
                            });
                            let states = tokio::sync::RwLock::new(ProviderStates::new(
                                document.provider_context().environment.clone(),
                            ));
                            reload_provider_states(&self.root, &builder, None, &states).await;
                            analyze_document(&states.into_inner(), document, current_file_path)
                        })
                })
                .join()
                .expect("diagnostic fixture thread panicked")
        })
    }
}

struct SnapshotFactory {
    name: String,
    snapshot: Arc<LoadedProviderSnapshot>,
}

impl SnapshotFactory {
    fn inner(&self) -> Option<&dyn ProviderFactory> {
        find_factory(self.snapshot.factories(), &self.name)
    }
}

impl ProviderFactory for SnapshotFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        self.inner()
            .map_or(self.name.as_str(), |factory| factory.display_name())
    }
    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        session: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        match self.inner() {
            Some(factory) => factory.validate_config(attributes, session),
            None => Ok(()),
        }
    }
    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        session: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        match self.inner() {
            Some(factory) => factory.validate_custom_type(identity, value, session),
            None => Ok(()),
        }
    }
    fn create_provider(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        self.inner()
            .expect("schema-only fixtures do not construct providers")
            .create_provider(session, mint)
    }
    fn create_normalizer(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        self.inner()
            .expect("schema-only fixtures do not construct normalizers")
            .create_normalizer(session, mint)
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        match self.inner() {
            Some(factory) => factory.schemas(),
            None => {
                let mut schemas = self
                    .snapshot
                    .schema_registry()
                    .iter()
                    .filter(|(owner, _, _, _)| *owner == self.name)
                    .map(|(_, resource_type, kind, schema)| {
                        (
                            resource_type,
                            kind == SchemaKind::DataSource,
                            schema.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                schemas.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
                schemas.into_iter().map(|(_, _, schema)| schema).collect()
            }
        }
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        match self.inner() {
            Some(factory) => factory.schema_witness(),
            None => ValidatedProviderSchemaWitness::new(
                self.schemas(),
                Default::default(),
                Default::default(),
                Vec::new(),
            )
            .unwrap(),
        }
    }
    fn identity_attributes(&self) -> Vec<&str> {
        self.inner()
            .map_or_else(Vec::new, |factory| factory.identity_attributes())
    }
    fn config_completions(&self) -> std::collections::HashMap<String, Vec<CompletionValue>> {
        self.inner()
            .map_or_else(Default::default, |factory| factory.config_completions())
    }
}
