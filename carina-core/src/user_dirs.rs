use std::ffi::OsString;
use std::path::{Path, PathBuf};

const TRUST_RECORD_DIR_ENV: &str = "CARINA_TRUST_RECORD_DIR";
const PLUGIN_CACHE_DIR_ENV: &str = "CARINA_PLUGIN_CACHE_DIR";
const NEXTEST_RUN_ID_ENV: &str = "NEXTEST_RUN_ID";
const TEST_HOME_ENV: &str = "CARINA_TEST_HOME";

/// Every per-user directory used by Carina.
///
/// Library code receives this value from its caller instead of discovering the
/// process home directory itself. Add future `~/.carina/*` locations here so
/// binaries and tests continue to configure user storage through one boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDirs {
    trust_record_dir: PathBuf,
    plugin_cache_dir: PathBuf,
    compiled_wasm_cache_dir: PathBuf,
    agents_skills_root: PathBuf,
}

impl UserDirs {
    /// Resolve Carina's user directories from the process's real home.
    ///
    /// This is the only Carina API that discovers a home directory. Binary
    /// entry points call it once and pass the resulting value through library
    /// code. Tests should use [`Self::under`] instead.
    pub fn from_real_home() -> Result<Self, UserDirsError> {
        // Home discovery belongs only at this process boundary. All library
        // consumers receive the resulting typed paths from their caller.
        #[expect(clippy::disallowed_methods)]
        let home = dirs::home_dir().ok_or(UserDirsError::HomeDirectoryUnavailable)?;
        Self::from_home_and_environment(
            home,
            std::env::var_os(TRUST_RECORD_DIR_ENV),
            std::env::var(PLUGIN_CACHE_DIR_ENV).ok().map(PathBuf::from),
            std::env::var_os(NEXTEST_RUN_ID_ENV).is_some(),
            std::env::var_os(TEST_HOME_ENV),
        )
    }

    /// Build every user-owned path beneath `root`, treating it as a home directory.
    pub fn under(root: &Path) -> Self {
        let carina = root.join(".carina");
        Self {
            trust_record_dir: carina.join("trust"),
            plugin_cache_dir: carina.join("plugin-cache"),
            compiled_wasm_cache_dir: carina.join("cache"),
            agents_skills_root: root.join(".agents").join("skills"),
        }
    }

    /// Directory containing user-owned provider trust records.
    pub fn trust_record_dir(&self) -> &Path {
        &self.trust_record_dir
    }

    /// Directory containing downloaded provider artifacts shared across projects.
    pub fn plugin_cache_dir(&self) -> &Path {
        &self.plugin_cache_dir
    }

    /// Directory containing compiled WebAssembly cache artifacts.
    pub fn compiled_wasm_cache_dir(&self) -> &Path {
        &self.compiled_wasm_cache_dir
    }

    /// Root directory containing user-installed Agent Skills.
    pub fn agents_skills_root(&self) -> &Path {
        &self.agents_skills_root
    }

    fn from_home_and_environment(
        home: PathBuf,
        trust_record_override: Option<OsString>,
        plugin_cache_override: Option<PathBuf>,
        in_nextest: bool,
        test_home: Option<OsString>,
    ) -> Result<Self, UserDirsError> {
        let nextest_test_home = if in_nextest {
            let Some(test_home) = test_home else {
                return Err(UserDirsError::NextestTestHomeMissing { home });
            };
            let test_home = PathBuf::from(test_home);
            if home != test_home {
                return Err(UserDirsError::NextestHomeMismatch { home, test_home });
            }
            Some(test_home)
        } else {
            None
        };

        let mut dirs = Self::under(&home);
        if let Some(value) = trust_record_override {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(UserDirsError::RelativeTrustRecordDirectory { path });
            }
            dirs.trust_record_dir = path;
        }
        if let Some(path) = plugin_cache_override {
            dirs.plugin_cache_dir = path;
        }
        if let Some(test_home) = nextest_test_home {
            if !dirs.trust_record_dir.starts_with(&test_home) {
                return Err(UserDirsError::NextestTrustRecordDirectoryOutsideTestHome {
                    path: dirs.trust_record_dir,
                    test_home,
                });
            }
            if !dirs.plugin_cache_dir.starts_with(&test_home) {
                return Err(UserDirsError::NextestPluginCacheDirectoryOutsideTestHome {
                    path: dirs.plugin_cache_dir,
                    test_home,
                });
            }
        }
        Ok(dirs)
    }
}

/// Owned, automatically cleaned user-directory fixture for tests.
#[cfg(any(test, feature = "test-utils"))]
pub struct TestUserDirs {
    user_dirs: UserDirs,
    _root: tempfile::TempDir,
}

#[cfg(any(test, feature = "test-utils"))]
impl TestUserDirs {
    /// Create an isolated user home that remains alive for this fixture's lifetime.
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("create isolated test user home");
        let user_dirs = UserDirs::under(root.path());
        Self {
            user_dirs,
            _root: root,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for TestUserDirs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl std::ops::Deref for TestUserDirs {
    type Target = UserDirs;

    fn deref(&self) -> &Self::Target {
        &self.user_dirs
    }
}

/// Failure to resolve a complete set of user-owned Carina directories.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UserDirsError {
    #[error("Could not determine the user home directory")]
    HomeDirectoryUnavailable,

    #[error("{TRUST_RECORD_DIR_ENV} must be an absolute path, got {path:?}")]
    RelativeTrustRecordDirectory { path: PathBuf },

    #[error(
        "nextest home isolation guard: {TEST_HOME_ENV} is missing while {NEXTEST_RUN_ID_ENV} is set (resolved home: {home:?})"
    )]
    NextestTestHomeMissing { home: PathBuf },

    #[error(
        "nextest home isolation guard: resolved home {home:?} does not match {TEST_HOME_ENV} {test_home:?}"
    )]
    NextestHomeMismatch { home: PathBuf, test_home: PathBuf },

    #[error(
        "nextest home isolation guard: effective trust record directory {path:?} is outside {TEST_HOME_ENV} {test_home:?}"
    )]
    NextestTrustRecordDirectoryOutsideTestHome { path: PathBuf, test_home: PathBuf },

    #[error(
        "nextest home isolation guard: effective plugin cache directory {path:?} is outside {TEST_HOME_ENV} {test_home:?}"
    )]
    NextestPluginCacheDirectoryOutsideTestHome { path: PathBuf, test_home: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_overrides_preserve_existing_path_semantics() {
        let home = PathBuf::from("/users/tester");
        let trust = OsString::from("/var/tmp/carina-trust");
        let plugin_cache = PathBuf::from("relative-plugin-cache");

        let dirs = UserDirs::from_home_and_environment(
            home.clone(),
            Some(trust),
            Some(plugin_cache.clone()),
            false,
            None,
        )
        .unwrap();

        assert_eq!(dirs.trust_record_dir(), Path::new("/var/tmp/carina-trust"));
        assert_eq!(dirs.plugin_cache_dir(), plugin_cache);
        assert_eq!(dirs.compiled_wasm_cache_dir(), home.join(".carina/cache"));
    }

    #[test]
    fn relative_trust_record_override_is_rejected() {
        let error = UserDirs::from_home_and_environment(
            PathBuf::from("/users/tester"),
            Some(OsString::from("relative/trust")),
            None,
            false,
            None,
        )
        .unwrap_err();

        assert_eq!(
            error,
            UserDirsError::RelativeTrustRecordDirectory {
                path: PathBuf::from("relative/trust")
            }
        );
    }

    #[test]
    fn nextest_requires_a_matching_test_home_marker() {
        let home = PathBuf::from("/tmp/nextest-home");
        let missing =
            UserDirs::from_home_and_environment(home.clone(), None, None, true, None).unwrap_err();
        assert_eq!(
            missing,
            UserDirsError::NextestTestHomeMissing { home: home.clone() }
        );

        let mismatch = UserDirs::from_home_and_environment(
            home.clone(),
            None,
            None,
            true,
            Some(OsString::from("/tmp/other-home")),
        )
        .unwrap_err();
        assert_eq!(
            mismatch,
            UserDirsError::NextestHomeMismatch {
                home: home.clone(),
                test_home: PathBuf::from("/tmp/other-home")
            }
        );

        let dirs = UserDirs::from_home_and_environment(
            home.clone(),
            None,
            None,
            true,
            Some(home.clone().into_os_string()),
        )
        .unwrap();
        assert_eq!(dirs.trust_record_dir(), home.join(".carina/trust"));
    }

    #[test]
    fn nextest_rejects_trust_record_override_outside_test_home() {
        let home = PathBuf::from("/tmp/nextest-home");
        let error = UserDirs::from_home_and_environment(
            home.clone(),
            Some(OsString::from("/tmp/outside/trust")),
            None,
            true,
            Some(home.clone().into_os_string()),
        )
        .unwrap_err();

        assert_eq!(
            error,
            UserDirsError::NextestTrustRecordDirectoryOutsideTestHome {
                path: PathBuf::from("/tmp/outside/trust"),
                test_home: home,
            }
        );
    }

    #[test]
    fn nextest_rejects_plugin_cache_override_outside_test_home() {
        let home = PathBuf::from("/tmp/nextest-home");
        let error = UserDirs::from_home_and_environment(
            home.clone(),
            None,
            Some(PathBuf::from("/tmp/outside/plugin-cache")),
            true,
            Some(home.clone().into_os_string()),
        )
        .unwrap_err();

        assert_eq!(
            error,
            UserDirsError::NextestPluginCacheDirectoryOutsideTestHome {
                path: PathBuf::from("/tmp/outside/plugin-cache"),
                test_home: home,
            }
        );
    }
}
