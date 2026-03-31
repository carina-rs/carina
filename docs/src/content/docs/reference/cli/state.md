---
title: state
---

State management commands for inspecting, modifying, and maintaining the Carina state file.

## Usage

```bash
carina state <SUBCOMMAND> [OPTIONS]
```

## Subcommands

### `list`

List all managed resources from the state file.

```bash
carina state list [PATH]
```

Output shows provider, resource type, and binding name (or resource name) for each resource:

```
aws.s3.bucket my_bucket
aws.ec2.vpc main_vpc
aws.ec2.subnet public_subnet
```

Prints "No resources in state." if the state is empty.

### `show`

Show all managed resources with full attributes.

```bash
carina state show [OPTIONS] [PATH]
```

Output groups attributes under each resource:

```
# aws.s3.bucket (my_bucket)
  bucket = "my-bucket-name"
  versioning_status = "Enabled"
```

#### Flags

| Flag | Description |
|------|-------------|
| `--tui` | Display state in interactive TUI mode |

### `lookup`

Look up resource attributes from the state file.

```bash
carina state lookup [OPTIONS] <QUERY> [PATH]
```

**QUERY** can be:
- `<binding_or_name>` -- returns all attributes as a JSON object
- `<binding_or_name>.<attribute>` -- returns a single attribute value

The lookup searches by binding name first, then falls back to resource name.

```bash
# Get all attributes of a resource
carina state lookup my_bucket

# Get a specific attribute
carina state lookup my_bucket.bucket
```

Single attribute values are returned as raw values (no quotes for strings), suitable for shell usage. Use `--json` to always output JSON.

#### Flags

| Flag | Description |
|------|-------------|
| `--json` | Always output as JSON |

Shell completions are supported for both resource names and attribute names.

### `refresh`

Refresh state from cloud providers without planning or applying. Reads the current state of all managed resources from the providers and updates the state file.

```bash
carina state refresh [OPTIONS] [PATH]
```

#### Flags

| Flag | Description |
|------|-------------|
| `--lock <BOOL>` | Enable/disable state locking (default: `true`) |

### `bucket-delete`

Delete the state bucket. This is a destructive operation that removes the bucket and all state history.

```bash
carina state bucket-delete [OPTIONS] <BUCKET_NAME> [PATH]
```

The bucket name must match the `bucket` attribute in the backend configuration. Without `--force`, Carina prompts for confirmation by requiring the user to type the bucket name.

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
- **resources** -- list of managed resources with their provider, type, name, binding, attributes, and dependency bindings
