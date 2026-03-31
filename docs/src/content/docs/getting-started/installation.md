---
title: Installation
description: How to install Carina from source.
---

Carina is distributed as a Rust binary built from source.

## Prerequisites

- **Rust toolchain** (1.75 or later) -- install via [rustup](https://rustup.rs/)
- **AWS credentials** -- required for `plan` and `apply` commands. Configure via environment variables, `~/.aws/credentials`, or IAM roles.

## Build from source

```bash
git clone https://github.com/carina-rs/carina.git
cd carina
cargo build --release
```

The binary is at `target/release/carina`. Add it to your `PATH`:

```bash
cp target/release/carina /usr/local/bin/
```

## Verify installation

```bash
carina --help
```

## AWS credential setup

Carina needs AWS credentials to interact with your infrastructure. Any standard AWS credential method works:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- Shared credentials file (`~/.aws/credentials`)
- IAM instance roles (when running on EC2)
- Tools like [aws-vault](https://github.com/99designs/aws-vault):

```bash
aws-vault exec myprofile -- carina plan main.crn
```
