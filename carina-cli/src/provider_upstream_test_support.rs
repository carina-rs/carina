use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use carina_core::config_loader::load_configuration_with_config;
use carina_core::parser::{InferredFile, ProviderConfig, ProviderContext, UpstreamState};
use carina_core::provider::{BoxFuture, ProviderFactory, ProviderResult};
use carina_core::resource::{AuthoredResource as Resource, RelativeBindingName, Value};
use carina_core::schema::SchemaRegistry;
use carina_state::StateFile;
use indexmap::IndexMap;

pub(crate) struct ProviderConfigCaptureFactory {
    pub(crate) received_role_arn: Arc<Mutex<Option<Value>>>,
}

impl ProviderFactory for ProviderConfigCaptureFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "provider upstream state capture"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "local".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        *self.received_role_arn.lock().unwrap() = config.attributes().get("role_arn").cloned();
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(carina_provider_mock::MockProvider::new())))
        })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts = vec![carina_core::schema::ResourceSchema::new("test.resource")];
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

pub(crate) fn provider_upstream_fixture() -> (tempfile::TempDir, PathBuf, InferredFile) {
    let temp = tempfile::tempdir().unwrap();
    let upstream = temp.path().join("upstream");
    let downstream = temp.path().join("downstream");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::create_dir_all(&downstream).unwrap();
    std::fs::write(
        upstream.join("main.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    let mut upstream_state = StateFile::new();
    upstream_state.insert_export_json_for_test(
        "role_arn".to_string(),
        serde_json::json!("arn:aws:iam::123456789012:role/upstream"),
    );
    std::fs::write(
        upstream.join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
        carina_core::utils::pretty_with_newline(&upstream_state).unwrap(),
    )
    .unwrap();
    std::fs::write(
        downstream.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("providers.crn"),
        "provider test { role_arn = mgmt.role_arn }\n",
    )
    .unwrap();
    std::fs::write(
        downstream.join("main.crn"),
        "let mgmt = upstream_state { source = \"../upstream\" }\n",
    )
    .unwrap();
    let parsed = load_configuration_with_config(
        &downstream,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .unwrap()
    .parsed
    .into_parsed();
    (temp, downstream, parsed)
}

pub(crate) fn provider_default_tags_local_resource_fixture()
-> (tempfile::TempDir, PathBuf, InferredFile) {
    let temp = tempfile::tempdir().unwrap();
    let base_dir = temp.path().to_path_buf();
    let mut parsed = InferredFile::default();
    parsed.providers.push(ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::from([(
            "Account".to_string(),
            Value::resource_ref("acct", "account_id", vec![]),
        )]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    let mut account = Resource::with_provider("mock", "test.resource", "acct", None);
    account.binding = Some(RelativeBindingName::new("acct"));
    parsed.resources.push(account); // allow: direct — fixture test inspection
    (temp, base_dir, parsed)
}

pub(crate) fn provider_default_tags_missing_upstream_fixture()
-> (tempfile::TempDir, PathBuf, InferredFile) {
    let temp = tempfile::tempdir().unwrap();
    let base_dir = temp.path().to_path_buf();
    let missing = base_dir.join("missing-upstream");
    std::fs::create_dir_all(&missing).unwrap();
    std::fs::write(
        missing.join("main.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();

    let mut parsed = InferredFile::default();
    parsed.providers.push(ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::from([(
            "Owner".to_string(),
            Value::resource_ref("mgmt", "owner", vec![]),
        )]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    parsed.upstream_states.push(UpstreamState {
        binding: RelativeBindingName::new("mgmt"),
        source: PathBuf::from("missing-upstream"),
    });
    (temp, base_dir, parsed)
}
