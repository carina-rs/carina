import { workspace, window, ExtensionContext } from 'vscode';
import type {
  LanguageClient as LanguageClientType,
  LanguageClientOptions,
  ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClientType | undefined;

/**
 * Attempt to load `vscode-languageclient/node` and start the LSP client.
 *
 * VS Code does NOT automatically surface exceptions thrown from `activate`
 * to the user — the activation just silently fails. So when the runtime
 * dependency is missing (e.g. the extension was installed by copying the
 * source directory into `~/.vscode/extensions/` without running
 * `npm install`), users see the language registration succeed but get
 * zero completions and zero diagnostics, with no visible error.
 *
 * This helper isolates the dependency load so failures can be surfaced
 * via `showError` and logged. Returns the started client on success, or
 * `undefined` if the dependency is missing or the client failed to start.
 *
 * Factored out (and parameterised over `requireFn` and `showError`) so
 * the failure path can be exercised without a real VS Code host.
 */
export function tryStartLanguageClient(
  serverPath: string,
  showError: (msg: string) => void,
  requireFn: (id: string) => unknown = require,
): LanguageClientType | undefined {
  let mod: typeof import('vscode-languageclient/node');
  try {
    mod = requireFn('vscode-languageclient/node') as typeof import('vscode-languageclient/node');
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    const msg =
      'Carina extension dependencies are missing (failed to load ' +
      "'vscode-languageclient'). Reinstall via: " +
      'code --install-extension <path/to/carina-X.Y.Z.vsix>. ' +
      "If you don't have a .vsix, build one with: " +
      'cd editors/vscode && npm install && npx vsce package. ' +
      `Underlying error: ${detail}`;
    console.error('[carina] ' + msg);
    showError(msg);
    return undefined;
  }

  const serverOptions: ServerOptions = {
    run: { command: serverPath },
    debug: { command: serverPath },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: 'file', language: 'carina' }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher('**/*.crn'),
    },
  };

  try {
    const started = new mod.LanguageClient(
      'carina',
      'Carina Language Server',
      serverOptions,
      clientOptions,
    );
    started.start();
    return started;
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    const msg =
      'Carina language client failed to start: ' +
      detail +
      ". Check the 'Carina Language Server' output channel and verify " +
      `that '${serverPath}' is on PATH (or set 'carina.server.path').`;
    console.error('[carina] ' + msg);
    showError(msg);
    return undefined;
  }
}

export function activate(_context: ExtensionContext): void {
  const config = workspace.getConfiguration('carina');
  const serverPath = config.get<string>('server.path') || 'carina-lsp';
  client = tryStartLanguageClient(serverPath, (msg) => {
    void window.showErrorMessage(msg);
  });
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
