use sha2::Sha256;
use sigstore::bundle::Bundle;
use sigstore::bundle::verify::blocking::Verifier;
use sigstore::bundle::verify::policy::Identity;
use sigstore_protobuf_specs::dev::sigstore::bundle::v1::bundle::Content;

use super::trust_root::embedded_document;
use super::{ExpectedIdentity, TRUST_ROOT_STALENESS_HINT, error_chain, tlog, verification_failure};

const BUNDLE_V03_COMPAT_MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle+json;version=0.3";
const BUNDLE_V03_MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";

pub(super) fn verify(
    artifact_digest: Sha256,
    bundle_json: &[u8],
    expected: &ExpectedIdentity,
) -> Result<(), String> {
    verify_inner(artifact_digest, bundle_json, expected).map_err(verification_failure)
}

fn verify_inner(
    artifact_digest: Sha256,
    bundle_json: &[u8],
    expected: &ExpectedIdentity,
) -> Result<(), String> {
    let mut bundle: Bundle = serde_json::from_slice(bundle_json)
        .map_err(|error| format!("cannot parse Sigstore bundle JSON: {error}"))?;
    ensure_message_signature_bundle(&bundle)?;
    match bundle.media_type.as_str() {
        BUNDLE_V03_MEDIA_TYPE => {}
        BUNDLE_V03_COMPAT_MEDIA_TYPE => {
            bundle.media_type = BUNDLE_V03_MEDIA_TYPE.to_string();
        }
        media_type => {
            return Err(format!(
                "unsupported Sigstore bundle mediaType {media_type:?}; bundle v0.3 is required"
            ));
        }
    }

    let trust_root = embedded_document()?;
    trust_root.warn_if_rekor_keys_near_expiry();
    let integrated_time = tlog::verify_all(&bundle, &trust_root)
        .map_err(|error| format!("transparency-log verification failed: {error}"))?;
    let verifier_root = trust_root.verifier_root(integrated_time)?;
    let verifier = Verifier::new(Default::default(), verifier_root).map_err(|error| {
        format!(
            "cannot initialize the offline Sigstore verifier: {}",
            error_chain(&error)
        )
    })?;
    let (identity, issuer) = expected.values();
    let policy = Identity::new(identity, issuer);
    verifier
        .verify_digest(artifact_digest, bundle, &policy, true)
        .map_err(|error| {
            format!(
                "certificate, identity, artifact signature, or hashedrekord verification failed: {}; {TRUST_ROOT_STALENESS_HINT}",
                error_chain(&error),
            )
        })
}

fn ensure_message_signature_bundle(bundle: &Bundle) -> Result<(), String> {
    match bundle.content.as_ref() {
        Some(Content::MessageSignature(_)) => Ok(()),
        Some(Content::DsseEnvelope(_)) => {
            Err("DSSE bundles are unsupported; a messageSignature bundle is required".to_string())
        }
        None => Err("bundle does not contain messageSignature".to_string()),
    }
}
