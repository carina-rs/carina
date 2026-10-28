use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;

use carina_provider_resolver::{self, LockMode, ProviderInstallLock};

use crate::commands::migrate_state::{MigrationOutcome, SourceDisposition, run_init_migrate_state};
use crate::commands::{
    BackendDriftStatus, drift_warning, ensure_backend_lock, inspect_backend_drift,
};
use carina_core::hint::ProjectCommand;

pub async fn run_init(
    path: &Path,
    upgrade: bool,
    locked: bool,
    migrate_state: bool,
    force: bool,
) -> Result<(), String> {
    run_init_with_provider_resolver(
        path,
        upgrade,
        locked,
        migrate_state,
        force,
        |install_lock, providers, mode| {
            carina_provider_resolver::resolve_all(install_lock, providers, mode)
                .map(|resolved| resolved.len())
                .map_err(|error| error.to_string())
        },
    )
    .await
}

async fn run_init_with_provider_resolver(
    path: &Path,
    upgrade: bool,
    locked: bool,
    migrate_state: bool,
    force: bool,
    resolve_providers: impl FnOnce(
        &ProviderInstallLock,
        &[carina_core::parser::ProviderConfig],
        LockMode,
    ) -> Result<usize, String>,
) -> Result<(), String> {
    if upgrade && locked {
        return Err("--upgrade and --locked are mutually exclusive".to_string());
    }
    let mode = if upgrade {
        LockMode::Upgrade
    } else if locked {
        LockMode::Locked
    } else {
        LockMode::Normal
    };

    let base_dir = get_base_dir(path);
    let path_buf = path.to_path_buf();

    let provider_context = ProviderContext::default();
    let loaded = load_configuration_with_config(
        &path_buf,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(|e| format!("Failed to load configuration: {e}"))?;

    // `source` is a kind-level property — only the kind's default
    // instance carries it. Named instances declared as
    // `let <name> = provider <kind> { ... }` cannot set it (the
    // parser rejects them). Restrict the pre-resolution check to
    // default instances so a named instance's deliberate absence
    // of `source` does not surface as a user error (carina#3023).
    let missing_source: Vec<String> = loaded
        .parsed
        .providers
        .iter()
        .filter(|provider| {
            provider.is_default()
                && provider.source.is_none()
                && !carina_provider_mock::uses_builtin_provider(std::iter::once(*provider), |_| {
                    false
                })
        })
        .map(|p| crate::commands::missing_provider_source_message(&p.name))
        .collect();
    if !missing_source.is_empty() {
        return Err(missing_source.join("\n"));
    }

    if !loaded.parsed.providers.is_empty() {
        let action = match mode {
            LockMode::Upgrade => "Upgrading",
            LockMode::Locked => "Verifying locked",
            LockMode::Normal => "Resolving",
        };
        println!(
            "{}",
            format!(
                "{} {} provider(s)...",
                action,
                loaded.parsed.providers.len()
            )
            .cyan()
        );

        let install_lock = ProviderInstallLock::acquire(base_dir).map_err(|error| {
            format!(
                "Failed to acquire provider install lock for {}: {error}",
                base_dir.display()
            )
        })?;
        let resolved_count = resolve_providers(&install_lock, &loaded.parsed.providers, mode)?;

        println!(
            "{}",
            format!(
                "{} provider(s) installed in .carina/providers/",
                resolved_count
            )
            .green()
        );
    }

    let backend_config = loaded.parsed.backend.as_ref();
    let mut migration_pending = false;

    match inspect_backend_drift(base_dir, backend_config).map_err(|e| e.to_string())? {
        BackendDriftStatus::Fresh => {
            ensure_backend_lock(base_dir, backend_config)
                .map_err(|e| format!("Failed to create backend lock: {e}"))?;
            if migrate_state {
                println!(
                    "{}",
                    "No state migration needed; backend lock initialized.".green()
                );
            }
        }
        BackendDriftStatus::Unchanged => {
            // Already initialized against this backend; nothing to do.
            if migrate_state {
                println!(
                    "{}",
                    "No state migration needed; backend lock already matches the configuration."
                        .green()
                );
            }
        }
        BackendDriftStatus::Drifted(drift) => {
            if !migrate_state {
                eprintln!(
                    "{}",
                    drift_warning(drift.existing(), drift.configured(), path).yellow()
                );
                migration_pending = true;
            } else {
                match run_init_migrate_state(path, backend_config, force)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    MigrationOutcome::NotNeeded => {
                        // Races with another init that already migrated; the
                        // lock now matches, so this is a benign no-op.
                    }
                    MigrationOutcome::Migrated { resources, source } => {
                        println!(
                            "{}",
                            format!("State migrated ({resources} resource(s)).").green()
                        );
                        // `DeleteFailed` already produced a precise stderr
                        // warning inside the migration; only the deliberate
                        // remote-backup case needs this extra hint here.
                        if source == SourceDisposition::KeptAsBackup {
                            println!(
                                "The old state at the previous backend was kept as a \
                                 backup; remove it manually once you have confirmed \
                                 the new backend."
                            );
                        }
                    }
                }
            }
        }
    }

    if migration_pending {
        println!(
            "{}",
            format!(
                "Backend migration pending. Provider plugins resolved; run `{}` to complete initialization.",
                ProjectCommand::new("init --migrate-state", path)
            )
            .yellow()
        );
    } else {
        println!("{}", "Initialized successfully.".green());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_provider_resolver::FixtureRegistryHttp;
    use std::fs;

    const SIGNED_ARTIFACT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../carina-provider-resolver/src/signing/testdata/a.txt"
    ));
    const SIGNED_BUNDLE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../carina-provider-resolver/src/signing/testdata/bundle.sigstore.json"
    ));
    const SIGNED_VERSION: &str = "0.0.0-main.1";
    const SIGNED_SHA256: &str = "a0cfc71271d6e278e57cd332ff957c3f7043fdda354c4cbb190a30d56efa01bf";
    const SIGNED_ARTIFACT_URL: &str = "https://downloads.example.test/aws-main.wasm";
    const SIGNED_BUNDLE_URL: &str = "https://downloads.example.test/aws-main.sigstore.json";

    fn signed_fixture_registry() -> FixtureRegistryHttp {
        FixtureRegistryHttp::default()
            .json(
                "https://registry.carina-rs.dev/.well-known/carina.json",
                r#"{"providers.v1":"/v1/providers/"}"#,
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
                format!(
                    r#"{{"sequence":7,"valid_until":"2999-01-01T00:00:00Z","versions":[{{"version":"{SIGNED_VERSION}","protocols":["1"]}}]}}"#
                ),
            )
            .json(
                "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.0.0-main.1/download",
                format!(
                    r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"{SIGNED_ARTIFACT_URL}","shasum":"{SIGNED_SHA256}","shasum_authored_by":"registry","signature":{{"type":"sigstore-bundle","certificate_identity":"deliberately-wrong","certificate_oidc_issuer":"https://wrong.example.test","bundle_url":"{SIGNED_BUNDLE_URL}"}}}}"#
                ),
            )
            .response(SIGNED_BUNDLE_URL, 200, SIGNED_BUNDLE)
            .downloadable_bytes(SIGNED_ARTIFACT_URL, SIGNED_ARTIFACT.to_vec())
    }

    async fn assert_init_runs_real_verifier_inside_async_command() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("providers.crn"),
            r#"provider aws {
  source = 'carina-rs/aws'
  version = '0.0.0-main.1'
}
"#,
        )
        .unwrap();
        let http = signed_fixture_registry();

        run_init_with_provider_resolver(
            project.path(),
            false,
            false,
            false,
            false,
            |install_lock, providers, mode| {
                carina_provider_resolver::resolve_all_with_fixture_registry(
                    install_lock,
                    providers,
                    mode,
                    &http,
                )
                .map(|resolved| resolved.len())
                .map_err(|error| error.to_string())
            },
        )
        .await
        .unwrap();

        let lock = fs::read_to_string(project.path().join("carina-providers.lock")).unwrap();
        assert!(lock.contains("version = 4"), "{lock}");
        assert!(
            lock.contains("signature_state = \"required_current\""),
            "{lock}"
        );
        assert!(
            lock.contains("prerelease_workflow = \"https://github.com/sigstore-conformance/extremely-dangerous-public-oidc-beacon/.github/workflows/extremely-dangerous-oidc-beacon.yml\""),
            "{lock}"
        );
        assert!(
            project
                .path()
                .join(".carina/providers/carina-rs/aws/0.0.0-main.1/aws.wasm")
                .is_file()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn init_real_verifier_is_safe_on_current_thread_runtime() {
        assert_init_runs_real_verifier_inside_async_command().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn init_real_verifier_is_safe_on_multi_thread_runtime() {
        assert_init_runs_real_verifier_inside_async_command().await;
    }
}
