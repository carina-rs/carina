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
    # unified test features: a dependency feature must not grant wire decoders.
    manifest_path = fixture / "Cargo.toml"
    manifest_path.write_text(manifest_path.read_text().replace(
        '[patch.crates-io]',
        'carina-plugin-sdk = { path = ' + json.dumps(str(repo / "carina-plugin-sdk")) + ' }\n'
        'serde = { version = "1", features = ["derive"] }\n'
        'serde_json = "1"\n\n'
        '[features]\nunified-test-utils = ["carina-core/test-utils", "carina-plugin-sdk/test-utils"]\n'
        'export-http = []\nexport-aws = ["carina-plugin-sdk/aws"]\nforbidden-provider-host = ["dep:host-feature-enabler"]\n\n'
        '[patch.crates-io]',
    ))
    # Activate core's host feature through another dependency, not a direct
    # manifest feature edge. Guest safety must not depend on the lint script.
    enabler = fixture / "host-feature-enabler"
    (enabler / "src").mkdir(parents=True)
    (enabler / "src/lib.rs").write_text("")
    (enabler / "Cargo.toml").write_text(
        '[package]\nname = "host-feature-enabler"\nversion = "0.0.0"\nedition = "2024"\n'
        '[dependencies]\ncarina-core = { path = ' + json.dumps(str(repo / "carina-core"))
        + ', features = ["provider-host"] }\n'
    )
    manifest_path.write_text(manifest_path.read_text().replace(
        '[dependencies]',
        '[dependencies]\nhost-feature-enabler = { path = "host-feature-enabler", optional = true }',
        1,
    ))
    for world in ([], ["--features", "export-http"], ["--features", "export-aws"]):
        shutil.copyfile(repo / "scripts/tests/provider-cause-test-api/wasm_export_forbid_unsafe.rs", fixture / "src/lib.rs")
        result = subprocess.run([
            "cargo", "check", "--offline", "--lib", "--target", "wasm32-wasip2", *world,
            "--manifest-path", str(manifest_path), "--target-dir", str(repo / "target"),
        ], env=environment, capture_output=True, text=True, timeout=600)
        if result.returncode != 0:
            print(result.stdout, file=sys.stderr)
            print(result.stderr, file=sys.stderr)
            sys.exit(f"ERROR: export_provider! must compile under forbid(unsafe_code): world={world}")
        print(f"OK: export_provider! compiles under forbid(unsafe_code); world={world}.")
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
        for filename, should_compile in (("wasm_native_fixtures.rs", False), ("wasm_sdk_encoding.rs", True)):
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
                "native_fixture": "E0433",
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
                sys.exit(f"ERROR: flattened SDK encoding or native fixture check failed: {filename}, features={features}")
        print(f"OK: flattened wire encoding compiles and native fixtures remain unavailable; features={features}.")

    # Host diagnostic plumbing stays private even with provider test features.
    # Emission through the host-supplied session is the positive control.
    for target in ([], ['--target', 'wasm32-wasip2']):
        for features in ([], ['--features', 'unified-test-utils']):
            for filename, forbidden in (('forbidden.rs', True), ('allowed.rs', False)):
                shutil.copyfile(repo / 'scripts/tests/provider-diagnostic-host-api' / filename, fixture / 'src/lib.rs')
                result = subprocess.run([
                    'cargo', 'check', '--offline', '--lib', '--message-format=json',
                    *target, *features, '--manifest-path', str(fixture / 'Cargo.toml'),
                    '--target-dir', str(repo / 'target'),
                ], env=environment, capture_output=True, text=True, timeout=600)
                errors = []
                for line in result.stdout.splitlines():
                    try:
                        message = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if message.get('reason') == 'compiler-message' and message['message']['level'] == 'error':
                        errors.append(message['message'])
                expected = {'session', 'session_builder', 'inherit_diagnostic_host', 'for_new_guest', 'for_diagnostic_call', 'install_event_schemas', 'provider_http_trace_enabled'}
                rejected = {name for error in errors for name in expected if name in error['message']}
                valid = (result.returncode == 101 and rejected == expected and len(errors) == len(expected)
                         and all((error.get('code') or {}).get('code') == ('E0425' if 'provider_http_trace_enabled' in error['message'] else 'E0624') for error in errors)) if forbidden else result.returncode == 0
                if not valid:
                    for error in errors:
                        print(error.get('rendered', error['message']), file=sys.stderr)
                    print(result.stderr, file=sys.stderr)
                    sys.exit(f'ERROR: host diagnostic plumbing visibility: {filename}, target={target}, features={features}')
            print(f'OK: provider emission allowed; host session, presentation and call-budget plumbing private (E0624); target={target}, features={features}.')

    # Initialization fixtures expose session construction only to native hosts;
    # test-utils alone must not grant this extension trait through a glob import.
    shutil.copyfile(repo / "scripts/tests/provider-diagnostic-host-api/initialize_fixture.rs", fixture / "src/lib.rs")
    for features, should_compile in (("unified-test-utils", False), ("unified-test-utils,forbidden-provider-host", True)):
        result = subprocess.run([
            "cargo", "check", "--offline", "--lib", "--message-format=json",
            "--features", features, "--manifest-path", str(manifest_path),
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
        rejected_trait = any(
            (error.get("code") or {}).get("code") == "E0432" and "InitializeFixture" in error["message"]
            for error in errors
        )
        if (should_compile and result.returncode != 0) or (not should_compile and (result.returncode != 101 or not rejected_trait)):
            for error in errors:
                print(error.get("rendered", error["message"]), file=sys.stderr)
            print(result.stderr, file=sys.stderr)
            sys.exit(f"ERROR: initialization fixture must require provider-host: features={features}")
        print(f"OK: initialization fixture visibility follows provider-host; features={features}.")

    for target, filename in (([], "forbidden.rs"), (["--target", "wasm32-wasip2"], "allowed.rs")):
        shutil.copyfile(repo / "scripts/tests/provider-diagnostic-host-api" / filename, fixture / "src/lib.rs")
        result = subprocess.run([
            "cargo", "check", "--offline", "--lib", "--message-format=json",
            *target, "--features", "forbidden-provider-host", "--manifest-path", str(manifest_path),
            "--target-dir", str(repo / "target"),
        ], env=environment, capture_output=True, text=True, timeout=600)
        if not target:
            if result.returncode != 0:
                print(result.stdout, file=sys.stderr)
                print(result.stderr, file=sys.stderr)
                sys.exit("ERROR: trusted native hosts must retain host plumbing under feature unification")
            print("OK: native host plumbing compiles with a transitively enabled provider-host feature.")
            continue
        expected = "carina-core host plumbing is not available to wasm32 guest providers; remove the `provider-host` feature from guest dependencies"
        diagnostics = []
        for line in result.stdout.splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("reason") == "compiler-message" and message["message"]["level"] == "error":
                diagnostics.append(message["message"])
        if result.returncode != 101 or not any(error["message"] == expected for error in diagnostics):
            for error in diagnostics:
                print(error.get("rendered", error["message"]), file=sys.stderr)
            print(result.stderr, file=sys.stderr)
            sys.exit("ERROR: wasm32 must reject a transitively enabled provider-host feature with the core compile_error")
        print("OK: wasm32 rejects transitively enabled host plumbing with the core compile_error.")
PY
