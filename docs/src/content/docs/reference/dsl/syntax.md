---
title: Syntax
description: File structure, provider blocks, resource blocks, let bindings, and comments in the Carina DSL.
---

Carina configuration files use the `.crn` extension. A file consists of a sequence of top-level statements that declare infrastructure resources and their relationships.

## File Structure

A `.crn` file contains zero or more top-level statements in any order:

```crn
# Provider configuration
provider awscc {
  region = awscc.Region.ap_northeast_1
}

# Backend configuration (optional)
backend s3 {
  bucket = 'my-state-bucket'
  key    = 'infra/carina.state.json'
  region = 'ap-northeast-1'
}

# Resource declarations
let vpc = awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'main'
  }
}

awscc.ec2.internet_gateway {
  tags = {
    Name = 'main'
  }
}
```

## Statements

The following statements are valid at the top level of a `.crn` file:

| Statement | Purpose |
|-----------|---------|
| `provider` | Configure a cloud provider |
| `backend` | Configure remote state storage |
| `let` | Bind a name to a resource or value |
| Anonymous resource | Declare a resource without a binding |
| `import` (state) | Import an existing cloud resource into state |
| `removed` | Remove a resource from state without deleting it |
| `moved` | Rename a resource in state |
| `for` | Iterate to create multiple resources |
| `if` | Conditionally create resources |
| `fn` | Define a reusable function |
| `require` | Assert a condition with an error message |
| `arguments` | Declare module input parameters |
| `attributes` | Declare module output values |

## Provider Block

Every `.crn` file that declares resources must configure at least one provider. The provider block specifies which cloud provider to use and its settings.

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}
```

Multiple providers can be configured in the same file:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

provider aws {
  region = aws.Region.ap_northeast_1
}
```

## Resource Blocks

Resources represent cloud infrastructure objects. A resource block specifies the provider, service, and resource type using dot notation, followed by a block of attributes.

### Anonymous Resources

When you do not need to reference a resource elsewhere, declare it without a binding:

```crn
awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'main'
  }
}
```

The resource is identified in state by its `name` attribute or a hash of its attributes.

### Named Resources (Let Bindings)

Use `let` to bind a name to a resource so you can reference its attributes:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'main'
  }
}

let subnet = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = '10.0.1.0/24'
  availability_zone = 'ap-northeast-1a'
}
```

The `let` keyword is also used to bind non-resource values:

```crn
let cidr = '10.0.0.0/16'
let environments = ['dev', 'stg', 'prod']
let config = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}
```

### Discard Pattern

Use `let _ =` when you want to evaluate an expression but do not need to reference the result:

```crn
let _ = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}
```

### Data Sources (Read Resources)

Use the `read` keyword to query existing cloud resources without managing them:

```crn
let identity = read aws.sts.caller_identity {}
```

The returned value can be referenced like any other bound resource:

```crn
let identity = read aws.sts.caller_identity {}

awscc.ec2.ipam_pool {
  source_resource = {
    resource_owner = identity.account_id
  }
}
```

## Attributes

Attributes are key-value pairs inside a resource block:

```crn
awscc.ec2.vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true
}
```

### Nested Blocks

Some resources accept nested blocks for repeated or structured configuration:

```crn
awscc.ec2.ipam {
  tier = advanced

  operating_region {
    region_name = 'ap-northeast-1'
  }
}
```

### Local Bindings Inside Blocks

Use `let` inside a resource block to create block-scoped variables. These are evaluated during parsing but are not sent to the provider:

```crn
awscc.ec2.vpc {
  let base_cidr = '10.0.0.0/16'

  cidr_block = base_cidr

  tags = {
    Name = "vpc-${base_cidr}"
  }
}
```

## Backend Block

The `backend` block configures where Carina stores state:

```crn
backend s3 {
  bucket = 'my-state-bucket'
  key    = 'infra/carina.state.json'
  region = 'ap-northeast-1'
}
```

When no backend is configured, state is stored locally in `carina.state.json`.

## State Manipulation

### Import

Import an existing cloud resource into Carina's state:

```crn
import {
  to = awscc.ec2.vpc 'imported_vpc'
  id = 'vpc-0123456789abcdef0'
}

awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'imported'
  }
}
```

### Moved

Rename a resource in state without destroying and recreating it:

```crn
moved {
  from = awscc.ec2.vpc 'old_name'
  to   = awscc.ec2.vpc 'new_name'
}
```

### Removed

Remove a resource from Carina's state without deleting the actual cloud resource:

```crn
removed {
  from = awscc.ec2.vpc 'old_vpc'
}
```

## Remote State

Reference resources managed by another Carina project:

```crn
let network = remote_state {
  path = 'network.state.json'
}

awscc.ec2.security_group {
  vpc_id = network.vpc.vpc_id
}
```

Remote state with an S3 backend:

```crn
let network = remote_state 's3' {
  bucket = 'my-state-bucket'
  key    = 'network/carina.state.json'
  region = 'ap-northeast-1'
}
```

## Comments

Carina supports three comment styles:

```crn
# Hash-style line comment
// Slash-style line comment

/* Block comment
   can span multiple lines */

/* Block comments /* can be nested */ like this */
```

All comments are ignored by the parser.

## User-Defined Functions

Define reusable functions with `fn`:

```crn
fn tag_name(env: string, service: string): string {
  join('-', [env, service, 'vpc'])
}

awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = tag_name('prod', 'web')
  }
}
```

See [Expressions](/reference/dsl/expressions/) for details on function definitions.

## Require Statement

Assert conditions that must be true, with a custom error message if they fail:

```crn
require length(subnets) > 0, 'At least one subnet must be specified'
require cidr_block != '', 'CIDR block cannot be empty'
```

The condition is a [validate expression](/reference/dsl/expressions/#validate-expressions) that supports comparison operators (`==`, `!=`, `>`, `<`, `>=`, `<=`) and logical operators (`&&`, `||`, `!`).
