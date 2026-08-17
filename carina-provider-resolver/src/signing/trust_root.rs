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

pub(super) fn embedded_document() -> Result<TrustedRootDocument, String> {
    serde_json::from_str(TRUSTED_ROOT_JSON)
        .map_err(|error| format!("cannot parse embedded trusted_root.json: {error}"))
}

impl TrustedRootDocument {
    pub(super) fn verifier_root(
        &self,
        integrated_time: u64,
    ) -> Result<ManualTrustRoot<'static>, String> {
        let mut fulcio_certs = Vec::new();
        for authority in &self.certificate_authorities {
            if !valid_at(authority.valid_for.as_ref(), integrated_time)? {
                continue;
            }
            for certificate in &authority.cert_chain.certificates {
                fulcio_certs.push(decode_base64("Fulcio certificate", &certificate.raw_bytes)?);
            }
        }

        let rekor_keys = decode_valid_log_keys(&self.tlogs, integrated_time)?;
        let ctfe_keys = decode_valid_log_keys(&self.ctlogs, integrated_time)?;
        if fulcio_certs.is_empty() || rekor_keys.is_empty() || ctfe_keys.is_empty() {
            let integrated_time = format_timestamp(integrated_time)?;
            return Err(format!(
                "embedded trust root has no verification material valid at integrated time {integrated_time}; {TRUST_ROOT_STALENESS_HINT}"
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
        let all_expiring = self.every_rekor_key_expires_by(warning_threshold);

        if all_expiring {
            static WARNED: OnceLock<()> = OnceLock::new();
            if WARNED.set(()).is_ok() {
                eprintln!(
                    "WARNING: every Rekor key in carina's embedded Sigstore trust root expires within 60 days or has already expired. Upgrade carina now to refresh the pinned trust root."
                );
            }
        }
    }

    fn every_rekor_key_expires_by(&self, timestamp: u64) -> bool {
        !self.tlogs.is_empty()
            && self.tlogs.iter().all(|log| {
                log.public_key
                    .valid_for
                    .as_ref()
                    .and_then(|range| range.end.as_deref())
                    .is_some_and(|end| parse_timestamp(end).is_ok_and(|end| end <= timestamp))
            })
    }
}

fn decode_valid_log_keys(
    logs: &[TransparencyLog],
    integrated_time: u64,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut keys = BTreeMap::new();
    for log in logs {
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
        let unsupported_logs = root
            .tlogs
            .iter()
            .filter(|log| log.public_key.key_details != ECDSA_P256_KEY_DETAILS)
            .collect::<Vec<_>>();
        assert_eq!(
            unsupported_logs.len(),
            1,
            "embedded trust root must contain exactly one unsupported Rekor key type"
        );
        let unsupported_log = unsupported_logs[0];
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
        let supported_logs = root
            .tlogs
            .iter()
            .filter(|log| log.public_key.key_details == ECDSA_P256_KEY_DETAILS)
            .collect::<Vec<_>>();
        assert_eq!(
            supported_logs.len(),
            1,
            "embedded trust root must contain exactly one supported Rekor key type"
        );
        let supported_log = supported_logs[0];
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
    fn rekor_expiry_warning_requires_every_key_to_have_a_near_end() {
        let mut root = embedded_document().unwrap();
        assert!(!root.every_rekor_key_expires_by(u64::MAX));

        for log in &mut root.tlogs {
            log.public_key
                .valid_for
                .get_or_insert(ValidityPeriod {
                    start: None,
                    end: None,
                })
                .end = Some("2026-01-01T00:00:00Z".to_string());
        }

        assert!(root.every_rekor_key_expires_by(u64::MAX));
    }
}
