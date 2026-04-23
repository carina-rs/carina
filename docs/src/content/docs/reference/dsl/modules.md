---
title: Modules
description: Module definition, arguments, attributes, import syntax, directory modules, and module resolution in the Carina DSL.
---

Modules are reusable units of Carina configuration. A module defines a set of resources along with input parameters (`arguments`) and output values (`attributes`).

## Block Roles at a Glance

A directory of `.crn` files may contain three interface-like blocks. They share a similar surface — `name: Type = expr` — but play different roles, and the `=` part means something different in each:

| Block | Role | `=` expr means | Who consumes it |
|---|---|---|---|
| `arguments` | declare inputs | **optional default** for the parameter | callers of the module (`module { name = value }`) |
| `attributes` | define module outputs | **required value** for the output | callers who bind the module (`let m = module { ... }; m.foo`) |
| `exports` | define upstream-state outputs | **required value** for the export | other projects referencing this directory via `upstream_state` |

The blocks fall into two groups by role:

- **Declaration side** (`arguments`): the type annotation is required and load-bearing; `= expr` is an optional fallback when the caller omits the argument.
- **Definition side** (`attributes`, `exports`): the value expression is required and load-bearing; the type annotation is an optional extra.

Because `description` and `validation` are input-side concerns (documenting what callers should pass, and checking what they did pass), they are available on `arguments` entries through the block form but are deliberately absent from `attributes` and `exports`. Outputs are values produced internally from bindings the module already controls, so there is nothing to validate and no caller-facing documentation to attach beyond the type.

`attributes` and `exports` share their body grammar but are intentionally kept separate because their consumers are different: `attributes` is the interface a module presents to its direct caller, while `exports` is the interface a directory presents to other projects through `upstream_state`.

## Module Structure

A module is a directory containing one or more `.crn` files that together declare `arguments` and `attributes` blocks. Carina merges every `.crn` file in the directory as peers; there is no privileged filename like `main.crn`.

```crn
# modules/network/main.crn  (any filename works — main.crn is just convention)

arguments {
  cidr_block : String
  subnet_cidr: String
  az         : String
}

let vpc = awscc.ec2.Vpc {
  cidr_block = cidr_block

  tags = {
    Name = 'module-test'
  }
}

let subnet = awscc.ec2.Subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = subnet_cidr
  availability_zone = az

  tags = {
    Name = 'module-test-subnet'
  }
}

attributes {
  vpc_id   : awscc.ec2.Vpc = vpc.vpc_id
  subnet_id: awscc.ec2.Subnet = subnet.subnet_id
}
```

## Arguments Block

The `arguments` block defines the parameters a module accepts from its caller.

### Basic Parameters

Each parameter has a name and a type:

```crn
arguments {
  cidr_block: String
  count     : Int
  enabled   : Bool
}
```

### Default Values

Parameters can have default values:

```crn
arguments {
  cidr_block: String
  az        : String = 'ap-northeast-1a'
  enable_dns: Bool   = true
}
```

### Block Form with Description and Validation

Parameters can use an expanded block form for description, default values, and validation rules:

```crn
arguments {
  instance_count: Int {
    description = 'Number of instances to create'
    default     = 1
    validation {
      condition     = instance_count >= 1 && instance_count <= 10
      error_message = 'Instance count must be between 1 and 10'
    }
  }

  cidr_block: String {
    description = 'The CIDR block for the VPC'
    validation {
      condition     = length(cidr_block) > 0
      error_message = 'CIDR block cannot be empty'
    }
  }
}
```

### Supported Types

Parameter types can be any [type expression](/reference/dsl/types-and-values/#type-expressions):

```crn
arguments {
  name       : String
  count      : Int
  ratio      : Float
  enabled    : Bool
  subnet_ids : list(String)
  tags       : map(String)
  cidr_block : Ipv4Cidr
  role_arn   : Arn
}
```

## Attributes Block

The `attributes` block defines the output values that a module exposes to its caller.

Each attribute has a name, an optional type, and a value expression:

```crn
attributes {
  vpc_id        : awscc.ec2.Vpc = vpc.vpc_id
  subnet_id     : awscc.ec2.Subnet = subnet.subnet_id
  route_table_id: awscc.ec2.RouteTable = rt.route_table_id
}
```

The type annotation is optional; you can also use the simpler form:

```crn
attributes {
  vpc_id = vpc.vpc_id
}
```

## Importing Modules

Use `import` to load a module, then call it by name with arguments:

```crn
let network = import './modules/network'

network {
  cidr_block  = '10.0.0.0/16'
  subnet_cidr = '10.0.1.0/24'
  az          = 'ap-northeast-1a'
}
```

### Accessing Module Outputs

When a module call is bound with `let`, its `attributes` values can be accessed with dot notation:

```crn
let network = import './modules/network'

let net = network {
  cidr_block  = '10.0.0.0/16'
  subnet_cidr = '10.0.1.0/24'
  az          = 'ap-northeast-1a'
}

awscc.ec2.SecurityGroup {
  vpc_id = net.vpc_id
}
```

## Directory Modules

A module is always a **directory** containing one or more `.crn` files. Every
`.crn` file in that directory is merged into a single module — no file name
(including `main.crn`) is treated as privileged, so definitions can be split
across `arguments.crn`, `exports.crn`, `resources.crn`, etc. as naturally as
you like.

```
modules/network/
  main.crn        # optional; just one of the module's .crn files
  arguments.crn   # merged in as peers
  exports.crn
```

Single-file imports (`import "./modules/network.crn"`) are **not supported**:
the loader returns `NotADirectory` for any path that is not a directory.
If your module is currently a single `.crn` file, move it into a directory of
its own.

## Module Resolution

Import paths are resolved relative to the file containing the `import`
statement and must point at a directory:

```crn
# From project/main.crn, imports every .crn file under project/modules/network/
let network = import './modules/network'

# Relative path from current file
let helper = import '../shared/helper'
```

## Nested Modules

Modules can import and use other modules:

```crn
# modules/network_with_rt/main.crn

let network = import '../network'

arguments {
  cidr_block : String
  subnet_cidr: String
  az         : String
}

let net = network {
  cidr_block  = cidr_block
  subnet_cidr = subnet_cidr
  az          = az
}

let rt = awscc.ec2.RouteTable {
  vpc_id = net.vpc_id

  tags = {
    Name = 'nested-module-test-rt'
  }
}

attributes {
  vpc_id        : awscc.ec2.Vpc = net.vpc_id
  route_table_id: awscc.ec2.RouteTable = rt.route_table_id
}
```

## Modules with For Expressions

Modules can be used inside `for` expressions to create multiple instances:

```crn
let network = import './modules/network'

let cidrs = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}

let networks = for name, cidr in cidrs {
  network {
    cidr_block  = cidr
    subnet_cidr = cidr_subnet(cidr, 8, 1)
    az          = 'ap-northeast-1a'
  }
}
```

This creates a separate set of network resources for each entry in the map. The `name` variable (`dev`, `stg`) is used to distinguish the resources in state.

## Provider Configuration

Modules do not declare their own `provider` blocks. The provider configuration from the root `.crn` file is inherited by all modules. A module only needs to declare `arguments`, `attributes`, and resources.
