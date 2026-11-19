---
title: Built-in Functions
description: Complete reference for all built-in functions available in the Carina DSL.
---

Carina provides built-in functions for string manipulation, list operations, map operations, networking, and security. Built-ins with multiple required arguments support [partial application](/reference/dsl/expressions/#partial-application) when called with at least one argument but fewer than their required arity.

## String Functions

### `upper`

Converts a string to uppercase.

```
upper(string: String) -> String
```

```crn
upper('hello')        # => 'HELLO'
upper('Hello World')  # => 'HELLO WORLD'
```

### `lower`

Converts a string to lowercase.

```
lower(string: String) -> String
```

```crn
lower('HELLO')        # => 'hello'
lower('Hello World')  # => 'hello world'
```

### `trim`

Removes leading and trailing whitespace from a string.

```
trim(string: String) -> String
```

```crn
trim('  hello  ')   # => 'hello'
trim("\n hello \t")  # => 'hello'
```

### `replace`

Replaces all occurrences of a search string. Data-last argument order for pipe compatibility.

```
replace(search: String, replacement: String, string: String) -> String
```

```crn
replace('-', '_', 'hello-world')      # => 'hello_world'
'hello-world' |> replace('-', '_')    # => 'hello_world' (pipe form)
replace('::', '.', 'foo::bar::baz')   # => 'foo.bar.baz'
```

### `split`

Splits a string into a list using a separator.

```
split(separator: String, string: String) -> list(String)
```

```crn
split('-', 'a-b-c')      # => ['a', 'b', 'c']
'a-b-c' |> split('-')    # => ['a', 'b', 'c'] (pipe form)
split('::', 'a::b::c')   # => ['a', 'b', 'c']
```

### `join`

Joins list elements into a string using a separator. Elements must be scalars (strings, numbers, bools, enum values, or durations); if any element is secret, the result is secret.

```
join(separator: String, list: list(Any)) -> String
```

```crn
join('-', ['a', 'b', 'c'])    # => 'a-b-c'
['a', 'b', 'c'] |> join('-') # => 'a-b-c' (pipe form)
join(', ', ['hello', 42])     # => 'hello, 42'
```

Collection membership and element values can become available separately. `length` and `keys` can compute from known membership even when elements or values still await apply. Collection-producing functions preserve those pending leaves and their dependency selectors; `flatten` additionally needs each inner shape at the level being flattened. `join` requires known scalar elements.

## List Functions

### `concat`

Appends items to a list. Data-last argument order for pipe compatibility. The result is `base_list` followed by `items`.

```
concat(items: list(Any), base_list: list(Any)) -> list(Any)
```

```crn
concat([3, 4], [1, 2])          # => [1, 2, 3, 4]
[1, 2] |> concat([3, 4])        # => [1, 2, 3, 4] (pipe form)
concat(['c'], ['a', 'b'])        # => ['a', 'b', 'c']
```

### `flatten`

Flattens nested lists by one level. Non-list elements are kept as-is.

```
flatten(list: list(Any)) -> list(Any)
```

```crn
flatten([[1, 2], [3, 4]])     # => [1, 2, 3, 4]
flatten([['a', 'b'], ['c']])  # => ['a', 'b', 'c']
flatten([[1, 2], 3, [4]])     # => [1, 2, 3, 4]
```

Only one level of nesting is removed:

```crn
flatten([[1, [2, 3]]])  # => [1, [2, 3]]
```

### `length`

Returns the number of elements in a list or map, or the length of a string in bytes (UTF-8).

A resource loop binding is a structural handle, so `length(rs)` is invalid. Select attributes into an explicitly authored value collection instead. Counting a list with pending members does not make those members eligible as value keys: a simple bound loop needs distinct public Strings; use `(i, x)` for index addresses.

```
length(value: list(Any) | map(Any) | String) -> Int
```

```crn
length([1, 2, 3])       # => 3
length({a = 1, b = 2})  # => 2
length('hello')         # => 5
length([])              # => 0
```

### `map`

Extracts a field from each element of a collection. The accessor must be a dot-prefixed string.

```
map(accessor: String, collection: list(Any) | map(Any)) -> list(Any) | map(Any)
```

When applied to a list of maps, returns a list of the extracted values:

```crn
let subnets = [
  { name = 'a', subnet_id = 'id-1' },
  { name = 'b', subnet_id = 'id-2' },
]

map('.subnet_id', subnets)       # => ['id-1', 'id-2']
subnets |> map('.subnet_id')     # => ['id-1', 'id-2'] (pipe form)
```

When applied to a map of maps, returns a map with the same keys and extracted values:

```crn
let envs = {
  dev = { cidr = '10.0.0.0/16', name = 'development' }
  stg = { cidr = '10.1.0.0/16', name = 'staging' }
}

envs |> map('.cidr')  # => { dev = '10.0.0.0/16', stg = '10.1.0.0/16' }
```

## Map Functions

### `keys`

Returns the keys of a map as a sorted list of strings. It accepts a map value, not a resource-loop handle; `keys(rs)` is invalid for a bound loop. Simple list and map loop instances use String subscripts when selecting attributes.

```
keys(map: map(Any)) -> list(String)
```

```crn
keys({ b = 2, a = 1, c = 3 })  # => ['a', 'b', 'c']
keys({})                         # => []
```

### `values`

Returns the values of a map as a list, ordered by sorted keys.

```
values(map: map(Any)) -> list(Any)
```

```crn
values({ b = 2, a = 1, c = 3 })  # => [1, 2, 3]
values({})                         # => []
```

### `lookup`

Looks up a key in a map, returning a default value if the key is not found.

```
lookup(map: map(Any), key: String, default: Any) -> Any
```

```crn
lookup({ a = 'one', b = 'two' }, 'a', 'default')  # => 'one'
lookup({ a = 'one', b = 'two' }, 'c', 'default')  # => 'default'
```

## Numeric Functions

### `min`

Returns the smaller of two numbers, comparing their exact values without rounding integers. If both are integers, returns an integer. If either is a float, returns a float; if the selected integer cannot be represented exactly as a float, returns an error.

```
min(a: Number, b: Number) -> Number
```

```crn
min(3, 5)      # => 3
min(2.5, 1.0)  # => 1.0
min(1, 2.5)    # => 1.0
```

### `max`

Returns the larger of two numbers, comparing their exact values without rounding integers. If both are integers, returns an integer. If either is a float, returns a float; if the selected integer cannot be represented exactly as a float, returns an error.

```
max(a: Number, b: Number) -> Number
```

```crn
max(3, 5)      # => 5
max(2.5, 1.0)  # => 2.5
max(1, 2.5)    # => 2.5
```

## Networking Functions

### `cidr_subnet`

Calculates a subnet CIDR block within a given IP network address prefix.

```
cidr_subnet(prefix: String, newbits: Int, netnum: Int) -> String
```

- `prefix`: base CIDR string (e.g., `"10.0.0.0/16"`)
- `newbits`: number of additional bits for the subnet mask
- `netnum`: subnet number within the new address space

```crn
cidr_subnet('10.0.0.0/16', 8, 0)    # => '10.0.0.0/24'
cidr_subnet('10.0.0.0/16', 8, 1)    # => '10.0.1.0/24'
cidr_subnet('10.0.0.0/16', 8, 255)  # => '10.0.255.0/24'
cidr_subnet('10.0.0.0/8', 8, 10)    # => '10.10.0.0/16'
```

This is commonly used with `for` expressions to allocate subnets:

```crn
let vpcs = for (i, env) in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = cidr_subnet('10.0.0.0/8', 8, i)
  }
}
```

## Environment Functions

### `env`

Reads an environment variable. Returns an error if the variable is not set.

`env()` returns an ordinary string that can appear in diagnostics and plan output
and is stored as plaintext in state. An environment variable that must stay hidden
is written `secret(env('NAME'))`; this masks the value and hashes it in state.

```
env(name: String) -> String
```

```crn
let home = env('HOME')
let db_host = env('DB_HOST')
let password = secret(env('DB_PASSWORD'))
```

## Security Functions

### `secret`

Marks a value as secret. The value is sent to the provider, and plan output displays `(secret)` instead of the actual value. The serialized representation of `Secret` in resource state is an Argon2id hash rather than plaintext. Applying `secret()` to an already-secret value is idempotent.

Secret values become available when their inputs do. For example, `upper(secret(resource.identifier))` waits for the identifier while keeping the result protected.

`carina plan --out` also stores only an Argon2id hash in a typed secret
placeholder. `carina apply <plan.json>` re-evaluates the source configuration,
verifies that the secret is unchanged, and only then restores the plaintext
for the provider. Change a secret by creating a new saved plan; an existing
saved plan will refuse to apply it.

Values derived from `secret()` or `decrypt()` cannot be published through `exports`; `carina validate` and `carina plan` report `export '<name>' cannot hold a secret value`.

Backend attributes also cannot use secret-derived values. Backends accept only
ordinary configuration values, so validation rejects `secret()` and
`decrypt()` results in a `backend` block.

Carina decides whether a value is secret from the configuration as written: it follows references, operands, interpolation, user-function results and module outputs back to a `secret()` or `decrypt()` call. Values returned by a provider or exported from an upstream configuration are public values; wrap them with `secret()` where you use them if they must be protected. The alternatives of `??`, and the candidates and default of a `lookup()` whose map is known from the configuration, must be all secret or all public. Mixing them is a validation error, because the plan could not tell whether the selected value is a secret. Wrap the public alternatives in `secret()` to fix it. An empty list or map agrees with either kind.

To make that decision, Carina follows at most 127 references in a chain from an attribute and at most 4096 reference lookups per attribute. The same limits apply to each let binding, module argument, output, export and function body. A longer chain, or one that needs more lookups, is a validation error. Only references count toward the chain: functions, lists, maps, `??`, `if` and interpolation around a reference do not. Passing a value through a module output counts two references for each module, its argument and its output, so a chain of modules reaches the limit at 64 modules.

```
secret(value: Any) -> Secret
```

```crn
let password = secret(env('DB_PASSWORD'))
```

### `decrypt`

Decrypts ciphertext using the configured provider's encryption service (e.g., AWS KMS). The key argument is optional when the key identifier is embedded in the ciphertext.

```
decrypt(ciphertext: String, key?: String) -> Secret
```

```crn
# Key embedded in ciphertext (e.g., AWS KMS encrypted blob)
let password = decrypt('AQICAHh...')

# Explicit key
let password = decrypt('AQICAHh...', 'alias/my-key')

# The decrypted result is already secret (ciphertext is a placeholder)
let decrypted_password = decrypt('AQICAHh...')
```

The result is automatically masked as `(secret)` in plans, and providers receive the plaintext. Wrapping it in `secret()` is harmless but unnecessary.

Requires a configured provider with encryption support. An error is raised if no provider is configured or credentials are unavailable.
