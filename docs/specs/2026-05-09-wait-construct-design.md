# `wait` Construct: Design

<!-- constrained-by ./2026-05-09-wait-construct-design.md#out-of-scope -->

## Goal

Introduce a `wait` right-hand-side expression to the Carina DSL so users can declare "block downstream resources until target reaches a desired state" — most immediately, ACM `Certificate.Status == ISSUED` for the carina-rs/infra registry usecase (T6), but generalisable to any "create-then-poll-until-ready" pattern across AWS / awscc / future providers (RDS Available, Lambda Active, EC2 Running, MSK Active, etc.).

This is the long-form output of a brainstorming session that started from `carina-rs/carina-provider-aws#244` (ACM Certificate + CertificateValidation) and concluded that the most type-safe, future-proof, philosophically consistent way to express "waiting" is a first-class DSL construct rather than a per-provider waiter resource.

## Non-goals

- Implementing ACM Certificate / CertificateValidation themselves. That work happens on top of this construct, tracked separately on the aws side as the consumer.
- Adding `time_sleep` / "just sleep N seconds" semantics. The `wait` construct is for "wait for a condition on a target", not raw sleeps. Pure sleep deserves its own `sleep { duration = ... }` construct if needed.
- Adding `fail_when` (early-fail on known-bad states). MVP relies on `timeout` to surface "didn't reach the desired state". Can be added later without breaking change.
- Exposing `interval` (poll cadence) to the user. MVP keeps it provider-internal. Can be added later.
- Persisting wait results across plan/apply runs. The `wait` construct does not write to the state file; every plan/apply re-evaluates.
- Provider-side `wait()` API. MVP runs the polling in the executor on top of the existing `read()` trait method, so providers (including WASM plugins) need no contract change.

## Why `wait` belongs in carina-core, not in each provider

The brainstorming surfaced and rejected six alternatives:

| Approach | Why rejected |
|---|---|
| **B**: Bake polling into `aws.acm.Certificate.create()` | Cyclic dependency — the validation DNS record needs `cert.domain_validation_options` (only known after `RequestCertificate`), so the cert cannot wait on the record before its own create returns. Same problem applies to **G** ("`Certificate` = `ISSUED` guarantee"). |
| **A**: Per-provider `aws.acm.CertificateValidation` waiter resource (Terraform style) | Works, but multiplies into a class of `~Validation` / `~Ready` resources across services (RDS Available, Lambda Active, ...). Carries three runtime-escape hacks (no-op `delete()`, dummy state-row id, every attribute `ForceNew`) that violate the project's "type safety over runtime checks" rule. |
| **C-2**: `lifecycle { wait_for = ... }` on the downstream resource | Spreads the wait responsibility across every consumer of the cert (Distribution, ALB Listener, ...), duplicating the predicate; also requires lifecycle-block expression evaluation to dereference *another* resource's attributes. |
| **H**: New `assert {...}` top-level statement | Conflates "must be true at point in time" with "wait until true", which are different operations. |
| **I**: `cert.arn when cert.status == ISSUED` reference modifier | Cute but unsalvageable: the same cert may be referenced from multiple downstream resources, each of which needs the same wait — duplicating the `when` expression. |

The chosen approach (**X'** in the brainstorming) — `let cert_issued = wait cert { until = ..., depends_on = [...], timeout = ... }` — sidesteps every one of those problems:

- Cert and validation record can be authored independently (cert is "request-and-return", record references the cert's `domain_validation_options`).
- `wait` is a single declaration that expresses the synchronization contract once; multiple downstream resources reference `cert_issued.arn` to inherit the wait.
- No per-provider waiter resource class needed.
- No runtime hacks (no fake state rows, no no-op deletes).
- The construct is provider-agnostic and reusable across the entire AWS / awscc surface.

## DSL syntax

```crn
let cert = aws.acm.Certificate {
    domain_name       = "registry.carina-rs.dev"
    validation_method = "DNS"
}

let validation_record = aws.route53.RecordSet {
    hosted_zone_id   = zone.id
    name             = cert.domain_validation_options[0].resource_record_name
    type             = cert.domain_validation_options[0].resource_record_type
    ttl              = 60
    resource_records = [cert.domain_validation_options[0].resource_record_value]
}

let cert_issued = wait cert {
    until      = cert.status == aws.acm.Certificate.Status.Issued
    depends_on = [validation_record]
    timeout    = 75min
}

let dist = aws.cloudfront.Distribution {
    viewer_certificate {
        acm_certificate_arn      = cert_issued.arn
        ssl_support_method       = "sni-only"
        minimum_protocol_version = "TLSv1.2_2021"
    }
    # ... origin / behaviors etc
}
```

### Grammar

```
let_binding   = "let" identifier "=" (resource_expr | upstream_state_expr | wait_expr | ...)

wait_expr     = "wait" target_ref "{" wait_attr* "}"
target_ref    = identifier                    # binding name of the target resource
wait_attr     = until_attr
              | depends_on_attr               # provided by the carina depends_on extension
              | timeout_attr

until_attr    = "until"   "=" wait_predicate
timeout_attr  = "timeout" "=" duration_literal
```

`wait_predicate` reuses the existing `validate_expr` grammar (already in `carina.pest`: `validate_or_expr` / `validate_and_expr` / `validate_comparison`) so the parser can short-cut to a known production. MVP only enforces the `<binding>.<attr> == <enum_or_literal>` shape via a post-parse type check; the grammar accepts the wider validate-expr surface and incrementally supports more operators (`!=`, `&&`, `||`, `>=`, `in [...]`) as later issues land.

`duration_literal` is a new lexical token — see "Duration type" below.

### Reusing the right-hand-side family

Carina already has multiple "right-hand sides of a `let`":

- `aws.s3.Bucket { ... }` — managed resource declaration
- `module.web_tier { ... }` — module instantiation
- `upstream_state { ... }` — external state reference

`wait <target> { ... }` is added to this family. The choice — `let cert_issued = wait cert { ... }` instead of `wait cert_issued { ... }` (Y in the brainstorming) — preserves Carina's "every binding is `let`" invariant and matches the precedent set by `upstream_state { ... }`.

### `target` as a positional argument

`wait <target> { ... }` puts the target binding name in the keyword's positional slot rather than as an in-block `target = cert` field. Two reasons:

1. The reader sees what is being waited on at the top of the line, before any block contents.
2. `target` is the wait's primary subject; everything in the block configures *how* the wait behaves. Promoting subject to positional matches that hierarchy.

Consequence for the parser: `wait_expr` accepts exactly one positional `identifier` after `wait` and before `{`. This is a new positional pattern in Carina's right-hand sides, but a small one — it does not generalise into a "every keyword can take positional arguments" rule.

### Value semantics

`<wait-binding>.<attr>` resolves to **`<target>.<attr>`** (passthrough), with the lifecycle constraint that the value is not available to downstream resources until the wait completes successfully. Same idiom as Terraform's `aws_acm_certificate_validation.example.certificate_arn` — an indirection through the wait gives downstream consumers an implicit "waited-on" version of the underlying value.

This means:

- `cert_issued.arn` has the same type and content as `cert.arn` (the certificate ARN is a string regardless of validation status); the difference is purely the dependency edge in the execution graph.
- Downstream resources that don't care about the wait (e.g. an audit-log resource that just records the cert ARN) can reference `cert.arn` directly and skip the wait. Downstream resources that need the cert to actually be `ISSUED` reference `cert_issued.arn` and inherit the synchronisation.
- All attributes of the target are accessible: `cert_issued.domain_name`, `cert_issued.subject_alternative_names`, etc. Reading from the wait binding returns the snapshot of the target captured by the read() that satisfied `until`.

## Block fields

| Field | Required? | Type | Default | Description |
|---|---|---|---|---|
| `until` | Yes | typed predicate | — | Evaluated against each `read()` of `target`; wait completes when this returns `true`. |
| `depends_on` | No | list of bindings | `[]` | Bindings that must complete before the wait starts polling. (Carina-wide meta-arg; see dependency.) |
| `timeout` | No | `duration` | from target's schema | Hard cap on total wait time. Exceeding it returns `ProviderError::Timeout`. |

### `until` — typed predicate

MVP grammar accepts `<target>.<attr> == <value>` only:

```crn
until = cert.status == aws.acm.Certificate.Status.Issued
until = instance.state == aws.ec2.Instance.State.Running
until = func.state == aws.lambda.Function.State.Active
```

Type-checking rules (enforced post-parse, surfaceable via LSP diagnostics):

- The left-hand side must reference an attribute of the wait's `target` (cross-target predicates are out of scope for MVP). `cert.status` where `cert` is the target binding ✓; `other_cert.status` ✗.
- The attribute must exist in the target's schema. Typo on attribute name → diagnostic.
- The right-hand side must be type-compatible with the attribute's declared type. For enum attributes (the dominant case), the RHS must be a namespaced enum value (`aws.acm.Certificate.Status.Issued`).

Future extensions (out of MVP, no breakage required):
- `!=`
- Boolean combinators `&&` / `||`
- Numeric comparisons `>=` / `<=` / `>` / `<` (for "completed_steps >= 10" style)
- `in [...]` for "any of these states"

The `validate_expr` grammar that Carina already uses for `arguments { validation { condition = ... } }` covers all of these shapes; reusing it amortises grammar work.

### `depends_on` — provided by separate extension

`depends_on = [<binding>, ...]` declares additional ordering edges that aren't expressed via value references. ACM Validation needs this because the wait references the *cert*, not the *validation record*, but cannot start until the record is published.

`depends_on` is **not** a wait-specific feature. It belongs as a Carina-wide meta-arg available on every `let` binding (resource, wait, future others). The wait construct depends on it but does not introduce it. See "Dependencies on other Carina extensions" below.

### `timeout` — duration with schema-provided default

`timeout` is optional. When omitted, the executor uses the default declared on the target resource's schema (`AwsSchemaConfig::default_wait_timeout` or equivalent — see "Schema additions" below). Each provider/resource sets a sensible default (ACM Certificate: 75 minutes, matching Terraform's `aws_acm_certificate_validation` default; EC2 Instance: 5 minutes; etc.).

Exceeding `timeout` produces `ProviderError::Timeout` whose message includes:

- The wait binding name (`cert_issued`)
- The unmet predicate (`cert.status == ISSUED`)
- The last observed value (`cert.status = PENDING_VALIDATION`)
- The elapsed time

## Duration type

Carina has no `Duration` literal today. This proposal introduces one as a precondition; the wait construct depends on it.

### Lexical syntax

```
duration_literal = integer_literal duration_unit
duration_unit    = "s" | "sec" | "second" | "seconds"
                 | "m" | "min" | "minute" | "minutes"
                 | "h" | "hr"  | "hour"   | "hours"
```

Examples: `30s`, `5min`, `1h`, `75min`, `30sec`, `2hours`.

Compound forms (`1h30m`) are deferred until a real use case appears; the MVP supports a single `<integer><unit>` only. ACM's 75-minute window and every reasonable provider default fit comfortably inside that.

### Type

A new first-class `Duration` type in carina-core (likely a thin wrapper around `std::time::Duration`). DSL-side: an attribute typed as `Duration` requires a `duration_literal` value; trying to assign an `Int` or `String` is a type error.

### Reuse beyond `wait`

Once introduced, `Duration` is naturally usable for:

- Future `lifecycle { create_timeout = ..., delete_timeout = ... }` extensions
- TTL-typed attributes (Route 53 record TTL, CloudWatch metric retention, etc.) — currently typed as `Int seconds`
- Retry / backoff configuration on resources that need it

The wait construct is the first use site, but the type is broadly applicable.

## Effect model

### New variant

```rust
pub enum Effect {
    Create { ... },
    Update { ... },
    Replace { ... },
    Delete { ... },
    Read { ... },
    Import { ... },
    Remove { ... },
    Move { ... },
    Wait {
        binding: String,                 // e.g., "cert_issued"
        target_id: ResourceId,           // resolved from `wait cert` → cert's id
        target_identifier: Option<String>,
        until: WaitPredicate,            // typed predicate AST
        timeout: Duration,
        interval: Duration,              // resolved from schema default; not user-visible
        depends_on: Vec<EffectIdx>,      // populated by planner, not by user
    },
}
```

`WaitPredicate` is a typed AST for the supported predicate shapes. Initial enum:

```rust
pub enum WaitPredicate {
    Equals { attr: AttrPath, value: Value },
    // future: NotEquals, And, Or, Comparison, In, ...
}
```

`AttrPath` supports nested fields (`renewal_summary.renewal_status`) so future use cases that need to dig into struct attributes work without re-parsing.

### Executor logic

The `Wait` effect is dispatched by carina-core's executor (not by the provider). The executor:

1. Waits for `depends_on` effects (target's `Create`/`Update`, plus any user-declared additional bindings) to complete.
2. Loops:
   1. `provider.read(target_id, target_identifier).await?`
   2. Evaluate `until` against the returned `State.attributes`.
   3. If true → success, capture the snapshot for downstream resolution.
   4. If false → check elapsed time:
      - If `>= timeout` → `Err(ProviderError::Timeout { ... })`.
      - Else → `tokio::time::sleep(interval).await; continue;`.

The provider sees only ordinary `read()` calls; nothing in the WIT contract or the `Provider` trait changes. WASM plugins automatically support being waited on by virtue of implementing `read()`.

### Downstream value resolution

When a downstream effect references `<wait-binding>.<attr>`, the executor's binding resolution layer treats `<wait-binding>` as resolving to the State snapshot captured at wait-completion time. This is the same machinery that resolves `<resource-binding>.<attr>` → post-create State; we just register the wait's captured snapshot in the same map under the wait's binding name.

Failure semantics: if the wait errors (timeout), the wait binding never gets registered, so any downstream effect that references it surfaces the standard "dependency failed" skip behaviour already implemented in the executor (`failed_bindings` set, dependency-aware skip).

## State file

`Wait` effects do **not** write to `carina.state.json`. They are evaluated fresh on every plan/apply:

- If the target already satisfies `until`, the wait completes in one `read()` (typically sub-second).
- If the target does not satisfy `until`, the wait either eventually succeeds (within `timeout`) or fails.

Rationale: a wait represents a synchronisation contract, not a managed object. There is no "waited" or "unwaited" state to persist; the source of truth is the current state of the target, which is itself either persisted (for managed resources) or re-read (for data sources).

This avoids:

- "Wait drift" — a previously-satisfied wait whose target has since fallen out of `until`.
- The state file growing with synthetic rows for every wait.
- The `delete()` semantic question for "synthetic" resources (which is what made approach **A** awkward).

## Plan display

```
+ aws.acm.Certificate.cert
+ aws.route53.RecordSet.validation_record
> cert_issued (until cert.status == aws.acm.Certificate.Status.Issued)
+ aws.cloudfront.Distribution.dist
```

Format follows the existing one-line-per-effect convention in `carina-core/src/plan.rs:format_effect_brief`:

- `> <binding-name> (until <predicate-stringified>)`
- ASCII single-character marker `>`, consistent with other markers (`+`, `~`, `+/-`, `<=`, `<-`, `x`, `->`).
- No emoji, no multi-line block — keeps `carina plan` grep-friendly and snapshot-test-stable.
- `timeout` is omitted when at the schema default; printed when overridden.
- Predicate is rendered using its surface form (`cert.status == aws.acm.Certificate.Status.Issued`), not the parsed AST.

During `carina apply`, the same line gets a progress annotation when actively polling (`> cert_issued ... [waited 12s]`); details left to the apply UI implementation, not load-bearing for the design.

## Dependencies on other Carina extensions

This proposal is non-trivially layered. Three independent Carina-core changes need to land in order:

| Order | Carina extension | Purpose | Standalone value |
|---|---|---|---|
| 1 | `Duration` type + `<integer><unit>` literal | Concrete syntax for `timeout = 75min` | Yes — useful for any resource attribute currently typed `Int seconds` |
| 2 | `depends_on` meta-arg on `let` bindings | Express ordering not captured by value references | Yes — Terraform parity, useful for resources that interact via side effects |
| 3 | `wait` construct (this design) | Block downstream until target reaches a condition | Builds on 1 + 2 |

Each is a separate Carina RFC / issue. The wait construct is the third; the prior two are prerequisites with their own design rationale (and standalone utility, so they're not "subsidiaries" of the wait work).

The `aws.acm.Certificate` consumer issue (carina-rs/carina-provider-aws#244) sits on top of the third. ACM's `CertificateValidation` is no longer a separate awscc-or-aws-side resource; it is expressed entirely as `wait` against the existing `aws.acm.Certificate`.

## Schema additions

To support per-resource defaults for `wait`'s `timeout` and `interval`, the existing `AwsSchemaConfig` (and the awscc equivalent) gains:

```rust
pub struct AwsSchemaConfig {
    // ... existing fields
    pub default_wait_timeout: Option<Duration>,   // default: None → carina-core fallback (e.g. 5min)
    pub default_wait_interval: Option<Duration>,  // default: None → carina-core fallback (e.g. 5s)
}
```

Codegen-generated resource configs populate these from `ResourceDef`-side metadata:

```rust
ResourceDef {
    name: "acm.Certificate",
    // ...
    default_wait_timeout: Some(Duration::from_secs(75 * 60)),
    default_wait_interval: Some(Duration::from_secs(5)),
}
```

For the MVP, only resources that need a non-default wait declare these fields. Carina-core falls back to fixed defaults when both are `None`.

The "派生 A" (one timeout per resource) shape from the brainstorming is preserved; "per-state-transition" timeouts (派生 B) are out of MVP scope and can be added later by extending the schema field to a map keyed by predicate signature.

## LSP / formatter / diagnostics

| Component | Change required |
|---|---|
| `carina-lsp/src/completion/top_level.rs` | Add `let <name> = wait` as a snippet completion. |
| `carina-lsp/src/completion/values.rs` | Inside a `wait <target> { ... }` block: complete `until`, `depends_on`, `timeout` as block-level keys; complete `<target>.<attr>` for the LHS of `until`; complete enum values for the RHS. |
| `carina-lsp/src/semantic_tokens.rs` | Highlight `wait` and `until` as keywords; highlight duration literals as numeric. |
| `carina-lsp/src/diagnostics/` | Diagnose: target not found, attribute not in target schema, type mismatch in `until`, unsupported operator (anything beyond `==` in MVP), missing `until`, invalid duration. |
| Formatter | Format `wait` blocks consistently with existing `let foo = aws.... { ... }` blocks. |
| TextMate grammars | Add `wait`, `until`, `depends_on`, `timeout`, and duration literal patterns to both `editors/vscode/syntaxes/carina.tmLanguage.json` and `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` (parity test enforced by `carina-core/tests/tmlanguage_keyword_parity.rs`). |

## Edge cases and constraints

### Wait on a wait

```crn
let cert_issued        = wait cert { until = cert.status == ISSUED }
let cert_issued_strict = wait cert_issued { until = cert_issued.signature_algorithm == "SHA256WITHRSA" }
```

Allowed in principle (target can be any binding, including another wait), but no use case yet. MVP allows it via the existing binding lookup; it falls out for free from the value-semantics rule (`cert_issued`'s value = `cert`'s value, so `cert_issued.signature_algorithm` = `cert.signature_algorithm`).

### Self-referential `until`

```crn
let foo = wait cert { until = foo.something == ... }
```

A `until` predicate that references the wait's own binding (rather than the target) is a parse-time error: the wait binding is not in scope inside its own `until` (analogous to `let x = x + 1` — disallowed in most languages and not useful here).

### Wait without `depends_on`

```crn
let cert_issued = wait cert { until = cert.status == ISSUED }
```

Legal. The wait starts as soon as the target's `Create`/`Update` completes. ACM's case happens to require `depends_on = [validation_record]` because the validation record must exist for `until` to ever become true; for resources where `until` becomes true purely from the target's own create (e.g. EC2 Instance reaching `Running` after `RunInstances`), `depends_on` is unnecessary.

### `read()` returns `not_found`

If the target's read returns `State::not_found` mid-poll (e.g. someone deleted the cert out-of-band), the wait fails immediately with a distinct error variant (`ProviderError::NotFound { ... }`), not after `timeout`. This mirrors how regular reads handle drift.

### Wait against a data source

Allowed in principle — the executor calls whichever read API the target uses (`provider.read` for managed resources, `provider.read_data_source` for data sources). No special handling needed beyond looking up the target's binding kind.

### Wait inside a module

A `wait` declared inside a module behaves identically to a wait at the root: it has its own binding name (scoped to the module), and downstream references work via the standard module-resolver paths. Module exports can include wait bindings — exporting a wait is equivalent to exporting its target reference plus the guarantee that the target satisfies the wait's predicate before the export resolves. Nothing special in the design; falls out of treating waits as ordinary `let` bindings.

### Anonymous waits

Carina has the concept of anonymous resources (`aws.s3.Bucket { ... }` without `let`); does an anonymous wait make sense?

```crn
wait cert { until = cert.status == ISSUED }   # no `let`, no binding name
```

Use case: the user wants to gate apply on the target reaching a state but doesn't need to reference the wait's value from anywhere else. MVP: **disallowed**. Anonymous resources get an auto-generated identity from their attributes (typically `name`); a wait has no `name` attribute and no AWS-side identity, so the auto-id machinery doesn't apply. If a user genuinely wants a "fire and forget" wait, they can write `let _ = wait cert { ... }` (binding-name = `_` is the existing discard pattern, see `let_binding = { "let" ~ (discard_pattern | identifier) ~ "=" ~ ... }` in `carina.pest`).

## Out of scope (for MVP, deferred to follow-up)

- `fail_when` for early-fail on known-bad states.
- `interval` exposed to user.
- `on_timeout = "warn" | "skip"` modes (only "error" supported in MVP).
- Compound duration literals (`1h30m`).
- Predicate operators beyond `==`.
- Cross-target predicates (`until = other_resource.attr == ...` where target is something else).
- Per-state-transition timeouts (different timeout for `Issued` vs `Failed`).
- Provider-specific native wait implementations (every wait runs through executor + `read()` polling).
- Persisting wait results across runs (always re-evaluated).
- `time_sleep`-equivalent pure-sleep construct.

## Acceptance criteria

The `wait` construct is considered "done" for MVP when:

1. `let foo = wait <target> { until = <==-predicate>, depends_on = [...], timeout = <duration> }` parses cleanly across `carina validate` and the LSP, with diagnostics for missing `until`, unknown target, type-mismatched predicate.
2. `<wait-binding>.<attr>` resolves correctly in downstream resources (passthrough of target).
3. Plan output displays `> <binding> (until <predicate>)` per the format above; snapshot tests cover at least one fixture (`carina-cli/tests/fixtures/plan_display/wait_cert/`).
4. Apply executes the wait by polling `read()` at the schema-declared interval, satisfies `until`, and unblocks downstream effects.
5. Apply on a wait that fails to satisfy within `timeout` returns `ProviderError::Timeout` with a message including the unmet predicate and last observed attribute value.
6. State file (`carina.state.json`) contains no entries for wait bindings.
7. The `aws.acm.Certificate` + Route53 record + `wait` end-to-end pattern works against real AWS in the registry usecase (carina-rs/infra T6).

## Risks

- **Predicate evaluation as a long-term language feature.** Starting with `==` is conservative, but every predicate operator added later (`>=`, `&&`, `in`) becomes a language-level commitment. Mitigation: reuse `validate_expr` so we're not inventing a new expression evaluator; cap MVP at `==` and require an explicit RFC for each new operator.
- **Default timeouts as a per-resource curated dataset.** Codegen needs a place to express "ACM Certificate default = 75min, EC2 Instance default = 5min, ...". Wrong defaults would surface as nuisance timeouts. Mitigation: only declare defaults for resources that have a known wait pattern; fall back to a conservative carina-core default (e.g. 5min) otherwise; document and review per-resource defaults in `ResourceDef` review.
- **`read()` polling cost at scale.** A workspace with many waits in flight (e.g. 50 EC2 instances, each waited on) generates 50 × `DescribeInstances` calls per interval. AWS API rate limits could be hit. Mitigation: this concern is real but out of MVP scope; future work could batch reads (single `DescribeInstances` covering all 50) but that requires a provider-side capability beyond `read()`. For MVP, document that high-fan-out waits should use longer intervals; revisit when a real workload hits limits.
- **`Duration` type sneaks into resource schemas before users are ready.** The Duration extension is independent of wait; once it lands, users can use `30s` anywhere a Duration is expected. If we later realise a different lexical form is preferred (e.g. ISO 8601 `PT75M`), changing the literal is a breaking change to every `.crn` file using it. Mitigation: Carina has explicit "no backward compat" policy (project memory), so a future swap is permitted but not free. Settle the literal form via this design doc and don't revisit lightly.

## Related work

- carina-rs/carina-provider-aws#244 — the consumer issue (ACM Certificate + DNS validation for the registry usecase). After this design lands, `#244` becomes "implement `aws.acm.Certificate` + a `wait` example demonstrating DNS validation".
- carina-rs/carina#TBD-A — `depends_on` meta-arg RFC (prerequisite).
- carina-rs/carina#TBD-B — `Duration` type and literal RFC (prerequisite).
- carina-rs/carina#TBD-C — `wait` construct RFC (this document, on the carina side).
- Terraform's `aws_acm_certificate_validation` source (`hashicorp/terraform-provider-aws@main:internal/service/acm/certificate_validation.go`) — surveyed for reference; the synthetic-resource pattern was found to be a Terraform idiom (also used by `time_sleep`, `null_resource`, `terraform_data`) but rejected for Carina due to the runtime-escape hacks it requires.
- carina-rs/infra Issue #29 (T6: `usecases/registry/`) — the ultimate consumer.
- carina-rs/infra `docs/specs/2026-05-05-registry-dev-bootstrap-design.md` D6 — the design that surfaced the requirement.
