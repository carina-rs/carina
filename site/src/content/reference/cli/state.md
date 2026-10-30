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
| `--json` | Output the raw state-file JSON |

`state show --json` is a state-file inspection command, so its output retains
the persisted state-file encoding. Canonical enums use a `$enum` host payload,
redacted secrets use a `$secret` host payload, and each user map key beginning
with `$` has one additional leading `$` (for example, `$secret` is stored as
`$$secret`). This differs intentionally from the decoded output of
`state lookup --json`.

### `lookup`

Look up resource attributes from the state file.

```bash
carina state lookup [OPTIONS] <QUERY> [PATH]
```

**QUERY** can be:
- `<binding_or_name>` -- returns all attributes as a JSON object
- `<binding_or_name>.<attribute>` -- returns a single attribute value

The query is matched against resource bindings and resource identities, picking the longest one that is a prefix of the query, so a module-prefixed binding such as `r.distribution` resolves `r.distribution.id`. On a tie, a binding wins over an identity.

```bash
# Get all attributes of a resource
carina state lookup my_bucket

# Get a specific attribute
carina state lookup my_bucket.bucket
```

Single attribute values are returned as raw values (no quotes for strings), suitable for shell usage. Use `--json` to always output JSON.

Lookup output is decoded for use as ordinary JSON: canonical enums are their
provider API strings, `$`-prefixed user keys are unescaped, and redacted
secrets are the fixed string `(secret)`. It never emits the raw `$enum`,
`$secret`, or `$$` state-file representation. Consequently, a real redacted
secret remains distinguishable from a user map whose key is `$secret`.

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
through human output, `state lookup --json`, exports, or `upstream_state`;
decoded display views render a redacted secret as `(secret)`. Only
`state show --json` exposes the raw file encoding.
