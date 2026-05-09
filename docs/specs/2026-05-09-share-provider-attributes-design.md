# Sharing provider attributes across components

<!-- derived-from ../../CLAUDE.md#directory-scoped-never-single-file -->

Issue: [#2717](https://github.com/carina-rs/carina/issues/2717)

## Goal

Make it possible to factor out a shared set of provider-block attributes
— most concretely `default_tags` — and reuse it across multiple
component directories without copy-pasting the literal map.

The driver is `carina-rs/infra`. The next several PRs (`registry/dev/registry-deploy/`,
`registry/dev/registry/`, `registry/prod/registry-deploy/`,
`registry/prod/registry/`, etc.) all want the same five-key
`default_tags` set with one or two values flipped per environment /
component. Today every `providers.crn` has to repeat the literal map.
We want a first-class way to share it.

## Background

`provider <name> { ... }` is currently a closed shape:

- The pest grammar accepts `attribute*` inside the block, where each
  `attribute = identifier "=" expression`.
- `parse_provider_block` in `carina-core/src/parser/blocks/provider.rs`
  evaluates each attribute via `parse_expression` and stuffs the
  results into an `IndexMap<String, Value>`.
- A handful of attributes are pulled out of that map by name with
  pattern matches that **assume a literal value at parse time**:

  ```rust
  let default_tags = if let Some(Value::Map(tags)) =
      attributes.shift_remove("default_tags") {
      tags
  } else {
      IndexMap::new()
  };
  ```

  The same pattern applies to `source` (`Value::String`), `version`,
  and `revision`. If the right-hand side resolves to a `Value::ResourceRef`
  — which is what a `let`-binding reference produces at parse time —
  the pattern does not match and the attribute silently becomes empty
  / `None`. No diagnostic, no error.

That silent-empty behaviour is the concrete blocker for
`default_tags = tags.tags`.

## Chosen approach

**Option 1** from the issue: allow the right-hand side of provider-block
attribute values to be an arbitrary expression, including a reference
to a top-level `let` binding. Source the shared values via the existing
module + `exports` + `use` mechanism. No new vocabulary.

The end-user shape:

```crn
# infra/.../registry/dev/registry-deploy/providers.crn
let st = use { source = '../../../modules/standard-tags' }

let tags = st {
  environment = 'dev'
  component   = 'registry'
}

provider awscc {
  source       = 'github.com/carina-rs/carina-provider-awscc'
  revision     = 'main'
  region       = awscc.Region.ap_northeast_1
  default_tags = tags.tags
}
```

```crn
# infra/.../modules/standard-tags/main.crn
arguments {
  environment: String
  component:   String
}

attributes {
  tags: map(String) = {
    ManagedBy   = 'carina'
    Project     = 'carina-rs'
    Repository  = 'carina-rs/infra'
    Environment = environment
    Component   = component
  }
}
```

The module exposes its tag map via the `attributes { ... }` block,
not `exports { ... }`. The two surfaces are unrelated:

- `attributes { ... }` declares a module's **return-value fields** —
  the things a call-site binding (`let st = standard_tags { ... }`)
  carries on `st.tags`. `module_resolver::expander` turns each entry
  into a virtual-resource attribute at module-call time, so the value
  is resolvable as soon as the module call is expanded.
- `exports { ... }` declares **state values** that another
  configuration directory can read via `upstream_state { source = '...' }`.
  The exports are evaluated against the directory's *applied state*
  (post-plan/apply), not against parse-time DSL values.

A module's return value is `attributes`. `exports` is the
state-export surface — using it for module return values does not
work, because the call-site binding only sees `attributes` entries.

### Why Option 1, not Options 2/3

| Axis | Option 1 (let + use) | Option 2 (`tag_set` module type) | Option 3 (parent-dir inheritance) |
| ---- | -------------------- | -------------------------------- | --------------------------------- |
| Language surface | None added (grammar already permits `expression`) | New module kind, new grammar, new LSP/diagnostics paths | New parent-walk semantics, new file convention |
| Repetition removal | Full | Full | Full |
| Greppability | Explicit reference at the use site | Explicit | Implicit; can't see the source from the call site |
| Reuse beyond `default_tags` | `region`, `allowed_account_ids`, `revision` etc. all benefit | Per-attribute new types needed | Same parent-walk, but every attribute opts into implicit inheritance |

Option 2 is rejected because it adds vocabulary for a problem the
existing module system already solves. Option 3 is rejected because
the issue itself flags it as "implicit; less greppable" and the
project's `directory-scoped, never single-file` discipline does not
extend to *parent*-directory walking.

### What is already in place

A test fixture written during this design pass parses cleanly through
the grammar today:

- `let st = use { source = '...' }` — existing
- `let tags = st { environment = 'dev', component = 'registry' }`
  parses as a `module_call` whose `module_name` is the alias `st`.
  `collect_known_bindings_merged` (`carina-core/src/parser/resolve.rs:262`)
  already includes `parsed.uses[].alias` in the binding name set, so
  the `st` module name resolves.
- Module `arguments { ... }` and `attributes { ... }` blocks —
  existing. (`exports { ... }` is the state-export surface read by
  `upstream_state { source }` and is unrelated to module-call return
  values; the call-site binding carries `attributes` entries.)

The only place that breaks is the parse-time extraction of `default_tags`
in `parse_provider_block`.

## Key design decisions

### D1. Defer extraction of well-known provider attributes until after the resolver pass

The pattern-match strip in `parse_provider_block` runs *during* parsing.
The resolver runs *after* parsing. To accept references in
`default_tags`, the strip needs to move past the resolver — or the
provider config needs to carry an unresolved `Value` through the
resolver and only get its well-known attributes peeled off at the end.

Two viable shapes:

- **D1a — Parse keeps everything in `attributes`; a post-resolver pass
  peels off `default_tags` / `source` / `version` / `revision`.**
  `ProviderConfig` stops carrying typed `default_tags`, `source`, etc.
  fields at parse time; instead it carries the raw `attributes` map
  plus a small post-resolver step that interprets the well-known keys
  once their values have been resolved to literals.

- **D1b — Parse keeps the typed fields, but their type widens to
  accept unresolved values; the resolver visits provider configs and
  rewrites them in place.** `default_tags: IndexMap<String, Value>`
  stays, but the entry "value is `Value::ResourceRef`" is now
  legitimate, and a new resolver step replaces it with the resolved
  literal map.

D1a is preferred. It keeps the resolver as the single source of truth
for "when does a `ResourceRef` become a literal" and prevents new
`Value`-shape assumptions from leaking into provider-aware code paths
elsewhere (state, plan display, the WIT bridge).

The implementation issue gets to pick the precise shape, but the
design contract is: **after the resolver pass completes, `ProviderConfig`
exposes typed `default_tags` / `source` / `version` / `revision`
that look exactly like today's, and downstream consumers (provider
plugin host, `merge_default_tags_for_provider`, plan display) keep
working unchanged.**

### D2. Validate the well-known attributes once they are resolved

Type validation of `default_tags` (must resolve to a map of strings),
`source` (string), `version` (string parseable as a version
constraint), and `revision` (string) is currently inlined in
`parse_provider_block` and emitted as a `ParseError`. After D1, those
checks have to move to the post-resolver pass and surface as the same
parse-class error so existing tests (e.g.
`parse_provider_block_with_default_tags`,
`parse_provider_version_revision_mutually_exclusive`) continue to
catch the same shapes. The error site moves; the user-facing message
doesn't.

### D3. `merge` is out of scope for this PR

Issue #2717's example shows `standard_tags { environment = 'dev', component = 'registry' }`
overlaying a base set. The chosen approach handles per-instance overrides
**through module arguments** (the `st { ... }` call), so `merge`
itself is not on the critical path for solving the issue. A `merge`
DSL function may still be desirable as a separate utility — for
mixing default tags with per-resource tags, for example — but it is
deferred to a follow-up issue. This document does not specify it.

### D4. No new grammar

The pest grammar already allows arbitrary `expression` in
`attribute = identifier "=" expression`, which is what
`provider_block` uses (`"provider" identifier "{" attribute* "}"`).
No grammar change is needed; the change is entirely in
`parse_provider_block` and the resolver.

### D5. Scope of well-known attributes covered by this work

The issue lists `default_tags` as the immediate need and mentions
`allowed_account_ids`, `region`, and similar as future targets. This
PR's design covers `default_tags` end-to-end (extraction site moved
past resolver, validation moved, fixtures, real-infra smoke). The
same mechanism transparently unlocks the other attributes once they
go through the same post-resolver peel — but expanding the test
coverage / docs / LSP completion to call them out is **not in scope**
for this design. They become "free" once `default_tags` works, and
each can be promoted in a follow-up issue when there's a concrete
driver.

## File structure / architecture

Touched components:

- **`carina-core/src/parser/blocks/provider.rs`** — main change. Parse
  collects all attributes into `IndexMap<String, Value>` without
  peeling. Mutual-exclusion check for `version` + `revision` either
  stays here (still works on the raw values when both are literals,
  errors for the resolved case after D1) or moves to the post-resolver
  pass — implementer's choice.
- **`carina-core/src/parser/ast.rs`** — `ProviderConfig`'s typed fields
  (`default_tags`, `source`, `version`, `revision`) keep their types
  and stay public. Internal storage may grow a transient
  `unresolved_attributes` field (or equivalent) used only between
  parse and resolver.
- **`carina-core/src/parser/resolve.rs`** (or a sibling module) — new
  post-resolver step that walks each `ProviderConfig`, looks up the
  reserved keys in the unresolved map, validates their resolved
  shapes, and writes them into the typed fields. Existing identifier-
  scope checks (`accumulate_undefined_reference_errors`,
  `check_identifier_scope`) already know how to chase `ResourceRef`
  roots; they just need to be told to also visit provider attribute
  values.
- **Diagnostics in `carina-lsp/src/diagnostics/`** — must continue to
  catch invalid shapes (e.g., `default_tags = "string"`,
  `version = some_resource_ref`) with the same severity and message
  class as today. The error sites move from parse to post-resolver
  but the user-visible behaviour does not regress.
- **`carina-cli/tests/fixtures/`** — at least one multi-file fixture
  matching the `infra/aws/management/<dir>/` shape:
  `main.crn` + `providers.crn` + a sibling `modules/standard-tags/`
  directory. The fixture asserts that `default_tags` resolved through
  `let` + `use` produces the same effective map as a literal
  `default_tags = { ... }`.

Untouched:

- WIT bridge to provider plugins (provider host receives the same
  resolved `IndexMap<String, Value>` it does today).
- `merge_default_tags_for_provider` and the `Provider` trait's
  `merge_default_tags` hook (`carina-core/src/provider.rs`). These see
  resolved values and don't care how the values reached them.
- State persistence and plan display (their input is the
  resource-level merged tag map, after the provider host applies
  `default_tags`).

## Edge cases and constraints

- **Silent-empty regression risk.** Today, a syntactically-valid but
  type-wrong `default_tags = "string"` falls into the `else` branch
  and silently produces an empty map. The post-resolver peel must
  raise an error in that case rather than continuing the silent-empty
  behaviour. A regression test pinning the diagnostic is required.
- **Unknown reference at provider attribute.** A reference to an
  undefined binding inside a provider attribute must surface via the
  existing `UndefinedIdentifier` flow. `accumulate_undefined_reference_errors`
  currently iterates `parsed.resources`, `parsed.attribute_params`,
  `parsed.module_calls`, `parsed.export_params`, and deferred for-iterables.
  A new arm for `parsed.providers[].attributes` (and the unresolved
  default_tags / source / etc., if those live separately) is required.
- **Directory-scoped, never single-file.** Per the
  `directory-scoped, never single-file` rule (CLAUDE.md), the
  acceptance fixture for this work is multi-file:
  `providers.crn` + a `modules/standard-tags/` directory at minimum.
  Adding a single-string unit test is fine *in addition*, never
  *instead*.
- **`use` + same-name `let` collision.** `let st = use { ... }` already
  reserves `st` as the alias. If the user then writes
  `let st = "literal"` later in the same directory, that is an
  existing duplicate-binding error and remains so — the design does
  not change collision semantics.
- **Module call as RHS.** `let tags = st { environment = 'dev' }`
  parses as a `module_call`. `parse_module_call` rejects
  `module_name == "remote_state"` for migration reasons; no other
  reserved names exist. The alias-equals-`module_name` resolution path
  was added when `use` was introduced and is exercised by existing
  tests.
- **Empty `default_tags` after resolution.** If the resolved value is
  an empty map (`{}`), preserve the existing behaviour: empty default
  tags is allowed and equivalent to omitting the attribute.

## Out of scope

- A `merge` DSL function (D3).
- Auto-migration of existing `providers.crn` files in `carina-rs/infra`.
- Promoting `allowed_account_ids` / `region` / `revision` /
  `region`-list to formally-tested shared-attribute targets (D5).
- LSP completion suggesting `let` bindings inside provider blocks. The
  identifier scope already includes them; `value_completions_for_attr`
  may need a follow-up to surface them in completion lists, but that's
  polish, not correctness.

## Acceptance

- `default_tags = some_let_binding.field` produces the same effective
  resource-level tag map as a literal `default_tags = { ... }` with
  the same contents, in plan, apply, and round-trip state.
- Fixture demonstrating the multi-component shape from `Goal`.
- Real-infra smoke: `aws-vault exec mizzy -- carina validate <one
  registry/* component using a shared standard-tags module>`
  succeeds, and `aws-vault exec mizzy -- carina plan ...` shows the
  same effective tags it would have shown for the literal-map version.
- All existing `parse_provider_block_*` tests continue to pass with
  their current assertions; if any need to be replaced with
  post-resolver equivalents, the replacement keeps the same
  user-facing diagnostic.
