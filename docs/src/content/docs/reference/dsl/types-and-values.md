---
title: Types & Values
description: Primitive types, collection types, resource references, enum identifiers, and string interpolation in the Carina DSL.
---

The Carina DSL is a statically-aware language with the following value types.

## Primitive Types

### String

Strings can be enclosed in either single or double quotes. The two forms have
different semantics, and the recommended style is to pick the form based on
whether interpolation is needed.

#### Single-Quoted Strings (Literal)

Single quotes produce a literal string. `${...}` inside single quotes is **not**
interpreted as interpolation — it is kept verbatim.

```crn
let name = 'my-vpc'
let region = 'ap-northeast-1'
let literal = 'price is ${amount}'   # the six characters ${amount} stay as-is
```

The only escape sequences recognized inside single quotes are `\'` (single
quote) and `\\` (backslash).

#### Double-Quoted Strings (Interpolation)

Double quotes support `${...}` interpolation and the full set of escape
sequences. Use them whenever you need to embed an expression in a string.

```crn
let env  = 'prod'
let name = "vpc-${env}"           # => 'vpc-prod'
let cidr = "${base_cidr}"         # => value of base_cidr
let tag  = "${env}-${service}"    # => 'prod-web'
```

Any expression can appear inside `${}`, including function calls:

```crn
let tag = "vpc-${join('-', ['prod', 'web'])}"
```

#### Recommended Style

- **Use single quotes** for literal strings that do not need interpolation
  (the common case).
- **Use double quotes** only when you need `${...}` interpolation or an
  escape sequence that is not supported in single quotes.

Both forms are always valid — this is a style guideline, not a correctness
requirement. A literal written with double quotes (e.g. `"ap-northeast-1"`)
is accepted and behaves identically to the single-quoted form.

#### Escape Sequences

Double-quoted strings support the following escape sequences:

| Sequence | Meaning |
|----------|---------|
| `\"` | Double quote |
| `\\` | Backslash |
| `\n` | Newline |
| `\r` | Carriage return |
| `\t` | Tab |
| `\$` | Literal dollar sign (prevents interpolation) |

Single-quoted strings recognize only `\'` and `\\`.

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
let zones = ['ap-northeast-1a', 'ap-northeast-1c', 'ap-northeast-1d']
let ports = [80, 443, 8080]
let empty = []
```

Lists can contain mixed types and a trailing comma is allowed:

```crn
let mixed = [
  'hello',
  42,
  true,
]
```

### Map

Key-value collections enclosed in curly braces. Keys are identifiers (unquoted), values are expressions:

```crn
let config = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}
```

Maps are also used for resource tags and nested configuration:

```crn
awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name        = 'main'
    Environment = 'prod'
  }
}
```

## Namespaced Identifiers

Dot-separated identifiers are used for resource types and enum values. They are not strings and are not quoted.

### Resource Types

Resource types use the format `provider.service.resource_type`:

```crn
awscc.ec2.Vpc { ... }
awscc.ec2.Subnet { ... }
awscc.s3.Bucket { ... }
```

### Enum Identifiers

Enum values are namespaced identifiers that represent predefined constants:

```crn
# Provider-level enums
awscc.Region.ap_northeast_1
aws.Region.us_east_1

# Resource-level enums
awscc.ec2.Vpc.InstanceTenancy.default
```

Enum identifiers are used directly as attribute values without quotes:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  instance_tenancy = awscc.ec2.Vpc.InstanceTenancy.default
}
```

## Resource References

When a resource is bound with `let`, its attributes can be accessed using dot notation:

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

let subnet = awscc.ec2.Subnet {
  vpc_id     = vpc.vpc_id      # Reference vpc's vpc_id attribute
  cidr_block = '10.0.1.0/24'
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
let dev_cidr = config['dev']
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
  name      : String
  count     : Int
  ratio     : Float
  enabled   : Bool
  cidr_block: Ipv4Cidr
  role_arn  : Arn
}
```

### Generic Types

`list` and `map` accept a type parameter:

```crn
arguments {
  subnet_ids  : list(String)
  cidr_blocks : list(Ipv4Cidr)
  tags        : map(String)
}
```

### Resource Type References

Resource types can be used as type expressions to indicate a reference:

```crn
attributes {
  vpc_id   : awscc.ec2.Vpc = vpc.vpc_id
  subnet_id: awscc.ec2.Subnet = subnet.subnet_id
}
```

## Null

The `null` literal represents the absence of a value. It can only appear in [validate expressions](/reference/dsl/expressions/#validate-expressions) such as `require` statements and argument validation:

```crn
require value != null, 'Value must not be null'
```
