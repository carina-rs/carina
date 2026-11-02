---
title: "AWS Credentials"
description: "How Carina resolves AWS profiles, IAM Identity Center sessions, environment credentials, and refreshing role credentials on the host."
---

Carina resolves AWS credentials in the host process. The sandboxed AWS and AWSCC providers do not read your home directory, profile files, SSO token cache, container endpoint, or instance metadata service.

## Supported credential sources

Carina uses the AWS SDK default credential chain. It supports, in the SDK's normal order:

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, with optional `AWS_SESSION_TOKEN`
- the profile selected by `AWS_PROFILE`, including credentials and `credential_process`
- IAM Identity Center (AWS SSO) profiles and the AWS CLI SSO token cache
- web identity, ECS container credentials, and EC2 instance-role credentials

For a named profile:

```bash
AWS_PROFILE=development carina plan
AWS_PROFILE=development carina apply
```

For IAM Identity Center, establish the session before starting Carina:

```bash
aws sso login --profile development
AWS_PROFILE=development carina apply
```

Carina never opens a browser, starts device authorization, or runs `aws sso login`. An expired session that cannot be refreshed fails promptly and tells you to run the login command yourself.

## What the provider receives

Only a provider loaded with the AWS trust kind can import host credentials. The host passes the current base access key, secret key, optional session token, and expiration through a typed component interface. It does not put credentials in the guest environment, and the guest cannot inspect `~/.aws`.

The source stays alive for the whole command. Credentials are cached and refreshed before they expire. Long-running operations therefore receive fresh base credentials when they ask again. If a provider uses `assume_role`, the provider's AWS SDK still owns and refreshes the assumed-role session; the host sees only the base identity.

Provider-block settings remain separate. In particular, `region` and each block's `assume_role` configuration are not taken from the host credential identity.

## Commands that need credentials

Schema-only operations such as `validate`, `fmt`, and LSP diagnostics do not resolve AWS credentials. Runtime loading for `plan`, `apply`, and `destroy` checks credentials before starting the first AWS guest, so a missing or expired source fails before an API operation begins.

When no source is found, the error suggests `AWS_PROFILE`, `aws sso login`, or the static environment keys. In CI, provide the same environment or workload credential source that an AWS SDK application would use. Do not copy profile files or credentials into a provider component.

Local provider builds also need an explicit AWS trust grant; see [`--trust-local-provider`](/reference/cli/global-options/#trust-local-provider-pathkind). The grant authorizes the typed credential import but does not expose metadata endpoints.

## KMS reference planning permissions

When a KMS reference differs from the value returned by the service, planning resolves both values to key identities before deciding whether an update or replacement is needed. Key ARNs need no lookup. Key IDs, alias names, and alias ARNs require `kms:DescribeKey`. Identical concrete values and values deferred until apply need no lookup. Validation and LSP diagnostics remain offline.

Grant `kms:DescribeKey` to the effective identity of each routed provider instance, including its assumed role. The KMS key policy must also authorize the lookup. Cross-account references require authorization in the target account's key policy and the caller's IAM policy. Bare key IDs and aliases use the instance's region and credentials; qualified ARNs retain their own region and account.

This permission is required whenever a lookup is needed, independently of the advisory `--check-iam` option. Access denials, credential failures, transient failures, and invalid references abort planning with exit code 1 and produce no executable saved plan. A missing desired reference is also an error. A current reference that no longer resolves is shown as a difference so that an update can repair it.

Resolving an AWS-managed alias such as `alias/aws/ecr` uses `DescribeKey` directly. On first use, this call can initialize the service's AWS-managed key during planning. There is no alias precheck or special success for an alias that cannot be found.

Resolved key ARNs are comparison-only. Resource payloads and state retain their raw values. A normal `apply` resolves references again when it re-plans; applying a saved plan uses its raw effects under the existing saved-plan contract.
