---
title: module info
---

Show the structure of a module, including its arguments, exported attributes, resources, and dependencies.

## Usage

```bash
carina module info [OPTIONS] <PATH>
```

**PATH** is a module directory (a directory containing one or more `.crn` files). Single-file paths are rejected.

## Flags

### `--tui`

Display module info in interactive TUI mode.

## Output Format

The output displays the module signature, including:

- **Module name** -- derived from the module's directory name
- **Arguments** -- parameters the module expects, defined by unresolved references
- **Attributes** -- values exported by the module for use by the caller
- **Resources** -- infrastructure resources defined in the module
- **Dependencies** -- references to other resources or bindings

## Related: `module list`

List all imported modules in a configuration:

```bash
carina module list [PATH]
```

**PATH** defaults to `.` (current directory).

Output shows each module's alias and path:

```
Modules:
  web_tier    ./modules/web_tier
  database    ./modules/database
```

Prints "No modules imported." if no modules are used.

## Examples

Show info for a module:

```bash
carina module info modules/web_tier/
```

Show info in TUI mode:

```bash
carina module info --tui modules/web_tier/
```
