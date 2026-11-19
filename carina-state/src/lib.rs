//! Carina State Management
//!
//! This crate provides state management for the Carina infrastructure tool.
//! It supports storing infrastructure state locally, in S3, and in GitHub
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
//! use carina_state::{BackendSelection, MutationBackendSelection};
//! use std::path::Path;
//!
//! let raw_config = ParserBackendConfig {
//!     backend_type: "s3".to_string(),
//!     attributes: [
//!         ("bucket".to_string(), Value::Concrete(ConcreteValue::String("my-state-bucket".into()))),
//!         ("key".to_string(), Value::Concrete(ConcreteValue::String("infra/prod/carina.state".into()))),
//!         ("region".to_string(), Value::Concrete(ConcreteValue::enum_identifier("aws.Region.ap_northeast_1"))),
//!     ].into_iter().collect(),
//!     attribute_spans: Default::default(),
//! };
//! let selected = BackendSelection::verified_for_mutation(Path::new("."), Some(&raw_config))?;
//! let MutationBackendSelection::Verified(selected) = selected else {
//!     // Initialize or migrate the backend lock before mutating state.
//!     return Err("backend is not ready for mutation".into());
//! };
//! let shutdown = carina_core::shutdown::ShutdownToken::running();
//! let backend = selected.resolve(&shutdown).await?;
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

#![deny(clippy::iter_over_hash_type)]

pub mod backend;
mod backend_config;
pub mod backend_lock;
mod backend_selection;
pub mod backends;
mod heartbeat;
pub mod lock;
mod refresh;
pub mod state;
pub use refresh::{
    PreparedRows, RefreshRows, RefreshTarget, RefreshableCurrent, RefreshableDeposed,
    RefreshableOrphan, RefreshableOrphans, RemovedRows, ResolvedRemovedClaims,
};

// Re-export main types for convenience
pub use backend::{
    BackendError, BackendResult, InvalidResourceIdentityError, LOCAL_BACKEND_TYPE, RawState,
    S3_BACKEND_TYPE, SaveFailureKind, StateBackend, StorageReadiness, StorageReadinessStatus,
    StorageUnavailable, read_published_exports,
};
pub use backend_config::{
    BackendAttributeError, BackendConfig, BackendConfigError, BackendConfigErrors, GitBranch,
    GitHubBackendSettings, LeasePath, LocalBackendSettings, LocalStateLocateError, RepositorySlug,
    S3BackendSettings, StatePath,
};
pub use backend_lock::BackendLock;
pub use backend_selection::{
    BackendConfigurationSource, BackendDrift, BackendDriftStatus, BackendSelection,
    BucketDeleteBackendSelection, BucketDeleteSelectionError, CleanupPermit, MigrationBackends,
    MutationBackendSelection, ProtectedBackendBucket, PublishedBackend, ReadBackendOrigin,
    ReadBackendSelection, ResolvedReadBackend, VerifiedMutationBackendSelection,
    inspect_backend_drift,
};
pub use backends::{
    GitHubBackend, GitHubError, LocalBackend, S3AutoCreate, S3AutoCreateDisabled, S3Bootstrap,
    S3BootstrapInfo, StateUrl, anchored_local_path, load_encoded_published_exports_from_url,
    load_state_from_url,
};
#[cfg(any(test, feature = "test-utils"))]
pub use backends::{local_backend_for_test, s3_backend_for_test};
pub use heartbeat::{HeartbeatGuard, LockHeartbeat, LockLost, LockedStateWriter};
pub use lock::LockInfo;
pub use state::{
    ApplyDecision, CurrentInstance, DeposedAttributes, DeposedInstance, DeposedKey,
    ImportTargetDisposition, LoadedOrphans, NameOverride, NotOutcomeUnknown, NotOutcomeUnknownKind,
    ObservationStateAuthority, ObservedStateFile, OutcomeUnknownRow, PreviousSecretHashAuthority,
    PublishedExports, ResourceState, ResourceStateEncodingError, RowBasis, StateFile, load_state,
    load_state_bytes, should_apply_override,
};

#[cfg(any(test, feature = "test-utils"))]
#[path = "../../carina-core/tests/support/provider_text.rs"]
mod provider_text_fixtures;
