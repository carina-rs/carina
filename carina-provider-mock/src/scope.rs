//! Explicit storage ownership and transaction synchronization for builtin mocks.
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

use crate::{MockObservations, MockStates};

#[derive(Clone)]
pub struct MockScope(Arc<ScopeInner>);

struct ScopeInner {
    store: Store,
    transaction: Mutex<()>,
}

// This identity map shares synchronization for an external file; it does not
// hold test configuration. Weak entries let unused scopes drop.
static FILE_SCOPES: LazyLock<Mutex<HashMap<PathBuf, Weak<ScopeInner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

enum Store {
    Memory(Mutex<MockStates>),
    File(PathBuf),
}

pub(crate) struct ScopeLock<'a> {
    store: &'a Store,
    _transaction: MutexGuard<'a, ()>,
}

impl MockScope {
    pub fn memory() -> Self {
        Self::new(Store::Memory(Mutex::new(MockStates::new())))
    }

    pub fn file(path: PathBuf) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mock state path must be absolute",
            ));
        }
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "mock state path has no file name",
            )
        })?;
        let mut parent = PathBuf::new();
        for component in path.parent().unwrap().components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    parent.pop();
                }
                _ => {
                    parent.push(component);
                    match parent.canonicalize() {
                        Ok(canonical) => parent = canonical,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        let path = parent.join(file_name);
        let mut scopes = FILE_SCOPES
            .lock()
            .map_err(|_| io::Error::other("mock scope identity map poisoned"))?;
        scopes.retain(|_, scope| scope.strong_count() > 0);
        if let Some(scope) = scopes.get(&path).and_then(Weak::upgrade) {
            return Ok(Self(scope));
        }
        let scope = Self::new(Store::File(path.clone()));
        scopes.insert(path, Arc::downgrade(&scope.0));
        Ok(scope)
    }

    fn new(store: Store) -> Self {
        Self(Arc::new(ScopeInner {
            store,
            transaction: Mutex::new(()),
        }))
    }

    pub(crate) fn lock(&self) -> io::Result<ScopeLock<'_>> {
        let transaction = self
            .0
            .transaction
            .lock()
            .map_err(|_| io::Error::other("mock state lock poisoned"))?;
        Ok(ScopeLock {
            store: &self.0.store,
            _transaction: transaction,
        })
    }
}

impl ScopeLock<'_> {
    pub(crate) fn load(&self) -> io::Result<MockStates> {
        match self.store {
            Store::Memory(states) => Ok(states
                .lock()
                .map_err(|_| io::Error::other("mock store poisoned"))?
                .clone()),
            Store::File(path) => match fs::read_to_string(path) {
                Ok(content) => crate::MockProvider::decode_state_file(&content),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MockStates::new()),
                Err(error) => Err(error),
            },
        }
    }

    pub(crate) fn save(&self, states: &MockStates) -> io::Result<()> {
        match self.store {
            Store::Memory(store) => {
                *store
                    .lock()
                    .map_err(|_| io::Error::other("mock store poisoned"))? = states.clone();
                Ok(())
            }
            Store::File(path) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let content = carina_core::utils::pretty_with_newline(&crate::MockStateFileRef {
                    version: crate::MOCK_STATE_VERSION,
                    states,
                })?;
                let temporary = path.with_file_name(format!(
                    ".{}.tmp-{}",
                    path.file_name().unwrap().to_string_lossy(),
                    uuid::Uuid::new_v4()
                ));
                let result = (|| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)?;
                    file.write_all(content.as_bytes())?;
                    file.sync_all()?;
                    fs::rename(&temporary, path)
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temporary);
                }
                result
            }
        }
    }
}

/// Update observations belong to one configured invocation, independent of its store.
#[derive(Default)]
pub(crate) struct UpdateTelemetry(Mutex<UpdateCounts>);

#[derive(Default)]
struct UpdateCounts {
    active: usize,
    maximum: usize,
}

impl UpdateTelemetry {
    pub(crate) fn new(observations: &MockObservations) -> Self {
        if let Some(path) = &observations.max_active {
            write_observation(path, "0");
        }
        if let Some(path) = &observations.update_trace {
            write_observation(path, "");
        }
        Self::default()
    }

    pub(crate) fn enter_update(
        &self,
        resource: String,
        observations: &MockObservations,
    ) -> ActiveUpdateGuard<'_> {
        let mut telemetry = self.0.lock().unwrap();
        telemetry.active += 1;
        telemetry.maximum = telemetry.maximum.max(telemetry.active);
        if let Some(path) = &observations.max_active {
            write_observation(path, &telemetry.maximum.to_string());
        }
        append_trace(
            &observations.update_trace,
            "start",
            &resource,
            Some(telemetry.active),
        );
        ActiveUpdateGuard {
            telemetry: self,
            resource,
            trace: observations.update_trace.clone(),
        }
    }
}

fn append_trace(path: &Option<PathBuf>, event: &str, resource: &str, active: Option<usize>) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let line = match active {
            Some(active) => format!("{event} {resource} {active}\n"),
            None => format!("{event} {resource}\n"),
        };
        let _ = file.write_all(line.as_bytes());
    }
}

fn write_observation(path: &Path, value: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, value);
}

pub(crate) struct ActiveUpdateGuard<'a> {
    telemetry: &'a UpdateTelemetry,
    resource: String,
    trace: Option<PathBuf>,
}
impl Drop for ActiveUpdateGuard<'_> {
    fn drop(&mut self) {
        let mut telemetry = self.telemetry.0.lock().unwrap();
        append_trace(&self.trace, "finish", &self.resource, None);
        telemetry.active -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConfiguredBuiltinMock, MockBehavior};

    #[test]
    fn separately_opened_file_scopes_share_arc_and_preserve_concurrent_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let first = MockScope::file(path.clone()).unwrap();
        let second = MockScope::file(path).unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));

        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for (worker, scope) in [&first, &second].into_iter().enumerate() {
                let barrier = &barrier;
                threads.spawn(move || {
                    barrier.wait();
                    for item in 0..32 {
                        let transaction = scope.lock().unwrap();
                        let mut states = transaction.load().unwrap();
                        states.insert(format!("worker-{worker}-{item}"), HashMap::new());
                        std::thread::yield_now();
                        transaction.save(&states).unwrap();
                    }
                });
            }
        });
        assert_eq!(first.lock().unwrap().load().unwrap().len(), 64);
    }

    #[test]
    fn file_scope_normalizes_parent_paths_before_and_after_directory_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing/state.json");
        let scope = MockScope::file(path.clone()).unwrap();
        let canonical_parent = dir.path().canonicalize().unwrap();
        let alias =
            MockScope::file(canonical_parent.join("missing/../missing/state.json")).unwrap();
        assert!(Arc::ptr_eq(&scope.0, &alias.0));
        fs::create_dir(dir.path().join("missing")).unwrap();
        let reopened = MockScope::file(path).unwrap();
        assert!(Arc::ptr_eq(&scope.0, &reopened.0));

        let weak = Arc::downgrade(&scope.0);
        drop((scope, alias, reopened));
        assert!(weak.upgrade().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_scope_shares_identity_through_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let scope = MockScope::file(dir.path().join("state.json")).unwrap();
        let alias = MockScope::file(link.join("state.json")).unwrap();
        assert!(Arc::ptr_eq(&scope.0, &alias.0));
    }

    #[test]
    fn file_scope_reports_relative_paths_and_missing_file_names_separately() {
        for (path, expected) in [
            ("relative.json", "mock state path must be absolute"),
            ("/", "mock state path has no file name"),
        ] {
            let error = MockScope::file(PathBuf::from(path)).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn aliases_from_one_configured_mock_share_update_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let maximum = dir.path().join("maximum");
        let trace = dir.path().join("trace");
        let behavior = MockBehavior {
            observations: MockObservations {
                max_active: Some(maximum.clone()),
                update_trace: Some(trace.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let configured = ConfiguredBuiltinMock::new(behavior.clone(), MockScope::memory());
        let mock_alias = configured.create_provider();
        let mock_active = mock_alias
            .telemetry
            .enter_update("mock".into(), &behavior.observations);
        let default_alias = configured.clone().create_provider();
        assert_eq!(fs::read_to_string(&maximum).unwrap(), "1");
        assert_eq!(fs::read_to_string(&trace).unwrap(), "start mock 1\n");

        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let _active = default_alias
                    .telemetry
                    .enter_update("default".into(), &behavior.observations);
                barrier.wait();
                barrier.wait();
            });
            barrier.wait();
            let observed_maximum = fs::read_to_string(&maximum).unwrap();
            let observed_trace = fs::read_to_string(&trace).unwrap();
            barrier.wait();
            assert_eq!(observed_maximum, "2");
            assert_eq!(observed_trace, "start mock 1\nstart default 2\n");
        });
        drop(mock_active);
        let _active = default_alias
            .telemetry
            .enter_update("later".into(), &behavior.observations);
        assert_eq!(fs::read_to_string(&maximum).unwrap(), "2");
    }

    #[test]
    fn fresh_configured_mock_on_same_scope_resets_update_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let maximum = dir.path().join("maximum");
        let trace = dir.path().join("trace");
        let behavior = MockBehavior {
            observations: MockObservations {
                max_active: Some(maximum.clone()),
                update_trace: Some(trace.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let scope = MockScope::memory();
        let first_invocation = ConfiguredBuiltinMock::new(behavior.clone(), scope.clone());
        let first = first_invocation.create_provider();
        let a = first
            .telemetry
            .enter_update("first-a".into(), &behavior.observations);
        let b = first
            .telemetry
            .enter_update("first-b".into(), &behavior.observations);
        assert_eq!(fs::read_to_string(&maximum).unwrap(), "2");
        drop((a, b));
        assert_eq!(fs::read_to_string(&trace).unwrap().lines().count(), 4);

        let second_invocation = ConfiguredBuiltinMock::new(behavior.clone(), scope);
        assert_eq!(fs::read_to_string(&maximum).unwrap(), "0");
        assert_eq!(fs::read_to_string(&trace).unwrap(), "");
        let second = second_invocation.create_provider();
        let active = second
            .telemetry
            .enter_update("second".into(), &behavior.observations);
        drop(active);
        assert_eq!(fs::read_to_string(&maximum).unwrap(), "1");
        assert_eq!(
            fs::read_to_string(&trace).unwrap(),
            "start second 1\nfinish second\n"
        );
    }
}
