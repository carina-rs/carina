---
title: "Using Modules"
description: "Learn how to organize Carina infrastructure code into reusable modules with arguments, attributes, directory modules, and nested module loading."
---

Modules let you group related resources into reusable units. A module defines its inputs with `arguments`, its outputs with `attributes`, and can be loaded with `use` and called from any `.crn` file.

## Creating a module

A module is a `.crn` file (or a directory containing `main.crn`) that declares `arguments` and optionally `attributes`.

Create a file at `modules/network/main.crn`:

```crn
arguments {
  cidr_block : String
  subnet_cidr: String
  az         : String
}

let vpc = awscc.ec2.Vpc {
  cidr_block = cidr_block

  tags = {
    Name = 'module-vpc'
  }
}

let subnet = awscc.ec2.Subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = subnet_cidr
  availability_zone = az

  tags = {
    Name = 'module-subnet'
  }
}

attributes {
  vpc_id   : awscc.ec2.Vpc = vpc.vpc_id
  subnet_id: awscc.ec2.Subnet = subnet.subnet_id
}
```

### Arguments block

The `arguments` block declares the inputs the module accepts. Each parameter has a name and a type:

```crn
arguments {
  cidr_block: String
  env_name  : String
}
```

Supported types include `String`, `Bool`, `Int`, `Float`, `list(String)`, `map(String)`, and resource type references like `awscc.ec2.Vpc`.

### Attributes block

The `attributes` block declares the outputs the module exposes to callers:

```crn
attributes {
  vpc_id   : awscc.ec2.Vpc = vpc.vpc_id
  subnet_id: awscc.ec2.Subnet = subnet.subnet_id
}
```

Each attribute has a name, an optional type annotation, and a value expression.

## Loading and calling a module

From your root configuration, load the module with `use` and call it with arguments:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let network = use { source = './modules/network' }

network {
  cidr_block  = '10.0.0.0/16'
  subnet_cidr = '10.0.1.0/24'
  az          = 'ap-northeast-1a'
}
```

The `use` expression loads the module from its `source` directory and binds it to a name. You then call the module like a function, passing its arguments as a block.

## Accessing module attributes

To use a module's output attributes, bind the module call with `let`:

```crn
let network = use { source = './modules/network' }

let net = network {
  cidr_block  = '10.0.0.0/16'
  subnet_cidr = '10.0.1.0/24'
  az          = 'ap-northeast-1a'
}

# Use module output to create another resource
awscc.ec2.RouteTable {
  vpc_id = net.vpc_id
  tags   = { Name = 'my-route-table' }
}
```

The `net.vpc_id` reference accesses the `vpc_id` attribute declared in the module's `attributes` block.

## Directory modules

A module can be either:

- **A single file**: `modules/network.crn`
- **A directory**: `modules/network/main.crn`

Directory modules are useful when a module grows large or needs helper files. The entry point is always `main.crn` inside the directory.

Both forms are loaded the same way:

```crn
let network = use { source = './modules/network' }
```

## Nested modules

A module can load other modules. This lets you compose infrastructure from smaller building blocks.

For example, `modules/network_with_rt/main.crn` can load the network module:

```crn
let network = use { source = '../network' }

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
    Name = 'nested-module-rt'
  }
}

attributes {
  vpc_id        : awscc.ec2.Vpc = net.vpc_id
  route_table_id: awscc.ec2.RouteTable = rt.route_table_id
}
```

`source` paths are relative to the module file's location.

## Using modules with `for` expressions

Modules can be called inside `for` expressions to create multiple instances:

```crn
let vpc_mod = use { source = './modules/vpc_only' }

let cidrs = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}

let networks = for name, cidr in cidrs {
  vpc_mod {
    cidr_block = cidr
    env_name   = name
  }
}
```

This creates one VPC per entry in the `cidrs` map. See the [For / If Expressions](/guides/for-if-expressions/) guide for more details.

## Inspecting modules

Use the CLI to inspect a module's structure:

```bash
# Show module arguments, attributes, and dependencies
carina module info modules/network

# List all modules loaded in a configuration
carina module list
```
