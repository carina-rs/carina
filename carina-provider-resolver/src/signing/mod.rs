//! Offline verification for Sigstore bundles returned by a provider registry.

mod trust_root;
mod verifier;

use sha2::Sha256;

pub(super) const TRUST_ROOT_STALENESS_HINT: &str =
    "carina's embedded Sigstore trust root may be outdated — upgrading carina may resolve this";

/// Where the expected signing identity came from. Resolver code calls the
/// pinned constructor only with lock data. When a pin exists, the resolver
/// rejects a response-declared identity or issuer that disagrees before
/// verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedIdentity {
    kind: ExpectedIdentityKind,
    certificate_identity: String,
    certificate_oidc_issuer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedIdentityKind {
    Pinned,
    FirstUse,
}

impl ExpectedIdentity {
    pub(crate) fn pinned(certificate_identity: String, certificate_oidc_issuer: String) -> Self {
        Self {
            kind: ExpectedIdentityKind::Pinned,
            certificate_identity,
            certificate_oidc_issuer,
        }
    }

    pub(crate) fn first_use(declared_identity: String, declared_issuer: String) -> Self {
        Self {
            kind: ExpectedIdentityKind::FirstUse,
            certificate_identity: declared_identity,
            certificate_oidc_issuer: declared_issuer,
        }
    }

    pub(crate) fn values(&self) -> (&str, &str) {
        (&self.certificate_identity, &self.certificate_oidc_issuer)
    }

    pub(crate) fn is_first_use(&self) -> bool {
        matches!(self.kind, ExpectedIdentityKind::FirstUse)
    }
}

/// A signing identity that can only be produced by completed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    certificate_identity: String,
    certificate_oidc_issuer: String,
}

impl VerifiedIdentity {
    fn from_expected(expected: &ExpectedIdentity) -> Self {
        let (certificate_identity, certificate_oidc_issuer) = expected.values();
        Self {
            certificate_identity: certificate_identity.to_string(),
            certificate_oidc_issuer: certificate_oidc_issuer.to_string(),
        }
    }

    pub(crate) fn into_parts(self) -> (String, String) {
        (self.certificate_identity, self.certificate_oidc_issuer)
    }
}

/// Reject signature schemes that this client cannot verify.
pub(crate) fn ensure_supported_signature_type(signature_type: &str) -> Result<(), String> {
    if signature_type == "sigstore-bundle" {
        Ok(())
    } else {
        Err(verification_failure(format!(
            "unsupported registry signature type {signature_type:?}"
        )))
    }
}

/// Verify a message-signature bundle completely offline.
pub(crate) fn verify(
    artifact_digest: Sha256,
    bundle_json: &[u8],
    expected: &ExpectedIdentity,
) -> Result<VerifiedIdentity, String> {
    verifier::verify(artifact_digest, bundle_json, expected)?;
    Ok(VerifiedIdentity::from_expected(expected))
}

pub(crate) fn verification_failure(detail: impl std::fmt::Display) -> String {
    format!("Sigstore signature verification failed; there is no override: {detail}")
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut details = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        details.push_str(": ");
        details.push_str(&error.to_string());
        source = error.source();
    }
    details
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use serde_json::{Value, json};
    use sha2::Digest as _;

    use super::*;

    const FIXTURE_ARTIFACT: &[u8] = include_bytes!("testdata/a.txt");
    const FIXTURE_BUNDLE: &[u8] = include_bytes!("testdata/bundle.sigstore.json");
    const FIXTURE_IDENTITY: &str = "https://github.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.github/workflows/extremely-dangerous-oidc-beacon.yml@refs/heads/main";
    const FIXTURE_ISSUER: &str = "https://token.actions.githubusercontent.com";

    #[test]
    fn verifies_conformance_fixture_end_to_end() {
        let verified = verify(
            digest_for(FIXTURE_ARTIFACT),
            FIXTURE_BUNDLE,
            &fixture_first_use_identity(),
        )
        .expect("the conformance bundle should verify offline");

        assert_eq!(
            verified.into_parts(),
            (FIXTURE_IDENTITY.to_string(), FIXTURE_ISSUER.to_string())
        );
    }

    #[test]
    fn expected_identity_records_first_use_origin() {
        let first_use = fixture_first_use_identity();
        let pinned =
            ExpectedIdentity::pinned(FIXTURE_IDENTITY.to_string(), FIXTURE_ISSUER.to_string());

        assert!(first_use.is_first_use());
        assert!(!pinned.is_first_use());
    }

    #[test]
    fn rejects_dsse_bundle() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            let message_signature = bundle.as_object_mut().unwrap().remove("messageSignature");
            assert!(message_signature.is_some());
            bundle["dsseEnvelope"] = json!({});
        });

        let error = verify(
            digest_for(FIXTURE_ARTIFACT),
            &bundle,
            &fixture_first_use_identity(),
        )
        .unwrap_err();
        assert!(error.contains("DSSE"), "{error}");
    }

    #[test]
    fn rejects_bundle_without_signature_content() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            let message_signature = bundle.as_object_mut().unwrap().remove("messageSignature");
            assert!(message_signature.is_some());
        });

        let error = verify(
            digest_for(FIXTURE_ARTIFACT),
            &bundle,
            &fixture_first_use_identity(),
        )
        .unwrap_err();
        assert!(error.contains("messageSignature"), "{error}");
        assert!(error.contains("no override"), "{error}");
    }

    #[test]
    fn rejects_wrong_bundle_media_type() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["mediaType"] = json!("application/vnd.dev.sigstore.bundle.v0.2+json");
        });

        assert_verification_fails(&bundle, fixture_first_use_identity());
    }

    #[test]
    fn rejects_pinned_identity_mismatch() {
        assert_verification_fails(
            FIXTURE_BUNDLE,
            ExpectedIdentity::pinned(
                "https://github.com/example/other/.github/workflows/release.yml@refs/heads/main"
                    .to_string(),
                FIXTURE_ISSUER.to_string(),
            ),
        );
    }

    #[test]
    fn rejects_issuer_mismatch() {
        assert_verification_fails(
            FIXTURE_BUNDLE,
            ExpectedIdentity::first_use(
                FIXTURE_IDENTITY.to_string(),
                "https://issuer.example".to_string(),
            ),
        );
    }

    #[test]
    fn rejects_artifact_digest_mismatch() {
        let error = verify(
            digest_for(b"different artifact"),
            FIXTURE_BUNDLE,
            &fixture_first_use_identity(),
        )
        .unwrap_err();
        assert!(error.contains("no override"), "{error}");
        assert!(error.contains("trust root may be outdated"), "{error}");
        assert!(
            error.contains("upgrading carina may resolve this"),
            "{error}"
        );
    }

    #[test]
    fn rejects_unknown_signature_type() {
        let error = ensure_supported_signature_type("something-else").unwrap_err();
        assert!(error.contains("something-else"), "{error}");
        assert!(error.contains("no override"), "{error}");
    }

    #[test]
    fn rejects_unknown_log_id() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["logId"]["keyId"] =
                json!(BASE64_STANDARD.encode([0u8; 32]));
        });

        assert_verification_fails(&bundle, fixture_first_use_identity());
    }

    fn fixture_first_use_identity() -> ExpectedIdentity {
        ExpectedIdentity::first_use(FIXTURE_IDENTITY.to_string(), FIXTURE_ISSUER.to_string())
    }

    fn digest_for(bytes: &[u8]) -> Sha256 {
        let mut digest = Sha256::new();
        digest.update(bytes);
        digest
    }

    fn mutate_bundle(bundle: &[u8], mutate: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut bundle: Value = serde_json::from_slice(bundle).unwrap();
        mutate(&mut bundle);
        serde_json::to_vec(&bundle).unwrap()
    }

    fn assert_verification_fails(bundle: &[u8], expected: ExpectedIdentity) {
        let error = verify(digest_for(FIXTURE_ARTIFACT), bundle, &expected).unwrap_err();
        assert!(error.contains("no override"), "{error}");
    }
}
