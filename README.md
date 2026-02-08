<p align="center">
  <img src="assets/header.png" alt="Carina">
</p>

# Carina

**Carina** represents the keel of *Argo Navis*:
the structural backbone that quietly supports everything above it.

> [!CAUTION]
> This is an experimental project. The DSL syntax, APIs, and features are subject to change without notice.

A strongly typed infrastructure management tool written in Rust.

## Features

- **Custom DSL**: Simple, expressive syntax for defining infrastructure
- **Effects as Values**: Side effects are represented as data structures, not immediately executed
- **Strong Typing**: Catch configuration errors at parse time with schema validation
- **Data Sources**: Reference existing infrastructure without managing its lifecycle
- **Provider Architecture**: Extensible provider system for multi-cloud support
- **Modules**: Reusable infrastructure components with typed inputs/outputs
- **State Management**: Remote state storage with locking (S3 backend)
- **LSP Support**: Editor integration with completion, diagnostics, and syntax highlighting
- **Terraform-like Workflow**: Familiar `validate`, `plan`, `apply`, `destroy` commands

## Installation

```bash
cargo build --release
```

The binary will be available at `target/release/carina`.

## Quick Start

### 1. Define your infrastructure

Create a `.crn` file:

```hcl
# main.crn

provider aws {
  region = aws.Region.ap_northeast_1
}

let main_vpc = aws.vpc {
  name       = "main-vpc"
  cidr_block = "10.0.0.0/16"
}

let web_sg = aws.security_group {
  name   = "web-sg"
  vpc_id = main_vpc.id
}

aws.security_group.ingress_rule {
  name              = "http"
  security_group_id = web_sg.id
  from_port         = 80
  to_port           = 80
  protocol          = "tcp"
  cidr_blocks       = ["0.0.0.0/0"]
}
```

### 2. Validate

```bash
$ carina validate main.crn
Validating...
✓ 3 resources validated successfully.
  • vpc.main-vpc
  • security_group.web-sg
  • security_group.ingress_rule.http
```

### 3. Plan

```bash
$ carina plan main.crn
Execution Plan:

  + vpc
      name: "main-vpc"
      cidr_block: "10.0.0.0/16"
        └─ + security_group
              name: "web-sg"
              vpc_id: main_vpc.id
              └─ + security_group.ingress_rule
                    name: "http"
                    security_group_id: web_sg.id

Plan: 3 to add, 0 to change, 0 to destroy.
```

### 4. Apply

```bash
$ carina apply main.crn
Applying changes...

  ✓ Create vpc.main-vpc
  ✓ Create security_group.web-sg
  ✓ Create security_group.ingress_rule.http

Apply complete! 3 changes applied.
```

## DSL Syntax

### Provider Block

```hcl
provider aws {
  region = aws.Region.ap_northeast_1
}
```

### Resources

**Anonymous resources** - ID is derived from the `name` attribute:

```hcl
aws.security_group.ingress_rule {
  name              = "http"
  security_group_id = web_sg.id
  from_port         = 80
  to_port           = 80
  protocol          = "tcp"
}
```

**Named resources** - Use `let` binding for referencing:

```hcl
let web_sg = aws.security_group {
  name   = "web-sg"
  vpc_id = main_vpc.id
}
```

### Data Sources

Use the `read` keyword to reference existing infrastructure without managing its lifecycle. Data sources are read-only and cannot be created, modified, or deleted by Carina.

```hcl
# Read an existing VPC (data source)
let existing_vpc = read aws.vpc {
  name = "production-vpc"
}

# Create a new subnet that references the existing VPC
let app_subnet = aws.subnet {
  name              = "app-subnet"
  vpc_id            = existing_vpc.id
  cidr_block        = "10.0.100.0/24"
  availability_zone = aws.AvailabilityZone.ap_northeast_1a
}
```

In plan output, read effects are displayed with the `<=` symbol to distinguish them from mutations.

### Enum Values

Enum values support multiple formats. The shorthand forms are automatically resolved based on schema context:

```hcl
# Full namespace format
versioning = aws.s3.VersioningStatus.Enabled

# Type.value format
versioning = VersioningStatus.Enabled

# Value-only format (shortest, recommended)
versioning = Enabled
instance_tenancy = dedicated
```

### Modules

Modules enable reusable infrastructure components with typed inputs and outputs.

**Module definition** (`modules/web_tier/main.crn`):

```hcl
input {
  vpc: ref(aws.vpc)
  cidr_blocks: list(cidr)
  enable_https: bool = true
}

output {
  security_group: ref(aws.security_group) = web_sg.id
}

let web_sg = aws.security_group {
  name        = "web-sg"
  vpc_id      = input.vpc
  description = "Security group for web servers"
}
```

**Using modules**:

```hcl
import "./modules/web_tier" as web_tier

let main_vpc = aws.vpc {
  name       = "main-vpc"
  cidr_block = "10.0.0.0/16"
}

web_tier {
  vpc         = main_vpc.id
  cidr_blocks = ["10.0.1.0/24", "10.0.2.0/25"]
}
```

**Inspect module structure**:

```bash
$ carina module info modules/web_tier
Module: web_tier

=== REQUIRES ===

  vpc: ref(aws.vpc)  (required)
  cidr_blocks: list(cidr)  (required)
  enable_https: bool = true

=== CREATES ===

  input { vpc: ref(aws.vpc) }
    └── web_sg: aws.security_group
       ├── http: aws.security_group.ingress_rule
       └── https: aws.security_group.ingress_rule

=== EXPOSES ===

  security_group: ref(aws.security_group)
    <- from: web_sg
```

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
├── carina-cli/              # CLI application
├── carina-core/             # Core library (provider-agnostic)
│   ├── src/
│   │   ├── effect.rs        # Effect type definitions
│   │   ├── plan.rs          # Plan (collection of Effects)
│   │   ├── resource.rs      # Resource and State types
│   │   ├── provider.rs      # Provider trait
│   │   ├── interpreter.rs   # Effect interpreter
│   │   ├── differ.rs        # State comparison
│   │   ├── parser/          # DSL parser (pest-based)
│   │   ├── schema.rs        # Type validation (generic types only)
│   │   ├── module.rs        # Module signature and dependency graph
│   │   ├── module_resolver.rs # Module import and expansion
│   │   └── formatter/       # Code formatter
│   └── ...
├── carina-provider-aws/     # AWS provider implementation
│   └── src/schemas/         # AWS-specific type definitions
├── carina-provider-awscc/   # AWS Cloud Control provider implementation
│   └── src/
│       ├── bin/codegen.rs   # Schema code generator from CloudFormation
│       └── schemas/generated/ # Auto-generated schemas from CloudFormation
├── carina-state/            # State management
│   └── src/backends/        # State backends (S3, etc.)
└── carina-lsp/              # Language Server Protocol implementation
```

## AWS Provider

The AWS provider requires valid AWS credentials. Configure via:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- AWS credentials file (`~/.aws/credentials`)
- IAM roles (when running on AWS)

### Using with aws-vault

```bash
aws-vault exec myprofile -- carina apply main.crn
```

## Commands

### Format

Format `.crn` files:

```bash
# Format a single file
$ carina fmt example.crn

# Format all .crn files in current directory
$ carina fmt

# Format recursively
$ carina fmt -r

# Check formatting without modifying files
$ carina fmt --check

# Show diff of formatting changes
$ carina fmt --diff
```

### Destroy

Remove all resources defined in a configuration:

```bash
$ carina destroy main.crn
Destroy Plan:

  - security_group.ingress_rule.http
  - security_group.web-sg
  - vpc.main-vpc

Plan: 3 to destroy.

Do you really want to destroy all resources?
  This action cannot be undone. Type 'yes' to confirm.

  Enter a value: yes

Destroying resources...

  ✓ Delete security_group.ingress_rule.http
  ✓ Delete security_group.web-sg
  ✓ Delete vpc.main-vpc

Destroy complete! 3 resources destroyed.
```

Use `--auto-approve` to skip the confirmation prompt.

### Module Info

Inspect module structure and dependencies:

```bash
$ carina module info modules/web_tier
```

## State Management

Carina supports remote state storage for tracking infrastructure state across team members and CI/CD pipelines.

### S3 Backend

Store state in an S3 bucket:

```hcl
backend s3 {
  bucket      = "my-carina-state"
  key         = "infra/prod/carina.crnstate"
  region      = aws.Region.ap_northeast_1
  encrypt     = true
  auto_create = true  # Automatically create the bucket if it doesn't exist
}

provider aws {
  region = aws.Region.ap_northeast_1
}

aws.s3.bucket {
  name = "my-app-data"
}
```

The state file tracks:
- Resource states and attributes
- Serial number for change detection
- Locking to prevent concurrent modifications

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

- [x] Resource dependencies and references
- [x] Modules and reusability
- [x] Destroy command
- [x] State file management (S3 backend)
- [x] Data sources (read existing infrastructure)
- [ ] More AWS resources (EC2, IAM, Lambda, etc.)
- [ ] GCP provider
- [ ] Import existing resources
