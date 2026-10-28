---
title: Global options
---

Global options can appear before or after a Carina subcommand.

## `--trust-local-provider <PATH=KIND>`

Grant a local `file://` provider the credential and metadata capabilities of an official provider for the current process. The flag is repeatable. `PATH` must be an absolute path to an existing `.wasm` file, and `KIND` must be `aws` or `github`.

```bash
carina plan --trust-local-provider /abs/path/carina-provider-aws.wasm=aws
```

Carina resolves symlinks and compares the source file's SHA-256 with the bytes installed by `carina init`. If they differ, the provider remains untrusted and Carina asks you to re-run `carina init`. An `aws` grant can load only an `aws` or `awscc` provider block; a `github` grant can load only a `github` block.

Without a matching grant, a local provider receives no AWS or GitHub credentials and cannot reach the EC2 or ECS metadata service. Remote providers ignore local grants; their capabilities are decided from the project's `carina-providers.lock` entry and the installed artifact that matches it. Treat the lock file and the `.carina/` directory as trusted inputs, and review both in a repository you did not write.

For multiple commands, use `CARINA_TRUST_LOCAL_PROVIDER` with the same `PATH=KIND` syntax. Separate multiple entries with the platform path-list separator: `:` on Unix and `;` on Windows.

```bash
export CARINA_TRUST_LOCAL_PROVIDER='/abs/path/aws.wasm=aws:/abs/path/github.wasm=github'
carina plan
```

The LSP reads only `CARINA_TRUST_LOCAL_PROVIDER`; it does not receive the CLI flag.

The flag and environment variable are process-local authorization. They are never stored in `carina-providers.lock`, the provider cache, a saved plan, or state.
