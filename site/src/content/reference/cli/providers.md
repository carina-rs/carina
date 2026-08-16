---
title: providers
---

Recover a registry provider's security state in `carina-providers.lock`.

Carina records per-provider security state in the lock file: whether the registry serves signed artifacts and which signing identity is expected, an anti-rollback `sequence` observation and the reference point derived from it, and the versions the registry has reported as yanked. These are one-way ratchets — normal resolution can strengthen them but never weaken them, so a registry that stops signing, rolls its `sequence` backwards, or drops a yank flag is refused rather than accepted.

That is the correct behavior when the registry is hostile. It is also what happens when the change is legitimate: a provider rotates its signing identity after a repository transfer or CI migration, or a registry is restored from a backup and serves an older `sequence`. In those cases the ratchet has to be reset deliberately, by an operator who has confirmed out-of-band that the change is genuine.

The two subcommands here are that reset. Each clears one narrow piece of state and leaves everything else intact. Neither is ever performed automatically — an automatic reset would hand an attacker the whole mechanism, since tripping a guard would be enough to make the client clear it.

## repin-identity

Discards the pinned signing identity so the next signed artifact establishes a new one.

Use this when signature verification fails because the registry's signing identity legitimately changed. The signature *requirement* survives: after re-pinning, an unsigned artifact is still refused. Only the expectation of *which* identity signs is replaced.

Everything else is retained, including the recorded yanked versions, the `sequence` observation and reference point, and the pinned checksum.

```bash
carina providers repin-identity <PROVIDER> [PATH]
```

## re-bootstrap

Discards the `sequence` observation and its reference point, returning the provider to the state it was in before first contact.

Use this when resolution fails because the registry's `sequence` went backwards or jumped further ahead than the client will accept, and you have confirmed the registry is behaving legitimately. The next successful resolution establishes a new reference point from what it observes.

The signing identity and signature requirement, the recorded yanked versions, and the pinned checksum are all retained. In particular, a yanked version stays yanked — un-yanking is not an operation Carina offers, because a genuine un-yank cannot be distinguished from an attacker stripping the flag.

```bash
carina providers re-bootstrap <PROVIDER> [PATH]
```

## Usage

```bash
carina providers repin-identity <PROVIDER> [PATH] [--force]
carina providers re-bootstrap  <PROVIDER> [PATH] [--force]
```

- **PROVIDER** -- the registry provider source, as `namespace/name` or `hostname/namespace/name`. The error message that sent you here names the exact string to use.
- **PATH** -- defaults to `.`. Must be a directory containing `carina-providers.lock`.
- **--force** -- skip the confirmation prompt. Intended for non-interactive use; prefer the prompt when running by hand.

Both commands print what they are about to discard, then ask you to retype the provider source before making any change. Answering with anything else cancels without touching the lock file.

## Examples

Re-pin a provider whose signing identity changed, after verifying the new identity out-of-band:

```bash
carina providers repin-identity carina-rs/aws
carina init
```

Reset freshness state for a registry restored from a backup:

```bash
carina providers re-bootstrap carina-rs/aws
carina init
```

In both cases `carina init` is what actually establishes the replacement: a new identity pin from the next verified signature, or a new reference point from the next accepted listing.

## Do not edit the lock file by hand

It is tempting to fix these failures by deleting the provider's entry from `carina-providers.lock` and re-running `carina init`. Do not — deleting the entry also discards the recorded yanked versions, which silently un-yanks every version the registry had withdrawn. These subcommands exist so that the narrow reset you actually want is available without that side effect.
