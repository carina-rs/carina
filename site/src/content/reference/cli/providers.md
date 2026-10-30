---
title: providers
---

Recover registry security state in `carina-providers.lock`.

Carina records per-provider security state in the lock file: whether the registry serves signed artifacts, the expected source identity, separate stable and pre-release workflow pins, an anti-rollback `sequence` observation and the reference point derived from it, and the versions the registry has reported as yanked. These are one-way ratchets — normal resolution can strengthen them but never weaken them, so a registry that stops signing, rolls its `sequence` backwards, or drops a yank flag is refused rather than accepted.

Carina also records a discovery pin for each registry host: the resolved value of every discovery field it consumes, currently the provider API base URL. This is host-level state, with one record shared by every provider resolved through that host.

That is the correct behavior when the registry is hostile. It is also what happens when the change is legitimate: a registry changes its discovery configuration, a provider rotates its signing identity after a repository transfer or CI migration, or a registry is restored from a backup and serves an older `sequence`. In those cases the relevant pin or ratchet has to be reset deliberately, by an operator who has confirmed out-of-band that the change is genuine.

The three subcommands here are those deliberate resets. Two target one provider; `repin-discovery` targets the shared registry host. Each clears one narrow piece of state and leaves everything else intact. None is ever performed automatically — an automatic reset would hand an attacker the whole mechanism, since tripping a guard would be enough to make the client clear it.

## repin-discovery

Discards a registry host's consumed discovery values so the next resolution can verify and establish a new discovery pin.

Use this when discovery fails because either value legitimately changed and you have confirmed the new registry configuration out-of-band. Because the pin belongs to the host, this command is keyed by hostname rather than provider source. Every provider entry and all per-provider security state are retained.

```bash
carina providers repin-discovery <HOST> [PATH] [--force]
```

## repin-identity

Discards every signer pin for the provider — its shared source pin and both channels' workflow pins, or its legacy certificate SAN and issuer — so the next signed artifact establishes a new source and the workflow for its channel.

Use this when signature verification fails because the signing source or workflow legitimately changed. The signature *requirement* survives: after re-pinning, an unsigned artifact is still refused. The stable source-ref rule also remains in force because it is derived from the version, not stored as a pin.

Everything else is retained, including the recorded yanked versions, the `sequence` observation and reference point, and the pinned checksum.

```bash
carina providers repin-identity <PROVIDER> [PATH] [--force]
```

## re-bootstrap

Discards the `sequence` observation, its reference point, and the record that the registry serves `sequence` at all. The lock records a reset-pending anchor state until the next successful resolution, so a failed attempt cannot consume the operator-authorized reset.

Use this when resolution fails because the registry's `sequence` went backwards, jumped further ahead than the client will accept, or stopped being served, and you have confirmed the registry is behaving legitimately. The next successful resolution consumes the marker and establishes a new reference point from what it observes; if the listing has no `sequence`, it records that sequence-less first contact so later resolutions stay silent.

The signer source and workflow pins, signature requirement, recorded yanked versions, and pinned checksum are all retained. In particular, a yanked version stays yanked — un-yanking is not an operation Carina offers, because a genuine un-yank cannot be distinguished from an attacker stripping the flag.

```bash
carina providers re-bootstrap <PROVIDER> [PATH] [--force]
```

Some refusals have no reset command: an artifact that lost its signature, its `valid_until` or its transparency-log entry, a stable release built from the wrong source ref, a dropped yank flag, or a changed checksum or hostname. The error then names no recovery command. When a registry listing is refused for several reasons at once, every reason is shown, and a recovery command is suggested only if each reason can be cleared by it.

## Usage

```bash
carina providers repin-discovery <HOST>     [PATH] [--force]
carina providers repin-identity  <PROVIDER> [PATH] [--force]
carina providers re-bootstrap    <PROVIDER> [PATH] [--force]
```

- **HOST** -- the registry hostname whose shared discovery pin should be cleared. The error message that sent you here names the exact host to use.
- **PROVIDER** -- the registry provider source, as `namespace/name` or `hostname/namespace/name`. The error message that sent you here names the exact string to use.
- **PATH** -- defaults to `.`. Must be a directory containing `carina-providers.lock`.
- **--force** -- skip the confirmation prompt. Intended for non-interactive use; prefer the prompt when running by hand.

Without `--force`, each command prints what it is about to discard before making any change. `repin-discovery` asks you to retype the registry host; the two provider-level commands ask you to retype the provider source. Answering with anything else cancels without touching the lock file.

## Examples

Re-pin discovery after a registry host legitimately changes its discovery document or API base URL:

```bash
carina providers repin-discovery registry.carina-rs.dev
carina init
```

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

In all three cases `carina init` is what actually establishes the replacement: a new host discovery pin, a new identity pin from the next verified signature, or a new reference point from the next accepted listing.

## Do not edit the lock file by hand

It is tempting to fix these failures by deleting the provider's entry from `carina-providers.lock` and re-running `carina init`. Do not — deleting the entry also discards the recorded yanked versions, which silently un-yanks every version the registry had withdrawn. These subcommands exist so that the narrow reset you actually want is available without that side effect.
