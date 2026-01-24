# Carina

A functional infrastructure management tool written in Rust. Carina treats infrastructure changes as values (Effects) rather than immediately executing side effects, enabling safer and more predictable infrastructure management.

## Features

- **Custom DSL**: Simple, expressive syntax for defining infrastructure
- **Effects as Values**: Side effects are represented as data structures, not immediately executed
- **Strong Typing**: Catch configuration errors at parse time with schema validation
- **Provider Architecture**: Extensible provider system for multi-cloud support
- **Terraform-like Workflow**: Familiar `validate`, `plan`, `apply` commands

## Installation

```bash
cargo build --release
```

The binary will be available at `target/release/carina`.

## Quick Start

### 1. Define your infrastructure

Create a `.crn` file:

```
# example.crn

provider aws {
    region = aws.Region.ap_northeast_1
}

# Anonymous resource (ID derived from name attribute)
aws.s3.bucket {
    name = "my-app-logs"
    region = aws.Region.ap_northeast_1
    versioning = true
    expiration_days = 90
}

# Named resource (for referencing)
let backup = aws.s3.bucket {
    name = "my-app-backup"
    region = aws.Region.ap_northeast_1
}
```

### 2. Validate

```bash
$ carina validate example.crn
Validating...
✓ 2 resources validated successfully.
  • s3_bucket.my-app-logs
  • s3_bucket.my-app-backup
```

### 3. Plan

```bash
$ carina plan example.crn
Using AWS provider (region: ap-northeast-1)
Execution Plan:

  + s3_bucket.my-app-logs
      name: "my-app-logs"
      expiration_days: 90
      region: "aws.Region.ap_northeast_1"
      versioning: true
  + s3_bucket.my-app-backup
      name: "my-app-backup"
      region: "aws.Region.ap_northeast_1"

Plan: 2 to add, 0 to change, 0 to destroy.
```

### 4. Apply

```bash
$ carina apply example.crn
Using AWS provider (region: ap-northeast-1)
Applying changes...

  ✓ Create s3_bucket.my-app-logs
  ✓ Create s3_bucket.my-app-backup

Apply complete! 2 changes applied.
```

## DSL Syntax

### Provider Block

```
provider aws {
    region = aws.Region.ap_northeast_1
}
```

### Resources

**Anonymous resources** - ID is derived from the `name` attribute:

```
aws.s3.bucket {
    name = "my-bucket"
    region = aws.Region.ap_northeast_1
    versioning = true
}
```

**Named resources** - Use `let` binding for referencing:

```
let logs = aws.s3.bucket {
    name = "my-logs"
    region = aws.Region.ap_northeast_1
}
```

### Supported Attributes for S3 Bucket

| Attribute | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | Yes | Bucket name (must be globally unique) |
| `region` | enum | Yes | AWS region (e.g., `aws.Region.ap_northeast_1`) |
| `versioning` | bool | No | Enable versioning |
| `expiration_days` | int | No | Auto-delete objects after N days |

## Architecture

Carina follows a functional architecture where side effects are treated as values:

```
DSL File (.crn)
     │
     ▼
┌─────────┐
│ Parser  │  Parse DSL into Resources
└────┬────┘
     │
     ▼
┌─────────┐
│ Differ  │  Compare desired vs current state
└────┬────┘
     │
     ▼
┌─────────┐
│  Plan   │  Collection of Effects (Create/Update/Delete)
└────┬────┘
     │
     ▼
┌─────────────┐
│ Interpreter │  Execute Effects through Provider
└──────┬──────┘
       │
       ▼
┌──────────┐
│ Provider │  AWS, GCP, etc.
└──────────┘
```

### Core Concepts

- **Resource**: Desired state declared in DSL
- **State**: Current state fetched from infrastructure
- **Effect**: Represents a side effect (Create, Update, Delete, Read)
- **Plan**: Collection of Effects to be executed
- **Provider**: Abstraction for infrastructure operations
- **Interpreter**: Executes Plan through Provider

## Project Structure

```
carina/
├── carina-cli/          # CLI application
├── carina-core/         # Core library
│   ├── src/
│   │   ├── effect.rs    # Effect type definitions
│   │   ├── plan.rs      # Plan (collection of Effects)
│   │   ├── resource.rs  # Resource and State types
│   │   ├── provider.rs  # Provider trait
│   │   ├── interpreter.rs # Effect interpreter
│   │   ├── differ.rs    # State comparison
│   │   ├── parser/      # DSL parser (pest-based)
│   │   ├── schema.rs    # Type validation
│   │   └── providers/   # Built-in provider schemas
│   └── ...
└── carina-provider-aws/ # AWS provider implementation
```

## AWS Provider

The AWS provider requires valid AWS credentials. Configure via:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- AWS credentials file (`~/.aws/credentials`)
- IAM roles (when running on AWS)

### Using with aws-vault

```bash
aws-vault exec myprofile -- carina apply example.crn
```

## Development

### Run tests

```bash
cargo test
```

### Build

```bash
cargo build
```

## License

MIT

## Roadmap

- [ ] More AWS resources (EC2, IAM, Lambda, etc.)
- [ ] GCP provider
- [ ] State file management
- [ ] Resource dependencies and references
- [ ] Modules and reusability
- [ ] Destroy command
- [ ] Import existing resources
