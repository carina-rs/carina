# Carina VS Code Extension

Language support for the Carina DSL (`.crn` files): syntax highlighting,
completions, and live diagnostics. The completion and diagnostic features
are powered by the `carina-lsp` language server, which the extension
launches as a child process.

## Installation

The extension is **not** published to the marketplace. Install from source.

### 1. Build and install the LSP binary

The extension launches `carina-lsp` from `PATH` by default. Install it
with the repo's standard target:

```bash
make install-lsp
```

This places `carina-lsp` in `$HOME/.cargo/bin` (or wherever
`INSTALL_DIR` points). Verify it is on `PATH`:

```bash
which carina-lsp
```

If it is not, add `$HOME/.cargo/bin` to `PATH`, or set
`carina.server.path` in VS Code settings to an absolute path.

### 2. Build the `.vsix`

From the repo root:

```bash
make vscode-package
```

This runs `npm install`, compiles the TypeScript, and produces
`editors/vscode/carina-<version>.vsix`.

### 3. Install the `.vsix` into VS Code

```bash
code --install-extension editors/vscode/carina-0.1.0.vsix
```

Then **reload the VS Code window** (Command Palette →
`Developer: Reload Window`). Open any `.crn` file and confirm the status
bar shows "Carina" and that completions appear on Ctrl+Space.

> [!IMPORTANT]
> Do **not** install by copying `editors/vscode/` directly into
> `~/.vscode/extensions/`. That path skips `npm install`, so
> `node_modules/vscode-languageclient/` is missing at activation time
> and the language client silently fails to start (#2420). Always go
> through `code --install-extension <path-to-vsix>`.

## Troubleshooting

If completions or diagnostics never appear in `.crn` files:

1. **Open the language server output channel.** In VS Code:
   `View → Output`, then pick **Carina Language Server** from the
   dropdown. Errors from `carina-lsp` and the language client appear
   here.
2. **Look for an error popup at activation.** The extension performs a
   startup self-check (`tryStartLanguageClient` in
   `src/extension.ts`); if loading `vscode-languageclient` fails or the
   language client constructor throws, an error message is shown
   directly with reinstall instructions. If you missed it, reload the
   window to trigger activation again.
3. **Confirm `carina-lsp` is on `PATH`.** Run `which carina-lsp` in a
   terminal. If it is missing, run `make install-lsp` from the repo
   root. If it lives somewhere unusual, set `carina.server.path` in VS
   Code settings.
4. **Confirm the file is tagged as `carina`.** Run
   `Developer: Inspect Editor Tokens and Scopes` from the Command
   Palette inside a `.crn` file. The language ID at the top should be
   `carina`. If it is not, the LSP will not engage.
5. **Reinstall from a fresh `.vsix`.** When in doubt:
   ```bash
   code --uninstall-extension mizzy.carina
   make vscode-package
   code --install-extension editors/vscode/carina-0.1.0.vsix
   ```
   then reload the window.

## Development

```bash
cd editors/vscode
npm install
npm run watch     # incremental compile on save
```

In VS Code, press F5 in this directory to launch an Extension
Development Host with the in-tree extension loaded; this picks up
changes without producing a `.vsix`.

## Future work

#2421 will bundle the runtime dependency (`vscode-languageclient`) into
`out/extension.js` with esbuild, eliminating the missing-`node_modules/`
failure mode entirely. Once that lands, the startup self-check in
`tryStartLanguageClient` becomes defense in depth — it should never
trigger on a properly-built `.vsix`.
