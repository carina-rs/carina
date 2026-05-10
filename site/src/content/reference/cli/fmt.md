---
title: fmt
---

Format `.crn` files. The formatter aligns attributes within each block and applies consistent indentation, matching the formatting the LSP applies on **Format Document**.

## Usage

```bash
carina fmt [OPTIONS] [PATH]
```

**PATH** defaults to `.` (current directory). May be a single `.crn` file or a directory.

## Flags

### `--check`, `-c`

Exit with a non-zero status if any file would be reformatted. Does not modify files. Intended for CI.

### `--diff`

Print the formatting diff to stdout instead of rewriting the file.

### `--recursive`, `-r`

When PATH is a directory, recurse into subdirectories and format every `.crn` file found.

## Examples

Format every `.crn` file in the current directory:

```bash
carina fmt
```

Format a single file:

```bash
carina fmt main.crn
```

Format every `.crn` under the current tree:

```bash
carina fmt --recursive .
```

Show the diff without writing changes:

```bash
carina fmt --diff main.crn
```

Verify formatting in CI:

```bash
carina fmt --check --recursive .
```
