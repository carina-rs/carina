use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use carina_sigstore_tlog::RekorKey;
use serde::Deserialize;
use sigstore::trust::ManualTrustRoot;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::TRUST_ROOT_STALENESS_HINT;

const TRUSTED_ROOT_JSON: &str = include_str!("trusted_root.json");
const REKOR_EXPIRY_WARNING_SECONDS: u64 = 60 * 24 * 60 * 60;
const ECDSA_P256_KEY_DETAILS: &str = "PKIX_ECDSA_P256_SHA_256";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrustedRootDocument {
    pub(super) tlogs: Vec<TransparencyLog>,
    certificate_authorities: Vec<CertificateAuthority>,
    ctlogs: Vec<TransparencyLog>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TransparencyLog {
    base_url: String,
    public_key: PublicKey,
    log_id: LogId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicKey {
    raw_bytes: String,
    key_details: String,
    #[serde(default)]
    valid_for: Option<ValidityPeriod>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogId {
    key_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CertificateAuthority {
    cert_chain: CertificateChain,
    #[serde(default)]
    valid_for: Option<ValidityPeriod>,
}

#[derive(Deserialize)]
struct ValidityPeriod {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

#[derive(Deserialize)]
struct CertificateChain {
    certificates: Vec<RawCertificate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCertificate {
    raw_bytes: String,
}

#[derive(Debug, Eq, PartialEq)]
enum RekorKeyExpiryStatus {
    NoUsableKeys,
    AllExpiring,
    Fine,
}

pub(super) fn embedded_document() -> Result<TrustedRootDocument, String> {
    serde_json::from_str(TRUSTED_ROOT_JSON)
        .map_err(|error| format!("cannot parse embedded trusted_root.json: {error}"))
}

impl TrustedRootDocument {
    pub(super) fn verifier_root(
        &self,
        integrated_time: u64,
    ) -> Result<ManualTrustRoot<'static>, String> {
        let fulcio_certs =
            decode_valid_fulcio_certs(&self.certificate_authorities, integrated_time)?;
        let rekor_keys = decode_valid_log_keys(&self.tlogs, integrated_time)?;
        let ctfe_keys = decode_valid_log_keys(&self.ctlogs, integrated_time)?;
        let missing_material = [
            fulcio_certs.is_empty().then_some("Fulcio"),
            rekor_keys.is_empty().then_some("Rekor"),
            ctfe_keys.is_empty().then_some("CTFE"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !missing_material.is_empty() {
            let integrated_time = format_timestamp(integrated_time)?;
            return Err(format!(
                "embedded trust root has no verification material valid at integrated time {integrated_time} (missing: {}); {TRUST_ROOT_STALENESS_HINT}",
                missing_material.join(", ")
            ));
        }

        Ok(ManualTrustRoot {
            fulcio_certs: fulcio_certs.into_iter().map(Into::into).collect(),
            rekor_keys,
            ctfe_keys,
        })
    }

    pub(super) fn select_rekor_key(
        &self,
        entry_key_id: &[u8],
        integrated_time: u64,
    ) -> Result<RekorKey, String> {
        for log in &self.tlogs {
            let declared_key_id = decode_base64("Rekor log key ID", &log.log_id.key_id)?;
            if declared_key_id != entry_key_id {
                continue;
            }
            if log.public_key.key_details != ECDSA_P256_KEY_DETAILS {
                return Err(format!(
                    "Rekor log key {} uses unsupported key type {}",
                    lower_hex(entry_key_id),
                    log.public_key.key_details
                ));
            }
            if !valid_at(log.public_key.valid_for.as_ref(), integrated_time)? {
                return Err(format!(
                    "Rekor log key {} is not valid at integrated time {integrated_time}",
                    lower_hex(entry_key_id)
                ));
            }
            let origin_name = https_origin_host(&log.base_url)?;
            let key_id = declared_key_id.try_into().map_err(|key_id: Vec<u8>| {
                format!(
                    "Rekor log key ID must decode to 32 bytes, got {}",
                    key_id.len()
                )
            })?;
            let der_spki = decode_base64("Rekor log public key", &log.public_key.raw_bytes)?;
            return RekorKey::new(der_spki, key_id, origin_name).map_err(|_| {
                format!(
                    "embedded Rekor log ID does not match the SHA-256 of its DER public key for {}",
                    log.base_url
                )
            });
        }

        Err(format!(
            "bundle log ID {} is not present in the embedded trust root; {TRUST_ROOT_STALENESS_HINT}",
            lower_hex(entry_key_id),
        ))
    }

    pub(super) fn warn_if_rekor_keys_near_expiry(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let warning_threshold = now.saturating_add(REKOR_EXPIRY_WARNING_SECONDS);
        let warning = match self.rekor_key_expiry_status(warning_threshold) {
            RekorKeyExpiryStatus::NoUsableKeys => {
                "WARNING: carina cannot verify with any Rekor key in its embedded Sigstore trust root. Upgrade carina now to refresh the pinned trust root."
            }
            RekorKeyExpiryStatus::AllExpiring => {
                "WARNING: every Rekor key in carina's embedded Sigstore trust root expires within 60 days or has already expired. Upgrade carina now to refresh the pinned trust root."
            }
            RekorKeyExpiryStatus::Fine => return,
        };

        static WARNED: OnceLock<()> = OnceLock::new();
        if WARNED.set(()).is_ok() {
            eprintln!("{warning}");
        }
    }

    fn rekor_key_expiry_status(&self, timestamp: u64) -> RekorKeyExpiryStatus {
        if self.tlogs.is_empty() {
            return RekorKeyExpiryStatus::Fine;
        }

        let mut usable_keys = usable_log_keys(&self.tlogs).peekable();
        if usable_keys.peek().is_none() {
            return RekorKeyExpiryStatus::NoUsableKeys;
        }

        if usable_keys.all(|log| {
            log.public_key
                .valid_for
                .as_ref()
                .and_then(|range| range.end.as_deref())
                .is_some_and(|end| parse_timestamp(end).is_ok_and(|end| end <= timestamp))
        }) {
            RekorKeyExpiryStatus::AllExpiring
        } else {
            RekorKeyExpiryStatus::Fine
        }
    }
}

fn decode_valid_fulcio_certs(
    authorities: &[CertificateAuthority],
    integrated_time: u64,
) -> Result<Vec<Vec<u8>>, String> {
    let mut certificates = Vec::new();
    for authority in authorities {
        if !valid_at(authority.valid_for.as_ref(), integrated_time)? {
            continue;
        }
        for certificate in &authority.cert_chain.certificates {
            certificates.push(decode_base64("Fulcio certificate", &certificate.raw_bytes)?);
        }
    }
    Ok(certificates)
}

fn usable_log_keys(logs: &[TransparencyLog]) -> impl Iterator<Item = &TransparencyLog> {
    logs.iter()
        .filter(|log| log.public_key.key_details == ECDSA_P256_KEY_DETAILS)
}

fn decode_valid_log_keys(
    logs: &[TransparencyLog],
    integrated_time: u64,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut keys = BTreeMap::new();
    for log in usable_log_keys(logs) {
        if !valid_at(log.public_key.valid_for.as_ref(), integrated_time)? {
            continue;
        }
        let key_id = decode_base64("transparency log key ID", &log.log_id.key_id)?;
        let key = decode_base64("transparency log public key", &log.public_key.raw_bytes)?;
        keys.insert(lower_hex(&key_id), key);
    }
    Ok(keys)
}

fn valid_at(range: Option<&ValidityPeriod>, timestamp: u64) -> Result<bool, String> {
    let start = range
        .and_then(|range| range.start.as_deref())
        .map(parse_timestamp)
        .transpose()?;
    let end = range
        .and_then(|range| range.end.as_deref())
        .map(parse_timestamp)
        .transpose()?;
    Ok(start.is_none_or(|start| start <= timestamp) && end.is_none_or(|end| timestamp <= end))
}

fn parse_timestamp(value: &str) -> Result<u64, String> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("invalid trust-root timestamp {value:?}: {error}"))?;
    u64::try_from(parsed.unix_timestamp())
        .map_err(|_| format!("trust-root timestamp predates the Unix epoch: {value:?}"))
}

fn format_timestamp(value: u64) -> Result<String, String> {
    let value = i64::try_from(value)
        .map_err(|_| format!("integrated time {value} is outside the RFC3339 timestamp range"))?;
    OffsetDateTime::from_unix_timestamp(value)
        .map_err(|error| {
            format!("integrated time {value} is outside the RFC3339 timestamp range: {error}")
        })?
        .format(&Rfc3339)
        .map_err(|error| format!("cannot render integrated time as RFC3339: {error}"))
}

fn decode_base64(kind: &str, encoded: &str) -> Result<Vec<u8>, String> {
    BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("{kind} is not valid base64: {error}"))
}

fn https_origin_host(base_url: &str) -> Result<String, String> {
    let without_scheme = base_url
        .strip_prefix("https://")
        .ok_or_else(|| format!("trust-root log URL must use HTTPS: {base_url}"))?;
    let host = without_scheme.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(format!("trust-root log URL has no host: {base_url}"));
    }
    Ok(host.to_string())
}

pub(super) fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_rekor_log_index(logs: &[TransparencyLog], supported: bool) -> usize {
        let matching_indices = logs
            .iter()
            .enumerate()
            .filter(|(_, log)| (log.public_key.key_details == ECDSA_P256_KEY_DETAILS) == supported)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let support = if supported {
            "supported"
        } else {
            "unsupported"
        };
        assert_eq!(
            matching_indices.len(),
            1,
            "embedded trust root must contain exactly one {support} Rekor key type"
        );
        matching_indices[0]
    }

    fn single_supported_rekor_log(logs: &[TransparencyLog]) -> &TransparencyLog {
        &logs[single_rekor_log_index(logs, true)]
    }

    fn single_supported_rekor_log_mut(logs: &mut [TransparencyLog]) -> &mut TransparencyLog {
        let index = single_rekor_log_index(logs, true);
        &mut logs[index]
    }

    fn single_unsupported_rekor_log(logs: &[TransparencyLog]) -> &TransparencyLog {
        &logs[single_rekor_log_index(logs, false)]
    }

    #[test]
    fn verifier_root_excludes_fulcio_ca_retired_before_integrated_time() {
        let root = embedded_document().unwrap();
        let during_retired_ca = parse_timestamp("2022-06-01T00:00:00Z").unwrap();
        let after_retired_ca = parse_timestamp("2024-03-19T17:26:26Z").unwrap();

        assert_eq!(
            root.verifier_root(during_retired_ca)
                .unwrap()
                .fulcio_certs
                .len(),
            3
        );
        assert_eq!(
            root.verifier_root(after_retired_ca)
                .unwrap()
                .fulcio_certs
                .len(),
            2
        );
    }

    #[test]
    fn verifier_root_reports_rfc3339_time_and_upgrade_guidance() {
        let root = embedded_document().unwrap();
        let integrated_time = parse_timestamp("2020-01-01T00:00:00Z").unwrap();

        let error = root.verifier_root(integrated_time).unwrap_err();

        assert!(error.contains("2020-01-01T00:00:00Z"), "{error}");
        assert!(!error.contains(&integrated_time.to_string()), "{error}");
        assert!(error.contains("trust root may be outdated"), "{error}");
        assert!(
            error.contains("upgrading carina may resolve this"),
            "{error}"
        );
    }

    #[test]
    fn unknown_rekor_log_id_includes_upgrade_guidance() {
        let root = embedded_document().unwrap();
        let integrated_time = parse_timestamp("2024-03-19T17:26:26Z").unwrap();

        let error = match root.select_rekor_key(&[0; 32], integrated_time) {
            Ok(_) => panic!("an unknown Rekor log ID must be rejected"),
            Err(error) => error,
        };

        assert!(
            error.contains("not present in the embedded trust root"),
            "{error}"
        );
        assert!(error.contains("trust root may be outdated"), "{error}");
        assert!(
            error.contains("upgrading carina may resolve this"),
            "{error}"
        );
    }

    #[test]
    fn unsupported_rekor_key_type_precedes_validity_window() {
        let root = embedded_document().unwrap();
        let unsupported_log = single_unsupported_rekor_log(&root.tlogs);
        let validity_start = unsupported_log
            .public_key
            .valid_for
            .as_ref()
            .and_then(|range| range.start.as_deref())
            .expect("unsupported Rekor log key must have valid_for.start");
        let validity_start = parse_timestamp(validity_start).unwrap();
        let inside_validity_window = validity_start
            .checked_add(1)
            .expect("unsupported Rekor log validity-window start must permit a later timestamp");
        let before_validity_window = validity_start
            .checked_sub(1)
            .expect("unsupported Rekor log validity-window start must permit an earlier timestamp");
        assert!(
            valid_at(
                unsupported_log.public_key.valid_for.as_ref(),
                inside_validity_window
            )
            .unwrap(),
            "timestamp after unsupported Rekor log validity-window start must be inside the window"
        );
        assert!(
            !valid_at(
                unsupported_log.public_key.valid_for.as_ref(),
                before_validity_window
            )
            .unwrap(),
            "timestamp before unsupported Rekor log validity-window start must be outside the window"
        );
        let key_id = decode_base64("Rekor log key ID", &unsupported_log.log_id.key_id).unwrap();
        let expected_error = format!(
            "Rekor log key {} uses unsupported key type {}",
            lower_hex(&key_id),
            unsupported_log.public_key.key_details
        );

        for integrated_time in [inside_validity_window, before_validity_window] {
            let error = match root.select_rekor_key(&key_id, integrated_time) {
                Ok(_) => panic!("an unsupported Rekor key type must be rejected"),
                Err(error) => error,
            };

            assert_eq!(error, expected_error);
            assert!(!error.contains("not valid at integrated time"), "{error}");
            assert!(!error.contains(TRUST_ROOT_STALENESS_HINT), "{error}");
        }
    }

    #[test]
    fn supported_rekor_key_outside_validity_window_reports_the_window() {
        let root = embedded_document().unwrap();
        let supported_log = single_supported_rekor_log(&root.tlogs);
        let validity_start = supported_log
            .public_key
            .valid_for
            .as_ref()
            .and_then(|range| range.start.as_deref())
            .expect("supported Rekor log key must have valid_for.start");
        let validity_start = parse_timestamp(validity_start).unwrap();
        let before_validity_window = validity_start
            .checked_sub(1)
            .expect("supported Rekor log validity-window start must permit an earlier timestamp");
        assert!(
            !valid_at(
                supported_log.public_key.valid_for.as_ref(),
                before_validity_window
            )
            .unwrap(),
            "timestamp before supported Rekor log validity-window start must be outside the window"
        );
        let key_id = decode_base64("Rekor log key ID", &supported_log.log_id.key_id).unwrap();
        let expected_error = format!(
            "Rekor log key {} is not valid at integrated time {before_validity_window}",
            lower_hex(&key_id)
        );

        let error = match root.select_rekor_key(&key_id, before_validity_window) {
            Ok(_) => panic!("a supported Rekor key outside its validity window must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error, expected_error);
        assert!(!error.contains("uses unsupported key type"), "{error}");
        assert!(!error.contains(TRUST_ROOT_STALENESS_HINT), "{error}");
    }

    #[test]
    fn verifier_root_rejects_only_window_valid_rekor_key_with_unsupported_algorithm() {
        let mut root = embedded_document().unwrap();
        let unsupported_log = single_unsupported_rekor_log(&root.tlogs);
        let unsupported_validity_start = unsupported_log
            .public_key
            .valid_for
            .as_ref()
            .and_then(|range| range.start.as_deref())
            .expect("unsupported Rekor log key must have valid_for.start")
            .to_string();
        let integrated_time = parse_timestamp(&unsupported_validity_start)
            .unwrap()
            .checked_add(1)
            .expect("unsupported Rekor log validity-window start must permit a later timestamp");
        assert!(
            valid_at(
                unsupported_log.public_key.valid_for.as_ref(),
                integrated_time
            )
            .unwrap(),
            "integrated time must be inside the unsupported Rekor key's validity window"
        );

        let supported_log = single_supported_rekor_log_mut(&mut root.tlogs);
        supported_log
            .public_key
            .valid_for
            .get_or_insert(ValidityPeriod {
                start: None,
                end: None,
            })
            .end = Some(unsupported_validity_start);
        assert!(
            !valid_at(supported_log.public_key.valid_for.as_ref(), integrated_time).unwrap(),
            "integrated time must be outside the supported Rekor key's retired validity window"
        );

        let fulcio_certs =
            decode_valid_fulcio_certs(&root.certificate_authorities, integrated_time).unwrap();
        let rekor_keys = decode_valid_log_keys(&root.tlogs, integrated_time).unwrap();
        let ctfe_keys = decode_valid_log_keys(&root.ctlogs, integrated_time).unwrap();
        assert!(
            !fulcio_certs.is_empty(),
            "Fulcio verification material must be valid at the integrated time"
        );
        assert!(
            rekor_keys.is_empty(),
            "Rekor verification material must be empty at the integrated time"
        );
        assert!(
            !ctfe_keys.is_empty(),
            "CTFE verification material must be valid at the integrated time"
        );

        let error = root.verifier_root(integrated_time).unwrap_err();

        let integrated_time = format_timestamp(integrated_time).unwrap();
        assert_eq!(
            error,
            format!(
                "embedded trust root has no verification material valid at integrated time {integrated_time} (missing: Rekor); {TRUST_ROOT_STALENESS_HINT}"
            )
        );
    }

    #[test]
    fn decode_valid_log_keys_excludes_window_valid_unsupported_key() {
        let root = embedded_document().unwrap();
        let unsupported_log = single_unsupported_rekor_log(&root.tlogs);
        let validity_start = unsupported_log
            .public_key
            .valid_for
            .as_ref()
            .and_then(|range| range.start.as_deref())
            .expect("unsupported Rekor log key must have valid_for.start");
        let inside_validity_window = parse_timestamp(validity_start)
            .unwrap()
            .checked_add(1)
            .expect("unsupported Rekor log validity-window start must permit a later timestamp");
        assert!(
            valid_at(
                unsupported_log.public_key.valid_for.as_ref(),
                inside_validity_window
            )
            .unwrap(),
            "timestamp must be inside the unsupported Rekor key's validity window"
        );
        let key_id = decode_base64("Rekor log key ID", &unsupported_log.log_id.key_id).unwrap();

        let keys = decode_valid_log_keys(
            std::slice::from_ref(unsupported_log),
            inside_validity_window,
        )
        .unwrap();

        assert!(
            !keys.contains_key(&lower_hex(&key_id)),
            "a window-valid unsupported key must not be decoded"
        );
    }

    #[test]
    fn rekor_expiry_warning_requires_every_usable_key_to_have_a_near_end() {
        let mut root = embedded_document().unwrap();
        assert_eq!(
            root.rekor_key_expiry_status(u64::MAX),
            RekorKeyExpiryStatus::Fine
        );

        for log in root
            .tlogs
            .iter_mut()
            .filter(|log| log.public_key.key_details == ECDSA_P256_KEY_DETAILS)
        {
            log.public_key
                .valid_for
                .get_or_insert(ValidityPeriod {
                    start: None,
                    end: None,
                })
                .end = Some("2026-01-01T00:00:00Z".to_string());
        }

        assert_eq!(
            root.rekor_key_expiry_status(u64::MAX),
            RekorKeyExpiryStatus::AllExpiring
        );
    }

    #[test]
    fn rekor_expiry_warning_distinguishes_unsupported_keys_from_empty_logs() {
        let mut root = embedded_document().unwrap();
        root.tlogs
            .retain(|log| log.public_key.key_details != ECDSA_P256_KEY_DETAILS);
        assert!(!root.tlogs.is_empty());
        assert_eq!(usable_log_keys(&root.tlogs).count(), 0);
        for log in &mut root.tlogs {
            log.public_key
                .valid_for
                .get_or_insert(ValidityPeriod {
                    start: None,
                    end: None,
                })
                .end = Some("2026-01-01T00:00:00Z".to_string());
        }

        assert_eq!(
            root.rekor_key_expiry_status(u64::MAX),
            RekorKeyExpiryStatus::NoUsableKeys
        );

        root.tlogs.clear();

        assert_eq!(
            root.rekor_key_expiry_status(u64::MAX),
            RekorKeyExpiryStatus::Fine
        );
    }

    #[test]
    fn rekor_expiry_warning_counts_only_supported_keys() {
        let mut root = embedded_document().unwrap();
        let supported_log_index = single_rekor_log_index(&root.tlogs, true);
        let unsupported_log = single_unsupported_rekor_log(&root.tlogs);
        let retirement_end = unsupported_log
            .public_key
            .valid_for
            .as_ref()
            .and_then(|range| range.start.as_deref())
            .expect("unsupported Rekor log key must have valid_for.start")
            .to_string();
        assert!(
            unsupported_log
                .public_key
                .valid_for
                .as_ref()
                .is_some_and(|range| range.end.is_none()),
            "unsupported Rekor log key must not have valid_for.end"
        );
        let retirement_end_timestamp = parse_timestamp(&retirement_end).unwrap();
        let warning_threshold = retirement_end_timestamp
            .checked_add(1)
            .expect("Rekor log validity-window end must permit a later timestamp");

        root.tlogs[supported_log_index]
            .public_key
            .valid_for
            .get_or_insert(ValidityPeriod {
                start: None,
                end: None,
            })
            .end = Some(retirement_end);

        assert_eq!(
            root.rekor_key_expiry_status(retirement_end_timestamp),
            RekorKeyExpiryStatus::AllExpiring
        );
        assert_eq!(
            root.rekor_key_expiry_status(warning_threshold),
            RekorKeyExpiryStatus::AllExpiring
        );
    }
}
