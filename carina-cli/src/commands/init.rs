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
    user_dirs: &carina_core::UserDirs,
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
            carina_provider_resolver::resolve_all(user_dirs, install_lock, providers, mode)
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
        let provider_configs = crate::wiring::provider_loading_configs(&loaded.parsed.providers);
        let resolved_count = resolve_providers(&install_lock, &provider_configs, mode)?;

        println!(
            "{}",
            format!(
                "{} provider(s) installed in .carina/providers/",
                resolved_count
            )
            .green()
        );
    }

    let backend_config = loaded
        .parsed
        .backend
        .clone()
        .map(carina_core::parser::BackendConfig::into_expanded)
        .transpose()
        .map_err(|error| error.to_string())?;
    let backend_config = backend_config.as_ref();
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
    use carina_core::UserDirs;
    use carina_core::provider_source::{ProviderSource, RemoteSource};
    use carina_provider_resolver::{
        AssetId, FixtureGithubHttp, FixtureRegistryHttp, LockEntry, LockEntryKind, LockFile,
        ProviderTrustClass, ReleaseTag, TrustedLocalGrants,
    };
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

    fn official_signed_fixture_registry() -> FixtureRegistryHttp {
        const WORKFLOW: &str =
            "https://github.com/carina-rs/carina-provider-aws/.github/workflows/ci.yml";
        signed_fixture_registry().verified_signer_identity(
            &format!("{WORKFLOW}@refs/heads/main"),
            "https://token.actions.githubusercontent.com",
            "260157389",
            "123456789",
            "refs/heads/main",
            WORKFLOW,
            None,
        )
    }

    fn tree_contains_file(path: &Path) -> bool {
        fs::read_dir(path).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry.file_type().is_ok_and(|kind| {
                    kind.is_file() || (kind.is_dir() && tree_contains_file(&entry.path()))
                })
            })
        })
    }

    fn request_count(http: &FixtureRegistryHttp, url: &str) -> usize {
        http.requested_urls()
            .iter()
            .filter(|requested| requested.as_str() == url)
            .count()
    }

    async fn run_init_with_fixture(
        user_dirs: &UserDirs,
        project: &Path,
        http: &FixtureRegistryHttp,
    ) {
        run_init_with_provider_resolver(
            project,
            false,
            false,
            false,
            false,
            |install_lock, providers, mode| {
                carina_provider_resolver::resolve_all_with_fixture_registry(
                    user_dirs,
                    install_lock,
                    providers,
                    mode,
                    http,
                )
                .map(|resolved| resolved.len())
                .map_err(|error| error.to_string())
            },
        )
        .await
        .unwrap();
    }

    async fn run_init_with_fixtures(
        user_dirs: &UserDirs,
        project: &Path,
        registry_http: &FixtureRegistryHttp,
        github_http: &FixtureGithubHttp,
    ) {
        run_init_with_provider_resolver(
            project,
            false,
            false,
            false,
            false,
            |install_lock, providers, mode| {
                carina_provider_resolver::resolve_all_with_fixture_clients(
                    user_dirs,
                    install_lock,
                    providers,
                    mode,
                    registry_http,
                    github_http,
                )
                .map(|resolved| resolved.len())
                .map_err(|error| error.to_string())
            },
        )
        .await
        .unwrap();
    }

    async fn assert_installed_provider_loads_as_aws(user_dirs: &UserDirs, project: &Path) {
        let loaded = load_configuration_with_config(
            project,
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let config = loaded.parsed.providers.first().unwrap();
        let installed = carina_provider_resolver::find_installed_provider(project, config).unwrap();
        let grants = TrustedLocalGrants::parse(&[], None).unwrap();
        let class = installed
            .load_with(user_dirs, &grants, |verified| async move {
                Ok::<_, std::convert::Infallible>(verified.identity().trust().class())
            })
            .await
            .unwrap();
        assert_eq!(class, ProviderTrustClass::Aws);
    }

    #[test]
    fn init_with_up_to_date_github_lock_records_fresh_runner_trust_once() {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(init_with_up_to_date_github_lock_case());
    }

    async fn init_with_up_to_date_github_lock_case() {
        let project = tempfile::tempdir().unwrap();
        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
        let source = "github.com/carina-rs/carina-provider-aws";
        let version = "0.1.0";
        let bytes = b"up-to-date GitHub provider";
        fs::write(
            project.path().join("providers.crn"),
            format!("provider aws {{\n  source = '{source}'\n}}\n"),
        )
        .unwrap();
        let parsed_source: ProviderSource = source.parse().unwrap();
        let ProviderSource::Remote(RemoteSource::GithubDirect(github_source)) = &parsed_source
        else {
            panic!("test source must be GitHub-direct")
        };
        let source_key = github_source.source_key();
        let artifact =
            carina_provider_resolver::cache_path_wasm(project.path(), &source_key, version);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, bytes).unwrap();
        let mut lock = LockFile::default();
        lock.upsert(LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: LockEntryKind::GithubRelease {
                version: version.parse().unwrap(),
                tag: serde_json::from_str::<ReleaseTag>(&format!("\"v{version}\"")).unwrap(),
                asset_id: serde_json::from_str::<AssetId>("42").unwrap(),
                constraint: None,
            },
            sha256: carina_provider_resolver::sha256_file(&artifact).unwrap(),
            registry: None,
        });
        let lock_path = project.path().join("carina-providers.lock");
        lock.save(&lock_path).unwrap();
        let lock_before = fs::read(&lock_path).unwrap();
        let release_url = format!(
            "https://api.github.com/repos/{}/{}/releases/tags/v{version}",
            github_source.owner(),
            github_source.repo()
        );
        let origin_url = format!(
            "https://api.github.com/repos/{}/{}/releases/assets/42",
            github_source.owner(),
            github_source.repo()
        );
        let registry_http = FixtureRegistryHttp::default();
        let github_http = FixtureGithubHttp::default()
            .json(
                &release_url,
                format!(
                    r#"{{"tag_name":"v{version}","assets":[{{"name":"provider.wasm","id":42}}]}}"#
                ),
            )
            .bytes(&origin_url, bytes);

        run_init_with_fixtures(&user_dirs, project.path(), &registry_http, &github_http).await;

        assert_eq!(
            github_http
                .requested_urls()
                .iter()
                .filter(|requested| requested.as_str() == origin_url)
                .count(),
            1
        );
        assert_eq!(
            github_http
                .requested_urls()
                .iter()
                .filter(|requested| requested.as_str() == release_url)
                .count(),
            1
        );
        assert!(tree_contains_file(user_dirs.trust_record_dir()));
        assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
        assert_installed_provider_loads_as_aws(&user_dirs, project.path()).await;

        run_init_with_fixtures(&user_dirs, project.path(), &registry_http, &github_http).await;

        assert_eq!(
            github_http
                .requested_urls()
                .iter()
                .filter(|requested| requested.as_str() == origin_url)
                .count(),
            1
        );
        assert_eq!(
            github_http
                .requested_urls()
                .iter()
                .filter(|requested| requested.as_str() == release_url)
                .count(),
            1
        );
        assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    }

    #[test]
    fn init_with_up_to_date_registry_lock_records_fresh_runner_trust_once() {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(init_with_up_to_date_registry_lock_case());
    }

    async fn init_with_up_to_date_registry_lock_case() {
        let project = tempfile::tempdir().unwrap();
        let first_user_root = tempfile::tempdir().unwrap();
        let fresh_user_root = tempfile::tempdir().unwrap();
        let first_user_dirs = UserDirs::under(first_user_root.path());
        let fresh_user_dirs = first_user_dirs
            .clone()
            .with_trust_record_dir(fresh_user_root.path().join(".carina").join("trust"));
        assert_eq!(
            first_user_dirs.plugin_cache_dir(),
            fresh_user_dirs.plugin_cache_dir()
        );
        fs::write(
            project.path().join("providers.crn"),
            "provider aws {\n  source = 'carina-rs/aws'\n}\n",
        )
        .unwrap();
        let http = official_signed_fixture_registry();

        run_init_with_fixture(&first_user_dirs, project.path(), &http).await;

        let lock_path = project.path().join("carina-providers.lock");
        let lock_before = fs::read(&lock_path).unwrap();
        let downloads_before = request_count(&http, SIGNED_ARTIFACT_URL);
        assert_eq!(downloads_before, 1);
        let project_wasm = project
            .path()
            .join(".carina/providers/carina-rs/aws")
            .join(SIGNED_VERSION)
            .join("aws.wasm");
        let global_wasm = first_user_dirs
            .plugin_cache_dir()
            .join("carina-rs/aws")
            .join(SIGNED_VERSION)
            .join("aws.wasm");
        // Model a warm shared cache whose verified bytes are not yet trusted by
        // the fresh user.
        fs::create_dir_all(global_wasm.parent().unwrap()).unwrap();
        fs::copy(&project_wasm, &global_wasm).unwrap();
        assert!(
            global_wasm.is_file(),
            "warm shared global cache must contain {}",
            global_wasm.display()
        );
        assert_installed_provider_loads_as_aws(&first_user_dirs, project.path()).await;

        assert!(!tree_contains_file(fresh_user_dirs.trust_record_dir()));

        run_init_with_fixture(&fresh_user_dirs, project.path(), &http).await;

        assert_eq!(
            request_count(&http, SIGNED_ARTIFACT_URL),
            downloads_before + 1
        );
        assert!(tree_contains_file(fresh_user_dirs.trust_record_dir()));
        assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
        assert_installed_provider_loads_as_aws(&fresh_user_dirs, project.path()).await;

        run_init_with_fixture(&fresh_user_dirs, project.path(), &http).await;

        assert_eq!(
            request_count(&http, SIGNED_ARTIFACT_URL),
            downloads_before + 1
        );
        assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    }

    async fn assert_init_runs_real_verifier_inside_async_command() {
        let project = tempfile::tempdir().unwrap();
        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
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
                    &user_dirs,
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
        assert!(lock.contains("version = 5"), "{lock}");
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

    #[test]
    fn init_real_verifier_is_safe_on_current_thread_runtime() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(assert_init_runs_real_verifier_inside_async_command());
    }

    #[test]
    fn init_real_verifier_is_safe_on_multi_thread_runtime() {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(assert_init_runs_real_verifier_inside_async_command());
    }
}
