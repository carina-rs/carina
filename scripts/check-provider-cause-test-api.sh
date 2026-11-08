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

    expected_methods = {"matches_for_test", "to_wire", "round_trip"}
    missing_methods = {
        name for error in errors for name in expected_methods if name in error["message"]
    }
    if result.returncode == 0:
        sys.exit("ERROR: provider test predicates or native encoding are exposed")
    if result.returncode != 101 or missing_methods != expected_methods or any(
        (error.get("code") or {}).get("code") != "E0599"
        or not any(name in error["message"] for name in expected_methods)
        for error in errors
    ):
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        sys.exit("ERROR: expected E0599 for unavailable predicates and native wire fixtures")
    print("OK: downstream predicates and native encoding/round-trip fixtures are unavailable (E0599).")

    # Compile as an actual guest provider depending on the SDK. Repeat with
    # unified test features: a dependency feature must not grant wire readers.
    manifest_path = fixture / "Cargo.toml"
    manifest_path.write_text(manifest_path.read_text().replace(
        '[patch.crates-io]',
        'carina-plugin-sdk = { path = ' + json.dumps(str(repo / "carina-plugin-sdk")) + ' }\n'
        'serde_json = "1"\n\n'
        '[features]\nunified-test-utils = ["carina-core/test-utils", "carina-plugin-sdk/test-utils"]\n\n'
        '[patch.crates-io]',
    ))
    for target in ([], ["--target", "wasm32-wasip2"]):
        for features in ([], ["--features", "unified-test-utils"]):
            for filename, should_compile in (("wire_decode.rs", False), ("direct_authoring.rs", True)):
                shutil.copyfile(repo / "scripts/tests/provider-cause-test-api" / filename, fixture / "src/lib.rs")
                result = subprocess.run([
                    "cargo", "check", "--offline", "--lib", "--message-format=json",
                    *target, *features, "--manifest-path", str(manifest_path),
                    "--target-dir", str(repo / "target"),
                ], env=environment, capture_output=True, text=True, timeout=600)
                errors = []
                for line in result.stdout.splitlines():
                    try:
                        message = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if message.get("reason") == "compiler-message" and message["message"]["level"] == "error":
                        errors.append(message["message"])
                codes = [(error.get("code") or {}).get("code") for error in errors]
                valid_failure = result.returncode == 101 and sorted(codes) == sorted(["E0308", "E0624" if features and not target else "E0599"])
                if (should_compile and result.returncode != 0) or (not should_compile and not valid_failure):
                    print(result.stdout, file=sys.stderr)
                    print(result.stderr, file=sys.stderr)
                    sys.exit(f"ERROR: provider wire decode check failed: {filename}, target={target}, features={features}")
            print(f"OK: provider wire decoders are unavailable and direct authoring compiles; target={target}, features={features}.")
    for features in ([], ["--features", "unified-test-utils"]):
        for filename, should_compile in (("wasm_lossy_rewrap.rs", False), ("wasm_sdk_encoding.rs", True)):
            shutil.copyfile(repo / "scripts/tests/provider-cause-test-api" / filename, fixture / "src/lib.rs")
            result = subprocess.run([
                "cargo", "check", "--offline", "--lib", "--message-format=json",
                "--target", "wasm32-wasip2", *features,
                "--manifest-path", str(manifest_path), "--target-dir", str(repo / "target"),
            ], env=environment, capture_output=True, text=True, timeout=600)
            errors = []
            for line in result.stdout.splitlines():
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if message.get("reason") == "compiler-message" and message["message"]["level"] == "error":
                    errors.append(message["message"])
            expected_codes = {
                "error_message": "E0609", "partial_reason": "E0609",
                "sdk_error_message": "E0609", "sdk_partial_reason": "E0609",
                "serialize_error": "E0277", "serialize_partial": "E0277",
                "private_wire_field": "E0616", "native_fixture": "E0433",
                "native_round_trip_fixture": "E0433",
            }
            lines = (fixture / "src/lib.rs").read_text().splitlines()
            current_function = None
            functions = {}
            for number, line in enumerate(lines, 1):
                if line.startswith("pub fn "):
                    current_function = line.split("pub fn ", 1)[1].split("(", 1)[0]
                functions[number] = current_function
            rejected = {}
            for error in errors:
                primary = next((span for span in error["spans"] if span["is_primary"]), {})
                function = functions.get(primary.get("line_start"))
                rejected.setdefault(function, []).append((error.get("code") or {}).get("code"))
            valid_failure = result.returncode == 101 and rejected == {
                function: [code] for function, code in expected_codes.items()
            }
            if (should_compile and result.returncode != 0) or (not should_compile and not valid_failure):
                print(result.stdout, file=sys.stderr)
                print(result.stderr, file=sys.stderr)
                sys.exit(f"ERROR: wasm SDK opacity check failed: {filename}, features={features}")
        print(f"OK: wasm SDK encoding compiles and all nine lossy routes are rejected; features={features}.")
PY
