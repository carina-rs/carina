---
title: "Functions"
description: "Learn how to use built-in functions, define custom functions, and compose them with the pipe operator in Carina."
---

Carina provides built-in functions for common operations and lets you define your own functions. This guide covers both, along with the pipe and compose operators.

## Built-in functions

### String functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `upper` | `upper(string) -> String` | Converts to uppercase |
| `lower` | `lower(string) -> String` | Converts to lowercase |
| `trim` | `trim(string) -> String` | Removes leading/trailing whitespace |
| `replace` | `replace(search, replacement, string) -> String` | Replaces all occurrences |
| `split` | `split(separator, string) -> list(String)` | Splits string into a list |
| `join` | `join(separator, list) -> String` | Joins list elements into a string |

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
| `length` | `length(list \| map \| String) -> Int` | Returns the element count for lists/maps, or UTF-8 byte count for strings |
| `concat` | `concat(items, base_list) -> list(Any)` | Appends items to a list |
| `flatten` | `flatten(list) -> list(Any)` | Flattens nested lists by one level |
| `keys` | `keys(map) -> list(String)` | Returns map keys as a sorted list |
| `values` | `values(map) -> list(Any)` | Returns map values sorted by key |
| `lookup` | `lookup(map, key, default) -> Any` | Looks up a key with a fallback |
| `map` | `map(accessor, collection) -> list(Any) \| map(Any)` | Extracts a field from each element |

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

Collection operations use the information they need. `length` can count a list with pending elements, and `keys` can return a known map's keys while its values await apply. `values`, `concat`, `lookup`, and `map` preserve pending selected values and their dependencies. `flatten` needs the inner collection shapes for the level it flattens. Scalar operations such as `join` need their element values before evaluation.

A secret passed through a typed function parameter or return keeps its input timing. A `String` annotation does not force a secret containing a resource reference to evaluate during validation.

This does not make an arbitrary provider output's membership known. A loop over `keys(resource.tags)` still requires the map's keys to be available at that loop's supported expansion boundary.

### Numeric functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `min` | `min(a, b) -> Number` | Returns the smaller value |
| `max` | `max(a, b) -> Number` | Returns the larger value |

### Network functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `cidr_subnet` | `cidr_subnet(prefix, newbits, netnum) -> String` | Calculates a subnet CIDR block |

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
| `env` | `env(name) -> String` | Reads an environment variable as an ordinary string |
| `secret` | `secret(value) -> Secret` | Marks a value as secret (stored as hash in state) |
| `decrypt` | `decrypt(ciphertext, key?) -> Secret` | Decrypts base64 ciphertext with host-side AWS KMS; returns a masked secret stored as a hash in state |

`env()` returns an ordinary string that can appear in diagnostics and plan output.
An environment variable that must stay hidden is written `secret(env('NAME'))`:

```crn
let password = secret(env('DB_PASSWORD'))
```

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

- **Parameters** can have type annotations (`: String`, `: Int`, etc.)
- **Return type** annotation is optional (`: String`)
- The function body is a single expression (the return value)

### Type promises at validate

Each declared parameter and return type is a promise checked at every call,
including calls inside modules. If the value is known at `validate`, Carina
checks the value. Otherwise its type must prove that it fits the declaration;
a broad `String` cannot prove a literal union or a provider's refined string
type. A `String` reference argument into a provider's refined string parameter
currently passes `validate`; this gap is tracked in [carina#4594](https://github.com/carina-rs/carina/issues/4594).
An upstream export without a declared type cannot prove a promise either.
Unknown results of built-ins typed as `Any`, such as `lookup`, also need value evidence.
Unavailable provider schemas or failed upstream loads leave the promise check pending.
Definite type mismatches and invalid annotations remain declaration errors,
including in unused functions.

For an unknown return value, Carina types the body once with each parameter's
declared type. For example, `fn pick(x: String): 'dev' | 'prod' { x }` accepts
`pick('dev')`, but rejects `pick(src.identifier)` when `src.identifier` is an
unknown `String`. Declaring `x` with a narrower type supplies the corresponding
return evidence. Untyped parameters supply no type evidence for declared returns
or typed parameters in nested calls.

Calls inside a function body use the enclosing function's declared parameter
types, once per declaration. A non-proving nested call is reported at each outer
call whose relevant argument is unknown. An unused outer function does not
trigger that validation error.

Inside a module, unknown arguments use the types declared in that module's
`arguments` block. For example, `env: 'dev' | 'prod'` proves `pick(env)` fits the
same union even when the caller passes an unknown `String`. Known argument values
still receive value checks. The caller-to-module boundary is checked separately.

For `fn pick(x: 'dev' | 'prod'): String { x }`, passing that unknown `String`
produces:

```text
function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.
```

Return errors name the return value and the body's type. An untyped upstream
export error names the export and also suggests declaring its type.

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
fn make_tags(name: String, env: String = 'dev'): map(String) {
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
let piped = 'hello_world' |> upper() |> split('_') |> join('-')
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
