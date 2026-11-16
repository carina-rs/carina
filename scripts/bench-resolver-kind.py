#!/usr/bin/env python3
"""Compare existing Carina binaries offline; never build binaries or run plan.

Example (run separately from the Rust tests):
  python3 scripts/bench-resolver-kind.py --main /path/to/main/carina \
      --head /path/to/head/carina --config /path/to/config --out /path/to/results

Only validate is measured: plan can open state backends and contact providers.
The closure follows literal local source/import paths, preserving their relative
layout and project .carina directories. Absolute local sources and interpolated
local paths are refused rather than letting a subprocess use the original tree.
Already installed user trust/plugin artifacts are copied from
CARINA_RESOLVER_KIND_HOME (default: the caller's home). No init or download runs.
Build/toolchain/features may be described in CARINA_RESOLVER_KIND_BUILD_INFO.
All writes, subprocess logs, HOME, caches and temporary files live under --out.
No configuration or command output is printed.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import statistics
import subprocess
import tempfile
import time


WARMUPS = 5
PAIRS = 30
# Keep strings intact while removing comments, so commented sources are ignored.
TOKEN = re.compile(r"'(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\"|//[^\n]*|#[^\n]*|/\*.*?\*/", re.S)
LOCAL_SOURCE = re.compile(r"\b(?:source\s*=|import\s+|use\s+)\s*('(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\")")


class Unavailable(Exception):
    """A safe, content-free reason that a measurement cannot be used."""


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def git_value(directory, *arguments):
    result = subprocess.run(
        ["git", "-C", str(directory), "rev-parse", *arguments],
        capture_output=True, timeout=10, check=False,
    )
    return result.stdout.decode().strip() if result.returncode == 0 else None


def literal(token):
    if token.startswith('"'):
        return json.loads(token)
    return token[1:-1].replace("\\'", "'").replace("\\\\", "\\")


def closure(config):
    pending, directories, files = [config], set(), set()
    while pending:
        directory = pending.pop()
        if directory in directories:
            continue
        directories.add(directory)
        for source in directory.rglob("*.crn"):
            if any(part in {".git", ".carina", "target"} for part in source.relative_to(directory).parts):
                continue
            text = TOKEN.sub(lambda match: match[0] if match[0][0] in "'\"" else "", source.read_text())
            for match in LOCAL_SOURCE.finditer(text):
                value = literal(match[1])
                if "${" in value:
                    raise Unavailable("closure contains an interpolated source")
                if value.startswith("file://"):
                    value = value[7:]
                elif not value.startswith((".", "/")) and not match[0].startswith(("import", "use")):
                    continue  # A remote provider source, not a local module.
                if Path(value).is_absolute():
                    raise Unavailable("closure contains an interpolated or absolute local source")
                target = (source.parent / value).resolve()
                if not target.exists():
                    raise Unavailable("a local source in the configuration closure is missing")
                if target.is_dir():
                    pending.append(target)
                else:
                    files.add(target)
    checkout = git_value(config, "--show-toplevel")
    boundary = Path(checkout).resolve() if checkout else Path(os.path.commonpath(directories))
    # Provider artifacts/locks can belong to an ancestor project, not the module.
    for directory in list(directories):
        for parent in (directory, *directory.parents):
            if not parent.is_relative_to(boundary):
                break
            if (parent / ".carina").is_dir():
                directories.add(parent / ".carina")
            lock = parent / "carina-providers.lock"
            if lock.is_file():
                files.add(lock)
            if parent == boundary:
                break
    return directories, files, checkout


def copy_configuration(config, out, scratch):
    directories, files, checkout = closure(config)
    sources = directories | files
    if any(out.is_relative_to(path) for path in directories):
        raise Unavailable("--out must be outside the configuration closure")
    anchor = Path(os.path.commonpath([str(path.parent) for path in sources]))
    destination = scratch / "configuration"
    destination.mkdir()
    for source in sorted(directories, key=lambda path: (len(path.parts), str(path))):
        target = destination / source.relative_to(anchor)
        if target.exists():
            continue
        shutil.copytree(source, target,
                        symlinks=False, ignore=shutil.ignore_patterns(".git", "target", "__pycache__"))
    for source in files:
        target = destination / source.relative_to(anchor)
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
    records = []
    for source in sorted(destination.rglob("*")):
        if source.is_file():
            records.append({"path": str(source.relative_to(destination)),
                            "bytes": source.stat().st_size, "sha256": digest(source)})
    return destination / config.relative_to(anchor), {
        "checkout_sha": git_value(checkout or config, "HEAD"),
        "closure_directories": len(directories),
        "input_root_files": len(list(config.glob("*.crn"))),
        "input_root_file_bytes": sum(path.stat().st_size for path in config.glob("*.crn")),
        "crn_files": sum(record["path"].endswith(".crn") for record in records),
        "files": records,
    }


def isolated_environment(scratch):
    home, temporary = scratch / "home", scratch / "tmp"
    home.mkdir()
    temporary.mkdir()
    original = Path(os.environ.get("CARINA_RESOLVER_KIND_HOME", str(Path.home())))
    artifacts = []
    for name, override in (("trust", "CARINA_TRUST_RECORD_DIR"), ("plugin-cache", "CARINA_PLUGIN_CACHE_DIR")):
        source = Path(os.environ.get(override, str(original / ".carina" / name)))
        target = home / ".carina" / name
        if source.is_dir():
            shutil.copytree(source, target, symlinks=False)
            artifacts.extend({"path": str(path.relative_to(home)), "sha256": digest(path)}
                             for path in sorted(target.rglob("*")) if path.is_file())
    env = os.environ.copy()
    credential_variables = {
        "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN",
        "AWS_PROFILE", "AWS_DEFAULT_PROFILE", "AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_ROLE_ARN", "AWS_ROLE_SESSION_NAME",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN", "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    }
    for key in list(env):
        if key in credential_variables or key in {
            "CARINA_TEST_HOME", "NEXTEST_RUN_ID", "CARINA_TRUST_RECORD_DIR", "CARINA_PLUGIN_CACHE_DIR",
        }:
            del env[key]
    env.update(HOME=str(home), TMPDIR=str(temporary), TMP=str(temporary), TEMP=str(temporary),
               AWS_EC2_METADATA_DISABLED="true", AWS_CONFIG_FILE=str(home / "absent-aws-config"),
               AWS_SHARED_CREDENTIALS_FILE=str(home / "absent-aws-credentials"),
               NO_COLOR="1", CLICOLOR="0")
    return env, artifacts


def normalize(document, scratch):
    if isinstance(document, dict):
        return {key.replace(str(scratch), "<SCRATCH>"): normalize(value, scratch)
                for key, value in document.items()}
    if isinstance(document, list):
        return [normalize(value, scratch) for value in document]
    return document.replace(str(scratch), "<SCRATCH>") if isinstance(document, str) else document


def output_digest(document):
    # JSON object order is immaterial; arrays retain their order everywhere.
    return hashlib.sha256(json.dumps(document, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def run(binary, config, scratch, env, log, expected):
    cpu_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter_ns()
    try:
        result = subprocess.run([str(binary), "validate", str(config), "--json"], cwd=config,
                                env=env, capture_output=True, timeout=180, check=False)
    except subprocess.TimeoutExpired as error:
        log.with_suffix(".stdout").write_bytes(error.stdout or b"")
        log.with_suffix(".stderr").write_bytes(error.stderr or b"")
        raise Unavailable("validate timed out; private command output is under --out") from error
    wall_ms = (time.perf_counter_ns() - started) / 1_000_000
    cpu_after = resource.getrusage(resource.RUSAGE_CHILDREN)
    sample = {"wall_ms": wall_ms, "user_ms": (cpu_after.ru_utime - cpu_before.ru_utime) * 1000,
              "system_ms": (cpu_after.ru_stime - cpu_before.ru_stime) * 1000,
              "status": result.returncode, "log": str(log.relative_to(scratch.parent))}
    # Preserve timings/statuses even if a later output check makes the run unavailable.
    with (scratch / "runs.jsonl").open("a") as stream:
        stream.write(json.dumps(sample) + "\n")
    log.with_suffix(".stdout").write_bytes(result.stdout)
    log.with_suffix(".stderr").write_bytes(result.stderr)
    if result.returncode != 0:
        raise Unavailable(f"validate exited {result.returncode}; private command output is under --out")
    try:
        document = normalize(json.loads(result.stdout), scratch)
    except (UnicodeDecodeError, ValueError) as error:
        raise Unavailable("validate did not produce JSON; private command output is under --out") from error
    if not isinstance(document, dict) or document.get("status") != "ok" or document.get("resource_count", 0) <= 0:
        raise Unavailable("validate did not reach successful nonzero resource coverage")
    fingerprint = output_digest(document)
    if expected is not None and fingerprint != expected:
        raise Unavailable("validate JSON differs between runs or binaries; no ordering was suppressed")
    return {**sample, "json_sha256": fingerprint, "resource_count": document["resource_count"]}


def measure(label, left, right, config, scratch, env):
    log_dir = scratch / label
    log_dir.mkdir()
    expected, warmups, pairs = None, [], []
    for number in range(WARMUPS):
        for lane, binary in (("main", left), ("candidate", right)):
            sample = run(binary, config, scratch, env, log_dir / f"warmup-{number}-{lane}", expected)
            expected = sample["json_sha256"]
            warmups.append({"lane": lane, **sample})
    for number in range(PAIRS):
        lanes = [("main", left), ("candidate", right)]
        if number % 2:
            lanes.reverse()
        pair = {"order": [lane for lane, _ in lanes]}
        for lane, binary in lanes:
            pair[lane] = run(binary, config, scratch, env, log_dir / f"pair-{number:02}-{lane}", expected)
        pairs.append(pair)
    stats = {}
    for lane in ("main", "candidate"):
        values = sorted(pair[lane]["wall_ms"] for pair in pairs)
        stats[lane] = {"median_ms": statistics.median(values), "p95_ms": values[math.ceil(0.95 * len(values)) - 1]}
    delta = {}
    for metric in ("median", "p95"):
        baseline, candidate = stats["main"][metric + "_ms"], stats["candidate"][metric + "_ms"]
        delta[metric + "_ms"] = candidate - baseline
        delta[metric + "_percent"] = (candidate / baseline - 1) * 100
    regression = (delta["median_percent"] > 5 and delta["median_ms"] > 5) or (
        delta["p95_percent"] > 10 and delta["p95_ms"] > 10)
    return {"warmups": warmups, "pairs": pairs, "statistics": stats, "delta": delta,
            "verdict": "regression" if regression else "within_thresholds"}


def markdown(result):
    lines = ["validate only; plan excluded because it may contact providers or state backends.", "",
             "| Experiment | Main median ms | Candidate median ms | Change % | Main p95 ms | Candidate p95 ms | Change % | Verdict |",
             "|---|---:|---:|---:|---:|---:|---:|---|"]
    for name, experiment in result.get("experiments", {}).items():
        left, right, delta = experiment["statistics"]["main"], experiment["statistics"]["candidate"], experiment["delta"]
        lines.append(f"| {name} | {left['median_ms']:.3f} | {right['median_ms']:.3f} | {delta['median_percent']:.2f} | "
                     f"{left['p95_ms']:.3f} | {right['p95_ms']:.3f} | {delta['p95_percent']:.2f} | {experiment['verdict']} |")
    lines.extend(["", f"Verdict: {result['verdict']}.",
                  "Regression: median >5% and >5 ms, or p95 >10% and >10 ms. Noise is reported, never subtracted.",
                  "Each experiment: 5 warmups per lane, then 30 pairs reversing order each pair; p95 uses nearest rank."])
    if "reason" in result:
        lines.extend(["", result["reason"]])
    if "noise_floor" in result:
        noise = result["noise_floor"]
        lines.extend(["", f"Main-versus-main absolute differences: median {noise['median_ms']:.3f} ms / "
                      f"{noise['median_percent']:.2f}%; p95 {noise['p95_ms']:.3f} ms / {noise['p95_percent']:.2f}%."])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("main", "head", "config", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    config, out = args.config.resolve(), args.out.resolve()
    result = {"command": "validate --json", "plan": "excluded: providers/state backends may require network or AWS",
              "warmups_per_lane": WARMUPS, "alternating_pairs": PAIRS, "experiments": {},
              "cache_mode": "copied trust/plugins; shared isolated compiled cache, warmed per lane",
              "build_info": os.environ.get("CARINA_RESOLVER_KIND_BUILD_INFO", "not supplied; binary hashes recorded")}
    writable_output = False
    try:
        binaries = {name: getattr(args, name).resolve() for name in ("main", "head")}
        if not config.is_dir() or any(not binary.is_file() or not os.access(binary, os.X_OK) for binary in binaries.values()):
            raise Unavailable("configuration directory or executable binary is missing")
        checkout = git_value(config, "--show-toplevel")
        script_checkout = Path(__file__).resolve().parent.parent
        directories, _, _ = closure(config)
        if (out.is_relative_to(script_checkout) or any(out.is_relative_to(path) for path in directories)
                or (checkout and out.is_relative_to(Path(checkout).resolve()))):
            raise Unavailable("--out must be outside the source checkout")
        out.mkdir(parents=True, exist_ok=True)
        writable_output = True
        os.chmod(out, 0o700)
        result["binaries"] = {name: {"path": str(binary), "sha256": digest(binary)} for name, binary in binaries.items()}
        scratch = Path(tempfile.mkdtemp(prefix="resolver-kind-", dir=out))
        copied, result["configuration"] = copy_configuration(config, out, scratch)
        env, result["user_artifacts"] = isolated_environment(scratch)
        result["scratch"] = str(scratch)
        result["experiments"]["main_vs_main_noise"] = measure("main-vs-main", binaries["main"], binaries["main"], copied, scratch, env)
        result["noise_floor"] = {key: abs(value) for key, value in result["experiments"]["main_vs_main_noise"]["delta"].items()}
        result["experiments"]["main_vs_head"] = measure("main-vs-head", binaries["main"], binaries["head"], copied, scratch, env)
        result["verdict"] = result["experiments"]["main_vs_head"]["verdict"]
    except (Unavailable, OSError, ValueError, subprocess.SubprocessError) as error:
        # Exception text from the OS can contain paths, but never print source or CLI output.
        result.update(verdict="unavailable", reason=str(error) if isinstance(error, Unavailable) else type(error).__name__)
    table = markdown(result)
    if writable_output:
        (out / "resolver-kind.json").write_text(json.dumps(result, indent=2) + "\n")
        (out / "resolver-kind.md").write_text(table)
    print(table, end="")
    return {"within_thresholds": 0, "regression": 1, "unavailable": 2}[result["verdict"]]


if __name__ == "__main__":
    raise SystemExit(main())
