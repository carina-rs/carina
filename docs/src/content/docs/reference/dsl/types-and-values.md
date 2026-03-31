---
title: Types & Values
description: Primitive types, collection types, resource references, enum identifiers, and string interpolation in the Carina DSL.
---

The Carina DSL is a statically-aware language with the following value types.

## Primitive Types

### String

Strings are enclosed in double quotes:

```crn
let name = "my-vpc"
let region = "ap-northeast-1"
```

#### Escape Sequences

Strings support the following escape sequences:

| Sequence | Meaning |
|----------|---------|
| `\"` | Double quote |
| `\\` | Backslash |
| `\n` | Newline |
| `\r` | Carriage return |
| `\t` | Tab |
| `\$` | Literal dollar sign (prevents interpolation) |

#### String Interpolation

Embed expressions inside strings with `${}`:

```crn
let env = "prod"
let name = "vpc-${env}"           # => "vpc-prod"
let cidr = "${base_cidr}"         # => value of base_cidr
let tag = "${env}-${service}"     # => "prod-web"
```

Any expression can appear inside `${}`, including function calls:

```crn
let tag = "vpc-${join("-", ["prod", "web"])}"
```

### Int

Integer values, optionally negative:

```crn
let port = 443
let offset = -1
let count = 0
```

### Float

Floating-point values with a decimal point:

```crn
let ratio = 0.5
let threshold = -3.14
```

### Bool

Boolean values `true` or `false`:

```crn
let enabled = true
let public = false
```

## Collection Types

### List

Ordered sequences enclosed in square brackets:

```crn
let zones = ["ap-northeast-1a", "ap-northeast-1c", "ap-northeast-1d"]
let ports = [80, 443, 8080]
let empty = []
```

Lists can contain mixed types and a trailing comma is allowed:

```crn
let mixed = [
  "hello",
  42,
  true,
]
```

### Map

Key-value collections enclosed in curly braces. Keys are identifiers (unquoted), values are expressions:

```crn
let config = {
  dev = "10.0.0.0/16"
  stg = "10.1.0.0/16"
}
```

Maps are also used for resource tags and nested configuration:

```crn
awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"

  tags = {
    Name        = "main"
    Environment = "prod"
  }
}
```

## Namespaced Identifiers

Dot-separated identifiers are used for resource types and enum values. They are not strings and are not quoted.

### Resource Types

Resource types use the format `provider.service.resource_type`:

```crn
awscc.ec2.vpc { ... }
awscc.ec2.subnet { ... }
awscc.s3.bucket { ... }
```

### Enum Identifiers

Enum values are namespaced identifiers that represent predefined constants:

```crn
# Provider-level enums
awscc.Region.ap_northeast_1
aws.Region.us_east_1

# Resource-level enums
awscc.ec2.vpc.InstanceTenancy.default
```

Enum identifiers are used directly as attribute values without quotes:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc {
  instance_tenancy = awscc.ec2.vpc.InstanceTenancy.default
}
```

## Resource References

When a resource is bound with `let`, its attributes can be accessed using dot notation:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

let subnet = awscc.ec2.subnet {
  vpc_id     = vpc.vpc_id      # Reference vpc's vpc_id attribute
  cidr_block = "10.0.1.0/24"
}
```

### Chained Access

Dot notation can be chained for nested access:

```crn
let sg = network.security_group.group_id
```

### Index Access

Use square brackets to access list elements or map values:

```crn
let first_zone = zones[0]
let dev_cidr = config["dev"]
```

Index and field access can be combined:

```crn
let subnet_id = subnets[0].subnet_id
```

## Type Expressions

Type expressions are used in `arguments` and `attributes` blocks in modules, and in function parameters.

### Simple Types

```crn
arguments {
  name      : string
  count     : int
  ratio     : float
  enabled   : bool
  cidr_block: cidr
  role_arn  : arn
}
```

### Generic Types

`list` and `map` accept a type parameter:

```crn
arguments {
  subnet_ids  : list(string)
  cidr_blocks : list(cidr)
  tags        : map(string)
}
```

### Resource Type References

Resource types can be used as type expressions to indicate a reference:

```crn
attributes {
  vpc_id   : awscc.ec2.vpc = vpc.vpc_id
  subnet_id: awscc.ec2.subnet = subnet.subnet_id
}
```

## Null

The `null` literal represents the absence of a value. It is primarily used in validation expressions:

```crn
require value != null, "Value must not be null"
```
