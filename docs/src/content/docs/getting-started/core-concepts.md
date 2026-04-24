---
title: Core Concepts
description: The key ideas behind Carina -- effects as values, provider architecture, state management, and DSL syntax.
---

## Effects as values

Carina treats infrastructure operations as data. Instead of immediately creating or deleting resources, it builds a **Plan** -- a collection of **Effects** that describe what would happen:

| Effect | Meaning |
|--------|---------|
| **Create** | Provision a new resource |
| **Update** | Modify an existing resource in place |
| **Delete** | Remove a resource |
| **Replace** | Delete and recreate a resource (when a create-only property changes) |
| **Read** | Query an existing resource without managing it (data source) |

Effects are values you can inspect before anything is executed. Running `carina plan` produces a Plan; running `carina apply` executes it.

## Plan before apply

The workflow is always:

1. **Validate** -- `carina validate .` checks syntax and schema without cloud access
2. **Plan** -- `carina plan .` computes the diff between desired state (`.crn` files) and current state
3. **Apply** -- `carina apply .` executes the plan

This separation ensures you always see what will change before it happens.

## Provider architecture

Providers are WASM components loaded at runtime. Each provider runs in a sandboxed environment with:

- **HTTP allow-list** -- providers can only call pre-approved API endpoints
- **Environment variable restriction** -- only explicitly passed variables are visible
- **Resource limits** -- memory and execution bounds

Configure a provider in your `.crn` file:

```crn
provider awscc {
  source  = 'github.com/carina-rs/carina-provider-awscc'
  version = '0.5.0'
  region  = awscc.Region.ap_northeast_1
}
```

Multiple providers can be used in the same file:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

provider aws {
  region = aws.Region.ap_northeast_1
}
```

## State management

Carina records managed resources in a JSON state file (`carina.state.json` by default). The state tracks:

- Resource identifiers (cloud provider IDs)
- Current attribute values
- Resource dependencies

On each run, Carina compares the desired configuration against the state to determine what needs to change. For team usage, configure an [S3 backend](/guides/state-management/#configuring-the-s3-backend) to share state.

## DSL syntax

Carina uses `.crn` files with a purpose-built DSL. Key constructs:

### Provider block

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}
```

### Anonymous resources

When no other resource needs to reference it:

```crn
awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}
```

### Named resources with `let`

When you need to reference a resource's attributes:

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.Subnet {
  vpc_id     = vpc.vpc_id
  cidr_block = '10.0.1.0/24'
}
```

### Data sources with `read`

Query existing resources without managing them:

```crn
let caller = read aws.sts.caller_identity {}
```

### Lifecycle blocks

Control resource behavior during updates and deletions:

```crn
awscc.s3.Bucket {
  bucket_name = 'my-bucket'

  lifecycle {
    force_delete = true
  }
}
```

Available lifecycle options:

| Option | Default | Effect |
|--------|---------|--------|
| `force_delete` | `false` | Force-delete the resource (e.g., non-empty S3 buckets) |
| `create_before_destroy` | `false` | Create the replacement before destroying the old resource |
| `prevent_destroy` | `false` | Block any plan that would destroy this resource |
