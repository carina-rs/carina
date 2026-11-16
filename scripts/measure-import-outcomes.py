#!/usr/bin/env python3
"""Measure genuine main vs HEAD using saved bin/{main,head} pairs and artifacts.json.
Usage: python3 scripts/measure-import-outcomes.py --evidence INPUT --output OUTPUT
Run via heavy-slot; all runtime files and classifications go to OUTPUT."""

import argparse
import hashlib
import json
import os
import queue
import subprocess
import threading
import time
import traceback
from pathlib import Path
from urllib.parse import unquote, urlparse


PREFIX = ["nice", "-n", "19", "env", "CARGO_BUILD_JOBS=4", "NEXTEST_TEST_THREADS=4"]
FIXTURES = (
    Path(__file__).resolve().parent.parent
    / "carina-cli/tests/fixtures/import_outcome_matrix"
)
DECISION_KEYS = {
    "user-prohibitions",
    "4398-placement",
    "4398-importability",
    "4398-readable-prohibitions",
    "A",
    "C",
}


def dump(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    path.write_text(text)


def clean_env(output, phase, ident):
    home = output / "homes" / phase / ident
    for directory in ["tmp", "trust", "plugins", "cache"]:
        (home / directory).mkdir(parents=True, exist_ok=True)
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("AWS_")
        and not any(
            secret in key.upper()
            for secret in ["TOKEN", "PASSWORD", "SECRET", "ACCESS_KEY"]
        )
    }
    for key in [
        "CARINA_MOCK_STATE_FILE",
        "CARINA_MOCK_LOG_FILE",
        "CLICOLOR_FORCE",
        "RUST_LOG",
        "CARINA_TRUST_LOCAL_PROVIDER",
    ]:
        env.pop(key, None)
    env.update(
        HOME=str(home),
        TMPDIR=str(home / "tmp"),
        CARINA_TEST_HOME=str(home),
        CARINA_TRUST_RECORD_DIR=str(home / "trust"),
        CARINA_PLUGIN_CACHE_DIR=str(home / "plugins"),
        XDG_CACHE_HOME=str(home / "cache"),
        NO_COLOR="1",
        AWS_EC2_METADATA_DISABLED="true",
        AWS_CONFIG_FILE="/dev/null",
        AWS_SHARED_CREDENTIALS_FILE="/dev/null",
    )
    return env


def prepare_fixture(directory, fixture, root):
    root.mkdir(parents=True)
    files = {
        str(path.relative_to(directory)): path.read_text()
        for path in sorted(directory.rglob("*.crn"))
    }
    files.update(fixture["files"])
    for relative in fixture["directories"]:
        (root / relative).mkdir(parents=True, exist_ok=True)
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        kind = fixture["runtime_unreadable"].get(relative)
        path.write_bytes(b"\xff" if kind == "invalid_utf8" else text.encode())
        if kind == "mode000":
            path.chmod(0o000)
        if kind:
            try:
                path.read_text()
            except (OSError, UnicodeError):
                pass
            else:
                raise RuntimeError(
                    f"{fixture['id']}: {relative} must be unreadable; run unprivileged"
                )
    return files


def restore_permissions(root, fixture):
    for relative, kind in fixture["runtime_unreadable"].items():
        path = root / relative
        if kind == "mode000" and path.exists():
            path.chmod(0o600)


class Lsp:
    def __init__(self, binary, root, output, env):
        self.root = root
        self.output = output
        self.sent = bytearray()
        self.raw = bytearray()
        self.events = []
        self.latest = {}
        self.messages = queue.Queue()
        self.next_id = 0
        self.versions = {}
        self.stderr = (output / "lsp.stderr").open("wb")
        self.server = subprocess.Popen(
            PREFIX + [str(binary)],
            cwd=root,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
        )
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        try:
            while True:
                headers = {}
                while True:
                    line = self.server.stdout.readline()
                    self.raw.extend(line)
                    if not line:
                        self.messages.put({"eof": True})
                        return
                    if line in [b"\r\n", b"\n"]:
                        break
                    key, value = line.decode().split(":", 1)
                    headers[key.lower()] = value.strip()
                body = self.server.stdout.read(int(headers["content-length"]))
                self.raw.extend(body)
                event = json.loads(body)
                self.events.append(event)
                self.messages.put(event)
        except Exception as error:
            self.messages.put({"reader_error": str(error)})

    def send(self, **message):
        body = json.dumps({"jsonrpc": "2.0", **message}, ensure_ascii=True).encode()
        packet = f"Content-Length: {len(body)}\r\n\r\n".encode() + body
        self.sent.extend(packet)
        self.server.stdin.write(packet)
        self.server.stdin.flush()

    def receive(self):
        while True:
            message = self.messages.get(timeout=45)
            if "eof" in message or "reader_error" in message:
                raise RuntimeError(message)
            if "id" in message and "method" in message:
                self.send(id=message["id"], result=None)
                continue
            if message.get("method") == "textDocument/publishDiagnostics":
                params = message["params"]
                self.latest[params["uri"]] = params["diagnostics"]
            return message

    def request(self, method, params):
        self.next_id += 1
        ident = self.next_id
        message = {"id": ident, "method": method}
        if method != "shutdown":
            message["params"] = params
        self.send(**message)
        while True:
            response = self.receive()
            if response.get("id") == ident:
                if "error" in response:
                    raise RuntimeError(response)
                return response["result"]

    def document(self, relative, text):
        uri = (self.root / relative).as_uri()
        if relative in self.versions:
            self.versions[relative] += 1
            self.send(
                method="textDocument/didChange",
                params={
                    "textDocument": {"uri": uri, "version": self.versions[relative]},
                    "contentChanges": [{"text": text}],
                },
            )
        else:
            self.versions[relative] = 1
            self.send(
                method="textDocument/didOpen",
                params={
                    "textDocument": {
                        "uri": uri,
                        "languageId": "carina",
                        "version": 1,
                        "text": text,
                    }
                },
            )
        while True:
            message = self.receive()
            if (
                message.get("method") == "textDocument/publishDiagnostics"
                and message["params"]["uri"] == uri
            ):
                return

    def snapshot(self):
        return {
            str(Path(unquote(urlparse(uri).path)).relative_to(self.root)): diagnostics
            for uri, diagnostics in sorted(self.latest.items())
        }

    def feature(self, method, relative, line, character):
        params = {
            "textDocument": {"uri": (self.root / relative).as_uri()},
            "position": {"line": line, "character": character},
        }
        if method == "completion":
            params["context"] = {"triggerKind": 1}
        return self.request("textDocument/" + method, params)

    def finish(self):
        try:
            self.request("shutdown", None)
            self.send(method="exit")
            self.server.stdin.close()
            self.server.wait(timeout=15)
            self.thread.join(timeout=5)
        finally:
            if self.server.poll() is None:
                self.server.kill()
                self.server.wait()
                self.thread.join(timeout=5)
            self.stderr.close()
            (self.output / "lsp.stdin").write_bytes(self.sent)
            (self.output / "lsp.stdout").write_bytes(self.raw)
            (self.output / "lsp.exit").write_text(str(self.server.returncode) + "\n")
            dump(self.output / "lsp.events.json", self.events)


def cli(binary, root, output, env, label, args):
    # Keep the nice wrapper's stderr separate from the application's native bytes.
    command = PREFIX + [
        "/bin/sh",
        "-c",
        'exec "$@" 2>"$CARINA_MEASURE_STDERR"',
        "carina-measure",
        str(binary),
        *args,
    ]
    run_env = dict(env, CARINA_MEASURE_STDERR=str(output / (label + ".stderr")))
    result = subprocess.run(
        command, cwd=root / "caller", env=run_env, capture_output=True, timeout=90
    )
    (output / (label + ".runner-stderr")).write_bytes(result.stderr)
    stderr = (output / (label + ".stderr")).read_bytes()
    (output / (label + ".stdout")).write_bytes(result.stdout)
    (output / (label + ".exit")).write_text(str(result.returncode) + "\n")
    return {
        "exit": result.returncode,
        "stdout": result.stdout.decode("utf8"),
        "stderr": stderr.decode("utf8"),
    }


def interface_overlay(fixture, files, mode):
    original = files[fixture["immediate_importer"]]
    prefix = original.split("let result = service", 1)[0].rstrip() + "\n"
    if mode == "ui":
        return prefix + "let result = service {\n  n = 1\n}\n\n"
    if mode == "empty":
        return prefix + "let result = service {\n  \n}\n"
    if mode == "argument":
        return prefix + "let result = service { n = 'wrong' unexpected = 1 }\n"
    if mode == "reference":
        return (
            prefix
            + "let supplied = mock.test.resource { name = 'supplied' }\n"
            + "let result = service { n = supplied.name }\n"
            + "mock.test.resource { name = result.absent }\n"
        )
    if mode == "exports":
        return (
            prefix
            + "let result = service {\n}\n"
            + "let sink = use { source = '../completion-sink' }\n"
            + "let sink_call = sink {\n  text = \n}\n"
        )
    raise ValueError(mode)


def caller_part(fixture, snapshot):
    return {
        name: value
        for name, value in snapshot.items()
        if name.startswith("caller/")
        or (fixture["route"] == "nested" and name.startswith("bridge/"))
    }


def target_part(fixture, snapshot):
    return {
        name: snapshot[name] for name in fixture["target_paths"] if name in snapshot
    }


def measure(binaries, root, output, env, fixture, files):
    cells = {}
    for name, args in [
        ("validate_human", ["validate", "."]),
        ("validate_json", ["validate", "--json", "."]),
        ("lint", ["lint", "."]),
    ]:
        cells[name] = cli(binaries / "carina", root, output, env, name, args)
    lsp = Lsp(binaries / "carina-lsp", root, output, env)
    try:
        initialize = lsp.request(
            "initialize",
            {
                "processId": None,
                "rootUri": root.as_uri(),
                "capabilities": {
                    "textDocument": {
                        "completion": {"completionItem": {"snippetSupport": True}}
                    }
                },
            },
        )
        dump(output / "initialize.json", initialize)
        lsp.send(method="initialized", params={})
        while True:
            message = lsp.receive()
            ready = message.get("params", {}).get("message", "")
            if message.get("method") == "window/logMessage" and ready.startswith(
                "Loaded providers for "
            ):
                break
        if ready != fixture["schema_readiness"]:
            raise RuntimeError(
                f"{fixture['id']}: unexpected provider readiness: {ready}"
            )
        dump(output / "readiness.json", {"message": ready})
        opened = sorted(
            (name for name in files if name not in fixture["unreadable_paths"]),
            key=lambda name: (name == fixture["caller"], name),
        )
        for _ in range(2):
            for relative in opened:
                lsp.document(relative, files[relative])
        disk = lsp.snapshot()
        dump(output / "diagnostics.disk.json", disk)
        for relative in fixture["unreadable_paths"]:
            lsp.document(relative, files[relative])
        for relative in sorted(files):
            lsp.document(relative, files[relative])
        all_open = lsp.snapshot()
        dump(output / "diagnostics.all-open.json", all_open)
        cells["lsp_caller"] = {
            "all_siblings_open": caller_part(fixture, all_open),
            "faithful_disk_error": caller_part(fixture, disk),
        }
        cells["lsp_target"] = {
            "all_siblings_open": target_part(fixture, all_open),
            "faithful_disk_error": target_part(fixture, disk),
        }
        owner = fixture["immediate_importer"]
        overlay = interface_overlay(fixture, files, "ui")
        lsp.document(owner, overlay)
        dump(output / "ui-overlay.json", {"document": owner, "text": overlay})
        lines = overlay.splitlines()
        call_line = next(
            i for i, line in enumerate(lines) if line.startswith("let result = service")
        )
        for column, method, line, character in [
            ("hover_module", "hover", call_line, 14),
            ("hover_argument", "hover", call_line + 1, 2),
            ("completion_parameter", "completion", call_line + 1, 2),
            ("completion_top_level", "completion", len(lines) - 1, 0),
        ]:
            cells[column] = lsp.feature(method, owner, line, character)
        top = cells["completion_top_level"]
        items = top.get("items", []) if isinstance(top, dict) else top or []
        cells["completion_snippet"] = [
            item
            for item in items
            if item.get("label") == "service"
            or item.get("insertText", "").startswith("service {")
        ]
        lsp.document(owner, interface_overlay(fixture, files, "empty"))
        cells["completion_parameter_empty"] = lsp.feature(
            "completion", owner, call_line + 1, 2
        )
        export = interface_overlay(fixture, files, "exports")
        lsp.document(owner, export)
        export_line = next(
            i for i, line in enumerate(export.splitlines()) if line == "  text = "
        )
        cells["completion_export_reference"] = lsp.feature(
            "completion", owner, export_line, len("  text = ")
        )
        for mode, column in [
            ("argument", "callsite_argument_checks"),
            ("reference", "module_reference_checks"),
        ]:
            text = interface_overlay(fixture, files, mode)
            owner_path = root / owner
            before = owner_path.read_bytes()
            owner_path.write_text(text)
            try:
                lsp.document(owner, text)
                if owner != fixture["caller"]:
                    lsp.document(fixture["caller"], files[fixture["caller"]])
                snapshot = lsp.snapshot()
                dump(
                    output / (mode + "-overlay.json"),
                    {"document": owner, "text": text, "diagnostics": snapshot},
                )
                human = cli(
                    binaries / "carina", root, output, env, mode + "-human",
                    ["validate", "."],
                )
                report = cli(
                    binaries / "carina", root, output, env, mode + "-json",
                    ["validate", "--json", "."],
                )
            finally:
                owner_path.write_bytes(before)
            cells[column] = {
                "human": human,
                "json": report,
                "lsp_caller": caller_part(fixture, snapshot),
            }
        lsp.document(owner, files[owner])
        if owner != fixture["caller"]:
            lsp.document(fixture["caller"], files[fixture["caller"]])
        restored = lsp.snapshot()
        dump(output / "diagnostics.restored.json", restored)
        if restored != all_open:
            raise RuntimeError(f"{fixture['id']}: editor overlays were not restored")
    finally:
        lsp.finish()
    for column, data in cells.items():
        dump(output / (column + ".cell.json"), data)
    report = {
        "fixture": fixture["id"],
        "columns": sorted(cells),
        "lsp_exit": lsp.server.returncode,
        "readiness": ready,
        "counts": {name: len(value) for name, value in all_open.items()},
        "cli_exit": {
            name: cells[name]["exit"]
            for name in ["validate_human", "validate_json", "lint"]
        },
    }
    dump(output / "measurement.json", report)
    return report


def normalize(value, root):
    if isinstance(value, str):
        return value.replace(str(root), "$ROOT")
    if isinstance(value, list):
        return [normalize(item, root) for item in value]
    if isinstance(value, dict):
        return {
            normalize(key, root): normalize(item, root) for key, item in value.items()
        }
    return value


def refresh_main_baselines(output, expectations, revision):
    baselines = []
    for directory, expected in expectations:
        path = directory / "main.json"
        before = path.read_bytes()
        stored = json.loads(before)
        if not revision or stored["revision"] != revision:
            raise RuntimeError(f"{directory.name}: genuine main revision mismatch")
        raw = output / "raw" / directory.name / "main"
        root = output / "fixtures" / "main" / directory.name
        columns = {
            column: normalize(json.loads((raw / (column + ".cell.json")).read_text()), root)
            for column in expected["columns"]
            if column != "resolved_signature"
        }
        if columns.keys() != stored["columns"].keys():
            raise RuntimeError(f"{directory.name}: genuine main column mismatch")
        baselines.append((path, before, {"revision": revision, "columns": columns}))
    receipts = []
    for path, before, baseline in baselines:
        dump(path, baseline)
        receipts.append({
            "id": path.parent.name,
            "before_sha256": hashlib.sha256(before).hexdigest(),
            "after_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "columns": len(baseline["columns"]),
        })
    dump(output / "main-baseline-refresh.json", {"revision": revision, "fixtures": receipts})


def compare(output, expectations):
    records = []
    counts = {}
    markers = 0
    for directory, expected in expectations:
        ident = directory.name
        for column, cell in expected["columns"].items():
            if column == "resolved_signature":
                continue
            values = {}
            for phase in ["main", "head"]:
                path = output / "raw" / ident / phase / (column + ".cell.json")
                values[phase] = normalize(
                    json.loads(path.read_text()), output / "fixtures" / phase / ident
                )
            gap = "Unknown attribute 'absent' on 'result'." in json.dumps(cell["value"])
            markers += int(gap)
            equals_expected = values["head"] == cell["value"]
            if not equals_expected:
                classification = "unintentionally different"
            elif values["head"] == values["main"]:
                classification = "main-4485" if gap else "main"
            elif cell["classification"].startswith("changed:"):
                classification = cell["classification"]
            else:
                classification = "needs decision"
            counts[classification] = counts.get(classification, 0) + 1
            records.append(
                {
                    "id": ident + ":" + column,
                    "classification": classification,
                    "expected_classification": cell["classification"],
                    "ordinary_difference_marker": "#4485" if gap else None,
                    "equals_decided_value": equals_expected,
                }
            )
    for classification in [
        "main", "main-4485", "unintentionally different", "needs decision"
    ]:
        counts.setdefault(classification, 0)
    result = {"counts": counts, "4485_marker_count": markers, "records": records}
    dump(output / "comparison.json", result)
    print(json.dumps({"counts": counts, "4485_marker_count": markers}), flush=True)
    return bool(counts["unintentionally different"] or counts["needs decision"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--evidence", type=Path, required=True,
        help="saved bin/main and bin/head inputs",
    )
    parser.add_argument(
        "--output", type=Path, required=True,
        help="empty directory for all new output",
    )
    parser.add_argument("--phase", choices=["main", "head", "both"], default="both")
    parser.add_argument("--fixtures", type=Path, default=FIXTURES)
    parser.add_argument(
        "--refresh-main-baselines", action="store_true",
        help="replace main.json from measured main cells, retaining its verified revision",
    )
    args = parser.parse_args()
    if args.refresh_main_baselines and args.phase == "head":
        parser.error("--refresh-main-baselines requires a main measurement")
    output = args.output.resolve()
    fixture_root = args.fixtures.resolve()
    binary_root = args.evidence.resolve() / "bin"
    if fixture_root.is_relative_to(output) or binary_root.is_relative_to(output):
        parser.error(
            "--output must be separate from the fixture and saved binary inputs"
        )
    if output.exists() and (not output.is_dir() or any(output.iterdir())):
        parser.error("--output must be an empty directory")
    expectations = [
        (directory, json.loads((directory / "expected.json").read_text()))
        for directory in sorted(fixture_root.iterdir())
        if directory.is_dir()
    ]
    if not expectations:
        parser.error("--fixtures contains no fixture directories")
    if os.name != "posix" or os.geteuid() == 0:
        skipped = [directory.name for directory, expected in expectations
                   if "mode000" in expected["fixture"]["runtime_unreadable"].values()]
        expectations = [(directory, expected) for directory, expected in expectations
                        if directory.name not in skipped]
        if skipped:
            print("SKIP mode-000 fixtures: root can read mode 000; "
                  "an unprivileged Unix runner is required: " + ", ".join(skipped), flush=True)
    allowed = {"main", "main-4485"} | {"changed:" + key for key in DECISION_KEYS}
    for directory, expected in expectations:
        if expected["fixture"]["id"] != directory.name:
            parser.error(f"{directory}: fixture id differs from directory name")
        for column, cell in expected["columns"].items():
            if cell["classification"] not in allowed:
                parser.error(f"{directory.name}:{column}: unknown classification")
    phases = ["main", "head"] if args.phase == "both" else [args.phase]
    # Validate saved provenance before writing anything or launching a binary.
    provenance = {}
    for phase in phases:
        binaries = binary_root / phase
        provenance[phase] = json.loads((binaries / "artifacts.json").read_text())
        artifacts = provenance[phase]["bins"]
        if set(artifacts) != {"carina", "carina-lsp"}:
            parser.error(
                f"{phase}: expected saved carina and carina-lsp binary receipts"
            )
        for name, artifact in artifacts.items():
            actual = hashlib.sha256((binaries / name).read_bytes()).hexdigest()
            if actual != artifact["sha256"]:
                parser.error(f"{phase}/{name}: saved binary receipt mismatch")
    output.mkdir(parents=True, exist_ok=True)
    for phase in phases:
        results = []
        for directory, expected in expectations:
            fixture = expected["fixture"]
            root = output / "fixtures" / phase / fixture["id"]
            raw = output / "raw" / fixture["id"] / phase
            raw.mkdir(parents=True)
            start = time.monotonic()
            try:
                files = prepare_fixture(directory, fixture, root)
                env = clean_env(output, phase, fixture["id"])
                result = measure(binary_root / phase, root, raw, env, fixture, files)
                results.append(result)
                seconds = round(time.monotonic() - start, 2)
                print(phase, fixture["id"], seconds, flush=True)
            except Exception:
                (raw / "harness-error.txt").write_text(traceback.format_exc())
                raise
            finally:
                restore_permissions(root, fixture)
        dump(output / ("measure-" + phase + "-summary.json"), results)
    if args.refresh_main_baselines:
        refresh_main_baselines(output, expectations, provenance["main"].get("revision"))
    if args.phase == "both" and compare(output, expectations):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
