---
title: "Using Modules"
description: "Learn how to organize Carina infrastructure code into reusable modules with arguments, attributes, directory modules, and nested module loading."
---

Modules let you group related resources into reusable units. A module defines its inputs with `arguments`, its outputs with `attributes`, and can be loaded with `use` and called from any `.crn` file.

## Creating a module

A module is a directory containing one or more `.crn` files. Every `.crn` file in the directory is merged together — no filename (including `main.crn`) is privileged. Single-file modules are not supported; always point `use` at a directory.

Create a file at `modules/network/main.crn` (the filename is up to you):

```crn
arguments {
  cidr_block : Ipv4Cidr
  subnet_cidr: Ipv4Cidr
  az         : aws.AvailabilityZone.ZoneName
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
  vpc      = vpc
  vpc_id   : aws.ec2.Vpc.Id = vpc.vpc_id
  subnet_id: aws.ec2.Subnet.Id = subnet.subnet_id
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

Supported types include `String`, `Bool`, `Int`, `Float`, `list(String)`,
`map(String)`, built-in types such as `Ipv4Cidr`, types registered by
providers such as `aws.AvailabilityZone.ZoneName` or `aws.ec2.Vpc.Id`, and
resource handle types like `awscc.ec2.Vpc`. A resource handle type accepts
only a matching bare resource or module output handle; a string or typed ID is
still a value and does not satisfy it. Arguments are also type-checked against
the attributes they flow into: passing a `String` argument to a `cidr_block`
attribute is an error, because the attribute expects an `Ipv4Cidr`.

### Attributes block

The `attributes` block declares the outputs the module exposes to callers:

```crn
attributes {
  vpc      = vpc
  vpc_id   : aws.ec2.Vpc.Id = vpc.vpc_id
  subnet_id: aws.ec2.Subnet.Id = subnet.subnet_id
}
```

Each attribute has a name, an optional type annotation, and a required
right-hand side. That right-hand side may be an ordinary value or a local or
received resource handle. Carina infers unannotated handle outputs as their
known resource type.

## Loading and calling a module

From your root configuration, load the module with `use` and call it with arguments:

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
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
  vpc_id = net.vpc.vpc_id
  tags   = { Name = 'my-route-table' }
}
```

The `net.vpc` output preserves the underlying VPC handle.
`net.vpc.vpc_id` selects an ordinary value from that resource, while
`net.vpc_id` accesses the separately published identity value.

### Passing resource handles between modules

A consumer can declare a resource-typed argument and use it for ordering:

```crn
# modules/consumer/arguments.crn
arguments {
  validation_record: aws.route53.RecordSet
}

# modules/consumer/main.crn
aws.acm.CertificateValidation {
  # ...

  directives {
    depends_on = [validation_record]
  }
}
```

If a producer declares
`attributes { validation_record = validation_record }`, the root can pass
`producer.validation_record` directly to this argument. The handle remains
typed and resolves to the producer's concrete instance-qualified resource;
it is not converted to a string or provider value. Select an attribute such
as `producer.validation_record.name` when a value is needed.

## Splitting a module across multiple files

Every `.crn` file in a module directory is merged as a peer, so you can split a module across files however you like. A common convention is:

```
modules/network/
  main.crn          # resources
  arguments.crn     # arguments block
  attributes.crn    # attributes block
```

The filenames are arbitrary — `main.crn` is a convention, not a requirement. Load the module by pointing `use` at the directory:

```crn
let network = use { source = './modules/network' }
```

## Nested modules

A module can load other modules. This lets you compose infrastructure from smaller building blocks.

For example, `modules/network_with_rt/main.crn` can load the network module:

```crn
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
    Name = 'nested-module-rt'
  }
}

attributes {
  vpc           = net.vpc
  vpc_id        : aws.ec2.Vpc.Id = net.vpc.vpc_id
  route_table_id: aws.ec2.RouteTable.Id = rt.route_table_id
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
