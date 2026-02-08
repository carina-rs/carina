# Carina

A strongly typed infrastructure management tool written in Rust.

## Key Features

- **Custom DSL** for infrastructure definition (`.crn` files)
- **Effects as Values** - side effects are represented as data, inspectable before execution
- **Strong Typing** - resource attributes are validated at parse time
- **Provider Architecture** - pluggable providers (AWS, AWSCC)
- **Modules** - reusable infrastructure components
- **State Management** - S3 backend for remote state
- **LSP Support** - editor integration with completions and diagnostics

## Quick Start

```bash
# Build from source
cargo build --release

# Validate a configuration
cargo run --bin carina -- validate example.crn

# Preview changes
aws-vault exec <profile> -- cargo run --bin carina -- plan example.crn

# Apply changes
aws-vault exec <profile> -- cargo run --bin carina -- apply example.crn
```

## Providers

- [AWSCC Provider](providers/awscc/index.md) - AWS Cloud Control API provider

For more details, see the [README](https://github.com/mizzy/carina).
