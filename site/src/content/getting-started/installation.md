---
title: Installation
description: How to install Carina and set up provider plugins.
---

## Homebrew (macOS)

```bash
brew install carina-rs/tap/carina
```

This installs both `carina` (CLI) and `carina-lsp` (Language Server).

## GitHub Releases

Download pre-built binaries from [GitHub Releases](https://github.com/carina-rs/carina/releases/latest). Available for macOS (aarch64, x86_64) and Linux (aarch64, x86_64). Each archive contains both `carina` and `carina-lsp`.

```bash
# macOS (Apple Silicon) — replace <VERSION> with the latest release tag (e.g. 0.5.0)
curl -LO https://github.com/carina-rs/carina/releases/download/v<VERSION>/carina-<VERSION>-macos-aarch64.tar.gz
tar xzf carina-<VERSION>-macos-aarch64.tar.gz
sudo mv carina-<VERSION>-macos-aarch64/carina carina-<VERSION>-macos-aarch64/carina-lsp /usr/local/bin/
```

Each archive has a matching `.sha256` file, and `checksums-sha256.txt` lists them all.

## Build from source

Prerequisites:
- **Rust toolchain** (stable) -- install via [rustup](https://rustup.rs/)
- **wasm32-wasip2 target** -- required for building provider plugins

```bash
rustup target add wasm32-wasip2
```

Clone the repository (with the WIT definitions submodule) and build in release mode:

```bash
git clone https://github.com/carina-rs/carina.git
cd carina
git submodule update --init --recursive
cargo build --release
```

The binaries are at `target/release/carina` and `target/release/carina-lsp`. Add them to your `PATH` or copy it to a directory already on your `PATH`.

## Provider plugins

Carina uses WASM-based provider plugins. Declare each provider's `source` and `version` (a semver constraint) in a `provider` block, then run `carina init` in the configuration directory:

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}
```

```bash
carina init
```

`carina init` resolves the version through the provider registry (`registry.carina-rs.dev`), verifies the artifact's signature, installs the plugin into `.carina/providers/`, and records the resolved version and checksum in `carina-providers.lock`. For an allow-listed `carina-rs` source, it also records the downloaded artifact's canonical source, SHA-256, and verified signer when available in the current user's trust directory, `~/.carina/trust/` (overridable with `CARINA_TRUST_RECORD_DIR`). If a matching project artifact or global cache entry for an allow-listed source exists without this user record, `carina init` downloads the artifact from its origin again before creating the record. The command fails if it cannot write a required trust record; third-party sources neither write nor require one.

After upgrading Carina, the first `carina init` re-downloads each allow-listed `carina-rs` provider from its origin to create the current trust record, so it needs network access. For a GitHub `revision` source whose locked commit artifact has expired (GitHub normally retains artifacts for about 90 days), run `carina init --upgrade` to resolve a current artifact.

Commit the lock file; `carina init --upgrade` re-resolves versions from the constraints, and `carina init --locked` fails if the lock does not cover every provider (for CI). `validate`, `plan`, and `apply` do not download anything — they fail with `Run carina init` if a provider is missing. An allow-listed `carina-rs` AWS, AWSCC, or GitHub provider loads only when the exact bytes match this user's valid trust record. A `carina-providers.lock` file or `.carina/` directory shipped in a repository cannot create that verification. If the record is absent, unreadable, malformed, or names a signer outside `carina-rs`, Carina fails before running the provider and the error tells the user to run `carina init`.

In CI, restoring `.carina/` without `~/.carina/trust/` therefore makes `carina-rs` providers fail to load. Run `carina init` in the job or persist the trust directory; set `CARINA_TRUST_RECORD_DIR` when the CI cache uses another location.

`source` takes one of three forms:

| Form | Example | Resolved from |
|------|---------|---------------|
| Registry | `carina-rs/awscc`, or `host/namespace/name` for another registry | The provider registry (default host `registry.carina-rs.dev`) |
| GitHub | `github.com/carina-rs/carina-provider-awscc` | The repository's GitHub Releases |
| Local file | `file:///path/to/provider.wasm` | A locally built component |

Instead of `version`, `revision` installs the build for a branch or tag. `version` only accepts a version constraint, so `version = 'main'` is an error.

With a registry source, the registry resolves the revision to the newest build it has for that branch or tag, so the build must be published there. The public registry serves releases only; branch builds are on the dev registry:

```crn
provider awscc {
  source   = 'registry-dev.carina-rs.dev/carina-rs/awscc'
  revision = 'main'
  region   = awscc.Region.ap_northeast_1
}
```

With a GitHub source, `revision` can also be a commit SHA and installs the CI build artifact for that commit. [Your First Provider](/providers/writing/getting-started/#use-it-from-github) describes which builds qualify. Resolving it calls the GitHub API, so it needs a token (`GITHUB_TOKEN`, or a logged-in `gh`). A registry source needs no token.

The two official providers are:

| Provider | Source |
|----------|--------|
| AWSCC (Cloud Control API) | `carina-rs/awscc` (or `github.com/carina-rs/carina-provider-awscc`) |
| AWS (native SDK) | `carina-rs/aws` (or `github.com/carina-rs/carina-provider-aws`) |

### Building from source (optional)

If you need to build providers from source:

```bash
git clone https://github.com/carina-rs/carina-provider-awscc.git
cd carina-provider-awscc
cargo build -p carina-provider-awscc --target wasm32-wasip2 --release
```

The WASM component is at `target/wasm32-wasip2/release/carina-provider-awscc.wasm`.

### Building the AWS provider

```bash
git clone https://github.com/carina-rs/carina-provider-aws.git
cd carina-provider-aws
cargo build -p carina-provider-aws --target wasm32-wasip2 --release
```

The WASM component is at `target/wasm32-wasip2/release/carina-provider-aws.wasm`.

To use a locally built component, point `source` at it with a `file://` URL and run `carina init`:

```crn
provider aws {
  source = 'file:///path/to/carina-provider-aws/target/wasm32-wasip2/release/carina-provider-aws.wasm'
  region = aws.Region.ap_northeast_1
}
```

Local components receive no AWS credentials and cannot reach the EC2 or ECS metadata service by default. To grant those capabilities to this exact build for one command, pass its absolute source path and the `aws` kind:

```bash
carina init
carina plan --trust-local-provider /abs/path/carina-provider-aws.wasm=aws
```

The path is canonicalized, including symlink resolution, and the installed bytes must still match the source file. Re-run `carina init` after rebuilding the component.

For a sequence of commands, set `CARINA_TRUST_LOCAL_PROVIDER` instead. It accepts the same `PATH=KIND` entries, separated by the platform path-list separator (`:` on Unix and `;` on Windows):

```bash
CARINA_TRUST_LOCAL_PROVIDER='/abs/path/carina-provider-aws.wasm=aws' carina plan
```

The LSP reads only `CARINA_TRUST_LOCAL_PROVIDER`; it cannot receive `--trust-local-provider` from a Carina CLI invocation.

Every configured entry is validated when a provider-loading command starts. A stale path or otherwise invalid `CARINA_TRUST_LOCAL_PROVIDER` entry makes those commands fail until the entry is fixed or removed.

The other supported kind is `github`, for a local build of the GitHub provider. Grants last only for the current process; Carina never writes them to the provider lock, cache, saved plan, or state.

## Shell completions

```bash
# Bash
carina completions bash > ~/.local/share/bash-completion/completions/carina

# Zsh
mkdir -p ~/.zfunc
carina completions zsh > ~/.zfunc/_carina
# Add to .zshrc: fpath=(~/.zfunc $fpath); autoload -Uz compinit; compinit

# Fish
carina completions fish > ~/.config/fish/completions/carina.fish
```

## Verify the installation

```bash
carina --help
```
