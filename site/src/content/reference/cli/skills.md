---
title: skills
---

Manage Agent Skills bundled in the Carina binary. Agent Skills are standardized instruction files (`SKILL.md`) that teach AI agents how to use Carina effectively.

Skills are installed to `~/.agents/skills/carina/` following the [Agent Skills standard](https://agentskills.io), making them discoverable by any compatible AI agent client.

## Usage

```bash
carina skills <COMMAND>
```

## Commands

### `list`

List embedded skills with their version.

```bash
carina skills list
```

### `install`

Install the bundled `SKILL.md` to `~/.agents/skills/carina/`.

```bash
carina skills install
```

### `status`

Show install status and compare the installed version against the embedded version.

```bash
carina skills status
```

Output when not installed:

```
Not installed.
  Embedded version: v0.3.0
  Run 'carina skills install' to install.
```

Output when up to date:

```
Installed at: /Users/you/.agents/skills/carina/SKILL.md
  Version: v0.3.0 (up to date)
```

Output when a newer version is available:

```
Installed at: /Users/you/.agents/skills/carina/SKILL.md
  Installed version: v0.2.0
  Embedded version: v0.3.0
  Run 'carina skills update' to update.
```

### `update`

Update the installed skill if the embedded version is newer. If not installed, performs an install.

```bash
carina skills update
```

### `reinstall`

Force overwrite the installed skill regardless of version.

```bash
carina skills reinstall
```

### `uninstall`

Remove the installed skill directory.

```bash
carina skills uninstall
```

## Examples

Install skills after upgrading Carina:

```bash
carina skills update
```

Check if your installed skills match the current binary:

```bash
carina skills status
```

## What is SKILL.md?

The installed `SKILL.md` contains:

- Common Carina workflows (validate, plan, apply)
- DSL syntax quick reference
- CLI command reference
- Best practices for working with Carina via AI agents

AI agents load this file automatically to learn how to operate Carina without relying on web search or training data.
