# Host-resolved AWS credentials for AWS-kind guests (carina#4164)

## Problem

AWS-kind WASM providers (`aws`, `awscc`) resolve credentials inside the
guest. The guest's AWS SDK default chain sees only the variables in the
host's `AWS_ENV_ALLOWLIST`. It has no filesystem, so it cannot read
`~/.aws/config`, `~/.aws/credentials` or the SSO token cache, and
`AWS_PROFILE` never crosses. IAM Identity Center (SSO) and named-profile
users therefore have no working path. Today they fall into the
"no credentials" path, which also hangs (carina#4165, tracked
separately).

The maintainer decided (comments on #4164, 2026-10-31):

- The host resolves AWS credentials with the AWS SDK, honouring
  `AWS_PROFILE`, SSO and `~/.aws`.
- The host keeps the refreshing credentials provider for the whole run.
- A new host import gives the guest the current credentials, and the guest
  SDK calls it when its cached credentials near expiry.
- The import is typed and AWS-only. Only the access key, secret, session
  token and expiry cross. `~/.aws` and `AWS_PROFILE` never cross.
- A resolve or refresh failure is a typed error returned to the guest,
  never a hang.
- Neither carina-core nor carina-plugin-host depends on the AWS SDK. They
  hold only a typed trait. The AWS implementation lives in its own crate,
  which the CLI binds to the AWS kind.

This note settles the remaining shape. Each decision lists its
alternatives and why the long-term, type-safety and root-cause lenses all
select it.

## Decisions

### D1. A separate WIT world for AWS-kind providers

`carina-plugin-wit` gains an interface and a world:

```wit
interface aws-credentials {
    record aws-credentials {
        access-key-id: string,
        secret-access-key: string,
        session-token: option<string>,
        /// Seconds since the Unix epoch. None means the credentials do not expire.
        expires-at: option<u64>,
    }

    variant aws-credential-error {
        /// No source in the host's chain produced credentials. The payload
        /// is a host-rendered, secret-free explanation.
        not-found(string),
        /// A source was found but failed, for example an expired SSO
        /// session or a network error. Host-rendered and secret-free.
        provider-failed(string),
    }

    current-credentials: func() -> result<aws-credentials, aws-credential-error>;
}

world carina-provider-aws {
    import wasi:http/outgoing-handler@0.2.6;
    import aws-credentials;
    export provider;
}
```

The host links `aws-credentials` only into an instance whose
`ProviderKind` is `Aws`. A component that imports it but is loaded as
`GitHub` or `Untrusted` fails at instantiation with a typed load error
("this provider requires AWS credentials, which only an AWS-kind provider
can receive"). This means a non-AWS guest cannot reach the import at all.

Rejected:

- One world with the import for everyone and a runtime `not-allowed` error
  for non-AWS kinds. Every future caller of the linker would have to
  remember the kind check, and a mistake leaks credentials.
- A generic `get-credential(kind) -> string`. The maintainer rejected it,
  and it moves the type into a convention.

### D2. The host is the only credential source for an AWS-kind guest

The credential variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_SESSION_TOKEN`, `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`,
`AWS_CONTAINER_CREDENTIALS_FULL_URI`) leave `AWS_ENV_ALLOWLIST`. The AWS
kind's metadata endpoints (IMDS, ECS) and the IMDS reachability probe that
sets `AWS_EC2_METADATA_DISABLED` are removed. The host chain already reads
the environment, ECS and IMDS natively, so every source that works today
still works, now through one path.

What stays in the guest env is region and endpoint configuration only:
`AWS_REGION`, `AWS_DEFAULT_REGION` and `AWS_ENDPOINT_URL`.

The guest SDK config uses one credentials provider, the host-backed one
(D4). It no longer runs `aws_config`'s default credential chain. Role
assumption from the provider block (`assume_role`) stays in the guest. It
uses the host-backed provider as its base credentials and STS refreshes
the assumed role itself.

Rejected: keeping the guest chain as a fallback. That leaves two paths
that can disagree, keeps metadata endpoints reachable from the guest, and
keeps the hanging "nothing found" path (#4165) reachable.

Side effect: with no default chain left in AWS guests, #4165's hang is no
longer reachable through credential resolution. #4165 stays open for its
own root cause.

### D3. Typed trait in carina-plugin-host, AWS implementation in a new crate

carina-plugin-host defines the boundary types and a trait, with no AWS
dependency:

```rust
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: SecretString,       // redacted Debug
    session_token: Option<SecretString>,   // redacted Debug
    expires_at: Option<SystemTime>,
}

pub enum AwsCredentialError {
    NotFound(String),
    ProviderFailed(String),
}

pub trait AwsCredentialSource: Send + Sync + 'static {
    fn current(&self) -> BoxFuture<'_, Result<AwsCredentials, AwsCredentialError>>;
}
```

The error payloads are rendered by the source and must not contain secret
material. The source owns redaction. The trait doc and a test on the
implementation pin this.

carina-core is not touched. The kind partition and the factory both live
in carina-plugin-host, so core has nothing to hold.

A new crate, `carina-credentials-aws`, implements `AwsCredentialSource`
over `aws_config`'s default chain (with SSO). It keeps one
`SharedCredentialsProvider` for the run and its own cache of the last
credentials. Each `current()` is answered from that cache and fetches again
within five minutes of expiry. If that fetch fails while the cached
credentials are still valid, it returns the cached ones; it fails only
after they have expired. Every SDK load and fetch runs under a timeout, so
an unreachable IMDS cannot stall a run. carina-cli depends on
it and binds it to the AWS kind (D5). Neither carina-core nor
carina-plugin-host depends on it.

### D4. The guest side

- carina-plugin-sdk generates bindings for `aws-credentials` and exposes
  `carina_plugin_sdk::aws_credentials::current()` returning a typed result.
  It adds an `export_provider!(T, aws, egress: [...])` arm that selects the
  `carina-provider-aws` world. The SDK gains no AWS SDK dependency.
- `carina-aws-types` (carina-provider-aws repo, shared by aws and awscc)
  gains `HostCredentialsProvider`, an `aws_credential_types::provider::ProvideCredentials`
  implementation over that function. It maps `expires-at` to the SDK's
  expiry so the SDK's identity cache asks again before expiry.
- aws and awscc switch to `export_provider!(T, aws, egress: [...])` and set
  `HostCredentialsProvider` as the config's credentials provider.

### D5. Binding and lifetime in the host

The runtime network for an AWS-kind instance must carry a source. The
type makes that required:

```rust
pub enum RuntimeCapabilities {
    Aws { credentials: Arc<dyn AwsCredentialSource> },
    GitHub,
    Untrusted,
}
```

`RuntimeCapabilities` replaces the bare `ProviderKind` in
`GuestNetwork::Runtime`. A runtime factory for an `Aws`-trusted artifact
cannot be built without a source, and the schema-only path never builds
one. So `validate`, `fmt`, LSP and schema loading never trigger SSO or
read `~/.aws`.

The CLI creates one `carina-credentials-aws` source per command run,
lazily, the first time it builds an AWS-kind runtime factory. It shares
the source across all AWS-kind instances of that run. Before it starts
any AWS-kind guest, the CLI calls `current()` once. A failure stops the
command with the typed error and names the workaround. This fails fast
before any effect starts, as the first decision required. Later failures,
for example an SSO session that ends during a long apply, reach the guest
as `aws-credential-error` and surface as that operation's provider error.

### D7. `assume_role` and the two expiry clocks

Both providers accept `assume_role = { role_arn, session_name?,
external_id?, duration? }` and check `allowed_account_ids` against the
role's account. In aws this is `extract_assume_role` /
`check_cross_account` in `factory.rs`; awscc has the same pair in `main.rs`.
Both build the guest config with `aws_config::defaults(...)` and wrap it
with `aws_config::sts::AssumeRoleProvider::builder(role_arn).configure(&base)`.
`configure` takes the base config's credentials provider as the source
credentials. infra uses this in the registry dev/prod `serve` stacks
(`envs/registry/{dev,prod}/serve/providers.crn`, the named instance for
the cross-account delegation-writer role used for the Route 53 alias).

This stays in the guest unchanged. Only the base changes: the base config's
credentials provider becomes `HostCredentialsProvider` instead of the
default chain. `AssumeRoleProvider` then calls STS `AssumeRole` with
host-provided base credentials, exactly as it does today with
env-provided ones. `check_cross_account` and the
`allowed_account_ids` / `forbidden_account_ids` guard run before and after,
as today.

There are two expiry clocks, and each side owns one:

- **Base credentials** (SSO role credentials, env keys, ECS/IMDS) belong
  to the host. The host's credential cache refreshes them near expiry, using
  the SSO token cache for SSO. The host never starts an interactive
  login: no browser, no device-code prompt, and no `aws sso login`
  subprocess. It only reads the token cache and refreshes the token with
  the SSO OIDC refresh token when one is present, which is how
  `aws_config`'s SSO provider already behaves. A test with an expired,
  non-refreshable token asserts a prompt `provider-failed` error that names
  `aws sso login`, and no child process. The guest sees them through
  `HostCredentialsProvider`, whose `expires-at` makes the guest's identity
  cache ask the host again before expiry. If the SSO session itself has
  ended, the refresh fails as `provider-failed` with the SDK's message (for
  example, run `aws sso login`).
- **Assumed-role credentials** belong to the guest. `AssumeRoleProvider`
  caches them with their STS expiry and calls `AssumeRole` again near
  expiry. That call signs with the current base credentials, which come
  from the host. So a long apply crossing either expiry keeps working
  without any special code. A base refresh failure surfaces as the
  `AssumeRole` call's credentials error.

The host never sees the role ARN or the assumed credentials. The import
returns only the base identity.

### D8. Region

Region does not change, and nothing visible to users changes.

Neither provider ever read a profile's `region`. Both call
`extract_region_from_attrs(attributes, "ap-northeast-1")` and pass the
result to `aws_config::defaults(...).region(...)`. So the guest SDK always
gets an explicit region: the provider block's `region`, or
`ap-northeast-1` when it is omitted. The guest also never had the profile
file, so a profile region was unreachable twice over. The region stays a
provider-block setting, passed to the guest as today. `AWS_REGION` and
`AWS_DEFAULT_REGION` stay in the env allowlist for the SDK's own use.

The host-side chain resolves its own region from the profile or env, and
uses it only for the credential sources that need one: the SSO portal uses
the profile's `sso_region`, and STS web identity uses the profile or env
region. That region is not passed to the guest.

### D9. One source per run, per-block settings stay per instance

The base identity comes from the process environment, `AWS_PROFILE` and
`~/.aws`. All of these are global to the process, and no provider block
has a `profile` attribute today. So the CLI creates exactly one
`AwsCredentialSource` per command run and shares it across every AWS-kind
instance: every block of `aws` and `awscc`, including aliases such as
infra's `let us = provider aws { region = us_east_1 }`.

What differs per block stays per instance in the guest: region,
`assume_role` (each instance has its own `AssumeRoleProvider` and cache),
`allowed_account_ids` / `forbidden_account_ids`, and `default_tags`. Two
blocks with different `assume_role` therefore hold two different assumed
identities over the same base.

`RuntimeCapabilities::Aws { credentials }` carries the source per
instance. A later per-block `profile` attribute would therefore pass a
different source to that instance, with no change to the type.

The S3 state backend keeps its own `aws_config` loader in carina-state, as
today. Sharing it with this source is outside this issue.

### D6. Protocol

This is a WIT change. Several PRs (#4162, #4168, this one) bump the
protocol at about the same time, so the number is not fixed in advance:
it is main's version plus one at the time this PR merges, and
`MIN_SUPPORTED_PROTOCOL_VERSION` moves with it. There is no compatibility shim, per project policy. The order is
plugin-wit → carina → aws → awscc, coordinated with the orchestrator,
together with the egress declarations already in flight.

## Security notes

- Secrets cross only through the typed record, never the environment, so
  `RUST_LOG` / env dumps of the guest no longer contain them.
- `AwsCredentials` and the WIT record conversion never derive `Debug` over
  secrets. A test asserts the `Debug` output has no secret bytes.
- The import is linked per instance after the kind is known. A test
  instantiates a component that imports `aws-credentials` as `Untrusted`
  and as `GitHub` and asserts the typed load error.
- Metadata endpoints leave the AWS kind (D2), so no guest can reach
  instance metadata.

## Tests

- carina-credentials-aws: a static source produces credentials; an empty
  environment with no profile produces `NotFound` promptly (bounded time);
  the error text has no secret bytes.
- carina-plugin-host: the import returns the source's credentials; a source
  error reaches the guest as `aws-credential-error`; a non-AWS kind cannot
  instantiate a component importing the interface; the AWS kind has no
  credential variables and no metadata endpoints in its partitions.
- mock provider: a mock-http fixture built with the `aws` world calls
  `current-credentials` and echoes a non-secret field, so the integration
  test covers the real import path. A refresh test serves an expiring
  credential first and a fresh one second.
- CLI e2e: with no AWS credentials anywhere, `apply` of an AWS-kind
  provider fails fast with the typed error, which pins this issue's
  reported hang path at the CLI level.
- aws / awscc (in their repos): with a fake host source, the guest config
  uses `HostCredentialsProvider` as the base. `assume_role` builds
  `AssumeRoleProvider` over that base, verified with a recorded STS
  exchange. Two instances with different `assume_role` keep separate
  assumed identities over one source.

### Real-account checks

These run before the provider rev bump merges. They use carina-test-*
accounts only.

1. SSO only: `AWS_PROFILE=<sso profile>` with no env keys. `apply` and
   `destroy` an `awscc.sns.Topic` and an `aws` resource. This is #4164's
   own repro, so it must now succeed.
2. Static env keys only (the acceptance-tests runner path) and an empty
   environment (expect the typed error, promptly).
3. `assume_role` alias: two `aws` blocks in one directory, one with
   `assume_role` into a second carina-test account and one without. Each
   resource lands in its own account. Run it with an SSO base and with an
   env-key base.
4. Refresh: a role session of 15 minutes (the shortest STS allows) and an
   `assume_role.duration` of 15 minutes. Run an apply that outlives both,
   for example a resource with a long create wait. It must finish without
   a credentials error.

infra itself is not run by me (no real-infra AWS commands). Once the
providers are bumped, the maintainer runs `carina plan` with their usual
credentials in these directories and expects no change:
`envs/registry/dev/serve` and `envs/registry/prod/serve` (region alias
plus the cross-account `assume_role` alias), `aws/management/route53`,
and one plain directory such as `envs/registry/dev/network`. I hand over
the exact commands.

## Impact

| Area | Change |
|---|---|
| carina-plugin-wit | `aws-credentials` interface, `carina-provider-aws` world |
| carina-provider-protocol | Protocol version bump (D6) |
| carina-plugin-host | `AwsCredentials`, `AwsCredentialSource`, `RuntimeCapabilities`; per-kind linking; AWS env partition and metadata endpoints shrink |
| carina-credentials-aws | New crate (aws-config default chain + SSO) |
| carina-cli | Creates the source, fail-fast check, binds it to AWS-kind factories |
| carina-plugin-sdk | `aws_credentials::current()`, `export_provider!(T, aws, ...)` |
| carina-provider-mock-http | An `aws`-world fixture for integration tests |
| carina-provider-aws | `HostCredentialsProvider` in carina-aws-types; switch world |
| carina-provider-awscc | Switch world; use `HostCredentialsProvider` |
| Docs | Provider authoring (worlds), credentials page (SSO now supported) |
| State, plan, lock | None |
