# Provider-Declared Egress Hosts: Design

Tracking: carina-rs/carina#4021
Related: carina-rs/registry#16 (third-party onboarding gate), carina#4022
(per-kind metadata partition, closed), carina#4018 (provider authoring docs)

## Problem

The plugin host lets a WASM provider's `wasi:http` requests reach only
`*.amazonaws.com` and `*.amazonaws.com.cn` (`HTTP_ALLOWED_HOST_SUFFIXES` and
`check_host` in `carina-plugin-host/src/wasm_factory.rs`). Every other
hostname, and every IP literal that is not an instance-metadata address, is
denied with `EgressDenied`. Neither a provider nor a user can extend the
list, so a provider for any non-AWS HTTP API (Cloudflare's
`api.cloudflare.com` is the first target; the GitHub provider would need
`api.github.com`) cannot make a single request.

Two facts about the current code shape the design:

- The hostname check ignores `ProviderKind`. Only metadata IPs are
  partitioned per kind (carina#4022). A `GitHub`-kind provider, an
  `Untrusted` provider and the kind-less info/schemas instance can all reach
  any `*.amazonaws.com` host today.
- The hard-coded list is host policy that no provider declared and no user
  agreed to. It works only because the only real HTTP providers are
  first-party AWS providers.

The root cause is that egress has no owner: nothing ties "which hosts may
this provider reach" to the provider's identity. The fix gives it one owner
(the provider artifact), one pinned copy (the lock), one consent step
(`carina init`), and one enforcement point (`check_host`), with types that
make an unconsented or unverified declaration impossible to turn into a
permission.

## Decisions

Each decision below was either chosen by the maintainer from listed
options (marked "chosen"), or follows uniquely from the three lenses
(long-term, type safety, root cause) and is marked "follows".

### D1. The provider declares its egress inside the WASM artifact (chosen)

Options considered:

| Option | Where the declaration lives | Verdict |
|---|---|---|
| A | Inside the WASM artifact; copied into the lock; consented at init | chosen |
| B | Registry metadata | Not covered by the artifact signature; unavailable for `file://` and direct-GitHub sources |
| C | User config (`allowed_hosts` in the `provider` block) | Every user must re-type what the author knows; anyone can widen; the guarantee is no longer tied to the provider's identity |
| D | A per-`ProviderKind` table in the host | Cannot serve third-party providers |

A wins on type safety and root cause because the declaration becomes part of
the bytes that the lock's `sha256` and the Sigstore signature already pin,
for every source form. Its weakness is hosts that depend on user config
(GitHub Enterprise Server, an `AWS_ENDPOINT_URL` pointing at LocalStack).
That case is out of scope here.

A user-side narrowing knob (a subset of the declaration) may be added later.
Widening beyond the declaration is never allowed.

### D2. The declaration is a versioned WASM custom section (chosen)

Options considered:

- **A1**: a new field in the `info()` JSON.
- **A2**: a custom section, which is data in the binary rather than code.

A2 was chosen. The declaration is then a pure function of the artifact
bytes. The host reads it without instantiating the guest, so `carina init`
shows the user what will be allowed before any guest code has run, and the
same `sha256` always means the same declaration.

Section shape:

- Name: `carina:egress`.
- Payload: UTF-8 JSON, `{"version": 1, "hosts": ["api.cloudflare.com", ...]}`.
- The section carries its own `version`, so WIT and protocol version 13 do
  not change.

Reading rules (fail closed):

| Section state | Result |
|---|---|
| Absent, component has no `wasi:http/` import | Empty declaration |
| Absent, component has any `wasi:http/` import (the existing HTTP-world classifier) | Load error naming the SDK version that embeds the section |
| Unknown `version`, malformed JSON, or an invalid pattern | Load error. Never "deny some, allow the rest" |
| More than one `carina:egress` section anywhere in the component, including nested core modules | Load error |

**SDK.** The HTTP form of the export macro requires the declaration, so an
HTTP provider cannot be written without one:

```rust
carina_plugin_sdk::export_provider!(MyProvider, http, egress: ["**.amazonaws.com", "**.amazonaws.com.cn"]);
```

The macro emits the section with `#[used] #[link_section = "carina:egress"]`
and checks pattern syntax at compile time. The host repeats every check
(public suffix and forbidden address space included), because the section
is untrusted input.

**Verified on a real build (2026-10-31).** Setup:

- rustc `wasm32-wasip2` and `wasm-tools` 1.246.1.
- A throwaway crate with
  `#[used] #[link_section = "carina:egress"] static EGRESS: [u8; N] = *include_bytes!("egress.json");`.

Results:

- The section survives the component wrapping. In both the dev profile and
  a release profile with `strip = true` and `lto = true`, it sits inside the
  component's main core module (`(core module $main …)` →
  `(@custom "carina:egress" …)`), not at the component level.
- `wasm-tools strip` with its default options, and `strip --all`, both
  remove it.

The rules that follow:

- The host searches the component and every nested core module, and
  requires exactly one `carina:egress` section across all of them.
- The linker concatenates same-named `link_section` statics, so a second
  declaration somewhere in the dependency graph produces a payload that is
  not valid JSON and fails to load. Two declarations can never merge
  silently.
- A provider build must not run `wasm-tools strip` without excluding this
  section. If it does, an HTTP provider fails to load with the
  missing-section error.
- Neither aws nor awscc post-processes its `.wasm` today; their workflows
  and Cargo configs have no `wasm-opt` or `wasm-tools` step.

**Non-Rust providers.** `wasm-tools` 1.246 has no subcommand that adds an
arbitrary custom section. The verified path adds it to the core module in
text form before wrapping it as a component. `wit-component` keeps unknown
custom sections of the core module, as the Rust build shows. The provider
authoring docs (carina#4018) get these steps:

```sh
# core.wasm: the language toolchain's core module output
wasm-tools print core.wasm > core.wat
# Insert a custom-section annotation before the module's final ")":
#   (@custom "carina:egress" "{\"version\":1,\"hosts\":[\"api.example.com\"]}")
wasm-tools parse core.wat -o core-with-egress.wasm
wasm-tools component embed carina-plugin-wit/wit --world carina-provider-with-http \
  core-with-egress.wasm -o embedded.wasm
wasm-tools component new embedded.wasm -o provider.wasm
```

On the spike, this produced `(@custom "carina:egress" …)` inside the
component's core module. For a toolchain that already emits a component,
the same annotation is added to its core module before `component new`.
Phase 1 includes a host test fixture built this way.

### D3. Host pattern grammar: exact, `*.`, and `**.` (chosen)

Measured on aws `be42734`, awscc `7145283` and `aws-sdk-s3` 1.156. Both
providers only pass a region to `aws_config::defaults()`; the SDK builds
every endpoint. The label counts below are those in front of
`amazonaws.com`:

| Endpoint kind | Host shape | Labels |
|---|---|---|
| Global | `iam.amazonaws.com`, `route53.amazonaws.com` | 1 |
| Regional (STS, CloudControl, EC2, ...) | `{svc}.{region}.amazonaws.com` | 2 |
| Fixed-region | `organizations.us-east-1...`, `account.us-east-1...` | 2 |
| S3 virtual-hosted | `{bucket}.s3.{region}.amazonaws.com` | 3 |
| S3 bucket with dots | Path style `s3.{region}.amazonaws.com/{bucket}` | 2 |
| SSO | `portal.sso.{region}...` | 3 |
| OIDC | `oidc.{region}...` | 2 |
| China | Same shapes, under `amazonaws.com.cn` | same |

FIPS and dual-stack endpoints are unreachable today because
`AWS_USE_FIPS_ENDPOINT` and `AWS_USE_DUALSTACK_ENDPOINT` are not on the host
env allow-list and `~/.aws/config` is not visible to the guest. They would
add 4-label shapes and `*.api.aws`. Regions are runtime values, so a static
declaration needs a multi-label wildcard.

Grammar:

| Pattern | Matches |
|---|---|
| `api.cloudflare.com` | Exactly that host |
| `*.example.com` | Exactly one label before `example.com`, as in TLS certificates |
| `**.example.com` | One or more labels before `example.com` |

Rules:

- A wildcard is allowed only as the whole leftmost label. A wildcard in any
  other position (`api.*.com`, `*foo.com`, `a.**.com`) is rejected.
- The part after a wildcard must not be a public suffix (`*.com`,
  `**.co.uk`, `*.github.io`). The check uses the Public Suffix List through
  the `psl` crate, which is a new dependency of the host and SDK pattern
  crate.
- Rejected at parse time:
  - IP literals in any form, including bracketed IPv6
  - `localhost` and `*.localhost`
  - Any pattern whose literal labels form a metadata or link-local name
  - Non-ASCII labels (authors write A-labels)
  - Empty labels
  - Labels longer than 63 bytes
- Before comparison, both the pattern and the request host are lowercased
  and stripped of one trailing dot. A non-ASCII request host is denied.
- The post-resolution check stays as it is: every resolved address is still
  vetted against metadata and link-local space. A declared name that
  resolves to `169.254.169.254` is still denied unless the kind owns that
  endpoint.

aws and awscc declare `**.amazonaws.com` and `**.amazonaws.com.cn`, which is
exactly today's hard-coded reach. No declaration grammar can stop a
legitimate S3 provider from writing to an attacker-owned bucket under
`*.s3.{region}.amazonaws.com`. That is abuse of a legitimately granted
capability (registry `docs/full-scale-architecture.md` §2.1) and belongs to
registry#16, not to hostname policy.

**Consent display.** The display separates `**.` patterns from `*.`
patterns, makes the `**.` ones the most prominent, and says in words that
the provider may reach a host at any depth below the suffix.

### D4. Consent: carina-rs-signed providers by lock commit, others explicitly (chosen)

Options considered:

- **C1**: prompt for every provider.
- **C2**: treat the lock commit as consent for every provider.
- **C3**: split by trust class.

C3 was chosen:

- **carina-rs-signed providers.** These are the providers that
  `classify_verified_remote` puts in the `Aws` or `GitHub` class: their
  source is allowlisted and their signer is carina-rs. `carina init` records
  the declaration in the lock without prompting. Committing the lock is the
  consent.
- **Every other provider** (an `Untrusted` remote, a `file://` build, or a
  `--trust-local-provider` grant) needs explicit consent when `carina init`
  writes a new or changed declaration into the lock:
  - On a TTY, `carina init` shows the declaration and asks y/N.
  - Without a TTY, `carina init` fails unless
    `--accept-egress <source>` (repeatable) or `CARINA_ACCEPT_EGRESS` names
    the provider's source.

The flag takes the source, not the configured name. The consent it produces
is bound to the source and the declaration (D5, D7), never to a configured
name.

**Trusted-local grants (follows).** `--trust-local-provider PATH=aws` is
unsigned, so it falls on the explicit-consent side. D6 means a rebuild with
an unchanged declaration does not prompt again.

**Changed declarations.** When a declaration changes, `carina init` prints
the added and removed patterns for both classes. carina-rs-signed providers
still need no prompt.

### D5. Explicit consent is recorded in the lock (chosen)

Options considered:

- **S1**: record consent in the lock.
- **S2**: record consent in a per-user trust record.
- **S3**: put the declaration in the lock and the consent in a per-user
  record.

S1 was chosen. Only the person whose `carina init` writes a new declaration
into the lock is asked. Other team members and CI consent by accepting the
committed lock, which is the same model as carina-rs-signed providers.

**What D5 does and does not protect.** An `explicit` record in the lock is an
unauthenticated assertion: anyone who can commit to the repository can add
one that matches their provider's declaration, and plan/apply then run with
no prompt. That is the D5 model (committing the lock is the consent), so the
protection is review of the lock diff, which shows each provider's hosts
one per line. The type rules still guarantee that the recorded hosts equal
the declaration in the loaded bytes, and that no hand edit can claim the
`carina-rs-signed` class.

### D6. Unchanged declaration across a byte change carries consent over (chosen)

Options considered:

- **R1**: ask again on every `sha256` change.
- **R2**: carry the consent over while the declaration is unchanged.

R2 was chosen. When the source is the same and the new declaration is
exactly equal to the one that was explicitly consented, `carina init`:

- prints "egress hosts unchanged"
- carries the record over to the new `sha256` without prompting

Consent is not carried over in these cases:

- **The source changes**, for example from `file://` to a registry. This is
  a new consent.
- **The declaration changes at all**, whether patterns are added or removed.
  `carina init` shows the diff and asks.

The carry-over can only be built from a previous explicit-consent record
plus an equal declaration with the same source (see Types).

### D7. Follow-on decisions (follows)

- **Kind-less instance.** The info/schemas instance (`kind: None`) gets no
  egress at all. It never calls `initialize` or a credentialed operation and
  has no reason to use the network. Today it can reach `*.amazonaws.com`.
- **AWS host list.** `HTTP_ALLOWED_HOST_SUFFIXES` is deleted. Egress comes
  only from a consented declaration. `ProviderKind` keeps deciding only the
  two secret-bearing partitions, credentials (env) and metadata endpoints. A
  `GitHub`-kind or `Untrusted` provider no longer reaches `*.amazonaws.com`
  unless it declares it.
- **Test route.** `HttpTestRoute` stays test-only, constructible only under
  `cfg(any(test, feature = "test-utils"))`. Today a matching route skips
  `check_host`. It is changed to run after the policy check and replace only
  name resolution. Integration tests then exercise both directions through
  the real `ConsentedEgress`: a declared host is allowed, and an undeclared
  host is denied.
- **Consent is per source, not per configured name.** Lock entries are
  already one per canonical source (`sources_match`). Two configured
  providers that share a source therefore share one declaration and one
  consent, and init prompts once per source. `--accept-egress` takes the
  source, not the configured name, for example
  `--accept-egress github.com/example/carina-provider-cloudflare`, or the
  canonical path for a `file://` source. The configured name never takes
  part in consent identity.
- **Which instances need consent.** The declaration is read and validated
  whenever a provider is loaded. A malformed or missing section is a provider
  load error, handled by each command's existing load-failure policy:
  validate and lint report it, plan and apply fail, and fmt treats it like
  any other provider load failure on its best-effort path.
  Consent is required only where a runtime (credentialed) instance is
  created. `validate`, `fmt`, and the LSP use only the kind-less
  info/schemas instance, which gets no egress (above). They never need
  consent and never fail on a missing or stale consent.
- **`carina init` modes.**
  - `--upgrade` rebuilds the lock from scratch. It reads the old entries'
    consent records before discarding them, so D6's carry-over still applies.
  - `--locked` never prompts and never records or changes an egress record.
    It still saves the other lock state exactly as on `main` (signer pins,
    registry sequence anchors). A missing or
    changed declaration is a hard error.
  - `--accept-egress` together with `--locked` is rejected as a flag
    conflict.
- **Recovery commands.** `carina providers repin-identity` and `rebootstrap`
  rewrite signer pins, not egress records. The `CarinaRsSigned` witness is
  minted from the signer verified at each load (Types), so a repin
  automatically revalidates the `carina-rs-signed` records.

### D8. `file://` consent binds path and declaration, not `sha256` (chosen)

D5 binds an explicit consent to source, `sha256` and declaration. For
`file://` sources the resolver never compares the installed bytes with the
lock's `sha256` (a rebuild changes it, and each init rewrites it).

Options considered:

- **Enforce the `sha256`.** Every local rebuild would then need
  `carina init` before `plan`, even though it never prompts.
- **Bind the consent to the canonical path and the declaration** and keep
  the recorded `sha256` informational.

The second was chosen. For `file://` sources:

- File providers still load the copy that `carina init` installed, as on
  `main`, so a rebuild takes effect only after `carina init`.
- At that init, an unchanged declaration carries consent over without a
  prompt. A changed declaration shows the diff and asks.

Lock-controlled sources keep the full binding, because the resolver already
refuses bytes whose `sha256` differs from the lock.

**The canonical path is project-relative (chosen, during Phase 1).** It is
normalized relative to the directory that holds `carina-providers.lock`,
never as a machine-specific absolute path. The lock's `source` spelling for
file providers stays as it is on `main`. A lock committed on one machine
therefore loads with the same consent on CI and on other machines. A test
pins this by copying one project to two different absolute directories.

### D9. Resolved-address policy per kind (chosen, during Phase 1)

The adversarial review found that a declared name can resolve to loopback,
private or other-cloud metadata addresses. Before this decision, post-DNS
vetting denied only AWS metadata and link-local addresses. The maintainer
chose option P2:

| Resolved address | `Aws` kind | Other kinds, schema-only |
|---|---|---|
| Loopback (127.0.0.0/8, ::1), unspecified (0.0.0.0, ::) | denied | denied |
| Private (RFC 1918, fc00::/7, CGNAT 100.64.0.0/10) | allowed (VPC interface endpoints / PrivateLink) | denied |
| Metadata (AWS IMDS/ECS, Alibaba 100.100.100.200, Azure 168.63.129.16) | AWS endpoints only | denied |
| Link-local | denied | denied |

How the policy is enforced:

- Whether private addresses are allowed is a field of the per-kind
  partition, next to `credential_partition` and `metadata_endpoints`. It is
  not a check each caller makes.
- Every address is canonicalised before classification, so IPv4-mapped and
  NAT64-embedded IPv6 forms cannot bypass the policy.
- `--trust-local-provider PATH=aws` builds are `Aws` kind and get the same
  private-address allowance. The user explicitly trusted them as AWS, and
  they already receive credentials, so withholding private addresses would
  add no protection. The partition is decided by kind alone.
- Metadata classification wins over the private range. For example,
  100.100.100.200 is inside CGNAT and is still denied to the `Aws` kind.

Options considered and rejected:

- **P1** denies private addresses for every kind. It breaks PrivateLink for
  aws and awscc.
- **P3** keeps the old vetting. It leaves a third-party path into the user's
  internal network.

## Types

The aim is that a request permission can only be built from:

- a declaration read from the bytes that were actually loaded
- a lock record for the same source
- one of the two consent producers (D4)

The anchor is the existing `VerifiedLoad { bytes, identity, artifact }`
(`carina-provider-resolver/src/verified_artifact.rs`). Only the resolver
constructs it, and the factory gets its bytes only from it.

```rust
// Parsed, validated pattern. `parse` is the only constructor.
pub struct HostPattern(HostPatternKind);
enum HostPatternKind {
    Exact(AsciiHost),
    OneLabel(RegistrableSuffix),   // "*."
    AnyDepth(RegistrableSuffix),   // "**."
}

// Read from the custom section of the bytes inside a VerifiedLoad. It
// carries the load's canonical source and sha256, so it cannot be paired
// with another artifact.
pub struct VerifiedEgressDeclaration {
    source: CanonicalSource,
    sha256: Sha256,
    patterns: BTreeSet<HostPattern>,
}
impl VerifiedEgressDeclaration {
    pub fn read(load: &VerifiedLoad) -> Result<Self, EgressSectionError>;
}

// Minted by the resolver only on a remote load whose signer was verified as
// carina-rs and whose source is allowlisted (the `classify_verified_remote`
// path), for that load's source and sha256. Trusted-local grants never mint
// it, even when they assign class Aws or GitHub.
pub struct CarinaRsSigned { source: CanonicalSource, sha256: Sha256 }

// A recorded explicit consent for one source's declaration.
pub struct ExplicitConsent { source: CanonicalSource, patterns: BTreeSet<HostPattern> }
impl ExplicitConsent {
    // y on a TTY, or --accept-egress / CARINA_ACCEPT_EGRESS naming this source.
    pub(crate) fn granted(decl: &VerifiedEgressDeclaration, by: ConsentInput) -> Self;
    // D6: previous record for the same source + equal patterns.
    pub(crate) fn carry_over(prev: ExplicitConsent, decl: &VerifiedEgressDeclaration)
        -> Result<Self, CarryOverRefused>;
}

pub enum EgressConsent { CarinaRsSigned(CarinaRsSigned), Explicit(ExplicitConsent) }

// What the host enforces.
pub struct ConsentedEgress { patterns: BTreeSet<HostPattern> }
impl ConsentedEgress {
    // Loads that read the lock (plan, apply, destroy).
    pub fn from_lock(entry: &LockedEgress, decl: VerifiedEgressDeclaration,
                     signed: Option<CarinaRsSigned>) -> Result<Self, EgressConsentError>;
    // carina init, after granting, carrying over, or minting a signed witness.
    pub(crate) fn from_consent(decl: VerifiedEgressDeclaration, consent: EgressConsent)
        -> Result<Self, EgressConsentError>;
}

// One value per guest store; kind and egress cannot disagree.
enum GuestNetwork {
    SchemaOnly,                                            // no egress, no metadata
    Runtime { kind: ProviderKind, egress: ConsentedEgress },
}
```

`ConsentedEgress::from_lock` checks:

| Lock record | Accepted only when |
|---|---|
| Either kind | The lock entry's source equals `decl.source`, and its `hosts` equal `decl.patterns` |
| `consent = "carina-rs-signed"` | `signed` is `Some` and was minted for `decl.source` and `decl.sha256`. A hand-edited `file://` entry, or a trusted-local `=aws` grant, therefore cannot claim lock-commit consent |
| `consent = "explicit"` | It is accepted as a previously granted `ExplicitConsent` for `decl.source` |

`from_consent` applies the same equality checks.

**`ConsentedEgress::nothing_declared(decl)` (added during Phase 1).** This
constructor succeeds only when the declaration is empty, so the value it
produces permits no host. It needs no consent and no lock record. A provider
with no `wasi:http` import, or with an empty list, therefore never makes
`carina init` prompt or fail non-interactively. If that provider later
declares hosts, the declaration is new and goes through the normal consent
flow. Every value that permits at least one host still comes from one of
the two consent producers.

There is no other public constructor, and no egress-specific test
constructor. Tests build a
`VerifiedLoad` through the existing test-utils resolver paths and use the
same functions. The test-only `from_precompiled` path has no raw bytes, so
it can only build `GuestNetwork::SchemaOnly`.

`AllowListHttpHooks` holds a `GuestNetwork` instead of
`kind: Option<ProviderKind>`. `check_host(&GuestNetwork, host)` works as
follows:

- **Metadata IP literals.** Decided by the `Runtime` kind.
- **Hostnames.** Decided by `Runtime.egress`.
- **`SchemaOnly`.** Denies every request.

**Detecting the HTTP world before instantiation.** The rule "absent section
plus HTTP world is an error" (D2) uses the existing import classifier
(`ComponentImports::from_component`, which treats any `wasi:http/` import as
the HTTP world). The section itself is read from the `VerifiedLoad` bytes,
not from the compiled or cached component.

**What the SDK macro checks at compile time.** `export_provider!` is
`macro_rules!`. It passes the patterns to a `const fn` checker in a `const`
item, so a structural error fails the build. Structural errors are a
wildcard in a position other than the leftmost label, an empty or oversized
label, a non-ASCII character, or an IP-literal shape. The public-suffix and
reserved-name checks need the Public Suffix List and cannot run in `const`,
so they run in the host, which repeats every check anyway. Provider crates
can run the same full check in their own tests by calling `HostPattern::parse`
through the SDK; there is one implementation, in `carina-provider-protocol`.

## Lock format (folded into v5)

Lock v5 landed on `main` on 2026-10-30 (451317b7) and is in no release tag;
carina v0.9.0 writes v4. The egress fields therefore join v5 instead of
creating a v6. If v5 is released before Phase 1 merges, this becomes a v6
bump with the same fields.

Each `[[provider]]` entry gains an optional `[provider.egress]` sub-table.
Consent and hosts sit together in it, so "consent without hosts" and "hosts
without consent" cannot be written:

```toml
[[provider]]
name = "cloudflare"
source = "github.com/example/carina-provider-cloudflare"
sha256 = "…"
mode = "version"
version = "0.3.1"

[provider.egress]
consent = "explicit"        # or "carina-rs-signed"
hosts = [
  "api.cloudflare.com",
]
```

How the sub-table is written:

- `hosts` is written one pattern per line in sorted order, so a lock diff
  shows exactly which hosts were added or removed for which provider.
- An empty declaration writes no sub-table. It needs no consent
  (`ConsentedEgress::nothing_declared`, see Types).
- The sub-table holds no user identity, timestamp or machine data. Who
  consented is visible from the commit.

How the sub-table is parsed:

- It uses `deny_unknown_fields`.
- `LockEntry` itself cannot, because it flattens `LockEntryKind`. So a
  misspelled sub-table name such as `[provider.egres]` is ignored, and the
  entry reads as having no consent. That fails closed, and the "run
  `carina init` to record egress hosts" message names the provider's source.

Entries that have no egress record:

- **v5 entries from unreleased `main` binaries.** These have no
  `[provider.egress]`. They load, but have no consent: runtime instances are
  refused until `carina init` writes the record.
- **v1–v4 files.** These migrate in memory through the existing
  `LoadedLockFile::Migrated` path, with the same no-consent result.
- **Older `main` binaries.** These do not reject the new sub-table. Serde
  ignores unknown keys at the entry level, so such a binary drops the
  sub-table the next time it saves the lock (on `init`, `repin-identity`, or
  `rebootstrap`). That too fails closed: the next new binary sees no consent
  and asks for `carina init`.

**How `sha256` relates to consent.**

- For lock-controlled artifacts (registry, GitHub release, revision), the
  resolver already refuses bytes whose `sha256` differs from the lock, so
  `decl.sha256` equals the locked `sha256`.
- For `file://` sources, each init refreshes the recorded sha and
  re-installs the copy that loads use (unchanged from `main`). The
  maintainer chose (D8) not to bind file consent to the sha: for a `file://` source, consent binds
  the canonical path and the declaration, and the recorded `sha256` is
  informational.
  - After a rebuild, `carina init` carries consent over without a prompt
    when the declaration is unchanged, and shows the diff and asks when it
    changed.

## Impact

| Surface | Change |
|---|---|
| WIT | None |
| Provider protocol | None. Protocol 13 stays; the section has its own version |
| Lock | New fields in the unreleased v5 (above) |
| Saved plan | No format bump. `RecordedProviderArtifact` already pins `sha256`, which fixes the declaration; saved-plan apply already requires the loaded artifacts to equal the recorded ones |
| State | None |
| SDK | `export_provider!(T, http, egress: [...])` is required for the HTTP world; the plain world is unchanged |
| Host | `HostPattern`, `VerifiedEgressDeclaration`, `ConsentedEgress` and `GuestNetwork` (which replaces `kind: Option<ProviderKind>` in the HTTP hooks); `check_host` takes `GuestNetwork`; `HttpTestRoute` moves after the policy check; `HTTP_ALLOWED_HOST_SUFFIXES` is deleted |
| Resolver | Mints `CarinaRsSigned` on carina-rs-verified remote loads; lock v5 gains `[provider.egress]`; `--upgrade` keeps the old consent records for carry-over |
| CLI | `carina init` gains the consent prompt, the diff display, `--accept-egress <source>` and `CARINA_ACCEPT_EGRESS` (conflicts with `--locked`); runtime-instance creation in plan, apply and destroy requires `ConsentedEgress` |
| LSP, validate, fmt | Section validity is checked on load; failures follow each command's load-failure policy; no consent is needed (schema-only instance) |
| carina-provider-aws | Bump SDK rev; declare `**.amazonaws.com`, `**.amazonaws.com.cn` |
| carina-provider-awscc | Same as aws |
| carina-provider-mock-http | Declares its test hosts. The integration tests keep using `HttpTestRoute` for routing, and assert that a host outside the declaration is denied |

## Rollout

1. **carina.** Covers everything except the provider declarations:
   - The pattern and section types.
   - The SDK macro and the host reader.
   - `ConsentedEgress` enforcement.
   - The lock v5 `[provider.egress]` sub-table, the `carina init` consent
     flow, and the consent check at runtime-instance creation.
   - Removal of the hard-coded suffix list.
   - Docs for non-Rust authors (`wasm-tools`).
2. **aws, awscc.** There is no separate rev bump for this design. The egress
   declarations ride in the single provider rev bump that also carries the
   protocol-13 work (carina#4136, carina#4150 and related), so each provider
   moves once.
3. **carina.** Bump the provider revs and release.

Between Phase 1 and Phase 2, carina `main` refuses to load the currently
released aws and awscc builds. The error appears at load time and names the
SDK version needed, so requests are not silently denied. Phase 1 merges only
when the orchestrator has scheduled it against that combined provider bump.

## Relation to registry#16

registry#16 lists "WASIp2 capability-grant story for consumers" but decides
nothing about it: who declares, the shape, where it lives, or how consent
and upgrades work. Nothing there blocks this design. This design is the
first concrete piece of that story, and it is shaped so registry#16 can
extend it instead of replacing it:

- The section is versioned, so other capabilities (env or credential
  partitions, metadata) can be added in later versions.
- `EgressConsent` already separates the carina-rs-signed path from the
  explicit path. The third-party onboarding gate can add a reviewed or
  registry-attested class as another witness without changing enforcement.
- What stays open for registry#16 is the legitimate-abuse problem, for
  example an attacker-owned bucket under a declared suffix. No hostname
  grammar can close it.

## Out of scope

- **User-side narrowing** of a declaration. It may be added later; widening
  is never allowed.
- **User-config-dependent hosts**, such as GHES, custom `AWS_ENDPOINT_URL`
  and LocalStack. These get a separate issue.
- **Scheme and port policy.** The check stays host-only, as today.
- **FIPS and dual-stack AWS endpoints.** These are unreachable today for env
  allow-list reasons that this design does not change.
