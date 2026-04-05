use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A parsed semver version constraint (e.g., "~0.5.0", "^1.2.0").
#[derive(Debug, Clone)]
pub struct VersionConstraint {
    /// Original constraint string from the DSL.
    pub raw: String,
    /// Parsed semver requirement.
    pub req: VersionReq,
}

impl VersionConstraint {
    pub fn parse(s: &str) -> Result<Self, String> {
        let req = VersionReq::parse(s)
            .map_err(|e| format!("Invalid version constraint '{}': {}", s, e))?;
        Ok(Self {
            raw: s.to_string(),
            req,
        })
    }

    /// Check if a version string satisfies this constraint.
    pub fn matches(&self, version: &str) -> Result<bool, String> {
        let ver = semver::Version::parse(version)
            .map_err(|e| format!("Invalid version '{}': {}", version, e))?;
        Ok(self.req.matches(&ver))
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for VersionConstraint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for VersionConstraint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        VersionConstraint::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tilde_constraint() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        assert_eq!(c.raw, "~0.5.0");
        assert!(c.matches("0.5.0").unwrap());
        assert!(c.matches("0.5.9").unwrap());
        assert!(!c.matches("0.6.0").unwrap());
    }

    #[test]
    fn parse_caret_constraint() {
        let c = VersionConstraint::parse("^1.2.0").unwrap();
        assert!(c.matches("1.2.0").unwrap());
        assert!(c.matches("1.9.0").unwrap());
        assert!(!c.matches("2.0.0").unwrap());
    }

    #[test]
    fn parse_exact_version() {
        let c = VersionConstraint::parse("=0.5.0").unwrap();
        assert!(c.matches("0.5.0").unwrap());
        assert!(!c.matches("0.5.1").unwrap());
    }

    #[test]
    fn parse_range_constraint() {
        let c = VersionConstraint::parse(">=0.5.0, <1.0.0").unwrap();
        assert!(c.matches("0.5.0").unwrap());
        assert!(c.matches("0.9.9").unwrap());
        assert!(!c.matches("1.0.0").unwrap());
    }

    #[test]
    fn parse_star_constraint() {
        let c = VersionConstraint::parse("*").unwrap();
        assert!(c.matches("0.1.0").unwrap());
        assert!(c.matches("99.0.0").unwrap());
    }

    #[test]
    fn parse_invalid_constraint() {
        assert!(VersionConstraint::parse("not-a-version").is_err());
    }

    #[test]
    fn display_shows_raw() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        assert_eq!(format!("{c}"), "~0.5.0");
    }

    #[test]
    fn serde_roundtrip() {
        let c = VersionConstraint::parse("~0.5.0").unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"~0.5.0\"");
        let c2: VersionConstraint = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.raw, "~0.5.0");
    }
}
