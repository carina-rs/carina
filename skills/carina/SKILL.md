---
name: carina
description: Manage infrastructure with Carina - a strongly typed infrastructure management tool. Use when working with .crn files, running carina validate/plan/apply/destroy, or managing cloud infrastructure.
metadata:
  author: carina-rs
  version: "0.3.0"
---

# Carina

Carina is a strongly typed infrastructure management tool written in Rust. It uses a custom DSL (`.crn` files) to define infrastructure as code.

## Workflow

The standard workflow is:

1. **Write** `.crn` files defining your infrastructure
2. **Validate** syntax and types: `carina validate`
3. **Plan** changes: `carina plan`
4. **Apply** changes: `carina apply`

## DSL Quick Reference

### Provider Configuration

```crn
provider aws {
  region = aws.Region.ap_northeast_1
}
```

### Resource Definition

Anonymous resource (ID derived from `name` attribute):

```crn
aws.s3.bucket {
  name = "my-bucket"
}
```

Named resource with `let` binding (enables references between resources):

```crn
let main_vpc = aws.ec2.vpc {
  name       = "main-vpc"
  cidr_block = "10.0.0.0/16"
}

aws.ec2.subnet {
  name       = "public-subnet"
  vpc_id     = main_vpc.id
  cidr_block = "10.0.1.0/24"
}
```

### Modules

```crn
let web = import "./modules/web_tier" {
  vpc_id = main_vpc.id
  subnet_ids = [subnet_a.id, subnet_b.id]
}
```

### State Backend

```crn
backend s3 {
  bucket = "my-state-bucket"
  key    = "carina.state.json"
  region = "ap-northeast-1"
}
```

## CLI Commands

| Command | Description |
|---------|-------------|
| `carina validate [PATH]` | Validate .crn files (syntax, types, schemas) |
| `carina plan [PATH]` | Show execution plan without applying |
| `carina apply [PATH]` | Apply changes to reach desired state |
| `carina destroy [PATH]` | Destroy all managed resources |
| `carina fmt [PATH]` | Format .crn files |
| `carina init [PATH]` | Download and install provider plugins |
| `carina state show` | Display current state |
| `carina state lookup <RESOURCE> [ATTR]` | Query specific resource attributes |
| `carina docs` | Display embedded documentation |
| `carina docs --list` | List all available documents |
| `carina docs --search <QUERY>` | Search documentation |

## Common Patterns

### Plan with exit code for CI

```bash
carina plan --detailed-exitcode
# Exit 0 = no changes, Exit 2 = changes present
```

### Save and apply a plan

```bash
carina plan --out plan.json
carina apply plan.json
```

### Auto-approve apply

```bash
carina apply --auto-approve
```

### Format check (CI)

```bash
carina fmt --check
```

## Best Practices

- Always run `carina validate` before `carina plan`
- Use `let` bindings when resources reference each other
- Use anonymous resources (no `let`) when the binding is not referenced elsewhere
- Use modules for reusable infrastructure patterns
- Configure a remote backend (S3) for team collaboration
- Run `carina init` after adding or updating provider requirements

## Detailed Documentation

For complete documentation, use `carina docs` to access version-accurate embedded docs:

```bash
carina docs --list                       # See all available documents
carina docs reference/dsl/syntax         # DSL syntax reference
carina docs guides/writing-resources     # Resource writing guide
carina docs guides/using-modules         # Module usage guide
```
