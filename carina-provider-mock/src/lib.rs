use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use carina_core::effect::PlanOp;
use carina_core::provider::{
    BoxFuture, CreateOutcome, CreateRequest, DeleteRequest, PatchOpKind, Provider, ProviderError,
    ProviderResult, ReadRequest, UpdateOutcome, UpdateRequest,
};
use carina_core::resource::{ConcreteValue, DataSource, Resource, ResourceId, State, Value};
use carina_core::value::{json_to_dsl_value, value_to_json};

pub struct MockProvider {
    state_file: PathBuf,
    partial_create: Option<PartialConfig>,
    partial_update: Option<PartialConfig>,
}

#[derive(Clone, Debug)]
struct PartialConfig {
    resource_id_pattern: String,
    missing_attributes: Vec<String>,
}

static ACTIVE_UPDATES: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE_UPDATES: AtomicUsize = AtomicUsize::new(0);

struct ActiveUpdateGuard;

impl ActiveUpdateGuard {
    fn enter() -> Self {
        let active = ACTIVE_UPDATES.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_ACTIVE_UPDATES.fetch_max(active, Ordering::SeqCst);
        write_max_active();
        Self
    }
}

impl Drop for ActiveUpdateGuard {
    fn drop(&mut self) {
        ACTIVE_UPDATES.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MockProvider {
    pub fn new() -> Self {
        let state_file = env::var_os("CARINA_MOCK_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".carina/state.json"));
        if env::var_os("CARINA_MOCK_MAX_ACTIVE_PATH").is_some() {
            ACTIVE_UPDATES.store(0, Ordering::SeqCst);
            MAX_ACTIVE_UPDATES.store(0, Ordering::SeqCst);
            write_max_active();
        }
        Self {
            state_file,
            partial_create: partial_create_config_from_env(),
            partial_update: partial_update_config_from_env(),
        }
    }

    pub fn with_partial_create_for(
        mut self,
        resource_id_pattern: impl Into<String>,
        missing_attributes: Vec<String>,
    ) -> Self {
        self.partial_create = Some(PartialConfig {
            resource_id_pattern: resource_id_pattern.into(),
            missing_attributes,
        });
        self
    }

    pub fn with_partial_update_for(
        mut self,
        resource_id_pattern: impl Into<String>,
        missing_attributes: Vec<String>,
    ) -> Self {
        self.partial_update = Some(PartialConfig {
            resource_id_pattern: resource_id_pattern.into(),
            missing_attributes,
        });
        self
    }

    fn load_states(&self) -> HashMap<String, HashMap<String, serde_json::Value>> {
        if let Ok(content) = fs::read_to_string(&self.state_file) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    fn save_states(
        &self,
        states: &HashMap<String, HashMap<String, serde_json::Value>>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = carina_core::utils::pretty_with_newline(states)?;
        fs::write(&self.state_file, content)
    }

    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.identity_or_empty())
    }

    fn partial_create_config_for(&self, id: &ResourceId) -> Option<&PartialConfig> {
        let config = self.partial_create.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_update_config_for(&self, id: &ResourceId) -> Option<&PartialConfig> {
        let config = self.partial_update.as_ref()?;
        Self::partial_config_matches(config, id).then_some(config)
    }

    fn partial_config_matches(config: &PartialConfig, id: &ResourceId) -> bool {
        let full = format!(
            "{}.{}.{}",
            id.provider,
            id.resource_type,
            id.identity_or_empty()
        );
        let short = Self::resource_key(id);
        config.resource_id_pattern == "*"
            || config.resource_id_pattern == full
            || config.resource_id_pattern == short
    }

    fn append_delete_log(path: PathBuf, id: &ResourceId) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", Self::resource_key(id))
    }

    fn append_op_log(path: PathBuf, op: &str, id: &ResourceId) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{} {}", op, Self::resource_key(id))
    }

    fn append_op_log_if_configured(op: &str, id: &ResourceId) -> ProviderResult<()> {
        if let Some(path) = env::var_os("CARINA_MOCK_OP_LOG").map(PathBuf::from) {
            Self::append_op_log(path, op, id).map_err(|e| {
                ProviderError::internal("Failed to append operation log").with_cause(e)
            })?;
        }
        Ok(())
    }

    fn create_fail_error_for(id: &ResourceId) -> Option<ProviderError> {
        let target = env::var("CARINA_MOCK_CREATE_FAIL_FOR").ok()?;
        let key = Self::resource_key(id);
        (target == key).then(|| {
            ProviderError::internal(format!(
                "CARINA_MOCK_CREATE_FAIL_FOR requested create failure for {key}"
            ))
        })
    }

    fn test_resource_identifier(id: &ResourceId, name: Option<&str>) -> Option<String> {
        (id.resource_type == "test.resource"
            && env::var_os("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA").is_some())
        .then(|| format!("{}-id", name.unwrap_or_else(|| id.identity_or_empty())))
    }

    fn populate_test_resource_identifier_json(
        id: &ResourceId,
        attrs: &mut HashMap<String, serde_json::Value>,
    ) {
        let name = attrs
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(identifier) = Self::test_resource_identifier(id, name.as_deref()) {
            attrs
                .entry("identifier".to_string())
                .or_insert_with(|| serde_json::Value::String(identifier));
        }
    }

    fn populate_test_resource_identifier_value(
        id: &ResourceId,
        attrs: &mut HashMap<String, Value>,
    ) {
        let name = attrs
            .get("name")
            .and_then(|value| match value {
                Value::Concrete(ConcreteValue::String(name)) => Some(name.as_str()),
                _ => None,
            })
            .map(str::to_string);
        if let Some(identifier) = Self::test_resource_identifier(id, name.as_deref()) {
            attrs
                .entry("identifier".to_string())
                .or_insert_with(|| Value::Concrete(ConcreteValue::String(identifier)));
        }
    }

    fn delete_delay_for(id: &ResourceId) -> Option<Duration> {
        let target = env::var("CARINA_MOCK_DELETE_DELAY_MS_FOR").ok()?;
        if target != Self::resource_key(id) {
            return None;
        }
        env::var("CARINA_MOCK_DELETE_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|millis| *millis > 0)
            .map(Duration::from_millis)
    }

    async fn create_resource(
        &self,
        id: ResourceId,
        resource: Resource,
    ) -> ProviderResult<CreateOutcome> {
        if let Some(err) = Self::create_fail_error_for(&id) {
            return Err(err);
        }

        let mut states = self.load_states();
        let key = Self::resource_key(&id);

        let mut attrs: HashMap<String, serde_json::Value> = resource
            .attributes
            .iter()
            .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
            .collect::<Result<_, _>>()
            .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)))?;
        Self::populate_test_resource_identifier_json(&id, &mut attrs);

        let partial_create = self.partial_create_config_for(&id);
        if let Some(config) = partial_create {
            for attr in &config.missing_attributes {
                attrs.remove(attr);
            }
        }

        states.insert(key, attrs);
        self.save_states(&states)
            .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

        let mut state_attrs = resource.resolved_attributes();
        Self::populate_test_resource_identifier_value(&id, &mut state_attrs);
        let mut state = State::existing(id.clone(), state_attrs).with_identifier("mock-id");
        if let Some(config) = partial_create {
            for attr in &config.missing_attributes {
                state.attributes.remove(attr);
            }
            Self::append_op_log_if_configured("create", &id)?;
            return Ok(CreateOutcome::partial_success(
                state,
                "mock partial create".to_string(),
                config.missing_attributes.clone(),
            ));
        }

        Self::append_op_log_if_configured("create", &id)?;
        Ok(CreateOutcome::Success { state })
    }
}

fn partial_create_config_from_env() -> Option<PartialConfig> {
    let resource_id_pattern = env::var("CARINA_MOCK_PARTIAL_CREATE_FOR").ok()?;
    let missing_attributes = env::var("CARINA_MOCK_PARTIAL_CREATE_MISSING")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["computed".to_string()]);
    Some(PartialConfig {
        resource_id_pattern,
        missing_attributes,
    })
}

fn partial_update_config_from_env() -> Option<PartialConfig> {
    let resource_id_pattern = env::var("CARINA_MOCK_PARTIAL_UPDATE_FOR").ok()?;
    let missing_attributes = env::var("CARINA_MOCK_PARTIAL_UPDATE_MISSING")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec!["computed".to_string()]);
    Some(PartialConfig {
        resource_id_pattern,
        missing_attributes,
    })
}

fn update_delay() -> Option<Duration> {
    env::var("CARINA_MOCK_UPDATE_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

fn write_max_active() {
    let Some(path) = env::var_os("CARINA_MOCK_MAX_ACTIVE_PATH").map(PathBuf::from) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, MAX_ACTIVE_UPDATES.load(Ordering::SeqCst).to_string());
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            let states = self.load_states();
            let key = Self::resource_key(&id);

            if let Some(attrs) = states.get(&key) {
                let attributes: HashMap<String, Value> = attrs
                    .iter()
                    .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                    .collect();
                Ok(State::existing(id, attributes).with_identifier("mock-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<CreateOutcome>> {
        let id = id.clone();
        let resource = request.resource.as_resource().clone();
        Box::pin(async move { self.create_resource(id, resource).await })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move {
            let _active = ActiveUpdateGuard::enter();
            if let Some(delay) = update_delay() {
                tokio::time::sleep(delay).await;
            }
            // Apply the patch on top of `from` to construct the post-update
            // attribute map. The mock writes only what the user changed —
            // matching the Level 3 contract that providers MUST NOT touch
            // unspecified fields.
            let mut attributes = request.from.attributes.clone();
            for op in request.patch.ops {
                match op.kind {
                    PatchOpKind::Add | PatchOpKind::Replace => {
                        if let Some(value) = op.value {
                            attributes.insert(op.key, value);
                        }
                    }
                    PatchOpKind::Remove => {
                        attributes.remove(&op.key);
                    }
                }
            }

            let mut states = self.load_states();
            let key = Self::resource_key(&id);
            let partial_update = self.partial_update_config_for(&id);
            let mut attrs: HashMap<String, serde_json::Value> = attributes
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect::<Result<_, _>>()
                .map_err(|e| ProviderError::internal(format!("Failed to convert value: {}", e)))?;
            if let Some(config) = partial_update {
                for attr in &config.missing_attributes {
                    attrs.remove(attr);
                }
            }

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

            let mut state = State::existing(id, attributes).with_identifier("mock-id");
            if let Some(config) = partial_update {
                for attr in &config.missing_attributes {
                    state.attributes.remove(attr);
                }
                Self::append_op_log_if_configured("update", &state.id)?;
                return Ok(UpdateOutcome::partial_success(
                    state,
                    "mock partial update".to_string(),
                    config.missing_attributes.clone(),
                ));
            }

            Self::append_op_log_if_configured("update", &state.id)?;
            Ok(UpdateOutcome::Success { state })
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            if let Some(delay) = Self::delete_delay_for(&id) {
                tokio::time::sleep(delay).await;
            }

            if let Some(path) = env::var_os("CARINA_MOCK_DELETE_LOG").map(PathBuf::from) {
                Self::append_delete_log(path, &id).map_err(|e| {
                    ProviderError::internal("Failed to append delete log").with_cause(e)
                })?;
            }

            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            states.remove(&key);
            self.save_states(&states)
                .map_err(|e| ProviderError::internal("Failed to save state").with_cause(e))?;

            Self::append_op_log_if_configured("delete", &id)?;
            Ok(())
        })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: PlanOp) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;
    use std::time::Instant;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = env::var_os(name);
            // SAFETY: EnvGuard is used only while ENV_LOCK is held in these tests.
            unsafe {
                env::set_var(name, value);
            }
            Self { name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let previous = env::var_os(name);
            // SAFETY: EnvGuard is used only while ENV_LOCK is held in these tests.
            unsafe {
                env::remove_var(name);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: EnvGuard values are declared after the ENV_LOCK guard, so
            // they are dropped before the lock is released.
            unsafe {
                if let Some(previous) = &self.previous {
                    env::set_var(self.name, previous);
                } else {
                    env::remove_var(self.name);
                }
            }
        }
    }

    fn string_value(value: impl Into<String>) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    // Pin the byte-level shape so MockProvider's state file matches
    // the trailing-newline convention used by carina.state.json
    // (#2721) and carina-backend.lock (#2583).
    #[test]
    fn state_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file: state_file.clone(),
            partial_create: None,
            partial_update: None,
        };
        let mut states = HashMap::new();
        let mut entry = HashMap::new();
        entry.insert("k".to_string(), serde_json::json!("v"));
        states.insert("aws.s3.Bucket.b".to_string(), entry);
        provider.save_states(&states).unwrap();
        let bytes = fs::read(&state_file).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "MockProvider state file must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[test]
    fn required_permissions_returns_empty_vec() {
        let provider = MockProvider::default();
        let id = ResourceId::with_provider_identity("mock", "foo", "example", None);
        assert_eq!(
            provider.required_permissions(&id, carina_core::effect::PlanOp::Create),
            Vec::<String>::new()
        );
    }

    #[test]
    fn append_delete_log_records_resource_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("delete.log");
        let first = ResourceId::with_provider_identity("mock", "test.resource", "first", None);
        let second = ResourceId::with_provider_identity("mock", "test.resource", "second", None);

        MockProvider::append_delete_log(log_path.clone(), &first).unwrap();
        MockProvider::append_delete_log(log_path.clone(), &second).unwrap();

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "test.resource.first\ntest.resource.second\n"
        );
    }

    #[test]
    fn op_log_records_successful_create_update_delete_in_order() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let _create_fail = EnvGuard::unset("CARINA_MOCK_CREATE_FAIL_FOR");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
        let id = ResourceId::with_provider_identity("mock", "test.resource", "web-acl-new", None);
        let resource = Resource::with_provider("mock", "test.resource", "web-acl-new", None)
            .with_attribute("name", string_value("web-acl-new"))
            .with_attribute("comment", string_value("v1"));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            provider
                .create_resource(id.clone(), resource)
                .await
                .expect("create should succeed");

            let mut from_attrs = HashMap::new();
            from_attrs.insert("name".to_string(), string_value("web-acl-new"));
            from_attrs.insert("comment".to_string(), string_value("v1"));
            let from = State::existing(id.clone(), from_attrs).with_identifier("mock-id");
            provider
                .update(
                    &id,
                    "mock-id",
                    UpdateRequest {
                        from,
                        patch: carina_core::provider::UpdatePatch {
                            ops: vec![carina_core::provider::PatchOp {
                                kind: PatchOpKind::Replace,
                                key: "comment".to_string(),
                                value: Some(string_value("v2")),
                            }],
                        },
                    },
                )
                .await
                .expect("update should succeed");

            provider
                .delete(&id, "mock-id", DeleteRequest::default())
                .await
                .expect("delete should succeed");
        });

        assert_eq!(
            fs::read_to_string(log_path).unwrap(),
            "create test.resource.web-acl-new\n\
             update test.resource.web-acl-new\n\
             delete test.resource.web-acl-new\n"
        );
    }

    #[test]
    fn create_fail_for_returns_error_without_op_log_entry() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let log_path = tmp.path().join("op.log");
        let _op_log = EnvGuard::set("CARINA_MOCK_OP_LOG", log_path.as_os_str());
        let _create_fail = EnvGuard::set("CARINA_MOCK_CREATE_FAIL_FOR", "test.resource.blocked");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
        let id = ResourceId::with_provider_identity("mock", "test.resource", "blocked", None);
        let resource = Resource::with_provider("mock", "test.resource", "blocked", None)
            .with_attribute("name", string_value("blocked"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        let err = runtime
            .block_on(provider.create_resource(id, resource))
            .expect_err("create should fail when CARINA_MOCK_CREATE_FAIL_FOR matches");
        let message = err.to_string();
        assert!(
            message.contains("CARINA_MOCK_CREATE_FAIL_FOR")
                && message.contains("test.resource.blocked"),
            "create failure should mention env var and resource key, got: {message}"
        );

        let log = fs::read_to_string(log_path).unwrap_or_default();
        assert!(
            !log.lines()
                .any(|line| line == "create test.resource.blocked"),
            "failed create must not emit CARINA_MOCK_OP_LOG entry, got: {log:?}"
        );
    }

    #[test]
    fn delete_delay_env_var_delays_matching_resource_delete() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let state_file = tmp.path().join("mock-state.json");
        let provider = MockProvider {
            state_file,
            partial_create: None,
            partial_update: None,
        };
        let id = ResourceId::with_provider_identity("mock", "test.resource", "slow", None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        // SAFETY: This test serializes access to the process environment with
        // ENV_LOCK and clears the variables before releasing the lock.
        unsafe {
            env::set_var("CARINA_MOCK_DELETE_DELAY_MS_FOR", "test.resource.slow");
            env::set_var("CARINA_MOCK_DELETE_DELAY_MS", "25");
        }

        let started = Instant::now();
        runtime
            .block_on(provider.delete(&id, "mock-id", DeleteRequest::default()))
            .unwrap();
        let elapsed = started.elapsed();

        // SAFETY: See the set_var safety note above; ENV_LOCK is still held.
        unsafe {
            env::remove_var("CARINA_MOCK_DELETE_DELAY_MS_FOR");
            env::remove_var("CARINA_MOCK_DELETE_DELAY_MS");
        }

        assert!(
            elapsed >= Duration::from_millis(25),
            "matching delete should be delayed by at least 25ms, elapsed {elapsed:?}"
        );
    }
}
