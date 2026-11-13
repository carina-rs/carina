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
    assert_eq!(settings.path(), Some("state.json"));
    // Both persistent representations are candidates, never saved authority.
    let saved = checked.to_parser_config();
    assert_eq!(BackendConfig::try_from(&saved).unwrap(), checked);
    let lock = crate::BackendLock::from_config(&checked);
    assert_eq!(lock.to_state_config().unwrap(), checked);

    let default = BackendConfig::implicit_local();
    assert_eq!(default.path(), None);

    let url = S3BackendSettings::from_url_parts("bucket".into(), "key".into()).unwrap();
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
    let location = LocalBackendLocation::for_file(&path).unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();
    let backend = crate::backends::LocalBackend::from_location(location).unwrap();
    backend.write_state(&crate::StateFile::new()).await.unwrap();
    assert!(first.join("state.json").exists());
    assert!(!second.join("state.json").exists());
    assert_eq!(backend.state_path().to_string(), path.display().to_string());
}

#[cfg(unix)]
#[test]
fn backend_local_native_address_pin_preserves_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(std::ffi::OsString::from_vec(b"state-\xff.json".to_vec()));
    let location = LocalBackendLocation::for_file(&path).unwrap();
    assert_eq!(
        location.path().identity(),
        ResolvedPath::new(&path).unwrap().identity()
    );
    assert_ne!(
        location.path().identity(),
        ResolvedPath::new(path.to_string_lossy().as_ref())
            .unwrap()
            .identity()
    );
    let backend = crate::backends::LocalBackend::from_location(location).unwrap();
    assert_eq!(
        backend.state_path(),
        &carina_core::paths::DisplayPath::new(&path).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn review_r1_native_file_origin_is_explicit_and_not_admitted_settings() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(std::ffi::OsString::from_vec(b"native-\xff.json".to_vec()));
    let location = LocalBackendLocation::for_file(&path).unwrap();
    assert!(matches!(location, LocalBackendLocation::NativeFile(_)));
    assert_eq!(
        location.path().identity(),
        ResolvedPath::new(&path).unwrap().identity()
    );
    let settings = BackendConfig::implicit_local();
    assert!(matches!(
        settings
            .pin(&ResolvedPath::new(root.path()).unwrap())
            .unwrap(),
        LocalBackendLocation::Configured(_)
    ));
}

#[test]
fn review_r1_locate_error_keeps_the_io_source() {
    let error: LocalStateLocateError =
        LocalBackendLocation::for_file(std::path::Path::new("bad\0state")).unwrap_err();
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn review_r1_configured_locate_error_matches_main_text() {
    let root = tempfile::tempdir().unwrap();
    let raw = parser::BackendConfig::<NoPlaceholder> {
        backend_type: "local".into(),
        attributes: [(
            "path".to_owned(),
            Value::Concrete(ConcreteValue::String("bad\0state".into())),
        )]
        .into_iter()
        .collect(),
        attribute_spans: Default::default(),
    };
    let BackendConfig::Local(settings) = BackendConfig::try_from(&raw).unwrap() else {
        panic!("local")
    };
    let base = ResolvedPath::new(root.path()).unwrap();
    let io = crate::backends::anchored_local_path(&settings, &base).unwrap_err();
    let error = crate::backends::LocalBackend::from_config(&settings, &base)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!("I/O error: Failed to locate state file: {io}")
    );
}
