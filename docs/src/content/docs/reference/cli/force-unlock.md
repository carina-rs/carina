---
title: force-unlock
---

Force-release a stuck state lock.

Carina takes a lock on the state backend during `apply` and `destroy` to prevent concurrent writers. If a previous run was killed before it could release the lock, subsequent runs will refuse to proceed. `force-unlock` removes that lock manually.

Only use this when you are certain no other Carina process is currently writing to the same state. Releasing a lock while another process holds it can corrupt state.

## Usage

```bash
carina force-unlock <LOCK_ID> [PATH]
```

- **LOCK_ID** -- the ID printed by the failed run (or by the backend's lock metadata).
- **PATH** -- defaults to `.`. Must be a directory containing the backend configuration.

## Examples

Release the lock identified in a failed run:

```bash
carina force-unlock 1730000000000-abcd1234 .
```
