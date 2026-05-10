---
title: docs
---

Display documentation embedded in the Carina binary. This allows AI agents and users to access version-accurate documentation without web search or external files.

## Usage

```bash
carina docs [OPTIONS] [NAME]
```

## Flags

### `--list`

List all available embedded documents with their names and titles.

```bash
carina docs --list
```

### `--search <QUERY>`

Search all embedded documents for a keyword (case-insensitive). Results show the document name, line number, and matching line.

```bash
carina docs --search provider
```

## Arguments

### `[NAME]`

Show a specific document by name. Use `--list` to see available names.

```bash
carina docs reference/dsl/syntax
```

If no name or flags are given, the README is displayed.

## Available Documents

Documents are organized by category:

| Category | Documents |
|----------|-----------|
| Getting Started | `getting-started/installation`, `getting-started/quick-start`, `getting-started/core-concepts` |
| Guides | `guides/writing-resources`, `guides/using-modules`, `guides/state-management`, `guides/functions`, `guides/for-if-expressions`, `guides/lsp-setup` |
| DSL Reference | `reference/dsl/syntax`, `reference/dsl/types-and-values`, `reference/dsl/expressions`, `reference/dsl/modules`, `reference/dsl/built-in-functions` |
| CLI Reference | `reference/cli/validate`, `reference/cli/plan`, `reference/cli/apply`, `reference/cli/state`, `reference/cli/module-info`, `reference/cli/docs` |

## Examples

Display the README:

```bash
carina docs
```

List all documents:

```bash
carina docs --list
```

Read the DSL syntax reference:

```bash
carina docs reference/dsl/syntax
```

Search for module-related content:

```bash
carina docs --search module
```

## Design

All documents are embedded into the binary at compile time using Rust's `include_str!()` macro. This ensures:

- Documentation always matches the installed binary version
- No network access or external files required
- AI agents get accurate information instead of stale web search results or training data
