---
title: state
---

State management commands for inspecting, modifying, and maintaining the Carina state file.

## Usage

```bash
carina state <SUBCOMMAND> [OPTIONS]
```

For project-backed inspection commands, Carina compares the configured backend
with `carina-backend.lock`. While a backend migration is pending, `list`,
`show`, and `lookup` print a warning to stderr and read the locked old backend.
An explicit `--state-url` bypasses project backend resolution and reads exactly
the supplied URL.

## Subcommands

### `list`

List all managed resources from the state file.

```bash
carina state list [OPTIONS] [PATH]
```

Output shows provider, resource type, and binding (or resource identity when there is no binding) for each resource:

```
aws.s3.Bucket my_bucket
aws.ec2.Vpc main_vpc
aws.ec2.Subnet public_subnet
```

Rows whose state is not a plain applied resource carry a marker: `(no current instance)`, `(missing provider identifier)`, `(create in flight since <time>)`, `(create outcome unknown since <time>; the create may have reached the cloud)`, or `(deposed <key> <identifier>)` for a pre-replacement instance pending delete.

Prints "No resources in state." if the state is empty.

#### Flags

| Flag | Description |
|------|-------------|
| `--state-url <URL>` | Read state directly from a URL (`s3://bucket/key`, `file://path`, or a bare local path), bypassing `.crn` / backend resolution. Mutually exclusive with PATH |

### `show`

Show all managed resources with full attributes.

```bash
carina state show [OPTIONS] [PATH]
```

Output groups attributes under each resource:

```
# aws.s3.Bucket (my_bucket)
  bucket = 'my-bucket-name'
```

Human-readable output shows canonical enums as their provider API strings and
restores user map keys exactly as authored, including keys that start with
`$`.

#### Flags

| Flag | Description |
|------|-------------|
| `--state-url <URL>` | Read state directly from a URL, bypassing `.crn` / backend resolution. Mutually exclusive with PATH |
| `--tui` | Display state in interactive TUI mode |
| `--json` | Output the versioned public JSON document described below |

#### JSON output schema v1

`state show --json` returns a public inspection document that is independent
of the on-disk state format:

```json
{
  "schema_version": 1,
  "state": "present",
  "resources": [
    {
      "binding": "vpc",
      "identity": "main-vpc",
      "provider": "awscc",
      "resource_type": "ec2.Vpc",
      "protected": false,
      "current": {
        "kind": "applied",
        "identifier": "vpc-0123456789abcdef0",
        "attributes": {
          "cidr_block": "10.0.0.0/16"
        }
      },
      "deposed": [
        {
          "identifier": "vpc-0fedcba9876543210"
        }
      ]
    }
  ]
}
```

The top-level fields are:

| Field | Meaning |
|-------|---------|
| `schema_version` | Version of this public JSON contract. Consumers must check it before reading other fields. A version bump indicates an incompatible contract change. |
| `state` | `present` when a state file was loaded, or `absent` when the configured backend has no state file. |
| `resources` | Resource rows in state order. This is empty when `state` is `absent` or when the present state contains no resources. |

Each resource object has these fields:

| Field | Meaning |
|-------|---------|
| `binding` | DSL binding for the resource, or `null` for an anonymous resource. |
| `identity` | Stable Carina resource identity stored in state. |
| `provider` | Provider name, such as `awscc`. |
| `resource_type` | Provider resource type, such as `ec2.Vpc`. |
| `protected` | Whether Carina protects the resource from deletion. |
| `current` | Current-generation status, identifier, and attributes. |
| `deposed` | One object per older replacement generation still pending deletion. Each object contains its provider-side `identifier`. |

The `current` object has these fields:

| Field | Meaning |
|-------|---------|
| `kind` | One of the closed set described below. |
| `identifier` | Provider-side identifier when one is known, including during an in-flight create; otherwise `null`. |
| `attributes` | Sorted attribute object containing ordinary JSON values. It is empty for kinds that do not carry current attributes. |

The closed `current.kind` set in schema version 1 is:

| Kind | Meaning |
|------|---------|
| `applied` | A completed current resource instance with a provider identifier. |
| `none` | The row has no current instance, but may retain deposed generations. |
| `create_in_flight` | The provider accepted a create that has not completed. |
| `missing_identifier` | A legacy or malformed current instance has attributes but no provider identifier. |

Strings, numbers, booleans, lists, and maps are emitted as ordinary JSON.
Canonical enums use their provider API string. Persisted host encodings and
secret digests are never emitted. A secret or accepted write-only value is
always replaced by this marker:

```json
{"redacted": true}
```

User map keys are restored as authored, including keys beginning with `$`;
those keys are ordinary user data, not persisted `$enum` or `$secret` host
payloads.

When the configured backend has no state file, `state show --json` exits 0 and
prints:

```json
{"schema_version":1,"state":"absent","resources":[]}
```

Backend, read, migration, and parse failures remain errors and exit non-zero.
Non-JSON `state show` output is unchanged.

### `lookup`

Look up resource attributes from the state file.

```bash
carina state lookup [OPTIONS] <QUERY> [PATH]
```

**QUERY** can select:

- `<binding_or_name>` -- a whole resource
- `<binding_or_name>.<attribute>` -- one current attribute

The query is matched against resource bindings and resource identities, picking the longest one that is a prefix of the query, so a module-prefixed binding such as `r.distribution` resolves `r.distribution.id`. On a tie, a binding wins over an identity.

```bash
# Get all attributes of a resource
carina state lookup my_bucket

# Get a specific attribute
carina state lookup my_bucket.bucket
```

Without `--json`, single attribute values are returned as raw values (no
quotes for strings), suitable for shell usage. Human-readable lookup output is
unchanged.

With `--json`, lookup uses the same schema version 1 conversion as
`state show --json`:

- A whole-resource lookup returns that resource object, beginning with
  `binding` and ending with `deposed`. It does not wrap the object in the
  top-level document.
- An attribute lookup returns that value from `current.attributes`, including
  the `{"redacted": true}` marker for a secret or accepted write-only value.
- A whole-resource lookup of an in-flight create returns the resource object
  with `current.kind` set to `create_in_flight`. Looking up one of its
  attributes remains an error.

Lookup requires an existing state and a matching resource, so an absent state
remains an error. Canonical enums and user map keys use the same decoded forms
as `state show --json`; persisted `$enum`, `$secret`, and `$$` encodings are
never exposed.

#### Flags

| Flag | Description |
|------|-------------|
| `--state-url <URL>` | Read state directly from a URL, bypassing `.crn` / backend resolution. Mutually exclusive with PATH |
| `--json` | Always output as JSON |

`state lookup` reads resources only; to read values from an `exports` block, use [`carina export`](/reference/cli/export/).

Shell completions are supported for both resource names and attribute names.

### `refresh`

Refresh state from cloud providers without planning or applying. Reads the current state of all managed resources from the providers and updates the state file. Every provider must load before the refresh starts. Progress is shown the same way as for `carina apply` (see [Progress output](/reference/cli/apply/#progress-output)).

```bash
carina state refresh [OPTIONS] [PATH]
```

#### Flags

| Flag | Description |
|------|-------------|
| `--lock <BOOL>` | Enable/disable state locking (default: `true`) |

### `forget`

Forget a token-less create-outcome-unknown row after confirming that the
resource does not exist in the provider. The command cannot remove applied,
in-flight, missing-identifier, or absent rows; for ordinary state removal it
points to a `removed { from = ... }` block instead.

```bash
carina state forget <RESOURCE_TYPE> <IDENTITY> [OPTIONS] [PATH]
```

For example:

```bash
carina state forget awscc.ec2.Vpc "main-vpc"
```

The command acquires the configured state lock, shows the row, and asks for
confirmation before saving. After it succeeds, the next plan can propose the
create normally.

#### Flags

| Flag | Description |
|------|-------------|
| `--auto-approve` | Skip the confirmation prompt |

### `bucket-delete`

Delete the state bucket. This is a destructive operation that removes the bucket and all state history.

```bash
carina state bucket-delete [OPTIONS] <BUCKET_NAME> [PATH]
```

The bucket name must match the `bucket` attribute in the backend configuration. The configuration and its providers must load successfully; Carina checks this before asking for confirmation. Without `--force`, Carina prompts for confirmation by requiring the user to type the bucket name.

During a pending backend migration, `bucket-delete` targets the configured NEW backend, not the OLD backend recorded in `carina-backend.lock`.

#### Flags

| Flag | Description |
|------|-------------|
| `--force` | Force deletion without confirmation |

## Related Commands

### `force-unlock`

Force unlock a stuck state lock. This is a top-level command, not a state subcommand.

```bash
carina force-unlock <LOCK_ID> [PATH]
```

Use this when a previous operation was interrupted and left the state locked. The lock ID is shown in the error message when a lock conflict occurs.

## State File Format

Carina stores state in `carina.state.json` (local backend) or in an S3 bucket (remote backend). The state file tracks:

- **lineage** -- unique identifier for the state, used for drift detection
- **serial** -- incrementing version number
- **resources** -- list of managed resources with their provider, type, identity, binding, attributes, and dependency bindings
- **exports** -- values published by the `exports` block
- **exports_version** -- version of the exports encoding, independent of the state format version, so older Carina versions can still read exports from a newer state file

Since state format version 14, Carina reserves single-`$` object keys for host payloads.
Canonical enums are stored as `$enum` payloads and redacted secrets as
`$secret` payloads, while user keys beginning with `$` are escaped by adding
one leading `$`. Carina decodes that representation when values are shown
through public output, exports, or `upstream_state`. Human-readable state
output renders a redacted secret as `(secret)`; schema version 1 JSON uses
`{"redacted": true}`. The persisted representation is an internal storage
detail and is not the `state show --json` contract.
