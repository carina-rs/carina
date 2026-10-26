---
title: "LSP Setup"
description: "Set up the Carina Language Server for editor integration with autocompletion, diagnostics, hover information, semantic highlighting, code actions, and formatting."
---

Carina includes a Language Server Protocol (LSP) implementation that provides a rich editing experience for `.crn` files. This guide covers how to set it up and what features are available.

## Features

The Carina LSP (`carina-lsp`) provides:

- **Autocompletion** -- resource types, attributes, attribute values, keywords, and built-in function names
- **Diagnostics** -- parse errors, type validation, unknown resource types, module validation, and cross-directory upstream-reference shape checks
- **Hover information** -- documentation for resource attributes and types on hover
- **Semantic tokens** -- syntax highlighting for resource types, regions, and identifiers
- **Code actions** -- quick fixes for diagnostics (e.g. inserting the canonical enum identifier when an attribute value does not match any variant)
- **Document formatting** -- automatic formatting of `.crn` files

## Installation

If you installed Carina via Homebrew or GitHub Releases, `carina-lsp` is already included -- no additional installation is needed.

To build from source instead:

```bash
cargo install --git https://github.com/carina-rs/carina.git carina-lsp
```

Make sure the `carina-lsp` binary is in your `PATH`.

### Provider schemas

Resource types, attributes, and schema-based diagnostics come from the provider plugins installed in the configuration directory. The LSP loads the plugins that `carina init` installed under `.carina/providers/`; it does not download them itself. Run `carina init` in the directory first. Until then the LSP reports the missing provider as a diagnostic, and it picks the plugin up once it is installed, without restarting the editor.

## VS Code setup

The Carina extension is not published to the VS Code marketplace. Build it from the repository and install the `.vsix`:

```bash
make vscode-package
code --install-extension editors/vscode/carina-0.1.0.vsix
```

Then reload the VS Code window and open a `.crn` file. The extension launches `carina-lsp` from your `PATH`. To use a binary elsewhere, set its path in `settings.json`:

```json
{
  "carina.server.path": "/path/to/carina-lsp"
}
```

The extension also provides syntax highlighting and turns on format-on-save for `.crn` files. Install it with `code --install-extension`, not by copying `editors/vscode/` into `~/.vscode/extensions/`: a copied directory lacks its npm dependencies and the language client silently fails to start.

For TextMate, `editors/carina.tmbundle` provides the same syntax highlighting.

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
      root_dir = lspconfig.util.root_pattern('carina-providers.lock', '.git'),
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

- **Top-level keywords**: `provider`, `backend`, `let`, `let use`, `import`, `read`, `fn`, `arguments`, `attributes`, `exports`, `moved`, `removed`, `require`, `upstream_state`, `wait`
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
- **Module validation**: Errors in imported modules, missing module directories
- **Provider loading**: Providers that are not installed (run `carina init`) or fail to load

## Hover

Hover over a resource attribute to see its documentation, including the expected type and description from the provider schema.

## Code actions

The LSP offers quick fixes for selected diagnostics. Open the code-actions menu (in VS Code: light-bulb / `Ctrl+.`) on a diagnostic to apply one.

- **Insert canonical enum identifier** -- when an attribute's value does not match any variant of its enum type, the LSP offers one code action per candidate that replaces the offending value with the canonical namespaced identifier (e.g. `aws.s3.Bucket.VersioningStatus.enabled`). Both bare-identifier mismatches and quoted string literals are supported.

## Semantic tokens

The LSP emits semantic tokens for richer syntax highlighting beyond what the TextMate grammar can express:

- **Types**: PascalCase resource type segments (e.g. the `Vpc` in `awscc.ec2.Vpc`) and region identifiers
- **Functions**: Built-in and user-defined function names

DSL keywords (`provider`, `let`, `fn`, `for`, `if`, …) are intentionally **not** emitted as semantic tokens — the bundled TextMate grammar handles them with finer-grained scopes (#1948).

## Document formatting

The LSP supports formatting `.crn` files. In VS Code, use **Format Document** (Shift+Alt+F). The formatter aligns attributes and applies consistent indentation.

You can also format from the CLI:

```bash
# Format all .crn files in the current directory
carina fmt

# Format all .crn files in the current directory recursively
carina fmt --recursive

# Check formatting without modifying files
carina fmt --check

# Show diff of formatting changes without modifying files
carina fmt --diff
```

## Troubleshooting

If the LSP is not working:

1. **Check the binary**: Run `which carina-lsp` to verify it is installed and on your `PATH` (`carina-lsp` has no `--version` flag; run without arguments it waits for an LSP client on stdin)
2. **Check providers**: Run `carina init` in the configuration directory so the LSP can load provider schemas
3. **Check logs**: Set the `RUST_LOG=debug` environment variable and check the LSP output in your editor's log panel (in VS Code: **View → Output → Carina Language Server**)
4. **File type**: Ensure your editor recognizes `.crn` files and routes them to the Carina LSP
