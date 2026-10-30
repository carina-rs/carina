---
title: "State Management"
description: "Learn how Carina tracks infrastructure state, configure an S3 backend, import existing resources, move and remove state entries, and reference upstream state."
---

Carina uses a state file to track which resources it manages and their current attributes. This guide covers how to configure state storage, manipulate state entries, and share state between projects.

## How state works

When you run `carina apply`, Carina records each managed resource and its attributes in a state file. On subsequent runs, it compares the desired state (your `.crn` files) with the recorded state to determine what needs to change.

By default, state is stored locally as `carina.state.json`. Use `backend local { path = '...' }` to store it at another local path, and configure the S3 backend for team usage.

`apply` and `destroy` save state while effects run, not only at the end, so an interrupted run (Ctrl+C, a killed CI job) keeps the results of the operations that already finished. A create that the cloud accepted but that did not finish is recorded as in flight, and the next plan resumes it instead of creating the resource a second time.

If an initial create fails after a request may have reached the cloud but before
the provider returns an identifier or continuation, state format v20 records a
token-less `create_outcome_unknown` row. Carina does not refresh, destroy, or
re-create that address automatically. If the resource exists, add the import
block shown by `carina plan`. If it does not exist, remove only that recovery
row with `carina state forget <TYPE> <IDENTITY>` and then plan again.

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
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
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
`carina-backend.lock`. If they differ, a bare `carina init` warns and
leaves migration pending. Project-backed readers — `plan`, `state list`,
`state show`, `state lookup`, `export`, and `upstream_state` consumers — warn
and read state from the old backend recorded in the lock. Explicit
`--state-url` reads bypass this selection. `apply`, `destroy`, and `state
refresh` refuse to run until you opt in with `--migrate-state` (or revert the
backend configuration), so you never silently abandon the old state. With
`--migrate-state`, `init` copies the state to the new backend, verifies the
copy, and rewrites the lock.

A local source is deleted after the verified copy; a remote source is
kept as a recoverable backup. Add `--force` only when a *different*
state already exists at the new backend and you intend to replace it.

See the [`init` reference](/reference/cli/init/) for the full semantics
and crash-recovery behavior.

## Importing existing resources

To bring an existing cloud resource under Carina management, use the `import` block:

```crn
import {
  to = awscc.ec2.Vpc 'main_vpc'
  id = 'vpc-0abc123def456'
}

let main_vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

- `to` is the resource type plus the `let` binding name of the resource that adopts the existing one. An address that matches no declared resource does not attach to it: an anonymous resource would be planned as a separate create.
- `id` is the cloud provider's identifier for the existing resource

Run `carina plan` to verify the import matches the resource definition, then `carina apply` to record it in state.

## Moving resources in state

When you rename a resource or reorganize your code, use the `moved` block to update the state without destroying and recreating the resource:

```crn
moved {
  from = awscc.ec2.Vpc 'vpc'
  to   = awscc.ec2.Vpc 'main_vpc'
}

let main_vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

Here the resource was previously bound as `let vpc` and is now bound as `let main_vpc`. `from` and `to` must use the same provider and resource type; changing either is not a move (use `removed` plus `import` instead). After the move is applied, you can remove the `moved` block from your code.

## Removing resources from state

To stop managing a resource without destroying it in the cloud, use the `removed` block:

```crn
removed {
  from = awscc.ec2.Vpc 'main_vpc'
}
```

This removes the resource from Carina's state but leaves the actual cloud resource untouched. This is useful when transferring ownership of a resource to another tool or project.

## Referencing upstream state

To read outputs from another Carina project's state, use `upstream_state`. The upstream project publishes values with an `exports` block:

```crn
# ../network/exports.crn
exports {
  vpc_id: aws.ec2.Vpc.Id = vpc.vpc_id
}
```

The downstream project reads them:

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

The `upstream_state` expression points at an upstream project's directory. Carina loads that directory's configuration, resolves its backend, reads its state, and exposes the upstream's `exports` through the `let`-bound name. If that project has a pending backend migration, Carina warns and reads its locked old backend. In this example, `network.vpc_id` references the `vpc_id` value published by the `../network` project's `exports` block.

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

Carina locks state during `apply` and `destroy` to prevent concurrent modifications, with both backends: the local backend writes a lock file next to the state file, and the S3 backend writes a `<key>.lock` object. `plan` does not take the lock; it warns if the state changed while it was planning. A running operation keeps renewing its lock, and a lock left behind by a crashed run expires after an hour and can then be taken over. To release a stuck lock sooner, use `carina force-unlock <lock-id>`.

You can disable locking with the `--lock=false` flag:

```bash
carina apply --lock=false
```
