---
title: module
---

Module management commands: inspect a module's signature or list the modules a configuration imports.

## Usage

```bash
carina module <SUBCOMMAND>
```

## Subcommands

### `info`

Show the structure of a module, including its arguments, exported attributes, and resources with their dependencies.

```bash
carina module info [OPTIONS] <PATH>
```

**PATH** is a module directory (a directory containing one or more `.crn` files). Single-file paths are rejected.

#### Flags

| Flag | Description |
|------|-------------|
| `--tui` | Display module info in interactive TUI mode |

#### Output Format

When the directory declares an `arguments` or `attributes` block, it is shown as a module signature:

- **Module name** -- derived from the module's directory name
- **ARGUMENTS** -- parameters declared in the `arguments` block, with type, default or `(required)`, and description
- **CREATES** -- resources defined in the module, as a dependency tree
- **ATTRIBUTES** -- values declared in the `attributes` block, exported for use by the caller

```
Module: web_tier

=== ARGUMENTS ===

  vpc_id: awscc.ec2.Vpc  (required)
    The VPC to deploy resources into
  instance_type: String = 't3.micro'
    EC2 instance type

=== CREATES ===

  web_sg: awscc.ec2.SecurityGroup
    └── web_instance: awscc.ec2.Instance

=== ATTRIBUTES ===

  security_group_id: awscc.ec2.SecurityGroup
  instance_id: awscc.ec2.Instance
```

Otherwise the directory is treated as a root configuration and the output shows `File: <name>` with `IMPORTS` and `CREATES` sections.

### `list`

List all imported modules in a configuration.

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

List the modules imported by the current directory:

```bash
carina module list
```
