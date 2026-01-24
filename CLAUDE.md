# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Test Commands

```bash
# Build
cargo build

# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p carina-core
cargo test -p carina-provider-aws

# Run a single test
cargo test -p carina-core test_name

# Run CLI commands
cargo run -- validate example.crn
cargo run -- plan example.crn
cargo run -- apply example.crn

# With AWS credentials (using aws-vault)
aws-vault exec <profile> -- cargo run -- plan example.crn
```

## Architecture

Carina is a functional infrastructure management tool that treats side effects as values (Effects) rather than immediately executing them.

### Data Flow

```
DSL (.crn) → Parser → Resources → Differ → Plan (Effects) → Interpreter → Provider → Infrastructure
```

### Key Abstractions

- **Effect** (`carina-core/src/effect.rs`): Enum representing side effects (Create, Update, Delete, Read). Effects are values, not executed operations.
- **Plan** (`carina-core/src/plan.rs`): Collection of Effects. Immutable, can be inspected before execution.
- **Provider** trait (`carina-core/src/provider.rs`): Async trait for infrastructure operations. Returns `BoxFuture` for async methods.
- **Interpreter** (`carina-core/src/interpreter.rs`): Executes a Plan by dispatching Effects to a Provider.

### Crate Structure

- **carina-core**: Core library with parser, types, and traits. No AWS dependencies.
- **carina-provider-aws**: AWS implementation of Provider trait using `aws-sdk-s3`.
- **carina-cli**: Binary that wires everything together.

### DSL Parser

The parser uses [pest](https://pest.rs/) grammar defined in `carina-core/src/parser/carina.pest`. Key constructs:
- `provider <name> { ... }` - Provider configuration
- `<provider>.<service>.<resource> { ... }` - Anonymous resource (ID from `name` attribute)
- `let <binding> = <resource>` - Named resource binding

### Region Format Conversion

The DSL uses `aws.Region.ap_northeast_1` format, but AWS SDK uses `ap-northeast-1`. Conversion happens in:
- `carina-provider-aws/src/lib.rs`: `convert_region_value()` for DSL→SDK
- Provider read operations return DSL format for consistent state comparison
