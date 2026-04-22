---
title: "Functions"
description: "Learn how to use built-in functions, define custom functions, and compose them with the pipe operator in Carina."
---

Carina provides built-in functions for common operations and lets you define your own functions. This guide covers both, along with the pipe and compose operators.

## Built-in functions

### String functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `upper` | `upper(string) -> string` | Converts to uppercase |
| `lower` | `lower(string) -> string` | Converts to lowercase |
| `trim` | `trim(string) -> string` | Removes leading/trailing whitespace |
| `replace` | `replace(search, replacement, string) -> string` | Replaces all occurrences |
| `split` | `split(separator, string) -> list` | Splits string into a list |
| `join` | `join(separator, list) -> string` | Joins list elements into a string |

Examples:

```crn
awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = upper('my-vpc')              # 'MY-VPC'
    Env  = replace('_', '-', 'my_env')  # 'my-env'
    Id   = join('-', ['vpc', 'prod'])    # 'vpc-prod'
  }
}
```

### Collection functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `length` | `length(list \| map \| string) -> int` | Returns element/character count |
| `concat` | `concat(items, base_list) -> list` | Appends items to a list |
| `flatten` | `flatten(list) -> list` | Flattens nested lists by one level |
| `keys` | `keys(map) -> list` | Returns map keys as a sorted list |
| `values` | `values(map) -> list` | Returns map values sorted by key |
| `lookup` | `lookup(map, key, default) -> any` | Looks up a key with a fallback |
| `map` | `map(accessor, collection) -> list \| map` | Extracts a field from each element |

Examples:

```crn
let parts1 = ['web', 'test']
let parts2 = ['vpc']

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = join('-', concat(parts2, parts1))  # 'web-test-vpc'
  }
}
```

### Numeric functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `min` | `min(a, b) -> number` | Returns the smaller value |
| `max` | `max(a, b) -> number` | Returns the larger value |

### Network functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `cidr_subnet` | `cidr_subnet(prefix, newbits, netnum) -> string` | Calculates a subnet CIDR block |

Example:

```crn
let vpcs = for (i, env) in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = cidr_subnet('10.0.0.0/8', 8, i)
    # i=0 -> '10.0.0.0/16', i=1 -> '10.1.0.0/16'

    tags = {
      Name = "vpc-${env}"
    }
  }
}
```

### Environment and security functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `env` | `env(name) -> string` | Reads an environment variable |
| `secret` | `secret(value) -> secret` | Marks a value as secret (stored as hash in state) |
| `decrypt` | `decrypt(ciphertext, key?) -> string` | Decrypts using the provider's encryption service (e.g., AWS KMS) |

## User-defined functions

Define reusable logic with the `fn` keyword:

```crn
fn tag_name(env: String, service: String): String {
  join('-', [env, service, 'vpc'])
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = tag_name('production', 'web')  # 'production-web-vpc'
  }
}
```

### Function syntax

```crn
fn name(param1: type1, param2: type2): return_type {
  expression
}
```

- **Parameters** can have type annotations (`: string`, `: int`, etc.)
- **Return type** annotation is optional (`: string`)
- The function body is a single expression (the return value)

### Local variables in functions

Functions can have local `let` bindings before the final expression:

```crn
fn subnet_name(env: String, tier: String, index: Int): String {
  let prefix = join("-", [env, tier])
  "${prefix}-${index}"
}
```

### Default parameter values

Parameters can have default values:

```crn
fn make_tags(name: String, env: String = 'dev'): map(string) {
  {
    Name        = name
    Environment = env
  }
}
```

## Pipe operator

The pipe operator `|>` passes the result of the left side as the **last** argument to the function on the right (data-last convention):

```crn
# Without pipe
let result = join('-', split('_', upper('hello_world')))

# With pipe -- reads left to right
let result = 'hello_world' |> upper() |> split('_') |> join('-')
# Result: 'HELLO-WORLD'
```

The pipe operator is especially useful with collection functions:

```crn
let names = ['web', 'api', 'worker']

# Extract and transform
let result = names |> join(', ')
```

## Compose operator

The compose operator `>>` creates a new function by chaining two partially applied functions (closures). Both sides must be closures:

```crn
# split('_') is a closure (1 of 2 args provided)
# join('-') is a closure (1 of 2 args provided)
let transform = split('_') >> join('-')
```

The resulting function applies the left function first, then passes the result to the right function.

## Partial application

When you call a built-in function with fewer arguments than it expects, you get a **closure** -- a partially applied function that waits for the remaining arguments:

```crn
# replace expects 3 args; giving 2 returns a closure
let dashify = replace('_', '-')

# The closure is called when the last argument arrives (via pipe)
let result = 'hello_world' |> dashify()  # 'hello-world'
```

This works naturally with the pipe operator, since the piped value fills in the last argument.
