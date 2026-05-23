//! Structured identity of a provider-defined custom type.
//!
//! Replaces the flat `semantic_name: Option<String>` that
//! `AttributeType::Custom` previously carried. A flat string conflated
//! the type-identity key with the display name and had no provider
//! axis, so two providers exposing a same-named type (`aws.Region` vs a
//! future `gcp.Region`, different formats) collided: the type system
//! treated them as the same type because their `semantic_name` strings
//! were equal.
//!
//! [`TypeIdentity`] keys identity on discrete axes —
//! `provider + segments + kind`. Identity equality is **per-axis**: two
//! identities denote the same type iff every *populated* axis is equal.
//! An empty axis (`provider: None`, or an empty `segments`) means "not
//! distinguished on this axis" — a wider type, higher in the base
//! chain — not a wildcard.
//!
//! The dotted display form (`aws.iam.Role.Arn`) is *derived* from the
//! structure via [`Display`]; it is never the source of truth and is
//! never parsed back into axes.
//!
//! See `notes/specs/2026-05-16-semantic-name-redesign-design.md`.

use std::fmt;

/// Structured identity of an `AttributeType::Custom`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TypeIdentity {
    /// Provider segment, e.g. `Some("aws")`. `None` when the type
    /// carries no provider axis — a truly provider-agnostic wrapper
    /// (e.g. the built-in `Ipv4Cidr`). A `None` provider is *wider*
    /// than any `Some`: it does not distinguish on the provider axis.
    pub provider: Option<String>,
    /// Service / resource segments between the provider and the kind,
    /// e.g. `["iam", "Role"]` for `aws.iam.Role.Arn`. Empty for a
    /// provider-scoped but resource-agnostic type such as the generic
    /// `aws.Arn`.
    pub segments: Vec<String>,
    /// The type's own name, e.g. `"Arn"`, `"VpcId"`, `"Region"`.
    pub kind: String,
}

impl TypeIdentity {
    /// Construct an identity from its three axes.
    pub fn new(
        provider: Option<impl Into<String>>,
        segments: impl IntoIterator<Item = impl Into<String>>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.map(Into::into),
            segments: segments.into_iter().map(Into::into).collect(),
            kind: kind.into(),
        }
    }

    /// A provider-agnostic identity carrying only a `kind` — no provider
    /// axis, no service/resource segments. Used for the built-in DSL
    /// custom types (`Ipv4Cidr`, `Email`, …) that are not owned by any
    /// provider.
    pub fn bare(kind: impl Into<String>) -> Self {
        Self {
            provider: None,
            segments: Vec::new(),
            kind: kind.into(),
        }
    }

    /// Parse a dotted identity string produced by a provider's
    /// serialized schema (`carina-provider-protocol`).
    ///
    /// This is the one place a dotted string is turned back into the
    /// structure — the schema-transport JSON channel, distinct from the
    /// WIT boundary which already carries the structured record. The
    /// classification rule mirrors the parser's `Ref`-vs-`SchemaType`
    /// split: a name with a provider segment and a PascalCase tail
    /// (`aws.iam.Role.Arn`, `aws.VpcId`) parses to a provider-scoped
    /// identity; a bare PascalCase name (`VpcId`, `Ipv4Cidr`) — what a
    /// not-yet-migrated provider still emits — parses to a bare,
    /// provider-agnostic identity.
    pub fn from_dotted(name: &str) -> Self {
        match name.split_once('.') {
            // `provider.<rest...>.Kind` — at least one provider segment
            // plus a kind.
            Some((provider, rest)) => {
                let mut parts: Vec<&str> = rest.split('.').collect();
                let kind = parts.pop().unwrap_or(rest);
                Self {
                    provider: Some(provider.to_string()),
                    segments: parts.into_iter().map(String::from).collect(),
                    kind: kind.to_string(),
                }
            }
            // Bare name — no provider axis.
            None => Self::bare(name),
        }
    }

    /// Build an identity from a `TypeExpr::SchemaType`'s three parts.
    ///
    /// `provider` is the provider segment, `path` the dotted
    /// service/resource path (`"ec2"` or `"iam.Role"`, possibly empty),
    /// and `kind` the PascalCase type name. Used to project a parsed
    /// dotted type annotation onto the structured identity it denotes.
    pub fn from_schema_type(provider: &str, path: &str, kind: &str) -> Self {
        let segments = if path.is_empty() {
            Vec::new()
        } else {
            path.split('.').map(String::from).collect()
        };
        Self {
            provider: Some(provider.to_string()),
            segments,
            kind: kind.to_string(),
        }
    }

    /// Whether two identities denote the **same type**.
    ///
    /// Equality is per-axis over *populated* axes only:
    ///
    /// - `kind` must always match (the `kind` axis is never empty).
    /// - `provider` must match when **both** sides populate it. If
    ///   either side has `provider: None`, the provider axis is not
    ///   distinguished — `None` is the wider type.
    /// - `segments` must match when **both** sides are non-empty. An
    ///   empty `segments` on either side is the wider type.
    ///
    /// This yields the required distinctions: `aws.iam.Role.Arn` and
    /// `aws.acm.Certificate.Arn` differ (segments differ), `aws.Region`
    /// and `gcp.Region` differ (provider differs), while the generic
    /// `aws.Arn` stays assignable against a more specific `aws.*.Arn`.
    pub fn same_type(&self, other: &TypeIdentity) -> bool {
        if self.kind != other.kind {
            return false;
        }
        if let (Some(a), Some(b)) = (&self.provider, &other.provider)
            && a != b
        {
            return false;
        }
        if !self.segments.is_empty()
            && !other.segments.is_empty()
            && self.segments != other.segments
        {
            return false;
        }
        true
    }

    /// Whether a value of `self`'s type can be assigned into a sink of
    /// `other`'s type — **directional** per-axis subsumption.
    ///
    /// Unlike [`same_type`](Self::same_type), which is symmetric and
    /// answers "do these denote the same type?", this method asks
    /// "does the source carry enough provenance to satisfy the sink?".
    /// The two rules are deliberately separate (see carina#3218): the
    /// symmetric equivalence is correct for display and schema-registry
    /// lookup, but the assignment check inside
    /// [`AttributeType::is_assignable_to`] needs the directional shape.
    ///
    /// Rules, per-axis:
    ///
    /// - `kind` must match (the `kind` axis is never empty).
    /// - `provider`: if the sink populates it (`Some(_)`), the source
    ///   must populate it with the same value. A sink `None` (the
    ///   provider-agnostic kind) accepts any provider on the source —
    ///   widening is OK, narrowing is not.
    /// - `segments`: if the sink populates `segments`, the source must
    ///   carry **the same** segments. An empty source `segments` on a
    ///   populated sink is rejected — the source carries no segment
    ///   evidence that it satisfies the sink's resource scope (mirror
    ///   of the existing "anonymous source → semantic sink" rule on
    ///   `Custom.identity` itself). An empty sink `segments` accepts
    ///   any source (the sink does not distinguish at this axis).
    ///
    /// This yields:
    /// - `aws.iam.Role.Arn` (source) → `aws.Arn` (sink): **accepted**
    ///   (narrower → wider on the segments axis).
    /// - `aws.Arn` (source) → `aws.iam.Role.Arn` (sink): **rejected**
    ///   (the empty `segments` source carries no evidence of being an
    ///   IAM Role ARN).
    /// - `aws.iam.Role.Arn` ↔ `aws.acm.Certificate.Arn`: rejected both
    ///   ways (populated segments differ).
    /// - `aws.Region` ↔ `gcp.Region`: rejected both ways (populated
    ///   providers differ).
    pub fn assignable_to(&self, sink: &TypeIdentity) -> bool {
        if self.kind != sink.kind {
            return false;
        }
        if let Some(sink_provider) = &sink.provider
            && self.provider.as_deref() != Some(sink_provider.as_str())
        {
            return false;
        }
        if !sink.segments.is_empty() && self.segments != sink.segments {
            return false;
        }
        true
    }
}

impl fmt::Display for TypeIdentity {
    /// Dotted form: `provider.seg.seg.kind`, omitting empty axes.
    /// `aws.iam.Role.Arn`, `aws.Arn`, or a bare `Ipv4Cidr`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(provider) = &self.provider {
            write!(f, "{}.", provider)?;
        }
        for seg in &self.segments {
            write!(f, "{}.", seg)?;
        }
        write!(f, "{}", self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_identity_has_no_provider_or_segments() {
        let id = TypeIdentity::bare("Ipv4Cidr");
        assert_eq!(id.provider, None);
        assert!(id.segments.is_empty());
        assert_eq!(id.kind, "Ipv4Cidr");
    }

    #[test]
    fn display_renders_dotted_form() {
        let role_arn = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        assert_eq!(role_arn.to_string(), "aws.iam.Role.Arn");

        let generic_arn = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        assert_eq!(generic_arn.to_string(), "aws.Arn");

        let bare = TypeIdentity::bare("Ipv4Cidr");
        assert_eq!(bare.to_string(), "Ipv4Cidr");
    }

    #[test]
    fn same_provider_same_kind_different_segments_are_distinct() {
        // The headline disambiguation: aws.iam.Role.Arn != aws.acm.Certificate.Arn.
        let role_arn = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let cert_arn = TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Arn");
        assert!(!role_arn.same_type(&cert_arn));
        assert!(!cert_arn.same_type(&role_arn));
    }

    #[test]
    fn same_kind_different_provider_are_distinct() {
        // The motivating collision: aws.Region != gcp.Region.
        let aws_region = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region");
        let gcp_region = TypeIdentity::new(Some("gcp"), Vec::<String>::new(), "Region");
        assert!(!aws_region.same_type(&gcp_region));
        assert!(!gcp_region.same_type(&aws_region));
    }

    #[test]
    fn empty_provider_axis_is_not_distinguished() {
        // A None provider is the wider type — it does not distinguish
        // on the provider axis, so it matches either side.
        let bare_arn = TypeIdentity::bare("Arn");
        let aws_arn = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        assert!(bare_arn.same_type(&aws_arn));
        assert!(aws_arn.same_type(&bare_arn));
    }

    #[test]
    fn empty_segments_axis_is_not_distinguished() {
        // Generic aws.Arn (no segments) is wider than aws.iam.Role.Arn.
        let generic_arn = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        let role_arn = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        assert!(generic_arn.same_type(&role_arn));
        assert!(role_arn.same_type(&generic_arn));
    }

    #[test]
    fn different_kind_is_always_distinct() {
        let arn = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        let vpc_id = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "VpcId");
        assert!(!arn.same_type(&vpc_id));
    }

    #[test]
    fn identical_identities_are_same_type() {
        let a = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let b = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        assert!(a.same_type(&b));
    }

    #[test]
    fn from_dotted_parses_provider_scoped_and_bare_names() {
        // Fully specified: provider + service/resource + kind.
        let role_arn = TypeIdentity::from_dotted("aws.iam.Role.Arn");
        assert_eq!(role_arn.provider.as_deref(), Some("aws"));
        assert_eq!(role_arn.segments, vec!["iam", "Role"]);
        assert_eq!(role_arn.kind, "Arn");

        // Provider + kind, no service/resource segments.
        let aws_vpc_id = TypeIdentity::from_dotted("aws.VpcId");
        assert_eq!(aws_vpc_id.provider.as_deref(), Some("aws"));
        assert!(aws_vpc_id.segments.is_empty());
        assert_eq!(aws_vpc_id.kind, "VpcId");

        // Bare name — a not-yet-migrated provider's flat spelling.
        let bare = TypeIdentity::from_dotted("VpcId");
        assert_eq!(bare.provider, None);
        assert!(bare.segments.is_empty());
        assert_eq!(bare.kind, "VpcId");
    }

    #[test]
    fn from_dotted_round_trips_through_display() {
        for s in ["aws.iam.Role.Arn", "aws.VpcId", "Ipv4Cidr"] {
            assert_eq!(TypeIdentity::from_dotted(s).to_string(), s);
        }
    }

    #[test]
    fn assignable_to_is_directional_on_segments() {
        let generic = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        let role = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        // narrower (populated segments) → wider (empty segments): OK
        assert!(role.assignable_to(&generic));
        // wider (empty segments) → narrower (populated segments): NG
        assert!(!generic.assignable_to(&role));
    }

    #[test]
    fn assignable_to_is_directional_on_provider() {
        let bare = TypeIdentity::bare("Arn");
        let scoped = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        // narrower (Some provider) → wider (None provider): OK
        assert!(scoped.assignable_to(&bare));
        // wider (None provider) → narrower (Some provider): NG
        assert!(!bare.assignable_to(&scoped));
    }

    #[test]
    fn assignable_to_rejects_distinct_populated_axes() {
        // aws.Region vs gcp.Region: providers differ → both directions NG.
        let aws_region = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region");
        let gcp_region = TypeIdentity::new(Some("gcp"), Vec::<String>::new(), "Region");
        assert!(!aws_region.assignable_to(&gcp_region));
        assert!(!gcp_region.assignable_to(&aws_region));

        // aws.iam.Role.Arn vs aws.acm.Certificate.Arn: segments differ.
        let role = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let cert = TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Arn");
        assert!(!role.assignable_to(&cert));
        assert!(!cert.assignable_to(&role));
    }

    #[test]
    fn assignable_to_kind_must_match() {
        let arn = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn");
        let vpc_id = TypeIdentity::new(Some("aws"), Vec::<String>::new(), "VpcId");
        assert!(!arn.assignable_to(&vpc_id));
        assert!(!vpc_id.assignable_to(&arn));
    }

    #[test]
    fn from_schema_type_builds_provider_scoped_identity() {
        let id = TypeIdentity::from_schema_type("awscc", "ec2", "VpcId");
        assert_eq!(id.provider.as_deref(), Some("awscc"));
        assert_eq!(id.segments, vec!["ec2"]);
        assert_eq!(id.kind, "VpcId");

        // Empty path → no service/resource segments.
        let no_path = TypeIdentity::from_schema_type("aws", "", "Arn");
        assert!(no_path.segments.is_empty());
        assert_eq!(no_path.to_string(), "aws.Arn");
    }
}
