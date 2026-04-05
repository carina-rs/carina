---
title: Installation
description: How to install Carina and set up provider plugins.
---

## Homebrew (macOS)

```bash
brew install carina-rs/tap/carina
```

## GitHub Releases

Download pre-built binaries from [GitHub Releases](https://github.com/carina-rs/carina/releases/latest). Available for macOS (aarch64, x86_64) and Linux (aarch64, x86_64).

```bash
# macOS (Apple Silicon)
curl -LO https://github.com/carina-rs/carina/releases/latest/download/carina-0.2.0-macos-aarch64.tar.gz
tar xzf carina-0.2.0-macos-aarch64.tar.gz
sudo mv carina /usr/local/bin/
```

## Build from source

Prerequisites:
- **Rust toolchain** (stable) -- install via [rustup](https://rustup.rs/)
- **wasm32-wasip2 target** -- required for building provider plugins

```bash
rustup target add wasm32-wasip2
```

Clone the repository and build in release mode:

```bash
git clone https://github.com/carina-rs/carina.git
cd carina
cargo build --release
```

The binary is at `target/release/carina`. Add it to your `PATH` or copy it to a directory already on your `PATH`.

## Provider plugins

Carina uses WASM-based provider plugins. When you specify a `source` and `version` in your `.crn` file, Carina automatically downloads the provider plugin from GitHub Releases on first use:

```crn
provider awscc {
    source = "github.com/carina-rs/carina-provider-awscc"
    version = "0.2.0"
    region = "ap-northeast-1"
}
```

The two official providers are:

| Provider | Source |
|----------|--------|
| AWSCC (Cloud Control API) | `github.com/carina-rs/carina-provider-awscc` |
| AWS (native SDK) | `github.com/carina-rs/carina-provider-aws` |

### Building from source (optional)

If you need to build providers from source:

```bash
git clone https://github.com/carina-rs/carina-provider-awscc.git
cd carina-provider-awscc
cargo build -p carina-provider-awscc --target wasm32-wasip2 --release
```

The WASM component is at `target/wasm32-wasip2/release/carina_provider_awscc.wasm`.

### Building the AWS provider

```bash
git clone https://github.com/carina-rs/carina-provider-aws.git
cd carina-provider-aws
cargo build -p carina-provider-aws --target wasm32-wasip2 --release
```

The WASM component is at `target/wasm32-wasip2/release/carina_provider_aws.wasm`.

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
