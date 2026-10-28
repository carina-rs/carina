#![cfg(feature = "test-utils")]

use std::path::Path;

use carina_core::UserDirs;

#[test]
fn user_dirs_under_keeps_every_user_owned_path_beneath_the_supplied_home() {
    let home = Path::new("/tmp/carina-user-dirs-test-home");

    let dirs = UserDirs::under(home);

    assert_eq!(dirs.trust_record_dir(), home.join(".carina/trust"));
    assert_eq!(dirs.plugin_cache_dir(), home.join(".carina/plugin-cache"));
    assert_eq!(dirs.compiled_wasm_cache_dir(), home.join(".carina/cache"));
    assert_eq!(dirs.agents_skills_root(), home.join(".agents/skills"));
    for path in [
        dirs.trust_record_dir(),
        dirs.plugin_cache_dir(),
        dirs.compiled_wasm_cache_dir(),
        dirs.agents_skills_root(),
    ] {
        assert!(
            path.starts_with(home),
            "{} escaped the test home",
            path.display()
        );
    }
}
