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
//! use carina_state::{create_backend, BackendConfig};
//!
//! let config = BackendConfig {
//!     backend_type: "s3".to_string(),
//!     attributes: [
//!         ("bucket".to_string(), Value::String("my-state-bucket".to_string())),
//!         ("key".to_string(), Value::String("infra/prod/carina.state".to_string())),
//!         ("region".to_string(), Value::String("aws.Region.ap_northeast_1".to_string())),
//!     ].into_iter().collect(),
//! };
//!
//! let backend = create_backend(&config).await?;
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

pub mod backend;
pub mod backends;
pub mod lock;
pub mod state;

// Re-export main types for convenience
pub use backend::{BackendConfig, BackendError, BackendResult, StateBackend};
pub use backends::create_backend;
pub use lock::LockInfo;
pub use state::{ResourceState, StateFile};
