---
title: module info
---

Show the structure of a module, including its arguments, exported attributes, resources, and dependencies.

## Usage

```bash
carina module info [OPTIONS] <FILE>
```

**FILE** is the path to a module `.crn` file or a module directory (containing a `main.crn`).

## Flags

### `--tui`

Display module info in interactive TUI mode.

## Output Format

The output displays the module signature, including:

- **Module name** -- derived from the directory name (for directory-based modules) or file stem (for single-file modules)
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
  database    ./modules/database.crn
```

Prints "No modules imported." if no modules are used.

## Examples

Show info for a directory-based module:

```bash
carina module info modules/web_tier/
```

Show info for a single-file module:

```bash
carina module info modules/database.crn
```

Show info in TUI mode:

```bash
carina module info --tui modules/web_tier/
```
