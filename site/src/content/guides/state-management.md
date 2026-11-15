---
title: "State Management"
description: "Learn how Carina tracks infrastructure state, configure an S3 backend, import existing resources, move and remove state entries, and reference upstream state."
---

Carina uses a state file to track which resources it manages and their current attributes. This guide covers how to configure state storage, manipulate state entries, and share state between projects.

## How state works

When you run `carina apply`, Carina records each managed resource and its attributes in a state file. On subsequent runs, it compares the desired state (your `.crn` files) with the recorded state to determine what needs to change.

By default, state is stored locally as `carina.state.json`. Use `backend local { path = '...' }` to store it at another local path, and configure the S3 backend for team usage.

`apply` and `destroy` checkpoint cumulative state while effects run. After interruption, the last durable checkpoint retains completed work; later work may require recovery. A create with a persisted continuation can be resumed.

### State format upgrades

State format v20 records a token-less `create_outcome_unknown` row when an
initial create fails after a request may have reached the cloud but before the
provider returns an identifier or continuation. It also makes
`prevent_destroy` tri-state, preserving the difference between an omitted
directive and an explicit `false` so provider-schema defaults can protect an
unspecified resource. Carina does not refresh, destroy, or re-create a
token-less unknown address automatically. If the resource exists, add the
import block shown by `carina plan`. If it does not exist, remove only that
recovery row with `carina state forget <TYPE> <IDENTITY>` and then plan again.

State format v21 stores current and deposed dependency metadata as complete,
typed resource IDs instead of binding-name strings. When Carina reads a v20
file, it classifies every legacy dependency against state rows and the current
expanded resource graph. A missing or ambiguous name on a configured row is a
migration error. An orphan or deposed row retains every known edge and prints
a warning for each unknown name before rewrite; unknown names are displayed,
never silently dropped or persisted in v21.

You can test this migration against a copy without changing either the copy or
the configured backend:

```bash
cp carina.state.json /tmp/carina-state-migration-check.json
carina state migrate \
  --check /absolute/path/to/infra \
  --state-url /tmp/carina-state-migration-check.json
```

`state migrate --check` performs the same graph-aware classification as
plan/apply without credentials, provider initialization, CRUD or data-source
calls, upstream-state reads, backend access, locks, network access, cache
writes, or state writes. It loads schema metadata only from exact installed
provider artifacts. Repeat `--provider-schema KIND=/absolute/provider.wasm`
to select explicit artifacts. The check requires providers built for the
current protocol (28), including on this metadata-only path. Older artifacts,
including released protocol-v12 providers, are rejected with a protocol-version
error because their schemas do not declare pattern match modes. Rebuild them
against the current SDK before checking the migration. A successful run reports
configured rows plus orphan/deposed warnings and confirms that no state was
written.

State format v17 adds a top-level `exports_version` that versions the exports
encoding separately from the rest of the state file. Readers that only need
exports (`upstream_state` and `carina export`) check `exports_version` instead
of the state format version, so a later state format bump does not break a
downstream project that runs an older Carina, as long as that Carina supports
the exports encoding. `exports_version` changes only when the encoding of
exported values changes.

This protection starts with v17 itself. A Carina that predates v17 reads
exports through the full state format check, so it rejects a v17 state with
"State file version 17 is newer than supported version 16". Upgrade the projects that read an
upstream state before applying the upstream project with v17.

`carina state lookup` reads resources only. To read exports from another
project's state, use `carina export --state-url <URL>`.

State format v14 stores a redacted secret structurally as a single-key object:

```json
{"$secret":"$argon2id$..."}
```

Ordinary strings are always strings, even when their text starts with
`_secret:` and otherwise looks like an older secret marker. User map keys such
as `$secret` are stored as `$$secret` so they cannot be confused with the
host-owned tag. Loading v13 or older state migrates legacy secret markers only
inside current and deposed resource attributes; exported strings retain their
literal value.

State format v13 gives host-owned values a reserved `$` namespace. Canonical
enums are stored under `$enum`; user map keys that begin with `$` are escaped
with one additional `$` and restored when state is read. Loading an older state
file migrates legacy enum objects and `$`-prefixed user keys automatically.

The v14 loader also preserves the v12 migration for old raw secret markers.
An `_secret:argon2:<hash>` value from an earlier state version is bound to the
provider, resource type, stored identity, and top-level attribute key from its
own row, then stored as a `$secret` object carrying an
`argon2-legacy:<row-salt-hex>:<hash-hex>` digest. This load-time upgrade does
not require the plaintext and does not create a resource change. The next state
write that has the matching plaintext replaces the legacy digest with a
standard `$argon2id$...` PHC digest containing a fresh random salt and explicit
Argon2 parameters.

## Configuring the S3 backend

Add a `backend` block to your `.crn` file:

```crn
backend s3 {
  bucket = 'my-carina-state'
  key    = 'production/carina.state.json'
  region = 'ap-northeast-1'
}

provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

The S3 backend stores state in the specified bucket and key. It also supports state locking to prevent concurrent modifications.

### Automatic bucket creation

By default, `carina apply` automatically creates the S3 bucket if it doesn't exist. This lets you get started without any manual setup:

```crn
backend s3 {
  bucket      = 'my-carina-state'
  key         = 'production/carina.state.json'
  region      = 'ap-northeast-1'
  auto_create = true  # This is the default; can be omitted
}
```

When the bucket is auto-created, Carina:

1. Creates the S3 bucket with public access blocked
2. Appends an `aws.s3.Bucket` resource definition to your `.crn` file
3. Registers the bucket as a **protected resource** in state, preventing it from being accidentally modified or destroyed

`carina plan` shows the upcoming bucket creation as a bootstrap plan before the main resource plan.

Set `auto_create = false` to disable implicit bucket creation. A missing bucket is an error unless a matching `aws.s3.Bucket` is explicitly declared for bootstrap.

To delete an auto-created state bucket, use:

```bash
carina state bucket-delete <bucket-name> --force
```

This is a destructive operation that removes the bucket and all state history.

## Configuring the GitHub backend

Store state in an existing GitHub.com repository and branch:

```crn
backend github {
  repository = 'owner/infra-state'
  branch = 'carina-state'
  path = 'states/prod/carina.state.json'
}
```

All three fields are required. Concrete expressions such as `env()` are
accepted. The repository and branch must already exist; Carina never creates
them or chooses a default branch. The path must be relative, with no empty or
`..` segments, and must not end in `.lock`. Carina stores its lease at
`<path>.lock`. Only `api.github.com` is supported.

Branch names are passed to GitHub as-is. GitHub rejects invalid ref names.

Authentication uses a non-empty `GITHUB_TOKEN`, otherwise
`gh auth token --hostname github.com`. Grant Contents read/write on the
state repository. Read-only consumers of `upstream_state`, exports, or state
commands need only Contents read. The token is kept outside backend
configuration, backend locks, and saved plans.

Carina trims surrounding ASCII whitespace from either credential source.
A whitespace-only `GITHUB_TOKEN` falls back to `gh`; control characters left
inside a token cause a credential error without displaying the token.
For fine-grained tokens, include the state repository in the repository selection.
The token must be allowed to push directly to the state branch. Rulesets or
branch protection must not block its direct pushes, including by requiring
changes through a pull request.

State and lease reads fail closed when Contents access is uncertain. If the path
returns 404, Carina makes one additional root Contents request at the same
commit. Only HTTP 200 proves Contents is readable and allows the path to be
treated as absent, including an empty root returning `[]`. Branch visibility
alone is insufficient; root HTTP 403/404 produces a Contents read permission
error, while other failures retain their API status.

For read-only access without a configuration directory, use:

```sh
carina state show --state-url 'github://owner/infra-state/states/prod/carina.state.json?branch=carina-state'
carina export --state-url 'github://owner/infra-state/states/prod/carina.state.json?branch=states%2Fprod'
```

The first URL uses the block's `carina-state` branch. The second shows
percent-encoding for a different branch named `states/prod`.
The `branch` query parameter is required. Percent-encode `/` in branch names
as `%2F`. These URLs never acquire a lock or modify state. Offline
`state migrate --check` accepts only a local copy, not a GitHub URL.

Use one branch per independently applied state. State and lease writes
use the Contents API with the observed blob SHA; mutations of different paths
on one branch can also conflict. Carina retries branch contention a bounded
number of times, and stops on a changed target or an unexpected response.
The maximum state size is 10,000,000 decoded bytes (10 MB), checked on reads
and before writes. Larger states fail with a size error; they are not split.

### Measured concurrency, not a GitHub guarantee

The concurrency behavior is measured, not guaranteed by GitHub's
documentation. In experiments on November 14, 2026, simultaneous creates
of the same lock file had exactly one winner in 50 of 50 trials, and
simultaneous updates using the same blob SHA had exactly one winner in
50 of 50 trials. Every loser returned HTTP 409. Simultaneous creates of
different paths on the same branch also conflicted in 50 of 50 trials.
These observations do not establish a guarantee during incidents or future
GitHub changes. Blob SHAs identify content, not monotonically increasing
generations, and checking the lease and writing state are separate requests.

M1 (measured November 15, 2026): GitHub ignores the SHA precondition when the
file is absent. A PUT carrying a deleted file's SHA recreates it with HTTP 201,
so a SHA precondition cannot detect a deletion.

Carina verifies that an acknowledged mutation's commit remains on the branch
with one compare request. A later commit on the same history is accepted;
a competing history is an unexpected outcome. This check does not prevent
later changes to the branch or file. Only the comparison's status and ahead/
behind counts are parsed. Compare responses have their own 1 MiB body
limit, separate from the state size limit. If that limit is exceeded, Carina
reports that the write was committed but could not be verified; it retains
the acknowledged state SHA for the next save. A November 15, 2026 measurement
across six commits adding 0.9–20 MB files returned 32,493 bytes: GitHub omitted
their large patches, though smaller text diffs do include patches.

When a renewal of an existing lease returns 201, Carina reports that the lock
is no longer held and attempts to delete the recreated lease using its returned
SHA. An expired lease that disappears during acquisition may still be created.
When a state write expected an existing file but returns 201, Carina reports
a concurrent-deletion conflict and keeps the created state file.

Locked state saves, including cleanup saves, require more than 65 seconds
remaining on the observed lease: 60 seconds plus a fixed 5-second clock-skew
allowance. This adds no request and does not change the cleanup time budget.
It narrows the expiry window; a force-unlock or takeover can still happen
between the ownership check and the state PUT. Removal of a recreated lease
can also fail or race with another client, and does not undo a successor's
work during that window.

If a mutation returns HTTP 500, 502, 503, or 504, Carina attempts to reconcile
it within the operation deadline, as it does for a lost transport reply. If
the outcome remains uncertain or a mutation times out, Carina returns the
error and remembers the uncertain write in memory. A mismatched blob
acknowledgement also retains this uncertainty. The next mutation in the same
process resolves it with a Contents metadata read (plus the root access check
if the path is absent): a blob SHA
matching the exact sent bytes is treated as our write, the prior SHA
means it did not commit, and a different SHA is a conflict. An acquisition or
renewal recovered this way recognizes its own lock UUID. Other lost replies
can also be recovered within the original deadline. For a release or
force-unlock with a lost reply, an absent lease or a different lock UUID
confirms release and preserves any successor's lease.

An existing state must be loaded by the backend before it can be replaced.
A fresh backend may create an absent state; discovering an existing blob SHA
during a save or raw export does not authorize replacing its contents.
Releasing an already absent lease succeeds. A force-unlock that confirms the
lease is absent returns `LockNotFound`; recovery of an earlier lost DELETE
reply still follows the release rule above.

Accepted limitation: if a mutation reply is lost while another client
force-unlocks or deletes in the same window, matching bytes are treated as our
write even if the PUT recreated a deleted path. Subsequent state writes still
carry a SHA precondition, so one of the competing writers loses.

### Cancellation cleanup limits

The first shutdown signal stops new provider work and lets in-flight work drain.
The second signal prioritizes state and lock cleanup and starts these limits:

| Backend | State flush | Total cleanup | Backend I/O limit during cleanup |
| --- | --- | --- | --- |
| Local | 1s | 2s | Local file operations |
| S3 | 1s | 2s | 300ms per request, one attempt |
| GitHub | 5s | 8s | 4.5s per save; 3s reserved for lock release |

Commands without a published mutation backend keep the 2s total. A late state
flush receives only the time remaining since the second signal. When its limit
expires, Carina attempts lock release. GitHub bounds each save and release by
the time remaining until 500ms before the supervisor deadline. A third signal
exits immediately.

GitHub writes each state or lock change as a separate commit. Measured GitHub
latencies were PUT p50 0.93s, p95 1.33s, max 1.88s; DELETE p50
0.85s; GET p50 0.28s. With an inline lease, a cleanup locked save with a
cached state SHA takes 3 requests (lease GET, conditional state PUT, compare),
and release takes 3 (lease GET, conditional DELETE, compare). A cold save
that discovers absent state takes 6, including a branch snapshot, state
metadata GET, and a root Contents readability proof. Before the inline-lease
optimization these paths took 4, 4, and 7, respectively. Split lease reads,
404 fallback, retries, and uncertain-outcome reconciliation can require more.
An experiment confirmed that the Contents API accepts the qualified
`refs/heads/<branch>` ref for both `main` and `exp/slash-branch`, returning
HTTP 200 and the same blob SHA as the short ref. This lets the inline lease
read use a qualified branch ref without an extra request. The ownership and
TTL check, SHA preconditions, and post-write reachability compare remain in place.

GitHub Actions sends SIGINT, then SIGTERM 7.5s later, then SIGKILL 2.5s
after SIGTERM. Carina's cleanup limit starts at the second signal, so Actions
can cut off GitHub's 8s cleanup with SIGKILL. The longer GitHub limit is
intentional given the measured request costs; it cannot extend the runner's
external deadline.

After a cut-off, run `carina state refresh` to reconcile state with the provider.
A cancelled run can leave its lock behind. The next run then reports:

```text
State is locked by <who> (lock ID: <id>, operation: apply, last updated <time>, expires <time>).
A previous run may have been stopped before it released the lock (for example, a CI job cancelled during cleanup); if so, the lock expires on its own at <time>.
If no other run is using this state, run: `carina force-unlock <id>`
```

Either wait for the 60-minute lease to expire or, once the previous process
has stopped, run the `carina force-unlock <id>` command it shows. Refresh
uses the state lock too, so resolve a leftover lock first.

### State history is kept

Keep the state repository **private, with narrow access**. Every version stays
in Git history. Once committed, a value cannot be removed from every copy,
including other people's clones; deleting it from the latest state does not
erase earlier versions. This retention is an explicit user decision, without
whole-state encryption.

State contains the following:

- **Plain text:** values not written as secrets, provider-populated outputs,
  and resume tokens of unfinished creates.
- **Hashed:** values written as secrets.

S3 by default keeps only the latest state. GitHub retains every committed
state and lease version, including values later changed or removed.

## Moving state to a different backend

<!-- derived-from ../reference/cli/init.md -->

When you change the `backend` block — switching between local, S3, and GitHub,
or changing the `key` while reorganizing component directories — the
state file must move with it. There is a single answer:

```bash
carina init --migrate-state
```

`init` compares the configured backend against the one recorded in
`carina-backend.lock`. If they differ, a bare `carina init` warns and
leaves migration pending. Project-backed readers — `plan`, `state list`,
`state show`, `state lookup`, `export`, and `upstream_state` consumers — warn
and read state from the old backend recorded in the lock. Explicit
`--state-url` reads bypass this selection. `apply`, `destroy`, and `state
refresh` refuse to run until you opt in with `--migrate-state` (or revert the
backend configuration), so you never silently abandon the old state. With
`--migrate-state`, `init` copies the state to the new backend, verifies the
copy, and rewrites the lock.

A local source is deleted after the verified copy; a remote source is
kept as a recoverable backup. Add `--force` only when a *different*
state already exists at the new backend and you intend to replace it.

See the [`init` reference](/reference/cli/init/) for the full semantics
and crash-recovery behavior.

## Importing existing resources

To bring an existing cloud resource under Carina management, use the `import` block:

```crn
import {
  to = awscc.ec2.Vpc 'main_vpc'
  id = 'vpc-0abc123def456'
}

let main_vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

- `to` is the resource type plus the `let` binding name of the resource that adopts the existing one. An address that matches no declared resource does not attach to it: an anonymous resource would be planned as a separate create.
- `id` is the cloud provider's identifier for the existing resource

Run `carina plan` to verify the import matches the resource definition, then `carina apply` to record it in state.

## Moving resources in state

When you rename a resource or reorganize your code, use the `moved` block to update the state without destroying and recreating the resource:

```crn
moved {
  from = awscc.ec2.Vpc 'vpc'
  to   = awscc.ec2.Vpc 'main_vpc'
}

let main_vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-vpc'
  }
}
```

Here the resource was previously bound as `let vpc` and is now bound as `let main_vpc`. `from` and `to` must use the same provider and resource type; changing either is not a move (use `removed` plus `import` instead). After the move is applied, you can remove the `moved` block from your code.

When `carina plan` or `carina apply` sees a destroy and create whose configured
create-only values match, it warns about a possible move. A unique match
includes the exact `moved` block to add; an ambiguous group lists every
destroyed and created address without choosing pairs. Resources with no
configured create-only values cannot be matched this way. Carina never rebinds
a named resource automatically, so the effects remain destroy and create until
you add the block explicitly.

### Moving a let-bound resource into a `for` loop

When a named resource becomes an element of a bound loop, move its state to
the expanded element address:

```crn
moved {
  from = awscc.ec2.Vpc 'vpc'
  to   = awscc.ec2.Vpc 'vpcs[0]'
}

let vpcs = for cidr in ['10.0.0.0/16'] {
  awscc.ec2.Vpc {
    cidr_block = cidr
  }
}
```

A map loop uses `'vpcs.key'` for an identifier-safe key, or
`"vpcs['key with space']"` when the key needs quoting. Elements from unbound
`for` and `if` expressions are anonymous: Carina derives their stable identity
from the resource schema instead of their position. Legacy `_forN[i]` state
rows migrate automatically the first time you plan or apply the updated
configuration.

To move an existing resource into an unbound loop, use the anonymous address
shown by `carina plan` as the `to` address. To give loop elements explicit
names instead, bind the loop with `let` and target the expanded bound address,
as in the example above.

## Removing resources from state

To stop managing a resource without destroying it in the cloud, use the `removed` block:

```crn
removed {
  from = awscc.ec2.Vpc 'main_vpc'
}
```

This removes the resource from Carina's state but leaves the actual cloud resource untouched. This is useful when transferring ownership of a resource to another tool or project.

`carina destroy` and `carina state refresh` treat the resource the same way: they drop it from state without reading it from the provider or deleting it in the cloud.

## Referencing upstream state

To read outputs from another Carina project's state, use `upstream_state`. The upstream project publishes values with an `exports` block:

```crn
# ../network/exports.crn
exports {
  vpc_id: aws.ec2.Vpc.Id = vpc.vpc_id
}
```

The downstream project reads them:

```crn
let network = upstream_state {
  source = "../network"
}

awscc.ec2.SecurityGroup {
  group_description = 'Web security group'
  vpc_id            = network.vpc_id

  tags = {
    Name = 'web-sg'
  }
}
```

The `upstream_state` expression points at an upstream project's directory. Carina loads that directory's configuration, resolves its backend, reads its state, and exposes the upstream's `exports` through the `let`-bound name. If that project has a pending backend migration, Carina warns and reads its locked old backend. In this example, `network.vpc_id` references the `vpc_id` value published by the `../network` project's `exports` block.

Exports and `upstream_state` are value-only state boundaries. A module may
expose a same-stack resource handle through `attributes`, but an `exports`
entry whose declared or inferred type is a resource handle is rejected. Export
an ID, ARN, or another resource attribute value instead; no handle is written
to state.

`source` is required and resolved relative to the enclosing `.crn` file's directory. The upstream directory must contain a valid Carina configuration (one or more `.crn` files — no filename is privileged) with an `exports` block that publishes the values consumers need.

## State CLI commands

Carina provides several CLI commands for inspecting and managing state:

```bash
# List all managed resources
carina state list

# Show all resources with full attributes
carina state show

# Look up a specific resource or attribute
carina state lookup vpc
carina state lookup vpc.vpc_id

# Refresh state from cloud providers
carina state refresh

# Force unlock a stuck state lock
carina force-unlock <lock-id>

# Delete a state bucket (destructive)
carina state bucket-delete <bucket-name> --force
```

## State locking

Carina locks state during `apply` and `destroy` to prevent concurrent modifications, with both backends: the local backend writes a lock file next to the state file, and the S3 backend writes a `<key>.lock` object. `plan` does not take the lock; it warns if the state changed while it was planning. A running operation keeps renewing its lock, and a lock left behind by a crashed run expires after an hour and can then be taken over. To release a stuck lock sooner, use `carina force-unlock <lock-id>`.

You can disable locking with the `--lock=false` flag:

```bash
carina apply --lock=false
```
