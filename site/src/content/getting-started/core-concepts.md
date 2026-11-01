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

1. **Init** -- `carina init` installs the provider plugins the configuration declares
2. **Validate** -- `carina validate` checks syntax and schema without cloud access
3. **Plan** -- `carina plan` computes the diff between desired state (`.crn` files) and current state
4. **Apply** -- `carina apply` executes the plan

This separation ensures you always see what will change before it happens.

## Provider architecture

Providers are WASM components loaded at runtime. Each provider runs in a sandboxed environment with:

- **HTTP allow-list** -- providers can only call pre-approved API endpoints
- **Environment variable restriction** -- only explicitly passed variables are visible
- **Resource limits** -- memory and execution bounds

Configure a provider in your `.crn` file. `source` and `version` tell `carina init` which plugin to install; the remaining attributes are the provider's own configuration:

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}
```

Multiple providers can be used in the same configuration:

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}

provider aws {
  source  = 'carina-rs/aws'
  version = '0.7.1'
  region  = aws.Region.ap_northeast_1
}
```

A configuration is a directory, not a single file: Carina merges every `.crn` file in the directory, so provider blocks commonly live in their own `providers.crn` next to `main.crn`.

## State management

Carina records managed resources in a JSON state file (`carina.state.json` by default). The state tracks:

- Resource identifiers (cloud provider IDs)
- Current attribute values
- Resource dependencies

On each run, Carina compares the desired configuration against the state to determine what needs to change. For team usage, configure an [S3 backend](/guides/state-management/#configuring-the-s3-backend) to share state.

## Anonymous resource identity

An anonymous resource has no `let` binding, so Carina derives its state address
from values that identify the same real-world object across plans. Providers
can mark top-level attributes or nested struct fields as identity. Identity is
always create-only: if the value changes, the provider cannot update the same
object in place.

When a schema declares one or more identity paths, the address hash uses only
those values. A different create-only setting can still force replacement, but
the replacement stays at the same address. This matters for resources such as
a budget whose nested budget name is its natural key while its notification
list is also create-only. When a schema declares no identity paths, Carina uses
configured top-level create-only values as the compatibility fallback.

Two anonymous resources of the same type and provider route cannot have equal
declared identity values; use `let` to name at least one of them. Every declared
identity value must be available as written material or an authored expression
at plan time. Carina canonicalizes a written value and hashes references and
composite expressions over references and literals by their authored spelling,
so `"/zones/${zone.id}"` has a stable address before the zone exists. An absent
value or an expression containing an unknown or unavailable leaf cannot produce
an anonymous address; write the value or give the resource a name with `let`.

When a provider first adds identity declarations, an existing state row may
still have the address produced by the older create-only rule, including one
that hashed a reference expression. Carina matches that row by the identity
values stored in state and plans a one-time `Move`. After apply, the next plan
uses the new address and is clean.

## DSL syntax

Carina uses `.crn` files with a purpose-built DSL. Key constructs:

### Provider block

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
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
let caller = read aws.sts.CallerIdentity {}
```

### Directives blocks

Control how Carina handles a resource during updates and deletions.
The `directives` block carries Carina-side instructions — it is not a
resource attribute, and the cloud provider never sees it:

```crn
awscc.s3.Bucket {
  bucket_name = 'my-bucket'

  directives {
    force_delete = true
  }
}
```

Available directives:

| Option | Default | Effect |
|--------|---------|--------|
| `force_delete` | `false` | Force-delete the resource (e.g., non-empty S3 buckets) |
| `create_before_destroy` | `false` | Create the replacement before destroying the old resource |
| `prevent_destroy` | resource schema (normally `false`) | Block any plan that would destroy this resource; an explicit `false` can opt out of schema-default protection |
| `depends_on` | `[]` | Explicit ordering edges to sibling `let` bindings |
| `provider` | (kind default) | Route this resource to a named provider instance — see [Named provider instances](/reference/dsl/syntax#named-provider-instances) |
| `ignore_changes` | `[]` | Let another system own selected authored attribute paths after creation |

Boolean directives resolve after module arguments and loop variables are
substituted. For schema-protected resource types, a `false` supplied by an
omitted module argument's default cannot disable protection; the caller must
pass that value explicitly. A defaulted iterable contributes its origin only
when the directive reads a loop variable. Literal `prevent_destroy = false`
stays explicit inside loops and conditional resource bodies.

For syntax, validation rules, and lifecycle semantics, see the
[directives reference](/reference/dsl/directives/).
