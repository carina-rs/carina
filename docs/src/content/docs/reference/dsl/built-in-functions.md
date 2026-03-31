---
title: Built-in Functions
description: Complete reference for all built-in functions available in the Carina DSL.
---

Carina provides built-in functions for string manipulation, list operations, map operations, networking, and security. All built-in functions support [partial application](/reference/dsl/expressions/#partial-application) when called with fewer arguments than expected.

## String Functions

### `upper`

Converts a string to uppercase.

```
upper(string: string) -> string
```

```crn
upper("hello")        # => "HELLO"
upper("Hello World")  # => "HELLO WORLD"
```

### `lower`

Converts a string to lowercase.

```
lower(string: string) -> string
```

```crn
lower("HELLO")        # => "hello"
lower("Hello World")  # => "hello world"
```

### `trim`

Removes leading and trailing whitespace from a string.

```
trim(string: string) -> string
```

```crn
trim("  hello  ")   # => "hello"
trim("\n hello \t")  # => "hello"
```

### `replace`

Replaces all occurrences of a search string. Data-last argument order for pipe compatibility.

```
replace(search: string, replacement: string, string: string) -> string
```

```crn
replace("-", "_", "hello-world")      # => "hello_world"
"hello-world" |> replace("-", "_")    # => "hello_world" (pipe form)
replace("::", ".", "foo::bar::baz")   # => "foo.bar.baz"
```

### `split`

Splits a string into a list using a separator.

```
split(separator: string, string: string) -> list
```

```crn
split("-", "a-b-c")      # => ["a", "b", "c"]
"a-b-c" |> split("-")    # => ["a", "b", "c"] (pipe form)
split("::", "a::b::c")   # => ["a", "b", "c"]
```

### `join`

Joins list elements into a string using a separator.

```
join(separator: string, list: list) -> string
```

```crn
join("-", ["a", "b", "c"])    # => "a-b-c"
["a", "b", "c"] |> join("-") # => "a-b-c" (pipe form)
join(", ", ["hello", 42])     # => "hello, 42"
```

## List Functions

### `concat`

Appends items to a list. Data-last argument order for pipe compatibility. The result is `base_list` followed by `items`.

```
concat(items: list, base_list: list) -> list
```

```crn
concat([3, 4], [1, 2])          # => [1, 2, 3, 4]
[1, 2] |> concat([3, 4])        # => [1, 2, 3, 4] (pipe form)
concat(["c"], ["a", "b"])        # => ["a", "b", "c"]
```

### `flatten`

Flattens nested lists by one level. Non-list elements are kept as-is.

```
flatten(list: list) -> list
```

```crn
flatten([[1, 2], [3, 4]])     # => [1, 2, 3, 4]
flatten([["a", "b"], ["c"]])  # => ["a", "b", "c"]
flatten([[1, 2], 3, [4]])     # => [1, 2, 3, 4]
```

Only one level of nesting is removed:

```crn
flatten([[1, [2, 3]]])  # => [1, [2, 3]]
```

### `length`

Returns the number of elements in a list or map, or the number of characters in a string.

```
length(value: list | map | string) -> int
```

```crn
length([1, 2, 3])       # => 3
length({a = 1, b = 2})  # => 2
length("hello")         # => 5
length([])              # => 0
```

### `map`

Extracts a field from each element of a collection. The accessor must be a dot-prefixed string.

```
map(accessor: string, collection: list | map) -> list | map
```

When applied to a list of maps, returns a list of the extracted values:

```crn
let subnets = [
  { name = "a", subnet_id = "id-1" },
  { name = "b", subnet_id = "id-2" },
]

map(".subnet_id", subnets)       # => ["id-1", "id-2"]
subnets |> map(".subnet_id")     # => ["id-1", "id-2"] (pipe form)
```

When applied to a map of maps, returns a map with the same keys and extracted values:

```crn
let envs = {
  dev = { cidr = "10.0.0.0/16", name = "development" }
  stg = { cidr = "10.1.0.0/16", name = "staging" }
}

envs |> map(".cidr")  # => { dev = "10.0.0.0/16", stg = "10.1.0.0/16" }
```

## Map Functions

### `keys`

Returns the keys of a map as a sorted list of strings.

```
keys(map: map) -> list
```

```crn
keys({ b = 2, a = 1, c = 3 })  # => ["a", "b", "c"]
keys({})                         # => []
```

### `values`

Returns the values of a map as a list, ordered by sorted keys.

```
values(map: map) -> list
```

```crn
values({ b = 2, a = 1, c = 3 })  # => [1, 2, 3]
values({})                         # => []
```

### `lookup`

Looks up a key in a map, returning a default value if the key is not found.

```
lookup(map: map, key: string, default: any) -> any
```

```crn
lookup({ a = "one", b = "two" }, "a", "default")  # => "one"
lookup({ a = "one", b = "two" }, "c", "default")  # => "default"
```

## Numeric Functions

### `min`

Returns the smaller of two numbers. If both are integers, returns an integer. If either is a float, returns a float.

```
min(a: number, b: number) -> number
```

```crn
min(3, 5)      # => 3
min(2.5, 1.0)  # => 1.0
min(1, 2.5)    # => 1.0
```

### `max`

Returns the larger of two numbers. If both are integers, returns an integer. If either is a float, returns a float.

```
max(a: number, b: number) -> number
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
cidr_subnet(prefix: string, newbits: int, netnum: int) -> string
```

- `prefix`: base CIDR string (e.g., `"10.0.0.0/16"`)
- `newbits`: number of additional bits for the subnet mask
- `netnum`: subnet number within the new address space

```crn
cidr_subnet("10.0.0.0/16", 8, 0)    # => "10.0.0.0/24"
cidr_subnet("10.0.0.0/16", 8, 1)    # => "10.0.1.0/24"
cidr_subnet("10.0.0.0/16", 8, 255)  # => "10.0.255.0/24"
cidr_subnet("10.0.0.0/8", 8, 10)    # => "10.10.0.0/16"
```

This is commonly used with `for` expressions to allocate subnets:

```crn
let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = cidr_subnet("10.0.0.0/8", 8, i)
  }
}
```

## Environment Functions

### `env`

Reads an environment variable. Returns an error if the variable is not set.

```
env(name: string) -> string
```

```crn
let home = env("HOME")
let db_host = env("DB_HOST")
```

## Security Functions

### `secret`

Marks a value as secret. The value is sent to the provider but stored only as a SHA256 hash in state. Plan output displays `(secret)` instead of the actual value.

```
secret(value: any) -> secret
```

```crn
awscc.rds.db_instance {
  master_user_password = secret(env("DB_PASSWORD"))
}
```

### `decrypt`

Decrypts ciphertext using the configured provider's encryption service (e.g., AWS KMS). The key argument is optional when the key identifier is embedded in the ciphertext.

```
decrypt(ciphertext: string, key?: string) -> string
```

```crn
# Key embedded in ciphertext (e.g., AWS KMS encrypted blob)
let password = decrypt("AQICAHh...")

# Explicit key
let password = decrypt("AQICAHh...", "alias/my-key")

# Combined with secret() to prevent storing the decrypted value in state
awscc.rds.db_instance {
  master_user_password = secret(decrypt("AQICAHh..."))
}
```

Requires a configured provider with encryption support. An error is raised if no provider is configured or credentials are unavailable.
