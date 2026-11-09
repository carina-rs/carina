#!/usr/bin/env bash
# Verify the cache-key boundary without invoking Cargo or needing Rust caches.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
python3 - "${1:-$repo_root/scripts/rust-cache-key.sh}" <<'PY'
from pathlib import Path
import subprocess
import sys
import tempfile

script = str(Path(sys.argv[1]).resolve())
original = '''[workspace]
members = ["example"]
[workspace.package]
version = "0.13.0"
publish = false
[workspace.dependencies.example]
version = "1.0.0"
[profile.dev]
debug = "line-tables-only"
[patch.crates-io]
example = { path = "vendor/example" }
'''

with tempfile.TemporaryDirectory(prefix="carina-rust-cache-key-") as temporary:
    manifest = Path(temporary) / "Cargo.toml"

    def key(contents):
        manifest.write_bytes(contents.encode())
        result = subprocess.check_output(["bash", script, str(manifest)], text=True)
        assert len(result.strip()) == 64 and int(result.strip(), 16) >= 0, result
        return result

    baseline = key(original)
    assert key(original.replace('version = "0.13.0"', 'version = "0.13.1"')) == baseline
    for before, after in [
        ('version = "1.0.0"', 'version = "1.0.1"'),
        ('debug = "line-tables-only"', 'debug = "full"'),
        ('debug = "line-tables-only"', 'debug = 0'),
        ('vendor/example', 'vendor/example-new'),
        ('members = ["example"]', 'members = ["example", "another"]'),
        ('publish = false', 'publish = true'),
        ('version = "0.13.0"', 'version  = "0.13.0"'),
    ]:
        assert key(original.replace(before, after)) != baseline, (before, after)
    assert key(original + "# keep comments in the key\n") != baseline
    assert key(original.rstrip("\n")) != baseline
    assert key(original.replace("\n", "\r\n")) != baseline

    # Versions of ordinary packages must also remain part of the root key.
    package = '[package]\nname = "root"\nversion = "1.0.0"\n' + original
    assert key(package) != key(package.replace('version = "1.0.0"', 'version = "1.0.1"'))

    # Fake headers inside multiline strings are never eligible for normalization.
    for quote in ('"""', "'''"):
        multiline = '[package.metadata]\ntext = ' + quote + '\n[workspace.package]\nversion = "1.0.0"\n' + quote + '\n'
        assert key(multiline) != key(multiline.replace("1.0.0", "1.0.1"))

    manifest.unlink()
    result = subprocess.run(["bash", script, str(manifest)], capture_output=True)
    assert result.returncode != 0, "a missing manifest must not produce a usable cache key"

print("OK: only the workspace package version is normalized in the Rust cache key.")
PY
