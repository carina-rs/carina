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

    if result.returncode == 0:
        sys.exit("ERROR: matches_for_test is exposed without test-utils")
    if result.returncode != 101 or not errors or any(
        (error.get("code") or {}).get("code") != "E0599"
        or "matches_for_test" not in error["message"]
        for error in errors
    ):
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit("ERROR: expected only E0599 for the unavailable matches_for_test method")
    print("OK: downstream matches_for_test is unavailable without test-utils (E0599).")
PY
