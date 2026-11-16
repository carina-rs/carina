#!/usr/bin/env python3
"""Compare existing Carina binaries offline; never build binaries or run plan.

Example (run separately from the Rust tests):
  python3 scripts/bench-resolver-kind.py --main /path/to/main/carina \
      --head /path/to/head/carina --config /path/to/config --out /path/to/results

Only validate is measured: plan can open state backends and contact providers.
The closure follows literal local source/import paths, preserving their relative
layout. Each directory entry includes every .crn beneath it, excluding artifact
and build directories. Only .crn files and provider/backend lock files are copied.
Absolute local sources and interpolated
local paths are refused rather than letting a subprocess use the original tree.
Installed trust/plugin directories are shared through explicit directory overrides;
no .carina or state files are copied. Credentials are removed from the inherited
environment; HOME/cache and temporary files are placed under --out. Other
environment variables are retained. No init or download runs.
Build/toolchain/features may be described in CARINA_RESOLVER_KIND_BUILD_INFO.
Subprocess logs and generated output live under --out.
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
import traceback


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


LOCK_FILES = ("carina-providers.lock", "carina-backend.lock")
EXCLUDED_DIRECTORIES = {".git", ".carina", "target", "__pycache__"}


def closure(config):
    pending, visited, directories, files = [config], set(), set(), set()
    while pending:
        entry = pending.pop()
        if entry in visited:
            continue
        visited.add(entry)
        if entry.is_dir():
            directories.add(entry)
            sources = (source for source in entry.rglob("*.crn")
                       if not any(part in EXCLUDED_DIRECTORIES for part in source.relative_to(entry).parts))
        else:
            if entry.suffix != ".crn":
                raise Unavailable("a local source is not a .crn file")
            directories.add(entry.parent)
            sources = [entry]
        for source in sources:
            if source in files:
                continue
            files.add(source)
            text = TOKEN.sub(lambda match: match[0] if match[0][0] in "'\"" else "", source.read_text())
            for match in LOCAL_SOURCE.finditer(text):
                value = literal(match[1])
                if "${" in value:
                    raise Unavailable("closure contains an interpolated source")
                if value.startswith("file://"):
                    value = value[7:]
                elif not value.startswith((".", "/")) and not match[0].startswith(("import", "use")):
                    continue
                if Path(value).is_absolute():
                    raise Unavailable("closure contains an absolute local source")
                if ".carina" in Path(value).parts:
                    raise Unavailable("a local source requires an excluded .carina directory")
                target = (source.parent / value).resolve()
                if not target.exists():
                    raise Unavailable("a local source in the configuration closure is missing")
                pending.append(target)
    checkout = git_value(config, "--show-toplevel")
    boundary = Path(checkout).resolve() if checkout else Path(os.path.commonpath(directories))
    for directory in directories:
        for parent in (directory, *directory.parents):
            if not parent.is_relative_to(boundary):
                break
            for name in LOCK_FILES:
                lock = parent / name
                if lock.is_file():
                    files.add(lock)
            if parent == boundary:
                break
    return directories, files, checkout


def copy_configuration(config, out, scratch):
    directories, files, checkout = closure(config)
    if any(out.is_relative_to(path) for path in directories):
        raise Unavailable("--out must be outside the configuration closure")
    anchor = Path(os.path.commonpath([str(path.parent) for path in files] + [str(config.parent)]))
    destination = scratch / "configuration"
    destination.mkdir()
    records = []
    for source in sorted(files):
        target = destination / source.relative_to(anchor)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        records.append({"path": str(target.relative_to(destination)),
                        "bytes": target.stat().st_size, "sha256": digest(target)})
    return destination / config.relative_to(anchor), {
        "checkout_sha": git_value(checkout or config, "HEAD"),
        "closure_directories": len(directories),
        "input_root_files": len(list(config.glob("*.crn"))),
        "input_root_file_bytes": sum(path.stat().st_size for path in config.glob("*.crn")),
        "crn_files": sum(record["path"].endswith(".crn") for record in records),
        "files": records,
    }


def credentials_removed_environment(scratch):
    home, temporary = scratch / "home", scratch / "tmp"
    home.mkdir()
    temporary.mkdir()
    original = Path.home()
    artifact_directories = {
        "trust": os.environ.get("CARINA_TRUST_RECORD_DIR", str(original / ".carina/trust")),
        "plugin_cache": os.environ.get("CARINA_PLUGIN_CACHE_DIR", str(original / ".carina/plugin-cache")),
    }
    env = os.environ.copy()
    credential_variables = {
        "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN",
        "AWS_PROFILE", "AWS_DEFAULT_PROFILE", "AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_ROLE_ARN", "AWS_ROLE_SESSION_NAME",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_AUTHORIZATION_TOKEN", "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    }
    for key in list(env):
        if (key in credential_variables or key.upper().endswith(("_TOKEN", "_KEY"))
                or "_SECRET" in key.upper() or key in {"CARINA_TEST_HOME", "NEXTEST_RUN_ID"}):
            del env[key]
    env.update(HOME=str(home), TMPDIR=str(temporary), TMP=str(temporary), TEMP=str(temporary),
               CARINA_TRUST_RECORD_DIR=artifact_directories["trust"],
               CARINA_PLUGIN_CACHE_DIR=artifact_directories["plugin_cache"],
               AWS_EC2_METADATA_DISABLED="true", AWS_CONFIG_FILE=str(home / "absent-aws-config"),
               AWS_SHARED_CREDENTIALS_FILE=str(home / "absent-aws-credentials"),
               NO_COLOR="1", CLICOLOR="0")
    return env, artifact_directories


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


def exceeds_thresholds(delta):
    return (delta["median_percent"] > 5 and delta["median_ms"] > 5) or (
        delta["p95_percent"] > 10 and delta["p95_ms"] > 10)


def paired_statistics(pairs):
    values = {lane: [pair[lane]["wall_ms"] for pair in pairs] for lane in ("main", "candidate")}
    if not pairs or any(not math.isfinite(value) or value <= 0 for lane in values.values() for value in lane):
        raise ValueError("paired timings must be nonempty, positive and finite")
    def stats(samples):
        return {"median_ms": statistics.median(samples),
                "p95_ms": sorted(samples)[math.ceil(0.95 * len(samples)) - 1]}
    lane_stats = {lane: stats(samples) for lane, samples in values.items()}
    differences = [pair["candidate"]["wall_ms"] - pair["main"]["wall_ms"] for pair in pairs]
    delta = stats(differences)
    baseline = lane_stats["main"]["median_ms"]
    for metric in ("median", "p95"):
        delta[metric + "_percent"] = delta[metric + "_ms"] / baseline * 100
    return {"statistics": lane_stats, "delta": delta,
            "verdict": "regression" if exceeds_thresholds(delta) else "within_thresholds"}


def comparison_verdict(experiments):
    # Both experiments use signed candidate-minus-main paired differences.
    # Noise has no preferred direction; threshold its summary magnitudes.
    noise = {key: abs(value) for key, value in experiments["main_vs_main_noise"]["delta"].items()}
    return ("inconclusive" if exceeds_thresholds(noise)
            else experiments["main_vs_head"]["verdict"]), noise


def measure_interleaved(binaries, config, scratch, env):
    experiments = {name: {"warmups": [], "pairs": []}
                   for name in ("main_vs_main_noise", "main_vs_head")}
    for name in experiments:
        (scratch / name).mkdir()
    expected = None
    for stage, count in (("warmup", WARMUPS), ("pair", PAIRS)):
        for number in range(count):
            # One main/main pair, then one main/head pair, throughout sampling.
            for name, experiment in experiments.items():
                candidate = binaries["main" if name == "main_vs_main_noise" else "head"]
                lanes = [("main", binaries["main"]), ("candidate", candidate)]
                if number % 2:
                    lanes.reverse()
                pair = {"order": [lane for lane, _ in lanes]}
                for lane, binary in lanes:
                    pair[lane] = run(binary, config, scratch, env,
                                     scratch / name / f"{stage}-{number:02}-{lane}", expected)
                    expected = pair[lane]["json_sha256"]
                experiment["warmups" if stage == "warmup" else "pairs"].append(pair)
    for experiment in experiments.values():
        experiment.update(paired_statistics(experiment["pairs"]))
    return experiments


def markdown(result):
    lines = ["validate only; plan excluded because it may contact providers or state backends.", "",
             "| Experiment | Main median ms | Candidate median ms | Paired median delta ms | Delta % | Paired p95 delta ms | Delta % | Verdict |",
             "|---|---:|---:|---:|---:|---:|---:|---|"]
    for name, experiment in result.get("experiments", {}).items():
        left, right, delta = experiment["statistics"]["main"], experiment["statistics"]["candidate"], experiment["delta"]
        lines.append(f"| {name} | {left['median_ms']:.3f} | {right['median_ms']:.3f} | {delta['median_ms']:.3f} | {delta['median_percent']:.2f} | "
                     f"{delta['p95_ms']:.3f} | {delta['p95_percent']:.2f} | {experiment['verdict']} |")
    lines.extend(["", f"Verdict: {result['verdict']}.",
                  "Regression: paired median >5% and >5 ms, or paired p95 >10% and >10 ms; percentages use the main lane's median.",
                  "Main/main noise uses the same paired statistics; a summary magnitude exceeding a threshold makes the result inconclusive.",
                  "5 warmup pairs and 30 measured pairs per experiment, interleaved main/main then main/head; lane order reverses each pair; p95 uses nearest rank.",
                  "Credentials removed; other environment is inherited, with temporary HOME/cache and shared installed trust/plugin directories."])
    if "reason" in result:
        lines.extend(["", result["reason"]])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("main", "head", "config", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    config, out = args.config.resolve(), args.out.resolve()
    result = {"command": "validate --json", "plan": "excluded: providers/state backends may require network or AWS",
              "warmups_per_lane": WARMUPS, "alternating_pairs": PAIRS, "experiments": {},
              "cache_mode": "temporary compiled cache; shared installed trust/plugins; credentials removed",
              "sampling": "interleaved main/main and main/head pairs",
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
        try:
            out.mkdir(parents=True)
        except FileExistsError:
            if not out.is_dir():
                raise Unavailable("--out is not a directory")
        else:
            os.chmod(out, 0o700)
        writable_output = True
        result["binaries"] = {name: {"path": str(binary), "sha256": digest(binary)} for name, binary in binaries.items()}
        scratch = Path(tempfile.mkdtemp(prefix="resolver-kind-", dir=out))
        copied, result["configuration"] = copy_configuration(config, out, scratch)
        env, result["artifact_directories"] = credentials_removed_environment(scratch)
        result["scratch"] = str(scratch)
        result["experiments"] = measure_interleaved(binaries, copied, scratch, env)
        result["verdict"], result["noise_floor"] = comparison_verdict(result["experiments"])
        if result["verdict"] == "inconclusive":
            result["experiments"]["main_vs_main_noise"]["verdict"] = "inconclusive"
    except (Unavailable, OSError, ValueError, subprocess.SubprocessError) as error:
        # Exception text from the OS can contain paths, but never print source or CLI output.
        result.update(verdict="unavailable", reason=str(error) if isinstance(error, Unavailable) else type(error).__name__)
    table = markdown(result)
    if writable_output:
        (out / "resolver-kind.json").write_text(json.dumps(result, indent=2) + "\n")
        (out / "resolver-kind.md").write_text(table)
    print(table, end="")
    return {"within_thresholds": 0, "regression": 1, "unavailable": 4, "inconclusive": 3}[result["verdict"]]


if __name__ == "__main__":
    try:
        exit_code = main()
    except Exception:
        traceback.print_exc()
        exit_code = 4
    raise SystemExit(exit_code)
