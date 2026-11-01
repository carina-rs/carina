#!/usr/bin/env bash
# Exercise the guard against isolated Cargo workspaces without building them.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/carina-sdk-bindings.XXXXXX")
trap 'rm -rf "$fixture_dir"' EXIT

mkdir -p "$fixture_dir/scripts" "$fixture_dir/provider/src" "$fixture_dir/dependency/src" "$fixture_dir/sdk/src"
cp "$repo_root/scripts/check-provider-sdk-bindings.sh" "$fixture_dir/scripts/"
printf 'fn main() {}\n' > "$fixture_dir/provider/src/main.rs"
touch "$fixture_dir/dependency/src/lib.rs"
touch "$fixture_dir/sdk/src/lib.rs"

cat > "$fixture_dir/Cargo.toml" <<'EOF'
[workspace]
members = ["provider", "dependency", "sdk"]
resolver = "2"
EOF

cat > "$fixture_dir/sdk/Cargo.toml" <<'EOF'
[package]
name = "carina-plugin-sdk"
version = "0.1.0"
edition = "2024"
EOF

write_provider() {
    cat > "$fixture_dir/provider/Cargo.toml" <<EOF
[package]
name = "${1:-carina-provider-fixture}"
version = "0.1.0"
edition = "2024"
EOF
    if [[ "${2:-}" != without-sdk ]]; then
        cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dependencies.sdk]
package = "carina-plugin-sdk"
path = "../sdk"
EOF
    fi
}

write_dependency() {
    cat > "$fixture_dir/dependency/Cargo.toml" <<EOF
[package]
name = "$1"
version = "0.1.0"
edition = "2024"
EOF
}

failures=0
valid_fixtures=0
regressions=0

assert_accepted() {
    local output
    valid_fixtures=$((valid_fixtures + 1))
    if ! output=$(bash "$fixture_dir/scripts/check-provider-sdk-bindings.sh" 2>&1); then
        printf 'FAIL: guard rejected %s:\n%s\n' "$1" "$output" >&2
        failures=$((failures + 1))
    fi
}

assert_rejected() {
    local output
    regressions=$((regressions + 1))
    if output=$(bash "$fixture_dir/scripts/check-provider-sdk-bindings.sh" 2>&1); then
        printf 'FAIL: guard accepted %s\n' "$1" >&2
        failures=$((failures + 1))
    elif [[ "$output" != *"$2"* ]]; then
        printf 'FAIL: unexpected diagnostic for %s:\n%s\n' "$1" "$output" >&2
        failures=$((failures + 1))
    fi
}

write_provider
write_dependency fixture-dependency
assert_accepted 'SDK-only dependencies'

printf '[WIT sources](https://github.com/carina-rs/carina-plugin-wit)\n' > "$fixture_dir/provider/README.md"
assert_accepted 'README link to carina-plugin-wit'
rm "$fixture_dir/provider/README.md"

write_dependency wit-bindgen
for table in dependencies dev-dependencies build-dependencies "target.'cfg(target_arch = \"wasm32\")'.dependencies"; do
    write_provider
    cat >> "$fixture_dir/provider/Cargo.toml" <<EOF
[$table]
bindings = { package = "wit-bindgen", path = "../dependency" }
EOF
    assert_rejected "renamed wit-bindgen in $table" 'directly depends on wit-bindgen'
done

write_provider differently-named-provider
cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dependencies]
bindings = { package = "wit-bindgen", path = "../dependency" }
EOF
assert_rejected 'provider without carina-provider- prefix' 'directly depends on wit-bindgen'

for dependency in wit-bindgen-rt wit-bindgen-rust-macro; do
    write_provider
    write_dependency "$dependency"
    cat >> "$fixture_dir/provider/Cargo.toml" <<EOF
[dependencies]
bindings = { package = "$dependency", path = "../dependency" }
EOF
    assert_rejected "renamed $dependency dependency" "directly depends on $dependency"
done

write_dependency wit-bindgen
for name in carina-provider-protocol carina-provider-resolver; do
    write_provider "$name" without-sdk
    cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dependencies]
bindings = { package = "wit-bindgen", path = "../dependency" }
EOF
    assert_accepted "$name without an SDK dependency"
done

write_provider carina-provider-test-consumer without-sdk
cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dev-dependencies.sdk]
package = "carina-plugin-sdk"
path = "../sdk"
[dependencies]
bindings = { package = "wit-bindgen", path = "../dependency" }
EOF
assert_accepted 'dev-only SDK consumer'

write_provider
write_dependency carina-plugin-bindings-provider
cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dependencies]
bindings = { package = "carina-plugin-bindings-provider", path = "../dependency" }
EOF
assert_rejected 'direct binding-crate dependency' 'directly depends on carina-plugin-bindings-provider'

write_provider
printf 'const WIT: &str = "../carina-plugin-wit/wit";\n' > "$fixture_dir/provider/build.rs"
assert_rejected 'WIT path in build script' 'build.rs:1: provider-owned WIT reference'
rm "$fixture_dir/provider/build.rs"
printf 'carina_plugin_sdk::__carina_wit_bindgen!({});\n' > "$fixture_dir/provider/src/main.rs"
assert_rejected 'consumer-side generation helper' 'main.rs:1: provider-owned WIT reference'

if [[ "$failures" -gt 0 ]]; then
    printf 'FAIL: Provider SDK bindings guard failed %s fixture(s).\n' "$failures" >&2
    exit 1
fi

printf 'OK: Provider SDK bindings guard accepted all %s valid fixtures and rejected all %s regressions.\n' "$valid_fixtures" "$regressions"
