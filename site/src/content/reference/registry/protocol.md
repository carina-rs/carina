---
title: "Registry Protocol"
description: "The Carina Provider Registry Protocol: the HTTP JSON API a Carina provider registry serves and a client verifies."
---

## Overview

The Carina Provider Registry Protocol is an HTTP JSON API for provider discovery, version resolution, and download URL retrieval.

In addition to the API, the registry hosts a web UI for provider documentation. `registry.carina-rs.dev` provides a single place for searching providers, browsing their docs, and inspecting version information.

The protocol is modeled after the Terraform Registry Protocol, but is significantly simplified because Carina providers are WASM binaries only (no per-platform builds).

## Design Principles

- **WASM-only**: no concept of os/arch
- **Metadata + documentation**: the registry is responsible for metadata, discovery, and provider documentation; binaries are hosted on each provider repo's GitHub Releases
- **Backward compatible**: the existing `github.com/owner/repo` source format continues to be supported
- **Gradual migration**: direct GitHub source references continue to work alongside the registry

## Provider Address Format

```
[hostname/]namespace/name
```

- If `hostname` is omitted, the default registry (`registry.carina-rs.dev`) is used
- The `github.com/owner/repo` form continues to be supported (resolved directly, bypassing any registry)

**Examples:**

| Reference | Resolves to |
|---|---|
| `carina-rs/aws` | `registry.carina-rs.dev/carina-rs/aws` |
| `github.com/carina-rs/carina-provider-aws` | Resolved directly via GitHub (existing behavior) |
| `example.com/myorg/custom` | Third-party registry |

## 1. Service Discovery

The registry exposes a well-known URL at the root of its domain.

### Request

```
GET https://{hostname}/.well-known/carina.json
```

### Response

```json
{
  "providers.v1": "/v1/providers/"
}
```

| Field | Type | Description |
|---|---|---|
| `providers.v1` | string | Base path of the Provider API. For the default registry this MUST be a **relative or same-origin** path (see below). |

This is an extension point for future additions such as `modules.v1`. The discovery pin below is scoped so that such additions do not trip it ("Discovery-document integrity").

### Discovery-document integrity

`.well-known/carina.json` is the **bootstrap** of all subsequent requests: a tampered `providers.v1` relocates the entire §2/§3/§4 API base *before* any signature or shasum check applies. It is the analogue of TUF's `root.json` problem, and the registry has no baked-in trust root to anchor it. Until a transparency log exists, constrain it:

- For the **default registry**, `providers.v1` MUST be a relative path or a same-origin absolute URL. A cross-origin absolute URL is honoured **only** when it matches an explicit, lock-pinned federation configuration (§6) — never silently from the discovery document alone.
- The discovery pin covers the **resolved value of every discovery field the client consumes** — today, exactly the API base resolved from `providers.v1` — never a hash of the document's bytes. For the API base, the resolved value is the absolute URL obtained by RFC 3986 reference resolution against the originally requested well-known URL — `https://{hostname}/.well-known/carina.json`, never a redirect target. Whatever the field, the client MUST NOT follow redirects on the discovery fetch: a redirect relocates the resolution base the same way a tampered `providers.v1` would. A redirect answer is a failed discovery fetch, not a discovery document and not a pin mismatch: only a 2xx body from the originally requested URL is compared against the pin or establishes it. Pinned values compare by byte equality of that resolved form: a document rewritten from a relative path to the absolute URL that path resolves to yields the same resolved bytes and does not trip the pin, while syntactic variants that resolve to different bytes do trip it — fail-closed simplicity in place of URL-equivalence rules. The client SHOULD pin those values in `carina-providers.lock` on first use; a client holding the pin MUST refuse a silently-changed pinned value on later fetches, and MUST NOT re-pin from a changed value automatically. That refusal also fires on legitimate change — a registry that relocates its API base trips the pin for every client holding it, since `providers.v1` is material every client consumes; the recovery is the operator-gated **discovery re-pin** operation (§2 "Operator recovery operations"). Changes outside the pinned material — an added field the client does not consume (the extension point above), a reserialization of the same values — do not trip the pin: a field the client never acts on cannot relocate the API or otherwise steer the client, whereas pricing every such change as a fleet-wide re-pin ceremony trains operators to rubber-stamp the vouch the ceremony exists to extract. The narrowing has a known cost: divergence confined to unconsumed content — including a document served differently to different clients — trips no pin; witnessing whole documents is the transparency log's job, not the pin's.
- The pin's material grows with consumption, under rules fixed here. A client that pins MUST bring every discovery field it acts on under the pin's discipline — pinned at its first use, compared on every later fetch — and MUST NOT rewrite or discard pinned material it does not consume: values pinned by a newer client version survive an older client's lock updates untouched, so a mixed-version fleet sharing one lock cannot silently shed a pin. When a future addition (such as `modules.v1`) graduates from ignored to consumed, the section defining it MUST define its pinned material, the first-contact constraints its first pin is verified against — the analogue of `providers.v1`'s same-origin-or-federation-configuration gate above, so adoption never pins a value nothing constrained — and the scope of the refusal a change to it trips and of the recovery that clears it (§2 "Discovery re-pin"). For a consumed field the document does not yet carry, absence pins nothing: the field's first verified appearance is its first contact and pins it under those constraints, and once pinned, a fetch that omits the field is a silent change the client MUST refuse.
- Transport MUST be HTTPS with HSTS; a downgraded or cache-poisoned discovery document is otherwise an unanchored redirect of the whole API.

## 2. List Provider Versions

Returns the list of available versions for a given provider.

### Request

```
GET /v1/providers/{namespace}/{name}/versions
```

### Response

```json
{
  "sequence": 42,
  "generated_at": "2026-05-31T00:00:00Z",
  "valid_until": "2026-06-07T00:00:00Z",
  "versions": [
    {
      "version": "0.3.0",
      "protocols": ["1"]
    },
    {
      "version": "0.2.0",
      "protocols": ["1"],
      "yanked": true
    }
  ]
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `sequence` | integer | yes (full deployment) | Monotonically increasing counter, bumped every time this provider's `versions` array changes (a version added or a `yanked` flag set). Re-issuing the same versions with renewed `generated_at`/`valid_until` keeps the same value. Used for freshness checks (see Freshness below). |
| `generated_at` | string (RFC 3339) | yes (full deployment) | When this listing was produced. |
| `valid_until` | string (RFC 3339) | yes (full deployment) | After this instant the listing is considered stale and clients SHOULD refuse it (anti-freeze, see Freshness below). |
| `versions` | array | yes | Array of version objects (newest first) |
| `versions[].version` | string | yes | Semantic version |
| `versions[].protocols` | array | yes | Supported Provider Protocol versions |
| `versions[].yanked` | boolean | optional | `true` if this version has been withdrawn from new resolutions (§9). Absent means not yanked; a registry MAY omit it entirely rather than emit `false`. A yanked version stays in this array — see §9 for why removal is not permitted. |

Unlike Terraform, there is no `platforms` field (WASM is platform-independent).

### Freshness — anti-rollback / anti-freeze

The per-version `download` response (§3) is signed and content-addressed, so an individual artifact cannot be forged. But the **versions listing itself is a collection** that asserts *which versions exist*, and a stale or hostile registry/mirror can serve an old listing that **hides a newer (patched) version**, pinning a client to a known-vulnerable-but-authentic one. Per-artifact integrity cannot detect this (every entry it serves is genuine). The `sequence` + `valid_until` fields are the minimal defense (the TUF *timestamp* role without an external witness):

- **Anti-rollback.** The client MUST persist accepted `sequence` observations per `(host, namespace/name)` in `carina-providers.lock`, but acceptance alone MUST NOT establish or advance a reference point. A successful resolution using a listing — including an existing lock-pin re-check — MUST establish or advance a reference point from its observation. That point alone is the rollback floor; lower values MUST be rejected.
- **Anti-freeze.** The client MUST treat a listing whose `valid_until` is in the past as stale and fail (or, where a degraded mode is acceptable, warn loudly) rather than silently trust it. This bounds how long an attacker can replay a frozen view.
- **Fast-forward guard.** A hostile registry could jump `sequence` far ahead and lock out legitimate updates, so the ceiling MUST bound advancement from the reference point, not the counter's magnitude. First contact has no point, so the guard MUST NOT apply or block acceptance; this is not an underivable ceiling, and the client MUST distinguish absence from a point at `0`. For an existing point, the ceiling MUST derive from a higher-integrity source (e.g. the checksum transparency log once it exists) or be bounded by a re-bootstrap recovery path ("Re-bootstrap" below); re-bootstrap MUST also establish a point. Accepted failed-resolution observations MUST remain persisted but MUST NOT move the point; otherwise an attacker can walk it forward just under the ceiling.
- **Durable ratchet acceptance.** The client MUST persist a listing's ratchet observation in the lock when accepting it for present or later use. Confirming a lock pin remains listed, reading its `yanked` state, and caching the listing are uses even without choosing a version; accepting a listing accepts every ratchet observation it carries, including `yanked: true` on versions the current resolution does not consider. The observation MUST be durable at acceptance and remain so however the resolution ends. An observation recorded before any successful provider pin carries no sequence reference point or reset state by definition; only the pinned provider record can carry that anchor state, and first contact is therefore the absence of a pinned record rather than the absence of observations. Proceeding after an anti-freeze warning counts as proceeding; an underivable fast-forward ceiling leaves the observation unaccepted, and declining the listing for that or any other reason, including anti-freeze, MUST NOT update the lock's ratchet state.

Durability and validity pull in opposite directions: delaying the lock update lets an induced failure erase an observation after the client proceeds; updating it prematurely lets a declined value poison the baseline. Persisting a guard-refused `sequence` hands the attacker the lock-out the guard exists to prevent.

This is an interim measure. The structural end-state is a checksum transparency log (an append-only, witnessed Merkle log of `namespace/name@version → shasum`), which provides anti-rollback, anti-omission, and anti-split-view in one structure.

### Field presence & downgrade protection (migration)

Several security fields are marked **"yes (full deployment)"** here and in §3 (`sequence`, `valid_until`, `signature`, …). A **minimal deployment** serves only the fields required without that mark, for example a hand-maintained static file set; a **full deployment** serves the marked fields as well. A registry adds each marked field as it implements the corresponding capability. A client MUST handle the transition **downgrade-safely**, so that a hostile registry cannot strip a protection a provider was already known to have:

- **Enforce-once-present.** A security check is applied only when its field is **present** in the response. While a field is absent, the corresponding check is skipped (so installs work against a minimal deployment).
- **Record presence in the lock.** On resolve, the client records, per `(host, namespace/name)`, whether each security field was present (and related state where applicable — e.g. the persisted `sequence` observation and the separate reference point, or the signing-identity pin held separately from the record that `signature` was present). Under the Freshness rules above, the reference point for `sequence` advances per `(host, namespace/name)`, `yanked: true` ratchets per `(host, namespace/name, version)`, and only the presence of `valid_until`, `signature`, and `transparency_log` ratchets per `(host, namespace/name)`. `valid_until` is checked against the current time, never against a previously seen value; its value is not a ratchet.
- **No silent downgrade.** If the lock records a field as previously **present** for a provider but a later response **omits** it, the client MUST reject the response (a registry that genuinely had the field does not lose it — published versions are immutable; an omission is an attack signal). A field going from absent→present **promotes** the lock and is enforced from then on.

This makes the move from a minimal to a full deployment happen automatically and per-provider on the client side: every check turns on the first time a registry serves its field, and can never be turned back off. The same rule governs §3's `signature` / `transparency_log`.

**`yanked` is sticky per version, not per provider.** The rule above keys presence on `(host, namespace/name)`, which is not enough for a per-version flag: a hostile registry could yank nothing, be recorded as "field absent", and thereafter drop the flag from any single version. So the client MUST record `yanked: true` in the lock against the specific `(host, namespace/name, version)` that carried it, and MUST reject a later listing that serves that version without the flag. Going `true` → absent for a version is an attack signal, not a correction; a genuine un-yank is indistinguishable from an attacker stripping the flag, which is what makes un-yank an operation this protocol does not offer (§9).

Note this is a one-way ratchet on a *withheld* version, so its failure mode is the safe one: the worst a stuck flag can do is refuse a version, never serve a withdrawn one.

**First contact pins the identity.** The absent→present promotion above covers *whether* a signature is required; the identity pin needs its own first-contact rule. What is pinned is the **signer** — the parts of the signing certificate that stay the same across a provider's releases: one source pin per `(host, namespace/name)` and one workflow pin per channel, stable and pre-release (§3 "Signer identity"). When no workflow is pinned for the channel of the version being resolved and no registry-independent source (§6) supplies an expectation, the client MUST pin, from the first signature in that channel that passes §3 verification, that channel's workflow — and the source as well, if no source is pinned yet; when a source is already pinned, the signature MUST match it before anything is pinned — trust-on-first-use, the value-level analogue of the promotion rule, and, like the fast-forward ceiling, an interim measure until the checksum transparency log can supply the expectation independently. The exposure is one hostile first response per provider, plus, for the second channel to be resolved, a choice among workflows of the already-pinned repository; everything after is held by the pin: a pinned signer MUST NOT be re-learned, overridden, or replaced from a response. The ref a signature was made at is never pinned: for a stable version it is checked against a fixed rule instead (§3 "Signer identity"), which applies from the very first install and so involves no first contact at all. Replacing a pin is exactly the operator-gated identity re-pin operation (below) — a client that silently re-pins from a response has re-implemented the TOFU downgrade that operation's automation ban exists to prevent.

### Operator recovery operations

The checks in this protocol sometimes reject legitimate state: the Freshness rules above refuse a registry restored from backup, §3's identity verification refuses a provider that legitimately rotated its signing identity, and §1's discovery pin refuses a registry that legitimately changed a pinned discovery value — today, relocated its API base. These are normal-operations failures, not compromises, and the protocol has to name the way out — otherwise operators invent one, and the obvious invention (delete the offending lock state and start over) discards far more than the state that caused the rejection. This section defines the only three sanctioned client-side recovery operations: **re-bootstrap** resets the freshness state, **identity re-pin** resets the pinned signing identity, **discovery re-pin** resets §1's host-level discovery pin. The set is deliberately closed for all the `carina-providers.lock` state it governs, per-provider entries and host-level discovery pin alike: a reset of that state this section does not define is not a recovery path but a bypass. (The lock-pinned federation configuration (§1) is not part of the set: it is operator-authored configuration, not verification state accumulated from responses, so editing it is an ordinary configuration change, not a reset this section must sanction.) All three are deliberate resets of `carina-providers.lock` state — the first two per-provider, discovery re-pin per-host — and they share one discipline:

- **Operator-only trigger.** Any of these operations MUST be performed only in response to a contemporaneous operator instruction naming the target it clears — the provider for the per-provider operations, the registry host for discovery re-pin — backed by the out-of-band operator determination its subsection defines, and MUST NOT be automated — never as a side effect of a failed resolution, and never under a standing flag, configuration, or unattended job that pre-approves it. The reason is structural and shared: an automatic reset converts a protection's own rejection into its bypass — the attacker never needs to defeat the protection, only to trip it and let the client clear it. Each operation's subsection names its instantiation of that attack. The manual act is the clearing alone; the re-establishment that follows is required, automatic behavior of the next successful acquisition its subsection defines — the next successful resolution for the per-provider operations, the next verified discovery fetch for discovery re-pin — and this prohibition does not extend to it.
- **Named, disjoint state.** Each operation clears exactly the state its subsection names, and the three sets are pairwise disjoint: performing one MUST NOT disturb the state of the others, and the clears are order-independent. Their re-acquisitions are not: a cleared discovery pin gates every request to its host, so a per-provider re-establishment at that host can complete only after the host's pin is re-established — a sequencing fact about re-acquisition, not a coupling between the clears. The clear MUST act on the same state the operator was shown: one lookup of the target — a read of the `(host, namespace/name)` entry for the per-provider operations, a read of the host's pin record for discovery re-pin — feeds both the display (Informed decision below) and the write, never two lookups that can disagree. A clear landing on any state other than the named fields of the named target is not a scoping slip; it destroys guarantees this protocol makes irreversible (§9). No operation is a wholesale reset of a provider's lock entry, still less of a host's entries.
- **Never entry deletion.** When a rejection is one these operations recover from, the client error SHOULD name the specific operation. A client error MUST NOT instruct deleting the provider's lock entry — nor any wholesale form of it: every entry for a host, or the lock file itself — whatever the rejection. Deletion discards recorded `yanked: true` flags along the way — flags §9 makes deliberately irreversible (un-yank is an operation this protocol does not offer) — so advice amounting to "delete the entry and start over" quietly revokes a §9 guarantee. Nor is yank the only casualty: deletion takes the presence records and the identity pin with it, so the provider's next resolution is full, unguarded first contact ("First contact pins the identity" above) — a provider with nothing yanked is not safe to delete. The host-level rejection scales the temptation up along with the damage: a tripped discovery pin fails every provider resolved through the host at once, and the wholesale way out — delete every entry for the host, or the lock file itself — multiplies the same losses across all of them, which is why the prohibition above names the wholesale forms alongside the single entry. A recovery operation MUST NOT clear those flags.
- **Informed decision.** Before proceeding, the client SHOULD display what the operation will discard — the concrete values (the persisted observation and reference point, the pinned source and workflows (or the legacy pin), the pinned discovery values — today, the resolved API base), not state-category names. And because each operation ends in re-acquisition, the client SHOULD surface the newly established value when it arrives — the reference point the next successful resolution establishes, the source and workflow pinned at each post-re-pin first contact, the discovery values pinned by the next verified discovery fetch (today, the API base) — so the operator can reject an acquisition their determination never covered. The operator's out-of-band determination is the integrity source that replaces the discarded knowledge — a human vouches for the discontinuity, which is why the trigger cannot be automated — and a vouch made blind, without seeing the state at stake, replaces nothing.

#### Re-bootstrap (freshness reset)

Re-bootstrap is the recovery path the Fast-forward guard's ceiling is allowed to lean on: a legitimate registry restored from backup, or one with a re-issue bug, serves a `sequence` below the reference point, and every client refuses it — correctly, per the Freshness rules above.

- **Determination and attack.** The operator's out-of-band determination is that the rejected `sequence` state is legitimate (a restore, a re-issue bug, a deliberate registry reset) or that a fast-forward ceiling cannot be derived. The shared automation ban lands here as: a hostile listing that trips the rollback or ceiling check would become a self-clearing obstacle, so an attacker would never need to defeat the ratchet, only to trip it and let the client reset itself.
- **What it clears.** The persisted `sequence` observation and the reference point for the single `(host, namespace/name)` it targets — both together; clearing one without the other is not a partial re-bootstrap but an undefined state. For a pinned provider, the pinned lock record replaces the reference point with a typed reset-pending marker, which cannot coexist with an established anchor and survives failed resolution attempts and failed saves. Pre-pin observations have no anchor state to replace; clearing one leaves the source at first contact. A surviving observation is dormant ordering knowledge: any future rule that derives a floor or ceiling from observations would re-arm it into exactly the lock-out being recovered from. The provider returns to the first-contact state of the Freshness rules only until the next successful resolution re-establishes a point or records that the listing remains sequence-less (below). Clearing the observation also clears the record that `sequence` was present — remembering that ordering was mandatory while forgetting the ordering is not a coherent state — so the next listing to omit `sequence` is first contact, not a downgrade signal. This is the one downgrade re-bootstrap sanctions, bounded by the same operator decision that authorized the reset. (Durable ratchet acceptance binds what a *resolution* may do to the lock; this operator operation is the one sanctioned removal, and only of this state. §5's pinned "§2 `sequence`" *is* this freshness state, not a separate pin that survives.) Everything else in the lock entry survives per the shared discipline: the recorded presence of every other field (`valid_until`, `signature`, `transparency_log` — clearing those would reopen the hole "No silent downgrade" closes), recorded `yanked: true` flags, and the pinned `shasum` and signer pins (§3 "Signer identity" — the source and workflow pins, or a legacy pin). In particular, re-bootstrap is not identity re-pin (below) and does not touch the signer pins.
- **Establishing the new point.** Re-bootstrap itself establishes nothing: a point taken at reset time from the registry's current claim would come from a listing no successful resolution has accepted — exactly the poisoned baseline the Freshness rules exist to prevent. Instead, after re-bootstrap the provider has no reference point, the Fast-forward guard's first-contact rule applies to the next acceptance, and the reset-pending marker is consumed only by a successful resolution. A listing with `sequence` establishes the new reference point; a listing without it consumes the marker into the ordinary already-seen sequence-less state, preventing the first-contact notice from repeating forever. That is how the guard's "re-bootstrap MUST also establish a point" obligation is discharged when a point exists, while a sequence-less registry does not retain a permanent first-contact exemption.
- **What "bounded by" buys.** The reset forgets the *ordering* of one provider's listings and nothing else; every content control — shasum recomputation, signature identity, the `yanked` ratchet, presence enforcement for every field except `sequence` (the sanctioned downgrade above) — still applies unchanged to the next listing accepted. This narrowness, plus the operator vouch the shared discipline requires, is what "bounded by a re-bootstrap recovery path" buys the Fast-forward guard: a ceiling without a higher-integrity derivation may falsely reject a legitimate fast-forward, and re-bootstrap bounds that failure to a deliberate, informed, per-provider human action instead of a permanent lock-out.

#### Identity re-pin (signing-identity reset)

When a provider legitimately rotates its signing identity — a CI migration, a renamed signing workflow, or a repository transfer once §8's owner re-verification has resumed ingest (re-pin recovers the client's pin, not the registry-side freeze) — every client holding the old pin refuses its §3 responses, correctly per §3 "Client verification". Identity re-pin is the recovery path for that rejection.

- **Determination and attack.** The operator's out-of-band determination is that the identity legitimately changed. The shared automation ban lands here as a TOFU downgrade: a client that accepts a replacement identity automatically hands an attacker the pin — presenting any mismatched identity would make the client clear and re-learn it, so the attacker would never need to forge the pinned identity, only to present a different one.
- **What it clears.** Every signer pin (§3 "Signer identity") of the single `(host, namespace/name)` it targets — the source pin and both channels' workflow pins, or the legacy pin of a lock that predates them — and nothing else. Clearing one channel alone would not recover the usual rotations: a repository transfer changes the source both channels share, and a channel-scoped reset that kept a source pin would refuse the transferred repository anyway. (§5's pinned "signer" *is* this pin set, not a separate copy that survives.) The freshness state (observation and reference point), recorded `yanked: true` flags, recorded field presence (for `signature` too — the pin is separate state; next bullet), and the pinned `shasum` all survive per the shared discipline.
- **Presence is not the pin.** The lock's record that `signature` was present and the pinned identity are separate state: re-pin changes *which* identity is expected, never *whether* a signature is required. Between the re-pin and the next successful resolution the client is in exactly the state "signature required, no signer currently pinned", and an unsigned response MUST still be rejected ("No silent downgrade" above). A lock schema that derives signature presence from the existence of an identity pin cannot represent that state and cannot conform: in such a schema, clearing the pin silently disarms the signed→unsigned refusal, leaving a provider that just performed a legitimate rotation accepting unsigned artifacts — a strictly worse outcome than the entry deletion this section forbids.
- **Where the new expectation comes from.** Re-pin clears the pin; it MUST NOT supply a replacement — not even from the response whose rejection prompted the operator, since that value is exactly the one under question. The provider returns to signature first contact: the next successful resolution re-establishes the pins the way first contact establishes it — from a registry-independent source (§6) when one supplies an expectation, else by pinning the first verified signature's signer ("First contact pins the identity" above). Re-pin never touches the stable channel's source-ref rule (§3 "Signer identity"): that rule is not a pin, so there is nothing to clear and nothing a rotation can loosen. What makes returning to first contact acceptable is the operator's out-of-band determination — the same integrity uplift re-bootstrap adds to freshness first contact; without it, this is the TOFU downgrade the automation ban forbids.

#### Discovery re-pin (host-level discovery reset)

When a registry legitimately changes a value §1's discovery pin covers — today, relocating its API base — every client whose pin covers that value (for the base, every client holding the pin) refuses the host — correctly, per §1 "Discovery-document integrity". Discovery re-pin is the recovery path for that rejection. Unlike the two operations above, it is per-host, not per-provider: the state it clears anchors the API base that every subsequent §2/§3/§4 request for that host resolves against.

- **Determination and attack.** The operator's out-of-band determination is that the change to the pinned discovery values is a legitimate act of the registry's operators — an announced API restructure. It is a vouch for the *discontinuity* at that host, never for a particular replacement value: unlike the signing identity, which a registry-independent source (§6) can attest, and the freshness point, which the Freshness rules guard at its next establishment, a candidate discovery document has no independent source that could authenticate it — so what the client accepts next is instead constrained and surfaced below ("Establishing the new pin"). The shared automation ban lands here on §1's own terms: the discovery document is the analogue of TUF's `root.json`, so a client that re-pins automatically on a changed value converts the pin's refusal into the unanchored redirect of the entire API the pin exists to prevent — an attacker never needs the pinned base, only to serve a different one and let the client adopt it.
- **What it clears.** The host's discovery pin record for the single host it targets — the resolved values §1's pin covers, today exactly the resolved API base, so the record and the base are one and the same. A future consumed value extends this operation the way it extends §1's pin: its defining section scopes the refusal a change to it trips and the recovery that clears it (§1 "Discovery-document integrity"), rather than this subsection pre-committing every future value to a whole-record clear. Whatever that scoping, it bounds the clear from above, and the performing client's own coverage bounds it again: the operation clears only pinned values that are both inside its scope and consumed by that client, and pinned material the client does not consume survives untouched, per §1's survival rule — the Informed-decision display cannot show, nor the operator's vouch cover, a value the client does not understand. Nothing else: every per-provider lock entry for that host survives per the shared discipline — freshness state (observations and reference points), pinned signing identities, pinned `shasum`s and resolved hostnames (§5), recorded field presence, and recorded `yanked: true` flags — and so does the host's lock-pinned federation configuration (§1), which the very next fetch's cross-origin verification depends on. Their survival is not scope hygiene alone; it is what bounds the operation. §1 is the bootstrap of all subsequent requests, so there is no outer pin to fall back on while the discovery pin is cleared — the window is the client's first contact with the host all over again — but it is first contact for the *location* of the API only: whatever the newly pinned base serves is still checked against every surviving per-provider control, so a wrongly vouched document can relocate the API but cannot roll back, unfreeze, unsign, or un-yank any provider the lock already knows. That bound reaches only providers the lock already knows: a provider first resolved through the newly pinned base before the operator has confirmed it ("Establishing the new pin" below) takes its entire first contact — identity pin included — through that base, so the one-hostile-first-response exposure of "First contact pins the identity" above scales here by every such provider; the surviving controls of known providers do not guard those acquisitions. A discovery re-pin that also cleared per-provider state would widen exactly the window it exists to make survivable.
- **One pin, not per-entry copies.** The cleared state is host-level: one record of the pinned discovery values per host, which per-provider entries reference. (§5 "Pinning" describes this same host-level record; it is this pin, not a separate copy that survives.) That is what lets the shared discipline's single-lookup rule apply at all — one read of the host's pin record feeds both the display and the write. A lock schema that instead replicates the pin into every provider entry for the host can represent a state this operation must never produce — a host whose entries disagree about where its API lives, a re-pin applied to some entries and not others — and offers the discipline no single state to read or write; like the presence-derived schema identity re-pin rules out, such a schema cannot conform.
- **Establishing the new pin.** Re-pin clears the pin; it MUST NOT supply a replacement — not even from the document whose rejection prompted the operator, since that value is exactly the one under question. The next discovery fetch is §1 first contact again: the client MUST verify the new document against §1's constraints as on first use — HTTPS with HSTS, resolution against the originally requested well-known URL with redirects refused, and for the default registry a relative or same-origin `providers.v1`, with a cross-origin value honoured only against a matching lock-pinned federation configuration, never from the document alone — and MUST pin the values the operation cleared — today, the newly resolved API base; a future value under the scoping its defining section sets (§1) — that defining section is also where a legitimately retired field's recovery would have to be defined — from the first fetch that passes that verification, re-establishing the state §1 "Discovery-document integrity" describes; pinned values that survived the clear are compared against that fetch, never re-pinned, per §1. Re-pin clears a pin the client necessarily holds; completing the operation without re-establishing one would leave the host silently unpinned — a standing downgrade no operator vouch covered. Because the operator's vouch named no replacement value, surfacing the newly pinned values (Informed decision above) — today, the base — carries more weight here than for either per-provider operation: it is the operator's one chance to reject a value their determination never covered.

Like the pin it recovers, this operation is interim: the structural end-state for discovery integrity is a transparency log for the discovery document (§1) — a log that exists nowhere yet, even as design, and is not the §2 checksum transparency log, which witnesses version→shasum claims and cannot witness where a host's API lives. Such a log replaces the operator's per-discontinuity vouch — paid, like the sibling operations' vouches, once per client holding the cleared state — with a witnessed, append-only record: a better answer than any pin-and-reset discipline for state that has no outer anchor.

### Error

| Status | Description |
|---|---|
| `404` | The provider does not exist |

## 3. Get Provider Version

Returns download information for a specific version of a provider.

### Request

```
GET /v1/providers/{namespace}/{name}/{version}/download
```

Terraform uses `/download/{os}/{arch}`, but Carina only ships WASM, so os/arch parameters are unnecessary.

### Response

```json
{
  "protocols": ["1"],
  "filename": "carina-provider-aws-v0.3.0.wasm",
  "download_url": "https://cdn.carina-rs.dev/blob/sha256:5f9c7aa7...",
  "shasum": "5f9c7aa76b7c34d722fc9123208e26b22d60440cb47150dd04733b9b94f4541a",
  "shasum_authored_by": "registry",
  "signature": {
    "type": "sigstore-bundle",
    "certificate_identity": "https://github.com/carina-rs/carina-provider-aws/.github/workflows/release.yml@refs/tags/v0.3.0",
    "certificate_oidc_issuer": "https://token.actions.githubusercontent.com",
    "bundle_url": "https://cdn.carina-rs.dev/sig/sha256:5f9c7aa7.../sigstore.json"
  },
  "transparency_log": {
    "rekor_uuid": "...",
    "rekor_index": 123456,
    "inclusion_proof_url": "..."
  }
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `protocols` | array | yes | Supported Provider Protocol versions |
| `filename` | string | yes | Filename of the WASM binary |
| `download_url` | string | yes | Download URL. SHOULD resolve to a by-digest blob (immutable), not a mutable tag. |
| `shasum` | string | yes | SHA256 of the raw `.wasm` (hex). Equals the `application/wasm` layer blob digest when hosted via OCI. |
| `shasum_authored_by` | string | yes | `"registry"` — the registry fetched the bytes and computed this hash itself. A payload-supplied hash the registry did not verify against observed bytes is not accepted. |
| `signature` | object | yes (full deployment) | Keyless (Sigstore) signature over the artifact, identity-bound to the provider's CI workflow. |
| `signature.type` | string | yes (full deployment) | Signature scheme discriminator; `"sigstore-bundle"` is the only defined value. |
| `signature.certificate_identity` | string | yes (full deployment) | The signing certificate's Subject Alternative Name as the registry verified it at publish: the signing workflow's URI, including the ref it ran at. Descriptive only — the client reads the signer from the certificate and never takes its expectation from this field (§6, "Signer identity" below). |
| `signature.certificate_oidc_issuer` | string | yes (full deployment) | OIDC issuer that minted the signing certificate. Descriptive only, like `certificate_identity`. |
| `signature.bundle_url` | string | yes (full deployment) | Sigstore bundle (with **inline** Rekor inclusion proof + Signed Entry Timestamp) for **offline** verification. |
| `transparency_log` | object | optional | Rekor log coordinates; REQUIRED once a checksum transparency log ships. |
| `docs` | object | optional | The version's docs bundle ("Provider Documentation" below). Present only for a stable version published with one; absent otherwise. Its absence is never a per-provider downgrade signal; its presence is sticky per version (Provider Documentation, "Documentation provenance"). |
| `docs.download_url` | string | yes (in `docs`) | By-digest URL of the docs bundle archive the registry retained at publish. |
| `docs.shasum` | string | yes (in `docs`) | SHA256 of the archive bytes (hex), computed by the registry over the bytes it fetched. |
| `docs.signature` | object | yes (in `docs`, when `signature` is present) | Same shape as `signature`, over the archive's digest: `certificate_identity` and `certificate_oidc_issuer` are read from the docs certificate, and `bundle_url` is the absolute read-plane URL of `/v1/providers/{namespace}/{name}/{version}/docs.sigstore.json`. Its signer equals the artifact's. |

"yes (full deployment)" fields are REQUIRED in a full deployment and absent in a minimal deployment. Clients handle the transition via the downgrade-safe presence rule in §2 ("Field presence & downgrade protection"): a check turns on the first time the field appears and can never be silently turned off.

### Signer identity

A keyless signing certificate names who signed in two kinds of fields. Its Subject Alternative Name (SAN) — the value `signature.certificate_identity` reports — is the signing workflow's URI *including the ref the workflow ran at*, so a tag-triggered release workflow yields a different SAN for every release (`…/release.yml@refs/tags/v0.3.0`, then `…/release.yml@refs/tags/v0.3.1`). Pinning that string would make every release trip the pin. Alongside the SAN, the certificate carries the signer's components as separate Fulcio extensions, populated by each supported OIDC issuer (GitHub Actions is the first instance). Verification reads the following from the verified certificate:

- the **source**: the OIDC issuer (Fulcio extension `1.3.6.1.4.1.57264.1.8`), the source repository owner identifier (`…1.17`) and the source repository identifier (`…1.15`). The numeric identifiers make a repository or account deleted and recreated under the same name a different source;
- the **source ref**: the source repository ref (`…1.14`), the ref the signed build ran for;
- the **workflow**: the SAN with the suffix `@` + source ref removed when the SAN ends with exactly that suffix, otherwise the whole SAN. The definition is by suffix match, never by searching for an `@`, since refs and workflow paths may both contain one. For an ordinary workflow the suffix is the per-release part, so the workflow is stable across releases; a reusable workflow's SAN names the reusable workflow's own ref, which does not change per release and stays in the pinned value.

Components compare by byte equality. A certificate that lacks any of them fails verification; an absent component is never a wildcard.

**What is pinned.** Each `(host, namespace/name)` holds one **source pin** and, per channel, one **workflow pin**. A version is a **pre-release** when its semver carries a pre-release component (`0.0.0-main.619.1.ga307084`), and **stable** otherwise. The two channels have separate workflow pins because a provider commonly signs pre-releases from a branch CI workflow and releases from a tag-triggered one; a single shared workflow pin would make whichever channel was resolved first refuse the other. They share the source pin, so once either channel is pinned, the other channel's first contact (§2) can only pick a workflow inside the already-pinned repository — never another repository or owner. Together the source pin and a channel's workflow pin are that channel's **signer**.

**What is checked instead of pinned.** The source ref changes with every release and is never pinned. For a stable version it is checked against a rule this protocol fixes: the source ref MUST be exactly `refs/tags/v` followed by the version string as listed (§2), build metadata included — `1.2.3` requires `refs/tags/v1.2.3`, `1.2.3+build.5` requires `refs/tags/v1.2.3+build.5`. Because the rule derives from the requested version rather than from any earlier response, it holds from the first install with no first contact, and it is what stops a correctly signed stable artifact from being served under a different version. Pre-releases have no source-ref rule — their ref is a branch, and nothing in the version string names which one — so a pre-release is bounded only by the pinned workflow, at any ref, and serving one correctly signed pre-release under another pre-release's version stays possible. A publisher whose stable releases are not built from such tags cannot publish stable versions under this protocol; the registry refuses them at publish (§7) rather than emit versions every conforming client would refuse.

**Locks that predate this section.** Such a lock pins the full SAN string and issuer (a **legacy pin**) instead of a source and workflows. A client MUST NOT discard a legacy pin, and MUST NOT derive a source or workflow pin by parsing the string: the string carries no repository or owner identifier. While it is held, the legacy pin gates both channels: a signature is accepted only if its SAN and issuer equal the legacy pin exactly, as before. The first such signature verified converts it — that signature establishes the source pin and its own channel's workflow pin, and only then is the legacy pin dropped; from that point the other channel is held by the source pin like any other. The version the lock already pins was accepted under the legacy pin, and its signature is immutable (§9), so before evaluating any other version the client MUST convert from that pinned version's own signature whenever it can still obtain and verify it; a tag-specific legacy pin (`…@refs/tags/v0.3.0`) is converted that way rather than refusing every later release. Only when that signature cannot be obtained does the legacy pin go on gating both channels by exact SAN and issuer. Only a legacy pin that no reachable signature can match any more — the pinned version's bundle is gone, or the repository or workflow was renamed — needs identity re-pin (§2).

### Client verification (normative MUST)

This is the linchpin: every server-side control is decorative without it. Before loading any `.wasm`, the client MUST, **fail-closed and with no opt-out**:

1. Recompute the SHA256 of the fetched bytes and reject on mismatch against `shasum`.
2. Verify the `signature`: the source and workflow read from the verified certificate MUST equal the **expected** signer for the version's channel — or, while the lock still holds a legacy pin, the SAN and issuer MUST equal it ("Locks that predate this section" above) — and for a stable version the certificate's source ref MUST be `refs/tags/v{version}` ("Signer identity" above). Per §6, the expected signer MUST come from the lock pin or another registry-independent source (source repo / transparency log); an existing expectation MUST NOT be replaced from this response, and the response's `signature.certificate_identity` / `certificate_oidc_issuer` are never the source of either the expectation or the value compared. When no pin exists yet for the channel, §2's "First contact pins the identity" rule applies; the source-ref rule applies regardless. Verification uses the bundle's **inline** Rekor inclusion proof + SET **offline** against a pinned trust root; the client MUST NOT make a live Rekor/Fulcio call on the install path (so a Sigstore outage cannot block installs). A pin that rejects a legitimately rotated identity is recovered through §2's identity re-pin operation, never by deleting the lock entry.
3. Verify the `carina-providers.lock` pin (`shasum`, resolved hostname per §5, and the source pin and the workflow pin for the version's channel, or the legacy pin while one is held). A pin component that does not exist yet — first contact, or the window an identity re-pin opens (§2) — is established per §2's first-contact rules, not treated as a verification failure.
4. When `transparency_log` is present, verify the inclusion proof.

A client MUST NOT offer a flag to skip any of these.

### Error

| Status | Description |
|---|---|
| `404` | The specified version does not exist |

## 4. Resolve Provider Revision

<!-- derived-from #3-get-provider-version -->

Returns download information for a provider identified by a **git revision** (branch name, tag, or commit SHA) rather than a semver version. This is the analogue of carina's `revision = "<ref>"` keyword that already works for `source = "github.com/owner/repo"` direct resolution, exposed here so registry-hosted providers can be referenced the same way.

### Request

```
GET /v1/providers/{namespace}/{name}/revisions/{revision}/download
```

`{revision}` may be:

- a **branch name** (e.g. `main`)
- a **tag name** (e.g. `v0.5.0`)
- a **commit SHA** (full or short)

The registry currently serves branch and tag revisions. Commit-SHA resolution is a protocol extension and is not yet served by the registry.

### Response

Identical shape to §3 `Get Provider Version`, plus the `version` the revision resolved to; it carries the resolved version's §3 `docs` unchanged. The example shows a minimal deployment's content; a signed provider's response carries §3's `signature` as well:

```json
{
  "version": "0.5.0",
  "protocols": ["1"],
  "filename": "carina-provider-aws-v0.5.0.wasm",
  "download_url": "https://github.com/carina-rs/carina-provider-aws/releases/download/v0.5.0/carina-provider-aws-v0.5.0.wasm",
  "shasum": "5f9c7aa76b7c34d722fc9123208e26b22d60440cb47150dd04733b9b94f4541a"
}
```

Unlike the immutable §3 object for one version, a §4 object is mutable: publishing or yanking a candidate can move the same revision key to another version. The registry therefore serves it with a short cache lifetime (`Cache-Control: public, max-age=60`) rather than §3's immutable cache policy.

### Server-side resolution

The registry resolves `{revision}` to one of the entries already listed in §2 `versions`:

- **branch name**: the registry expects the provider to publish pre-release versions of the form `0.0.0-{branch}.<run_number>.<short_sha>` (or any other identifier sequence whose **first identifier after `{branch}` is numeric** so that semver 2.0 §11 precedence orders them chronologically). Call such a version a build of `{branch}`: its pre-release starts with exactly the `.`-separated identifiers of `{branch}`, followed by a numeric identifier. This is a shape only; one version can be a build of several names (`0.0.0-release.1.7.gabc` is a build of both `release` and `release.1`), and the rules below decide which branch it is attributed to. Each entry is attributed to at most one branch, and the registry returns the highest-precedence entry attributed to `{branch}`:
  - a **signed** entry belongs to the branch named by its certificate's source ref `refs/heads/{branch}`, and only if it is a build of that branch, so a signed build of branch `release.1` never answers `release`;
  - an **unsigned** entry has no certificate to name its branch, so the registry derives it from the version string alone: the identifiers before the first numeric identifier. An entry with no numeric identifier, or whose first identifier is numeric, belongs to no branch. An unsigned build of a branch whose name contains an all-numeric identifier is therefore attributed to a shorter branch, or to none: `0.0.0-release.1.7.gabc` counts toward `release` and can be the entry returned for `revisions/release/download`, while `0.0.0-release-1.2.7.gabc` counts toward `release-1`;
  - consequently a branch name containing an all-numeric identifier (`release.1`, `release-1.2`) resolves as a branch only through signed builds; with none, it has no branch candidate and returns `404` unless a tag rule below answers the same key.

  After excluding yanked candidates when any active candidate remains, the presence of any signed candidate for a key excludes every unsigned candidate for that key; a higher unsigned run number therefore cannot replace a branch already held by signed builds. Providers that do not follow this convention have no entries to match and the request returns `404`.
- **tag name**: when the tag is `v{X}` and `X` is a listed stable version, the registry returns `X` (signed or not). Otherwise it returns the highest-precedence signed version whose signing certificate's source ref (§3 "Signer identity") is `refs/tags/{tag}`.
- **commit SHA**: when the certificate carries the optional source repository digest (Fulcio extension `1.3.6.1.4.1.57264.1.13`, which other forges may omit), the registry can return the version whose digest begins with the SHA, preferring the highest semver precedence when several match.

While a revision has any non-yanked candidates, yanked entries are skipped (§9). If every candidate is yanked, the registry MAY keep serving the highest-precedence yanked target instead of deleting the object; the client rejects that response through the mandatory not-yanked check below. This permits an append-only write plane with no delete permission while ensuring a yanked version cannot be newly selected by a conforming client.

A revision key is one path segment. The registry emits no object for an empty name, `.` or `..`, a name containing `/`, or a name containing characters outside `[0-9A-Za-z._-]`. If a branch and a tag produce the same valid key, the tag rule wins among candidates remaining after the yank filtering above. The listed-stable `v{X}` rule in turn wins over any other signed tag candidate for that key.

The registry MAY support a subset of the above. The current implementation supports **branch and tag resolution**; SHA resolution remains an extension.

### Client verification

A revision names no version, while the channel and the stable source-ref rule (§3 "Signer identity") are both derived from one, so a §4 response is never verified on its own terms. The client MUST reject a response without `version`, check that the `version` appears in the provider's §2 listing under the Freshness rules, and run §3 "Client verification" for that version exactly as if it had been requested by version. Then it MUST check that the verified certificate actually answers the revision asked for. The client cannot tell from the string alone whether `{revision}` names a branch, a tag or a commit, so the response is accepted if it satisfies the check for any kind the string is eligible for:

- **branch `b`**: the version is a non-yanked `0.0.0-{b}.*` pre-release (the identifiers of `b`, then a numeric identifier, as in "Server-side resolution") and its certificate's source ref is `refs/heads/{b}`. The client cannot see other entries' certificates without fetching them, so which build of the branch is newest remains the registry's claim; what the check guarantees is that the artifact was built from that branch;
- **tag `t`**: the certificate's source ref is `refs/tags/{t}` and the version is not yanked; when `t` is `v` followed by a version the listing carries as stable, the version MUST be exactly that one, so a pre-release built at a release tag cannot stand in for the release;
- **commit SHA**: only a string of at least 7 hexadecimal characters is eligible; the certificate's source repository digest begins with it, compared case-insensitively, and the version is not yanked.

While `signature` is not yet recorded as present for the provider (§2 "Field presence & downgrade protection"), there is no certificate to read: the certificate parts above are skipped like every other signature check, the version-only parts still apply (for a branch, the highest non-yanked `0.0.0-{b}.*` pre-release, matching what the registry returns for unsigned entries; for a tag `v{X}` with `X` listed, version `X`; for every kind, not yanked), and a commit-SHA revision is refused, since nothing then binds the response to the commit. For a requested branch name with no all-numeric identifier, which covers every name the registry resolves from unsigned entries, this version-only branch rule and the registry's unsigned attribution match the same set of entries. For a name that has one, the client rule accepts builds the registry attributes to a shorter branch or to none. This is deliberate rather than a gap: with no certificate to read, the version string is all there is to check, and the accepted version is still a listed, non-yanked build of that shape.

The checks read the certificate, not the version string, because the version string alone binds nothing on this path: without them a registry could answer a branch request with an artifact built from another branch or tag, or a tag request with another release's artifact, whenever both are signed by the same pinned workflow.

### Error

| Status | Description |
|---|---|
| `404` | No version matches the requested revision under this provider |

## 5. Registry Resolution & Pinning

<!-- constrained-by #provider-address-format -->

A provider address is `[hostname/]namespace/name`. Once more than one registry can serve providers (the default `registry.carina-rs.dev`, a third-party public registry, an enterprise private registry, or a mirror), how the client picks **which** registry resolves a given address is a security boundary, not a convenience. Getting it wrong is the dependency-confusion class (the pip `--extra-index-url` / 2021 namespace-substitution disaster). The following rules are normative.

1. **Hostname is part of identity.** `registryA/ns/name` and `registryB/ns/name` are **distinct providers** and MUST NEVER be substituted for one another. Resolution is single-host: the address deterministically names one registry.
2. **No cross-registry version merge.** When multiple registries are configured, the client MUST NOT union their listings, and MUST NOT pick the highest semver across them. The first registry that answers **authoritatively** (a non-404/410 response) for the address wins. (Go's ordered try-list, not pip's flat union.)
3. **No trust-boundary fall-through.** A request pinned to a private or specific host MUST NOT fall through to a different authority (e.g. the public default) on failure. Following Go's comma semantics, the client advances to the next configured source **only on a definite "not found" (404/410)** and MUST stop on any other error (5xx, timeout, DNS). This is both a privacy control (a private name never leaks to a public registry merely because the private one is down) and a fail-closed control. Same-authority **replica failover** (a different mirror of the *same* pinned authority) is permitted and is distinct from trust-boundary fall-through.
4. **Private patterns.** The client MAY declare scope→registry mappings and private-name patterns (e.g. `acme/*` resolves only against `acme.internal`). A namespace covered by a private pattern MUST NOT be queried against the public default. Where multiple mapping methods can match one address, scopes MUST be configured non-overlapping (exclude-wins), to avoid the multi-method "pick newest across all matches" footgun.
5. **No silent default fallback for an unmapped private intent.** A bare `namespace/name` (hostname omitted) resolves to the default registry. A project MUST be able to declare "no default registry" so a typo or an unmapped private name fails closed instead of being shadowed by whatever the default registry happens to serve.

### Pinning

- The client MUST record, per resolved provider, the **resolved hostname** (not just `namespace/name`) in `carina-providers.lock`, so re-resolution is reproducible and a shadow cannot silently swap the serving host.
- The lock entry SHOULD also pin the `shasum` (and, once available, the source pin and per-channel workflow pins of §3 "Signer identity" and the §2 `sequence`) so that a later fetch from any source is verified against the originally-resolved values.
- Per registry **host**, the §1 discovery pin, when held, is one host-level record in the lock — the resolved values §1's pin covers, today the resolved API base — that the host's provider entries share, never a copy in each entry (§2 "Discovery re-pin", "One pin, not per-entry copies").

## 6. Federated Trust Model

<!-- constrained-by #5-registry-resolution--pinning -->

In a single-registry world the registry is the trust authority. Once a second registry can be trusted, that no longer holds: a registry-authored `shasum` (§3) only attests "these are the bytes *this* registry named", which is exactly what a hostile registry controls. Federation therefore demotes **every** registry — including `registry.carina-rs.dev` — to an **untrusted, cacheable, mirrorable distribution layer**, and moves the trust root to the client. The following are normative.

1. **Trust is a client decision, never implicit from the address.** The client MUST explicitly declare which hostnames it trusts and the expected signing-identity policy for each. Encountering an address for an unconfigured hostname is not, by itself, a reason to trust it.
2. **Expected identity comes from a registry-independent source.** When verifying a provider's signature (once signing lands), the expected signer (§3 "Signer identity") MUST be obtained from a source independent of the serving registry — the provider's source repository, the `carina-providers.lock`, or a transparency log — and an existing expectation MUST NOT be overridden or replaced from the serving registry's §3 download response (including its signature fields). At signature first contact for a channel — no workflow pinned for the version's channel and no independent source — §2's "First contact pins the identity" rule applies, and the signature must still match the source pin when one exists; from the moment the channel's signer is pinned, this rule holds absolutely. This applies to the **default registry too**, not only to third-party ones.
3. **Mirror integrity is anchored upstream.** A mirror MAY re-serve another registry's content and rewrite `download_url` to its own by-digest store (the `shasum` is the `application/wasm` layer-blob digest, so the same blob verifies identically regardless of CDN). But the **expected** `shasum` and signer MUST come from the upstream/publisher signature, never from the mirror's own metadata.
4. **Per-registry audience.** Publish-time OIDC verification (§7) MUST bind a **per-registry audience**; a single fixed audience is replayable across federated registries.

Residual: namespace ownership and tombstoning are **per-registry-local** (the hostname disambiguates, so there is no global namespace-collision and no global tombstone authority). The unresolved hazard moves up to the **hostname layer** (DNS/homoglyph typosquatting of registry hostnames), which only the client-side explicit-trust rule (1) defends.

## 7. Publish API

<!-- constrained-by #6-federated-trust-model -->

Publishing a version is authenticated with **keyless OIDC trusted publishing** — there is no long-lived publish credential anywhere to phish or exfiltrate. The CI that builds the provider presents a short-lived OIDC token; the registry verifies it against a pre-registered trusted-publisher policy for `(namespace, name)` (the issuer, owner id, signing requirement and allowed download hosts are per namespace; the repository and trusted publishers are per provider) and mints an internal, short-lived, scoped grant.

This is **issuer-agnostic.** GitHub Actions (`token.actions.githubusercontent.com`) is the first supported issuer; the trust policy is a per-namespace `{allowed-issuer, claim-mapping}`, so adding GitLab CI / a self-hosted IdP / a private Fulcio is configuration, not a redesign.

### Request

```
POST /v1/publish
Authorization: Bearer <CI OIDC JWT>   (aud = the target registry's hostname-specific value)

{ "namespace": "...", "name": "...", "version": "...", "candidate_download_url": "..." }
```

The body MAY additionally declare `protocols` (supported Provider Protocol
versions; defaults to `["1"]`) and `filename` (defaults to the last path
segment of `candidate_download_url`). Both become immutable metadata of the
published version (§3) and are part of the same-content comparison for
idempotent retries.

The body MAY also include `sigstore_bundle`: the Sigstore bundle (JSON, with
inline Rekor inclusion proof + SET) produced by keyless signing of the
artifact in the publishing workflow. Whether it is required is part of the
per-namespace trusted-publisher policy, as an explicit two-state
(signing-required vs unsigned-allowed) with no implicit default — a policy
declaring neither fails closed. Under either state, a bundle that is present
is verified **offline** against the registry's pinned trust root before the
§3 `signature` block is emitted: the certificate must chain to the trust
root with a valid SCT, carry the identity derived from the verified OIDC
claims (the publishing workflow's URI + the policy's issuer), the
workflow commit and the source ref the publish token attests (so the
stable source-ref rule checked on the token's `ref` also holds for the
certificate clients will check, §3 "Signer identity"), sign the registry-computed
digest, be within its validity period at publish time, and include the
offline-verification material (Rekor inclusion proof + SET). An invalid
bundle rejects the publish. The registry cryptographically verifies the
inclusion proof and SET at publish time, using the same implementation the
client runs; the §3 client-side verification remains the normative backstop. The
signature of a published version is immutable: a retried publish never
replaces the recorded signature (see "Recorded signature pair" below for
how a retry is compared with it).

The body MAY also include `docs`, declaring the version's docs bundle
("Provider Documentation" below):

```json
"docs": { "candidate_download_url": "...", "sigstore_bundle": { ... } }
```

`docs.candidate_download_url` is required inside `docs` and is fetched like
`candidate_download_url`, with the same allow-list and the docs bundle's
16 MiB compressed cap ("Provider Documentation" below). `docs.sigstore_bundle` is the bundle from signing the archive's
digest in the same workflow run. It is present exactly when
`sigstore_bundle` is present: a publish carrying one without the other is
refused, whatever the policy says. It is verified like the artifact's,
against the archive's registry-computed digest. In addition, when `docs`
is present, both certificates MUST carry each of the following components
and the docs certificate MUST match the artifact's in every one, by byte
equality: the SAN, the OIDC issuer, the source (repository owner
identifier and repository identifier, §3 "Signer identity"), the source
ref, the build signer digest (Fulcio extension `1.3.6.1.4.1.57264.1.10`,
the workflow commit checked above), the source repository digest (`…1.13`,
the commit that was built) and the run invocation URI (`…1.21`), which
names the workflow run and its attempt. A certificate lacking any of them
fails, so an artifact certificate without a run invocation URI cannot be
published with docs. Because the attempt is part of the run invocation
URI, the provider signs both files in the same job; re-running that job
re-signs both. `docs` is refused on a pre-release version.
An invalid, oversized, unverifiable or wrongly signed docs bundle rejects
the whole publish: the version is not published without its docs, since a
version's docs cannot be attached later (§9). The archive's digest, or its
absence, is part of the same-content comparison for idempotent retries: a
retry declaring different docs, docs where the committed version has none,
or no docs where the committed version has them, is a conflict.

#### Recorded signature pair

The registry records a signed version's artifact bundle, and its docs
bundle when the publish carries one, in a single conditional write, before
writing either bundle to its path. This record is the version's recorded
signature pair: a version has its whole pair recorded or nothing, and the
first pair recorded is the one the version is published with. The bundles
written to their paths and referenced from §3 are always the recorded
pair's bytes.

A retry is compared with the recorded pair, not recorded over it. It
completes the publish from the recorded pair when all of these hold: its
artifact digest equals the recorded one; it carries docs only if the pair
does, with the same archive digest; and its certificates match the recorded
ones in every component listed above except the run invocation URI (the
SAN, OIDC issuer, source repository and owner identifiers, source ref,
build signer digest and source repository digest). Re-running the release
job therefore completes the publish even though it re-signs, whichever
attempt produced the recorded pair. Recorded bytes that differ from the
retried bundle are verified again, under the conditions that held when they
were recorded. The registry then writes whichever recorded bundle is still
missing from its path, so a publish interrupted at any point before the §3
object is written can be completed, docs included; once the §3 object is
written, a retry must also match it, as before. The retried bundles
themselves must still pass every check of a first publish, including the
docs bundle matching the retried artifact bundle in the components above.

A retry is refused as a conflict when its digests or those signer
components differ from the recorded pair, when it carries docs and the
pair has none, or when it is unsigned and a pair is recorded. A retry that
omits docs while the pair has them completes without docs only until the
§3 object is written; the recorded docs bundle is then neither written to
its path nor referenced from §3, and readers never consult it.

A version whose artifact bundle was written to its path before signature
pairs were recorded has no recorded pair. Such a version can be completed
with docs only by a docs bundle from the same run attempt as its recorded
artifact bundle; otherwise only without docs.

### Registry obligations (normative)

1. Verify the JWT signature against the issuer's JWKS, and verify its claims against the pre-registered trusted-publisher policy for `(namespace, name)`:
   - stable numeric **owner id** (never a mutable login/name)
   - `repository` + a specific workflow (`job_workflow_ref`)
   - `ref`: for a stable version, exactly `refs/tags/v{version}` of the version being published — the source-ref rule clients enforce (§3 "Signer identity"), checked here so the registry never emits a stable version every conforming client refuses; `refs/heads/main` for the `0.0.0-{branch}.*` dogfooding path
   - the **per-registry** `aud`
   - **reject** any token minted from a `pull_request` / `pull_request_target` context
2. Fetch `candidate_download_url`, compute the SHA256 over the observed bytes (authoritative `shasum`, §3); reject any payload-supplied hash. The fetch MUST be SSRF-guarded (host/scheme allow-list; deny IMDS/link-local/internal ranges; bounded redirects; size/time/decompression caps).
3. Verify the artifact signature (§3) before emitting. When `docs` is present, fetch the docs bundle the same SSRF-guarded way, compute its SHA256, validate it against "Documentation source: the docs bundle" below, and verify its signature (above) before emitting anything for the version. Retain the archive bytes in the registry's by-digest store and emit that copy's URL as §3 `docs.download_url`, never the publisher's origin URL; record the artifact and docs bundles as the version's signature pair, then write the pair's docs bundle, when present, at the path §3 `docs.signature.bundle_url` names ("Recorded signature pair" above). Render every docs page in memory before the first write, so a rendering failure rejects the publish like any other docs failure. Write the archive and `docs.sigstore.json` before the §3 object that points at them, and the rendered pages, docs version list and provider directory after it; a same-content retry re-emits the pages, the version list and the provider directory, so a publish interrupted after the §3 write is completed by retrying it.
4. Reject if `(namespace, name, version)` already exists (immutability, §9).
5. Mint a ≤15-minute scoped grant; store no long-lived credential.

The trusted-publisher policy store is a high-value target: it SHOULD be append-only/audited, mutated under two-person review, with the grant-minting key in an HSM/KMS, because whoever can change it can authorize any workflow to publish under a namespace.

```
POST /v1/yank   { "namespace": "...", "name": "...", "version": "..." }
```

Authorized exactly like publish: the same OIDC trusted-publisher check and the same owner-id binding (§8). Withdrawing a version is a write against `(namespace, name)`, so it demands the credential that could have published it — no weaker path.

The registry MUST reject a yank for a `(namespace, name, version)` it has never published, rather than recording the flag for a version that does not exist. Yanking an already-yanked version is a no-op and MUST succeed, so a retried request does not fail; because the flag is one-way (§9), that is the only repeat case.

See §9 for what a yank means and what it does not.

## 8. Namespace Ownership

<!-- constrained-by #7-publish-api -->

- `(namespace, name)` is bound to the **stable numeric owner id** of the issuer's account at first publish (forge-agnostic; GitHub `databaseId` is the first instance). Never bind to a login string or other mutable name.
- The initial `(namespace, name) → trusted-publisher` mapping is established by a **reviewed bootstrap**, never auto-created on first contact (which would let an attacker win the first-publish race).
- The owner id is **re-verified on every ingest and every resolution** (incl. §4). A mismatch **freezes** the provider (serve an advisory; never auto-follow a rename).
- `(namespace, name)` is **permanently tombstoned** to its original owner id; a reclaimed account name cannot inherit it (repo-jacking defense).
- A legitimate transfer/rename requires explicit re-verification of the new owner before ingest resumes.
- Ownership is **per-registry-local** (§6): a third-party registry governs its own namespaces; there is no global authority.

## 9. Immutability & Withdrawal

<!-- constrained-by #2-list-provider-versions -->

- A published `(namespace, name, version)` is **immutable**: no re-publish over it, no request-time shasum mutation, no individual-version deletion. This is what makes every §3 response permanently cacheable and every `carina-providers.lock` pin reproducible.
- **No whole-provider withdrawal**: this protocol offers no operation that removes a published provider. Withdrawal from *new* resolutions is the per-version yank below; nothing here removes listing entries or bytes ("Why whole-provider removal is not offered" below).

### Yank

A yanked version is withheld from **new** resolutions while existing lock pins keep resolving and the bytes stay downloadable. That is what lets a bad version be withdrawn without breaking reproducibility for anyone who already depends on it.

**What a registry MUST do.**

- Keep the version in the §2 listing with `yanked: true`. Removing the entry is **not** a yank — it breaks lock-pinned resolutions, because a client re-checks that its pinned version is still listed. Withholding and deleting are different operations, and only the first is offered here.
- Keep serving the §3 `download` response unchanged, including `shasum` and `signature`. The version was published; that fact does not become false. Bytes stay fetchable so a pinned build keeps working.
- Bump `sequence`. A yank changes the listing, so it is subject to the same freshness machinery as any other change (§2).
- Re-emit the provider's unversioned docs top page, its docs version list and the provider directory when the yank changes them (Provider Documentation). The yanked version's own docs pages stay.

**What a client MUST do.**

- Exclude yanked versions when resolving a version constraint, a `revision` match, or an unconstrained "newest" — every path that *chooses* a version.
- Keep resolving an existing lock pin even when the pinned version is yanked, and warn. Failing here would convert a withdrawal into a broken build for exactly the people the yank was meant not to disturb.
- Refuse to *newly* pin a yanked version. If a resolution has no non-yanked candidate satisfying the constraint, that is an error naming the yanked versions, not a silent fallback to one of them.

**No un-yank.** This protocol offers no operation to clear the flag. The reason is in §2's presence rule: clients treat `yanked: true` as sticky per version, so a registry that stopped sending the flag would be indistinguishable from an attacker stripping it to push a withdrawn version back into new resolutions. Making un-yank work would mean giving up that defense. The recovery path for a mistaken yank is to publish a new version — which is also the honest one, since a version that was announced as withdrawn cannot be un-announced from the caches and locks that already recorded it.

**Yank is not revocation.** It stops new adopters; it does nothing about installs already pinned to the version, which keep resolving by design. For an actively exploited artifact the response is out-of-band advisory plus a fixed release, not this mechanism.

### Why whole-provider removal is not offered

A bounded withdrawal — whole-provider removal within a tight post-publish window, and only with no dependents — is the natural-looking exception to immutability; npm's 72-hour rule is the same idea. This protocol deliberately does not offer it: neither half of that bound can be kept.

- **"No dependents" is a condition the registry cannot evaluate.** The read plane is unauthenticated, CDN-cached GETs (§2, §3); nothing reports who resolved or pinned a provider, and no reverse-dependency view exists to consult. The condition was meant to prevent removing a provider people already rely on (the left-pad failure), which is exactly the case it cannot detect.
- **The write plane is deliberately unable to delete.** The storage defense behind per-version immutability — the storage cannot overwrite or delete a published version — is that the publish path holds no delete or retention-bypass permission at all, so a buggy or compromised writer cannot destroy a pinned resolution. A withdrawal operation would hand back to the write plane exactly the capability whose removal closed that risk. A post-publish window in which withdrawal is allowed is a window in which that defense is off — open precisely when a freshly compromised publisher credential is most useful.

Nor would keeping the promise remedy the cases it was written for. Deletion does not recall bytes that were already downloaded and cached; for a credential baked into a published binary the remedy is rotating the credential, and for a malicious or mistaken release it is yank plus a fixed version. The only remedy deletion adds over yank is that registry-hosted bytes stop being fetchable at all (bytes a version serves from the publisher's origin were never the registry's to remove). Even that is felt only by parties fetching outside pinned resolutions, since a conforming client never chooses a yanked version. It is no help against a secret, which must be assumed harvested the moment it was published. Pinned clients experience deletion purely as breakage. The one harm yank cannot undo, occupation of the name, deletion cannot undo either: `(namespace, name)` stays tombstoned to its first owner (§8) whether the versions exist or not.

**Out-of-band removal (non-normative).** A registry operator may nonetheless be compelled to remove published objects — a takedown the operator cannot refuse, content that cannot remain hosted. That is an administrative action outside this protocol, not an operation of it: no API performs it, publish credentials cannot trigger it, and this document does not specify its procedure. The client-side defenses make no exception for it either — to a client, a provider whose listing or download objects disappeared is indistinguishable from an attack or data loss, and it fails closed (§2, §3). That consequence is accepted, not accidental: every pinned resolution of the removed provider breaks, and an operator who removes content does so knowing this protocol will not soften it.

## API Call Flow

A client resolves and downloads a provider with three registry calls, then fetches the binary directly from the release host. Solid arrows are requests; dashed arrows are responses.

<img src="/images/registry-api-call-flow.svg" alt="Sequence diagram: the client requests /.well-known/carina.json, the version list and the download endpoint from the registry, then downloads the WASM binary from download_url on GitHub Releases" width="780" height="488" style="width:100%;height:auto">

## Provider Metadata (Registry-side)

Metadata that the registry holds for each provider. In the initial implementation, this is managed as JSON files.

```plaintext
providers/
  carina-rs/
    aws.json
    awscc.json
```

**Example `aws.json`:**

```json
{
  "namespace": "carina-rs",
  "name": "aws",
  "source": "github.com/carina-rs/carina-provider-aws",
  "versions": [
    {
      "version": "0.3.0",
      "protocols": ["1"],
      "shasum": "5f9c7aa76b7c34d722fc9123208e26b22d60440cb47150dd04733b9b94f4541a"
    },
    {
      "version": "0.2.0",
      "protocols": ["1"],
      "shasum": "abc123..."
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `namespace` | string | Provider namespace (typically the GitHub org name) |
| `name` | string | Short name of the provider |
| `source` | string | Source URL of the GitHub repository |
| `versions` | array | Array of version entries |
| `versions[].version` | string | Semantic version |
| `versions[].protocols` | string[] | Provider Protocol versions |
| `versions[].shasum` | string | SHA256 of the WASM binary |

## Carina Configuration Integration

Provider references in `.crn` files after the registry is introduced:

```hcl
# Via the registry (new form)
provider aws {
  source  = "carina-rs/aws"
  version = "~0.3.0"
}

# Direct GitHub (existing form, still supported)
provider aws {
  source  = "github.com/carina-rs/carina-provider-aws"
  version = "~0.3.0"
}

# Custom registry
provider custom {
  source  = "registry.example.com/myorg/custom"
  version = "^1.0.0"
}
```

## Provider Documentation

The registry also hosts documentation for each provider through a web UI.

### URL Structure

```
https://registry.carina-rs.dev/providers/
https://registry.carina-rs.dev/providers/{namespace}/{name}
https://registry.carina-rs.dev/providers/{namespace}/{name}/latest
https://registry.carina-rs.dev/providers/{namespace}/{name}/{version}
```

Within a provider, the top page is the only unversioned docs page, reachable both without a version and as `latest`; there is no unversioned per-resource URL, so nothing is served under `latest/`. `latest` is the same page as the unversioned URL, written from the same choice of version, so the two never point at different versions. When the version is omitted, the page redirects, by an HTML `<meta http-equiv="refresh">` with a plain link as fallback, to the versioned top page of the newest stable version that has docs and is not yanked (§9), and names that version, since it need not be the newest published version. It carries no copy of that version's docs, so re-rendering a version never requires re-rendering it.

When every stable version that has docs is yanked, the page redirects to the newest of them and marks it yanked. When no stable version of the provider has docs, the unversioned URL returns 404; the page is first written when the first version with docs is published, and is never deleted afterwards (§9). `latest` follows the same rules.

The provider directory at `/providers/` lists every provider that has at least one stable version with docs, sorted by namespace and then name. Each entry links to that provider's unversioned top page and names the version that page redirects to, marked yanked when the top page marks it. It is derived from the published versions alone and is written at every registry start and whenever the set of providers with docs or a redirect target may have changed, so it exists, listing no provider, even while none has docs. A provider whose versions cannot be read while the directory is built is omitted from that rendering and reappears on the next write.

The docs archive, its `docs.sigstore.json` and the §3 object carrying `docs` are written once, like the rest of the version, and are the immutable record of a version's documentation. The pages, versioned and unversioned, the docs version list and the provider directory are mutable objects derived from it: they are written at publish, and the registry re-renders them from its own retained copy of each archive whenever its sanitizer, renderer or page template changes output, verifying the copy against `docs.shasum` first. A re-render overwrites keys that are already served with a short `max-age`, so it relies on that expiry instead of CDN invalidation, as the periodic listing re-issue does. Publish and yank of a version with docs rewrite only the unversioned top page, the docs version list and the provider directory (stored as `providers/index.html` and served at `/providers/`); versioned pages do not embed that list, so they are unaffected. A publish invalidates those three keys plus the page keys it created, so a cached 404 for a new page does not outlive the publish; since every page under a version's prefix comes from that version's one bundle, the registry MAY invalidate them as a single wildcard path covering that prefix. Because pages can be re-rendered and CDN invalidation cannot reach browser caches, pages, the docs version list and the provider directory are written as ordinary overwritable objects, never with the write-once or object-lock path, and MUST NOT be served with `immutable` or a long browser `max-age`; only the archive and `docs.sigstore.json` are cached as immutable.

The docs version list is served at `/providers/{namespace}/{name}/versions.json` (`application/json`) and lists every stable version that has docs, newest first:

```json
{ "versions": [ { "version": "0.3.0", "yanked": false } ] }
```

Pages are stored and served as `text/html; charset=utf-8` although their paths have no extension, like the extensionless JSON endpoints, and SHOULD be served with a `Content-Security-Policy` that admits no inline script. The retained archive is served as `application/gzip`.

**Examples:**
- `https://registry.carina-rs.dev/providers/carina-rs/aws` — top page of the newest non-yanked stable version that has docs
- `https://registry.carina-rs.dev/providers/carina-rs/aws/0.3.0` — top page of the v0.3.0 docs
- `https://registry.carina-rs.dev/providers/carina-rs/aws/0.3.0/resources/s3_bucket` — individual resource doc for v0.3.0
- `https://registry.carina-rs.dev/providers/carina-rs/aws/0.3.0/data-sources/ami` — individual data source doc for v0.3.0

Every versioned page carries a short server-rendered navigation (the version's top page, and the current page), and the version's top page lists every page of the version, so the docs stay navigable without scripts. Grouping pages by service is the serving registry's presentation, not part of the docs bundle. A registry MAY hold, per provider, navigation metadata that assigns resource and data-source slugs to services with display titles; it is the registry's own data, unsigned like the rendered pages, and it applies to every version of the provider, including versions published before it existed. A version is shown grouped only when every one of its resource and data-source pages is assigned to exactly one service, and no service has two of this version's resources, or two of its data sources, whose page titles share a type name (the title's last `.`-separated segment); each service then lists only this version's pages, and services with none are omitted. Otherwise, including when the provider has no metadata, navigation stays flat under Resources and Data Sources. Flat navigation's `nav.json` has `overview`, `resources` and `data_sources` fields. Grouped navigation's `nav.json` instead has `overview` and a `services` array whose entries have `id`, `title`, `resources` and `data_sources`; it has no top-level `resources` or `data_sources`. In both shapes, every resource and data-source entry has `slug`, `title` (the page title, in full) and `href`, and entries are sorted by slug. The sidebar labels each page from its title: under a service heading, by its type name alone (`aws.ec2.Vpc` shows as `Vpc` under EC2, since the heading already names the service), and in flat navigation, by the full title. The type-name rule above is what keeps those short labels distinct within each of a service's Resources and Data Sources sections. A page's HTML `<title>` begins with the full title. The sidebar renders each service as a collapsible heading with its non-empty Resources and Data Sources sections: the server-rendered top page includes every service closed, an individual page includes only its own service, open, listing only that page, and the full navigation installed by the same-origin script opens the current page's service and leaves the others closed. Like the pages, both `nav.json` shapes are derived (from the version's bundle and the provider's navigation metadata) and rewritten when the version is re-rendered, so changing the metadata re-renders every version.

The web UI provides a version-switcher dropdown so users can browse docs at any version. Every versioned page builds it in the browser from the docs version list, with a same-origin script, so the list is never embedded in a versioned page; the switcher marks yanked versions, including the one being viewed, and choosing a version opens that version's top page, since a resource need not exist in every version.

### Documentation source: the docs bundle

<!-- constrained-by #7-publish-api -->
<!-- constrained-by #3-get-provider-version -->

A provider ships its documentation as a **docs bundle**: one archive file, built by the same release workflow run that builds the `.wasm` and published at an origin URL the registry can fetch by unauthenticated GET (normally a release asset next to the `.wasm`). The registry never reads a repository tree, so how a provider repository lays out or generates its docs is the provider's own business, and unsigned docs work on any forge the §7 policy admits; signed docs need a forge whose signing certificates carry the components §7 compares.

```
carina-provider-aws-v0.3.0-docs.tar.gz
  index.md                 # required: provider overview and configuration
  resources/
    s3_bucket.md           # one page per resource
    ec2_instance.md
  data-sources/
    ami.md                 # one page per data source
```

A docs bundle is valid only if all of the following hold. The registry checks every rule at publish (§7) and refuses an invalid bundle rather than repairing it.

- It is exactly one gzip member holding a POSIX tar archive (`ustar` or `pax`), and nothing but zero bytes follows the tar end-of-archive marker.
- Every entry is a regular file or a directory. Symbolic links, hard links, device files, FIFOs and GNU long-name or long-link entries (`././@LongLink`) are invalid; a provider whose paths exceed the `ustar` limit writes `pax` (`tar --format=pax`). pax global headers (`g`) are invalid. pax extended headers (`x`) are metadata, not entries, and do not count toward the entry limit; each is at most 64 KiB; every rule here, including path normalization and the size limits, applies to an entry's effective `path` and `size`, which are the pax values when given.
- Every entry path is relative and uses `/` as the separator. Before any other rule is applied, one leading `./` is removed, and a directory entry's one trailing `/` is removed; a directory entry that is then empty (the archive root, as `tar -C dir .` writes it) is ignored, and must declare size 0 like every directory entry. A non-directory entry that is then empty is invalid. What remains MUST contain no empty, `.` or `..` component. Paths are compared byte for byte after this normalization.
- `index.md` is present at the archive root.
- The only directories are `resources` and `data-sources`. Every regular file is `index.md`, `resources/{slug}.md` or `data-sources/{slug}.md`, where `{slug}` matches `[a-z0-9][a-z0-9_]*` and is at most 128 bytes. No other file may appear, and no path may appear twice.
- Every `.md` documentation file is UTF-8 Markdown (GFM), and its GFM parse tree is at most 64 levels deep, counting the document itself as level 0 and each nested block or inline element, such as a blockquote, list item or emphasis, as one level more than its parent. Rendering cost grows with the square of nesting depth, so deeper nesting is refused rather than rendered.
- In every `.md` documentation file, one more than the largest number of `|` characters on any single line, multiplied by the file's number of lines (counting LF, CR and CRLF each as one line ending, as Markdown does, and a final line without a line ending as a line; an empty file has no lines), is at most 250,000. This bounds the table cells a renderer fills in for short rows, which can otherwise make a few kilobytes of Markdown cost gigabytes to render; ordinary tables are far below it.
- Every `resources/{slug}.md` and `data-sources/{slug}.md` file begins with its **page title** as an ATX level-1 heading on the very first line: the bytes `# ` followed by the title, ended by a line ending (LF, CRLF or CR) or by the end of the file. The title is the page's DSL name, for example `aws.acm.Certificate`. It matches `[A-Za-z0-9_]+(\.[A-Za-z0-9_]+)+` (at least two segments separated by `.`) and is at most 128 bytes. Nothing may precede the heading, not even a blank line or a byte-order mark. `index.md` has no title requirement. This rule was added after the first providers published docs; every bundle carina-rs/carina-provider-aws and carina-rs/carina-provider-awscc have published already satisfies it, but for any other provider it is a new requirement: a bundle without a valid first-line title is refused.
- The compressed archive is at most 16 MiB, it holds at most 4096 entries, each file is at most 256 KiB, the files total at most 64 MiB, and the whole decompressed tar stream, headers and padding included, is at most 80 MiB.

Since an idempotent retry compares the archive's digest (§7), a provider SHOULD build the archive reproducibly (entries sorted by name, fixed mtime, owner and group 0, `gzip -n`); otherwise a re-run that re-packs the docs is refused as a conflict.

`{slug}` is the page's name in the URL (`resources/s3_bucket.md` → `/providers/carina-rs/aws/0.3.0/resources/s3_bucket`). The slug is chosen by the provider; the registry does not derive it from resource type names. Conversely, every name the registry shows for a page (sidebar labels, `nav.json` titles, the HTML `<title>`) comes from the page title, never from the slug: the registry does not guess a DSL name from a file name.

Docs are optional. A version published without a docs bundle is served exactly like any other version (§2–§4); it simply has no documentation pages. Docs are accepted only for stable versions (§3 "Signer identity"): a pre-release publish carrying a docs bundle is refused, which keeps per-commit pre-releases from accumulating rendered pages. The web UI's version switcher therefore lists only stable versions that have docs, marking yanked ones; yanked versions keep their pages, since docs URLs are never removed.

A version's docs are fixed at its first publish, like the rest of the version (§9). They cannot be added to, replaced in, or removed from an already-published version; corrected docs ship with the next version.

### Rendering & sanitization

Documentation is Markdown authored by the provider's owner — including third-party owners — so it is **untrusted input** for rendering purposes (the binary is sandboxed, but the docs are HTML shown to a human). The registry MUST sanitize Markdown→HTML conversion with an allow-list sanitizer. Markdown is rendered as GFM with tables, strikethrough and autolinks; footnotes and task lists are not enabled. Raw HTML in the Markdown is omitted rather than rendered: an HTML block is dropped whole, and an inline tag is dropped while the text around it stays as text; text inside code spans and code blocks is escaped as usual, so `List<String>` in a code span shows literally. The renderer's output then passes an allow-list sanitizer as a second line of defense, which drops `script`, `style`, `iframe`, `object`, `embed`, `template`, `textarea`, `svg` and `math` elements together with their content, and unwraps every other element not listed here, keeping its text. Everything not listed here is removed, so `<script>`, `<iframe>`, `<base>`, `<meta>`, `<svg>`, `on*` event handlers and `style` attributes never survive.

- Elements: `h1`–`h6`, `p`, `br`, `hr`, `blockquote`, `ul`, `ol`, `li`, `pre`, `code`, `em`, `strong`, `del`, `a`, `table`, `thead`, `tbody`, `tr`, `th`, `td`, and `span` for the registry's own syntax highlighting of code blocks (see below). Images are not rendered; documentation pages are Markdown only.
- Attributes: `href` on `a`; `id` on headings, which the renderer sets to `user-content-` followed by the heading's slug (author-written `id`s are dropped); `align` on `th` and `td`; `class` on `code` only when it is `language-` followed by `[a-z0-9+-]+`; `class` on `span` only when every class is `hl-` followed by `[a-z0-9-]+`.
- `href` values: an absolute URL whose scheme is `http` or `https` with a host (`//` follows the scheme), or `mailto`; a fragment-only link (`#arguments`); or a relative link without a query that, percent-decoded once and resolved against the linking file's path inside the bundle (RFC 3986 §5.2), names a `.md` file of the same bundle; a link whose `..` segments would climb above the archive root is removed rather than clamped. That link is rewritten to the named page's versioned URL as a host-relative path. The fragment of a fragment-only link or of a rewritten bundle link is rewritten to `#user-content-` followed by the fragment, so it reaches the heading the author named; an absolute URL's fragment is kept unchanged. Any other `href`, including a relative link to a file the bundle does not hold, is removed, keeping the link text.

Code blocks whose language the registry recognizes are highlighted at render time, inside the same isolated renderer, by wrapping tokens in `span` elements with `hl-` classes; the colors come from a stylesheet the registry serves, never from `style` attributes, and the code text stays escaped. Since raw HTML is omitted, an author cannot supply such spans.

A heading's slug is its text content, lowercased, with every character other than a Unicode letter, a Unicode digit, a space, `-` or `_` removed and every space replaced by `-`. When a page has the same slug more than once, the second gets `-1` appended, the third `-2`, and so on, in document order, skipping any suffixed slug already used on the page, so no two headings share an id: each heading takes the first unused slug among its own slug, then that slug with `-1`, `-2`, and so on, and every slug a heading takes counts as used. Docs MUST be rendered to static HTML at ingest time (consistent with the static read plane), never at request time. Each page's Markdown renders to at most 2 MiB of HTML before sanitization, and the finished pages and navigation document of one bundle, page template included, total at most 64 MiB; the navigation that lists every page of a version is rendered once per version, at `/providers/{namespace}/{name}/{version}/nav.json` (`application/json`), not into every page, so that total grows linearly with the page count; a bundle whose rendering exceeds either limit is refused at publish like an invalid bundle, since a few kilobytes of Markdown, such as a wide table, can expand to many megabytes when rendered. These two limits are measured on the registry's own renderer output. Because the renderer is third-party code fed untrusted input, the registry also renders each bundle in an isolated process with its own memory, CPU-time and wall-clock limits, and refuses a bundle whose rendering exceeds them or ends abnormally, like an invalid bundle. All of these limits apply at every publish, including an idempotent retry. A re-render of an already-published version runs under the same isolated-process limits; one that fails or exceeds them leaves that version's existing pages in place and is retried later, so it never withdraws documentation.

### Documentation provenance

A malicious or swapped doc can socially-engineer a user into a dangerous configuration (over-broad IAM, a fake setup example pointing credentials at an attacker), and a mirror could swap docs independently of the author. When the version is signed, the docs bundle is therefore signed in the **same signing ceremony** as the `.wasm`: the same release workflow run signs the bundle's SHA256 keylessly, producing its own Sigstore bundle whose certificate matches the artifact's in the components §7 lists, the run invocation URI included. At publish the registry verifies that signature exactly as it verifies the artifact's (§7), so a docs bundle signed by another run, workflow, ref, commit or repository is refused.

The version's §3 response carries the bundle's coordinates in its `docs` field, so doc authenticity is verifiable independent of the serving registry (§6): a reader fetches the archive from `docs.download_url`, recomputes `docs.shasum`, and verifies `docs.signature` against the signer it already holds for the version. The rendered HTML is the serving registry's derivation of that archive and is not itself signed; reading the pages trusts the serving registry, while the archive can be checked by anyone.

Unlike the binary, doc verification need not be fully fail-closed for readers: a tampered doc misleads rather than executes (a different blast radius). A client that displays docs verifies them like the artifact (§3 "Client verification" steps 1 and 2, against `docs.shasum` and `docs.signature`); it MAY additionally check the docs certificate against the artifact's for the §7 components. A `docs` object without `docs.signature` on a version whose `signature` is present is a failed docs verification. A client that fails to verify docs SHOULD warn and MAY still display them. This relaxation is for readers only; the registry's publish-time checks are fail-closed. Because docs are optional per version, §2's per-provider presence rule does not apply to `docs`: a version without docs is normal. Its presence is instead sticky per version, like `yanked`: a version's docs are fixed at publish (§9), so a client that has seen `docs` for a `(host, namespace/name, version)` and later receives that version without it MUST treat the omission as a failed docs verification (warn), never as "this version has no docs".

## Future Considerations

The following are out of scope for the initial implementation but may be considered later:

- **Authentication**: token-based authentication for **read** access to private providers (the **publish** side is now normative in §7; this item is now only about read-side per-host auth)
- **Search API**: `GET /v1/providers?q=aws` for provider search
- **Publish API**: ~~an API for automatically registering providers from CI~~ — now normative in §7
- **Provider signing**: ~~signature verification of provider binaries~~ — now normative in §3 (the `signature` block + client-verification MUST); what remains deferred is the implementation
- **Mirrors/Proxy**: mirror registries for network-restricted environments (their trust model is now normative in §6; what remains deferred is the operational deployment)
- **Third-party provider registration**: a flow for registering third-party providers (e.g. an opt-in watch list combined with scheduled polling of their GitHub Releases)
- **SHA resolution on §4**: §4 specifies the full surface, while the current implementation serves branch and tag keys. SHA resolution remains out of scope and can be added later without breaking clients
