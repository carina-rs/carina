# `DeferredPopulate` Attribute Annotation: Design

<!-- derived-from ./2026-05-09-wait-construct-design.md -->

## Goal

Carry "this attribute is populated asynchronously by the provider after Create returns" as schema metadata, propagate it through validation and the LSP, and reject downstream chained references that don't have a synchronizing `wait` block ahead of them — at validate time, before `carina apply` is even attempted.

The trigger is carina#3032: a `route53.RecordSet`'s `resource_records = [cert.domain_validation_options[0].resource_record_value]` references an ACM Certificate attribute that AWS does not populate until *after* `RequestCertificate` returns, so the post-Create binding lacks the value, the resolver preserves the unresolved ref, and apply fails. The runtime fix in #3033 (executor fail-fast at `assert_fully_resolved`) gives the user an actionable error pointing at `wait` — but the user still has to run `carina apply` to discover that hint.

This design moves the diagnosis upstream to validate / LSP, so the user is told *while authoring* that the chained reference needs a `wait` and is offered a code action to insert one.

## Non-goals

- **Inferring `DeferredPopulate` automatically from AWS docs / Smithy traits.** First round is explicit per-attribute opt-in driven by the codegen template. Inference is a follow-up.
- **Generalizing to non-AWS providers in the first PR series.** The design must allow it (the annotation lives in `carina-core::schema`, not in `carina-aws-types`), but the first codegen PRs touch only the `aws` and `awscc` providers.
- **Removing the runtime fail-fast added in #3033.** It stays as defense-in-depth — a misannotated attribute (or a brand-new resource the codegen hasn't been updated for) should still fail loudly at apply rather than ship a literal `ResourceRef` to the WASM boundary.
- **Modeling provider-side eventual consistency for *all* attributes.** `DeferredPopulate` is for attributes whose value is genuinely undefined immediately post-Create (ACM `domain_validation_options`, CloudFront `domain_name`, RDS `endpoint`). Attributes that take time to *settle* but are *defined* immediately (e.g. tag propagation lag) are out of scope.
- **Wiring an automatic `wait` injection.** The validator points the user at `wait` and the LSP offers a code action; it does not synthesize the `wait` block silently. Synchronization remains the user's explicit declaration (matches the philosophy of the `wait` construct itself — see `./2026-05-09-wait-construct-design.md` §"Why `wait` belongs in carina-core").

## Why this lives in the schema, not in a per-resource carve-out

Three alternatives were ruled out before settling on the schema annotation:

| Approach | Why rejected |
|---|---|
| **A: Hard-code a list of `(resource_type, attribute)` pairs in `carina-core::validation`** | Doesn't scale across the AWS surface. Would also live in the wrong repo (provider-specific knowledge in core). |
| **B: Provider-side runtime polling — providers' `create()` blocks until DVO is populated** | Conflicts with the `wait` construct's design (`./2026-05-09-wait-construct-design.md` §B "Bake polling into create()"): cyclic dep when the validation record needs DVO before the cert can wait on the record. Same root cause that motivated `wait` itself. |
| **C: Surface only the runtime fail-fast (#3033) and rely on user reading the error** | Symptom-only fix. Each new resource that exposes a deferred-populate attribute hits the same wall, the user discovers it only at `carina apply` time, and CI feedback loops are minutes long. The annotation moves the signal to validate (~ms). |

The chosen design — annotation in `AttributeSchema` / `StructField`, propagated by codegen, enforced by validation + LSP — fixes all three:

- Provider-specific knowledge stays in the provider repos (codegen output).
- No runtime polling indirection; the existing `wait` construct is the user-facing primitive.
- Validation catches it at parse time; LSP shows the diagnostic as the user types.

## Annotation shape

A boolean field on `AttributeSchema` and `StructField`, opt-in via a builder method:

```rust
// carina-core/src/schema/mod.rs

#[derive(Debug, Clone)]
pub struct AttributeSchema {
    // ... existing fields ...

    /// Whether the value of this attribute is populated by the provider
    /// asynchronously *after* the Create call returns. Downstream
    /// resources that read this attribute via a chained access
    /// (`<binding>.<this_attr>...`) without a preceding `wait` block on
    /// the binding will be rejected at validate time. carina#3034.
    ///
    /// Examples (set in the provider codegen):
    /// - ACM `Certificate.domain_validation_options`
    /// - CloudFront `Distribution.domain_name`
    /// - RDS `DBInstance.endpoint`
    /// - Lambda `Function.invoke_arn` (after first invocation)
    ///
    /// Independent of `read_only`: a deferred-populate attribute may
    /// also be `read_only` (the user cannot set it), but a `read_only`
    /// attribute is not necessarily deferred-populate (it may be
    /// populated synchronously, e.g. an ARN echoed back by Create).
    pub deferred_populate: bool,
}

impl AttributeSchema {
    pub fn deferred_populate(mut self) -> Self {
        self.deferred_populate = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct StructField {
    // ... existing fields ...

    /// Same semantics as `AttributeSchema::deferred_populate` but for
    /// nested struct fields. Reached when the chained access traverses
    /// a `Struct` (e.g. `cert.domain_validation_options[0].resource_record_value`
    /// — the inner struct field is the deferred-populate one, not the
    /// outer list attribute).
    pub deferred_populate: bool,
}

impl StructField {
    pub fn deferred_populate(mut self) -> Self {
        self.deferred_populate = true;
        self
    }
}
```

Two field placements (top-level attribute and nested field) instead of one because real schemas have both shapes:

| Shape | Where to mark | Example |
|---|---|---|
| Whole list/value is empty until populated | `AttributeSchema.deferred_populate` | RDS `DBInstance.endpoint` (whole `endpoint` map) |
| List exists immediately but inner field is empty | `StructField.deferred_populate` | ACM `Certificate.domain_validation_options[*].resource_record_*` (the list has one entry per SAN immediately, but `resource_record` is None until ACM computes the validation record) |

The ACM case is the more subtle one and is the carina#3032 trigger: the AWS provider's `read_acm_certificate` (carina-provider-aws::services::acm::certificate.rs:209-249) populates the outer list as soon as DescribeCertificate returns, but the `if let Some(rr) = dv.resource_record()` arms skip when ACM has not yet computed `resource_record_value`. So the outer attribute is *present* (a List), but the inner field that the chained access wants is *absent*. Marking only the top-level attribute would over-trigger the diagnostic; marking only the inner field would miss the RDS shape.

### Validation rule

Spec, in order of precedence:

1. **Identify** every `Value::Deferred(ResourceRef { path })` in every resource's attribute tree.
2. **Look up** the schema along `path.attribute()` + `path.segments()`. If any segment hop traverses a `deferred_populate=true` attribute or struct field, mark the ref as *deferred-populate-bound*.
3. **Determine** whether the binding named in `path.binding()` has a synchronizing `wait` ahead of it in the directory's resource graph. A `wait` block whose `target_ref` is the binding (or one of its dependencies) and whose `until` predicate references a deferred-populate attribute *of that binding* synchronizes all chained accesses on that binding.
4. **If deferred-populate-bound and not synchronized**, emit a validation error:

   ```text
   carina-rs/infra/usecases/registry/acm.crn:30:22
       attribute `resource_records` references
       `cert.domain_validation_options[0].resource_record_value`,
       which is populated asynchronously by the provider after Create.
       Add a `wait cert { ... }` block synchronizing on the attribute
       before this resource:

           let cert_issued = wait cert {
               until = cert.status == aws.acm.Certificate.Status.Issued
           }

       Then change references from `cert.…` to `cert_issued.…`.
   ```

5. **Same rule for `BindingRef`**, `Interpolation`, and `FunctionCall` — any expression that recursively contains a deferred-populate-bound `ResourceRef`.

The validation runs in `carina-core::validation` against the merged directory-scoped `ParsedFile`, so cross-file `wait` blocks satisfy the rule.

### Synchronization detection

A `wait <binding> { until = <pred>, ... }` block declared in the same directory satisfies the rule for ANY chained access on `<binding>` (not just the predicate's specific attribute). Rationale: by the time the user has written a `wait` on a binding, they have asserted "this resource has reached a steady state"; we trust the user's predicate covers the populated-attribute set. Tightening the rule to "the wait predicate must reference *this exact attribute*" would force users into one wait per accessed attribute, which is impractical for nested structs (the cert's wait predicate is `cert.status == ISSUED`, which transitively guarantees DVO is populated, but the rule wouldn't see the connection).

The validator does NOT need to evaluate the predicate. Existence of the wait block is the contract.

### What "ahead of" means

The wait satisfies the rule for a downstream resource if:

- The downstream resource's `dependency_bindings` (or transitive closure thereof) contains the wait's binding, OR
- The downstream resource references the wait's binding directly (e.g. `dist.viewer_certificate.acm_certificate_arn = cert_issued.arn` — `cert_issued` is the wait binding).

A wait block whose result binding is never read by anything downstream does not satisfy any downstream's deferred-populate constraint — the user should be told to either remove the orphan wait or wire it in.

## Codegen

Provider codegen (in carina-provider-aws / carina-provider-awscc) emits `.deferred_populate()` on the relevant builders. Initial set, derived from the carina-rs/infra usecases that motivated #3032 plus a quick survey of comparable AWS APIs:

| Service | Resource | Attribute | Shape |
|---|---|---|---|
| ACM | `Certificate` | `domain_validation_options[*].resource_record_name` | StructField |
| ACM | `Certificate` | `domain_validation_options[*].resource_record_type` | StructField |
| ACM | `Certificate` | `domain_validation_options[*].resource_record_value` | StructField |
| ACM | `Certificate` | `status` | AttributeSchema (transitions PENDING → ISSUED) |
| CloudFront | `Distribution` | `domain_name` | AttributeSchema |
| CloudFront | `Distribution` | `etag` | AttributeSchema |
| RDS | `DBInstance` | `endpoint` (struct) | AttributeSchema |
| Lambda | `Function` | `invoke_arn` (synchronous; defer follow-up to confirm) | AttributeSchema |

The exact list is finalized in the per-provider codegen PR; the design doc commits only to the *mechanism* and the ACM rows (which gate the carina#3032 close).

The codegen template lives in `carina-codegen-aws/templates/` (and the awscc equivalent). The annotation is added to a small allowlist file (`deferred_populate_attributes.toml` or similar — exact format chosen in the implementation PR) that the template consults; this keeps the per-attribute decisions reviewable in one place rather than scattered across generated files.

## LSP

Two surfaces:

1. **Diagnostic** — same text as the validate-time error, surfaced by the existing `carina-lsp::diagnostics` pipeline. Severity = Error; the validate-side rule and the LSP diagnostic ship in the same PR so they cannot drift.
2. **Code action** — `Insert "wait" block for <binding>` that produces:

   ```crn
   let <binding>_ready = wait <binding> {
       until = <binding>.<best-guess-status-attr> == <suggested-value>
   }
   ```

   Plus a follow-up edit that rewrites all `<binding>.…` references in the current file to `<binding>_ready.…`. The "best-guess-status-attr" comes from a small table (`status` for ACM, `state` for EC2, `db_instance_status` for RDS, …); if the resource has no known steady-state attribute, the code action emits a placeholder `until = <binding>.??? == ???` and a TODO comment.

The code action is a convenience; the diagnostic alone is enough to satisfy the validate/LSP parity rule.

## Migration impact

- **Breaking for users with chained references to deferred-populate attributes without a `wait`.** This is exactly the carina#3032 class — they are already broken at apply time today. The change moves the failure earlier (validate time, milliseconds vs. apply minutes) but does not introduce *new* failures. carina-rs/infra has exactly one such site (the registry ACM Certificate) — caught by real-infra smoke as part of the implementation PR.
- **No backward-compatibility shims.** Existing schemas that haven't been annotated yet simply don't trigger the new diagnostic — opt-in.
- **State file unaffected.** No changes to state v3 schema.
- **WIT contract unaffected.** The annotation lives entirely in `carina-core::schema`; providers don't transmit it across the WASM boundary.

## PR series

This design lands as a separate PR before any implementation:

1. **This PR** — design doc only. (carina#3034 design)
2. **`carina-core` implementation** — schema fields, builder methods, validation rule, validator tests against directory fixtures (cross-file `wait` blocks must satisfy the rule), LSP diagnostic threading. (carina#3034 core)
3. **`carina-provider-aws` codegen** — apply the annotation to the ACM rows above; regenerate schemas. (aws#TBD)
4. **`carina-provider-awscc` codegen** — same for AWSCC's overlapping coverage. (awscc#TBD)

Steps 3 and 4 can happen in parallel after step 2 merges. carina#3032 closes when steps 2 + 3 land and `carina-rs/infra/usecases/registry` validates clean (step 4 is independent — awscc has no registry resource on this path).

## Open questions

- **Should the annotation also affect plan rendering?** Plan today renders unresolved refs as `(known after apply)`. A deferred-populate-flagged ref could be rendered as `(known after wait)` with a hint to add the synchronization. Probably yes; deferred to the implementation PR's scope discussion.
- **Should LSP completion suggest `wait <binding>` when the user starts typing a chained access on a deferred-populate-flagged binding?** Probably yes; deferred to the implementation PR.
- **Should the validator distinguish "no wait at all" vs. "wait exists but downstream doesn't read its result binding"?** The two errors have different fix-it actions. Worth two distinct diagnostic codes.
