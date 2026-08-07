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
git submodule update --init --recursive  # WIT definitions (carina-plugin-wit)
cargo build --release
```

The binary will be available at `target/release/carina`.

## Quick Start

### 1. Define your infrastructure

Create a `.crn` file:

```hcl
# main.crn

provider aws {
  source   = 'github.com/carina-rs/carina-provider-aws'
  revision = 'main'
  region   = aws.Region.ap_northeast_1
}

let main_vpc = aws.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'main-vpc'
  }
}

let web_sg = aws.ec2.SecurityGroup {
  group_name  = 'web-sg'
  description = 'Web server security group'
  vpc_id      = main_vpc.vpc_id
}

aws.ec2.SecurityGroupIngress {
  group_id    = web_sg.group_id
  description = 'Allow HTTP'
  ip_protocol = tcp
  from_port   = 80
  to_port     = 80
  cidr_ip     = '0.0.0.0/0'
}
```

### 2. Init

Download and install the provider plugins declared in the `provider` blocks:

```bash
$ carina init .
Resolving 1 provider(s)...
Resolving revision 'main' for provider 'aws'...
Installed provider 'aws' (github.com/carina-rs/carina-provider-aws@2906c22e0b25)
1 provider(s) installed in .carina/providers/
Initialized successfully.
```

### 3. Validate

Point `carina` at the directory containing your `.crn` files (all `.crn` files in that directory are merged):

```bash
$ carina validate .
Validating...
✓ 3 resources validated successfully.
  • aws.ec2.Vpc.main_vpc
  • aws.ec2.SecurityGroup.web_sg
  • aws.ec2.SecurityGroupIngress.aws_ec2_security_group_ingress_d819070a
```

### 4. Plan

```bash
$ carina plan .
Execution Plan:

  + aws.ec2.Vpc main_vpc
      cidr_block: "10.0.0.0/16"
      tags:
        Name: "main-vpc"
      vpc_id: (known after apply)
        │
        └─ + aws.ec2.SecurityGroup web_sg
              description: "Web server security group"
              group_name: "web-sg"
              vpc_id: main_vpc.vpc_id
              group_id: (known after apply)
              │
              └─ + aws.ec2.SecurityGroupIngress aws_ec2_security_group_ingress_d819070a
                    cidr_ip: "0.0.0.0/0"
                    description: "Allow HTTP"
                    from_port: 80
                    group_id: web_sg.group_id
                    ip_protocol: tcp
                    to_port: 80
                    security_group_rule_id: (known after apply)

Plan: 3 to add, 0 to change, 0 to destroy.
```

### 5. Apply

```bash
$ carina apply .
Applying changes...

  ✓ Create aws.ec2.Vpc main_vpc took 2.1s 1/3
  ✓ Create aws.ec2.SecurityGroup web_sg took 1.4s 2/3
  ✓ Create aws.ec2.SecurityGroupIngress aws_ec2_security_group_ingress_d819070a took 0.8s 3/3
  ✓ State saved (serial: 1)

Apply complete! 3 changes applied.
```

## DSL Syntax

### Provider Block

Declares which provider plugin to use (`source`/`revision`, installed by `carina init`) and its configuration:

```hcl
provider aws {
  source   = 'github.com/carina-rs/carina-provider-aws'
  revision = 'main'
  region   = aws.Region.ap_northeast_1
}
```

### Resources

Resource types are written as `<provider>.<service>.<ResourceType>`.

**Anonymous resources** - No binding name; Carina assigns an auto-generated identity:

```hcl
aws.ec2.SecurityGroupIngress {
  group_id    = web_sg.group_id
  ip_protocol = tcp
  from_port   = 80
  to_port     = 80
  cidr_ip     = '0.0.0.0/0'
}
```

**Named resources** - Use `let` binding for referencing:

```hcl
let web_sg = aws.ec2.SecurityGroup {
  group_name  = 'web-sg'
  description = 'Web server security group'
  vpc_id      = main_vpc.vpc_id
}
```

### Data Sources

Use the `read` keyword to reference existing infrastructure without managing its lifecycle. Data sources are read-only and cannot be created, modified, or deleted by Carina.

```hcl
# Look up an existing S3 bucket (data source)
let assets = read aws.s3.Bucket {
  bucket = 'my-existing-assets-bucket'
}
```

The looked-up attributes (`assets.arn`, `assets.region`, ...) can be referenced from other resources. In plan output, read effects are displayed with the `<=` symbol to distinguish them from mutations.

### Enum Values

Enum values support multiple formats. The shorthand forms are automatically resolved based on schema context:

```hcl
# Full namespace format
instance_tenancy = aws.ec2.Vpc.InstanceTenancy.dedicated

# Type.value format
instance_tenancy = InstanceTenancy.dedicated

# Value-only format (shortest, recommended)
instance_tenancy = dedicated
```

### Nested Objects (Struct Types)

Some resources support nested objects for inline configuration. Use repeated blocks for multiple items:

```hcl
awscc.ec2.SecurityGroup {
  vpc_id            = vpc.vpc_id
  group_description = 'Web server security group'

  security_group_ingress {
    ip_protocol = tcp
    from_port   = 80
    to_port     = 80
    cidr_ip     = '0.0.0.0/0'
  }

  security_group_ingress {
    ip_protocol = tcp
    from_port   = 443
    to_port     = 443
    cidr_ip     = '0.0.0.0/0'
  }
}
```

Array syntax is also supported:

```hcl
  security_group_ingress = [
    {
      ip_protocol = tcp
      from_port   = 80
      to_port     = 80
      cidr_ip     = '0.0.0.0/0'
    }
  ]
```

### Modules

Modules enable reusable infrastructure components with typed arguments and attributes. A module is a directory of `.crn` files.

**Module definition** (`modules/web_tier/main.crn`):

```hcl
arguments {
  vpc_id: aws.ec2.Vpc {
    description = 'The VPC to deploy resources into'
  }
  environment: String {
    description = 'Deployment environment name'
  }
  enable_https: Bool = true
}

attributes {
  security_group_id: aws.ec2.SecurityGroup = web_sg.group_id
}

let web_sg = aws.ec2.SecurityGroup {
  group_name  = 'web-sg'
  description = 'Security group for web servers'
  vpc_id      = vpc_id
}
```

**Using modules**:

```hcl
let web_tier = use {
  source = './modules/web_tier'
}

let main_vpc = aws.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

let web = web_tier {
  vpc_id      = main_vpc.vpc_id
  environment = 'production'
}
```

**Inspect module structure**:

```bash
$ carina module info modules/web_tier
Module: web_tier

=== ARGUMENTS ===

  vpc_id: aws.ec2.Vpc  (required)
    The VPC to deploy resources into
  environment: String  (required)
    Deployment environment name
  enable_https: Bool = true

=== CREATES ===

  web_sg: aws.ec2.SecurityGroup

=== ATTRIBUTES ===

  security_group_id: aws.ec2.SecurityGroup
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
┌──────────┐
│ Provider │  Execute Effects (AWS, GCP, etc.)
└──────────┘
```

### Core Concepts

- **Resource**: Desired state declared in DSL
- **State**: Current state fetched from infrastructure
- **Effect**: Represents a side effect (Create, Update, Delete, Read)
- **Plan**: Collection of Effects to be executed
- **Provider**: Abstraction for infrastructure operations

## Project Structure

```
carina/
├── carina-cli/              # CLI application
├── carina-core/             # Core library (provider-agnostic)
│   ├── src/
│   │   ├── effect.rs        # Effect type definitions
│   │   ├── plan.rs          # Plan (collection of Effects)
│   │   ├── resource/        # Resource and State types
│   │   ├── provider.rs      # Provider trait
│   │   ├── differ/          # State comparison
│   │   ├── parser/          # DSL parser (pest-based)
│   │   ├── schema/          # Type validation (generic types only)
│   │   ├── module.rs        # Module signature and dependency graph
│   │   ├── module_resolver/ # Module import and expansion
│   │   └── formatter/       # Code formatter
│   └── ...
├── carina-plugin-host/      # WASM plugin host for provider plugins
├── carina-plugin-sdk/       # SDK for building WASM provider plugins
├── carina-plugin-wit/       # WIT interface definitions (git submodule)
├── carina-provider-mock/    # Mock provider for testing
├── carina-provider-protocol/ # Protocol definitions for provider communication
├── carina-provider-resolver/ # Resolves and loads provider plugins
├── carina-state/            # State management
│   └── src/backends/        # State backends (S3, etc.)
├── carina-lsp/              # Language Server Protocol implementation
└── carina-tui/              # Terminal UI for plan display
```

## AWS Provider

AWS providers are distributed as separate repositories under [carina-rs](https://github.com/carina-rs) and loaded as WASM plugins by the core runtime:

- [carina-provider-aws](https://github.com/carina-rs/carina-provider-aws) — AWS provider (Smithy-based codegen)
- [carina-provider-awscc](https://github.com/carina-rs/carina-provider-awscc) — AWS Cloud Control provider

Configure valid AWS credentials via:

- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- AWS credentials file (`~/.aws/credentials`)
- IAM roles (when running on AWS)

### Using with aws-vault

```bash
aws-vault exec myprofile -- carina apply .
```

## Commands

### Format

Format `.crn` files:

```bash
# Format all .crn files in current directory
$ carina fmt

# Format all .crn files in a directory
$ carina fmt path/to/dir

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
$ carina destroy .
Destroy Plan:

  - aws.ec2.Vpc main_vpc
      cidr_block: "10.0.0.0/16"
      vpc_id: "vpc-0123456789abcdef0"
        │
        └─ - aws.ec2.SecurityGroup web_sg
              group_name: "web-sg"
              vpc_id: "vpc-0123456789abcdef0"
              │
              └─ - aws.ec2.SecurityGroupIngress aws_ec2_security_group_ingress_d819070a
                    from_port: 80
                    to_port: 80

Plan: 3 to destroy.

Do you really want to destroy all resources?
  This action cannot be undone. Type 'yes' to confirm.

  Enter a value: yes

Destroying resources...

  ✓ Delete aws.ec2.SecurityGroupIngress aws_ec2_security_group_ingress_d819070a took 0.9s 1/3
  ✓ Delete aws.ec2.SecurityGroup web_sg took 1.2s 2/3
  ✓ Delete aws.ec2.Vpc main_vpc took 1.8s 3/3
  ✓ State saved (serial: 2)

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
  bucket = 'my-carina-state'
  key    = 'infra/prod/carina.state.json'
}
```

Server-side encryption (`encrypt`) and automatic bucket creation (`auto_create`) are enabled by default and can be turned off explicitly:

```hcl
backend s3 {
  bucket      = 'my-carina-state'
  key         = 'infra/prod/carina.state.json'
  encrypt     = true
  auto_create = false  # Do not create the bucket automatically
}
```

The state file tracks:
- Resource states and attributes
- Serial number for change detection
- Locking to prevent concurrent modifications

## Development

### Run tests

Tests run under [nextest](https://nexte.st/) (`cargo install cargo-nextest --locked`); doctests need a separate run:

```bash
cargo nextest run
cargo test --workspace --doc
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
- [x] Import existing resources
- [ ] More AWS resources (EC2, IAM, Lambda, etc.)
- [ ] GCP provider
