# Resource vs Data Source representation — design document

**Related issue**: [#2325](https://github.com/carina-rs/carina/issues/2325)
**Blocks**: [#1791](https://github.com/carina-rs/carina/issues/1791)
**Feature branch**: `2325-resource-vs-data-source-design`
**Date**: 2026-05-02

## Goal

Decide how the distinction between **managed resources** (full CRUD lifecycle) and **data sources** (lookup-only references to existing infrastructure) is represented across Carina's three layers — schema, codegen, and generated docs — so that:

1. The kind information flows from the codegen `Def` (the source of truth a provider author writes) all the way to the docs site frontmatter without ambiguity.
2. The same AWS type (e.g. `aws.s3.Bucket`) can in the future be used both as a managed resource and as a lookup, without forcing an `ill-defined "Both"` variant on `ResourceSchema`.
3. #1791 (provider docs distinguishing data sources from resources) becomes implementable without further blocking design work.

This document is a decision tracker; it does not contain implementation. Implementation is split across follow-up issues identified at the end.

## Background

Today the kind information is represented inconsistently across layers:

- **Schema** (`carina-core/src/schema/mod.rs:1760`): a single `pub data_source: bool` flag flipped by `as_data_source()`. The flag is consumed by exactly one behavioural site — `carina-core/src/validation/mod.rs:35`, which enforces "if the schema is a data source, the DSL must use the `read` keyword." Other consumers are protocol pass-through (`carina-provider-protocol/src/types.rs:174`, `carina-plugin-host/src/wasm_convert.rs:274`) and tests.
- **IR** (`carina-core/src/resource/mod.rs:929`): `enum ResourceKind { Real, Virtual { … }, DataSource }`. The `read` keyword in the DSL produces `ResourceKind::DataSource`; the differ branches on it (`carina-core/src/differ/plan.rs:173`) and emits only `Effect::Read`. The CLI uses `is_data_source()` to split refresh into two phases (managed in phase 1, lookups in phase 2 — `carina-cli/src/wiring/mod.rs:866,1400`) and to skip data sources during destroy (`carina-cli/src/commands/destroy.rs`).
- **Codegen** (`carina-provider-aws/carina-codegen-aws/src/resource_defs.rs`): two parallel structs `ResourceDef` and `DataSourceDef`, each in its own `Vec`. Their fields differ substantially (`ResourceDef` has `create_op`, `update_ops`, `delete_op`, `force_replace`, `name_attribute`, etc.; `DataSourceDef` has `inputs: Vec<DataSourceInput>` and `read_ops` only). No type currently appears in both lists.
- **Generated markdown** (`carina-provider-aws/carina-codegen-aws/src/main.rs:2161,2739`): `generate_markdown_resource` and `generate_markdown_data_source` are separate functions. Neither emits frontmatter. Only the data source body emits a sentinel line `This is a **data source** (read-only). Use with the \`read\` keyword.`
- **Docs migration** (`docs/scripts/migrate-provider-docs.sh`): a one-time bash script that prepends frontmatter with a hard-coded `description: "...resource reference"` regardless of kind. The two existing data source docs (`aws/sts/caller_identity.md`, `aws/identitystore/user.md`) carry the wrong description today.

The crucial observation: **the managed-`read` and the data-source-`read` are not the same operation.**

| Aspect | Managed `read` | Data source `read` (`read_data_source`) |
|--------|----------------|------------------------------------------|
| Input | `state.identifier` (saved from a previous create) | User-supplied lookup inputs from the DSL |
| Provider entry point | `read(id, identifier)` | `read_data_source(resource)` |
| When `state` is missing | "not found" | Run lookup using DSL inputs |
| Purpose | Drift detection / refresh | Reference an existing resource |

This means the existing managed schema for `aws.s3.Bucket` cannot serve as a data source by itself: even if validation lets `read aws.s3.Bucket { ... }` past, no `read_data_source` implementation exists, lookup inputs are not declared, and the create-input attribute set is wrong for a lookup contract.

## Decisions

### Decision 1 — Where does `kind` live as data?

#### 1-1. Naming and shape on `ResourceSchema` and `ResourceKind`

**Replace `data_source: bool` with a 2-value enum** and align IR naming.

- `ResourceSchema { data_source: bool }` → `ResourceSchema { kind: SchemaKind }` where:
  ```rust
  pub enum SchemaKind { Managed, DataSource }
  ```
- `ResourceKind { Real, DataSource, Virtual }` → `ResourceKind { Managed, DataSource, Virtual }` (rename `Real` → `Managed`).
- **No `Both` variant.** See 1-2 for why two-use cases are modelled as two registry entries instead of a single ambiguous schema.

**Why "Managed / DataSource" and not other names**

We considered `ReadWrite / ReadOnly`, `Owned / External`, `Lifecycle / Lookup`. The deciding constraint is that the same vocabulary must work in both `SchemaKind` (a 2-value classification of a schema) and `ResourceKind` (a 3-value classification of an IR resource: managed, data source, or module-synthesised virtual). `ReadWrite / ReadOnly` describes write capability and does not extend naturally to a `Virtual` sibling. `Managed / DataSource` is the one pairing that:

1. Reuses an industry term (`data source`, used by Terraform, Pulumi, etc.) so provider authors and reviewers recognise it.
2. Makes `ResourceKind::Managed` semantically correct (the resource is managed by Carina) and keeps `Virtual` as a distinct, orthogonal third case (synthesised by the module resolver, not user-authored).
3. Avoids leaking UI vocabulary into core code. The end-user-facing label on the docs site is a separate decision (see Decision 3).

`ResourceKind` will keep its existing 3-variant shape (`Managed` / `DataSource` / `Virtual`). The fact that `Virtual` is on a different conceptual axis (origin: hand-written vs synthesised) than `Managed`/`DataSource` (write capability) is a known modelling smell. A future-work option is a 2-axis split (`Origin × Access`); we deliberately decline to make that change in this issue (see *Forward work* below).

#### 1-2. Same type usable as both: registry multi-registration

**The schema registry must allow the same `(provider, resource_type)` to be registered as both a `Managed` schema and a `DataSource` schema.** A type used in both ways (e.g. `aws.s3.Bucket` for both creating new buckets and looking up existing ones) is represented as **two separate schema entries**, not as a single schema with a `Both` flavour.

Rationale:

1. The two `read` operations have **different semantics, different inputs, different state contracts, and different provider entry points** (table above). A single schema cannot honestly carry both: which `attributes` map does it expose? Which `name_attribute`? Which `force_replace`? A `Both` variant ends up with internal sub-records anyway, which is just two schemas in disguise.
2. Codegen already has two separate `Def` structs (`ResourceDef`, `DataSourceDef`) with disjoint fields. Multi-registration aligns with that input shape: a provider author writes both `Defs` for `aws.s3.Bucket`, and the codegen produces two registry entries.
3. The IR `ResourceKind` already discriminates which entry to dispatch to: a DSL-authored `read aws.s3.Bucket { ... }` (`ResourceKind::DataSource`) reaches the DataSource entry; `aws.s3.Bucket { ... }` (`ResourceKind::Managed`) reaches the Managed entry. No new dispatch mechanism is needed.
4. Validation gains a clean two-sided check: "DSL uses `read` but no DataSource entry exists for this type" → error; "DSL omits `read` but no Managed entry exists for this type" → error. This closes the current one-sided gap where `read` against a managed-only schema passes validation but has no provider implementation.

The exact registry-key shape — `(provider, resource_type, kind)` triple, or a single key with kind-suffixed names, or a registry-of-pairs structure — is left to the implementation issue. WIT-boundary impact (the protocol currently exposes `data_source: bool`) is part of that issue's scope.

### Decision 2 — Who owns markdown frontmatter?

**Hybrid: codegen emits Carina-vocabulary frontmatter; the docs build adds Starlight-specific fields.**

Concretely:

- **Codegen (`carina-codegen-aws`, `carina-codegen-awscc`)** emits a frontmatter block at the top of every generated `.md` containing only Carina-vocabulary fields:
  - `title` (the DSL-form name, e.g. `aws.s3.Bucket`)
  - `description` (a kind-aware short string, e.g. `"AWS S3 Bucket resource reference"` for managed, `"AWS STS CallerIdentity data source reference"` for data sources)
  - A structured kind field (working name `kind: managed` or `kind: data_source`; the exact spelling is a small implementation detail). The point is that kind flows as **structured data**, not as a body sentinel string to be regex-grepped.
- **The docs build (in this repo)** runs a small preprocessing step that reads the frontmatter `kind:` field and adds Starlight-specific UI fields (notably `sidebar.badge`). This preprocessing replaces the kind-blind `description` hard-coding currently done by `docs/scripts/migrate-provider-docs.sh`.
- **`docs/scripts/migrate-provider-docs.sh`** is retired in its current form. Its responsibilities split: (i) the kind-aware description is pushed up into codegen; (ii) Starlight-specific fields move into the docs build preprocessor.

#### Why the hybrid

- **(a) codegen emits everything including Starlight fields**: rejected because it leaks docs-site-specific UI knowledge (e.g. `sidebar.badge.variant: "note"`, the exact badge text) into provider crates. Provider authors should not have to know Starlight's frontmatter schema.
- **(b) sentinel-grep sync script**: rejected because it throws away the structured kind information that 1-1 just ensured exists, then reconstructs it by string-matching the body. Brittle, and contradicts the spirit of 1-1.
- **(c) docs build re-generates markdown from `ResourceSchema`**: rejected because the schema is runtime metadata and does not retain Smithy-model details (raw `documentation` traits, type display strings, enum values) that the current codegen markdown generator uses. Re-implementing that path would require schema enrichment that is out of scope.
- **(a') hybrid (chosen)**: kind flows as structured data through frontmatter; UI vocabulary stays on the docs side; provider crates do not learn about Starlight.

#### Sentinel body line

The sentinel line `This is a **data source** (read-only). Use with the \`read\` keyword.` becomes redundant once kind is in frontmatter. Whether to keep it (for plain-text readability) or remove it (to reduce duplication) is left to the implementation issue.

### Decision 3 — `sidebar.badge` contract

**Deferred to #1791.** This issue establishes that kind reaches frontmatter as structured data; it does **not** finalise the badge text, badge variant, or whether managed pages also receive a badge. Those decisions are made in #1791 with the live UI in front of the implementer.

A consideration to record for the eventual badge decision: a single kind badge (e.g. "Data Source") on a per-page basis can be **mis-read as exclusive** — readers infer "no badge ⇒ this type cannot be used as a data source." Once Decision 1-2 is implemented and a type like `aws.s3.Bucket` is registered as both Managed and DataSource, the docs presentation must avoid that exclusivity inference. Possible directions (capability-style multi-badge, in-page capabilities section, …) are left open for #1791 and the 1-2 implementation.

### Decision 4 — Both-capable types in docs

**Deferred until needed.** No `Both`-style registration is planned at the moment, and 1-2 is itself follow-up work. The choice of one-file-per-kind vs single-merged-file vs other layouts is left to the issue that actually adds the first dual-registered type.

## Ownership and ordering

| Work | Repo | Issue type |
|------|------|-----------|
| Write this ADR | `carina` | Part of #2325 |
| Rename `bool` → `SchemaKind`, `ResourceKind::Real` → `Managed` | `carina` (core, protocol, plugin-host, plugin-sdk), `carina-provider-aws`, `carina-provider-awscc` | New follow-up |
| Codegen emits Carina-vocabulary frontmatter | `carina-provider-aws`, `carina-provider-awscc` | New follow-up |
| Docs build preprocessor (Starlight fields) and retirement of `migrate-provider-docs.sh` | `carina` | New follow-up (scope overlap with #1791) |
| Registry multi-registration + two-sided validation + `read_data_source` for first dual-registered type | `carina` (core), `carina-provider-aws` and/or `carina-provider-awscc` | New follow-up |
| Provider docs distinguish data sources from resources | `carina` | #1791 (existing) |

Independence of work:

- **#1791 does not depend on the rename (1-1).** It can land against the current `data_source: bool` if the rename is still in flight; the docs work only needs *some* kind signal in frontmatter, and codegen can branch on either the bool or the new enum.
- **#1791 does not depend on registry multi-registration (1-2).** No dual-registered type ships in #1791.
- **The rename (1-1) is independent of the registry change (1-2).** The rename is a mechanical refactor of the existing 2-state classification; multi-registration is an additive extension of the registry independent of how each entry is named internally.
- **Multi-registration (1-2) does depend on the rename (1-1) for clean naming**, but only stylistically. It can technically be implemented against either spelling.

Recommended order:

1. **This issue (#2325)**: ADR merged, follow-up issues filed.
2. In parallel: (a) the rename PR (1-1), (b) codegen frontmatter emission + docs preprocessor + #1791.
3. After (1) lands: registry multi-registration (1-2), the first dual-registered type, and the badge-contract revisit triggered by it.

## Forward work (explicitly out of scope here)

- **`ResourceKind` 2-axis split.** `Origin × Access` (e.g. `Origin: Authored | Synthesised`, `Access: Managed | DataSource`) would untangle the current 3-variant mixing of two conceptual axes. Not done here because (a) it pays for a refactor whose payoff (cleaner classification) is structural rather than user-visible, and (b) it touches every part of the codebase that branches on `ResourceKind`.
- **Lookup inputs in validation / LSP.** `DataSourceDef::inputs` is currently used only for markdown generation. Surfacing it in `ResourceSchema` so validation and LSP know what lookup inputs a data source requires is a natural follow-up but is independent of the decisions here.
- **Sentinel body line removal.** Cosmetic; bundled with the codegen frontmatter PR if convenient.

## Acceptance criteria for #2325

- [x] Written decision on (1) representation of `kind`.
- [x] Written decision on (2) ownership of frontmatter generation.
- [x] Written decision on (3) `sidebar.badge` contract (decision: defer to #1791 with recorded considerations).
- [x] Written decision on (4) Both-capable types in docs (decision: defer to the issue that introduces the first such type).
- [x] Per-decision repo ownership and ordering captured.
- [x] #1791 remains open and references this ADR as its blocker resolution.
