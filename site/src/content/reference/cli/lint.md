---
title: lint
---

Lint `.crn` files for style issues. Lint runs checks that go beyond syntactic validity (which is covered by `validate`) — it surfaces patterns that parse and type-check but are stylistically discouraged.

## Usage

```bash
carina lint [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files.

## Examples

Lint the current directory:

```bash
carina lint
```

Lint a specific directory:

```bash
carina lint path/to/config
```

## Related

- [`validate`](/reference/cli/validate/) -- syntactic and type-level checks
- [`fmt`](/reference/cli/fmt/) -- automatic formatting
