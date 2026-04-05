---
title: Installation
description: How to build Carina from source and set up provider plugins.
---

## Prerequisites

- **Rust toolchain** (stable) -- install via [rustup](https://rustup.rs/)
- **wasm32-wasip2 target** -- required for building provider plugins
- **AWS credentials** -- [aws-vault](https://github.com/99designs/aws-vault) is recommended

```bash
rustup target add wasm32-wasip2
```

## Build from source

Clone the repository and build in release mode:

```bash
git clone https://github.com/carina-rs/carina.git
cd carina
cargo build --release
```

The binary is at `target/release/carina`. Add it to your `PATH` or copy it to a directory already on your `PATH`.

## Provider plugins

Carina uses WASM-based provider plugins. The two official providers live in separate repositories:

| Provider | Repository |
|----------|------------|
| AWSCC (Cloud Control API) | [carina-provider-awscc](https://github.com/carina-rs/carina-provider-awscc) |
| AWS (native SDK) | [carina-provider-aws](https://github.com/carina-rs/carina-provider-aws) |

### Building the AWSCC provider

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

## Verify the installation

```bash
carina --help
```
