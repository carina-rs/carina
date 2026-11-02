//! Lock information for state backend locking.

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Default lock timeout in seconds (60 minutes).
pub const DEFAULT_LOCK_TIMEOUT_SECS: i64 = 3600;

fn default_renewal_ttl() -> StdDuration {
    StdDuration::from_secs(DEFAULT_LOCK_TIMEOUT_SECS as u64)
}

/// Information about a state lock.
///
/// The fields are read-only because lock values are ownership capabilities.
/// Backends mint and renew them; callers can inspect but cannot fabricate or
/// duplicate one while a heartbeat owns it.
#[derive(Debug, Serialize)]
pub struct LockInfo {
    /// Unique identifier for this lock.
    id: String,
    /// The operation being performed (e.g., "apply", "destroy", "plan").
    operation: String,
    /// Who acquired the lock (username@hostname).
    who: String,
    /// When the lock was created or most recently renewed.
    created: DateTime<Utc>,
    /// When the lock expires.
    expires: DateTime<Utc>,
    /// In-memory TTL used for subsequent renewals.
    #[serde(skip)]
    renewal_ttl: StdDuration,
}

#[derive(Deserialize)]
struct LockInfoFile {
    id: String,
    operation: String,
    who: String,
    created: DateTime<Utc>,
    expires: DateTime<Utc>,
}

impl LockInfo {
    /// Create a new lock for an operation.
    pub(crate) fn new(operation: impl Into<String>) -> Self {
        Self::with_timeout(operation, DEFAULT_LOCK_TIMEOUT_SECS)
    }

    /// Create a new lock with a custom timeout.
    pub(crate) fn with_timeout(operation: impl Into<String>, timeout_secs: i64) -> Self {
        let now = Utc::now();
        let renewal_ttl = if timeout_secs > 0 {
            StdDuration::from_secs(timeout_secs as u64)
        } else {
            default_renewal_ttl()
        };

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            who: get_lock_owner(),
            created: now,
            expires: now + Duration::seconds(timeout_secs),
            renewal_ttl,
        }
    }

    /// Return the lock's stable unique identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Return the operation that acquired the lock.
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Return the owner recorded when the lock was acquired.
    pub fn who(&self) -> &str {
        &self.who
    }

    /// Return when this lock was created or most recently renewed.
    pub fn created(&self) -> DateTime<Utc> {
        self.created
    }

    /// Return when this lock expires.
    pub fn expires(&self) -> DateTime<Utc> {
        self.expires
    }

    /// Check if the lock has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires
    }

    /// Get the remaining time until expiration.
    pub fn time_remaining(&self) -> Duration {
        self.expires - Utc::now()
    }

    pub(crate) fn renewal_ttl(&self) -> StdDuration {
        self.renewal_ttl
    }

    pub(crate) fn from_json_str(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json).map(Self::from_file)
    }

    pub(crate) fn from_json_slice(json: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(json).map(Self::from_file)
    }

    fn from_file(file: LockInfoFile) -> Self {
        Self {
            id: file.id,
            operation: file.operation,
            who: file.who,
            created: file.created,
            expires: file.expires,
            renewal_ttl: default_renewal_ttl(),
        }
    }

    /// Create a renewed lock with a fresh expiration timestamp.
    ///
    /// The lock ID, operation, and owner remain the same; only the `created`
    /// and `expires` fields are refreshed.
    pub(crate) fn renewed(&self) -> Self {
        self.renewed_at(Utc::now())
    }

    fn renewed_at(&self, now: DateTime<Utc>) -> Self {
        let renewal_ttl = Duration::from_std(self.renewal_ttl)
            .expect("state lock renewal TTL must fit in chrono::Duration");
        Self {
            id: self.id.clone(),
            operation: self.operation.clone(),
            who: self.who.clone(),
            created: now,
            expires: now + renewal_ttl,
            renewal_ttl: self.renewal_ttl,
        }
    }

    /// Fabricate a lock for backend and CLI tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(operation: impl Into<String>) -> Self {
        Self::new(operation)
    }

    /// Fabricate a lock with a short TTL for heartbeat tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test_with_timeout(operation: impl Into<String>, timeout: StdDuration) -> Self {
        let now = Utc::now();
        let timeout =
            Duration::from_std(timeout).expect("test state lock TTL must fit in chrono::Duration");
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            who: get_lock_owner(),
            created: now,
            expires: now + timeout,
            renewal_ttl: timeout
                .to_std()
                .expect("positive test state lock TTL must convert to std::time::Duration"),
        }
    }

    /// Stamp a fake renewal without consulting the real clock.
    #[cfg(test)]
    pub(crate) fn renewed_at_for_test(&self, now: DateTime<Utc>) -> Self {
        self.renewed_at(now)
    }

    /// Produce the value a test backend would return from `renew_lock`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn renewed_for_test(&self) -> Self {
        self.renewed()
    }
}

/// Get the lock owner string (username@hostname).
fn get_lock_owner() -> String {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());

    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());

    format!("{}@{}", username, hostname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_info_new() {
        let lock = LockInfo::new("apply");
        assert_eq!(lock.operation(), "apply");
        assert!(!lock.id().is_empty());
        assert!(!lock.who().is_empty());
        assert!(lock.expires() > lock.created());
    }

    #[test]
    fn test_lock_info_not_expired() {
        let lock = LockInfo::new("apply");
        assert!(!lock.is_expired());
    }

    #[test]
    fn test_lock_info_with_timeout() {
        let lock = LockInfo::with_timeout("apply", 60);
        let remaining = lock.time_remaining();
        assert!(remaining.num_seconds() > 55);
        assert!(remaining.num_seconds() <= 60);
    }

    #[test]
    fn test_lock_owner_format() {
        let who = get_lock_owner();
        assert!(who.contains('@'));
    }

    #[test]
    fn test_lock_info_renewed() {
        let lock = LockInfo::for_test_with_timeout("apply", StdDuration::from_secs(10));
        let renewed = lock.renewed();

        assert_eq!(renewed.id(), lock.id());
        assert_eq!(renewed.operation(), lock.operation());
        assert_eq!(renewed.who(), lock.who());
        assert!(renewed.expires() > lock.expires());
    }

    #[test]
    fn test_lock_info_serialization() {
        let lock = LockInfo::new("apply");
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let deserialized = LockInfo::from_json_str(&json).unwrap();

        assert_eq!(deserialized.id(), lock.id());
        assert_eq!(deserialized.operation(), lock.operation());
        assert_eq!(deserialized.who(), lock.who());
        assert_eq!(deserialized.renewal_ttl(), default_renewal_ttl());
    }
}
