//! Capture build-time git metadata for `carina --version`.
//!
//! Exports three rustc env vars the binary reads back with `env!`:
//! - `CARINA_GIT_HASH`: 7-char short commit (empty when not a git build,
//!   e.g. when `cargo install`ing from a published crate).
//! - `CARINA_GIT_DIRTY`: `-dirty` when the working tree has uncommitted
//!   changes at build time, empty otherwise.
//! - `CARINA_BUILD_DATE`: UTC `YYYY-MM-DD` of the build (from
//!   `SOURCE_DATE_EPOCH` if set — respecting reproducible builds — else
//!   the current time). Always set.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn format_ymd_utc(unix_secs: u64) -> String {
    // Civil-from-days conversion (Howard Hinnant's algorithm), no chrono dep.
    let days = (unix_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", year, m, d)
}

fn main() {
    // Only watch `SOURCE_DATE_EPOCH` explicitly. We deliberately do NOT
    // emit `rerun-if-changed=<git-dir>/HEAD`/`index` because that would
    // also opt out of Cargo's default of re-running this script when
    // any package file changes — which is what catches plain working-
    // tree edits that never touch the index (so the `-dirty` marker
    // stays accurate). The script is cheap, so paying for package-scan
    // reruns is fine.
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let hash = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    // `diff-index --quiet HEAD` matches `git describe --dirty`: exit 0
    // when the tracked-file working tree matches HEAD, non-zero when it
    // differs. Untracked files are ignored on purpose, so a stray scratch
    // file doesn't flip `-dirty` on an otherwise pristine checkout.
    // `update-index --refresh` first to drop false positives from stat
    // cache drift (common after fresh checkouts).
    let _ = Command::new("git")
        .args(["update-index", "--refresh"])
        .output();
    let dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD"])
        .status()
        .ok()
        .map(|s| !s.success())
        .unwrap_or(false);

    let unix_secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

    let date = format_ymd_utc(unix_secs);
    let dirty_suffix = if dirty { "-dirty" } else { "" };
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Render the full version string once here so main.rs can just
    // `env!("CARINA_VERSION_STRING")` without const-context gymnastics.
    // Empty hash (e.g. `cargo install`ing from crates.io with no git
    // context) falls back to the bare crate version.
    let version_string = if hash.is_empty() {
        pkg_version.clone()
    } else {
        format!("{} ({}{} {})", pkg_version, hash, dirty_suffix, date)
    };

    println!("cargo:rustc-env=CARINA_GIT_HASH={}", hash);
    println!("cargo:rustc-env=CARINA_GIT_DIRTY={}", dirty_suffix);
    println!("cargo:rustc-env=CARINA_BUILD_DATE={}", date);
    println!("cargo:rustc-env=CARINA_VERSION_STRING={}", version_string);
}
