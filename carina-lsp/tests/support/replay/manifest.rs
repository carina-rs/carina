use super::artifacts;
use carina_core::{
    EnvironmentSnapshot,
    parser::ProviderContext,
    paths::{CanonicalPath, PathIdentity, PersistedPath},
};
use carina_lsp::workspace::DiscoveryScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub root: PathBuf,
    pub corpus: String,
    pub documents: Vec<PathBuf>,
    pub inputs: BTreeMap<PathBuf, String>,
    pub exclusions: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub replay_home: PathBuf,
    pub artifacts: Option<PathBuf>,
    pub revision: String,
    #[serde(default)]
    pub shard: Option<(usize, usize)>,
    #[serde(default)]
    pub inventory: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub dependency_roots: Vec<PathBuf>,
}

pub fn existing_locator(path: &Path) -> PathBuf {
    CanonicalPath::new(path).expect("existing replay input");
    PersistedPath::new(path).unwrap().to_path_buf()
}

fn identities<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> BTreeSet<CanonicalPath> {
    paths
        .into_iter()
        .map(|path| CanonicalPath::new(path).expect("existing replay input"))
        .collect()
}

fn unique_locators(paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut by_identity = BTreeMap::new();
    for path in paths {
        by_identity
            .entry(CanonicalPath::new(&path).expect("existing replay input"))
            .or_insert_with(|| PersistedPath::new(path).unwrap().to_path_buf());
    }
    by_identity.into_values().collect()
}

pub fn sha256(path: &Path) -> String {
    // Stream large provider artifacts rather than retaining a second byte copy.
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("input {}: {error}", path.display()));
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).unwrap();
    format!("{:x}", hash.finalize())
}

impl Manifest {
    pub fn selected_documents(&self) -> Vec<&PathBuf> {
        let Some((index, count)) = self.shard else {
            return self.documents.iter().collect();
        };
        assert!(count > 0 && index < count);
        let directories = self
            .documents
            .iter()
            .map(|path| path.parent().unwrap())
            .collect::<BTreeSet<_>>();
        let owned = directories
            .into_iter()
            .enumerate()
            .filter_map(|(ordinal, directory)| (ordinal % count == index).then_some(directory))
            .collect::<BTreeSet<_>>();
        self.documents
            .iter()
            .filter(|path| owned.contains(path.parent().unwrap()))
            .collect()
    }

    pub fn environment(&self) -> EnvironmentSnapshot {
        EnvironmentSnapshot::new(
            self.environment
                .iter()
                .map(|(key, value)| (key.clone(), value.into())),
        )
    }

    pub fn scope(&self) -> DiscoveryScope {
        DiscoveryScope::replay(
            &self.root,
            self.exclusions.clone(),
            &self
                .documents
                .iter()
                .map(|path| self.root.join(path))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    pub fn freeze(output: &Path) -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = existing_locator(
            &std::env::var_os("CARINA_LSP_REPLAY_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| repo.to_path_buf()),
        );
        let infra = PathIdentity::new(&root) != PathIdentity::new(repo);
        let artifacts = std::env::var_os("CARINA_LSP_REPLAY_PROVIDERS")
            .map(|path| existing_locator(Path::new(&path)));
        assert!(
            !infra || artifacts.is_some(),
            "infra requires CARINA_LSP_REPLAY_PROVIDERS"
        );
        let replay_home = std::env::var_os("CARINA_LSP_REPLAY_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    std::env::var_os("CARINA_TEST_HOME")
                        .expect("run under nextest or supply CARINA_LSP_REPLAY_HOME"),
                )
                .join("lsp-replay-home")
            });
        std::fs::create_dir_all(replay_home.join("tmp")).unwrap();
        let cache = carina_core::UserDirs::under(&replay_home)
            .compiled_wasm_cache_dir()
            .to_path_buf();
        std::fs::create_dir_all(&cache).unwrap();
        let probe =
            tempfile::NamedTempFile::new_in(&cache).expect("compiled cache must be writable");
        probe.as_file().sync_all().unwrap();
        let documents = if infra {
            infra_documents(&root)
        } else {
            tracked_documents(&root)
        };
        assert!(!documents.is_empty(), "corpus has no documents");
        let mut manifest = Self {
            format: "carina-lsp-replay-input-v1".into(),
            root,
            corpus: if infra { "infra" } else { "repo" }.into(),
            documents,
            inputs: BTreeMap::new(),
            exclusions: vec![output.to_path_buf(), replay_home.clone()],
            environment: BTreeMap::new(),
            replay_home,
            artifacts,
            shard: None,
            inventory: None,
            dependency_roots: Vec::new(),
            revision: String::from_utf8(
                Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(repo)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .into(),
        };
        if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
            let target = PathBuf::from(target);
            manifest.exclusions.push(if target.is_absolute() {
                target
            } else {
                repo.join(target)
            });
        }
        // Trust records are explicit inputs, copied into the isolated replay home.
        if let Some(trust) = std::env::var_os("CARINA_LSP_REPLAY_TRUST") {
            copy_trust(
                Path::new(&trust),
                &manifest.replay_home.join(".carina/trust"),
            );
        }
        manifest.freeze_inputs();
        let inputs = identities(manifest.inputs.keys());
        for document in &manifest.documents {
            assert!(
                inputs.contains(
                    &CanonicalPath::new(manifest.root.join(document))
                        .expect("missing tracked document")
                ),
                "missing tracked input {}",
                document.display()
            );
        }
        manifest
    }

    pub fn freeze_inputs(&mut self) {
        self.inputs = self.disk_inputs();
        self.dependency_roots = self
            .inputs
            .keys()
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "crn")
                    && !CanonicalPath::new(path)
                        .unwrap()
                        .ancestors()
                        .any(|identity| identity == CanonicalPath::new(&self.root).unwrap())
            })
            .map(|path| path.parent().unwrap().to_path_buf())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.inventory = Some(self.inventory());
    }

    pub fn verify(&self) {
        if let Some(inventory) = &self.inventory {
            assert_eq!(
                identities(inventory),
                identities(&self.inventory()),
                "disk replay input inventory changed during sweep"
            );
            for (path, expected) in &self.inputs {
                assert_eq!(
                    *expected,
                    sha256(path),
                    "disk replay input changed: {}",
                    path.display()
                );
            }
        } else {
            assert_eq!(
                self.inputs
                    .iter()
                    .map(|(path, hash)| (CanonicalPath::new(path).unwrap(), hash))
                    .collect::<BTreeMap<_, _>>(),
                self.disk_inputs()
                    .iter()
                    .map(|(path, hash)| (CanonicalPath::new(path).unwrap(), hash))
                    .collect::<BTreeMap<_, _>>(),
                "disk replay inputs changed during sweep"
            );
        }
    }

    fn inventory(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let scope = self.scope();
        for root in std::iter::once(&self.root)
            .chain(&self.dependency_roots)
            .chain(self.artifacts.iter())
        {
            inventory(root, &scope, &mut visited, &mut paths);
        }
        plain_files(&self.replay_home.join(".carina/trust"), &mut paths);
        unique_locators(paths)
    }

    fn disk_inputs(&self) -> BTreeMap<PathBuf, String> {
        let scope = self.scope();
        let mut paths = BTreeSet::new();
        let mut visited = BTreeSet::new();
        scan(
            &self.root,
            &scope,
            &self.environment(),
            &mut visited,
            &mut paths,
        );
        let context = ProviderContext::new(self.environment());
        let mut artifacts = BTreeSet::new();
        for path in paths
            .iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "crn"))
        {
            let text = std::fs::read_to_string(path).unwrap();
            if let Ok(parsed) = carina_core::parser::parse_file(&text, path, &context) {
                let configs = parsed
                    .providers
                    .iter()
                    .map(|config| {
                        (
                            path.parent().unwrap().to_path_buf(),
                            config.loading_projection(),
                        )
                    })
                    .collect::<Vec<_>>();
                let configs =
                    artifacts::project(&configs, &self.root, self.artifacts.as_deref()).unwrap();
                for (directory, config) in configs {
                    let lock = directory.join("carina-providers.lock");
                    if lock.is_file() {
                        artifacts.insert(lock);
                    }
                    match artifacts::installed(&directory, &config) {
                        Ok(Some(installed)) => {
                            artifacts.insert(installed.path().as_path().to_path_buf());
                        }
                        Ok(None) => {}
                        // Loading rejection belongs to the shared production
                        // builder. Replay writes its diagnostics/coverage before
                        // failing the infra guard; hashing never replaces it.
                        Err(_) => {}
                    }
                }
            }
        }
        paths.extend(artifacts);
        let trust = self.replay_home.join(".carina/trust");
        plain_files(&trust, &mut paths);
        unique_locators(paths)
            .into_iter()
            .map(|path| {
                let hash = sha256(&path);
                (path, hash)
            })
            .collect()
    }
}

pub fn tracked_documents(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--", "*.crn"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git ls-files failed");
    let mut documents = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(std::str::from_utf8(path).expect("UTF-8 tracked document path")))
        .collect::<Vec<_>>();
    documents.sort();
    documents.dedup();
    documents
}

fn directories(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut paths = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn infra_documents(root: &Path) -> Vec<PathBuf> {
    let mut configs = directories(&root.join("aws/management"));
    for environment in directories(&root.join("envs/registry")) {
        configs.extend(directories(&environment));
    }
    let mut documents = Vec::new();
    for directory in configs {
        documents.extend(
            std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "crn"))
                .map(|path| path.strip_prefix(root).unwrap().to_path_buf()),
        );
    }
    documents.sort();
    documents
}

fn scan(
    directory: &Path,
    scope: &DiscoveryScope,
    environment: &EnvironmentSnapshot,
    visited: &mut BTreeSet<CanonicalPath>,
    paths: &mut BTreeSet<PathBuf>,
) {
    if scope.prunes(directory) {
        return;
    }
    let Ok(canonical) = CanonicalPath::new(directory) else {
        return;
    };
    if !visited.insert(canonical) {
        return;
    }
    let mut entries = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if scope.prunes(&path) {
            continue;
        }
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            scan(&path, scope, environment, visited, paths);
        } else if path.is_file()
            && (path.extension().is_some_and(|ext| ext == "crn")
                || path
                    .file_name()
                    .is_some_and(|name| name == "carina-providers.lock"))
        {
            paths.insert(path.clone());
            if path.extension().is_some_and(|ext| ext == "crn") {
                let text = std::fs::read_to_string(&path).unwrap();
                if let Ok(parsed) = carina_core::parser::parse_file(
                    &text,
                    &path,
                    &ProviderContext::new(environment.clone()),
                ) {
                    let base = path.parent().unwrap();
                    let sources = parsed
                        .uses
                        .iter()
                        .map(|import| Path::new(&import.path))
                        .chain(
                            parsed
                                .upstream_states
                                .iter()
                                .map(|upstream| upstream.source.as_path()),
                        );
                    for source in sources {
                        let source = base.join(source);
                        if source.is_dir() {
                            scan(&source, scope, environment, visited, paths);
                        }
                    }
                }
            }
        }
    }
}

fn plain_files(root: &Path, paths: &mut BTreeSet<PathBuf>) {
    if !root.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            plain_files(&path, paths);
        } else {
            paths.insert(path);
        }
    }
}

fn copy_trust(source: &Path, destination: &Path) {
    let mut files = BTreeSet::new();
    plain_files(source, &mut files);
    for path in files {
        let target = destination.join(path.strip_prefix(source).unwrap());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(path, target).unwrap();
    }
}

fn inventory(
    directory: &Path,
    scope: &DiscoveryScope,
    visited: &mut BTreeSet<CanonicalPath>,
    paths: &mut BTreeSet<PathBuf>,
) {
    if scope.prunes(directory) {
        return;
    }
    let Ok(canonical) = CanonicalPath::new(directory) else {
        return;
    };
    if !visited.insert(canonical) {
        return;
    }
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        let candidate = path
            .extension()
            .is_some_and(|ext| ext == "crn" || ext == "wasm")
            || path
                .file_name()
                .is_some_and(|name| name == "carina-providers.lock");
        if !kind.is_dir() && !kind.is_symlink() && !candidate {
            continue;
        }
        if scope.prunes(&path) {
            continue;
        }
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            inventory(&path, scope, visited, paths);
        } else if path.is_file()
            && (path
                .extension()
                .is_some_and(|ext| ext == "crn" || ext == "wasm")
                || path
                    .file_name()
                    .is_some_and(|name| name == "carina-providers.lock"))
        {
            paths.insert(path);
        }
    }
}
