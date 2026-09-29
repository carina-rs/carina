---
title: fmt
---

Format `.crn` files. The formatter aligns attributes within each block and applies consistent indentation, matching the formatting the LSP applies on **Format Document**.

## Usage

```bash
carina fmt [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory — single-file paths are rejected. Without `--recursive`, only the `.crn` files directly inside PATH are formatted.

## Flags

### `--check`, `-c`

Exit with a non-zero status if any file would be reformatted. Does not modify files. Intended for CI.

### `--diff`

Print the formatting diff to stdout without rewriting any files. Combine with `--check` to also exit non-zero when any file would be reformatted.

### `--recursive`, `-r`

Recurse into subdirectories of PATH and include every `.crn` file found.

## Examples

Format every `.crn` file in the current directory:

```bash
carina fmt
```

Format the `.crn` files in another directory:

```bash
carina fmt infra/network
```

Format every `.crn` under the current tree:

```bash
carina fmt --recursive
```

Show the diff without writing changes:

```bash
carina fmt --diff
```

Verify formatting in CI:

```bash
carina fmt --check --recursive
```
