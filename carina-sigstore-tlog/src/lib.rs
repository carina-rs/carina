//! Rekor transparency-log verification for Sigstore bundles.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use p256::ecdsa::signature::hazmat::PrehashVerifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use sigstore::bundle::Bundle;
use sigstore_protobuf_specs::dev::sigstore::rekor::v1::TransparencyLogEntry;

/// A Rekor public key together with the identity bound into Rekor signatures.
///
/// Constructing a key validates that its key ID is the SHA-256 digest of its
/// DER-encoded SubjectPublicKeyInfo bytes.
pub struct RekorKey {
    der_spki: Vec<u8>,
    key_id: [u8; 32],
    origin_name: String,
}

impl RekorKey {
    /// Creates a Rekor key after checking its key ID against its DER SPKI.
    pub fn new(der_spki: Vec<u8>, key_id: [u8; 32], origin_name: String) -> Result<Self, String> {
        let computed_key_id: [u8; 32] = Sha256::digest(&der_spki).into();
        if key_id != computed_key_id {
            return Err(format!(
                "Rekor key ID {} does not match the SHA-256 of its DER public key ({})",
                lower_hex(&key_id),
                lower_hex(&computed_key_id)
            ));
        }

        Ok(Self {
            der_spki,
            key_id,
            origin_name,
        })
    }
}

/// The validated bundle fields a caller needs in order to choose a Rekor key.
pub struct TlogSelectionInputs<'bundle> {
    /// The key ID declared by the bundle's sole transparency-log entry.
    pub log_key_id: &'bundle [u8],
    /// The non-negative integrated time declared by that entry.
    pub integrated_time: u64,
}

struct ExtractedTlogEntry<'bundle> {
    entry: &'bundle TransparencyLogEntry,
    inputs: TlogSelectionInputs<'bundle>,
    log_index: u64,
}

struct PreparedTlog<'key> {
    canonicalized_body: Vec<u8>,
    signed_entry_timestamp: Vec<u8>,
    integrated_time: u64,
    log_index: u64,
    proof: PreparedProof,
    key: &'key RekorKey,
}

struct PreparedProof {
    checkpoint: String,
    hashes: Vec<[u8; 32]>,
    log_index: u64,
    root_hash: [u8; 32],
    tree_size: u64,
}

/// Verifies the SET, RFC 6962 inclusion proof, and checkpoint signature.
///
/// Returns the bundle's Rekor integrated time after all checks succeed.
pub fn verify_all(bundle: &Bundle, key: &RekorKey) -> Result<u64, String> {
    let material = prepare(bundle, key)?;
    verify_set(&material)?;
    verify_inclusion_proof(&material)?;
    verify_checkpoint(&material)?;
    Ok(material.integrated_time)
}

/// Extracts and validates the bundle fields needed to select a Rekor key.
pub fn selection_inputs(bundle: &Bundle) -> Result<TlogSelectionInputs<'_>, String> {
    Ok(extract_tlog_entry(bundle)?.inputs)
}

fn extract_tlog_entry(bundle: &Bundle) -> Result<ExtractedTlogEntry<'_>, String> {
    let entries = &bundle
        .verification_material
        .as_ref()
        .ok_or_else(|| "bundle is missing verificationMaterial".to_string())?
        .tlog_entries;
    if entries.len() != 1 {
        return Err(format!(
            "bundle must contain exactly one tlogEntry, got {}",
            entries.len()
        ));
    }
    let entry = &entries[0];
    let kind_version = entry
        .kind_version
        .as_ref()
        .ok_or_else(|| "tlogEntry is missing kindVersion".to_string())?;
    if kind_version.kind != "hashedrekord" || kind_version.version != "0.0.1" {
        return Err(format!(
            "unsupported tlogEntry kind/version {}/{}; hashedrekord/0.0.1 is required",
            kind_version.kind, kind_version.version
        ));
    }

    let integrated_time = non_negative_u64(entry.integrated_time, "tlogEntry.integratedTime")?;
    let log_index = non_negative_u64(entry.log_index, "tlogEntry.logIndex")?;
    let log_id = entry
        .log_id
        .as_ref()
        .ok_or_else(|| "tlogEntry is missing logId".to_string())?;
    if log_id.key_id.len() != 32 {
        return Err(format!(
            "tlogEntry.logId.keyId must decode to 32 bytes, got {}",
            log_id.key_id.len()
        ));
    }

    Ok(ExtractedTlogEntry {
        entry,
        inputs: TlogSelectionInputs {
            log_key_id: &log_id.key_id,
            integrated_time,
        },
        log_index,
    })
}

fn prepare<'key>(bundle: &Bundle, key: &'key RekorKey) -> Result<PreparedTlog<'key>, String> {
    let extracted = extract_tlog_entry(bundle)?;
    let entry = extracted.entry;
    if extracted.inputs.log_key_id != key.key_id {
        return Err(format!(
            "tlogEntry.logId.keyId {} does not match the supplied Rekor key ID {}",
            lower_hex(extracted.inputs.log_key_id),
            lower_hex(&key.key_id)
        ));
    }

    let canonicalized_body = entry.canonicalized_body.clone();
    let signed_entry_timestamp = entry
        .inclusion_promise
        .as_ref()
        .ok_or_else(|| "tlogEntry is missing inclusionPromise".to_string())?
        .signed_entry_timestamp
        .clone();
    let proof = entry
        .inclusion_proof
        .as_ref()
        .ok_or_else(|| "tlogEntry is missing inclusionProof".to_string())?;
    let checkpoint = proof
        .checkpoint
        .as_ref()
        .ok_or_else(|| "tlogEntry inclusionProof is missing checkpoint".to_string())?
        .envelope
        .clone();
    let hashes = proof
        .hashes
        .iter()
        .enumerate()
        .map(|(index, hash)| exact_hash_bytes(&format!("inclusionProof.hashes[{index}]"), hash))
        .collect::<Result<Vec<_>, _>>()?;
    let root_hash = exact_hash_bytes("inclusionProof.rootHash", &proof.root_hash)?;
    let proof_log_index = non_negative_u64(proof.log_index, "inclusionProof.logIndex")?;
    let tree_size = non_negative_u64(proof.tree_size, "inclusionProof.treeSize")?;

    Ok(PreparedTlog {
        canonicalized_body,
        signed_entry_timestamp,
        integrated_time: extracted.inputs.integrated_time,
        log_index: extracted.log_index,
        proof: PreparedProof {
            checkpoint,
            hashes,
            log_index: proof_log_index,
            root_hash,
            tree_size,
        },
        key,
    })
}

fn verify_set(material: &PreparedTlog) -> Result<(), String> {
    let payload = set_payload(material)?;
    verify_ecdsa_signature(
        &material.key.der_spki,
        payload.as_bytes(),
        &material.signed_entry_timestamp,
        "Rekor signed entry timestamp",
    )
}

fn set_payload(material: &PreparedTlog) -> Result<String, String> {
    let mut payload = BTreeMap::new();
    payload.insert(
        "body",
        Value::String(BASE64_STANDARD.encode(&material.canonicalized_body)),
    );
    payload.insert(
        "integratedTime",
        Value::Number(material.integrated_time.into()),
    );
    payload.insert("logID", Value::String(lower_hex(&material.key.key_id)));
    payload.insert("logIndex", Value::Number(material.log_index.into()));
    serde_json::to_string(&payload)
        .map_err(|error| format!("cannot serialize Rekor SET payload: {error}"))
}

fn verify_inclusion_proof(material: &PreparedTlog) -> Result<(), String> {
    let proof = &material.proof;
    verify_inclusion_proof_coordinates(
        &material.canonicalized_body,
        proof.log_index,
        proof.tree_size,
        &proof.hashes,
        &proof.root_hash,
    )
}

fn verify_inclusion_proof_coordinates(
    leaf_body: &[u8],
    log_index: u64,
    tree_size: u64,
    hashes: &[[u8; 32]],
    root_hash: &[u8; 32],
) -> Result<(), String> {
    if tree_size == 0 || log_index >= tree_size {
        return Err(format!(
            "invalid inclusion proof coordinates: index {}, tree size {}",
            log_index, tree_size
        ));
    }

    let leaf_hash = hash_leaf(leaf_body);
    let inner = (u64::BITS - (log_index ^ (tree_size - 1)).leading_zeros()) as usize;
    let border_index = log_index.checked_shr(inner as u32).unwrap_or(0);
    let border = border_index.count_ones() as usize;
    let expected_hash_count = inner + border;
    if hashes.len() != expected_hash_count {
        return Err(format!(
            "inclusion proof has wrong hash count: got {}, expected {expected_hash_count}",
            hashes.len()
        ));
    }

    let mut calculated = leaf_hash;
    for (level, proof_hash) in hashes[..inner].iter().enumerate() {
        calculated = if ((log_index >> level) & 1) == 0 {
            hash_children(&calculated, proof_hash)
        } else {
            hash_children(proof_hash, &calculated)
        };
    }
    for proof_hash in &hashes[inner..] {
        calculated = hash_children(proof_hash, &calculated);
    }

    if calculated != *root_hash {
        return Err(format!(
            "inclusion proof root mismatch: calculated {}, bundle has {}",
            lower_hex(&calculated),
            lower_hex(root_hash)
        ));
    }
    Ok(())
}

fn verify_checkpoint(material: &PreparedTlog) -> Result<(), String> {
    let envelope = &material.proof.checkpoint;
    let separator = envelope
        .find("\n\n")
        .ok_or_else(|| "checkpoint is not a signed-note envelope".to_string())?;
    let body_without_final_newline = &envelope[..separator];
    let body_lines = body_without_final_newline.split('\n').collect::<Vec<_>>();
    if body_lines.len() != 3 {
        return Err(format!(
            "checkpoint body must contain exactly three lines, got {}",
            body_lines.len()
        ));
    }
    let origin = body_lines[0];
    let expected_origin_prefix = format!("{} - ", material.key.origin_name);
    if !origin.starts_with(&expected_origin_prefix) || origin.len() == expected_origin_prefix.len()
    {
        return Err(format!(
            "checkpoint origin {origin:?} does not identify trusted log {}",
            material.key.origin_name
        ));
    }
    let tree_size = body_lines[1]
        .parse::<u64>()
        .map_err(|error| format!("checkpoint tree size is not an integer: {error}"))?;
    if tree_size != material.proof.tree_size {
        return Err(format!(
            "checkpoint tree size {tree_size} does not match inclusion proof tree size {}",
            material.proof.tree_size
        ));
    }
    let root_hash = decode_hash("checkpoint root hash", body_lines[2])?;
    if root_hash != material.proof.root_hash {
        return Err(format!(
            "checkpoint root hash {} does not match inclusion proof root hash {}",
            lower_hex(&root_hash),
            lower_hex(&material.proof.root_hash)
        ));
    }

    let signed_body = &envelope.as_bytes()[..=separator];
    let signature_lines = envelope[separator + 2..]
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if signature_lines.is_empty() {
        return Err("checkpoint contains no signed-note signatures".to_string());
    }

    let mut verification_errors = Vec::new();
    for line in signature_lines {
        let Some(signature) = line.strip_prefix("— ") else {
            verification_errors.push(format!("invalid signed-note signature line {line:?}"));
            continue;
        };
        let Some((name, encoded)) = signature.split_once(' ') else {
            verification_errors.push(format!("invalid signed-note signature line {line:?}"));
            continue;
        };
        if name != material.key.origin_name {
            continue;
        }
        let note_signature = match decode_base64("checkpoint signature", encoded) {
            Ok(signature) => signature,
            Err(error) => {
                verification_errors.push(error);
                continue;
            }
        };
        if note_signature.len() <= 4 {
            verification_errors.push("checkpoint signature has no DER signature bytes".to_string());
            continue;
        }
        if note_signature[..4] != material.key.key_id[..4] {
            continue;
        }
        match verify_ecdsa_signature(
            &material.key.der_spki,
            signed_body,
            &note_signature[4..],
            "checkpoint signature",
        ) {
            Ok(()) => return Ok(()),
            Err(error) => verification_errors.push(error),
        }
    }

    Err(format!(
        "checkpoint has no signature verifiable by the selected Rekor key{}",
        if verification_errors.is_empty() {
            String::new()
        } else {
            format!(": {}", verification_errors.join("; "))
        }
    ))
}

fn verify_ecdsa_signature(
    der_spki: &[u8],
    message: &[u8],
    der_signature: &[u8],
    kind: &str,
) -> Result<(), String> {
    let key = VerifyingKey::from_public_key_der(der_spki)
        .map_err(|error| format!("cannot parse {kind} P-256 public key: {error}"))?;
    let signature = Signature::from_der(der_signature)
        .map_err(|error| format!("cannot parse {kind} DER signature: {error}"))?;
    let digest = Sha256::digest(message);
    key.verify_prehash(&digest, &signature)
        .map_err(|error| format!("{kind} verification failed: {error}"))
}

fn hash_leaf(body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0]);
    hasher.update(body);
    hasher.finalize().into()
}

fn hash_children(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([1]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn non_negative_u64(value: i64, field: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{field} is not a non-negative integer"))
}

fn exact_hash_bytes(kind: &str, bytes: &[u8]) -> Result<[u8; 32], String> {
    bytes
        .try_into()
        .map_err(|_| format!("{kind} must decode to 32 bytes, got {}", bytes.len()))
}

fn decode_hash(kind: &str, encoded: &str) -> Result<[u8; 32], String> {
    let bytes = decode_base64(kind, encoded)?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("{kind} must decode to 32 bytes, got {}", bytes.len()))
}

fn decode_base64(kind: &str, encoded: &str) -> Result<Vec<u8>, String> {
    BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("{kind} is not valid base64: {error}"))
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde_json::{Value, json};

    use super::*;

    const FIXTURE_BUNDLE: &[u8] = include_bytes!("testdata/bundle.sigstore.json");
    const LIVE_BUNDLE: &[u8] = include_bytes!("testdata/live-bundle.sigstore.json");
    const REKOR_DER_SPKI_BASE64: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE2G2Y+2tabdTV5BcGiBIx0a9fAFwrkBbmLSGtks4L3qX6yYY0zufBnhC8Ur/iy55GhWP/9A/bY2LhC30M9+RYtw==";
    const REKOR_ORIGIN_NAME: &str = "rekor.sigstore.dev";

    #[test]
    fn rekor_key_rejects_key_id_that_does_not_match_der_spki() {
        let der_spki = fixture_der_spki();
        let mut key_id: [u8; 32] = Sha256::digest(&der_spki).into();
        key_id[0] ^= 1;

        let error = match RekorKey::new(der_spki, key_id, REKOR_ORIGIN_NAME.to_string()) {
            Ok(_) => panic!("a mismatched Rekor key ID must be rejected"),
            Err(error) => error,
        };

        assert!(error.contains("does not match the SHA-256"), "{error}");
    }

    #[test]
    fn verify_all_rejects_bundle_log_id_that_does_not_match_rekor_key() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["logId"]["keyId"] =
                json!(BASE64_STANDARD.encode([0u8; 32]));
        });
        let bundle = parse_bundle_for_test(&bundle);
        let key = fixture_rekor_key();

        let error = verify_all(&bundle, &key).unwrap_err();

        assert!(
            error.contains("does not match the supplied Rekor key ID"),
            "{error}"
        );
    }

    #[test]
    fn selection_inputs_and_verify_all_reject_unsupported_kind_version() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["kindVersion"]["kind"] =
                json!("other-kind");
        });
        let bundle = parse_bundle_for_test(&bundle);

        let selection_error = match selection_inputs(&bundle) {
            Ok(_) => panic!("selection inputs must reject an unsupported tlog entry kind"),
            Err(error) => error,
        };
        let key = fixture_rekor_key();
        let verification_error = verify_all(&bundle, &key).unwrap_err();

        assert_eq!(selection_error, verification_error);
        assert!(
            selection_error.contains(
                "unsupported tlogEntry kind/version other-kind/0.0.1; hashedrekord/0.0.1 is required"
            ),
            "{selection_error}"
        );
    }

    #[test]
    fn verifies_live_bundle_set() {
        let bundle = parse_bundle_for_test(LIVE_BUNDLE);
        let key = fixture_rekor_key();
        let material = prepare(&bundle, &key).unwrap();

        verify_set(&material).expect("the live bundle SET should verify");
    }

    #[test]
    fn verifies_live_bundle_inclusion_proof() {
        let bundle = parse_bundle_for_test(LIVE_BUNDLE);
        let key = fixture_rekor_key();
        let material = prepare(&bundle, &key).unwrap();

        verify_inclusion_proof(&material).expect("the live bundle inclusion proof should verify");
    }

    #[test]
    fn verifies_live_bundle_checkpoint_signature() {
        let bundle = parse_bundle_for_test(LIVE_BUNDLE);
        let key = fixture_rekor_key();
        let material = prepare(&bundle, &key).unwrap();

        verify_checkpoint(&material).expect("the live bundle checkpoint should verify");
    }

    #[test]
    fn set_payload_is_exact_compact_canonical_json() {
        let bundle = parse_bundle_for_test(FIXTURE_BUNDLE);
        let key = fixture_rekor_key();
        let payload = set_payload(&prepare(&bundle, &key).unwrap()).unwrap();
        let bundle: Value = serde_json::from_slice(FIXTURE_BUNDLE).unwrap();
        let body = bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"]
            .as_str()
            .unwrap();

        assert_eq!(
            payload,
            format!(
                r#"{{"body":"{body}","integratedTime":1710869186,"logID":"c0d23d6ad406973f9559f3ba2d1ca01f84147d8ffc5b8445c224f98b9591801d","logIndex":79571823}}"#
            )
        );
    }

    #[test]
    fn tlog_uses_protobuf_value_when_json_contains_field_aliases() {
        let bundle: Value = serde_json::from_slice(FIXTURE_BUNDLE).unwrap();
        let canonicalized_body =
            bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"]
                .as_str()
                .unwrap();
        let mut tampered_body = BASE64_STANDARD.decode(canonicalized_body).unwrap();
        tampered_body[0] ^= 1;
        let tampered_body = BASE64_STANDARD.encode(tampered_body);
        let original_field = format!(r#""canonicalizedBody": "{canonicalized_body}""#);
        let duplicate_aliases =
            format!(r#"{original_field}, "canonicalized_body": "{tampered_body}""#);
        let bundle = std::str::from_utf8(FIXTURE_BUNDLE).unwrap().replacen(
            &original_field,
            &duplicate_aliases,
            1,
        );
        assert!(bundle.contains("canonicalized_body"));
        let bundle = parse_bundle_for_test(bundle.as_bytes());
        let key = fixture_rekor_key();
        let material = prepare(&bundle, &key).unwrap();

        let error = verify_set(&material).unwrap_err();

        assert!(error.contains("signed entry timestamp"), "{error}");
    }

    #[test]
    fn rejects_flipped_canonicalized_body_byte() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            flip_base64_byte(
                &mut bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"],
            );
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_flipped_set_signature_byte() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            flip_base64_byte(
                &mut bundle["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"]["signedEntryTimestamp"],
            );
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_wrong_inclusion_proof_log_index() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["logIndex"] =
                json!("75408391");
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_truncated_inclusion_proof_hashes() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["hashes"]
                .as_array_mut()
                .unwrap()
                .pop();
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_tampered_checkpoint_root_hash() {
        let bundle = mutate_checkpoint(FIXTURE_BUNDLE, |lines| {
            lines[2] = BASE64_STANDARD.encode([0u8; 32]);
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_tampered_checkpoint_signature() {
        let bundle = mutate_checkpoint(FIXTURE_BUNDLE, |lines| {
            let (_, encoded) = lines[4].rsplit_once(' ').unwrap();
            let mut signature = BASE64_STANDARD.decode(encoded).unwrap();
            *signature.last_mut().unwrap() ^= 1;
            lines[4] = format!(
                "— {REKOR_ORIGIN_NAME} {}",
                BASE64_STANDARD.encode(signature)
            );
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_checkpoint_tree_size_mismatch() {
        let bundle = mutate_checkpoint(FIXTURE_BUNDLE, |lines| {
            lines[1] = "75408394".to_string();
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_checkpoint_origin_suffix_not_bound_to_signature() {
        let bundle = mutate_checkpoint(LIVE_BUNDLE, |lines| {
            lines[0] = "rekor.sigstore.dev - 1193050959916656507".to_string();
        });
        let bundle = parse_bundle_for_test(&bundle);
        let key = fixture_rekor_key();
        let material = prepare(&bundle, &key).unwrap();

        let error = verify_checkpoint(&material).unwrap_err();

        assert!(error.contains("checkpoint signature"), "{error}");
    }

    #[test]
    fn rejects_missing_inclusion_promise() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]
                .as_object_mut()
                .unwrap()
                .remove("inclusionPromise");
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_missing_inclusion_proof() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]
                .as_object_mut()
                .unwrap()
                .remove("inclusionProof");
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn rejects_missing_checkpoint() {
        let bundle = mutate_bundle(FIXTURE_BUNDLE, |bundle| {
            bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]
                .as_object_mut()
                .unwrap()
                .remove("checkpoint");
        });

        assert_verification_fails(&bundle);
    }

    #[test]
    fn inclusion_proof_matches_naive_rfc6962_trees() {
        for tree_size in 1..=64 {
            let leaves = (0..tree_size)
                .map(|index| format!("tree-{tree_size}-leaf-{index}").into_bytes())
                .collect::<Vec<_>>();
            let root = naive_tree_hash(&leaves);

            for index in 0..tree_size {
                let hashes = naive_audit_path(&leaves, index);
                verify_inclusion_proof_coordinates(
                    &leaves[index],
                    index as u64,
                    tree_size as u64,
                    &hashes,
                    &root,
                )
                .unwrap_or_else(|error| panic!("tree size {tree_size}, index {index}: {error}"));

                if !hashes.is_empty() {
                    let mut tampered = hashes.clone();
                    tampered[0][0] ^= 1;
                    assert!(
                        verify_inclusion_proof_coordinates(
                            &leaves[index],
                            index as u64,
                            tree_size as u64,
                            &tampered,
                            &root,
                        )
                        .is_err(),
                        "tampered proof passed for tree size {tree_size}, index {index}"
                    );
                }
            }
        }
    }

    fn naive_tree_hash(leaves: &[Vec<u8>]) -> [u8; 32] {
        if leaves.len() == 1 {
            return hash_leaf(&leaves[0]);
        }
        let split = largest_power_of_two_less_than(leaves.len());
        hash_children(
            &naive_tree_hash(&leaves[..split]),
            &naive_tree_hash(&leaves[split..]),
        )
    }

    fn naive_audit_path(leaves: &[Vec<u8>], index: usize) -> Vec<[u8; 32]> {
        if leaves.len() == 1 {
            return Vec::new();
        }
        let split = largest_power_of_two_less_than(leaves.len());
        if index < split {
            let mut path = naive_audit_path(&leaves[..split], index);
            path.push(naive_tree_hash(&leaves[split..]));
            path
        } else {
            let mut path = naive_audit_path(&leaves[split..], index - split);
            path.push(naive_tree_hash(&leaves[..split]));
            path
        }
    }

    fn largest_power_of_two_less_than(value: usize) -> usize {
        let mut power = 1;
        while power * 2 < value {
            power *= 2;
        }
        power
    }

    fn fixture_der_spki() -> Vec<u8> {
        BASE64_STANDARD.decode(REKOR_DER_SPKI_BASE64).unwrap()
    }

    fn fixture_rekor_key() -> RekorKey {
        let der_spki = fixture_der_spki();
        let key_id = Sha256::digest(&der_spki).into();
        RekorKey::new(der_spki, key_id, REKOR_ORIGIN_NAME.to_string()).unwrap()
    }

    fn parse_bundle_for_test(bundle_json: &[u8]) -> Bundle {
        serde_json::from_slice(bundle_json).expect("the test bundle should be valid JSON")
    }

    fn verify_bundle(bundle_json: &[u8]) -> Result<u64, String> {
        let bundle = parse_bundle_for_test(bundle_json);
        let key = fixture_rekor_key();
        verify_all(&bundle, &key)
    }

    fn mutate_bundle(bundle: &[u8], mutate: impl FnOnce(&mut Value)) -> Vec<u8> {
        let mut bundle: Value = serde_json::from_slice(bundle).unwrap();
        mutate(&mut bundle);
        serde_json::to_vec(&bundle).unwrap()
    }

    fn flip_base64_byte(value: &mut Value) {
        let mut bytes = BASE64_STANDARD.decode(value.as_str().unwrap()).unwrap();
        bytes[0] ^= 1;
        *value = json!(BASE64_STANDARD.encode(bytes));
    }

    fn mutate_checkpoint(bundle: &[u8], mutate: impl FnOnce(&mut Vec<String>)) -> Vec<u8> {
        mutate_bundle(bundle, |bundle| {
            let envelope = bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]
                ["checkpoint"]["envelope"]
                .as_str()
                .unwrap();
            let trailing_newline = envelope.ends_with('\n');
            let mut lines = envelope
                .trim_end_matches('\n')
                .split('\n')
                .map(str::to_string)
                .collect::<Vec<_>>();
            mutate(&mut lines);
            let mut envelope = lines.join("\n");
            if trailing_newline {
                envelope.push('\n');
            }
            bundle["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["checkpoint"]["envelope"] =
                json!(envelope);
        })
    }

    fn assert_verification_fails(bundle: &[u8]) {
        let error = verify_bundle(bundle).unwrap_err();
        assert!(!error.is_empty());
    }
}
