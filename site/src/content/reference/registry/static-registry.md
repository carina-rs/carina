---
title: "Static Registry Layout"
description: "How to run a Carina provider registry from static files: which files to serve at which paths, and what they must contain."
---

<!-- derived-from ./protocol.md#1-service-discovery -->
<!-- derived-from ./protocol.md#2-list-provider-versions -->
<!-- derived-from ./protocol.md#3-get-provider-version -->
<!-- derived-from ./protocol.md#4-resolve-provider-revision -->
<!-- derived-from ./protocol.md#provider-documentation -->

A Carina provider registry can be run without any server software. Everything a client reads is a fixed file at a fixed path, so a registry can be a set of files served by a static HTTPS host: an object store bucket behind a CDN, a static site host, or a web server. This document says which files to put where and what they must contain. The normative contract is the [Registry Protocol](/reference/registry/protocol/) (`protocol.md`); this document restates the parts a static host needs and does not change them. When the two disagree, `protocol.md` wins and this document is wrong. Where the current `carina` CLI is stricter or looser than the protocol, this document says so explicitly.

The integrity of what you serve does not depend on the host. The client recomputes the SHA-256 of every provider binary, verifies its Sigstore signature when one is present, and pins what it saw in `carina-providers.lock` (protocol §3 "Client verification"). A static registry is therefore a distribution layer: it decides which versions exist and where their bytes are, and the client checks everything else.

What a static registry does not have is the official registry's publish API (protocol §7): no OIDC-authenticated publish, no namespace ownership checks, no server-enforced immutability. You write the files yourself, and keeping published files unchanged is your responsibility (protocol §9).

## Choosing a hostname

Users name your registry in the provider block of their configuration:

```hcl
provider aws {
  source  = "registry.example.com/carina-rs/aws"
  version = "0.7.3"
}
```

The hostname is part of the provider's identity: the client records it in `carina-providers.lock`, and `registry.example.com/carina-rs/aws` is a different provider from the same name on any other host (protocol §5). Changing the hostname later is a migration for every user, not a configuration change. Choose a hostname you will keep.

The registry must own the root of that hostname, because discovery is always fetched from `https://{hostname}/.well-known/carina.json`. A site served under a subpath, such as a project page at `user.github.io/repo/`, cannot be a registry. The host must also let you set `Content-Type` per path (see "Content-Type and caching"); a host that only infers types from file extensions, such as GitHub Pages, cannot serve the extensionless JSON files correctly. Static site generators that drop dot-directories silently remove `/.well-known/`; disable that processing.

## Files and paths

All paths are relative to `https://{hostname}/`. `{namespace}` and `{name}` are the provider address. `{version}` is the version string exactly as listed in the listing. `{revision}` is a branch, tag or commit SHA that users reference with `revision` (protocol §4).

| Path | Required | Content |
|---|---|---|
| `/.well-known/carina.json` | yes, once per host | Discovery document (protocol §1) |
| `/v1/providers/{namespace}/{name}/versions` | yes, per provider | Listing: the provider's versions (§2) |
| `/v1/providers/{namespace}/{name}/{version}/download` | yes, per listed version | Download document: where the binary is and its digest (§3) |
| `/v1/providers/{namespace}/{name}/{version}/sigstore.json` | when the version is signed | The binary's Sigstore bundle |
| `/v1/providers/{namespace}/{name}/{version}/docs.sigstore.json` | when the version is signed and has `docs` | The docs archive's Sigstore bundle |
| `/v1/providers/{namespace}/{name}/revisions/{revision}/download` | optional | Revision document (§4) |
| the provider binary | yes | Named by `download_url` |
| the docs archive | optional | Named by `docs.download_url` |
| `/providers/...` | optional | Documentation pages for people |

The binary and the docs archive can be anywhere reachable by unauthenticated HTTPS GET; they need not be on your host, and a release asset URL works. The binary's `sigstore.json` path is a convention: `signature.bundle_url` can point elsewhere. The path of the docs archive's Sigstore bundle is fixed by the protocol (§3, `docs.signature`).

Three of these paths have no file extension: `versions`, `download` and the revision `download`. Name the files exactly that, and see "Content-Type and caching" for how they must be served.

The `/v1/providers/` prefix is the value of `providers.v1` in the discovery document. You may choose another prefix. Make it a relative path or an absolute URL on the same origin as the discovery document. The protocol allows a cross-origin value only when it matches a federation configuration pinned in the client's lock (protocol §1); the current CLI refuses any cross-origin value.

A path that does not exist must answer `404`. When a user configures more than one registry, the client moves on to the next one only after a `404` or `410`; any other status, including `403`, stops resolution (protocol §5). Object stores commonly answer `403` for missing keys unless the reader may list the bucket, so check this on your host.

The host must serve everything over HTTPS and send HSTS, for example `Strict-Transport-Security: max-age=31536000` (protocol §1). The discovery URL must answer `2xx` directly: the client does not follow redirects on that fetch and treats a redirect as a failed discovery. Other fetches may redirect to another HTTPS URL. The current CLI accepts only certificates that chain to the public web PKI roots it bundles; a private certificate authority is not trusted.

Version strings are used verbatim in paths. Avoid semver build metadata (`1.0.0+build.5`): the `+` reaches the host unencoded, and some hosts, including S3's REST endpoint, decode it as a space and answer `404`.

## Required content

The examples below are the files in [`docs/registry/static-registry-example/minimal/`](https://github.com/carina-rs/carina/tree/main/docs/registry/static-registry-example/minimal), as served at `https://registry.example.com/`. They describe the real `carina-provider-aws` v0.7.3 release, so the digest is genuine and the set installs as is.

### Discovery document

<!-- example: minimal/.well-known/carina.json -->
```json
{
  "providers.v1": "/v1/providers/"
}
```

`providers.v1` is the only field the client reads. The client pins its resolved value, the absolute URL it resolves to, in the lock on first use and refuses a later change; moving the API base afterwards needs every user to run the discovery re-pin operation (protocol §2 "Operator recovery operations"). Choose it once.

### Listing

<!-- example: minimal/v1/providers/carina-rs/aws/versions -->
```json
{
  "versions": [
    { "version": "0.7.3", "protocols": ["1"] }
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `versions` | yes | Every published version, newest first by semver precedence |
| `versions[].version` | yes | The semantic version. A version with a pre-release component (`0.0.0-main.12.1.gabc123`) is a pre-release; anything else is stable |
| `versions[].protocols` | yes | Provider protocol versions the binary speaks; `["1"]` today |
| `versions[].yanked` | optional | `true` withdraws the version from new resolutions (see "Yanking a version") |

Never remove a version from the listing. A lock that pins it would stop resolving; withdraw it with `yanked` instead (protocol §9).

### Download document

<!-- example: minimal/v1/providers/carina-rs/aws/0.7.3/download -->
```json
{
  "protocols": ["1"],
  "filename": "carina-provider-aws-v0.7.3.wasm",
  "download_url": "https://github.com/carina-rs/carina-provider-aws/releases/download/v0.7.3/carina-provider-aws-v0.7.3.wasm",
  "shasum": "54dc14eff22c308a7657f5b602c0c4510d31e71bad598e40e654d8c3090b824f",
  "shasum_authored_by": "registry"
}
```

| Field | Required | Meaning |
|---|---|---|
| `protocols` | yes | Same as in the listing |
| `filename` | yes | The binary's file name |
| `download_url` | yes | HTTPS URL of the `.wasm`, answering `200`. Prefer a URL whose content can never change (a release asset or a digest-addressed path) |
| `shasum` | yes | Lowercase hex SHA-256 of the exact bytes at `download_url`. Compute it from the bytes you serve, for example with `shasum -a 256 provider.wasm` |
| `shasum_authored_by` | yes | `"registry"`: you computed `shasum` from the bytes, rather than copying a value someone else supplied |

The client downloads `download_url`, recomputes the SHA-256 and refuses the binary on any mismatch. It also pins `shasum` in the lock, so the bytes behind a published version must never change. If you need different bytes, publish a new version. The current CLI does not read `protocols`, `filename` or `shasum_authored_by`, but the protocol requires them; include them.

### Revision documents

Serve these only if your users reference providers by `revision`. A revision document is a download document with one more field, `version`, naming the listed version the revision resolves to. Unlike a version's download document it is mutable: after every publish and yank, recompute each revision file by the rules of protocol §4 ("Server-side resolution"). In short, a branch `b` resolves to the newest non-yanked pre-release `0.0.0-{b}.{number}.…`, and a tag `v{X}` resolves to the listed stable version `X`.

## Content-Type and caching

| Path | `Content-Type` | Suggested `Cache-Control` |
|---|---|---|
| `/.well-known/carina.json` | `application/json` | `public, max-age=60` |
| `.../versions` | `application/json` | `public, max-age=60` |
| `.../{version}/download` | `application/json` | `public, max-age=31536000, immutable` |
| `.../{version}/sigstore.json`, `.../docs.sigstore.json` | `application/json` | `public, max-age=31536000, immutable` |
| `.../revisions/{revision}/download` | `application/json` | `public, max-age=60` |
| the `.wasm`, if you host it | `application/wasm` | `public, max-age=31536000, immutable` |
| the docs archive, if you host it | `application/gzip` | `public, max-age=31536000, immutable` |
| documentation pages | `text/html; charset=utf-8` | `public, max-age=60` |

The extensionless JSON files are the thing static hosts most often get wrong. Most hosts infer the type from the extension and fall back to `application/octet-stream` or `text/plain` for a name like `versions`. Set the type explicitly: as object metadata in an object store, or with a per-path header rule on a static site host. `aws s3 sync` cannot infer it for extensionless names, and its `--content-type` applies to every file in the run, so upload the extensionless JSON files in a run of their own (`--exclude '*' --include '*/versions' --include '*/download'`) or with `aws s3 cp` per file. The protocol requires `application/json`; the current CLI does not check the type, but other clients and tools may.

The cache lifetimes follow from what may change. The listing and the revision documents change whenever you publish or yank, so keep their lifetime short. A version's download document and its bundles never change once published, so they can be cached forever. If your CDN supports purging, purge only the paths you changed.

## Publishing a version

Upload in this order, so that no client sees a listed version whose files are missing:

1. The binary, if you host it, and its `sigstore.json` if it is signed; the docs archive and `docs.sigstore.json`, if the version has docs.
2. The version's `download` document.
3. The listing, with the new version added, `sequence` increased and `valid_until` renewed if you use them.
4. Any revision documents the new version changes.
5. Purge the listing and revision paths from your CDN, if it caches them.

Uploading the listing first lets a client resolve a version whose `download` is still missing, and a CDN may cache that `404` under a path you marked immutable.

## Yanking a version

Set `"yanked": true` on the version's entry in the listing, increase `sequence` if you use it, and re-upload the listing. Keep the version's `download` document and its binary unchanged: locks that already pin the version still need them. Recompute any revision document that resolved to the yanked version. A yank cannot be undone; clients that have seen it refuse a listing that drops the flag (see below).

## Optional fields

A static registry can start with only the fields above and add the security fields later. The client applies each check only when its field is present, and from the first time it sees certain fields for a provider it requires them forever (see "Once added, never removed"). The full example in [`docs/registry/static-registry-example/full/`](https://github.com/carina-rs/carina/tree/main/docs/registry/static-registry-example/full) adds `sequence`, `generated_at`, `valid_until` and `signature`. Its `valid_until` was a week ahead when it was written and its `bundle_url` names `registry.example.com`, so regenerate those for your own host rather than copying them.

<!-- example: full/v1/providers/carina-rs/aws/versions -->
```json
{
  "sequence": 1,
  "generated_at": "2026-10-28T00:00:00Z",
  "valid_until": "2026-11-04T00:00:00Z",
  "versions": [
    { "version": "0.7.3", "protocols": ["1"] }
  ]
}
```

<!-- example: full/v1/providers/carina-rs/aws/0.7.3/download -->
```json
{
  "protocols": ["1"],
  "filename": "carina-provider-aws-v0.7.3.wasm",
  "download_url": "https://github.com/carina-rs/carina-provider-aws/releases/download/v0.7.3/carina-provider-aws-v0.7.3.wasm",
  "shasum": "54dc14eff22c308a7657f5b602c0c4510d31e71bad598e40e654d8c3090b824f",
  "shasum_authored_by": "registry",
  "signature": {
    "type": "sigstore-bundle",
    "certificate_identity": "https://github.com/carina-rs/carina-provider-aws/.github/workflows/release.yml@refs/tags/v0.7.3",
    "certificate_oidc_issuer": "https://token.actions.githubusercontent.com",
    "bundle_url": "https://registry.example.com/v1/providers/carina-rs/aws/0.7.3/sigstore.json"
  }
}
```

| Field | Where | When present | When absent |
|---|---|---|---|
| `sequence` | listing | The client refuses a listing whose `sequence` is lower than the one it last resolved with, so an old listing cannot hide a newer version | No rollback check |
| `valid_until` | listing | An RFC 3339 time. The client refuses the listing after it | No expiry check |
| `generated_at` | listing | An RFC 3339 time, informational: when you produced the listing | Nothing changes |
| `yanked: true` | a listing entry | The version is no longer chosen for new resolutions | The version is selectable |
| `signature` | download document | The client verifies the Sigstore bundle at `bundle_url` and pins the signer | No signature check |
| `transparency_log` | download document | Reserved for a checksum transparency log that does not exist yet (protocol §3). Leave it out | — |
| `docs` | download document | Points to the version's docs archive (protocol "Documentation provenance") | The version has no documentation |

### Once added, never removed

The client records in the lock, per provider, whether it has seen `sequence`, `valid_until`, `signature` and `transparency_log`. If a later response omits a field the lock says was present, the client refuses it as a downgrade attack; a genuine registry never loses a field. Adding one of these fields is therefore a one-way decision for that provider: from then on every listing, and every download document you publish, must carry it. Because presence is recorded per provider, not per version, this also reaches back: once a client has resolved a signed version, it refuses the provider's earlier unsigned versions too, and their download documents cannot be changed to add a signature. Start signing from a provider's first version if you can. This is why `transparency_log` should be left out: adding it would bind you to a field nothing yet fills.

If you stop serving a field anyway, every client that resolved the provider while the field was present refuses it from then on. For a dropped `valid_until` or `signature` there is no client-side recovery. The protocol's recovery operations reset only the `sequence` ordering, the pinned signer and the discovery pin (protocol §2 "Operator recovery operations"), and deleting the lock entry is not a sanctioned way out.

Two fields are sticky per version rather than per provider. Once a client has seen `yanked: true` on a version, a listing that serves that version without the flag is refused, so a yank cannot be undone (protocol §9). Once a client that reads documentation has seen `docs` on a version, a download document for that version without `docs` is treated as a failed documentation check, which warns rather than refuses (protocol "Documentation provenance"). The current CLI does not read `docs`.

### `sequence` is a counter

Increase `sequence` whenever the versions in the listing or their `yanked` flags change, and never reuse or lower a value. Re-serving the same versions with renewed `generated_at` and `valid_until` keeps the same `sequence`. Increase it in small steps, as a counter: the client also refuses a `sequence` that jumps implausibly far ahead of the one it last saw (protocol §2 "Fast-forward guard"; the current CLI refuses jumps of more than 1,000,000), so a timestamp is not a usable `sequence`.

### `valid_until` must be renewed before it passes

A listing past its `valid_until` is refused, even if nothing else changed. This bounds how long someone can keep serving a frozen old listing. It also means a static registry that sets `valid_until` must regenerate and re-upload every provider's listing on a schedule, before the time passes, for as long as the provider exists. Pick a window you can renew reliably, for example a seven-day validity renewed daily, so that a missed run does not take every user down. If you cannot run a scheduled job, do not set `valid_until`: once set, it cannot be dropped.

### Signatures come from the provider's release

You do not create the signature; the provider's release workflow does. It signs the binary keylessly with Sigstore and produces a bundle containing the certificate, the signature and an inline Rekor inclusion proof, which the client verifies offline. You host that bundle and point `signature.bundle_url` at it.

All four `signature` fields are required. `type` must be `"sigstore-bundle"`. `certificate_identity` and `certificate_oidc_issuer` must be present, but they are descriptive: the client reads the signer from the certificate, never from these fields, so copy them from the certificate for readers.

On the first signed version it sees for a provider, the client pins the signer: the source repository and, separately for stable and pre-release versions, the workflow (protocol §3 "Signer identity"). Later versions must be signed by the same signer. For a stable version the certificate must also have been made at the tag `refs/tags/v{version}`, so a stable release must be built from a tag named after its version. The certificate must carry the Fulcio extensions for the OIDC issuer, the source repository ID, its owner ID and the source ref; GitHub Actions certificates do.

### Documentation

`docs` is allowed only on stable versions. It has the shape protocol §3 defines: `download_url` and `shasum` of the docs archive (the protocol's "docs bundle", a gzip tar of Markdown), and, when the version is signed, `docs.signature` over the archive, signed in the same release run as the binary. On a signed version, `docs` without `docs.signature` fails documentation verification.

The client does not read anything under `/providers/`. The official registry renders human-readable pages there from each version's docs archive (protocol "Provider Documentation"); a static registry may serve its own HTML at those paths, or nothing.

## Checking a file set

For the host:

- `/.well-known/carina.json` answers `2xx` with no redirect, over HTTPS with HSTS, as `application/json`, with a same-origin `providers.v1`.
- A path that does not exist answers `404`.

For every provider:

- The listing and every download document parse as JSON and are served as `application/json`.
- Every version in the listing has a download document, and the SHA-256 of the bytes at its `download_url` equals its `shasum`.
- Any field from "Once added, never removed" that the provider has ever served is still present, and no version that was ever yanked has lost the flag.

The example sets in [`docs/registry/static-registry-example/`](https://github.com/carina-rs/carina/tree/main/docs/registry/static-registry-example) were run once, byte for byte, through the `carina init` verification of the current CLI with an in-process registry. That run had no network, so it did not cover the TLS and header requirements above. Its results:

- The minimal set installs.
- The full set installs offline and pins its signer.
- After the full set, dropping `sequence`, `signature` or `valid_until` is each refused as a downgrade.
- An expired `valid_until`, a binary that does not match `shasum`, and a cross-origin `providers.v1` are each refused.

A test in the carina repository (`carina-provider-resolver/tests/static_registry_examples.rs`) keeps the examples inlined above identical to those files.
