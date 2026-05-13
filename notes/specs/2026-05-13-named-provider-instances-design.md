# Named Provider Instances: Design

<!-- derived-from ./2026-04-16-upstream-state-dsl-design.md -->

## Goal

Let users declare **multiple instances of the same provider kind** in a single
Carina configuration, distinguished by a name, and route a managed resource
to a specific instance via `directives { provider = <binding> }`.

The immediate driver is `carina-rs/infra` T6c (the registry usecase): AWS
requires CloudFront viewer certificates to live in `us-east-1`, but the
rest of the registry stack lives in `ap-northeast-1`. With only one
`provider aws { ... }` block per stack today, that constraint forces a
manual per-region split that loses the in-stack
`cert.domain_validation_options → route53.RecordSet` wiring.

Closes #2191.

## Non-goals

- Multi-backend support. `backend s3 { ... }` remains a singleton. The
  asymmetry is intentional — see #2183's closing comment for the
  underlying 2-axis model (kind discriminator × named reference).
- Per-resource ad-hoc `provider = aws (region = "us-east-1")` inline
  expressions. Every named instance must be declared up front with a
  binding name; downstream resources reference the binding.
- Multi-kind aliasing (`alias = "us"` Terraform-style). The
  `let us = provider aws { ... }` form is strictly more general
  (regular `let` binding, composes with the rest of the language) and
  the issue's owner endorsed it; the Terraform-style alias was
  considered and rejected as a parallel mechanism.
- Provider plugin contract changes. The WIT interface stays as-is;
  multi-instance routing happens entirely in `carina-plugin-host` /
  `carina-provider-resolver` on top of the existing per-instance
  config payload.
- Multi-instance support for `awscc`, `mock`, or any other kind in a
  separate PR series. This design document is provider-agnostic;
  every kind that loads as a WASM plugin (or in-process) automatically
  inherits multi-instance support once the host wiring lands.

## Background

The current shape:

```crn
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
  region  = aws.Region.ap_northeast_1
}

aws.acm.Certificate { ... }   // implicitly uses the only `aws` instance
```

`provider <kind> { ... }` does double duty: it both **registers** the
kind (binding `source` / `version` / `revision` to the kind name) and
**configures** the singleton instance (`region`, credentials, ...).

That works for one instance per kind. For two, today you have to declare
the kind twice and repeat `source` / `version`:

```crn
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
  region  = aws.Region.ap_northeast_1
}

// Hypothetical second instance — source/version repetition is pure noise:
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
  region  = aws.Region.us_east_1
}
```

Worse, there is currently no syntax to *select* between two same-kind
instances on a resource — `aws.acm.Certificate { ... }` has no way to
say "use the us-east-1 one".

## Direction (confirmed)

#2183 (CLOSED, Direction C) framed the three top-level constructs as a
2-axis model:

|                                  | kind discriminator | no kind discriminator |
| -------------------------------- | ------------------ | --------------------- |
| **singleton / no named ref**     | `provider awscc { ... }`, `backend s3 { ... }` | — |
| **named ref / multiple**         | (this issue)       | `let orgs = upstream_state { ... }` |

The empty cell is "kind discriminator AND named reference". This issue
fills it with:

```crn
let us = provider aws { region = aws.Region.us_east_1 }
```

Following the precedent set by `upstream_state` (right-hand side of a
`let` binding) and `wait` (kind-labelled positional, then block of
attributes).

## DSL syntax

### Kind registration (singleton case — unchanged)

```crn
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
  region  = aws.Region.ap_northeast_1
}

aws.acm.Certificate { ... }            // uses default instance
let cert = aws.acm.Certificate { ... } // uses default instance
```

`provider <kind> { ... }` keeps its current shape and meaning. It
**registers the kind** (`source` / `version` / `revision`) and **declares
a default instance** with whatever other attributes are present. Today's
configurations need no edits.

### Named instance (new)

```crn
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
  region  = aws.Region.ap_northeast_1   // default
}

let us = provider aws {
  region = aws.Region.us_east_1
}

aws.acm.Certificate {
  domain_name = 'registry.carina-rs.dev'
  validation_method = aws.acm.ValidationMethod.Dns
  directives {
    provider = us
  }
}

let cert = aws.acm.Certificate {
  domain_name = 'registry.carina-rs.dev'
  directives {
    provider = us
  }
}
```

`let us = provider aws { ... }` declares a **named instance** of the
already-registered `aws` kind. `region` is an instance-level attribute;
`source` / `version` / `revision` are **not allowed here** — they are
kind-level (see "Errors" below).

### Why the provider reference lives inside `directives { ... }`

Two reasons:

1. **`provider` is Carina-internal, not a provider-API attribute.** The
   value selects which `ProviderInstance` runs the resource — it never
   reaches the AWS / awscc API call. `directives { ... }` is already
   defined (#2826) as the block for **directives to the Carina
   runtime**, and existing entries (`depends_on`,
   `create_before_destroy`, `prevent_destroy`, `force_delete`) all share
   that same property. The provider reference fits the same category.
2. **No attribute-name collision risk with provider schemas.** Resource
   schemas evolve independently in `carina-provider-aws` and
   `carina-provider-awscc`. Reserving `provider` as a top-level
   resource attribute would require every provider to permanently keep
   that key out of its schema. Routing it through `directives`
   sidesteps the contract — `directives { ... }` is a closed Carina
   vocabulary that providers do not see.

The resource's `directives` block is already stripped before the
resource attributes are passed to the provider, so no provider-plugin
change is needed to "filter out" the `provider` key.

### Kind registration with no default instance

If the user wants every resource to pick a named instance explicitly,
they can declare the kind without instance attributes:

```crn
provider aws {
  source  = 'github.com/carina-rs/carina-provider-aws'
  version = '~0.1.0'
}

let tokyo    = provider aws { region = aws.Region.ap_northeast_1 }
let virginia = provider aws { region = aws.Region.us_east_1 }

aws.acm.Certificate {
  domain_name = '...'
  directives { provider = virginia }
}   // OK

aws.s3.Bucket {
  bucket_name = '...'
}   // ERROR: no default
```

A kind block whose only fields are `source` / `version` / `revision`
registers the kind but does **not** materialise a default instance.
Resources whose `directives { ... }` lacks a `provider = <instance>`
key and whose kind has no default instance produce a validation error
(see "Errors").

## Grammar

Two additions to `carina-core/src/parser/carina.pest`:

```
// New: provider expression usable as RHS of `let`.
provider_expr = { "provider" ~ identifier ~ "{" ~ attribute* ~ "}" }

// Update: let_binding accepts provider_expr in the same family as
// upstream_state_expr, wait_expr, use_expr.
let_binding = {
    "let" ~ (discard_pattern | identifier) ~ "="
          ~ (use_expr | upstream_state_expr | wait_expr | provider_expr | expression)
}
```

Note that `provider_expr` and the existing `provider_block` share the
exact same body grammar (`identifier "{" attribute* "}"`). They are
distinguished by position: a top-level statement is a `provider_block`
(kind registration + default instance); the RHS of a `let` is a
`provider_expr` (named instance only). This mirrors the
`upstream_state_expr` / `wait_expr` precedent.

The `provider` reference on a resource is an attribute *inside the
existing `directives { ... }` block*; nothing new at the grammar level.
`directives` is already an arbitrary `attribute*` body, so
`directives { provider = us }` parses today without any rule changes —
all the work is post-parse in the directives-attribute validator and
in the resolver (Phase 3).

## AST / data model

### Today

```rust
pub struct ProviderConfig {
    pub name: String,                                    // kind ("aws")
    pub attributes: IndexMap<String, Value>,             // region, credentials, ...
    pub default_tags: IndexMap<String, Value>,
    pub source: Option<String>,                          // kind-level
    pub version: Option<VersionConstraint>,              // kind-level
    pub revision: Option<String>,                        // kind-level
    pub unresolved_attributes: IndexMap<String, Value>,
}
```

A single struct mixes kind-level fields (`source`, `version`, `revision`)
with instance-level fields (`attributes`, `default_tags`).

### Proposed split

Type-safety-first option (preferred per `feedback_long_term_and_type_safety.md`):
two structs, one per axis.

```rust
/// Kind registration: how to locate and load the provider plugin.
/// One per provider kind across the entire configuration.
pub struct ProviderKind {
    pub name: String,                              // "aws", "awscc"
    pub source: Option<String>,
    pub version: Option<VersionConstraint>,
    pub revision: Option<String>,
}

/// Provider instance: configured runtime context for `Provider::create`/`read`/...
/// Zero, one, or many per kind.
pub struct ProviderInstance {
    /// Binding name. For the default instance this is the kind name
    /// ("aws"); for named instances this is the `let` binding's
    /// identifier ("us", "tokyo", ...).
    pub binding: String,
    /// Which kind this instance configures.
    pub kind: String,
    /// Whether this instance is the kind's default (sourced from the
    /// kind's own `provider <kind> { ... }` block when that block
    /// carried instance attributes, vs declared via `let x = provider
    /// <kind> { ... }`).
    pub is_default: bool,
    pub attributes: IndexMap<String, Value>,
    pub default_tags: IndexMap<String, Value>,
    pub unresolved_attributes: IndexMap<String, Value>,
}
```

`File` gains a parallel pair of fields:

```rust
pub struct File<E> {
    // existing fields ...
    pub provider_kinds: Vec<ProviderKind>,
    pub provider_instances: Vec<ProviderInstance>,
    // The old `providers: Vec<ProviderConfig>` is removed.
}
```

### Why split (and not "one struct with `is_kind_registration: bool`")

The split lets the compiler enforce two invariants that the current
single-struct shape can only enforce at runtime:

1. **`source` / `version` / `revision` are kind-level.** They cannot
   appear on a `ProviderInstance`. The parser is the only place that
   has to reject the wrong shape; every downstream consumer (resolver,
   host, differ, state writer) operates on the already-split types and
   cannot accidentally read `source` from an instance.

2. **A resource references a `ProviderInstance`, never a
   `ProviderKind`.** Resolution from `directives { provider = us }` to
   the actual instance is typed:
   `Resource.directives.provider_instance: Option<InstanceRef>` where
   `InstanceRef` resolves to a `&ProviderInstance`. The plugin host
   loads `N` instances per kind and routes by instance.

### Resource side

The existing `Directives` struct gains a new field. It joins
`depends_on`, `create_before_destroy`, etc. — all Carina-runtime
directives, none of which are passed to the provider plugin.

```rust
pub struct Directives {
    // existing fields ...
    /// Binding name of the provider instance to route this resource
    /// to. `None` means "default instance for the resource's kind".
    /// Resolved to a concrete `ProviderInstance` during the parser's
    /// post-resolve phase.
    pub provider_instance: Option<String>,
}
```

The DSL surface `directives { provider = <ident> }` is captured as a
string at parse time (same as any binding reference) and resolved
alongside other forward-references in the resolver pass. From the
parser's perspective it is just another known key on the directives
attribute set.

## Resolution rules

| Resource shape | Effective instance |
| -------------- | ------------------ |
| `aws.acm.Certificate { ... }` with no `directives.provider` | The default instance for kind `aws` (the instance materialised by the top-level `provider aws { ... }` block, if any). |
| `aws.acm.Certificate { ..., directives { provider = us } }` | The instance bound to `us`. Its kind must match the resource's kind (`aws`). |
| Kind has no default instance and resource omits `directives.provider` | Validation error: `aws.acm.Certificate requires an explicit directives { provider = <instance> }; kind 'aws' has no default instance`. |
| `directives { provider = <binding> }` where binding is not a provider instance | Validation error: `'<binding>' is not a provider instance`. |
| `directives { provider = <binding> }` where binding's kind ≠ resource's kind | Validation error: `provider instance '<binding>' has kind '<X>', not '<Y>'`. |

## Plugin host changes

`carina-plugin-host` currently loads one Component per (kind, source,
version) tuple and stores per-kind state. The minimum change:

- The host's loader is keyed on `(kind, source, version, revision)` —
  one **Component** per kind (the WASM binary doesn't change between
  instances).
- A separate per-instance **store** holds the configured `Provider`
  handle for each `ProviderInstance`. Provider operations (`create`,
  `read`, `update`, `delete`, `plan`) are routed through the store
  identified by the resource's resolved `provider_instance`.

This is identical to how today's single-instance path works
conceptually; the only change is that the kind→store mapping becomes a
kind→{binding→store} mapping. No WIT contract change is required.

## Differ / Plan / Effect

Effects already carry the resource they target. The resolved
`ProviderInstance` binding is part of the resource's identity for
execution routing purposes. The differ does not need to change beyond
emitting the right instance reference on each Effect; the executor uses
that reference to pick the right per-instance store.

A new diff case: if a resource's `provider_instance` changes between
plans (`tokyo` → `us`), the diff is conservatively a destroy-and-create
because state stored under one provider instance is not portable to
another (different region in the ACM case → different ARN entirely).

## State schema

State v3 already records each resource by its `ResourceId`. The state
file gains one optional field per resource entry:

```json
{
  "provider_instance": "us"
}
```

When absent (which is how today's state files read), the resource is
assumed to belong to the kind's default instance. Per
`feedback_no_backward_compat.md` we add the field unconditionally on
write and tolerate the absent case on read; nothing called "migration"
is needed and the project's policy forbids using that word in code or
docs.

## LSP

- **Completion**: inside `directives { ... }` on a resource of kind `X`,
  `provider = ` completes every `ProviderInstance` whose kind is `X`.
  `provider` itself is also a completion candidate as a directives key
  once any named instances are declared.
- **Semantic tokens**: `provider` in `let us = provider aws { ... }`
  gets the same token type as `provider` in the top-level statement.
  Inside `directives { ... }` the `provider` key is a directives keyword
  (same token class as `depends_on`).
- **Diagnostics**: every resolution-rule error in the table above
  surfaces as an LSP diagnostic. `source` / `version` / `revision` in a
  `let x = provider <kind> { ... }` body produces a "field not allowed
  on named instance — declare it on the top-level `provider <kind>`
  block instead" diagnostic.

## Validation parity

Per `feedback_validate_lsp_parity.md`: every diagnostic listed for the
LSP must also fire from `carina validate` (and vice versa). Both
consume `carina-core::diagnostics`; no separate code path.

## Errors (parser / validator)

1. **`source` / `version` / `revision` on a named instance.**
   ```
   error: 'source' is a kind-level attribute and cannot be set on a
          named provider instance. Move it to `provider aws { ... }`.
     --> example.crn:4:3
       | let us = provider aws {
     4 |   source = '...'
       |   ^^^^^^
   ```
2. **Two `let`s with the same name binding a provider instance** — already
   caught by the existing `let` binding uniqueness check; no new rule.
3. **Kind not registered.** `let us = provider aws { ... }` with no
   top-level `provider aws { ... }` block:
   ```
   error: provider kind 'aws' is not registered. Add a top-level
          `provider aws { source = ..., version = ... }` block.
   ```
4. **Default-instance and named-instance attribute disagreement.** Not
   an error — named instances are independent configurations, they do
   not inherit from the default. Two instances of `aws` with the same
   `region` are valid (and a no-op cost in practice — same Component,
   two stores).

## Real-infra acceptance

The original blocker is `carina-rs/infra` T6c. The smoke test for this
issue is:

1. Write the `us-east-1` named instance into the registry stack.
2. `aws-vault exec ... carina plan ./registry/dev/registry/` succeeds
   without parser errors at the `directives { provider = us }` site.
3. The plan correctly schedules `aws.acm.Certificate` against the
   `us-east-1` AWS endpoint and the validation `route53.RecordSet`
   against `ap-northeast-1`.
4. Apply succeeds; the cert reaches `ISSUED`.

Per `feedback_no_real_infra_aws_commands.md` the real-AWS run is the
user's call, not mine. The smoke is "design supports it; unit / fixture
tests cover the wiring".

## Phasing

Implementation is split into phases by layer. Each phase is its own PR
in the order listed. Per `feedback_design_before_implementation_in_pr.md`,
this design PR merges before any implementation PR opens.

1. **Phase 1 — AST split.** Introduce `ProviderKind` /
   `ProviderInstance`, migrate today's single-instance code path
   (every existing `provider aws { ... }` becomes one
   `ProviderKind` + one default `ProviderInstance`). No DSL change yet;
   everything still parses as before. Acceptance: workspace nextest
   green, real-infra `carina plan` byte-identical.
2. **Phase 2 — Grammar + parser for `let x = provider <kind> { ... }`.**
   The parser accepts the new form. `ProviderConfig` gains the
   `binding` / `is_default` fields needed to distinguish kind-default
   from named instances. `Directives.provider_instance` field added but
   always `None` until phase 3. Acceptance: parser tests covering both
   forms (top-level kind block, `let` named instance); multi-file
   directory fixture; ProviderConfig roundtrip preserves the new fields.

   *Implementation note (chosen after design PR):* Phase 1 ("AST split
   to `ProviderKind` + `ProviderInstance`") is merged into Phase 3
   instead of running first. The split's invariants ("`source` only on
   kinds, `Resource` only references `ProviderInstance`") are
   load-bearing only once Phase 3 lands; pre-splitting them is 17-file
   churn that adds zip/re-merge boilerplate downstream without paying
   off until later. Phase 2 adds two narrow fields to `ProviderConfig`
   that Phase 3 will replace by the full split in one move.
3. **Phase 3 — `directives.provider` resolution + AST split.** The
   resolver reads `directives { provider = <ident> }` and binds each
   resource to a concrete `ProviderInstance`. At the same time,
   `ProviderConfig` is split into `ProviderKind` + `ProviderInstance`
   (the invariants become load-bearing here: a resource references an
   instance, not a kind; `source` lives only on kinds). The differ +
   executor consume the resolved instance reference. Acceptance:
   existing real-infra unchanged (every resource resolves to the
   default instance); a new fixture under
   `carina-cli/tests/fixtures/plan_display/` exercises the
   multi-instance path.
4. **Phase 4 — Plugin host multi-instance store.** `carina-plugin-host`
   manages per-instance stores. Acceptance: integration test loads two
   instances of the mock provider with different configs and verifies
   the operations route correctly.
5. **Phase 5 — LSP + validate parity.** Completion + diagnostics + error
   messages. Acceptance: parity test (validate diagnostic ↔ LSP
   diagnostic) for every error in the "Errors" section.
6. **Phase 6 — State schema field + docs.** Add `provider_instance` to
   state v3 writer/reader, document the new construct in
   `docs/reference/dsl/syntax.md`. Acceptance: real-infra apply against
   T6c succeeds end-to-end.

## Open questions

1. **How does `directives { provider = <ident> }` interact with
   `directives { depends_on = [...] }`?** Both live in the same block
   today. Implicit dependency edge: a resource depends on its provider
   instance's binding. The current code path treats the instance
   binding as a normal `let` binding; this should just work, but
   Phase 4 needs an explicit test.
2. **Should `let x = provider <kind> { ... }` allow inheriting attributes
   from the kind's default instance?** MVP says no — each instance is a
   complete configuration. Inheritance can be added later
   non-breakingly via an explicit attribute (e.g. `inherit = aws`) if
   real configurations duplicate too much.
3. **Module boundaries.** A child module declares its own
   `provider aws { ... }` today. With named instances, can a parent
   module pass `provider = tokyo` into a child module? Out of MVP
   scope; tracked as a follow-up issue if T6d/T6e need it.
4. **Default-tag inheritance across instances.** Today
   `default_tags` is per-`provider <kind>` block. Per-instance
   `default_tags` is supported by the data model split (each instance
   has its own field); MVP behaviour is: each instance has its own
   independent `default_tags`. No merging across instances.

## Related

- #2191 — this issue.
- #2183 — parent (CLOSED, Direction C); supplies the 2-axis model.
- #2826 — `directives` block design; this proposal extends its
  vocabulary with the `provider` key.
- #2825 / `wait` construct — precedent for "kind-labelled positional then
  attribute block as RHS of `let`".
- `carina-rs/infra` T6c / T6d / T6e — concrete consumer; blocked on this
  landing.
