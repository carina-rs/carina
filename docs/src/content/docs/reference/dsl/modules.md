---
title: Modules
description: Module definition, arguments, attributes, import syntax, directory modules, and module resolution in the Carina DSL.
---

Modules are reusable units of Carina configuration. A module defines a set of resources along with input parameters (`arguments`) and output values (`attributes`).

## Module Structure

A module is a directory containing one or more `.crn` files that together declare `arguments` and `attributes` blocks. Carina merges every `.crn` file in the directory as peers; there is no privileged filename like `main.crn`.

```crn
# modules/network/main.crn  (any filename works — main.crn is just convention)

arguments {
  cidr_block : string
  subnet_cidr: string
  az         : string
}

let vpc = awscc.ec2.vpc {
  cidr_block = cidr_block

  tags = {
    Name = 'module-test'
  }
}

let subnet = awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = subnet_cidr
  availability_zone = az

  tags = {
    Name = 'module-test-subnet'
  }
}

attributes {
  vpc_id   : awscc.ec2.vpc = vpc.vpc_id
  subnet_id: awscc.ec2.subnet = subnet.subnet_id
}
```

## Arguments Block

The `arguments` block defines the parameters a module accepts from its caller.

### Basic Parameters

Each parameter has a name and a type:

```crn
arguments {
  cidr_block: string
  count     : int
  enabled   : bool
}
```

### Default Values

Parameters can have default values:

```crn
arguments {
  cidr_block: string
  az        : string = 'ap-northeast-1a'
  enable_dns: bool   = true
}
```

### Block Form with Description and Validation

Parameters can use an expanded block form for description, default values, and validation rules:

```crn
arguments {
  instance_count: int {
    description = 'Number of instances to create'
    default     = 1
    validation {
      condition     = instance_count >= 1 && instance_count <= 10
      error_message = 'Instance count must be between 1 and 10'
    }
  }

  cidr_block: string {
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
  name       : string
  count      : int
  ratio      : float
  enabled    : bool
  subnet_ids : list(string)
  tags       : map(string)
  cidr_block : cidr
  role_arn   : arn
}
```

## Attributes Block

The `attributes` block defines the output values that a module exposes to its caller.

Each attribute has a name, an optional type, and a value expression:

```crn
attributes {
  vpc_id        : awscc.ec2.vpc = vpc.vpc_id
  subnet_id     : awscc.ec2.subnet = subnet.subnet_id
  route_table_id: awscc.ec2.route_table = rt.route_table_id
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

awscc.ec2.security_group {
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
  cidr_block : string
  subnet_cidr: string
  az         : string
}

let net = network {
  cidr_block  = cidr_block
  subnet_cidr = subnet_cidr
  az          = az
}

let rt = awscc.ec2.route_table {
  vpc_id = net.vpc_id

  tags = {
    Name = 'nested-module-test-rt'
  }
}

attributes {
  vpc_id        : awscc.ec2.vpc = net.vpc_id
  route_table_id: awscc.ec2.route_table = rt.route_table_id
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
