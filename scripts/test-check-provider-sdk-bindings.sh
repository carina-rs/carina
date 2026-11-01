#!/usr/bin/env bash
# Exercise the guard against isolated Cargo workspaces without building them.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/carina-sdk-bindings.XXXXXX")
trap 'rm -rf "$fixture_dir"' EXIT

mkdir -p "$fixture_dir/scripts" "$fixture_dir/provider/src" "$fixture_dir/dependency/src"
cp "$repo_root/scripts/check-provider-sdk-bindings.sh" "$fixture_dir/scripts/"
printf 'fn main() {}\n' > "$fixture_dir/provider/src/main.rs"
touch "$fixture_dir/dependency/src/lib.rs"

cat > "$fixture_dir/Cargo.toml" <<'EOF'
[workspace]
members = ["provider", "dependency"]
resolver = "2"
EOF

write_provider() {
    cat > "$fixture_dir/provider/Cargo.toml" <<'EOF'
[package]
name = "carina-provider-fixture"
version = "0.1.0"
edition = "2024"
EOF
}

write_dependency() {
    cat > "$fixture_dir/dependency/Cargo.toml" <<EOF
[package]
name = "$1"
version = "0.1.0"
edition = "2024"
EOF
}

assert_rejected() {
    local output
    if output=$(bash "$fixture_dir/scripts/check-provider-sdk-bindings.sh" 2>&1); then
        printf 'FAIL: guard accepted %s\n' "$1" >&2
        exit 1
    fi
    if [[ "$output" != *"$2"* ]]; then
        printf 'FAIL: unexpected diagnostic for %s:\n%s\n' "$1" "$output" >&2
        exit 1
    fi
}

write_provider
write_dependency carina-plugin-sdk
cat >> "$fixture_dir/provider/Cargo.toml" <<'EOF'
[dependencies]
sdk = { package = "carina-plugin-sdk", path = "../dependency" }
EOF
bash "$fixture_dir/scripts/check-provider-sdk-bindings.sh"

write_dependency wit-bindgen
for table in dependencies dev-dependencies build-dependencies "target.'cfg(target_arch = \"wasm32\")'.dependencies"; do
    write_provider
    cat >> "$fixture_dir/provider/Cargo.toml" <<EOF
[$table]
bindings = { package = "wit-bindgen", path = "../dependency" }
EOF
    assert_rejected "renamed wit-bindgen in $table" 'directly depends on wit-bindgen'
done

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

printf 'OK: Provider SDK bindings guard accepted SDK-only dependencies and rejected all 7 regressions.\n'
