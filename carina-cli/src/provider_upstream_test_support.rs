use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use carina_core::config_loader::load_configuration_with_config;
use carina_core::parser::{InferredFile, ProviderContext};
use carina_core::provider::{BoxFuture, ProviderFactory, ProviderResult};
use carina_core::resource::Value;
use carina_core::schema::SchemaRegistry;
use carina_state::StateFile;

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

    fn create_provider(
        &self,
        _binding: Option<&str>,
        config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        *self.received_role_arn.lock().unwrap() = config.attributes().get("role_arn").cloned();
        Box::pin(async {
            Ok(
                carina_core::provider::InitializeOutcome::unavailable().with_provider(Box::new(
                    carina_provider_mock::ConfiguredBuiltinMock::new(
                        carina_provider_mock::MockBehavior::default(),
                        carina_provider_mock::MockScope::memory(),
                    )
                    .create_provider(),
                )),
            )
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
        &carina_core::paths::ResolvedPath::new(&downstream).unwrap(),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
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
    let parsed = crate::test_support::inferred_fixture(
        "provider mock { default_tags = { Account = acct.account_id } }\nlet acct = mock.test.resource {}\n",
    );
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

    let parsed = crate::test_support::inferred_fixture(
        "provider mock { default_tags = { Owner = mgmt.owner } }\nlet mgmt = upstream_state { source = 'missing-upstream' }\n",
    );
    (temp, base_dir, parsed)
}
