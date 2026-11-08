#!/usr/bin/env bash
# Compile a downstream crate outside workspace dev-feature unification.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

python3 - "$repo_root" <<'PY'
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib

repo = Path(sys.argv[1])
workspace = tomllib.loads((repo / "Cargo.toml").read_text())
with tempfile.TemporaryDirectory(prefix="carina-provider-cause-api-") as temporary:
    fixture = Path(temporary)
    manifest = [
        '[package]',
        'name = "provider-cause-without-test-utils"',
        'version = "0.0.0"',
        'edition = "2024"',
        'publish = false',
        '',
        '[workspace]',
        'resolver = "2"',
        '',
        '[dependencies]',
        'carina-core = { path = ' + json.dumps(str(repo / "carina-core"))
        + ', default-features = false }',
        '',
        '[patch.crates-io]',
    ]
    # Keep resolution and compiler artifacts aligned with the real workspace.
    for name, patch in workspace["patch"]["crates-io"].items():
        manifest.append(
            name + ' = { path = ' + json.dumps(str(repo / patch["path"])) + ' }'
        )
    manifest += ['', '[profile.dev]', 'debug = "line-tables-only"', '']
    (fixture / "Cargo.toml").write_text("\n".join(manifest))
    shutil.copyfile(repo / "Cargo.lock", fixture / "Cargo.lock")
    (fixture / "src").mkdir()
    shutil.copyfile(
        repo / "scripts/tests/provider-cause-test-api/no_test_utils.rs",
        fixture / "src/lib.rs",
    )

    environment = dict(os.environ)
    environment["CARGO_NET_OFFLINE"] = "true"
    result = subprocess.run(
        [
            "cargo", "check", "--offline", "--lib", "--message-format=json",
            "--manifest-path", str(fixture / "Cargo.toml"),
            "--target-dir", str(repo / "target"),
        ],
        env=environment, capture_output=True, text=True, timeout=300,
    )
    errors = []
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-message":
            diagnostic = message["message"]
            if diagnostic["level"] == "error":
                errors.append(diagnostic)

    expected_methods = {"matches_for_test", "to_wire"}
    missing_methods = {
        name for error in errors for name in expected_methods if name in error["message"]
    }
    if result.returncode == 0:
        sys.exit("ERROR: provider test predicates or native encoding are exposed")
    if result.returncode != 101 or missing_methods != expected_methods or any(
        (error.get("code") or {}).get("code") != "E0599"
        or not any(name in error["message"] for name in ("matches_for_test", "to_wire"))
        for error in errors
    ):
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit("ERROR: expected E0599 for both unavailable predicates and native partial encoding")
    print("OK: downstream predicates and native partial plaintext encoding are unavailable (E0599).")
PY
