---
title: validate
---

Validate the configuration file without communicating with cloud providers. This checks syntax, type correctness, resource schema compliance, and module resolution.

## Usage

```bash
carina validate [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files — single-file paths are rejected.

## What It Checks

1. **Syntax** -- Parses all `.crn` files using the Carina grammar. Reports line and column for parse errors.
2. **Resource types** -- Validates that resource types exist in the provider schema (e.g., `aws.s3.Bucket`).
3. **Attribute types** -- Checks that attribute values match the expected types defined in the resource schema.
4. **Module resolution** -- Resolves `import` statements and validates module arguments.
5. **Unused bindings** -- Warns about `let` bindings that are never referenced. These can be replaced with anonymous resources.
6. **Duplicate attributes** -- Warns when the same attribute key appears multiple times in a resource block. The last value wins, but this is likely unintentional.

## Output

On success, Carina prints the number of validated resources and lists each resource ID:

```
Validating...
✓ 3 resources validated successfully.
  • aws.s3.Bucket.my-bucket
  • aws.ec2.Vpc.main
  • aws.ec2.Subnet.public
```

Warnings are printed after the success message:

```
⚠ Unused let binding 'temp'. Consider using an anonymous resource instead.
⚠ main.crn:Duplicate attribute 'tags' at line 12 (first defined on line 8). The last value will be used.
```

On failure, Carina prints the error and exits with code `1`.

## Flags

### `--json`

Output results as structured JSON instead of human-readable text.

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
    {"type": "unused_binding", "message": "Unused let binding 'temp'"},
    {"type": "duplicate_attribute", "message": "Duplicate attribute 'tags' at line 12", "file": "main.crn"}
  ]
}
```

## Examples

Validate the current directory:

```bash
carina validate
```

Validate a specific directory:

```bash
carina validate path/to/config
```
