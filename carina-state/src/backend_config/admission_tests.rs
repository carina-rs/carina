use super::*;
use carina_core::validation::admission::configuration::backend;

#[test]
fn backend_constructor_all_origins_require_admitted_settings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("backend.crn"),
        "backend local { path = state_file }",
    )
    .unwrap();
    std::fs::write(
        root.path().join("values.crn"),
        "let state_file = 'state.json'",
    )
    .unwrap();
    let loaded = carina_core::config_loader::load_configuration(
        &carina_core::paths::ResolvedPath::new(root.path()).unwrap(),
        &parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let admitted = backend::admit(loaded.parsed.backend.as_ref().unwrap()).unwrap();
    let checked = BackendConfig::from(admitted);
    let BackendConfig::Local(settings) = &checked else {
        panic!("local settings")
    };
    assert_eq!(
        settings.configuration().backend_kind(),
        backend::BackendKind::Local
    );
    assert_eq!(settings.path(), Some("state.json"));
    // Both persistent representations are candidates, never saved authority.
    let saved = checked.to_parser_config();
    assert_eq!(BackendConfig::try_from(&saved).unwrap(), checked);
    let lock = crate::BackendLock::from_config(&checked);
    assert_eq!(lock.to_state_config().unwrap(), checked);

    let default = BackendConfig::implicit_local();
    assert!(default.configuration().attributes().is_empty());
    assert_eq!(default.path(), None);

    let url = S3BackendSettings::from_url_parts("bucket".into(), "key".into()).unwrap();
    assert_eq!(url.configuration().backend_kind(), backend::BackendKind::S3);
    assert!(url.encrypt());
    assert!(!url.auto_create());
    assert_eq!(url.region(), None, "URL region stays with the SDK chain");
    assert_eq!(url.bucket(), "bucket");
    assert_eq!(url.key(), "key");
}

#[cfg(unix)]
#[tokio::test]
async fn backend_local_location_pins_the_admitted_path_once() {
    use crate::StateBackend;
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let alias = root.path().join("alias");
    symlink(&first, &alias).unwrap();
    let path = alias.join("state.json");
    let location = LocalBackendSettings::for_file(&path).unwrap();
    assert_eq!(location.settings().path(), path.to_str());
    std::fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();
    let backend = crate::backends::LocalBackend::from_location(location).unwrap();
    backend.write_state(&crate::StateFile::new()).await.unwrap();
    assert!(first.join("state.json").exists());
    assert!(!second.join("state.json").exists());
    assert_eq!(backend.state_path().to_string(), path.display().to_string());
}
