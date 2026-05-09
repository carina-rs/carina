# `depends_on` Meta-Argument: Design

<!-- blocked-by ./2026-05-09-wait-construct-design.md -->
<!-- constrained-by ../../docs/specs/2026-05-09-wait-construct-design.md -->

## Goal

Add a `depends_on = [<binding>, ...]` meta-argument to every `let` binding in Carina that produces a managed effect (resource, wait, module). The differ resolves the list into additional ordering edges in the plan, beyond what value references already imply.

This closes the gap between "B's value depends on A's value" (already expressible) and "B's create must happen after A's create even though no value flows between them" (currently inexpressible). The most immediate consumer is `wait` blocks for ACM DNS validation (`carina#2825`), which need to wait until a Route 53 validation record exists before polling certificate status — the wait targets the cert, not the record, so the record dependency cannot ride on a value reference.

## Non-goals

- `depends_on` on anonymous resources (no binding name to use). Anonymous resources can be wrapped in a `let` if a `depends_on` edge is needed.
- Cross-module `depends_on` referencing a binding inside another module instance (e.g., `module.web.bucket`). MVP accepts only flat binding names visible in the current scope.
- Attribute-level `depends_on` selectors (`[iam_role.id]`). MVP accepts the binding name only; if a value reference is needed the user writes the value reference directly.
- `depends_on` on `data` and `upstream_state` bindings (semantically weak — see "Allowed binding kinds" below).
- A separate `validates`-style sub-keyword for "depend only on Update" or "depend only on Delete". MVP treats every `depends_on` edge as "depend on the target binding's effect *as a whole*", with the planner's existing phasing logic (which handles create-before-destroy, etc.) doing the per-phase ordering automatically.

## Prerequisites

This design assumes the following Carina change has landed first:

- **`carina#2826` — Rename `lifecycle` block to `directives`.** The `depends_on` meta-arg lives inside the renamed block. If `#2826` slips, the work is mechanically rebasable (`directives` → `lifecycle` is a single sed pass), but writing the design and tests against the destination name keeps documentation stable.

This design is also referenced by:

- **`carina#2825` — `wait` construct implementation.** `wait` blocks need `depends_on` to express ordering against sibling resources whose values they don't reference. `#2825` lists `#2823` (this design) as a hard prerequisite.

## DSL syntax

```crn
let key = aws.kms.Key { ... }
let role = aws.iam.Role { ... }

let bucket = aws.s3.Bucket {
    bucket_name    = "my-bucket"
    encryption_key = key.arn          # value reference: bucket → key
    directives {
        prevent_destroy        = true
        create_before_destroy  = true
        depends_on             = [role]   # explicit edge: bucket → role
    }
}
```

`depends_on` is a regular block attribute inside `directives { ... }`, alongside the existing `force_delete`, `create_before_destroy`, and `prevent_destroy`. The right-hand side is a list literal whose elements are bare identifiers naming sibling `let` bindings.

### Why inside `directives` (not at attribute level)

The other binding-level meta-arguments (`force_delete`, `create_before_destroy`, `prevent_destroy`) all live inside the `directives` block already. Putting `depends_on` outside that block would split related concepts across two sites and force the user to remember which meta-args go where. Following the existing pattern is mechanical and consistent.

Alternatives considered:

- **Top-level attribute (`depends_on = [...]` directly inside the resource block).** Matches Terraform but breaks the "one block for all directives to Carina" structure that `#2826` codifies. Rejected.
- **Nested block (`depends_on { resources = [...] }`).** Verbose and over-structured. Rejected.

## Allowed binding kinds

| Binding kind | `depends_on` target? | Why |
|---|---|---|
| Managed resource (`let foo = aws.s3.Bucket { ... }`) | ✅ | Has Create/Update/Delete effects; "wait for these to complete" is the obvious meaning. |
| `wait` binding (`let foo = wait bar { ... }` from `#2825`) | ✅ | A wait is a passthrough of its target with a synchronisation semantic; treating it as a depends_on target means "wait for this wait to satisfy". Necessary for chained waits. |
| Module call (`let foo = module.web_tier { ... }`) | ✅ | A module instance is a virtual collection of resources; `depends_on = [module_binding]` expands to "depend on every effect produced by that module". |
| Data source (`let foo = aws.identitystore.User { ... }` registered as DataSource) | ❌ | Data sources are read at plan time; nothing to "wait for". A depends_on edge here would be a no-op. Diagnose as error so users don't get a false sense of ordering. |
| `upstream_state` binding | ❌ | Resolved at parse/compile time from another stack's state file, not from runtime effects. Same reason as data sources. |
| Anonymous resource | ❌ (target only) | Anonymous resources have no binding name to reference. (You can still depend *from* an anonymous resource — just give it a `let` binding to gain a depends_on field.) |

The four allowed shapes converge on the same operational meaning: **"wait until every Create / Update / Replace / Wait effect contributed by the target binding has completed before starting any effect contributed by the depending binding."** For modules, "contributed effects" expands to the entire module instance's effect set.

## Validation rules

The parser and `carina validate` (and LSP diagnostics) reject:

| Rule | Severity | Trigger | Message shape |
|---|---|---|---|
| Unknown binding | error | `depends_on = [non_existent]` and no `let non_existent = ...` exists in scope | `directives.depends_on: binding 'non_existent' is not declared in this scope` |
| Cycle (closed loop in the dependency graph including both value-refs and depends_on) | error | `a depends_on [b]` and `b depends_on [a]`, or `a` value-references `b` and `b depends_on [a]` | `directives.depends_on: cycle detected through a → b → a` |
| Disallowed binding kind | error | `depends_on = [data_source_binding]` or `depends_on = [upstream_binding]` | `directives.depends_on: data sources and upstream_state bindings are resolved at compile time and cannot be depended on; remove '<name>'` |
| Self-reference | error | `let a = ... { directives { depends_on = [a] } }` | `directives.depends_on: self-reference is not allowed (binding 'a' depends on itself)` |
| Element type mismatch | error | `depends_on = ["x"]` (string literal instead of identifier) | `directives.depends_on: list elements must be binding identifiers, not string literals` |
| Duplicate element | warning | `depends_on = [x, x]` | `directives.depends_on: binding 'x' is listed twice` |
| Redundant edge already implied by value reference | warning | `bucket` value-references `key.arn` *and* lists `key` in `depends_on` | `directives.depends_on: 'key' is already implied by a value reference; this entry is redundant` |

All seven checks run in the same `analysis` pass that already runs after parsing and before plan generation. The pass is shared by `carina validate` (CLI) and `carina-lsp` (live diagnostics) so users get identical errors regardless of where they hit them.

## Effect storage and the union strategy

A `depends_on = [role]` edge is stored in **two** places, which the differ keeps in sync:

### 1. `Resource.directives.depends_on: Vec<String>` — IR-side source of truth

Lives on the parsed `Resource` (and on the analogous IR struct for `wait` bindings). This is the structure the parser and `analysis` pass write; it survives serialisation to `carina.state.json` because `Resource.directives` is already serialised today (the field used to be `Resource.lifecycle: LifecycleConfig`, after `#2826` it becomes `Resource.directives: Directives`).

### 2. `Resource.dependency_bindings: BTreeSet<String>` — union with value-reference dependencies

Today, `Resource.dependency_bindings` holds the set of bindings the resource depends on via value references. `phased.rs::has_interdependent_replaces`, `parallel.rs::build_dependency_map`, and `plan_tree.rs::get_resource_dependencies` all read this single field to figure out ordering.

The differ extends this set with `directives.depends_on`. After differ runs:

```
Resource.dependency_bindings == get_resource_dependencies(value-refs)
                                ∪ Resource.directives.depends_on
```

`phased.rs`, `parallel.rs`, and `plan_tree.rs` need **no changes** — they keep reading `dependency_bindings` and treat all edges uniformly. Source provenance (value-ref vs. explicit) is intentionally lost at this layer because no consumer needs it.

### 3. `Effect.explicit_dependencies: HashSet<String>` — Effect-side display copy

Every `Effect` variant (`Create`, `Update`, `Replace`, `Delete`, `Read`, `Wait`, `Import`, `Remove`, `Move`) gains an `explicit_dependencies: HashSet<String>` field. The differ populates it from `Resource.directives.depends_on` directly (not the union). This field is not load-bearing for ordering — `dependency_bindings` already covers that. It exists so display layers (`format_effect_brief`, plan tree, snapshot test fixtures) can introspect "which edges came from depends_on" without re-running the differ.

`Effect::Delete` already has a `dependencies: HashSet<String>` field today (used for plan tree). The new `explicit_dependencies` is **additional**, not a replacement — `dependencies` keeps its current "all dependencies including value-refs" semantics for the plan-tree path; `explicit_dependencies` carries only the `directives.depends_on` subset.

### Why two storage shapes

| Storage | Audience | Lossy? |
|---|---|---|
| `Resource.directives.depends_on` | Parser, analysis pass, `carina validate`, `carina fmt`, LSP | No — preserves user intent verbatim |
| `Resource.dependency_bindings` | `differ`, `phased.rs`, `parallel.rs`, `plan_tree.rs` | Yes — unioned with value-refs, source lost |
| `Effect.explicit_dependencies` | Display, snapshots, debug | No — copy of `directives.depends_on` |

The split keeps each layer reading the shape it needs (parsers want intent; executors want a flat dependency set; displays want provenance). The differ is the single point that fans the IR-side source out to the other two.

## Plan display

### `format_effect_brief` (default `carina plan` brief output)

Unchanged. `depends_on` does not appear on the per-effect line. Ordering of effects in the brief output is the only signal of dependency, which already includes both value-refs and depends_on after the union.

### Plan tree (`carina plan` tree mode, when enabled)

`plan_tree.rs::get_resource_dependencies` already builds parent/child edges from value references. After this design lands, the same function returns the unioned set (value-refs ∪ explicit), so the tree automatically displays explicit edges alongside value-ref ones. The two source kinds are **not visually distinguished** in the tree — the user sees "bucket depends on role" without knowing whether that came from `encryption_key = role.arn` or `depends_on = [role]`.

If a future use case demands provenance in the tree (debugging, LSP code action hints), `Effect.explicit_dependencies` is already populated and the tree renderer can switch to a two-line render with source labels. Out of MVP scope.

## State file

`Resource.directives.depends_on` is already covered by `Resource.directives` serialisation (the existing `LifecycleConfig` -> after `#2826` `Directives` field is serialised in state v3). No state-format change is required:

- New writes record the new field automatically.
- State files written before this PR have no `depends_on` key in the `directives` map; the field deserialises to `Vec::default()` (empty), which means "no explicit edges", which is the correct legacy behaviour.

There is no state migration. The "no backward compatibility" project policy means we don't need to write a shim, but the change happens to be backward-compatible by virtue of `serde`'s default-on-missing behaviour.

## Cycle detection algorithm

The unioned dependency graph (`Resource.dependency_bindings` after differ runs) is the working representation. Cycle detection is performed by the existing `topological_sort` in `carina-core/src/deps.rs`, which already handles cycles for the value-reference case. Adding `depends_on` edges into `dependency_bindings` means the same algorithm catches `depends_on`-induced cycles for free.

The error message currently produced by `topological_sort` is sufficient for MVP. A future improvement (out of scope here) is making the error annotate which edge in the cycle is `depends_on`-sourced vs. value-ref-sourced, using `Effect.explicit_dependencies`.

## LSP behaviour

| Feature | Behaviour |
|---|---|
| Block-key completion inside `directives { ... }` | Adds `depends_on` to the existing keyword set (`force_delete`, `create_before_destroy`, `prevent_destroy`). Trigger: cursor at start of attribute position inside the block. |
| List-element completion inside `depends_on = [|]` | Strict filter: only `let` bindings of kinds resource / wait / module in the current scope; exclude the binding being defined (self-reference) and any names already present in the same list. Implemented by querying the LSP's `binding_index` with a kind filter and a `not_in` set. |
| Hover on a binding identifier inside `depends_on` | Reuses the existing identifier-hover handler — same shape as hovering on `key` inside `encryption_key = key.arn`. Returns binding name, kind (resource / wait / module), defining file + line, and the resource type. |
| Semantic tokens | Adds `depends_on` to `KEYWORDS` in `carina-core/src/keywords.rs` (the single source of truth shared with the TextMate grammars). Identifiers inside the list use the existing variable-reference token (no special treatment). |
| Diagnostics (live in editor) | All seven validation rules from the table above. Severity, message text, and source span all match what `carina validate` produces. |

The `binding_index` query for list-element completion is the only LSP-side bit that needs new code — everything else is mechanical extension of existing keyword tables and diagnostic dispatch.

## Edge cases and constraints

### Discard pattern and `depends_on`

```crn
let _ = aws.s3.BucketPolicy {
    ...
    directives { depends_on = [bucket] }
}
```

A `let _ = ...` discard binding (already supported in `carina.pest`) can carry a `depends_on`, since the discard pattern still produces an effect for execution. The discard binding cannot itself be a depends_on *target*, since it has no name to reference — `depends_on = [_]` is rejected with the unknown-binding diagnostic.

### `wait` target also listed in `depends_on`

```crn
let cert_issued = wait cert {
    until      = cert.status == ISSUED
    directives { depends_on = [cert] }   # `cert` is also the wait target
}
```

Legal but redundant: the wait already implicitly depends on its target (the wait can't read the target until the target's create completes). The redundancy diagnostic from the table catches it as a warning, mirroring the value-reference + depends_on redundancy case.

### `depends_on` referencing a `for_each`-expanded binding

Carina's `for` expression generates multiple resources from a single binding name. `depends_on = [for_each_binding]` is treated as "depend on every expanded resource". This falls out for free: the binding name in `Resource.dependency_bindings` already maps to the entire group via the existing reference-resolution logic, and the differ will emit edges to each instance. No new design needed.

### Module bindings expanded in `depends_on`

`depends_on = [web_tier]` where `web_tier` is a module call expands to "depend on every effect produced by `web_tier`". The expansion happens in the differ when it walks `dependency_bindings` and encounters a module-binding name; it substitutes the module's resource set in place. This matches how value references against module exports work today.

### Empty list

`depends_on = []` is legal (means "no explicit edges"), distinguishable from omitting the field entirely (also "no explicit edges"). Treating both as the same has no downside.

### Reordering elements

`depends_on = [a, b]` and `depends_on = [b, a]` are semantically identical (set semantics). Formatter normalises to one canonical order (alphabetical) on `carina fmt` to keep diffs stable.

## Risks

- **Cross-module `depends_on` is omitted.** A module's internal binding cannot be referenced from outside the module (`depends_on = [module.web.bucket]` is not supported). If the registry usecase or another consumer needs this later, we'd need to extend the lookup to module-scoped paths, which is a non-trivial change to `binding_index` and the resolver. Mitigation: document the limitation; if a consumer hits it, file a follow-up issue rather than rushing the design here.
- **Union strategy loses provenance at the executor layer.** A bug that produces a wrong dependency edge can't be traced back to "value-ref vs. explicit" from the executor's logs alone. Mitigation: `Effect.explicit_dependencies` retains the explicit subset specifically so debugging and snapshot tests can introspect; CLI / LSP / formatter all see the unmerged source.
- **Legacy state files round-trip the new field.** State written before this PR has no `depends_on` in `directives`; after a no-op refresh, `serde` writes the empty list back. This adds a tiny amount of noise to the first state-rewriting plan/apply on existing infrastructure. Mitigation: acceptable — the project memory rule is "no backward compatibility, and don't mention it" — so we just let it happen and move on.
- **Cycle messages don't yet identify edge provenance.** Today the planner's cycle error says `cycle detected through a → b → a` without saying which edge came from `depends_on`. Mitigation: out of MVP scope; the existing message is informative enough to debug, and `Effect.explicit_dependencies` makes it possible to enhance the message later without further design work.
- **`directives` rename (`#2826`) is in flight.** This design is written assuming `#2826` lands first. If it doesn't, every reference here to `directives` becomes `lifecycle`. Mitigation: trivial sed pass to revert; both names point to the same struct, so the design's substance is unaffected.

## Acceptance criteria

The `depends_on` meta-arg is considered "done" for MVP when:

1. `directives { depends_on = [<bindings>] }` parses across `carina validate` and the LSP, with diagnostics for all seven rules in the table above.
2. The differ unions `directives.depends_on` into `Resource.dependency_bindings`; the existing `phased.rs`, `parallel.rs`, and `plan_tree.rs` consumers treat the new edges identically to value-reference edges, with no source-side modification needed.
3. `Effect.explicit_dependencies` is populated on every Effect variant from `Resource.directives.depends_on`; snapshot tests cover at least one fixture per Effect kind that carries an explicit edge.
4. `carina plan` (brief) shows no per-effect annotation for `depends_on`; ordering changes are visible only via the surrounding effect order.
5. `carina plan` (tree) renders explicit edges identically to value-ref edges (no source label).
6. State file (`carina.state.json`) round-trips `directives.depends_on` correctly; legacy state files (no `depends_on` key) deserialise to an empty list and re-serialise with the field present.
7. Multi-file fixture: a directory containing `main.crn` (resources) + `directives.crn` (resource with `depends_on` referencing a binding from `main.crn`) parses and validates. Per CLAUDE.md "Directory-scoped, never single-file" rule.
8. The `carina#2825` consumer (the `wait` construct) compiles against the API exposed here and uses `depends_on = [validation_record]` to express the ACM DNS validation ordering.

## Related work

- `carina-rs/carina#2823` — this design's tracking issue.
- `carina-rs/carina#2826` — `lifecycle` → `directives` rename (prerequisite).
- `carina-rs/carina#2825` — `wait` construct implementation (consumer; depends on this).
- `carina-rs/carina#2824` — `Duration` type and literal (sibling prerequisite for `#2825`, independent of this design).
- `carina-rs/carina#2822` — merged design + plan documents for the `wait` construct, where the requirement for `depends_on` first surfaced.
- Terraform's `depends_on` meta-argument (https://developer.hashicorp.com/terraform/language/meta-arguments/depends_on) — the conceptual ancestor; Carina's variant is narrower (binding identifiers only, no attribute selectors).
