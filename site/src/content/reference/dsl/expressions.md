---
title: Expressions
description: For loops, if/else conditionals, let bindings, wait, pipe and compose operators, and function definitions in the Carina DSL.
---

Expressions in the Carina DSL normally produce values. They can appear as
attribute values, function arguments, or in any context where a value is
expected. Resource and module declarations instead produce structural
handles. A module output can preserve a resource handle; select a resource
attribute from that output before using it as a value.

## For Expression

The `for` expression iterates over a list or map to create multiple resources, data-source reads, or module instances. The body must end in a resource, a `read`, or a module call.

The iterable is a list literal, a binding, a function call, or a parenthesized expression; an inline map literal must be parenthesized (`for k, v in ({ a = 1 }) { ... }`). Use `_` for a loop variable you do not need.

### Iterating Over a List

```crn
let vpcs = for env in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = '10.0.0.0/16'

    tags = {
      Name        = "vpc-${env}"
      Environment = env
    }
  }
}
```

### Indexed Iteration

Use `(index, value)` to access both the index and the element:

```crn
let vpcs = for (i, env) in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = cidr_subnet('10.0.0.0/8', 8, i)

    tags = {
      Name        = "for-list-test-${env}"
      Environment = env
    }
  }
}
```

### Iterating Over a Map

Use `key, value` to iterate over map entries:

```crn
let cidrs = {
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}

let vpcs = for name, cidr in cidrs {
  awscc.ec2.Vpc {
    cidr_block = cidr

    tags = {
      Name        = "vpc-${name}"
      Environment = name
    }
  }
}
```

### Local Bindings in For Body

`let` bindings can be used inside a `for` body to compute intermediate values:

```crn
let networks = for name, cidr in cidrs {
  let subnet_cidr = cidr_subnet(cidr, 8, 1)

  network {
    cidr_block  = cidr
    subnet_cidr = subnet_cidr
    az          = 'ap-northeast-1a'
  }
}
```

### For with Module Calls

`for` works with module calls to create multiple instances of a module. See [Modules: Modules with For Expressions](/reference/dsl/modules/#modules-with-for-expressions) for a full example.

## If Expression

The `if` expression conditionally produces a resource or value.

### Conditional Resource Creation

```crn
let is_production = true

if is_production {
  awscc.ec2.NatGateway {
    allocation_id = eip.allocation_id
    subnet_id     = subnet.subnet_id
  }
}
```

### If/Else as a Value

`if`/`else` can be used inline to choose between values:

```crn
awscc.ec2.Vpc {
  cidr_block = if is_production { '10.0.0.0/16' } else { '172.16.0.0/16' }

  tags = {
    Name = if is_production { 'prod-vpc' } else { 'dev-vpc' }
  }
}
```

### Local Bindings in If Body

`let` bindings can be used inside `if` and `else` blocks:

```crn
if is_production {
  let cidr = '10.0.0.0/16'

  awscc.ec2.Vpc {
    cidr_block = cidr
  }
}
```

## Wait Expression

`wait` blocks downstream resources until a target resource reaches a condition. The target must be a managed resource; data sources declared with `read` and module calls are rejected. It is the right-hand side of a `let` binding:

```crn
let cert = aws.acm.Certificate {
  domain_name       = 'registry.example.com'
  validation_method = dns
}

let validation_record = aws.route53.RecordSet {
  hosted_zone_id   = 'Z123EXAMPLE'
  name             = cert.domain_validation_options[0].resource_record.name
  type             = cname
  ttl              = 60
  resource_records = [cert.domain_validation_options[0].resource_record.value]
}

let cert_issued = wait cert {
  until      = cert.status == aws.acm.Certificate.Status.issued
  depends_on = [validation_record]
  timeout    = 75min
}
```

| Field | Required | Meaning |
|-------|----------|---------|
| `until` | yes | Condition polled against the target, in the form `<target>.<attribute> == <value>` |
| `depends_on` | no | Managed-resource handles that must complete before polling starts |
| `timeout` | no | A [duration](/reference/dsl/types-and-values/#duration); defaults to the target schema's wait timeout, or 5 minutes |

Attributes read through the wait binding (`cert_issued.certificate_arn`) are the target's attributes, but a resource that references them is not created until the wait succeeds. Reference the target directly (`cert.certificate_arn`) to skip the wait. A wait is not stored in state.

`depends_on` accepts local handles and resource-handle module outputs. For
example, a consumer module can declare
`validation_record: aws.route53.RecordSet` and use
`depends_on = [validation_record]`; its caller may pass
`producer.validation_record` from another module. Carina resolves that operand
to the producer resource's complete, instance-qualified address before
planning. Values, resource attributes, data sources, and a module call itself
are not dependency handles.

A dependency adds ordering, not value flow. If the referenced resource is
unchanged, the wait and its downstream values remain known and unchanged. If
that resource changes, the wait is ordered after its effect.

## Let Binding

`let` binds a name to a value. At the top level, it binds resources or values. Inside blocks, it creates scoped variables.

```crn
# Top-level value binding
let env = 'prod'
let zones = ['ap-northeast-1a', 'ap-northeast-1c']

# Top-level resource binding
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

# Module use binding
let network = use { source = './modules/network' }
```

Upstream state is bound with `let <binding> = upstream_state { source = "..." }`. See [Upstream State](/reference/dsl/syntax/#upstream-state) for details.

Use `let _ =` (the discard pattern) when you need to evaluate an expression but do not need to reference the result. See [Syntax: Discard Pattern](/reference/dsl/syntax/#discard-pattern) for details.

### Field and Index Access

Field access (`.field`) and indexing (`[0]` or `["key"]`) can follow any value expression, including a function result, a parenthesized pipe expression, or a computed `let` binding:

```crn
let parts = split(':', queue.arn)
let account_id = parts[4]

let resource_name = split('/', role.arn)[1]
let queue_name = (queue.arn |> split(':'))[5]
```

Access on a value that is already known is evaluated immediately. Access whose base depends on a resource or upstream value is deferred until that value is available.

## Coalesce Operator (`??`)

`ref ?? default` uses the referenced value when it exists and `default` when it is absent. The left side must be a reference or access chain (`up.accounts`, `vpc.tags`); both sides are type-checked, so a default of the wrong type is rejected by `validate`.

```crn
let orgs = upstream_state {
  source = '../organizations'
}

for account_id in (orgs.accounts ?? []) {
  awscc.sso.Assignment {
    target_id = account_id
    # ...
  }
}
```

Which side is used is decided when the reference resolves, not when the file is parsed:

| Left side | Result |
|-----------|--------|
| Exists (an upstream export, or a resource attribute that has a value) | The referenced value |
| Absent (the loaded upstream state does not export it, or the attribute is unset) | `default` |
| Not known yet (upstream state not available, or a value computed during apply) | Deferred; resolved at apply time |

Without `??`, iterating over an export that the upstream state does not have is an error. `??` chains are right-associative: `a ?? b ?? c` is `a ?? (b ?? c)`.

## Pipe Operator (`|>`)

The pipe operator passes the result of the left expression as the last argument to the function on the right. This enables readable left-to-right data transformations.

```crn
# Without pipe: nested calls are hard to read
let result = join('-', concat(['vpc'], ['prod', 'web']))

# With pipe: reads left to right
let result = ['prod', 'web'] |> concat(['vpc']) |> join('-')
```

The pipe operator works with any built-in function. The piped value becomes the last argument:

```crn
# These are equivalent:
join('-', ['a', 'b', 'c'])
['a', 'b', 'c'] |> join('-')

# These are equivalent:
replace('-', '_', 'hello-world')
'hello-world' |> replace('-', '_')

# These are equivalent:
split(',', 'a,b,c')
'a,b,c' |> split(',')
```

### Chaining Multiple Pipes

Multiple pipe operations can be chained for complex transformations:

```crn
let result = ['prod', 'web']
  |> concat(['vpc'])
  |> join('-')
  |> upper()
```

## Compose Operator (`>>`)

The compose operator (`>>`) combines two partially applied functions into a new function. The result of the first function is passed as input to the second. Both sides of `>>` must be closures (partially applied functions).

```crn
# Compose split and join into a single function
let transform = split(',') >> join('-')

# Apply the composed function via pipe
let result = 'a,b,c' |> transform()
# => 'a-b-c'
```

Compose works with any partially applied built-in function:

```crn
# Extract IDs from a list of maps, then join them
let pipeline = map('.id') >> join(', ')
let result = [{ id = '1' }, { id = '2' }] |> pipeline()
# => '1, 2'
```

Three or more functions can be composed:

```crn
let transform = split(',') >> join('-') >> split('-')
```

The compose operator binds tighter than the pipe operator, so `f >> g |> h` means `(f >> g) |> h`.

## Function Calls

Functions are called with parentheses:

```crn
let len = length(zones)
let subnet = cidr_subnet('10.0.0.0/16', 8, 1)
let name = join('-', ['prod', 'web', 'vpc'])
```

### Partial Application

When a built-in function is called with fewer arguments than it expects, it returns a closure that captures the provided arguments. The closure waits for the remaining arguments before executing.

```crn
# split expects 2 args: split(separator, string)
# Providing only 1 creates a closure
let split_by_comma = split(',')

# The closure can be used with pipe (parentheses are required)
let parts = 'a,b,c' |> split_by_comma()
```

This is particularly useful with the pipe operator:

```crn
let result = 'hello-world' |> replace('-', '_')
# replace(search, replacement, string) gets '-' and '_' captured,
# then 'hello-world' is supplied as the third argument via pipe
```

## User-Defined Functions

Define functions with `fn`. Parameters can have optional type annotations and default values:

```crn
fn tag_name(env: String, service: String): String {
  join('-', [env, service, 'vpc'])
}
```

### Parameters

Function parameters support:
- Type annotations: `name: String`
- Default values: `name: String = "default"`
- No annotation: `name` (any type accepted)

An untyped parameter can receive either a value or a structural handle. The
argument determines which: `f(vpc)` binds the parameter as a handle, while
`f(vpc.vpc_id)` binds it as a value. Select an attribute such as
`parameter.vpc_id` before using a handle-backed parameter as a value; using
the parameter bare is invalid.

```crn
fn make_tags(env: String, service: String, team: String = 'platform') {
  {
    Environment = env
    Service     = service
    Team        = team
  }
}
```

### Local Bindings in Functions

Functions can contain `let` bindings before the return expression:

```crn
fn subnet_name(env: String, az: String): String {
  let short_az = replace('ap-northeast-1', '', az)
  "${env}-subnet-${short_az}"
}
```

### Return Type

The optional return type annotation follows the parameter list with a colon:

```crn
fn cidr_for_env(env: String): String {
  lookup({ dev = '10.0.0.0/16', stg = '10.1.0.0/16' }, env, '10.99.0.0/16')
}
```

## Validate Expressions

Validate expressions are boolean expressions used in `arguments` blocks for input validation and in `require` statements. They support comparison and logical operators.

### Comparison Operators

| Operator | Meaning |
|----------|---------|
| `==` | Equal |
| `!=` | Not equal |
| `>` | Greater than |
| `<` | Less than |
| `>=` | Greater than or equal |
| `<=` | Less than or equal |

### Logical Operators

| Operator | Meaning |
|----------|---------|
| `&&` | Logical AND |
| `\|\|` | Logical OR |
| `!` | Logical NOT |

### Example: Argument Validation

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
}
```

Function calls can be used in validate expressions:

```crn
arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = 'Name must not be empty'
    }
  }
}
```

The condition in an argument's `validation` block may reference only that
argument. To compare two or more module arguments, use a module-level
`require` statement:

```crn
arguments {
  min_size: Int
  max_size: Int
}

require min_size <= max_size, 'min_size must not exceed max_size'
```

Validate expressions are evaluated when their referenced values become known.
Literal arguments are checked during validation; reference values are checked
during planning or, for values produced by an upstream effect, during apply
before a provider operation consumes the known value. Inputs that remain
unresolved at the end of an otherwise successful apply are reported as an
error. See [Modules: When Constraints Run](/reference/dsl/modules/#when-constraints-run)
for the complete module and resource-schema timing rules.
