//! Offline verification for Sigstore bundles returned by a provider registry.

mod trust_root;
mod verifier;

use std::cell::RefCell;

use sha2::Sha256;
use sigstore::bundle::verify::policy::{PolicyError, PolicyResult, VerificationPolicy};
use x509_cert::Certificate;
use x509_cert::der::{Decode as _, asn1::Utf8StringRef};
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};
use x509_cert::spki::ObjectIdentifier;

pub(super) const TRUST_ROOT_STALENESS_HINT: &str =
    "carina's embedded Sigstore trust root may be outdated — upgrading carina may resolve this";

const SOURCE_OIDC_ISSUER_OID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.8");
const SOURCE_REPOSITORY_DIGEST_OID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.13");
const SOURCE_REF_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.14");
const SOURCE_REPOSITORY_ID_OID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.15");
const SOURCE_REPOSITORY_OWNER_ID_OID: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.17");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) oidc_issuer: String,
    pub(crate) repository_owner_id: String,
    pub(crate) repository_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractedSignerIdentity {
    certificate_san: String,
    source: SourceIdentity,
    source_ref: String,
    workflow: String,
    repository_digest: Option<String>,
}

/// Signer identity extracted from the exact certificate accepted by Sigstore.
/// Its private constructor prevents resolver code from fabricating verified
/// certificate material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedSignerIdentity(ExtractedSignerIdentity);

impl VerifiedSignerIdentity {
    pub(crate) fn certificate_san(&self) -> &str {
        &self.0.certificate_san
    }

    pub(crate) fn source(&self) -> &SourceIdentity {
        &self.0.source
    }

    pub(crate) fn source_ref(&self) -> &str {
        &self.0.source_ref
    }

    pub(crate) fn workflow(&self) -> &str {
        &self.0.workflow
    }

    pub(crate) fn repository_digest(&self) -> Option<&str> {
        self.0.repository_digest.as_deref()
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn for_test(
        certificate_san: &str,
        oidc_issuer: &str,
        repository_owner_id: &str,
        repository_id: &str,
        source_ref: &str,
        workflow: &str,
        repository_digest: Option<&str>,
    ) -> Self {
        Self(ExtractedSignerIdentity {
            certificate_san: certificate_san.to_string(),
            source: SourceIdentity {
                oidc_issuer: oidc_issuer.to_string(),
                repository_owner_id: repository_owner_id.to_string(),
                repository_id: repository_id.to_string(),
            },
            source_ref: source_ref.to_string(),
            workflow: workflow.to_string(),
            repository_digest: repository_digest.map(str::to_string),
        })
    }
}

struct SignerIdentityPolicy {
    extracted: RefCell<Option<ExtractedSignerIdentity>>,
}

impl SignerIdentityPolicy {
    fn new() -> Self {
        Self {
            extracted: RefCell::new(None),
        }
    }

    fn into_verified(self) -> Result<VerifiedSignerIdentity, String> {
        self.extracted
            .into_inner()
            .map(VerifiedSignerIdentity)
            .ok_or_else(|| "verified certificate identity was not captured".to_string())
    }
}

impl VerificationPolicy for SignerIdentityPolicy {
    fn verify(&self, certificate: &Certificate) -> PolicyResult {
        let identity = extract_signer_identity(certificate).map_err(|error| {
            PolicyError::ExtensionCheckFailed {
                extension: "Fulcio signer identity".to_string(),
                expected: "one URI SAN and every required source extension".to_string(),
                actual: error,
            }
        })?;
        self.extracted.replace(Some(identity));
        Ok(())
    }
}

fn extract_signer_identity(certificate: &Certificate) -> Result<ExtractedSignerIdentity, String> {
    let (_, subject_alt_name) = certificate
        .tbs_certificate
        .get::<SubjectAltName>()
        .map_err(|error| format!("cannot decode subjectAltName: {error}"))?
        .ok_or_else(|| "certificate must contain exactly one URI subjectAltName".to_string())?;
    let mut uri_names = subject_alt_name.0.iter().filter_map(|name| match name {
        GeneralName::UniformResourceIdentifier(uri) => Some(uri.as_str()),
        _ => None,
    });
    let (Some(certificate_san), None) = (uri_names.next(), uri_names.next()) else {
        return Err("certificate must contain exactly one URI subjectAltName".to_string());
    };

    let oidc_issuer =
        required_utf8_extension(certificate, SOURCE_OIDC_ISSUER_OID, "source OIDC issuer")?;
    let repository_owner_id = required_utf8_extension(
        certificate,
        SOURCE_REPOSITORY_OWNER_ID_OID,
        "source repository owner ID",
    )?;
    let repository_id = required_utf8_extension(
        certificate,
        SOURCE_REPOSITORY_ID_OID,
        "source repository ID",
    )?;
    let source_ref = required_utf8_extension(certificate, SOURCE_REF_OID, "source repository ref")?;
    let repository_digest = optional_utf8_extension(
        certificate,
        SOURCE_REPOSITORY_DIGEST_OID,
        "source repository digest",
    )?;
    let suffix = format!("@{source_ref}");
    let workflow = certificate_san
        .strip_suffix(&suffix)
        .unwrap_or(certificate_san)
        .to_string();

    Ok(ExtractedSignerIdentity {
        certificate_san: certificate_san.to_string(),
        source: SourceIdentity {
            oidc_issuer,
            repository_owner_id,
            repository_id,
        },
        source_ref,
        workflow,
        repository_digest,
    })
}

/// Parse only the leaf certificate from a captured bundle so decision tests
/// exercise real Fulcio certificate shapes without pretending to verify the
/// artifact signature. Production identities remain constructible only by
/// [`verify_artifact`].
#[cfg(test)]
pub(crate) fn extract_signer_identity_from_bundle_for_test(
    bundle_json: &[u8],
) -> Result<VerifiedSignerIdentity, String> {
    use sigstore::bundle::Bundle;
    use sigstore_protobuf_specs::dev::sigstore::bundle::v1::verification_material;

    let bundle: Bundle = serde_json::from_slice(bundle_json)
        .map_err(|error| format!("cannot parse Sigstore bundle JSON: {error}"))?;
    let material = bundle
        .verification_material
        .as_ref()
        .ok_or_else(|| "bundle is missing verificationMaterial".to_string())?;
    let raw = match material.content.as_ref() {
        Some(verification_material::Content::Certificate(certificate)) => {
            certificate.raw_bytes.as_slice()
        }
        Some(verification_material::Content::X509CertificateChain(chain)) => chain
            .certificates
            .first()
            .ok_or_else(|| "bundle certificate chain is empty".to_string())?
            .raw_bytes
            .as_slice(),
        Some(verification_material::Content::PublicKey(_)) | None => {
            return Err("bundle does not contain a signing certificate".to_string());
        }
    };
    let certificate = Certificate::from_der(raw)
        .map_err(|error| format!("cannot parse signing certificate: {error}"))?;
    extract_signer_identity(&certificate).map(VerifiedSignerIdentity)
}

fn required_utf8_extension(
    certificate: &Certificate,
    oid: ObjectIdentifier,
    name: &str,
) -> Result<String, String> {
    optional_utf8_extension(certificate, oid, name)?
        .ok_or_else(|| format!("certificate must contain exactly one {name} extension ({oid})"))
}

fn optional_utf8_extension(
    certificate: &Certificate,
    oid: ObjectIdentifier,
    name: &str,
) -> Result<Option<String>, String> {
    let mut matches = certificate
        .tbs_certificate
        .extensions
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|extension| extension.extn_id == oid);
    let first = matches.next();
    if matches.next().is_some() {
        return Err(format!(
            "certificate must contain at most one {name} extension ({oid})"
        ));
    }
    let Some(extension) = first else {
        return Ok(None);
    };
    let value = Utf8StringRef::from_der(extension.extn_value.as_bytes()).map_err(|error| {
        format!("certificate {name} extension ({oid}) is not DER UTF8String: {error}")
    })?;
    Ok(Some(value.as_str().to_string()))
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

pub(crate) fn verify_artifact(
    artifact_digest: Sha256,
    bundle_json: &[u8],
) -> Result<VerifiedSignerIdentity, String> {
    verifier::verify_artifact(artifact_digest, bundle_json)
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
    use x509_cert::Certificate;
    use x509_cert::der::{Encode as _, asn1::Utf8StringRef};

    use rcgen::{CertificateParams, CustomExtension, KeyPair, SanType};

    use super::*;

    const FIXTURE_ARTIFACT: &[u8] = include_bytes!("testdata/a.txt");
    const FIXTURE_BUNDLE: &[u8] = include_bytes!("testdata/bundle.sigstore.json");
    const AWS_STABLE_BUNDLE: &[u8] =
        include_bytes!("testdata/issue-3928/aws-0.7.0/bundle.sigstore.json");
    const FIXTURE_IDENTITY: &str = "https://github.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.github/workflows/extremely-dangerous-oidc-beacon.yml@refs/heads/main";

    const SOURCE_ISSUER_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 8];
    const SOURCE_REPOSITORY_DIGEST_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 13];
    const SOURCE_REF_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 14];
    const SOURCE_REPOSITORY_ID_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 15];
    const SOURCE_REPOSITORY_OWNER_ID_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 57264, 1, 17];

    #[test]
    fn extracts_signer_identity_from_fulcio_extensions() {
        let source_ref = "refs/heads/main";
        let san = format!(
            "https://github.com/carina-rs/carina-provider-aws/.github/workflows/ci@team.yml@{source_ref}"
        );
        let certificate = signer_certificate(&[&san], &[]);

        let identity = extract_signer_identity(&certificate).unwrap();

        assert_eq!(identity.certificate_san, san);
        assert_eq!(
            identity.source.oidc_issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert_eq!(identity.source.repository_owner_id, "260157389");
        assert_eq!(identity.source.repository_id, "1199024834");
        assert_eq!(identity.source_ref, source_ref);
        assert_eq!(
            identity.workflow,
            "https://github.com/carina-rs/carina-provider-aws/.github/workflows/ci@team.yml"
        );
        assert_eq!(identity.repository_digest.as_deref(), Some("12ec945"));
    }

    #[test]
    fn keeps_the_whole_san_when_the_exact_source_ref_suffix_is_absent() {
        let san = "https://example.test/workflow@team";
        let certificate = signer_certificate(&[san], &[]);

        let identity = extract_signer_identity(&certificate).unwrap();

        assert_eq!(identity.workflow, san);
    }

    #[test]
    fn rejects_each_missing_required_signer_component() {
        for omitted_oid in [
            SOURCE_ISSUER_OID,
            SOURCE_REF_OID,
            SOURCE_REPOSITORY_ID_OID,
            SOURCE_REPOSITORY_OWNER_ID_OID,
        ] {
            let certificate = signer_certificate(
                &["https://example.test/workflow@refs/heads/main"],
                &[omitted_oid],
            );

            let error = extract_signer_identity(&certificate).unwrap_err();
            assert!(error.contains("exactly one"), "{error}");
        }
    }

    #[test]
    fn rejects_zero_or_multiple_uri_subject_alternative_names() {
        for uri_sans in [
            Vec::new(),
            vec![
                "https://example.test/one@refs/heads/main",
                "https://example.test/two@refs/heads/main",
            ],
        ] {
            let certificate = signer_certificate(&uri_sans, &[]);

            let error = extract_signer_identity(&certificate).unwrap_err();
            assert!(error.contains("exactly one URI"), "{error}");
        }
    }

    fn signer_certificate(uri_sans: &[&str], omitted_oids: &[&[u64]]) -> Certificate {
        let mut params = CertificateParams::default();
        params.subject_alt_names = uri_sans
            .iter()
            .map(|san| SanType::URI((*san).try_into().unwrap()))
            .collect();
        for (oid, value) in [
            (
                SOURCE_ISSUER_OID,
                "https://token.actions.githubusercontent.com",
            ),
            (SOURCE_REPOSITORY_DIGEST_OID, "12ec945"),
            (SOURCE_REF_OID, "refs/heads/main"),
            (SOURCE_REPOSITORY_ID_OID, "1199024834"),
            (SOURCE_REPOSITORY_OWNER_ID_OID, "260157389"),
        ] {
            if !omitted_oids.contains(&oid) {
                let value = Utf8StringRef::new(value).unwrap().to_der().unwrap();
                params
                    .custom_extensions
                    .push(CustomExtension::from_oid_content(oid, value));
            }
        }
        let key_pair = KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key_pair).unwrap();
        Certificate::from_der(certificate.der()).unwrap()
    }

    #[test]
    fn verifies_conformance_fixture_end_to_end() {
        assert_conformance_fixture_verifies();
    }

    #[test]
    fn extracts_identity_from_captured_registry_leaf_certificate() {
        let identity = extract_signer_identity_from_bundle_for_test(AWS_STABLE_BUNDLE)
            .expect("the captured production bundle should contain a Fulcio leaf certificate");

        assert_eq!(
            identity.workflow(),
            "https://github.com/carina-rs/carina-provider-aws/.github/workflows/release.yml"
        );
        assert_eq!(identity.source_ref(), "refs/tags/v0.7.0");
        assert_eq!(identity.source().repository_owner_id, "260157389");
        assert_eq!(identity.source().repository_id, "1199024834");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verifies_conformance_fixture_inside_current_thread_runtime() {
        assert_conformance_fixture_verifies();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn verifies_conformance_fixture_inside_multi_thread_runtime() {
        assert_conformance_fixture_verifies();
    }

    fn assert_conformance_fixture_verifies() {
        let verified = verify_artifact(digest_for(FIXTURE_ARTIFACT), FIXTURE_BUNDLE)
            .expect("the conformance bundle should verify offline");

        assert_eq!(verified.certificate_san(), FIXTURE_IDENTITY);
        assert_eq!(verified.source_ref(), "refs/heads/main");
    }

    #[test]
    fn rejects_dsse_bundle() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            let message_signature = bundle.as_object_mut().unwrap().remove("messageSignature");
            assert!(message_signature.is_some());
            bundle["dsseEnvelope"] = json!({});
        });

        let error = verify_artifact(digest_for(FIXTURE_ARTIFACT), &bundle).unwrap_err();
        assert!(error.contains("DSSE"), "{error}");
    }

    #[test]
    fn rejects_bundle_without_signature_content() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            let message_signature = bundle.as_object_mut().unwrap().remove("messageSignature");
            assert!(message_signature.is_some());
        });

        let error = verify_artifact(digest_for(FIXTURE_ARTIFACT), &bundle).unwrap_err();
        assert!(error.contains("messageSignature"), "{error}");
        assert!(error.contains("no override"), "{error}");
    }

    #[test]
    fn rejects_wrong_bundle_media_type() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["mediaType"] = json!("application/vnd.dev.sigstore.bundle.v0.2+json");
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_artifact_digest_mismatch() {
        let error = verify_artifact(digest_for(b"different artifact"), FIXTURE_BUNDLE).unwrap_err();
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

        assert_verification_fails(&bundle);
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

    fn assert_verification_fails(bundle: &[u8]) {
        let error = verify_artifact(digest_for(FIXTURE_ARTIFACT), bundle).unwrap_err();
        assert!(error.contains("no override"), "{error}");
    }
}
