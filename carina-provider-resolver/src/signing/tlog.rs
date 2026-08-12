use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use p256::ecdsa::signature::hazmat::PrehashVerifier as _;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use sigstore::bundle::Bundle;

use super::trust_root::{SelectedLogKey, TrustedRootDocument, lower_hex};

struct PreparedTlog {
    canonicalized_body: Vec<u8>,
    signed_entry_timestamp: Vec<u8>,
    integrated_time: u64,
    log_index: u64,
    proof: PreparedProof,
    key: SelectedLogKey,
}

struct PreparedProof {
    checkpoint: String,
    hashes: Vec<[u8; 32]>,
    log_index: u64,
    root_hash: [u8; 32],
    tree_size: u64,
}

pub(super) fn verify_all(bundle: &Bundle, trust_root: &TrustedRootDocument) -> Result<u64, String> {
    let material = prepare(bundle, trust_root)?;
    verify_set(&material)?;
    verify_inclusion_proof(&material)?;
    verify_checkpoint(&material)?;
    Ok(material.integrated_time)
}

fn prepare(bundle: &Bundle, trust_root: &TrustedRootDocument) -> Result<PreparedTlog, String> {
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
    let key = trust_root.select_rekor_key(&log_id.key_id, integrated_time)?;

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
        integrated_time,
        log_index,
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
pub(super) fn verify_set_only(bundle_json: &[u8]) -> Result<(), String> {
    let trust_root = super::trust_root::embedded_document()?;
    verify_set(&prepare(&parse_bundle_for_test(bundle_json)?, &trust_root)?)
}

#[cfg(test)]
pub(super) fn verify_inclusion_proof_only(bundle_json: &[u8]) -> Result<(), String> {
    let trust_root = super::trust_root::embedded_document()?;
    verify_inclusion_proof(&prepare(&parse_bundle_for_test(bundle_json)?, &trust_root)?)
}

#[cfg(test)]
pub(super) fn verify_checkpoint_only(bundle_json: &[u8]) -> Result<(), String> {
    let trust_root = super::trust_root::embedded_document()?;
    verify_checkpoint(&prepare(&parse_bundle_for_test(bundle_json)?, &trust_root)?)
}

#[cfg(test)]
pub(super) fn set_payload_for_test(bundle_json: &[u8]) -> Result<String, String> {
    let trust_root = super::trust_root::embedded_document()?;
    set_payload(&prepare(&parse_bundle_for_test(bundle_json)?, &trust_root)?)
}

#[cfg(test)]
fn parse_bundle_for_test(bundle_json: &[u8]) -> Result<Bundle, String> {
    serde_json::from_slice(bundle_json)
        .map_err(|error| format!("cannot parse Sigstore bundle JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
