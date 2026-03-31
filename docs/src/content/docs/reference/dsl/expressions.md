---
title: Expressions
description: For loops, if/else conditionals, let bindings, pipe operator, compose operator, and function definitions in the Carina DSL.
---

Expressions in the Carina DSL produce values. They can appear as attribute values, function arguments, or in any context where a value is expected.

## For Expression

The `for` expression iterates over a list or map to create multiple resources or values.

### Iterating Over a List

```crn
let vpcs = for env in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"

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
let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = cidr_subnet("10.0.0.0/8", 8, i)

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
  dev = "10.0.0.0/16"
  stg = "10.1.0.0/16"
}

let vpcs = for name, cidr in cidrs {
  awscc.ec2.vpc {
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
    az          = "ap-northeast-1a"
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
  awscc.ec2.nat_gateway {
    allocation_id = eip.allocation_id
    subnet_id     = subnet.subnet_id
  }
}
```

### If/Else as a Value

`if`/`else` can be used inline to choose between values:

```crn
awscc.ec2.vpc {
  cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }

  tags = {
    Name = if is_production { "prod-vpc" } else { "dev-vpc" }
  }
}
```

### Local Bindings in If Body

`let` bindings can be used inside `if` and `else` blocks:

```crn
if is_production {
  let cidr = "10.0.0.0/16"

  awscc.ec2.vpc {
    cidr_block = cidr
  }
}
```

## Let Binding

`let` binds a name to a value. At the top level, it binds resources or values. Inside blocks, it creates scoped variables.

```crn
# Top-level value binding
let env = "prod"
let zones = ["ap-northeast-1a", "ap-northeast-1c"]

# Top-level resource binding
let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

# Module import binding
let network = import "./modules/network"

# Remote state binding
let shared = remote_state {
  path = "shared.state.json"
}
```

Use `let _ =` (the discard pattern) when you need to evaluate an expression but do not need to reference the result. See [Syntax: Discard Pattern](/reference/dsl/syntax/#discard-pattern) for details.

## Pipe Operator (`|>`)

The pipe operator passes the result of the left expression as the last argument to the function on the right. This enables readable left-to-right data transformations.

```crn
# Without pipe: nested calls are hard to read
let result = join("-", concat(["vpc"], ["prod", "web"]))

# With pipe: reads left to right
let result = ["prod", "web"] |> concat(["vpc"]) |> join("-")
```

The pipe operator works with any built-in function. The piped value becomes the last argument:

```crn
# These are equivalent:
join("-", ["a", "b", "c"])
["a", "b", "c"] |> join("-")

# These are equivalent:
replace("-", "_", "hello-world")
"hello-world" |> replace("-", "_")

# These are equivalent:
split(",", "a,b,c")
"a,b,c" |> split(",")
```

### Chaining Multiple Pipes

Multiple pipe operations can be chained for complex transformations:

```crn
let result = ["prod", "web"]
  |> concat(["vpc"])
  |> join("-")
  |> upper()
```

## Compose Operator (`>>`)

The compose operator (`>>`) combines two partially applied functions into a new function. The result of the first function is passed as input to the second. Both sides of `>>` must be closures (partially applied functions).

```crn
# Compose split and join into a single function
let transform = split(",") >> join("-")

# Apply the composed function via pipe
let result = "a,b,c" |> transform()
# => "a-b-c"
```

Compose works with any partially applied built-in function:

```crn
# Extract IDs from a list of maps, then join them
let pipeline = map(".id") >> join(", ")
let result = [{ id = "1" }, { id = "2" }] |> pipeline()
# => "1, 2"
```

Three or more functions can be composed:

```crn
let transform = split(",") >> join("-") >> split("-")
```

The compose operator binds tighter than the pipe operator, so `f >> g |> h` means `(f >> g) |> h`.

## Function Calls

Functions are called with parentheses:

```crn
let len = length(zones)
let subnet = cidr_subnet("10.0.0.0/16", 8, 1)
let name = join("-", ["prod", "web", "vpc"])
```

### Partial Application

When a built-in function is called with fewer arguments than it expects, it returns a closure that captures the provided arguments. The closure waits for the remaining arguments before executing.

```crn
# split expects 2 args: split(separator, string)
# Providing only 1 creates a closure
let split_by_comma = split(",")

# The closure can be used with pipe (parentheses are required)
let parts = "a,b,c" |> split_by_comma()
```

This is particularly useful with the pipe operator:

```crn
let result = "hello-world" |> replace("-", "_")
# replace(search, replacement, string) gets "-" and "_" captured,
# then "hello-world" is supplied as the third argument via pipe
```

## User-Defined Functions

Define functions with `fn`. Parameters can have optional type annotations and default values:

```crn
fn tag_name(env: string, service: string): string {
  join("-", [env, service, "vpc"])
}
```

### Parameters

Function parameters support:
- Type annotations: `name: string`
- Default values: `name: string = "default"`
- No annotation: `name` (any type accepted)

```crn
fn make_tags(env: string, service: string, team: string = "platform") {
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
fn subnet_name(env: string, az: string): string {
  let short_az = replace("ap-northeast-1", "", az)
  "${env}-subnet-${short_az}"
}
```

### Return Type

The optional return type annotation follows the parameter list with a colon:

```crn
fn cidr_for_env(env: string): string {
  lookup({ dev = "10.0.0.0/16", stg = "10.1.0.0/16" }, env, "10.99.0.0/16")
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
  instance_count: int {
    description = "Number of instances to create"
    default     = 1
    validation {
      condition     = instance_count >= 1 && instance_count <= 10
      error_message = "Instance count must be between 1 and 10"
    }
  }
}
```

Function calls can be used in validate expressions:

```crn
arguments {
  name: string {
    validation {
      condition     = length(name) > 0
      error_message = "Name must not be empty"
    }
  }
}
```
