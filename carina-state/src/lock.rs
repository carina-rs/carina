//! Lock information for state backend locking

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Default lock timeout in seconds (60 minutes)
///
/// Long-running operations such as `destroy` can legitimately wait 30+ minutes
/// for timed-out CloudControl deletes, so the TTL must be generous enough to
/// cover those cases without expiring mid-operation.
pub const DEFAULT_LOCK_TIMEOUT_SECS: i64 = 3600;

/// Information about a state lock
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    /// Unique identifier for this lock
    pub id: String,
    /// The operation being performed (e.g., "apply", "destroy", "plan")
    pub operation: String,
    /// Who acquired the lock (username@hostname)
    pub who: String,
    /// When the lock was created
    pub created: DateTime<Utc>,
    /// When the lock expires
    pub expires: DateTime<Utc>,
}

impl LockInfo {
    /// Create a new lock for an operation
    pub fn new(operation: impl Into<String>) -> Self {
        let now = Utc::now();
        let who = get_lock_owner();

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            who,
            created: now,
            expires: now + Duration::seconds(DEFAULT_LOCK_TIMEOUT_SECS),
        }
    }

    /// Create a new lock with a custom timeout
    pub fn with_timeout(operation: impl Into<String>, timeout_secs: i64) -> Self {
        let now = Utc::now();
        let who = get_lock_owner();

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            operation: operation.into(),
            who,
            created: now,
            expires: now + Duration::seconds(timeout_secs),
        }
    }

    /// Check if the lock has expired
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires
    }

    /// Get the remaining time until expiration
    pub fn time_remaining(&self) -> Duration {
        self.expires - Utc::now()
    }

    /// Create a renewed copy of this lock with a fresh expiration timestamp.
    ///
    /// The lock ID, operation, and owner remain the same; only the `created`
    /// and `expires` fields are refreshed.
    pub fn renewed(&self) -> Self {
        let now = Utc::now();
        Self {
            id: self.id.clone(),
            operation: self.operation.clone(),
            who: self.who.clone(),
            created: now,
            expires: now + Duration::seconds(DEFAULT_LOCK_TIMEOUT_SECS),
        }
    }
}

/// Get the lock owner string (username@hostname)
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
        assert_eq!(lock.operation, "apply");
        assert!(!lock.id.is_empty());
        assert!(!lock.who.is_empty());
        assert!(lock.expires > lock.created);
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
        // Should be close to 60 seconds (allowing for test execution time)
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
        let lock = LockInfo::with_timeout("apply", 10);
        let renewed = lock.renewed();

        // Same identity
        assert_eq!(renewed.id, lock.id);
        assert_eq!(renewed.operation, lock.operation);
        assert_eq!(renewed.who, lock.who);

        // Fresh expiration (should be close to DEFAULT_LOCK_TIMEOUT_SECS)
        let remaining = renewed.time_remaining();
        assert!(
            remaining.num_seconds() > DEFAULT_LOCK_TIMEOUT_SECS - 5,
            "renewed lock should have a fresh TTL"
        );
    }

    #[test]
    fn test_lock_info_serialization() {
        let lock = LockInfo::new("apply");
        let json = serde_json::to_string_pretty(&lock).unwrap();
        let deserialized: LockInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, lock.id);
        assert_eq!(deserialized.operation, lock.operation);
        assert_eq!(deserialized.who, lock.who);
    }
}
