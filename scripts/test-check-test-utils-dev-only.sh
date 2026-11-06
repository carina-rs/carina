#!/usr/bin/env bash
# Exercise the resolved-graph guard with offline path dependencies, without builds.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
checker="$repo_root/scripts/check-test-utils-dev-only.sh"
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/carina-test-utils.XXXXXX")
trap 'rm -rf "$fixture_dir"' EXIT
# All fixtures use path dependencies; never consult the registry or the network.
export CARGO_NET_OFFLINE=true

mkdir -p "$fixture_dir/fixture-lib/src" "$fixture_dir/fixture-bin/src"
touch "$fixture_dir/fixture-lib/src/lib.rs"
printf 'fn main() {}\n' > "$fixture_dir/fixture-bin/src/main.rs"

cat > "$fixture_dir/Cargo.toml" <<'EOF'
[workspace]
members = ["fixture-lib", "fixture-bin"]
resolver = "2"
EOF

write_library() {
    cat > "$fixture_dir/fixture-lib/Cargo.toml" <<EOF
[package]
name = "fixture-lib"
version = "0.1.0"
edition = "2024"

[features]
test-utils = []
test-utils-extra = []
forwarded = ["test-utils"]
${1:-}

[dev-dependencies]
fixture-lib = { path = ".", features = ["test-utils"] }
EOF
}

write_binary() {
    cat > "$fixture_dir/fixture-bin/Cargo.toml" <<EOF
[package]
name = "fixture-bin"
version = "0.1.0"
edition = "2024"

[dependencies]
fixture-lib = { path = "../fixture-lib"${1:-} }
EOF
}

failures=0
valid_fixtures=0
regressions=0

assert_accepted() {
    local output
    valid_fixtures=$((valid_fixtures + 1))
    if ! output=$(bash "$checker" "${2:-$fixture_dir}" 2>&1); then
        printf 'FAIL: guard rejected %s:\n%s\n' "$1" "$output" >&2
        failures=$((failures + 1))
    else
        printf 'PASS: accepted %s\n%s\n' "$1" "$output"
    fi
}

assert_rejected() {
    local output
    regressions=$((regressions + 1))
    if output=$(bash "$checker" "$fixture_dir" 2>&1); then
        printf 'FAIL: guard accepted %s\n' "$1" >&2
        failures=$((failures + 1))
    elif [[ "$output" != *'ERROR: feature "test-utils" is enabled in the non-dev dependency graph:'* ||
            "$output" != *'fixture-bin v0.1.0'* ||
            "$output" != *"$2"* || "$output" != *'  -> '* ]]; then
        printf 'FAIL: unexpected diagnostic for %s:\n%s\n' "$1" "$output" >&2
        failures=$((failures + 1))
    else
        printf 'PASS: rejected %s\n%s\n' "$1" "$output"
    fi
}

write_library
write_binary
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
[dev-dependencies]
fixture-lib = { path = "../fixture-lib", features = ["test-utils"] }
EOF
assert_accepted 'dev-dependency features (including a self dev-dependency)'

write_binary ', features = ["test-utils-extra"]'
assert_accepted 'a similarly named normal feature via a manifest path' "$fixture_dir/Cargo.toml"

write_binary ', features = ["test-utils"]'
assert_rejected 'a normal dependency enabling test-utils' 'fixture-lib v0.1.0'

write_binary ', features = ["forwarded"]'
assert_rejected 'a normal dependency enabling a forwarded feature' 'fixture-lib v0.1.0'

write_binary
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
[build-dependencies]
fixture-lib = { path = "../fixture-lib", features = ["test-utils"] }
EOF
printf 'fn main() {}\n' > "$fixture_dir/fixture-bin/build.rs"
assert_rejected 'a build dependency enabling test-utils' 'fixture-lib v0.1.0'
rm "$fixture_dir/fixture-bin/build.rs"

write_binary
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
[target.'cfg(target_arch = "wasm32")'.dependencies]
fixture-lib = { path = "../fixture-lib", features = ["test-utils"] }
EOF
assert_rejected 'a non-host target dependency enabling test-utils' 'fixture-lib v0.1.0'

write_binary
write_library 'default = ["test-utils"]'
assert_rejected 'a dependency default enabling test-utils' 'fixture-lib v0.1.0'

write_library
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
[features]
default = ["test-utils"]
test-utils = []
EOF
assert_rejected 'a workspace root default enabling its own test-utils' 'fixture-bin v0.1.0'

# Add an intermediate crate to exercise cross-crate feature forwarding too.
mkdir -p "$fixture_dir/fixture-forwarder/src"
touch "$fixture_dir/fixture-forwarder/src/lib.rs"
cat > "$fixture_dir/Cargo.toml" <<'EOF'
[workspace]
members = ["fixture-lib", "fixture-bin", "fixture-forwarder"]
resolver = "2"
EOF
cat > "$fixture_dir/fixture-forwarder/Cargo.toml" <<'EOF'
[package]
name = "fixture-forwarder"
version = "0.1.0"
edition = "2024"

[features]
fixtures = ["fixture-lib/test-utils"]

[dependencies]
fixture-lib = { path = "../fixture-lib" }
EOF

write_binary
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
[dev-dependencies]
fixture-forwarder = { path = "../fixture-forwarder", features = ["fixtures"] }
EOF
assert_accepted 'cross-crate forwarding enabled only by a dev-dependency'

write_binary
cat >> "$fixture_dir/fixture-bin/Cargo.toml" <<'EOF'
fixture-forwarder = { path = "../fixture-forwarder", features = ["fixtures"] }
EOF
assert_rejected 'a normal dependency forwarding another crate test-utils' 'fixture-lib v0.1.0'

if [[ "$failures" -gt 0 ]]; then
    printf 'FAIL: test-utils dev-only guard failed %s fixture(s).\n' "$failures" >&2
    exit 1
fi

printf 'OK: test-utils dev-only guard accepted all %s valid fixtures and rejected all %s regressions.\n' "$valid_fixtures" "$regressions"
