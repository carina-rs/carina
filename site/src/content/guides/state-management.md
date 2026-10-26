---
title: "State Management"
description: "Learn how Carina tracks infrastructure state, configure an S3 backend, import existing resources, move and remove state entries, and reference upstream state."
---

Carina uses a state file to track which resources it manages and their current attributes. This guide covers how to configure state storage, manipulate state entries, and share state between projects.

## How state works

When you run `carina apply`, Carina records each managed resource and its attributes in a state file. On subsequent runs, it compares the desired state (your `.crn` files) with the recorded state to determine what needs to change.

By default, state is stored locally as `carina.state.json`. For team usage, configure a remote S3 backend.

### State format upgrades

State format v17 adds a top-level `exports_version` that versions the exports
encoding separately from the rest of the state file. Readers that only need
exports (`upstream_state` and `carina export`) check `exports_version` instead
of the state format version, so a later state format bump does not break a
downstream project that runs an older Carina, as long as that Carina supports
the exports encoding. `exports_version` changes only when the encoding of
exported values changes.

This protection starts with v17 itself. A Carina that predates v17 reads
exports through the full state format check, so it rejects a v17 state with
"State file version 17 is newer than supported version 16". Upgrade the projects that read an
upstream state before applying the upstream project with v17.

`carina state lookup` reads resources only. To read exports from another
project's state, use `carina export --state-url <URL>`.

State format v14 stores a redacted secret structurally as a single-key object:

```json
{"$secret":"$argon2id$..."}
```

Ordinary strings are always strings, even when their text starts with
`_secret:` and otherwise looks like an older secret marker. User map keys such
as `$secret` are stored as `$$secret` so they cannot be confused with the
host-owned tag. Loading v13 or older state migrates legacy secret markers only
inside current and deposed resource attributes; exported strings retain their
literal value.

State format v13 gives host-owned values a reserved `$` namespace. Canonical
enums are stored under `$enum`; user map keys that begin with `$` are escaped
with one additional `$` and restored when state is read. Loading an older state
file migrates legacy enum objects and `$`-prefixed user keys automatically.

The v14 loader also preserves the v12 migration for old raw secret markers.
An `_secret:argon2:<hash>` value from an earlier state version is bound to the
provider, resource type, stored identity, and top-level attribute key from its
own row, then stored as a `$secret` object carrying an
`argon2-legacy:<row-salt-hex>:<hash-hex>` digest. This load-time upgrade does
not require the plaintext and does not create a resource change. The next state
write that has the matching plaintext replaces the legacy digest with a
standard `$argon2id$...` PHC digest containing a fresh random salt and explicit
Argon2 parameters.

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

1. Creates the S3 bucket with public access blocked
2. Appends an `aws.s3.Bucket` resource definition to your `.crn` file
3. Registers the bucket as a **protected resource** in state, preventing it from being accidentally modified or destroyed

`carina plan` shows the upcoming bucket creation as a bootstrap plan before the main resource plan.

To disable auto-creation and require the bucket to exist beforehand, set `auto_create = false`. If the bucket is missing, `carina plan` and `carina apply` will fail with an error.

To delete an auto-created state bucket, use:

```bash
carina state bucket-delete <bucket-name> --force
```

This is a destructive operation that removes the bucket and all state history.

## Moving state to a different backend

<!-- derived-from ../reference/cli/init.md -->

When you change the `backend` block — switching from local state to S3,
or changing the `key` while reorganizing component directories — the
state file must move with it. There is a single answer:

```bash
carina init --migrate-state
```

`init` compares the configured backend against the one recorded in
`carina-backend.lock`. If they differ it copies the state from the old
backend to the new one, verifies the copy, and rewrites the lock. A bare
`carina init` (and any `plan`/`apply`/`destroy`) **refuses** when the
backend changed, so you never silently abandon the old state — you have
to opt in with `--migrate-state` (or revert the backend configuration).

A local source is deleted after the verified copy; a remote source is
kept as a recoverable backup. Add `--force` only when a *different*
state already exists at the new backend and you intend to replace it.

See the [`init` reference](/reference/cli/init/) for the full semantics
and crash-recovery behavior.

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
