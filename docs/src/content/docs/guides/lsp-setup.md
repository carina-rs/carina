---
title: "LSP Setup"
description: "Set up the Carina Language Server for editor integration with autocompletion, diagnostics, hover information, semantic highlighting, and formatting."
---

Carina includes a Language Server Protocol (LSP) implementation that provides a rich editing experience for `.crn` files. This guide covers how to set it up and what features are available.

## Features

The Carina LSP (`carina-lsp`) provides:

- **Autocompletion** -- resource types, attributes, attribute values, keywords, and built-in function names
- **Diagnostics** -- parse errors, type validation, unknown resource types, and module validation
- **Hover information** -- documentation for resource attributes and types on hover
- **Semantic tokens** -- syntax highlighting for keywords, resource types, regions, and identifiers
- **Document formatting** -- automatic formatting of `.crn` files

## Installation

Build the LSP server from source:

```bash
cargo install --path carina-lsp
```

This installs the `carina-lsp` binary. Make sure it is in your `PATH`.

## VS Code setup

1. Install the **Carina** extension from the VS Code marketplace, or configure a generic LSP client extension (such as [vscode-languageclient](https://github.com/microsoft/vscode-languageserver-node)).

2. If using a generic LSP client, add to your VS Code `settings.json`:

```json
{
  "carina.lsp.path": "carina-lsp"
}
```

3. Open a `.crn` file. The LSP server starts automatically.

## Neovim setup

### With nvim-lspconfig

Add a custom server configuration:

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

if not configs.carina then
  configs.carina = {
    default_config = {
      cmd = { 'carina-lsp' },
      filetypes = { 'crn' },
      root_dir = lspconfig.util.root_pattern('.git', 'main.crn'),
    },
  }
end

lspconfig.carina.setup {}
```

### File type detection

Add to your Neovim configuration:

```lua
vim.filetype.add({
  extension = {
    crn = 'crn',
  },
})
```

## Autocompletion

The LSP provides context-aware completions:

- **Top-level keywords**: `provider`, `backend`, `let`, `import`, `fn`, `for`, `if`, `arguments`, `attributes`, `moved`, `removed`, `require`, `read`
- **Resource types**: Type `awscc.` to see available services, then `awscc.ec2.` to see resource types
- **Attributes**: Inside a resource block, get completions for all valid attributes
- **Attribute values**: For enum-typed attributes, get valid value completions
- **Built-in functions**: Function names with signature information

Completions are triggered by `.`, `=`, and space characters.

## Diagnostics

The LSP reports errors and warnings as you type:

- **Parse errors**: Syntax mistakes detected by the Carina parser
- **Unknown resource types**: Using a resource type that does not exist in the provider schema
- **Type validation**: Attribute values that do not match the expected type
- **Missing required attributes**: Required attributes that are not set
- **Module validation**: Errors in imported modules, missing module files

## Hover

Hover over a resource attribute to see its documentation, including the expected type and description from the provider schema.

## Semantic tokens

The LSP provides semantic token information for enhanced syntax highlighting:

- **Keywords**: `provider`, `backend`, `let`, `fn`, `for`, `if`, `else`, `import`, `arguments`, `attributes`, `moved`, `removed`, `require`, `read`
- **Types**: Resource types and region identifiers
- **Functions**: Built-in and user-defined function names

## Document formatting

The LSP supports formatting `.crn` files. In VS Code, use **Format Document** (Shift+Alt+F). The formatter aligns attributes and applies consistent indentation.

You can also format from the CLI:

```bash
# Format a single file
carina fmt main.crn

# Format all .crn files in a directory recursively
carina fmt --recursive .

# Check formatting without modifying files
carina fmt --check main.crn

# Show diff of formatting changes
carina fmt --diff main.crn
```

## Troubleshooting

If the LSP is not working:

1. **Check the binary**: Run `carina-lsp --version` to verify it is installed
2. **Check your PATH**: The LSP client must be able to find `carina-lsp`
3. **Check logs**: Set the `RUST_LOG=debug` environment variable and check the LSP output in your editor's log panel
4. **File type**: Ensure your editor recognizes `.crn` files and routes them to the Carina LSP
