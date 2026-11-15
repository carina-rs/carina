//! Lock information for state backend locking.

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// Default lock timeout in seconds (60 minutes).
pub const DEFAULT_LOCK_TIMEOUT_SECS: i64 = 3600;

/// Render the last acquisition or renewal in UTC, truncating to whole seconds.
pub fn format_lock_last_updated(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Render the lock expiry in UTC, rounding up to whole seconds.
/// If rounding would overflow, format the original timestamp instead.
pub fn format_lock_expiry(timestamp: &DateTime<Utc>) -> String {
    let rounded = if timestamp.timestamp_subsec_nanos() == 0 {
        *timestamp
    } else {
        timestamp
            .checked_add_signed(Duration::seconds(1))
            .unwrap_or(*timestamp)
    };
    rounded.to_rfc3339_opts(SecondsFormat::Secs, true)
}

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
    /// When the last acquisition or renewal occurred.
    #[serde(rename = "created")]
    last_updated: DateTime<Utc>,
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
    #[serde(rename = "created")]
    last_updated: DateTime<Utc>,
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
            last_updated: now,
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

    /// Return when the last acquisition or renewal occurred.
    pub fn last_updated(&self) -> DateTime<Utc> {
        self.last_updated
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
            last_updated: file.last_updated,
            expires: file.expires,
            renewal_ttl: default_renewal_ttl(),
        }
    }

    /// Create a renewed lock with a fresh expiration timestamp.
    ///
    /// The lock ID, operation, and owner remain the same; only the `last_updated`
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
            last_updated: now,
            expires: now + renewal_ttl,
            renewal_ttl: self.renewal_ttl,
        }
    }

    /// Fabricate a lock for backend and CLI tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(operation: impl Into<String>) -> Self {
        Self::new(operation)
    }

    /// Fabricate a lock from fixed persisted metadata for diagnostic tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test_from_json(json: &str) -> serde_json::Result<Self> {
        Self::from_json_str(json)
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
            last_updated: now,
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
    fn test_format_lock_last_updated_truncates_subseconds() {
        for timestamp in [
            "2026-11-15T17:12:03.000+09:00",
            "2026-11-15T17:12:03.987+09:00",
        ] {
            let timestamp = DateTime::parse_from_rfc3339(timestamp)
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(format_lock_last_updated(&timestamp), "2026-11-15T08:12:03Z");
        }
    }

    #[test]
    fn test_format_lock_expiry_rounds_up_subseconds() {
        for (timestamp, expected) in [
            ("2026-11-15T18:12:03.000+09:00", "2026-11-15T09:12:03Z"),
            ("2026-11-15T18:12:03.987+09:00", "2026-11-15T09:12:04Z"),
            (
                "2026-11-15T18:12:03.000000001+09:00",
                "2026-11-15T09:12:04Z",
            ),
            ("2026-11-15T23:59:59.987Z", "2026-11-16T00:00:00Z"),
        ] {
            let timestamp = DateTime::parse_from_rfc3339(timestamp)
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(format_lock_expiry(&timestamp), expected);
        }
    }

    #[test]
    fn test_format_lock_expiry_handles_max_timestamp() {
        let timestamp = DateTime::<Utc>::MAX_UTC;
        assert!(timestamp.timestamp_subsec_nanos() > 0);
        assert_eq!(format_lock_expiry(&timestamp), "+262142-12-31T23:59:59Z");
    }

    #[test]
    fn test_lock_info_new() {
        let lock = LockInfo::new("apply");
        assert_eq!(lock.operation(), "apply");
        assert!(!lock.id().is_empty());
        assert!(!lock.who().is_empty());
        assert!(lock.expires() > lock.last_updated());
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
        let renewal_time = lock.last_updated() + Duration::seconds(1);
        let renewed = lock.renewed_at(renewal_time);

        assert_eq!(renewed.id(), lock.id());
        assert_eq!(renewed.operation(), lock.operation());
        assert_eq!(renewed.who(), lock.who());
        assert_eq!(renewed.last_updated(), renewal_time);
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
        assert_eq!(deserialized.last_updated(), lock.last_updated());
        assert_eq!(deserialized.expires(), lock.expires());
        assert_eq!(deserialized.renewal_ttl(), default_renewal_ttl());
    }

    #[test]
    fn test_lock_info_round_trip_preserves_created_json_key() {
        let lock = LockInfo::new("apply");
        let json = serde_json::to_value(&lock).unwrap();
        assert_eq!(
            json["created"],
            serde_json::to_value(lock.last_updated()).unwrap()
        );
        assert!(json.get("last_updated").is_none());
        assert!(json.get("renewal_ttl").is_none());
        let round_tripped = LockInfo::from_json_str(&json.to_string()).unwrap();
        assert_eq!(round_tripped.last_updated(), lock.last_updated());
        assert_eq!(round_tripped.expires(), lock.expires());

        let legacy_json = r#"{
            "id": "legacy-lock",
            "operation": "apply",
            "who": "user@host",
            "created": "2026-11-15T17:12:03.123456+09:00",
            "expires": "2026-11-15T18:12:03.987654+09:00"
        }"#;
        let legacy = LockInfo::from_json_str(legacy_json).unwrap();
        assert_eq!(legacy.id(), "legacy-lock");
        assert_eq!(legacy.operation(), "apply");
        assert_eq!(legacy.who(), "user@host");
        assert_eq!(
            legacy.last_updated().to_rfc3339(),
            "2026-11-15T08:12:03.123456+00:00"
        );
        assert_eq!(
            legacy.expires().to_rfc3339(),
            "2026-11-15T09:12:03.987654+00:00"
        );
        assert_eq!(legacy.renewal_ttl(), default_renewal_ttl());
        let json = serde_json::to_value(&legacy).unwrap();
        assert_eq!(json["created"], "2026-11-15T08:12:03.123456Z");
        assert!(json.get("last_updated").is_none());
    }
}
