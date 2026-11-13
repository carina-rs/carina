#!/usr/bin/env python3
"""Compare validate output for every tracked directory containing a .crn file."""

import argparse
import difflib
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time


def file_sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_revision(root):
    run = subprocess.run(["git", "-C", str(root), "rev-parse", "--verify", "HEAD"], capture_output=True)
    return run.stdout.decode().strip() if run.returncode == 0 else None


def configuration_directories(root):
    if not root.is_dir():
        raise ValueError(f"Configuration root is not a directory: {root}")
    paths = subprocess.check_output(
        ["git", "-C", str(root), "ls-files", "-z", "--", "*.crn"]
    ).decode().split("\0")
    missing = [name for name in paths if name and not (root / name).is_file()]
    if missing:
        raise ValueError(f"Missing tracked configuration inputs in {root}: {missing}")
    return sorted({(root / name).parent for name in paths if name})


def validation_output(status, stdout, stderr):
    try:
        result = json.loads(stdout)
    except (ValueError, UnicodeError):
        result = None
    messages = [stderr.decode(errors="replace")]
    if isinstance(result, dict):
        messages.extend(error.get("message", "") for error in result.get("errors", [])
                        if isinstance(error, dict))
    provider_missing = any(re.search(r"\bprovider\b[^\n]*\bnot installed\b", message, re.IGNORECASE)
                           for message in messages)
    structured = (isinstance(result, dict) and isinstance(result.get("resources"), list)
                  and isinstance(result.get("status"), str))
    return status in (0, 1) and structured and not provider_missing, provider_missing


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--main", type=Path)
    mode.add_argument("--binary", type=Path)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--repeat", type=int)
    parser.add_argument("--repo", default=Path.cwd(), type=Path)
    parser.add_argument("--infra", type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--list-only", action="store_true")
    parser.add_argument("--directory-manifest", type=Path,
                        help="Replay the complete directories.json from --list-only; fail if the corpus changed")
    parser.add_argument("--shard", metavar="INDEX/COUNT",
                        help="Compare one zero-based shard of the complete directory manifest")
    args = parser.parse_args()
    if args.binary:
        if args.candidate or args.repeat is None or args.repeat < 8:
            parser.error("--binary requires --repeat N (N >= 8) and excludes --candidate")
        binaries = [(f"repeat-{index + 1:02}", args.binary.resolve()) for index in range(args.repeat)]
    else:
        if args.candidate is None or args.repeat is not None or args.shard:
            parser.error("--main requires --candidate and excludes --repeat and --shard")
        binaries = [("main", args.main.resolve()), ("candidate", args.candidate.resolve())]
    started = time.monotonic()
    args.repo = args.repo.resolve()
    args.evidence = args.evidence.resolve()
    roots = [args.repo.resolve()]
    if args.infra and args.infra.is_dir():
        roots.append(args.infra.resolve())
    all_directories = [directory for root in roots for directory in configuration_directories(root)]
    if not all_directories:
        parser.error("No tracked configuration directories found")
    if args.directory_manifest:
        recorded = json.loads(args.directory_manifest.read_text())
        if recorded != [str(directory) for directory in all_directories]:
            parser.error("Directory manifest does not match the complete tracked corpus")
    shard_index, shard_count = 0, 1
    if args.shard:
        try:
            shard_index, shard_count = map(int, args.shard.split("/"))
        except ValueError:
            parser.error("--shard requires INDEX/COUNT")
        if not 0 <= shard_index < shard_count <= len(all_directories):
            parser.error("--shard requires 0 <= INDEX < COUNT <= number of directories")
    shards = [all_directories[index::shard_count] for index in range(shard_count)]
    if sorted(directory for shard in shards for directory in shard) != sorted(all_directories):
        parser.error("Shards must cover the directory manifest exactly once")
    directories = shards[shard_index]
    args.evidence.mkdir(parents=True, exist_ok=True)
    (args.evidence / "directories.json").write_text(json.dumps([str(p) for p in all_directories], indent=2) + "\n")
    (args.evidence / "shards.json").write_text(json.dumps([[str(p) for p in shard] for shard in shards], indent=2) + "\n")
    (args.evidence / "selected-directories.json").write_text(json.dumps([str(p) for p in directories], indent=2) + "\n")
    if args.list_only:
        print(json.dumps({"directories": len(directories), "total_directories": len(all_directories),
                          "roots": [str(p) for p in roots]}))
        return 0

    # Both binaries get identical inputs, with no credential chain or metadata lookup.
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("AWS_", "CARINA_")) and not any(
            marker in key.upper() for marker in ("TOKEN", "PASSWORD", "SECRET", "ACCESS_KEY")
        )
    }
    environment.update({
        "AWS_EC2_METADATA_DISABLED": "true",
        "AWS_SHARED_CREDENTIALS_FILE": "/dev/null",
        "AWS_CONFIG_FILE": "/dev/null",
        "NO_COLOR": "1",
        "LC_ALL": "C",
        "LANG": "C",
        "TZ": "UTC",
        "HOME": str(args.evidence / "home"),
        "CARINA_TEST_HOME": str(args.evidence / "home"),
        "TMPDIR": str(args.evidence / "tmp"),
        "CARINA_TRUST_RECORD_DIR": str(args.evidence / "home/.carina/trust"),
        "CARINA_PLUGIN_CACHE_DIR": str(args.evidence / "home/.carina/plugin-cache"),
    })
    for key in ("HOME", "TMPDIR", "CARINA_TRUST_RECORD_DIR", "CARINA_PLUGIN_CACHE_DIR"):
        Path(environment[key]).mkdir(parents=True, exist_ok=True)
    environment.pop("CLICOLOR_FORCE", None)
    environment.pop("RUST_LOG", None)

    manifest = {
        "mode": "same-binary" if args.binary else "main-candidate",
        "repeat": args.repeat,
        "shard": {"index": shard_index, "count": shard_count},
        "roots": [str(root) for root in roots],
        "cwd": str(args.repo),
        "script_sha256": file_sha256(Path(__file__).resolve()),
        "revisions": {str(root): git_revision(root) for root in roots},
        "binaries": {str(binary): file_sha256(binary)
                     for binary in dict.fromkeys(binary for _, binary in binaries)},
        "inputs": {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
                   for directory in all_directories for path in sorted(directory.glob("*.crn"))},
        "environment": {key: environment[key] for key in (
            "HOME", "CARINA_TEST_HOME", "TMPDIR", "LC_ALL", "LANG", "TZ", "NO_COLOR",
            "AWS_EC2_METADATA_DISABLED", "AWS_SHARED_CREDENTIALS_FILE", "AWS_CONFIG_FILE",
            "CARINA_TRUST_RECORD_DIR", "CARINA_PLUGIN_CACHE_DIR",
        )},
    }
    (args.evidence / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    def normalize(output):
        for index, root in enumerate(sorted(roots, key=lambda p: len(str(p)), reverse=True)):
            output = output.replace(str(root), f"<root-{index}>")
        return output

    results = []
    differences = []
    failures = []
    validated_directories = set()
    provider_not_installed = set()
    for index, directory in enumerate(directories):
        for json_output in (False, True):
            outputs = []
            case_failed = False
            validation_runs = []
            for label, binary in binaries:
                command = [str(binary), "validate", str(directory)]
                if json_output:
                    command.append("--json")
                try:
                    run = subprocess.run(command, cwd=args.repo, env=environment, stdin=subprocess.DEVNULL,
                                         capture_output=True, timeout=120)
                    status, stdout, stderr = run.returncode, run.stdout, run.stderr
                    failure = ("crash" if status < 0 or status >= 128 else
                               "unexpected exit status" if status not in (0, 1) else None)
                except subprocess.TimeoutExpired as error:
                    status = 124
                    stdout = error.stdout or b""
                    stderr = error.stderr or b""
                    failure = "timeout"
                except OSError as error:
                    status, stdout, stderr = 127, b"", str(error).encode()
                    failure = "launch failure"
                stem = f"{index:04}-{'json' if json_output else 'text'}-{label}"
                (args.evidence / f"{stem}.stdout").write_bytes(stdout)
                (args.evidence / f"{stem}.stderr").write_bytes(stderr)
                (args.evidence / f"{stem}.status").write_text(f"{status}\n")
                if json_output:
                    reached_validation, provider_missing = validation_output(status, stdout, stderr)
                    validation_runs.append(reached_validation)
                    if provider_missing:
                        provider_not_installed.add(str(directory))
                outputs.append((status, stdout, stderr) if args.binary else (
                    status, normalize(stdout.decode()), normalize(stderr.decode())))
                if failure:
                    case_failed = True
                    failures.append({"directory": str(directory), "json": json_output,
                                     "invocation": label, "failure": failure, "status": status})
            varying = [i for i in range(1, len(outputs)) if outputs[i] != outputs[0]]
            record = {"directory": str(directory), "json": json_output,
                      "statuses": [o[0] for o in outputs], "differing_repetitions": [i + 1 for i in varying],
                      "identical": not varying and not case_failed}
            results.append(record)
            if json_output and all(validation_runs):
                validated_directories.add(str(directory))
            if not record["identical"]:
                differences.append(record)
                for repetition in varying:
                    diff = [f"exit status: {outputs[0][0]} -> {outputs[repetition][0]}\n"]
                    for stream, offset in (("stdout", 1), ("stderr", 2)):
                        before, after = outputs[0][offset], outputs[repetition][offset]
                        if isinstance(before, bytes):
                            before, after = before.decode(errors="backslashreplace"), after.decode(errors="backslashreplace")
                        diff.extend(difflib.unified_diff(before.splitlines(keepends=True), after.splitlines(keepends=True),
                                                       fromfile=f"{binaries[0][0]}.{stream}",
                                                       tofile=f"{binaries[repetition][0]}.{stream}"))
                    (args.evidence / f"{index:04}-{'json' if json_output else 'text'}-{binaries[repetition][0]}.diff").write_text("".join(diff))
            print(f"{index + 1}/{len(directories)} {'json' if json_output else 'text'} {'PASS' if record['identical'] else 'DIFF'} {directory}", flush=True)
    if args.binary and not validated_directories:
        failures.append({"failure": "No directory reached validation in this shard"})
    diagnostic_order_directories = {str(directory) for directory in directories
                                    if "diagnostic_order" in directory.parts}
    unvalidated_fixtures = sorted(diagnostic_order_directories - validated_directories)
    if args.binary and unvalidated_fixtures:
        failures.append({"failure": "Named diagnostic-order fixtures did not reach validation",
                         "directories": unvalidated_fixtures})
    summary = {"directories": len(directories), "total_directories": len(all_directories),
               "validation_directories": len(validated_directories),
               "validated_directory_names": sorted(validated_directories),
               "diagnostic_order_directory_names": sorted(diagnostic_order_directories),
               "unvalidated_diagnostic_order_directories": unvalidated_fixtures,
               "setup_failure_directories": len(directories) - len(validated_directories),
               "provider_not_installed_directories": len(provider_not_installed),
               "shard": {"index": shard_index, "count": shard_count},
               "comparisons": (len(binaries) - 1) * len(results),
               "invocations": len(binaries) * len(results), "repeat": args.repeat,
               "varying_directories": len({record["directory"] for record in differences if record["differing_repetitions"]}),
               "wall_seconds": time.monotonic() - started,
               "differences": differences, "failures": failures, "results": results}
    (args.evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key not in (
        "differences", "results", "validated_directory_names", "diagnostic_order_directory_names")}))
    return int(bool(differences or failures))


if __name__ == "__main__":
    raise SystemExit(main())
