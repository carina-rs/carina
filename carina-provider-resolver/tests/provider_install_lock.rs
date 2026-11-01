use std::fs;
use std::sync::{Arc, Barrier};

use carina_core::{UserDirs, parser::ProviderConfig};
use carina_provider_resolver::{LockFile, LockMode, ProviderInstallLock, resolve_all};
use indexmap::IndexMap;

fn file_provider(name: &str, path: &std::path::Path) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        source: Some(format!("file://{}", path.display()).parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    }
}

#[test]
fn acquiring_for_nonexistent_project_creates_nothing() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("missing");

    let error = match ProviderInstallLock::acquire(
        &carina_core::paths::ResolvedPath::new(&project).unwrap(),
    ) {
        Ok(_) => panic!("acquiring a lock for a nonexistent project must fail"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(error.to_string().contains(&project.display().to_string()));
    assert!(!project.exists());
}

#[test]
fn resolving_unchanged_file_provider_twice_leaves_only_final_artifact() {
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("provider.wasm");
    fs::write(&source, b"provider wasm").unwrap();
    let provider = file_provider("test", &source);
    let install_lock = ProviderInstallLock::acquire(
        &carina_core::paths::ResolvedPath::new(project.path()).unwrap(),
    )
    .unwrap();
    let user_dirs = UserDirs::under(&project.path().join("user"));

    let first = resolve_all(
        &user_dirs,
        &install_lock,
        std::slice::from_ref(&provider),
        LockMode::Normal,
    )
    .unwrap();
    let destination = first["test"].as_path().to_path_buf();
    resolve_all(&user_dirs, &install_lock, &[provider], LockMode::Normal).unwrap();

    let entries = fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![destination]);
}

#[test]
fn concurrent_resolutions_preserve_both_lock_entries() {
    let project = tempfile::tempdir().unwrap();
    let source_a = project.path().join("a.wasm");
    let source_b = project.path().join("b.wasm");
    fs::write(&source_a, vec![b'a'; 4 * 1024 * 1024]).unwrap();
    fs::write(&source_b, vec![b'b'; 4 * 1024 * 1024]).unwrap();
    let provider_a = file_provider("a", &source_a);
    let provider_b = file_provider("b", &source_b);
    let start = Arc::new(Barrier::new(3));

    let threads = [provider_a, provider_b].map(|provider| {
        let project_dir = project.path().to_path_buf();
        let user_dirs = UserDirs::under(&project.path().join("user"));
        let start = Arc::clone(&start);
        std::thread::spawn(move || {
            start.wait();
            let install_lock = ProviderInstallLock::acquire(
                &carina_core::paths::ResolvedPath::new(&project_dir).unwrap(),
            )
            .unwrap();
            resolve_all(&user_dirs, &install_lock, &[provider], LockMode::Normal).unwrap();
        })
    });
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }

    let loaded = LockFile::load(&carina_core::paths::ResolvedPath::new(project.path()).unwrap())
        .unwrap()
        .unwrap();
    let (lock_file, _) = loaded.into_parts();
    let mut names = lock_file
        .provider
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, vec!["a", "b"]);
}
