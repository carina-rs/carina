---
title: export
---

Print export values from the current state. Exports are values declared in an `exports` block; they are also the values consumed by other projects via `upstream_state`.

## Usage

```bash
carina export [OPTIONS] [NAME]
```

**NAME** is optional. When omitted, every export is printed. When set, only the named export is printed.

## Flags

### `--json`

Output as JSON. With a NAME, prints `{"<name>": <value>}`. Without, prints the full export map.

### `--raw`

Output the raw value without the export name or surrounding quotes. Requires a NAME. Useful for piping a single export into another command.

## Examples

Print every export:

```bash
carina export
```

Print a single export:

```bash
carina export vpc_id
```

Pipe a raw value into another tool:

```bash
aws ec2 describe-subnets --filters "Name=vpc-id,Values=$(carina export --raw vpc_id)"
```

Emit the full export map as JSON:

```bash
carina export --json
```
