//! Carina State Management
//!
//! This crate provides state management for the Carina infrastructure tool.
//! It supports storing infrastructure state in various backends (currently S3)
//! with locking support for safe concurrent access.
//!
//! # Overview
//!
//! The state management system consists of:
//!
//! - **StateFile**: The main state structure containing all managed resources
//! - **StateBackend**: A trait for state storage backends (S3, GCS, local, etc.)
//! - **LockInfo**: Information about state locks for concurrent access control
//!
//! # Example
//!
//! ```ignore
//! use carina_core::parser::BackendConfig as ParserBackendConfig;
//! use carina_core::resource::{ConcreteValue, Value};
//! use carina_state::{BackendConfig, create_backend};
//! use std::collections::HashMap;
//! use std::path::Path;
//!
//! let raw_config = ParserBackendConfig {
//!     backend_type: "s3".to_string(),
//!     attributes: [
//!         ("bucket".to_string(), Value::Concrete(ConcreteValue::String("my-state-bucket".into()))),
//!         ("key".to_string(), Value::Concrete(ConcreteValue::String("infra/prod/carina.state".into()))),
//!         ("region".to_string(), Value::Concrete(ConcreteValue::enum_identifier("aws.Region.ap_northeast_1"))),
//!     ].into_iter().collect(),
//!     attribute_spans: HashMap::new(),
//! };
//! let config = BackendConfig::try_from(&raw_config)?;
//!
//! let backend = create_backend(Some(&config), Path::new(".")).await?;
//!
//! // Acquire lock before modifying state
//! let lock = backend.acquire_lock("apply").await?;
//!
//! // Read current state
//! let state = backend.read_state().await?;
//!
//! // ... modify resources ...
//!
//! // Write updated state
//! backend.write_state(&state).await?;
//!
//! // Release lock
//! backend.release_lock(&lock).await?;
//! ```

#[cfg(test)]
fn fixture_applied_identifier(
    identifier: impl Into<String>,
) -> carina_core::provider::AppliedIdentifier {
    carina_core::provider::AppliedIdentifier::new(identifier)
        .expect("fixture provider identifier must be non-empty")
}

pub mod backend;
mod backend_config;
pub mod backend_lock;
pub mod backends;
mod heartbeat;
pub mod lock;
pub mod state;

// Re-export main types for convenience
pub use backend::{
    BackendError, BackendResult, InvalidResourceIdentityError, LOCAL_BACKEND_TYPE, RawState,
    S3_BACKEND_TYPE, StateBackend, read_published_exports,
};
pub use backend_config::{
    BackendAttributeError, BackendConfig, BackendConfigError, BackendConfigErrors,
    LocalBackendSettings, S3BackendSettings,
};
pub use backend_lock::BackendLock;
pub use backends::{
    LocalBackend, StateUrl, anchored_local_path, create_backend, create_remote_backend,
    load_published_exports_from_url, load_state_from_url, resolve_backend_for_read,
};
pub use heartbeat::{HeartbeatGuard, LockHeartbeat, LockLost, LockedStateWriter};
pub use lock::LockInfo;
pub use state::{
    ApplyDecision, CurrentInstance, DeposedAttributes, DeposedInstance, DeposedKey, LoadedOrphans,
    LoadedState, MigratedStateFile, MigrationInfo, NameOverride, PreviousSecretHashAuthority,
    PublishedExports, ResourceState, ResourceStateEncodingError, RowBasis, StateFile,
    check_and_migrate, check_and_migrate_bytes, log_state_migration_once, should_apply_override,
};
