//! Production offline provider resolution and factory construction.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::parser::ProviderConfig;
use carina_core::provider::ProviderFactory;
use carina_core::provider_source::ProviderSource;

use crate::backend::FactoryBuildResult;

#[derive(Debug, PartialEq, Eq)]
enum InstallRejection {
    MissingSource,
    ResolutionFailed { message: String },
}

impl std::fmt::Display for InstallRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallRejection::MissingSource => f.write_str(
                "no source configured. Add `source = 'carina-rs/aws'` for a registry provider, or `source = 'github.com/owner/repo'` for a direct GitHub source.",
            ),
            InstallRejection::ResolutionFailed { message } => f.write_str(message),
        }
    }
}

/// Resolve the on-disk artifact the LSP would load for `config`, or retain the
/// exact reason it rejected the source. The rejection enum prevents the load
/// path from having to re-run filesystem resolution to reconstruct a reason.
///
/// Used by `build_factories` at load time. The drift prober uses the resolver's
/// matching path selection and cheap file metadata, so its two-second poll
/// never hashes provider WASM bytes.
fn resolve_install(
    source_dir: &carina_core::paths::ResolvedPath,
    config: &ProviderConfig,
) -> Result<carina_provider_resolver::InstalledProvider, InstallRejection> {
    let source = config
        .source
        .as_ref()
        .ok_or(InstallRejection::MissingSource)?;
    let installed = match source {
        ProviderSource::File(_) => {
            carina_provider_resolver::find_file_provider_source(source_dir, config, &mut |notice| {
                eprintln!("{notice}")
            })
        }
        ProviderSource::Remote(_) => {
            carina_provider_resolver::find_installed_provider(source_dir, config, &mut |notice| {
                eprintln!("{notice}")
            })
            .map_err(|error| error.to_string())
        }
    }
    .map_err(|error| InstallRejection::ResolutionFailed {
        message: error.to_string(),
    })?;
    // Both resolver functions only construct `InstalledProvider` after their
    // selected path exists, so a second `.exists()` check here was unreachable.
    Ok(installed)
}

pub fn install_prober() -> crate::backend::ProviderInstallProber {
    std::sync::Arc::new(|path, config| {
        carina_core::paths::ResolvedPath::new(path)
            .ok()
            .map(|root| {
                carina_provider_resolver::provider_artifact_fingerprint(
                    &root,
                    config,
                    &mut |notice| eprintln!("{notice}"),
                )
            })
            .unwrap_or_else(|| Ok(Default::default()))
    })
}

/// Build provider factories from discovered provider configs.
/// Each entry is (source_directory, provider_config) so providers are installed
/// in the directory containing the `.crn` file, not at the workspace root.
pub fn build_factories(
    user_dirs: Result<&carina_core::UserDirs, &str>,
    providers: &[(PathBuf, ProviderConfig)],
    grants: &carina_provider_resolver::TrustedLocalGrants,
) -> FactoryBuildResult {
    build_factories_with_loader(user_dirs, providers, grants, &load_wasm_factory)
}

/// The sole construction seam: resolution, verification, trust and provenance
/// remain in `build_factories_with_loader`, before the loader sees any bytes.
pub type VerifiedFactoryLoader = dyn for<'a> Fn(
        carina_provider_resolver::VerifiedLoad,
        &'a carina_core::UserDirs,
        &'a mut carina_plugin_host::WasmLoadNoticeSink,
    ) -> carina_core::provider::BoxFuture<
        'a,
        Result<Box<dyn ProviderFactory>, carina_plugin_host::WasmProviderLoadError>,
    > + Send
    + Sync;

pub fn load_wasm_factory<'a>(
    verified: carina_provider_resolver::VerifiedLoad,
    user_dirs: &'a carina_core::UserDirs,
    notices: &'a mut carina_plugin_host::WasmLoadNoticeSink,
) -> carina_core::provider::BoxFuture<
    'a,
    Result<Box<dyn ProviderFactory>, carina_plugin_host::WasmProviderLoadError>,
> {
    Box::pin(async move {
        carina_plugin_host::WasmProviderFactory::from_bytes(verified, user_dirs, notices)
            .await
            .map(|factory| Box::new(factory) as Box<dyn ProviderFactory>)
    })
}

fn wasm_notice_sink() -> carina_plugin_host::WasmLoadNoticeSink {
    carina_plugin_host::WasmLoadNoticeSink::new(move |notice| {
        if matches!(
            notice,
            carina_plugin_host::WasmLoadNotice::ValidationRebuilt(_)
        ) {
            log::warn!(target: "carina_plugin_host::wasm_factory", "{notice}");
        } else {
            eprintln!("{notice}");
        }
    })
}

pub fn build_factories_with_loader(
    user_dirs: Result<&carina_core::UserDirs, &str>,
    providers: &[(PathBuf, ProviderConfig)],
    grants: &carina_provider_resolver::TrustedLocalGrants,
    loader: &VerifiedFactoryLoader,
) -> FactoryBuildResult {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut fingerprint: Vec<(String, crate::backend::ProviderInstallFingerprint)> =
        Vec::with_capacity(providers.len());

    for (source_dir, config) in providers {
        let project = match carina_core::paths::ResolvedPath::new(source_dir) {
            Ok(project) => project,
            Err(error) => {
                errors.insert(config.name.clone(), error.to_string());
                continue;
            }
        };
        fingerprint.push((
            config.name.clone(),
            carina_provider_resolver::provider_artifact_fingerprint(
                &project,
                config,
                &mut |notice| eprintln!("{notice}"),
            ),
        ));
        let installed = match resolve_install(&project, config) {
            Ok(installed) => installed,
            Err(rejection) => {
                // Named instances inherit the kind default's source. Their
                // missing source is deliberate. The source-less default mock
                // is also deliberate: it selects Carina's built-in provider.
                // Every other rejection is a diagnostic produced directly
                // from this one resolution.
                let uses_builtin_mock =
                    carina_provider_mock::uses_builtin_provider(std::iter::once(config), |_| false);
                if !uses_builtin_mock
                    && (config.is_default() || rejection != InstallRejection::MissingSource)
                {
                    errors.insert(config.name.clone(), rejection.to_string());
                }
                continue;
            }
        };
        let user_dirs = match user_dirs {
            Ok(user_dirs) => user_dirs,
            Err(error) => {
                errors.insert(
                    config.name.clone(),
                    format!(
                        "Could not resolve user directories required to load provider '{}': {error}",
                        config.name
                    ),
                );
                continue;
            }
        };

        let mut notices = wasm_notice_sink();
        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(installed.load_with(
                user_dirs,
                grants,
                &mut |notice| eprintln!("{notice}"),
                |verified| loader(verified, user_dirs, &mut notices),
            ))
        }) {
            Ok(factory) => {
                log::info!(
                    target: "carina_lsp",
                    "LSP: loaded provider '{}' from {}",
                    config.name,
                    installed.path().display()
                );
                factories.push(factory);
            }
            Err(error) => {
                // Final LSP-diagnostic boundary. The structured host chain is
                // rendered first and resolver provenance follows it.
                errors.insert(config.name.clone(), error.to_string());
            }
        }
    }

    (factories, errors, fingerprint)
}

pub fn trusted_local_grants_from_env(
    env_value: Option<&std::ffi::OsStr>,
    report_error: impl FnOnce(&carina_provider_resolver::GrantParseError),
) -> carina_provider_resolver::TrustedLocalGrants {
    match carina_provider_resolver::TrustedLocalGrants::parse_os(&[], env_value) {
        Ok(grants) => grants,
        Err(error) => {
            report_error(&error);
            carina_provider_resolver::TrustedLocalGrants::parse_os(&[], None)
                .expect("an empty trusted-local grant set is valid")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::ProviderConfig;
    use carina_provider_resolver::ProviderArtifactFingerprint;

    use std::path::PathBuf;

    fn no_local_grants() -> carina_provider_resolver::TrustedLocalGrants {
        carina_provider_resolver::TrustedLocalGrants::parse(&[], None).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_trusted_local_env_is_reported_and_ignored() {
        use std::os::unix::ffi::OsStringExt;

        let value = std::ffi::OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        let mut reported = None;
        let grants = trusted_local_grants_from_env(Some(&value), |error| {
            reported = Some(error.to_string());
        });

        assert_eq!(grants, no_local_grants());
        assert!(
            reported
                .as_deref()
                .is_some_and(|message| message.contains("must be valid UTF-8")),
            "the non-UTF-8 parse error must reach the LSP log boundary: {reported:?}"
        );
    }

    fn cfg(name: &str, source: Option<&str>, binding: Option<&str>) -> (PathBuf, ProviderConfig) {
        (
            PathBuf::from("/tmp"),
            ProviderConfig {
                name: name.to_string(),
                attributes: Default::default(),
                default_tags: Default::default(),
                source: source.map(|source| source.parse().unwrap()),
                version: None,
                revision: None,
                unresolved_attributes: Default::default(),
                binding: binding.map(String::from),
            },
        )
    }

    /// carina#3023: when a named provider instance sits beside the
    /// kind default, `build_factories` must produce a fingerprint
    /// entry for *every* config — same length as the input — so the
    /// LSP's drift-poll comparison against `probe_install_fingerprint`
    /// (which iterates every config unconditionally) keeps agreeing
    /// when nothing changed. Previously the fingerprint push was
    /// gated behind the missing-source error path; when we silenced
    /// that error for named instances, the push got silenced with
    /// it and the poll detected fake drift every tick.
    #[test]
    fn build_factories_fingerprint_length_matches_configs_length() {
        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = carina_core::UserDirs::under(user_root.path());
        let providers = vec![
            cfg("aws", Some("file:///nonexistent/fake.wasm"), None),
            cfg("aws", None, Some("us")),
        ];
        let (_factories, errors, fingerprint) =
            build_factories(Ok(&user_dirs), &providers, &no_local_grants());
        assert_eq!(
            fingerprint.len(),
            providers.len(),
            "fingerprint must emit one entry per config (including named instances); \
             otherwise the drift-poll mismatch causes a perpetual rebuild loop. carina#3023."
        );
        // Sanity: named instance does not surface the kind-level
        // "no source configured" error, since the parser forbids it
        // from setting `source` in the first place.
        assert!(
            !errors
                .get("aws")
                .is_some_and(|m| m.contains("no source configured")),
            "named instance must not trigger the kind-level missing-source diagnostic. \
             carina#3023. errors: {:?}",
            errors
        );
    }

    #[test]
    fn source_less_mock_uses_builtin_provider_without_load_error() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dirs = carina_core::UserDirs::under(&tmp.path().join("user"));
        let (_, config) = cfg("mock", None, None);
        let providers = vec![(tmp.path().to_path_buf(), config)];

        let (factories, errors, fingerprint) =
            build_factories(Ok(&user_dirs), &providers, &no_local_grants());

        assert!(factories.is_empty());
        assert!(
            !errors.contains_key("mock"),
            "source-less mock is handled by the built-in provider, not reported as a failed plugin load: {errors:?}",
        );
        assert_eq!(
            fingerprint,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )]
        );
    }

    #[test]
    fn user_dirs_resolution_failure_is_reported_as_a_provider_load_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("local-provider.wasm");
        std::fs::write(&wasm, b"fixture").unwrap();
        let providers = vec![cfg(
            "local",
            Some(&format!("file://{}", wasm.display())),
            None,
        )];
        let guard_error = "nextest home isolation guard rejected user directories";

        let (factories, errors, fingerprint) =
            build_factories(Err(guard_error), &providers, &no_local_grants());

        assert!(factories.is_empty());
        assert_eq!(fingerprint.len(), 1);
        assert_eq!(
            errors.get("local").map(String::as_str),
            Some(
                "Could not resolve user directories required to load provider 'local': nextest home isolation guard rejected user directories"
            )
        );
    }

    #[test]
    fn missing_source_hint_advertises_registry_and_github_forms() {
        let rendered = InstallRejection::MissingSource.to_string();

        assert_eq!(
            rendered,
            "no source configured. Add `source = 'carina-rs/aws'` for a registry provider, or `source = 'github.com/owner/repo'` for a direct GitHub source."
        );
    }

    #[test]
    fn direct_file_install_retains_non_lock_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("local-provider.wasm");
        std::fs::write(&wasm, b"fixture").unwrap();
        let providers = cfg("local", Some(&format!("file://{}", wasm.display())), None);

        let installed = resolve_install(
            &carina_core::paths::ResolvedPath::new(&providers.0).unwrap(),
            &providers.1,
        )
        .expect("direct file source should be observed by the LSP");
        assert_eq!(installed.path().as_path(), wasm);
        let rendered = installed.with_load_error("load failed").to_string();
        assert!(rendered.contains(&format!("Artifact: {}", wasm.display())));
        assert!(rendered.contains("provider resolved from file://"));
        assert!(rendered.contains("not controlled by carina-providers.lock"));
        assert!(!rendered.contains("carina init --upgrade"));
    }

    #[test]
    fn registry_source_resolves_installed_lock_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "carina-rs/awscc";
        let version = "0.5.0";
        let parsed_source: ProviderSource = source.parse().unwrap();
        let ProviderSource::Remote(remote_source) = &parsed_source else {
            panic!("registry source must parse as remote")
        };
        let wasm = carina_provider_resolver::cache_path_wasm(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &remote_source.source_key(),
            version,
        );
        std::fs::create_dir_all(wasm.parent().unwrap()).unwrap();
        std::fs::write(&wasm, b"fixture").unwrap();

        let mut lock = carina_provider_resolver::LockFile::default();
        lock.upsert(carina_provider_resolver::LockEntry {
            name: "awscc".into(),
            source: source.into(),
            kind: carina_provider_resolver::LockEntryKind::Version {
                version: version.into(),
                constraint: None,
            },
            sha256: carina_provider_resolver::sha256_file(
                &carina_core::paths::ResolvedPath::new(&wasm).unwrap(),
            )
            .unwrap(),
            registry: None,
        });
        lock.save(&tmp.path().join("carina-providers.lock"))
            .unwrap();

        let (_, config) = cfg("awscc", Some(source), None);
        let installed = resolve_install(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &config,
        )
        .expect("registry source should resolve through the shared provider source type");

        assert_eq!(installed.path().as_path(), wasm.as_path());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unverified_official_provider_is_reported_before_wasm_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tempfile::tempdir().unwrap();
        let user_dirs = carina_core::UserDirs::under(user_home.path());
        let source = "github.com/carina-rs/carina-provider-aws";
        let version = "0.5.0";
        let parsed_source: ProviderSource = source.parse().unwrap();
        let ProviderSource::Remote(remote_source) = &parsed_source else {
            panic!("GitHub source must parse as remote")
        };
        let wasm = carina_provider_resolver::cache_path_wasm(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &remote_source.source_key(),
            version,
        );
        std::fs::create_dir_all(wasm.parent().unwrap()).unwrap();
        std::fs::write(&wasm, b"not a wasm component").unwrap();
        let mut lock = carina_provider_resolver::LockFile::default();
        lock.upsert(carina_provider_resolver::LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: carina_provider_resolver::LockEntryKind::GithubRelease {
                version: version.parse().unwrap(),
                tag: serde_json::from_str(&format!("\"v{version}\"")).unwrap(),
                asset_id: serde_json::from_str("42").unwrap(),
                constraint: None,
            },
            sha256: carina_provider_resolver::sha256_file(
                &carina_core::paths::ResolvedPath::new(&wasm).unwrap(),
            )
            .unwrap(),
            registry: None,
        });
        lock.save(&tmp.path().join("carina-providers.lock"))
            .unwrap();
        let (_, config) = cfg("aws", Some(source), None);

        let (factories, errors, _) = build_factories(
            Ok(&user_dirs),
            &[(tmp.path().to_path_buf(), config)],
            &no_local_grants(),
        );

        assert!(factories.is_empty());
        let message = errors.get("aws").expect("LSP load diagnostic");
        assert!(
            message.starts_with(&format!(
                "{source} is a carina-rs provider but this user hasn't verified it; run `carina init {}`",
                tmp.path().display()
            )),
            "{message}"
        );
        assert!(
            !message.contains("Failed to load WASM component"),
            "{message}"
        );
    }

    #[test]
    fn mismatched_present_artifact_has_stable_fingerprint_and_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let source = "github.com/carina-rs/carina-provider-aws";
        let version = "0.5.0";
        let parsed_source: ProviderSource = source.parse().unwrap();
        let ProviderSource::Remote(remote_source) = &parsed_source else {
            panic!("GitHub source must parse as remote")
        };
        let wasm = carina_provider_resolver::cache_path_wasm(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &remote_source.source_key(),
            version,
        );
        std::fs::create_dir_all(wasm.parent().unwrap()).unwrap();
        std::fs::write(&wasm, b"mismatched provider bytes").unwrap();

        let mut lock = carina_provider_resolver::LockFile::default();
        lock.upsert(carina_provider_resolver::LockEntry {
            name: "aws".into(),
            source: source.into(),
            kind: carina_provider_resolver::LockEntryKind::GithubRelease {
                version: version.parse().unwrap(),
                tag: serde_json::from_str("\"v0.5.0\"").unwrap(),
                asset_id: serde_json::from_str("42").unwrap(),
                constraint: None,
            },
            sha256: "pinned-sha256".into(),
            registry: None,
        });
        lock.save(&tmp.path().join("carina-providers.lock"))
            .unwrap();

        let (_, config) = cfg("aws", Some(source), None);
        let providers = vec![(tmp.path().to_path_buf(), config.clone())];
        let user_dirs = carina_core::UserDirs::under(&tmp.path().join("user"));
        let (_factories, errors, fingerprint) =
            build_factories(Ok(&user_dirs), &providers, &no_local_grants());
        let probed = install_prober()(tmp.path(), &config);

        assert!(
            probed.as_ref().unwrap().artifact.is_some(),
            "the drift probe observes artifact metadata without hashing"
        );
        assert!(
            probed.as_ref().unwrap().lock_file.is_some(),
            "the drift probe observes lock-file metadata"
        );
        assert_eq!(fingerprint, vec![("aws".to_string(), probed)]);
        assert!(
            errors
                .get("aws")
                .is_some_and(|message| message.contains("installed artifact github.com/carina-rs/carina-provider-aws@0.5.0 does not match carina-providers.lock")),
            "the load diagnostic must retain the checksum mismatch: {errors:?}",
        );
    }

    #[test]
    fn install_fingerprint_changes_when_artifact_is_replaced_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("local-provider.wasm");
        std::fs::write(&wasm, b"first").unwrap();
        let (_, config) = cfg("local", Some(&format!("file://{}", wasm.display())), None);
        let prober = install_prober();

        let before = prober(tmp.path(), &config).unwrap();
        std::fs::write(&wasm, b"replacement with a different length").unwrap();
        let after = prober(tmp.path(), &config).unwrap();

        assert!(before.artifact.is_some());
        assert!(after.artifact.is_some());
        assert_ne!(before, after);

        #[cfg(unix)]
        {
            let before_rename = prober(tmp.path(), &config).unwrap();
            let before_artifact = before_rename.artifact.as_ref().unwrap();
            let replacement = tmp.path().join("local-provider-replacement.wasm");
            std::fs::write(
                &replacement,
                vec![b'x'; usize::try_from(before_artifact.len).unwrap()],
            )
            .unwrap();
            let modified = before_artifact.modified.unwrap();
            std::fs::File::options()
                .write(true)
                .open(&replacement)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            std::fs::rename(&replacement, &wasm).unwrap();

            let after_rename = prober(tmp.path(), &config).unwrap();
            let after_artifact = after_rename.artifact.as_ref().unwrap();
            assert_eq!(before_artifact.len, after_artifact.len);
            assert_eq!(before_artifact.modified, after_artifact.modified);
            assert_eq!(before_artifact.dev, after_artifact.dev);
            assert_ne!(before_artifact.ino, after_artifact.ino);
            assert_ne!(before_rename, after_rename);
        }
    }
}

#[cfg(test)]
mod load_logging_tests {
    use super::*;
    use std::sync::Mutex;

    struct Capture(Mutex<Vec<(String, String)>>);

    impl log::Log for Capture {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            self.0
                .lock()
                .unwrap()
                .push((record.target().into(), record.args().to_string()));
        }
        fn flush(&self) {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn library_cut_retains_the_binary_factory_log_target() {
        let capture = Box::leak(Box::new(Capture(Mutex::new(Vec::new()))));
        log::set_logger(capture).unwrap();
        log::set_max_level(log::LevelFilter::Info);
        let fixture = tempfile::tempdir().unwrap();
        let artifact = fixture.path().join("mock.wasm");
        std::fs::write(&artifact, b"resolver-verified fixture bytes").unwrap();
        let config = ProviderConfig {
            name: "mock".into(),
            source: Some(format!("file://{}", artifact.display()).parse().unwrap()),
            attributes: Default::default(),
            default_tags: Default::default(),
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let dirs = carina_core::UserDirs::under(&fixture.path().join("home"));
        let loader: Box<VerifiedFactoryLoader> = Box::new(|_, _, _| {
            Box::pin(async {
                Ok(Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
                    Default::default(),
                    carina_provider_mock::MockScope::memory(),
                )) as Box<dyn ProviderFactory>)
            })
        });
        let (factories, errors, _) = build_factories_with_loader(
            Ok(&dirs),
            &[(fixture.path().to_path_buf(), config)],
            &Default::default(),
            loader.as_ref(),
        );
        assert_eq!(factories.len(), 1);
        assert!(errors.is_empty());
        let records = capture.0.lock().unwrap();
        let loaded = records
            .iter()
            .filter(|(_, message)| message.starts_with("LSP: loaded provider '"))
            .collect::<Vec<_>>();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].0, "carina_lsp",
            "moving construction into the library must preserve log filtering"
        );
    }
}
