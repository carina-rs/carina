#!/usr/bin/env python3
"""Compare validate output for every tracked directory containing a .crn file."""

import argparse
from collections import Counter
import difflib
import json
import os
from pathlib import Path
import subprocess


# carina#4394: these fixtures vary in text order and JSON errors-array order.
ORDER_NONDETERMINISTIC_FIXTURES = {
    "carina-cli/tests/fixtures/value_knowledge/apply_operand_timing",
    "carina-cli/tests/fixtures/value_knowledge/guarded_failure_siblings",
}


def configuration_directories(root):
    if not root.is_dir():
        return []
    paths = subprocess.check_output(
        ["git", "-C", str(root), "ls-files", "-z", "--", "*.crn"]
    ).decode().split("\0")
    return sorted(
        {
            (root / name).parent
            for name in paths
            if name
        }
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--main", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--repo", default=Path.cwd(), type=Path)
    parser.add_argument("--infra", type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--list-only", action="store_true")
    args = parser.parse_args()
    roots = [args.repo.resolve()]
    if args.infra and args.infra.is_dir():
        roots.append(args.infra.resolve())
    directories = [directory for root in roots for directory in configuration_directories(root)]
    args.evidence.mkdir(parents=True, exist_ok=True)
    (args.evidence / "directories.json").write_text(json.dumps([str(p) for p in directories], indent=2) + "\n")
    if args.list_only:
        print(json.dumps({"directories": len(directories), "roots": [str(p) for p in roots]}))
        return 0

    # Both binaries get identical inputs, with no credential chain or metadata lookup.
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("AWS_") and not any(
            marker in key.upper() for marker in ("TOKEN", "PASSWORD", "SECRET", "ACCESS_KEY")
        )
    }
    environment.update({
        "AWS_EC2_METADATA_DISABLED": "true",
        "AWS_SHARED_CREDENTIALS_FILE": "/dev/null",
        "AWS_CONFIG_FILE": "/dev/null",
        "NO_COLOR": "1",
    })
    environment.pop("CLICOLOR_FORCE", None)

    def normalize(output):
        for index, root in enumerate(sorted(roots, key=lambda p: len(str(p)), reverse=True)):
            output = output.replace(str(root), f"<root-{index}>")
        return output

    results = []
    differences = []
    for index, directory in enumerate(directories):
        for json_output in (False, True):
            outputs = []
            for label, binary in (("main", args.main), ("candidate", args.candidate)):
                command = [str(binary.resolve()), "validate", str(directory)]
                if json_output:
                    command.append("--json")
                try:
                    run = subprocess.run(command, cwd=args.repo, env=environment, capture_output=True, timeout=120)
                    status, stdout, stderr = run.returncode, run.stdout.decode(), run.stderr.decode()
                except subprocess.TimeoutExpired as error:
                    status = 124
                    stdout = (error.stdout or b"").decode()
                    stderr = (error.stderr or b"").decode()
                stem = f"{index:04}-{'json' if json_output else 'text'}-{label}"
                (args.evidence / f"{stem}.stdout").write_text(stdout)
                (args.evidence / f"{stem}.stderr").write_text(stderr)
                outputs.append((status, normalize(stdout), normalize(stderr)))
            relative = directory.relative_to(roots[0]).as_posix() if directory.is_relative_to(roots[0]) else None
            comparable = outputs
            if relative in ORDER_NONDETERMINISTIC_FIXTURES:
                if json_output:
                    comparable = []
                    for code, out, err in outputs:
                        document = json.loads(out)
                        errors = Counter(json.dumps(error, sort_keys=True) for error in document.pop("errors")) if "errors" in document else None
                        comparable.append((code, json.dumps(document, sort_keys=True), errors, err))
                else:
                    comparable = [(code, Counter(out.splitlines(keepends=True)), Counter(err.splitlines(keepends=True))) for code, out, err in outputs]
            record = {"directory": str(directory), "json": json_output, "statuses": [o[0] for o in outputs], "identical": comparable[0] == comparable[1] and all(o[0] != 124 for o in outputs)}
            results.append(record)
            if not record["identical"]:
                differences.append(record)
                diff = "\n".join(difflib.unified_diff(repr(outputs[0]).splitlines(), repr(outputs[1]).splitlines(), fromfile="main", tofile="candidate"))
                (args.evidence / f"{index:04}-{'json' if json_output else 'text'}.diff").write_text(diff + "\n")
            print(f"{index + 1}/{len(directories)} {'json' if json_output else 'text'} {'PASS' if record['identical'] else 'DIFF'} {directory}", flush=True)
    summary = {"directories": len(directories), "comparisons": len(results), "invocations": 2 * len(results), "differences": differences, "results": results}
    (args.evidence / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return int(bool(differences))


if __name__ == "__main__":
    raise SystemExit(main())
