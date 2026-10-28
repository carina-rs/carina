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

`carina init` resolves the version through the provider registry (`registry.carina-rs.dev`), verifies the artifact's signature, installs the plugin into `.carina/providers/`, and records the resolved version and checksum in `carina-providers.lock`. Commit the lock file; `carina init --upgrade` re-resolves versions from the constraints, and `carina init --locked` fails if the lock does not cover every provider (for CI). `validate`, `plan`, and `apply` do not download anything — they fail with `Run carina init` if a provider is missing.

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
