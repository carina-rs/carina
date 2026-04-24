---
title: "State Management"
description: "Learn how Carina tracks infrastructure state, configure an S3 backend, import existing resources, move and remove state entries, and reference upstream state."
---

Carina uses a state file to track which resources it manages and their current attributes. This guide covers how to configure state storage, manipulate state entries, and share state between projects.

## How state works

When you run `carina apply`, Carina records each managed resource and its attributes in a state file. On subsequent runs, it compares the desired state (your `.crn` files) with the recorded state to determine what needs to change.

By default, state is stored locally as `carina.state.json`. For team usage, configure a remote S3 backend.

## Configuring the S3 backend

Add a `backend` block to your `.crn` file:

```crn
backend s3 {
  bucket = 'my-carina-state'
  key    = 'production/carina.state.json'
  region = 'ap-northeast-1'
}

provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

The S3 backend stores state in the specified bucket and key. It also supports state locking to prevent concurrent modifications.

### Automatic bucket creation

By default, `carina apply` automatically creates the S3 bucket if it doesn't exist. This lets you get started without any manual setup:

```crn
backend s3 {
  bucket      = 'my-carina-state'
  key         = 'production/carina.state.json'
  region      = 'ap-northeast-1'
  auto_create = true  # This is the default; can be omitted
}
```

When the bucket is auto-created, Carina:

1. Creates the S3 bucket with versioning enabled and public access blocked
2. Appends an `aws.s3.Bucket` resource definition to your `.crn` file
3. Registers the bucket as a **protected resource** in state, preventing it from being accidentally modified or destroyed

`carina plan` shows the upcoming bucket creation as a bootstrap plan before the main resource plan.

To disable auto-creation and require the bucket to exist beforehand, set `auto_create = false`. If the bucket is missing, `carina plan` and `carina apply` will fail with an error.

To delete an auto-created state bucket, use:

```bash
carina state bucket-delete <bucket-name> --force
```

This is a destructive operation that removes the bucket and all state history.

## Importing existing resources

To bring an existing cloud resource under Carina management, use the `import` block:

```crn
import {
  to = awscc.ec2.Vpc 'my-vpc'
  id = 'vpc-0abc123def456'
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

- `to` specifies the resource type and the name Carina will use in state
- `id` is the cloud provider's identifier for the existing resource

Run `carina plan` to verify the import matches the resource definition, then `carina apply` to record it in state.

## Moving resources in state

When you rename a resource or reorganize your code, use the `moved` block to update the state without destroying and recreating the resource:

```crn
moved {
  from = awscc.ec2.Vpc 'vpc'
  to   = awscc.ec2.Vpc 'main_vpc'
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

After the move is applied, you can remove the `moved` block from your code.

## Removing resources from state

To stop managing a resource without destroying it in the cloud, use the `removed` block:

```crn
removed {
  from = awscc.ec2.Vpc 'main_vpc'
}
```

This removes the resource from Carina's state but leaves the actual cloud resource untouched. This is useful when transferring ownership of a resource to another tool or project.

## Referencing upstream state

To read outputs from another Carina project's state, use `upstream_state`:

```crn
let network = upstream_state {
  source = "../network"
}

awscc.ec2.SecurityGroup {
  group_description = 'Web security group'
  vpc_id            = network.vpc_id

  tags = {
    Name = 'web-sg'
  }
}
```

The `upstream_state` expression points at an upstream project's directory. Carina loads that directory's configuration, resolves its backend, reads its state, and exposes the upstream's `exports` through the `let`-bound name. In this example, `network.vpc_id` references the `vpc_id` value published by the `../network` project's `exports` block.

`source` is required and resolved relative to the enclosing `.crn` file's directory. The upstream directory must contain a valid Carina configuration (one or more `.crn` files — no filename is privileged) with an `exports` block that publishes the values consumers need.

## State CLI commands

Carina provides several CLI commands for inspecting and managing state:

```bash
# List all managed resources
carina state list

# Show all resources with full attributes
carina state show

# Look up a specific resource or attribute
carina state lookup vpc
carina state lookup vpc.vpc_id

# Refresh state from cloud providers
carina state refresh

# Force unlock a stuck state lock
carina force-unlock <lock-id>

# Delete a state bucket (destructive)
carina state bucket-delete <bucket-name> --force
```

## State locking

When using the S3 backend, Carina automatically locks state during `apply` and `destroy` operations to prevent concurrent modifications. If a lock gets stuck (for example, after a crash), use `carina force-unlock` to release it.

You can disable locking with the `--lock=false` flag:

```bash
carina apply --lock=false
```
