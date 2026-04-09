//! Provider resolution: download, extract, cache, and verify provider binaries.

pub mod provider_resolver;
pub mod revision_resolver;
pub mod version_resolver;

pub use provider_resolver::*;
pub use version_resolver::{
    ResolvedVersion, fetch_latest_tag, fetch_release_tags, resolve_from_tags,
};
