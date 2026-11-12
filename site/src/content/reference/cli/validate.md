---
title: validate
---

Validate configuration syntax, types, schemas, and module resolution without provider resource operations. Configurations using `decrypt()` can still contact AWS KMS during expression evaluation.

## Usage

```bash
carina validate [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files — single-file paths are rejected.

## What It Checks

1. **Syntax** -- Parses all `.crn` files using the Carina grammar. Reports line and column for parse errors.
2. **Resource types** -- Validates that resource types exist in the provider schema (e.g., `aws.s3.Bucket`).
3. **Attribute types** -- Checks that attribute values match the expected types defined in the resource schema.
4. **Module resolution** -- Resolves `let module_name = use { source = "..." }` module declarations and validates module-call arguments.
5. **Unused bindings** -- Warns about unused resource bindings that can safely become anonymous resources. Ordinary value bindings and bindings required for structural expressions or stable resource identities are excluded. A binding named as the `to` of an `import` or `moved` block counts as referenced, since those blocks match the resource by its binding.
6. **Duplicate attributes** -- Warns when the same attribute key appears multiple times in a resource block. The last value wins, but this is likely unintentional. Inside `secret(...)`, duplicate keys are redacted and the warning points to the outer `secret` token.

## Output

On success, Carina prints the number of validated resources and lists each resource ID:

```
Validating...
✓ 3 resources validated successfully.
  • aws.s3.Bucket.my-bucket
  • aws.ec2.Vpc.main
  • aws.ec2.Subnet.public
```

### Resources inside a `for` loop over an unresolved value

<!-- derived-from ../../../../../notes/specs/2026-05-17-for-loop-same-config-read-iterable-design.md -->

When a `for` loop iterates a value that is not known until plan/apply
time (for example `for opt in cert.domain_validation_options` over a
provider-computed list), the loop body is enumerated as a
single **deferred placeholder** entry rather than expanded:

```
✓ 2 resources validated successfully.
  • aws.acm.Certificate.cert
  • aws.route53.RecordSet.<pending> (deferred for at records.crn:5)
```

Anonymous loop bodies use `<pending> (deferred for at <file>:<line>)`, as above.
Bound loops use `<binding>[?] (deferred: <for header> @ <file>:<line>)`;
`[?]` marks an address the planner will assign once the iterable resolves.
`<file>` is the configuration-relative source filename (with a basename fallback),
such as `records.crn`; `<line>` identifies the loop in that file. Both forms are
placeholders. These entries are **not** resolvable resource
addresses -- they describe a loop body, not a single resource. The
count includes one entry per deferred loop (its element count is
unknowable at validate time). The same entries appear in the
`resources` array of `--json` output.

Warnings are printed after the success message:

```
⚠ Unused let binding 'temp'. Consider using an anonymous resource instead.
⚠ main.crn:Duplicate attribute 'tags' at line 12 (first defined on line 8). The last value will be used.
```

On failure, Carina prints the error and exits with code `1`.

## Flags

### `--json`

Write one JSON document to stdout on success or failure. Diagnostics are included in that document; stderr is empty for reported validation findings. Exit code `0` means validation succeeded (including warnings), and `1` means validation failed.

```bash
carina validate --json
```

```json
{
  "status": "ok",
  "resource_count": 3,
  "resources": [
    "aws.s3.Bucket.my-bucket",
    "aws.ec2.Vpc.main",
    "aws.ec2.Subnet.public"
  ]
}
```

When warnings are present, they are included in the output:

```json
{
  "status": "ok",
  "resource_count": 2,
  "resources": ["aws.s3.Bucket.my-bucket", "aws.ec2.Vpc.main"],
  "warnings": [
    {
      "message": "Unused let binding 'temp'",
      "severity": "warning",
      "code": null,
      "location": {"file": "main.crn", "range": null},
      "type": "unused_binding"
    },
    {
      "message": "Duplicate attribute 'tags' at line 12 (first defined on line 8). The last value will be used.",
      "severity": "warning",
      "code": null,
      "location": {
        "file": "main.crn",
        "range": {
          "start": {"line": 12, "column": 3},
          "end": {"line": 12, "column": 7}
        }
      },
      "type": "duplicate_attribute",
      "file": "main.crn"
    }
  ]
}
```

Failures have `"status": "error"`, an empty resource inventory, and a nonempty `errors` array. Independent warnings, including duplicate attributes in readable files, remain available when another check fails. Human failure output continues to omit these lint warnings:

```json
{
  "status": "error",
  "resource_count": 0,
  "resources": [],
  "errors": [
    {
      "message": "Type mismatch: expected String, got Bool",
      "severity": "error",
      "code": null,
      "location": {
        "file": "main.crn",
        "range": {
          "start": {"line": 3, "column": 3},
          "end": {"line": 3, "column": 7}
        }
      }
    }
  ]
}
```

Each diagnostic contains `message`, `severity`, `code`, and `location`. Existing warning messages keep their original JSON wording, with secret duplicate keys replaced by `<redacted>`. Other diagnostics use safe human diagnostic text. Locations identify the source file and, when available, its range. Lines and columns start at `1`; columns count Unicode scalar values, and range ends are exclusive. A file-only location has `"range": null`. Unavailable codes and locations are `null`.

Empty `errors` and `warnings` arrays are omitted. Warning `type` values such as `input_rule`, `unused_binding`, and `duplicate_attribute` remain available; duplicate-attribute warnings also retain the legacy `file` field. Provider and parser warnings use the same diagnostic fields. Only safe diagnostic text is serialized, without source snippets or provider payloads.

Informational loader notices, such as local-provider trust decisions, lock-format migration, and precompile-cache fallback, appear only in human output. They do not write to stderr in JSON mode.

If a file cannot be parsed and contains `secret` followed by anything other than an ASCII letter, digit, or underscore (or followed by the end of the file), diagnostics protect the whole file. Human and editor output show only the parse error kind and position; JSON carries the kind and structured location. Source excerpts and content-based lints are suppressed for that file. Parseable files and other syntax failures keep their usual diagnostics.

## Examples

Validate the current directory:

```bash
carina validate
```

Validate a specific directory:

```bash
carina validate path/to/config
```
