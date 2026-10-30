---
title: Modules
description: Module definition, arguments, attributes, use syntax, directory modules, and module resolution in the Carina DSL.
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

When an export has no annotation and Carina cannot infer its type from the value, such as for a list with mixed element types, `carina validate`, `carina plan`, and `carina apply` all fail with a `type annotation required` error. Add an annotation to the export in that case.

Because `description` and `validation` are input-side concerns (documenting what callers should pass, and checking what they did pass), they are available on `arguments` entries through the block form but are deliberately absent from `attributes` and `exports`. Outputs are values produced internally from bindings the module already controls, so there is nothing to validate and no caller-facing documentation to attach beyond the type.

`attributes` and `exports` share their body grammar but are intentionally kept separate because their consumers are different: `attributes` is the interface a module presents to its direct caller, while `exports` is the interface a directory presents to other projects through `upstream_state`.

`exports` belongs to a root configuration only; a module directory that declares `exports` is rejected.

Exports cannot hold secret values, including values derived from `secret()` or `decrypt()`; `carina validate` and `carina plan` report `export '<name>' cannot hold a secret value`.

## Module Structure

A module is a directory containing one or more `.crn` files that together declare `arguments` and `attributes` blocks. Carina merges every `.crn` file in the directory as peers; there is no privileged filename like `main.crn`.

```crn
# modules/network/main.crn  (any filename works — main.crn is just convention)

arguments {
  cidr_block : Ipv4Cidr
  subnet_cidr: Ipv4Cidr
  az         : aws.AvailabilityZone.ZoneName
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

A `validation` block is local to the argument that contains it. Its
`condition` may reference that argument, but not another argument in the same
module. Use a module-level `require` when a rule relates multiple arguments.

### Require Constraints

A `require` statement declares an invariant across any of the module's
arguments. It consists of a boolean condition followed by the error message
shown when the condition is false:

```crn
arguments {
  min_size: Int
  max_size: Int
}

require min_size <= max_size, 'min_size must not exceed max_size'
```

Unlike an argument-local `validation` block, a `require` condition can
reference every argument declared by the module. An undeclared name is an
error during validation, even when one of the declared argument values is
still unresolved.

### When Constraints Run

Carina checks a constraint when its input value becomes known:

- Literal module arguments are checked during `carina validate`.
- Reference-valued arguments are checked after resolution during planning when
  the referenced value is already available.
- Values produced only by an apply-time effect are checked during apply,
  before Carina dispatches a provider operation that consumes the now-known
  argument.
- A constraint whose inputs are still unresolved at the end of an otherwise
  successful apply is an error.

Pending constraints remain attached to the module call and are checked again
at later resolution boundaries. This matters when an update or replacement
publishes a value that differs from the value available during planning.

Resource schema value constraints follow the same value-time rule. Carina
checks authored literal values first, then re-checks reference values against
schema `pattern`, `length`, and numeric `range` constraints when those values
become known. An apply-time value must pass these checks before the resource
create, update, or data-source read reaches its provider.

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
  role_arn   : aws.Arn
}
```

A structural-handle argument stays a handle inside the module, and code inside
the module must select an attribute from it. The resource type annotation
checks that the handle is the expected resource type:

```crn
arguments {
  network: awscc.ec2.Vpc
}

let subnet = awscc.ec2.Subnet {
  vpc_id = network.vpc_id
}
```

Passing a bare resource handle to a value parameter such as `String` is an
error at the module call. Passing an attribute such as `vpc.vpc_id` instead
binds a value, even when the parameter has a resource type (resource-typed
parameters accept identifier strings for provider configurations that model
references as IDs). Using a handle-backed parameter bare inside the module is
also an error.

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

An output must likewise be a value. `vpc_id = vpc` is invalid because `vpc`
is a resource handle; expose `vpc.vpc_id` (or another attribute) instead.

## Loading Modules

Use `use { source = '...' }` to load a module, then call it by name with arguments:

```crn
let network = use { source = './modules/network' }

network {
  cidr_block  = '10.0.0.0/16'
  subnet_cidr = '10.0.1.0/24'
  az          = 'ap-northeast-1a'
}
```

### Accessing Module Outputs

When a module call is bound with `let`, its `attributes` values can be accessed with dot notation:

```crn
let network = use { source = './modules/network' }

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
across `arguments.crn`, `attributes.crn`, `resources.crn`, etc. as naturally as
you like.

```
modules/network/
  main.crn        # optional; just one of the module's .crn files
  arguments.crn   # merged in as peers
  attributes.crn
```

The merged module must declare at least one `arguments` or `attributes`
block; a directory with neither is not treated as a module.

Single-file sources (`use { source = "./modules/network.crn" }`) are **not supported**:
the loader returns `NotADirectory` for any path that is not a directory.
If your module is currently a single `.crn` file, move it into a directory of
its own.

## Module Resolution

`source` paths are resolved relative to the file containing the `use`
statement and must point at a directory:

```crn
# From project/main.crn, loads every .crn file under project/modules/network/
let network = use { source = './modules/network' }

# Relative path from current file
let helper = use { source = '../shared/helper' }
```

## Nested Modules

Modules can load and use other modules:

```crn
# modules/network_with_rt/main.crn

let network = use { source = '../network' }

arguments {
  cidr_block : Ipv4Cidr
  subnet_cidr: Ipv4Cidr
  az         : aws.AvailabilityZone.ZoneName
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
let network = use { source = './modules/network' }

let cidrs = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}

let networks = for _, cidr in cidrs {
  network {
    cidr_block  = cidr
    subnet_cidr = cidr_subnet(cidr, 8, 1)
    az          = 'ap-northeast-1a'
  }
}
```

This creates a separate set of network resources for each entry in the map. Each instance is addressed in state by its map key (`networks.dev`, `networks.stg`).

## Provider Configuration

Modules inherit the provider configuration of the root configuration that loads them. Root-only blocks are rejected inside a module: `provider`, `backend`, `upstream_state`, `exports`, and the state blocks `import`, `moved`, and `removed`. A module declares `arguments`, `attributes`, resources, and other modules it uses.
