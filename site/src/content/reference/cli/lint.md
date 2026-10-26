---
title: lint
---

Lint `.crn` files for style issues. Lint runs checks that go beyond syntactic validity (which is covered by `validate`) — it surfaces patterns that parse and type-check but are stylistically discouraged.

## Usage

```bash
carina lint [PATH]
```

**PATH** defaults to `.` (current directory). It must be a directory containing one or more `.crn` files.

## What It Checks

- List literal syntax (`attr = [{ ... }]`) for `List<Struct>` attributes, where block syntax is preferred
- Duplicate attribute keys in the same block
- Direct calls to functions that read better in pipe form (`data |> fn(...)`)
- Binding names that are not `snake_case`
- Mixed tag key styles
- Duplicate declarations, including inside imported modules

## Output

Each finding is printed as `warning: <file>:<line>  <message>`. If any warning is found, `lint` exits with code `1`; otherwise it prints "No lint warnings found."

```
warning: main.crn:9  Duplicate attribute 'name' (first defined on line 8). The last value will be used.
Error: Found 1 lint warning(s).
```

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
