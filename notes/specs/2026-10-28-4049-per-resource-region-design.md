# Per-Resource Region Selection: Design Study

Tracking: carina-rs/carina#4049

Status: study. Nothing here is implemented. Two findings (P1, P2) were derived
from reading code and have not yet been reproduced with a test; the first
implementation step is to pin them with failing tests.

## Question

To place a resource in a different AWS region, a configuration today declares a
second provider instance and selects it per resource:

```crn
let us = provider aws {
  region = aws.Region.us_east_1
}

let cert = aws.acm.Certificate {
  directives {
    provider = us
  }
  ...
}
```

Terraform's AWS provider v6 added "enhanced region support": one provider
configuration serves many regions, and each resource takes an optional `region`
argument. Should Carina adopt a per-resource region selector, and what would it
take?

## How it works today

- **Syntax.** `let <name> = provider <kind> { ... }` declares a named instance
  (`carina-core/src/parser/let_binding.rs:424-438`). `directives { provider = <name> }`
  selects it; the parser resolves the value statically as a binding name
  (`carina-core/src/parser/blocks/resource.rs:398-405`), and the kind is checked
  in `carina-core/src/parser/resolve.rs:1196-1240`.
- **Instances.** Each binding gets its own WASM instance
  (`carina-plugin-host/src/wasm_factory.rs:2832-2846`, keyed by binding name).
  `ProviderRouter` routes on `(kind, binding)`
  (`carina-core/src/provider.rs:3902-3926`, `4113-4177`).
- **Where the region lives.** Only in the instance configuration, passed once
  through WIT `initialize(attrs)` (`carina-plugin-wit/wit/provider.wit:39`).
  `resource-id` is `{provider, resource-type, name}` (`types.wit:2-6`); there is
  no per-call region.
  - aws reads the region in `initialize`, builds `BoundedSdkConfig::load(region, assume_role)`,
    creates its 10 SDK clients from it, and runs the account guard through STS.
    `aws.s3.Bucket` uses the instance region as `LocationConstraint` and as a read
    value (`carina-provider-aws/.../services/s3/bucket.rs:36,187`).
  - awscc does the same for CloudControl, S3 and STS, and caches the account id
    per instance (`carina-provider-awscc/.../provider/mod.rs:85-145`).
- **Real usage** (`infra/envs/registry/{prod,dev}/serve/providers.crn`):
  - `let us = provider aws { region = us_east_1, default_tags }` places one ACM
    certificate in us-east-1 for CloudFront (`usecases/registry/serve/acm.crn`).
  - `let management = provider aws { allowed_account_ids = [...], assume_role = {...} }`
    writes a Route 53 NS delegation in another account
    (`usecases/registry/serve/route53.crn`). A region cannot express this.

## Terraform's design (for reference)

Source: `website/docs/guides/enhanced-region-support.html.markdown` in
hashicorp/terraform-provider-aws.

- `region` is Optional + Computed and defaults to the provider's region.
  Changing it forces replacement; removing it does not (the state value stays).
- Import IDs take an `@<region>` suffix (`vpc-a01106c2@eu-west-1`).
- Migration: upgrade to v6, `terraform apply -refresh-only`, then replace the
  `provider` meta-argument with `region`. Per-region provider blocks remain
  supported.
- Global services (IAM, Route 53, CloudFront, Organizations, ...) do not take
  `region`, and neither do resources that already have a `region` attribute.
- The guide does not say whether credentials or `assume_role` can vary per
  resource (not verified).

## Problems found

**P1. Routing is part of identity and the state key.** `ResourceId` includes
`provider_instance` (the binding name) in `Hash`/`Eq`
(`carina-core/src/resource/mod.rs:400-404`), and state rows are matched on it
(`carina-state/src/state/mod.rs:922-926`). Changing the routing produces a Create
for the new id and a Delete for the orphaned row; the
`rerouted_replacement_...` test in `state_writeback.rs:4817` assumes this. So
rewriting `directives { provider = us }` as a region selector would plan
delete+create for the production CloudFront certificate. Renaming the binding
`us` has the same effect today. *(Inferred from code; not yet reproduced.)*

**P2. Anonymous identity hashes the wrong region.** The identifier hash takes
the region from the first provider config of that kind
(`providers.find(|p| p.name == provider_name)`,
`carina-core/src/identifier/mod.rs:905-918`, `968-984`), not from the instance
the resource routes to. The collision check (`:1046-1058`) compares identifier
strings only, without `provider_instance`. Two anonymous resources with the
same create-only values, one per region, would fail with "identifier
collision". With per-resource regions this becomes a common case, so the hash
must use the effective (post-routing) region, while resources without an
override must keep today's hash input exactly (otherwise every anonymous
resource changes identity). The two routed resources in infra are both
`let`-bound and are unaffected. *(Inferred from code; not yet reproduced.)*

**P3. Name collision.** `aws.s3.Bucket` (and its data source) already has a
read-only `region` attribute
(`carina-provider-aws/.../schemas/generated/s3/bucket.rs:57`). A top-level
`region =` on resources would collide with it; Terraform special-cases the same
resources. No top-level `region` was found in awscc schemas (many nested
`*_region` fields exist).

**P4. Region does not cover the other instance settings.** `assume_role`,
`allowed_account_ids` / `forbidden_account_ids` and `default_tags` are
instance-level. The `management` instance in infra needs named-instance
selection to stay.

**P5. The selector must be a resolved value.** `directives { provider = ... }`
is a binding name, always known at parse time. A region value can arrive
through module arguments or `upstream_state` as `Value::Unknown`. Routing must
never key on an unresolved value: reject non-literal values and hold the
override only as a resolved newtype.

**P6. `provider = 'us-east-1'` turns a user string into routing identity.**
Naming an instance by a region string makes user-supplied text the routing key,
which the project rules forbid. A region selector should be an override applied
to the selected instance's configuration, not a way to name an instance.

**P7. Global services.** Carina schemas do not mark whether a resource type is
regional. Rejecting a region override on IAM, Route 53, CloudFront, etc. needs a
schema flag carried through both providers' codegen, the schema JSON and the
protocol minimum version.

## Recommendation

Adopt with conditions, as an **addition** to named instances, not a replacement.

1. **Directive, not attribute.** `directives { region = aws.Region.us_east_1 }`.
   It avoids P3 and keeps routing separate from attributes sent to the provider.
   Accept both the DSL enum and the AWS string form and normalize them the same
   way `extract_region_from_attrs` does (`carina-core/src/utils.rs:890`); validate
   against the factory's `provider_config_attribute_types()["region"]`.
2. **Keep named instances.** A region override layers on top of the selected
   instance (default or `directives { provider = ... }`), so `assume_role` and
   account guards keep working (P4).
3. **Host-side derived instances.** Extend the routing key to
   `(binding, Option<RegionOverride>)` as a type, and have the factory create a
   derived instance by re-running `initialize` with only `region` replaced. No
   WIT, protocol, aws or awscc change; the per-region cost is one instance and
   one STS call. A per-call region in WIT is rejected: both providers would have
   to rebuild client caches, the S3 `self.region` handling and the account guard
   per region.
4. **Effective region in anonymous identity** (P2), with the no-override hash
   input unchanged.
5. **Formats.** Record the override in the state row's directives (state version
   bump, currently v18) and bump the saved-plan format (`ResourceId` changes).
6. **LSP parity.** Add `region` to `directives_block_completions`
   (`carina-lsp/src/completion/values.rs:744-750`), reuse
   `config_completions("region")` for values, and add the diagnostic in
   `carina-lsp/src/diagnostics/checks.rs`.
7. **Data sources** route by `provider_instance` too
   (`carina-core/src/resource/data_source.rs`) and get the same key extension.
   Exports are unaffected. Import keeps inheriting routing from the `let`
   binding's directives (`parser/ast.rs:812-835`, `909-922`), so no `@region`
   suffix is needed.

## Open decisions

- **Migration from `directives { provider = us }` to a region override** (P1).
  This is the main decision; without it, the rewrite is delete+create.
  Options:
  - (a) Record the effective region and effective instance settings in the
    state row, and treat rows as the same resource when those match, with the
    matching done at one seam. Open question: `us` and the default instance are
    different bindings even with the same credentials, so "same settings" needs
    a precise, typed definition.
  - (b) Let `moved` express a routing change. This reverses part of the
    `StateBlockAddress` design.
  - (c) A `carina state` subcommand that re-routes rows explicitly.
  Recommendation: (a) if "same settings" can be defined as a typed comparison of
  the resolved instance configuration; otherwise (c). Either way the plan must
  never silently delete+create.
  **Decision: deferred to the implementation.** The feature is additive, so
  existing configs keep working unchanged; the migration path is chosen when
  the implementation starts, with these options as the starting point.
- **Global-service rejection** (P7). Recommended, via a schema `regional` flag;
  it spans codegen and protocol and may be split into its own issue.

## Next steps

1. Pin P1 and P2 with failing tests.
2. Decide the migration option (deferred to this point).
3. Implementation PR for core (routing key, derived instances, identity, formats,
   LSP), then the schema flag if P7 is taken in.
