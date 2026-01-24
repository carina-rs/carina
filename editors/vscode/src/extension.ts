import * as path from 'path';
import { workspace, ExtensionContext } from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient;

export function activate(context: ExtensionContext) {
  // Get the server path from settings, or use 'carina-lsp' from PATH
  const config = workspace.getConfiguration('carina');
  const serverPath = config.get<string>('server.path') || 'carina-lsp';

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

  client = new LanguageClient(
    'carina',
    'Carina Language Server',
    serverOptions,
    clientOptions
  );

  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }
  return client.stop();
}
