---
title: Global options
---

Global options can appear before or after a Carina subcommand.

## `--trust-local-provider <PATH=KIND>`

Grant a local `file://` provider the typed credential capability of an official provider for the current process. The flag is repeatable. `PATH` must be an absolute path to an existing `.wasm` file, and `KIND` must be `aws` or `github`.

```bash
carina plan --trust-local-provider /abs/path/carina-provider-aws.wasm=aws
```

Carina resolves symlinks and compares the source file's SHA-256 with the bytes installed by `carina init`. Each grant that applies is announced on stderr as `carina: trusting local provider <path> as <kind> (--trust-local-provider)` (or `(CARINA_TRUST_LOCAL_PROVIDER)`). If the bytes differ, the provider remains untrusted and Carina prints `carina: local provider <path> does not match the installed bytes; re-run carina init before trusting it`. An `aws` grant can load only an `aws` or `awscc` provider block; a `github` grant can load only a `github` block.

Without a matching grant, a local provider receives no AWS or GitHub credentials. Remote providers ignore local grants. No provider kind can reach EC2, ECS, or other cloud metadata endpoints. An allow-listed `carina-rs` AWS, AWSCC, or GitHub provider loads only after this user's `carina init` downloaded those exact bytes from the origin and recorded their source and SHA-256 under `~/.carina/trust/`. A lock file or `.carina/` directory shipped in a repository does not provide that verification. If the record is absent or unusable, Carina fails before running the provider and reports that the user should run `carina init`.

For multiple commands, use `CARINA_TRUST_LOCAL_PROVIDER` with the same `PATH=KIND` syntax. Separate multiple entries with the platform path-list separator: `:` on Unix and `;` on Windows.

```bash
export CARINA_TRUST_LOCAL_PROVIDER='/abs/path/aws.wasm=aws:/abs/path/github.wasm=github'
carina plan
```

The LSP reads only `CARINA_TRUST_LOCAL_PROVIDER`; it does not receive the CLI flag.

A stale path or otherwise invalid `CARINA_TRUST_LOCAL_PROVIDER` entry makes commands that load providers fail until the entry is fixed or removed.

The flag and environment variable are process-local authorization. They are never stored in `carina-providers.lock`, the provider cache, a saved plan, or state.
