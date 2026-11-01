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

Any expression can appear inside `${}`, including function calls, with the same whitespace rules as anywhere else:

```crn
let tag  = "vpc-${upper(env)}"
let name = "vpc-${join('-', ['prod', 'web'])}"   # => 'vpc-prod-web'
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

#### Heredocs

Multi-line strings use heredoc syntax. The body runs until a line containing only the closing marker:

```crn
let policy = <<EOT
{
  "Version": "2012-10-17"
}
EOT
```

| Form | Interpolation | Indentation |
|------|---------------|-------------|
| `<<EOT` | `${...}` is expanded | kept as written |
| `<<-EOT` | `${...}` is expanded | common leading whitespace is stripped |
| `<<'EOT'` | literal | kept as written |
| `<<-'EOT'` | literal | common leading whitespace is stripped |

The marker can be any identifier; `EOT` is only a convention.

```crn
let user_data = <<-EOT
  #!/bin/bash
  echo "${env}"
  EOT
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

### Duration

A duration is an integer followed by a unit, with no space in between:

```crn
let short = 30s
let medium = 15min
let long = 2h
```

| Unit | Accepted suffixes |
|------|-------------------|
| seconds | `s`, `sec`, `second`, `seconds` |
| minutes | `m`, `min`, `minute`, `minutes` |
| hours | `h`, `hr`, `hour`, `hours` |

Durations are used for `timeout` in [`wait`](/reference/dsl/expressions/#wait-expression) and for `Duration`-typed arguments.

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

Key-value collections enclosed in curly braces. Keys are identifiers, or quoted strings for keys that contain characters such as `.`, `/`, or `:`. Values are expressions:

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
    Name                     = 'main'
    Environment              = 'prod'
    'kubernetes.io/role/elb' = '1'
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
aws.ec2.Vpc.InstanceTenancy.default
```

Enum identifiers are used directly as attribute values without quotes:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  instance_tenancy = aws.ec2.Vpc.InstanceTenancy.default
}
```

Resource-level enums use the `aws.` namespace for both the `aws` and `awscc` providers. Where the attribute's type is known, the bare value (`instance_tenancy = default`) is also accepted.

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

The binding `vpc` is a structural handle, not a value. A bare handle is
therefore invalid anywhere a value is required:

```crn
# Invalid: choose an attribute from vpc instead.
let subnet = awscc.ec2.Subnet {
  vpc_id = vpc
}

# Also invalid in collections, exports, and interpolation.
let handles = [vpc]
exports { network = vpc }
let label = "network-${vpc}"
```

Use an attribute such as `vpc.vpc_id`. The same rule applies to bindings
created by `for`, `if`, `use`, module calls, and `wait`. A module output may
itself be a handle: if a module declares `attributes { vpc = vpc }`, then
`network.vpc` is the handle and `network.vpc.vpc_id` is a value selected from
the underlying resource.

A handle may be given another name with `let`. The alias is itself a handle,
so attribute access works through it, but using the alias bare in a value
position is still rejected:

```crn
let network = vpc

let subnet = awscc.ec2.Subnet {
  vpc_id = network.vpc_id # Valid: selects an attribute.
  name   = network.name   # Valid: selects an attribute.
  parent = network        # Invalid: network is still a bare handle.
}
```

Only syntax that explicitly transports a handle accepts one directly. Examples
are a module `attributes` entry, `directives.depends_on`,
`wait.depends_on`, `directives.provider`, and function or module arguments.
The argument itself decides what is transported: passing `vpc` binds the
parameter as a handle, while passing `vpc.vpc_id` binds it as a value. A
resource type annotation accepts only a matching handle; it does not turn a
string or identity value into a handle. An untyped function parameter can also
receive a handle:

```crn
fn id_of(resource) {
  resource.vpc_id
}

let id = id_of(vpc)
```

Inside the function or module, the parameter follows the same rule as the
original argument: select an attribute from a handle before using it as a
value. A bare `resource` in the example above is therefore invalid.

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

Type expressions are used in `arguments`, `attributes`, and `exports` blocks, and in function parameters and return types.

### Simple Types

```crn
arguments {
  name      : String
  count     : Int
  ratio     : Float
  enabled   : Bool
  timeout   : Duration
  cidr_block: Ipv4Cidr
  role_arn  : aws.Arn
  vpc_id    : aws.ec2.Vpc.Id
}
```

Primitive type names are PascalCase (`String`, not `string`). Besides the primitives, built-in types such as `Ipv4Cidr`, `Ipv4Address`, `Ipv6Cidr`, and `Ipv6Address` are always available, and providers register their own dotted types (for example `aws.Arn` or `aws.ec2.Vpc.Id`). An unknown type name is an error.

### Generic Types

`list` and `map` accept a type parameter:

```crn
arguments {
  subnet_ids  : list(String)
  cidr_blocks : list(Ipv4Cidr)
  tags        : map(String)
}
```

### String Literal Unions

A union of string literals restricts a value to a closed set:

```crn
arguments {
  environment: 'dev' | 'stg' | 'prod'
}
```

Passing any other string is an error at the module call.

### Struct Types

`struct { ... }` describes a map with typed fields:

```crn
arguments {
  network: struct { cidr: Ipv4Cidr, az: String }
}
```

### Resource Type References

Resource types used as type expressions mean resource handles. They are not
aliases for the resource's ID or another string attribute:

```crn
arguments {
  network: awscc.ec2.Vpc
}

attributes {
  network = network
}
```

A matching bare resource or module handle can satisfy `awscc.ec2.Vpc`. A
`String`, `vpc.vpc_id`, or even the typed identity value
`aws.ec2.Vpc.Id` cannot. When the boundary should carry the ID value, annotate
it with the identity type instead:

```crn
attributes {
  vpc_id: aws.ec2.Vpc.Id = vpc.vpc_id
}
```

Resource handles may cross module boundaries in the same expanded graph, but
they cannot be stored in `exports` or read through `upstream_state`.

## Null

The `null` literal represents the absence of a value. It can only appear in [validate expressions](/reference/dsl/expressions/#validate-expressions) such as `require` statements and argument validation:

```crn
require value != null, 'Value must not be null'
```
