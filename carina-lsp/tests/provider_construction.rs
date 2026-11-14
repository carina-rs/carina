use std::sync::Arc;

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories, install_prober};

#[tokio::test(flavor = "multi_thread")]
async fn production_construction_and_analysis_need_no_transport() {
    let fixture = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("main.crn", "mock.test.resource { name = 'disk' }\n"),
        ("providers.crn", "provider mock {}\n"),
        ("exports.crn", "exports {}\n"),
        ("backend.crn", "backend local {}\n"),
    ] {
        std::fs::write(fixture.path().join(name), text).unwrap();
    }
    let user_dirs = UserDirs::under(&fixture.path().join("home"));
    let builder: FactoryBuilder =
        Arc::new(move |configs| build_factories(Ok(&user_dirs), configs, &Default::default()));
    let providers = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
    let reload = reload_provider_states(
        fixture.path(),
        &builder,
        Some(&install_prober()),
        &providers,
    )
    .await;
    assert!(reload.reloaded);
    assert!(reload.warnings.is_empty());
    assert_eq!(reload.directories, 1);
    assert!(reload.schemas > 0);
    let reused = reload_provider_states(
        fixture.path(),
        &builder,
        Some(&install_prober()),
        &providers,
    )
    .await;
    assert!(!reused.reloaded);
    assert!(!reused.import_map_changed);
    let path = fixture.path().join("main.crn");
    let document = Document::new_with_path(
        "mock.test.resource { name = 123 }\n".into(),
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let diagnostics = analyze_document(&*providers.read().await, &document, Some(&path));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("String")),
        "{diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_loader_receives_resolver_verified_bytes_and_retains_error_provenance() {
    use carina_lsp::factories::{
        VerifiedFactoryLoader, build_factories_with_loader, load_wasm_factory,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = tempfile::tempdir().unwrap();
    let artifact = fixture.path().join("local.wasm");
    std::fs::write(&artifact, b"resolver-owned test bytes").unwrap();
    let config = carina_core::parser::ProviderConfig {
        name: "local".into(),
        source: Some(format!("file://{}", artifact.display()).parse().unwrap()),
        attributes: Default::default(),
        default_tags: Default::default(),
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let loader: Box<VerifiedFactoryLoader> = Box::new({
        let calls = Arc::clone(&calls);
        move |verified, dirs, notices| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(verified.bytes().as_bytes(), b"resolver-owned test bytes");
            assert_eq!(verified.identity().configured_name(), "local");
            load_wasm_factory(verified, dirs, notices)
        }
    });
    let dirs = UserDirs::under(&fixture.path().join("home"));
    let (factories, errors, fingerprint) = build_factories_with_loader(
        Ok(&dirs),
        &[(fixture.path().to_path_buf(), config)],
        &Default::default(),
        loader.as_ref(),
    );
    assert!(factories.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fingerprint.len(), 1);
    assert!(
        errors["local"].contains("not controlled by carina-providers.lock"),
        "{}",
        errors["local"]
    );
}
