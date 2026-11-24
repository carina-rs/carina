# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

---

# Part 1 — Core Rules

These are the rules that override default behavior. Read them first.

## Root-cause fixes only — and make the broken state unrepresentable

**The most important rule of this project.** A bug fix must answer two
questions, and passing the first is not evidence of passing the second:

1. **Root cause** — *what is broken right now?* Fix the *one* upstream
   change that restores the invariant, not a filter / guard / carve-out
   at every consumer site. If the same broken invariant produces
   symptoms in multiple code paths (`apply`, `destroy`, `state refresh`,
   `plan`, …), they are *one* bug, not N.
2. **Long-term / type safety** — *can a future caller re-reach this
   class of bug?* Prefer the fix that makes the broken state
   unrepresentable (newtypes, tagged unions, typestate) over a runtime
   filter every consumer must remember.

A fix that answers yes-to-(1) but no-to-(2) is **a runtime patch at
multiple consumer sites disguised as a root-cause fix** — it works today
and silently regresses when the next consumer is added.

**Self-checks before opening a PR:**

- If the diff *filters / guards / skips* the buggy condition instead of
  *removing* it, the fix is symptom-level. Step back, find the upstream seam.
- Does the diff add `find_*` / `resolve_*` / `lookup_*` at multiple
  consumer sites? Does it rely on every consumer remembering to do
  something? Is a sibling code path doing the same dance? Any "yes" →
  a newtype / typestate is the right factoring tool. The "new caller
  tomorrow" check must be answered by the **type signature**, not by
  documentation or convention. Make the resolver step required by the
  type: return a wrapper only a resolver can produce; make the raw type
  uncomparable to the resolved type.
- **5-round review passing is NOT evidence the fix is root-cause.** A
  bandaid passes every gate. Ask "if a new caller appears tomorrow, does
  it need to remember this filter too?" — if yes, the root is still broken.

**Never** propose "minimal fix in this PR, follow-up issue for the rest"
when "the rest" is the same class of bug at sibling sites — that is a
bandaid presented as scope discipline. **Never** invoke "1 PR = 1 topic"
to justify a per-site patch; "1 topic" means one *root cause*. Fixing the
root *is* the topic. **When in doubt, pick the broader fix** — shrinking
scope and offering a follow-up has been pushed back on every single time.

**Do the typed reshape in-PR. Do not measure radius and do not defer
it.** Stopping to count call sites is harmful — it primes you to think
"this looks big, let's compromise". The right answer is always "do the
reshape now". Past attempts to use radius to justify deferral have all
been wrong; remove the measurement from the loop entirely.

**Don't reach for type-shape-weakening tools as a compromise.**
`#[non_exhaustive]`, `#[must_use]`, doc comments saying "callers must
remember to", convention-only seams that turn into "grep `_ =>` arms" —
these convert a type-safety bug into a documentation convention. They
are not type-safe; they are the soft version of the very mistake this
rule is trying to prevent. If a single-method API permits silently
dropping a variant, the answer is to *change the API shape* — split into
two methods, return a typed enum the caller must destructure, etc. —
not to dress up the unsafe shape with attributes. If you find yourself
proposing one of these tools "for type safety", you have already lost
the argument. (carina#3476: an early draft chose
`FnMut(RefKind<'_>)` + `#[non_exhaustive]` over splitting `visit_refs`
into `visit_resource_refs` / `visit_binding_refs`; the user pushed back
that this is exactly the convention-only fix the rule forbids.)

> Past failure mode (carina#3324 / PR #3325 → carina#3326): a runtime
> resolver fix at three consumer sites passed five review rounds and
> merged as "root-cause". The user then asked whether it was type-safe.
> Honest answer: no — `ResourceId` still permitted a routing-mismatch
> comparison, and a fourth consumer would re-introduce the bug. The
> typed reshape (`StateBlockAddress` newtype) became a follow-up instead
> of landing in the same PR. The lesson: apply *both* lenses at PR
> creation, not after the user asks.

### Don't use `name` (or any user-supplied attribute) as an identity

A resource's `name` is **a user-facing attribute**, not its identity.
Anonymous resources without a name, anonymous resources whose `name`
attribute is unresolved (`Value::Unknown`, deferred refs), and resources
whose `name` is empty by construction can all coexist in a single plan.
Any data structure that keys on `name` collapses these into the same
slot and produces silent, often catastrophic, mis-scheduling — the
classic "two anonymous resources collide and one Delete eats the other's
edge" bug.

The rule: **never use `name` (or any other user-supplied attribute) as
the identity component of a scheduler key, plan-diff key, state-lookup
key, or any other equality-bearing position.** The identity of an effect
or resource is what the *system* assigns (the parser's resource address,
the state row's UUID/lineage, a routing tag), not what the *user*
types into a `name = "..."` slot.

Concretely:

- A `BindingKey::Anonymous`-style enum variant must not contain a
  `name: String` field sourced from the user-facing `name` attribute.
  Carry the system-assigned identity (a `ResourceAddress`, a
  state lineage, a parser-assigned ordinal — pick what the surrounding
  type already guarantees is unique) instead.
- If the existing identity type (`ResourceId`, `ResourceName`, etc.)
  *itself* permits a "pending / unresolved" state at the point a
  scheduler/state/diff consumer holds it, that is the bug to fix first.
  Split the type so the consumer can only hold the resolved variant —
  do not paper over with a runtime "must be Bound" assertion in the
  consumer.
- Do not justify the shortcut with "scope" or "blast radius". If
  reaching a name-free identity requires reshaping `ResourceId` itself
  or introducing a `ResolvedResourceId` newtype, do that in the same
  PR. "It would balloon the PR" is exactly the failure mode the prior
  rule forbids.
- Same rule applies to plan / state / diff keys, not just scheduler
  keys: an `HashMap<String, _>` keyed on a name attribute, a
  `BTreeSet<String>` of "resource names this depends on", a Display
  impl that round-trips through a name and is compared elsewhere —
  all of these are the same bug in disguise.

Past failure mode (Phase 1 of carina#3625 — PR #3630, closed):
`BindingKey::Anonymous { resource_type, name }` was sourced from
`ResourceId::name_str()`, which for `ResourceName::Pending` returns
`""`. Two anonymous resources of the same type without a resolved
`name` would hash to the same key and silently share scheduling slots.
The fix shipped in Phase 1 redo: drop the `name` field, carry the
resource's system-assigned identity (`ResourceId` or a split type
that guarantees resolved-only) instead. The lesson: the moment the
type signature says `name: String`, ask "does this slot ever
participate in equality?" — if yes, it is identity, and `name` is the
wrong source.

## Delegate hands-on work to Codex

Work that edits files — implementation, refactoring, and plan-writing —
is delegated to Codex via the **codex** skill. Opus does not edit files
directly; it reads, thinks, reviews, and writes docs only.

- **When**: any implementation, refactor/cleanup, or plan-writing task,
  and escalation after repeated failed fix attempts.
- **How**: invoke the codex skill and follow its process — carry the
  intent/essence down, delegate with concrete scope, review the draft
  (root cause first), and send findings back to Codex to fix.
- Opus writing the plan, the implementation, or the refactor edits
  itself is forbidden; docs are the only thing Opus writes.

**Codegen delegations always include the docs regen step.** Whenever a
Codex task touches anything that drives schema codegen
(`carina-provider-aws*/src/bin/codegen.rs`, `resource_type_overrides()`,
`generate-schemas.sh`, or the generated `schemas/` tree), the
delegation prompt MUST instruct Codex to run BOTH `generate-schemas.sh`
AND `generate-docs.sh` (cache-only mode is fine) and verify the
`generated-docs/` tree is clean before declaring done. CI's `Check
Docs Content` runs `generate-docs.sh --from-cache-only` and fails if
the docs tree drifts; only running `generate-schemas.sh` locally
passes Opus-side verify but trips that CI gate. The pair is
load-bearing — never instruct one without the other. Past failure
mode: awscc#364 burned a CI cycle on this exact mistake (the
`RedirectConfig.Protocol` enum block was still in `generated-docs/`
after the schema lost its enum framing).

See the codex skill for the full subcontracting protocol.

## Communication Style

- Be terse. Do not ask permission for obvious actions (e.g., using the correct AWS profile, standard build commands).
- When opening/merging a PR, always include the GitHub URL directly in the response.
- No sycophantic phrasing ('Great question!', 'You're absolutely right!').
- Do not over-explain hypothetical design options before checking the actual real-world case (screenshots, real output).
- **Do not ask "how should I proceed" / "1 PR or 2 PRs" / "in what order".**
  Process choices — PR splitting, implementation order, worktree
  layout, commit granularity — are mine to decide. Pausing to ask is
  what the user has called out as "進め方もいちいち聞くな". The
  separate rule is: **design choices with a real trade-off** (option
  A/B/C where lenses disagree) get asked once (pick-issue Step 4.5),
  and the user has equally called out "勝手に設計判断されると困る".
  The split is sharp — process: decide silently; design with
  unresolved trade-off: ask once. When in doubt about which side a
  question falls on, ask whether it changes any artifact the user
  reviews (PR title, commit history, type shape) — if only the path
  to the same artifact changes, it is process.

## 日本語の文章作法

日本語のドキュメントを書く・編集するときも、ユーザーと日本語で会話するときも、
次の作法に従う。

- **考える段階から日本語に寄せる。** 英語で考えてから訳すと横文字が増えたり訳文調に
  なったりしやすい。日本語の単語を使っていても言い回しだけが英語に引っ張られることが
  あるので注意する。ただし、その分野で普通に通じる言葉まで無理に言い換えなくてよい。
  ソフトウェアで定着した語（リリース、コミット、マージ、デプロイなど）はそのまま使う。
  逆に、定着した訳がないものを直訳すると不自然になることがある（例: ship を「出荷」と
  訳すより「リリース」のほうが通じる）。横文字を日本語に寄せることと、通用語をそのまま
  使うことの兼ね合いで判断する。
- **強調は最小限にする。** 太字は用語の初出定義や注記ラベルくらいにとどめ、本文中の
  単語を強調しすぎない。
- **箇条書きを続けすぎない。** 箇条書きが続くと機械的な文章に見えやすい。散文でつなげ
  られるところは自然な文章としてつなげる。ただし禁止ではなく、並列の項目や手順、対応
  関係など、箇条書きや表のほうが見やすい場面では適切に使ってよい。避けすぎて読みにくく
  散文へ流し込むのも同じく良くない。
- **何をしているかが分かる言葉を使う。** 専門用語はその場で短く説明しながら使い、読み手が
  当然知っているものとしていきなり置かない。また抽象的な言い回しで誤魔化さない。たとえば
  「安価な防御」では何をしているか分からない。「入口での形式チェックと流量制限」のように、
  実際に行う操作を指す言葉を選ぶ。
- **ファイル名・パス・コード上の名前を本文にそのまま出しすぎない。** 共有資料やスライドに
  なったときに伝わりにくいことがある。必要に応じて文書タイトルや概念名に置き換える。
- **囲み枠や注記ブロックを多用しない。** 自然な本文に溶かせる内容なら散文の中で扱う。
  ただし、本文から切り出して目立たせる理由がある場合（警告、重要な例外、補足など）は
  使ってよい。

## Code Style

- **Commit messages**: Write in English
- **Code comments**: Write in English

## TDD for Bug Fixes

- Always write a failing reproducing test BEFORE implementing the fix.
- For TTY/rendering bugs, use PTY-based reproduction.
- When bugs are found or issues are pointed out, write a test that captures the fix so the regression is caught and the expected behavior is documented.
- Run the local checks, review and CI gates before merge (see Verify Protocol).

## Review Checklist — Carina-Specific Concerns

In addition to generic review dimensions (correctness, scope, tests,
OWASP), the following concerns must be checked on every review round
in this codebase.

### Parser Output Boundary Validation

Verify that raw values from the pest parser (strings, token sequences)
do not flow into domain types without validation. A successful parse
does not equal domain-level validation. At conversion points from
`Rule` matches to `ResourceType`, `AttributeValue`, `NamespacedId`,
etc., confirm that a step exists to reject inputs that are syntactically
valid but domain-invalid.

### Credential and Secret Leakage

Confirm that AWS access keys, session tokens, and credentials included
in provider responses do not leak into log output, `Debug` impls, error
messages, or unintended fields in state files. Also check `tracing` span
attributes and `Display` impls that expand provider responses verbatim.

### Error Chain Preservation

Check that `format!("{}", err)` or `.to_string()` is not used to
stringify errors in a way that severs the `#[source]` chain. Also flag
duplicate logging where the same error is emitted via `tracing::error!`
at both the adapter layer and the use-case layer.

### State Operation Atomicity

Verify that the read → diff → write sequence for state is performed
under a lock (`apply`/`destroy` paths). `plan` intentionally does not
acquire a lock (see the plan concurrency contract), but drift detection
must be functional. When adding a new state operation path, confirm
consistency with the existing contract.

### Property-Based Testing Applicability

When modifying code with obvious invariants — parser round-trips
(parse → display → re-parse yields identical result), `NamespacedId`
parse/display round-trips, differ idempotency (same input → same plan)
— consider adding property-based tests. Not required when existing
example-based tests already provide sufficient coverage.

---

# Part 2 — Build, Test, Verify

## Commands

Tests run under nextest (`cargo nextest run`, install once with
`cargo install cargo-nextest --locked`; version 0.9.133 or newer, which
`.config/nextest.toml` requires). Nextest does not run doctests and uses one
process per test. `scripts/verify.sh` also runs doctests and checks in-process
isolation.

Run tests through nextest, not plain `cargo test`. The setup script in
`.config/nextest.toml` points `HOME`, `TMPDIR` and `CARINA_*` at a per-run
temporary home, so tests never write to the developer's real `~/.carina`
(carina#4055). Plain `cargo test` skips that isolation; use it only for
`--doc`.

In-process mock tests pass a `MockBehavior` and explicit `MockScope` through
`TestCommandRuntime`. Allocate a fresh scope for each fixture; aliases within
one fixture share that scope. Set `CARINA_MOCK_*` only on child `Command`s in
subprocess tests. The executable's `process_inputs` module captures those
controls and passes owned inputs into the library.

Run `scripts/test-in-process-isolation.sh` to check the CLI library, CLI binary
unit tests, and mock library with eight libtest threads per process. It discovers
executables through nextest and uses the same disposable HOME/TMPDIR setup as
nextest; it does not use plain `cargo test`. Both local discovery and CI's
archive use `--workspace --all-features`, then select the same three suites.
CI builds the test archive and WASM fixtures once, runs nextest partitions and
in-process isolation from those artifacts, and runs provider-cause API checks
and doctests in separate parallel jobs. The required `Test` job aggregates all
of their results.

```bash
# CLI commands — path defaults to cwd; if given, it must be a DIRECTORY, not a file
cargo run -- plan

# Anything hitting AWS needs the credential wrapper
aws-vault exec <profile> -- cargo run -- plan
```

## Verify Protocol

`scripts/verify.sh` requires Bash 4.4 or newer, cargo-nextest and cargo-hack
for its full check set. Rustup automatically reads `rust-toolchain.toml` to
select the pinned toolchain, `clippy`, `rustfmt` and `wasm32-wasip2` target.
Run `rustup toolchain install` (rustup 1.28.0 or newer) to install them.
Upgrade the toolchain by editing that one file in an explicit PR.
When selecting kinds, only their required tools are checked before execution.

`--local` has a known limit: a `-p` scope resolves dependency features only for
the selected packages, so feature unification can differ from CI's `--workspace`
run. When in doubt, run `scripts/verify.sh` without `--local`.

1. Before `gh pr create` and before pushing follow-up commits to an open PR,
   run only the checks that belong to the change: the pre-commit hook (fmt and
   clippy on the touched crates; do not set `SKIP_CLIPPY`), the tests the
   change adds or affects, selected by name, and `cargo check -p` for
   dependent crates when a type or public API changed. PR CI runs the full
   check set; do not run `scripts/verify.sh --local`, a workspace-wide
   nextest, the determinism repeats or the parity script locally as a gate.
   In addition, when `scripts/touched-crates.sh` prints `--workspace` (any
   carina-core change, manifests, or other workspace-wide inputs), run
   `bash scripts/core-cross-checks.sh` before pushing. It runs the core, CLI
   and LSP suites and the core doctests, which compare core output with
   recorded results or another route; these caught every test failure in PR
   CI from 2026-11-16 to 11-23. The script skips itself when the diff is
   narrower and, when a target directory is set, requires `target` to
   resolve to it. Its filter is the one definition of the local selection
   and lists each excluded test with the reason; CI still runs everything.
   When CI fails, read the failed job's log, fix it, and re-run only the
   failed tests locally before pushing.
   `scripts/verify.sh --local` remains available for debugging a CI failure
   that cannot be reproduced by name. It runs the same check definitions and
   feature flags as CI; only crate scope comes from `scripts/touched-crates.sh`.
   Local scope includes touched crates and their transitive reverse dependents
   in the workspace, including dependents through dev and build dependencies.
   Use `--base <ref>` to override the helper's default base of `origin/main`.
2. PR CI must be green before merge.

Use `scripts/validate-parity.py` for main/candidate text and JSON parity (carina#4394).
Run `python3 scripts/validate-parity.py --main <main> --candidate <candidate>
--repo . --infra ../../../infra --evidence <dir>`.
Use `--binary <path> --repeat 8 --repo . --evidence <dir>` to compare fresh
processes of the same binary over every tracked configuration directory without
reordering output. The `carina-cli` nextest `validate_diagnostic_order` integration
tests retain this gate in four checked shards; each shard reports validation,
provider-not-installed, and setup failure counts and must reach validation for
every named `diagnostic_order/*` fixture it owns.

CI keeps its parallel jobs by selecting kinds from `scripts/verify.sh`.
The archive job builds the WASM mock fixtures and host test archive once;
partitions and in-process isolation consume those artifacts. Provider-cause
API checks and doctests run in separate jobs, and the required `Test` job
aggregates their results. On main pushes, the archive job compiles tests to
warm its cache. Local verify builds the same WASM fixtures before its tests.

## While Working

Use `cargo check -p <crate>` for a quick compile-only check and
`cargo nextest run -p <crate>` for tests during iteration. Nextest already
compiles its test binaries, so a separate `cargo build` is unnecessary.
`scripts/touched-crates.sh` can select crate scope for these iteration runs.
Provider crates (aws, awscc) are in separate repositories; changes here may
require updating those repos.

### Crate-Scoped Verify Helper

`scripts/touched-crates.sh` maps changed files, including uncommitted changes,
to cargo `-p <crate>` flags for touched crates and their transitive reverse
dependents. It derives workspace membership and dependency paths from Cargo
metadata, including all dependency kinds:

```bash
# What crates does the current branch touch (vs origin/main)?
scripts/touched-crates.sh
# → "-p carina-cli -p carina-tui"   (or "--workspace", or empty)

scripts/verify.sh --local                      # shared checks with local crate scope
scripts/touched-crates.sh --base main            # diff against a specific base
git diff --name-only | scripts/touched-crates.sh --stdin   # custom file list
```

| Output           | Meaning                                                   |
| ---------------- | --------------------------------------------------------- |
| `-p <crate> ...` | Run touched crates and their transitive reverse dependents. Otherwise ignored files gain an owner only through a path string literal containing `/` outside comments in tracked Rust sources, resolving to the tracked file or a tracked ancestor directory relative to the source directory, crate root, or repo root. |
| `--workspace`    | Cross-cutting change (core changes or ownership, manifests, WASM fixture crates, Cargo execution scripts/configuration, CI/local actions, or an unrecognized path), or the dependency closure covers every member. Sweep the whole workspace. |
| (empty)          | Only ignored paths without an owner changed (any-depth Markdown, site/notes/infra/examples, static-check scripts, non-CI GitHub files, Makefile/LICENSE, release configuration, packaging/assets/editors/docs). `verify.sh --local` skips only crate-scoped kinds; static kinds still run. |

The helper is intentionally pessimistic. Changes to `carina-core`, manifests,
WASM fixture crates, or Cargo execution configuration require the full
workspace. Artifact consumers have no Cargo dependency edge to the fixture
crates listed in `scripts/wasm-fixture-crates.txt`. A new top-level directory or
unclassified file also requires the full workspace until the helper learns
about it.

A `cargo metadata` failure emits a warning and deliberately widens to
`--workspace` to avoid missing tests. A git failure is an error because the
helper cannot determine which files changed.

## Plan Display Testing

Plan display changes are covered by fixtures and insta snapshots — see the
**plan-display-testing** skill for the `make plan-*` targets, fixture
layout, and snapshot update flow.

## Build Cache Setup

Builds use sccache (plus `sccache-wrapper` for cross-worktree cache
reuse) with a per-worktree `target/`. This is one-time machine setup,
not per-task knowledge — see the **build-cache-setup** skill for the
install steps, the `.cargo/config.toml` shape, and what to do when
several worktrees verify in parallel.

---

# Part 3 — Architecture

Carina is a functional infrastructure management tool that treats side
effects as values (Effects) rather than immediately executing them: the
pipeline runs DSL (`.crn`) → parser → resources → differ → plan (Effects) →
provider. Effects are inspectable values, never executed operations.

Carina is a polyrepo under [carina-rs](https://github.com/carina-rs): this
repo holds core/CLI/LSP/plugin/state/TUI, and the `carina-provider-aws` and
`carina-provider-awscc` repos depend on its crates via `git` dependencies.

**Cross-repo changes: never assume the sibling provider is unaffected.**
When touching anything that crosses the provider boundary — WIT / wasi:http
protocol, `carina-provider-protocol`, `carina-aws-types`, host↔plugin
serialization, shared schema or differ logic — explicitly check whether
`carina-provider-aws` *and* `carina-provider-awscc` both need a
counterpart PR (and a rev bump in this repo afterwards). Past breakage
(carina#3364, awscc#346) was a wasi:http / protocol mismatch where one
provider had pinned an older rev and silently degraded; "this only
touches aws" / "awscc looks unrelated" intuitions kept being wrong.
Search both provider repos for the symbol or protocol surface you
changed before declaring a PR complete.

## DSL Parser

The parser uses [pest](https://pest.rs/) grammar defined in `carina-core/src/parser/carina.pest`. Key constructs:
- `provider <name> { ... }` - Provider configuration
- `<provider>.<service>.<ResourceType> { ... }` - Anonymous resource (system-assigned auto-generated identity)
- `let <binding> = <resource>` - Named resource binding

## LSP Integration

Changes to the DSL or resource schemas must be mirrored in `carina-lsp`
(completion, semantic tokens, diagnostics) — validate and LSP stay at
parity. See the **lsp-integration** skill for the specific sites.

The two TextMate grammars —
`editors/vscode/syntaxes/carina.tmLanguage.json` and
`editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` — must stay
**byte-identical**; edit both together.

## Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `aws.s3.BucketVersioning.VersioningStatus.enabled` / `aws.s3.BucketVersioning.VersioningStatus.suspended` in DSL (enum values are snake_case per the DSL convention).

## Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.BucketVersioning.VersioningStatus.enabled`.

**When adding new namespaced patterns:**

1. **Go through `NamespacedId::parse`.** Segment syntax is decided by the
   private predicates behind it in `carina-core/src/utils.rs` (service and
   intermediate segments accept digits, as in `s3`). Do not write a new
   `chars().all(..)` predicate elsewhere; if one is unavoidable, accept
   `c.is_ascii_digit()` too.

2. **Treat `TypeIdentity` as the source of truth when one is available.**
   `NamespacedId::parse` in `carina-core/src/utils.rs` remains the
   schema-free syntactic gate for namespaced-looking values, and its 5+
   part branch still pins `TypeName` at index 3 so dotted values
   (`ipsec.1`) flow into the trailing slice. Validation paths that have
   an identity, such as `validate_enum_namespace`, must compare against
   the identity's provider, structural segments, and kind rather than
   re-deriving those axes from the dotted display string. Deep
   multi-segment full paths validate by this structural identity match.
   (Not yet true for the full-form branch of `validate_enum_namespace`:
   carina#4543.)

3. **Refined types are checked per identity.** A refined string/int/float type
   with a validator takes a mandatory `TypeIdentity` (no identity-less
   validator constructor exists). The LSP checks refined types generically per
   identity, so a new type needs no per-type LSP wiring.

4. **Always test with actual values** - Don't assume pattern matching works; write a quick test to verify

## Struct Types

A struct type represents a nested object with typed fields. Most guarantees
below are held by types, compile_fail doctests and named tests next to the
type each bullet names; read that type's rustdoc for the details.

- `AttributeType` is an opaque newtype (`pub struct AttributeType { pub(crate)
  kind: AttrTypeKind }`) defined in `carina-core/src/schema/mod.rs`. Its
  variant enum `AttrTypeKind` is `pub(crate)`, so downstream crates cannot
  pattern-match a raw `&AttributeType` — they construct one via the
  constructor methods and read it through the `Ref`-peeled `Shape` view
  (carina#3349). No check pins this visibility yet: carina#4540.
- `StructField` input mode is selected once by the `optional`, `required`, or
  `provider_populated` constructor; its fields are documented on
  `PatternStructField` (`StructField` is an alias).
  A nested declaration is not itself an effective mode:
  provider-populated ancestry belongs to the reference path. Consumers must
  start from a validated schema and use `EffectiveAttribute`, `EffectiveType`,
  and `EffectiveField` traversal views; below provider-populated output these
  report `ProviderPopulated` even when a shared definition declares the field
  optional or required. Assignments are checked against that effective mode
  (carina#3772). (Four `input_rules` sites still read the raw
  `declared_input_mode`: carina#4538.) Canonical
  provider/state/differ lookup uses `effective_attribute`; authored DSL lookup
  that may accept a block alias uses the explicitly named
  `effective_attribute_by_authored_name` /
  `effective_node_at_authored_path` traversal.
- **Patterns require explicit semantics.** `PatternSpec::new(regex,
  PatternMatch)` has no default mode or string-only form. Use
  `PatternMatch::Substring` only for a documented exception.
- **Schema types are builder-only.** `StructField`, `AttributeSchema`, and
  `ResourceSchema` keep their fields `pub(crate)` (carina#3984 / PR #3999);
  registries and factories accept only `ValidatedResourceSchema` /
  `ValidatedConfigSchema`. To destructure exhaustively (provider conversions,
  the WASM seam), use the read-only views
  `parts()` → `StructFieldParts` / `AttributeSchemaParts` /
  `ResourceSchemaParts`. Their fields are all `pub` and they are not
  `#[non_exhaustive]`, so adding a field still breaks every destructuring
  site until it is carried across. The parts views call the raw mode
  `declared_input_mode` because it is conversion data, not a consumer-facing
  effective mode. Don't add `pub` back to the schema structs' fields to "make
  a conversion easier". (compile_fail doctests pin field privacy for
  `StructField` and `AttributeSchema` only; `ResourceSchema` and the ban on
  `..` in `*Parts` patterns are not pinned yet: carina#4540.)
- **Write-only, top-level and nested.** `write_only` marks a value that is
  sent on create/update but never returned by reads (CloudFormation
  `writeOnlyProperties`). Where it may be set, its rejection below
  provider-populated output, the derived `ResourceSchema::write_only_bearing`
  and `first_write_only_conflict` are held by doctests and schema tests.
  Declared requiredness, defaults, and block aliases below a provider-populated
  ancestor are inert on that path; walkers must obtain their effective mode and
  active alias from an `Effective*` view. The differ compares a write-only
  value against the accepted value recorded in state, not against the read
  model (`key_should_enter_patch`). So an unchanged write-only value never
  causes an Update by itself, and a changed one does, sent as a normal
  `Replace`. When an Update happens, every other updatable desired attribute
  that is write-only or write-only-bearing is re-sent as `PatchOp::Reassert`
  alongside the change (carina#3981, carina#3984).
- **Observed secret protection** (carina#4479): `value::SecretProtection`,
  `ObservationStateAuthority` and `ProtectedObservations` hold the projection
  and authority rules (private constructors, value_observed_secret_tests).
  Any crate may decode a state document into an authority; this does not
  authenticate its bytes or backend origin. Maps match keys one to one:
  missing keys, including write-only secrets, and extra observed keys never
  widen protection, and unmatched observed keys stay ordinary.
  Dispatch rules consume the separate `RuleUpdatePatch` with the existing lenient
  kind and positional annotation. Diagnostics and `RedactionSet` consume
  `DiagnosticUpdatePatch` with output protection: enroll every string classified
  secret in the actual sent patch, including matching leaves and Whole units.
  Output promotion must not affect reference bindings or ordinary data-source read
  attributes. Preserve state merge policy and structured event coverage
  (`Unclassified` for updates).
- **Provider input wire checks**: `WireReady<T>`, `DesiredInputs::from_provider_ready`,
  `ProviderWireError` and `FinalInput<T>` hold the checked, cached transport
  projection (private fields, compile_fail doctests). `WireReady` does not
  mint declaration admission. Updates expose desired values only through the
  checked patch, with no desired resource or validation view (#2559). Keep
  `ProviderWireError` terminal: render its typed conversion reason once,
  without input values. Create validation and
  normalization maps use protocol key order. Raw normalization queries belong
  to `DesiredView` or `NativeDesiredNormalization`.
  Final admission retains every finding across all attributes and leaves,
  and rejects undeclared attributes introduced by normalization.
  Plan comparison consumes `FinalPlanInputs` after normalization and identity
  reconciliation. A changed payload must obtain a new aggregate through admission.
  Update patches obtain their own aggregate after observed ignored leaves are
  grafted; preparing a replacement must succeed before its paired delete is released.
- **Refinement layers** (carina#4348): String and Int carry separate
  identity and member layers (`StringIdentityLayer`/`IntIdentityLayer`,
  `StringMemberConstraints`/`IntMemberConstraints`; see
  `carina-core/src/schema/refinement_layers.rs`). Identity-definition
  agreement (`ConflictingIdentityDeclarations`), annotation evidence, and owner
  forms are checked at schema admission and decode. An Enum occurrence never
  masks later scalar agreement. Single-schema identity-definition agreement
  does not compare Enum and Reference occurrences of the same identity; for
  identities with reference forms, Enum/Reference mixing remains an
  aggregate-only `NonStringReferenceForm` error. Validate the identity pattern then bounds, member pattern then
  bounds, and finally the semantic callback; retain first-failure diagnostics.
  Never repeat structural checks in callbacks.
  Callbacks do not participate in agreement. Dispatch merges callbacks per
  identity, so a String declaration's callback also applies to Reference slots
  of that identity.
  Provider migrations must put resource-specific String/Int bounds in member
  layers and Reference restrictions in input rules. Across different owners
  (for example AWS and AWSCC), annotation constraints are conjoined:
  values must satisfy every owner's identity layer.
  Structural scalar reads use the same canonical evidence as identity annotations,
  so equal identity-level types compare and serialize equally. Callbacks,
  transforms, enums and references retain exact projection provenance.
  A member layer belongs to its destination slot. Enforce it on writes through
  identity admission and the deferred destination check; identified source bounds
  are identity-layer bounds only. Anonymous String/Int declarations retain both
  layers as structural evidence (patterns and intersected bounds). String/Int/Float
  sources with or without identity flowing to compatible identity-less scalar sinks,
  including Int-to-Float, reject only provably disjoint bounds (#4362). Reference
  admission records `NeedsLocal` for constrained identity-less sinks; provider slots
  check resolved values against the full destination declaration before dispatch.
  Final admission follows canonicalization, provider normalization,
  re-canonicalization and tag merging, using the same declaration validator.
  It includes custom validators and merged tag keys, values and entry counts,
  while authored-shape cross-attribute validators retain their authored scope.
  Authored tag overrides win; overridden defaults do not participate in membership.
  `PlanPreviewProviderConfigs` (plain plan display only) cannot be saved or
  dispatched (compile_fail doctests in `parser/resolve.rs`).
  `ReadOnlyProviderConfigs` (destroy, state refresh, bucket deletion) has no
  default tags or projection into desired inputs (compile_fail doctests in the
  same file); it is the dispatch input for read/delete. Default tags for desired inputs must be known when
  provider configurations resolve; validate and LSP reject local apply-time
  references. Plain plan display may omit individual default values authored
  as map entries that refer only to missing upstream state, with a warning;
  whole-map expressions such as `default_tags = mgmt.tags` must still resolve
  to a map during configuration finalization;
  `plan --out` and `plan --json` require strict configurations and reject
  those missing defaults. Read-only validation retains authored defaults
  without demanding them; only the typed read/delete provider path excludes
  those inputs.
  Anonymous scalar refinements occur only in provider schema slots; `TypeExpr`
  boundaries use identified refinements or unconstrained scalars. Adding overlapping
  source constraints never reduces assignability.
  Unannotated collections, conditionals and coalesce
  require identical `TypeExpr`s, as on main. Reads of slots differing only in
  member constraints infer equal types. An explicit compatible annotation is
  the way to accept otherwise mixed declarations; inference never widens them.
  Enum definition and canonical-spelling disagreements remain typed errors.
  Enum bases describe definitions, never slots: schema admission rejects String/Int
  member constraints inside a base with `EnumBaseMemberLayerError`; identity-less
  enum base comparisons follow the same overlap-or-defer rule.
  Hover renders both layers through the core's read-only bounded refinement
  walk (`RefinementPositions`). Every emitted position, including an
  identity-only String/Int, consumes budget. Plain scalar leaves and bare
  References emit nothing and cost no budget; only actual omissions mark
  truncation. Anonymous refinements use
  structural labels with intersected bounds and every pattern rather than
  identity/member labels; identified leaves show every identity-conjunction
  pattern. Shared provider docgen
  must render both layers too.
  Every typed sink uses the same String/Int identity-admission rule for
  equal identities and allowed widenings (#4349), including annotations,
  exports, module arguments, and function parameters/returns. Resource/data-source
  reference admission retains the existing `NeedsLocal` obligation. Same-identity
  value admission, including a declared source, defers overlapping identity bounds
  to the local check; provably disjoint bounds reject statically. Different-identity
  widening between identified scalars requires source identity bounds to be
  contained by destination identity bounds. Strict declaration compatibility
  retains containment checks only for pairs of identified scalars. Compatible
  identity-less sinks use #4362's overlap-or-defer rule in both declaration and
  reference checks, regardless of source identity. Original-evidence rechecks
  inspect only identified leaves.
  Declaration compatibility admits types, not resolved-value validity.
  `NeedsLocal` exists only in the admission-facing reference relation and cannot
  convert back into general type compatibility. Configuration admission lives
  in `validation/admission/configuration/` and is closed off from resource
  admission by its types. S3 backend URLs keep encryption on and
  auto-creation off. Function parameters/returns use
  `check_resolved_constraints_at`. Unknown values must be proven by their type;
  otherwise validate reports a function-promise finding, emitted only by
  `parser/retained/function_promises.rs`, carried in its own type, and never a
  stop/parse error. Unknown returns reuse the body typed with declared parameter
  types; nested-call verdicts are reused for relevant unknown outer arguments.
  Calls inside modules use the child's declared argument types. Unavailable
  schemas or upstream loads defer. Destroy/state refresh skip this pass through
  `ValidationMode` (the `FullValidation` capability). Some String references into
  narrower named or Int parameters still defer (carina#4594). Definite mismatches
  remain declaration errors, including unused functions. Member String failures
  retain their pattern/length category and member label; identity failures run first.
  Int-to-Float widening between identified scalars uses identity-bound containment
  too. Float pairs with identities on both sides retain strict declaration range
  containment; same-identity Float reference admission retains resolved-value
  deferral. Identity-less Float and Int-to-Float
  pairs follow #4362 instead. Enum/unloaded reference leaves retain that deferral too.
  Unproven regex containment defers to the value check. At annotated sinks,
  declaration equality and display-name equality do not replace identity
  admission; mismatch names expose differing constraints or scalar kinds.
- **Schema field transport**: `StructField` and `AttributeSchema` cross the
  WASM boundary as JSON via the `schemas: func() -> string` WIT export. The
  sole typed core/protocol boundary is `carina_core::protocol_convert`: the SDK
  accepts only `ValidatedResourceSchema` / `ValidatedConfigSchema` witnesses
  and projects them there before JSON serialization, while the host keeps only
  JSON/WIT glue and delegates protocol-record decoding to the same module.
  The nested `write_only` flag, the pattern mode, `InputRule.condition` and
  Struct `input_rules` are required fields with no serde default, and
  `MIN_SUPPORTED_PROTOCOL_VERSION` requires them; never add a default to
  them. Value floats and float range bounds are `FiniteF64`, admitted only through its checked
  constructor (carina#4257). SDK
  guests derive custom-type dispatch from the identity/callback pairs in the
  same validated resource/config witnesses they serialize; providers do not
  implement an optional success-default hook. The guest builds a callback
  table once, keyed by `TypeIdentity`; every binding stores its validated
  `RefinedScalarKind` and the provider-wide callback set for that identity.
  Dispatch runs every pointer-distinct callback conjunctively and deduplicates
  pointer-identical callbacks only as an optimization. Callback-free sibling
  declarations share the identity's callback set, and separately allocated
  callbacks from repeated type-constructor calls are valid. This is the honest
  current-protocol (`PROTOCOL_VERSION`) behavior: the wire names an identity,
  not a schema declaration, for semantic callback dispatch. Structural identity
  and member constraints are transported separately and checked by the host. The current WIT keeps
  `validate-custom-type(TypeIdentity, string)`: the guest parses canonical text
  with the binding's stored kind, and aggregate schema admission rejects an
  identity reused with another scalar kind. Resource/config schema walks pass
  the exact declared `AttributeType` to their single schema owner, so local
  structural constraints remain declaration-specific. A host-backed SDK owner
  then applies that provider's identity-level guest callback set, which may
  include callbacks declared by sibling schemas because the protocol cannot identify a
  declaration more narrowly.
  Authored annotations cannot select an owner (`OwnedTypeIdentity`,
  `ProviderCustomTypes`). Never
  infer ownership from `TypeIdentity::provider`, because AWSCC may own types in
  the shared `aws` namespace (the field is still public and read directly:
  carina#4542). The current protocol has no fields for attribute completions
  or resource wait defaults: those are host-only authoring/execution metadata
  and are the two intentional one-way omissions, pinned by the named
  `_v18_omits_*` bindings in the exhaustive destructures. The legacy
  `Custom` decoder preserves the first outer record's name as the only
  identity, including an empty name; nested `Custom.name` and primitive-base
  identities never fill it in. Enclosing String/int refinements take
  precedence over nested/base identity-layer metadata, falling back to the inner
  identity metadata only when the enclosing record omits it. Legacy `Custom`
  has no member layer: a terminal String/Int base with non-empty member metadata
  is incompatible and returns a typed decode error, regardless of outer fields.
  Anonymous legacy `Custom` records with a List base remain ordinary refined
  lists. A named List base is also rejected: core cannot retain that enclosing
  identity, so decode returns a typed error instead of silently dropping it.
  Metadata incompatible with the terminal shape remains a typed error. The
  `ResourceSchema`, `AttributeSchema`, and `StructField` conversions destructure
  both core parts and protocol records exhaustively (`OperationConfig` needs no
  conversion: carina-core re-exports the protocol type, carina#3907), and
  `AttributeType` conversion patterns name every variant field.
  Resource and Struct declarations use the same admitted rule type,
  with Struct paths relative to each object instance (including recursive defs).
  `InputPredicate::Secret` (`kind: "secret"`) applies to scalar String Value
  targets (checked at rule admission). Check failure first, then
  `NodeRef::scope()`, then knownness: Secret is proven even with a pending
  payload, known Public is a violation, and a pending Public value and every
  withheld target remain pending. An absent optional target uses the existing
  omission proof. Never extract or estimate the plaintext for Secret. The schema
  input rule is the sole owner. The record decodes at
  protocol 28, but providers must not declare it until the next protocol bump:
  a host predating it rejects the whole schema as an unknown variant (nothing
  gates this yet: carina#4544).
  The shared input-rule admission gate checks writable canonical paths, typed
  scalar literals, enum identity and canonical membership, predicate compatibility,
  duplicate target/code pairs, and a condition depth limit of 16. The one evaluator
  uses runtime knowledge for presence and equality; unknown guards remain pending,
  including under negation. Equality and string predicates share typed scalar
  extraction: enum aliases use canonical API values, and invalid known values
  remain failures. Public binding and site evaluation return complete typed
  findings and pending obligations; only the private single-rule evaluator
  returns a verdict. Dispatch retains every established finding. Each write site
  owns one immutable `RuleInput` and authored checks require `AuthoredInputs`;
  both are minted only at stage boundaries (`RuleInput` by compile_fail
  doctests, `AuthoredInputs` by private fields only). Evaluation
  consumes the stage's union-fit authority; unavailable provider validation
  remains pending and never selects a branch by structural declaration order
  alone.
  Recursive admission memoizes declaration/node judgements, including provider
  callbacks, ancestry and reference guards. Fit children share immutable witnesses
  rather than copying subtrees on memo hits. Schema walkers consume cached
  protected classifications through a scoped capability that protects results
  before returning; they never reopen those payloads for classification.
  Every successful module
  publication retains resource authority even if provider configuration fails;
  unavailable default-map membership remains pending. Missing authority is an
  explicit error, never an empty successful report. Authored reports retain each
  resource's preparation errors alongside findings and pending obligations;
  consumers exhaustively render their typed outcomes. Schema fields are read literally
  through immutable ordered declarations with a shared name index; ordinary
  validation, aliases and rule metadata use that index. Only declared rule paths use `InputPath` parsing.
  Alias binding covers Struct, List, Map, selected Union and Secret instances,
  retaining protection and authored locations. Canonical root identities precede
  patch, default-membership and ignore queries. Shared schema canonicalization
  uses mutable path prefixes and renders only actual renames or errors; authored
  location renames are indexed and applied in one batch.
  Guards, targets,
  instance enumeration and bound-expression estimation share the graph and
  ignore policy. Classification and diagnostic protection travel together on
  every graph edge. Key cursors preserve withheld completeness and masked
  membership; the estimator never recovers proof from an unsent map expression.
  Withheld ancestors carry protection without granting a target-node capability.
  Owner prefixes are shared, and diagnostic paths are materialized only for
  findings or pending results, which are then sorted. The shared validator also
  shares path prefixes; published diagnostic keys have immutable concrete storage.
  Diagnostic identity and ordering use structured segments; protected map keys
  retain opaque instance ordinals and redact only their presentation. Literal field names
  are quoted when needed. Parser-owned locations share one structured index,
  invalidated only by their owning mutation methods, and missing intermediate
  objects retain the deepest authored containing location. Dispatch secret restoration
  establishes plaintext correspondence once and carries paired child cursors.
  Only complete replacement nodes and
  explicit removals prove absence; unsent omissions remain withheld at every
  depth. Default membership belongs to that graph and is recomputed at dispatch.
  An erased normalized expression has no authored fallback without carried
  correspondence. Update inputs overlay the exact annotated patch on the full
  protected desired tree, never observed state (`UpdateRequest::checked` mints
  the sealed `ProtectedUpdateInput`). Both owners use
  the same write-scope classifier; `AuthoredDesired` evaluates `CreateOnly` as
  warnings with core-rendered rejected-if-created wording, while `EveryWrite`
  remains an error. The core finding derives severity from rule scope and write
  context once; CLI and LSP render that fact. Unknown guards remain pending.
  `InputRuleDisplay` owns declaration wording for provider docs and diagnostics.
  Its private requirement projection supplies predicate and condition text to
  both displays, with the same diagnostic escaping and truncation budget.
  Quoted declaration tokens share one inline quoting helper. Literals and
  conditions stream into the requirement's budget. Shared pattern rendering
  keeps ordinary regex sources verbatim between slashes, quotes sources needing
  escaping or containing `/ (`. Reasons needing escapes or containing
  parentheses or quotes use the same quoting helper. This distinguishes literal
  controls, regex backslash escapes, and metadata delimiters. Plan errors budget
  only their prefix
  and embed the already-budgeted requirement verbatim. `DiagnosticText` and
  the closed `DiagnosticEncoding` (one escaper, generated together with its
  inventory) hold sanitization and escaping. Inventory spellings stay unbounded so
  complete escaped secrets are matched before diagnostic truncation.
  Independent Rust str `Debug` echoes are inventoried with the standard
  formatter, stripping only the surrounding quotes. They must not pass through
  diagnostic sanitization or truncation before matching.
  Violations retain their declaration and a rejection/evaluation-failure kind;
  `InputViolationReason` has no `Display` or `Error` (compile_fail doctest).
  Render through the violation's `message()`, never invent a second predicate
  formatter. Only provider-declared parameters may appear; rejected values and
  protected instance keys must not enter the declaration display. Struct-local
  declaration paths stay relative to their declaring object.
  Plans reject `CreateOnly` violations only at create-capable write sites.
  Adding
  a protocol field must break compilation until it is consciously carried
  across or explicitly bound and documented as unavailable on the destination
  side. Never use a catch-all `_` or `..` at this seam; a silently dropped field
  is invisible in production and cost carina#3034 its effect (carina#3772).
  (Not enforced by a check yet: carina#4541, carina#4533.)
- **Provider diagnostics** (carina#4334): provider worlds import the WIT
  `diagnostics` and `protected-material` interfaces, and legacy packages are
  rejected before any guest code runs. The typed event channel
  (`ProviderEvent`, `BoundProviderEvent`), host-side masking, discard-only raw
  WASI stdio, the registered protected material (only AWS session
  credentials and GitHub tokens: `AwsSessionMaterial`, `AwsSdkParts`,
  `GithubTokenMaterial`), the buffered-body WASI HTTP transport, per-call
  budgets and the consumed `ProviderSessionBuilder` are held by private
  fields, compile_fail doctests and the provider diagnostics tests. The
  `provider-host` feature boundary, including the wasm32 `compile_error!`, is
  held by the provider-diagnostic-host-api fixtures of
  `scripts/check-provider-cause-test-api.sh` and by the manifest check in
  `scripts/check-provider-boundaries.sh`.
  The presenter consumes Bound segments and never constructs `HostText` or
  uses generic Bound error or partial-reason constructors; closing those older
  constructors remains #4301. The SDK panic hook is installed before
  construction and at the start of every export (each export calls
  `enter_export()`). Only the first panic per call bypasses count/rate
  limits, and panic text shares a 256 KiB input cap. The default sink shows
  Error/Warn/Info; Debug/Trace require `CARINA_WASI_HTTP_TRACE=1`, read once
  by the host and never forwarded to guests. `AwsSdkParts` guarantees paired
  installation on one AWS SDK config builder; later changes through AWS APIs
  remain under caller control. The in-repo provider manifest check is an
  additional lint; it does not resolve transitive features or police external
  provider repos. Native in-process providers are trusted code linked into
  the host binary, where host feature unification is accepted. Native
  `tokio::spawn`, `spawn_blocking` and other threads do not inherit a call
  scope; their events share a bounded session budget, and the last diagnostic
  owner flushes the suppressed count as one notice on drop. Every
  configured normalizer owns live diagnostic inputs and dispatches in a call
  scope, including fixtures. Custom-type validation inherits the host sink and
  schemas. Native OS output remains outside this channel. Diagnostics are not
  persisted and do not change the state format.
- **Provider error retryability** (carina#4350): Provider errors carry a required
  exclusive retryability class: Unclassified, Transient (including throttling),
  or DependencyBlocked. Message, cause and code never infer this class; the wire
  preserves the provider's typed verdict. Reference errors expose only
  AccessDenied and InvalidReference. Temporary resolution failures use
  InvalidReference with an inner Transient class; reference variants never
  encode retryability.
- **Desired normalization is per leaf** (carina#4194). The normalization opacity policy,
  `carina_core::resource::is_deferred_desired_leaf`, marks every
  `Value::Deferred` leaf inside concrete lists/maps. The SDK's `DesiredView`
  presents the same `DesiredValue` tree on the native and WASM routes. Deferred
  leaves are opaque `DeferredLeaf` values that providers cannot construct,
  clone or inspect (its compile_fail doctests carry no error code yet:
  carina#4534). On the wire they are host-issued slots (`{"$carina_deferred": <u32>}`
  inside normalization JSON, whose user keys starting with `$` are escaped
  only by `carina_provider_protocol::normalization`); the host
  restores each slot only after checking it occurs exactly once in its original
  resource and attribute. CRUD, config and persisted-state values never carry
  slots, and create/update still deliver real secret values. Don't reintroduce a top-level deferred-name list
  or let provider code see a deferred leaf's value.
- **Runtime value knowledge** (carina#4316) is classified by
  `Value::knowledge()` in `carina-core/src/resource/knowledge.rs`. Its proofs,
  stage markers (`Knowledge<Plan>` / `Knowledge<Apply>`, `PlanResolved`,
  `KnownValue::from_apply`) and crate-private visitors are held by sealed
  traits, private fields and the compile_fail doctests in that module.
  Knowledge grants no plaintext access or transport permission.
  `KnownValue::from_apply` takes only `Knowledge<Apply>`, but it is not the
  only way to obtain `KnownValue` (`Classification::into_known` also yields
  one).
  Resource and data-source dispatch classify their final attributes with
  `KnowledgeContext::apply(bindings)` and obtain proofs through
  `KnownValue::from_apply`. Structural serialization checks do not replace
  this dispatch gate.
  Syntax/type judgement and normalization opacity remain separate policies.
  Reference timing uses the resolver's shared typed
  plan-reference decision. Per-attribute validation keeps failed inputs present
  and continues checking their siblings. Lexical UserFunction body/local
  analysis shares immutable evidence within one classification, keyed by the
  existing function Arc identity. The private lookup is discarded on return;
  arguments and site paths remain specific to each call. Typed plan projection
  preserves that sharing without reclassifying source values. Saved candidates
  must be re-admitted by the admission owner.
- **LSP integration**: when adding Struct validation, update
  `carina-lsp/src/diagnostics/validation.rs` to validate nested fields;
  completion should work recursively for struct fields.

## Module Loading

Modules are directory-scoped. An import target must be a directory containing
one or more `.crn` files; single-file modules are rejected with
`ModuleError::NotADirectory`. Inside a module directory all `.crn` files are
merged uniformly — no file name (including `main.crn`) is privileged.

- CLI: `load_module()` / `ModuleResolver::load_module` require a directory path.
- LSP: Module loading in `diagnostics/checks.rs` handles directory modules for proper validation.

## Plan Concurrency Contract

<!-- constrained-by #key-abstractions -->

`carina plan` **takes no state lock.** `apply`/`destroy` acquire an
exclusive lock (`backend.acquire_lock(...)`); `plan` deliberately does
not. A lock on a read-only operation is overkill, and because the
backend lock API is exclusive-only it would serialize concurrent
`plan`s and let a long `plan` block deploys — too strong for a command
whose output is only a prediction. (Deliberate decision for issue #3111;
options considered were: (1) document as intentional, (2) acquire a
shared/read lock, (3) detect drift and warn. Option 3 was chosen — option
1 is weaker than Terraform and leaves the user unaware of staleness;
option 2 needs a shared-lock mode the backends do not have and is only
justified once a saved-plan/`plan -out`→`apply` workflow exists.)

Because `plan` reads state once at `T0` and computes the diff against
that snapshot with no lock held, a concurrent `apply`/`destroy` can
make the displayed plan stale (a TOCTOU window — *not* a torn read;
both backends write state atomically). To bound this, `plan`:

1. Fingerprints state at `T0` (`StateFile::serial` + `lineage`) right
   after the initial read — `StateSnapshot::capture` in
   `carina-cli/src/commands/plan.rs`.
2. Re-reads state just before display and compares
   (`detect_state_drift`). A `lineage` change (state recreated) is
   reported in preference to a `serial` bump (concurrent write).
3. On drift, prints a **warning** to stderr and still shows the plan.
   Drift is never fatal: the plan is a prediction, and `apply`
   re-acquires the lock and recomputes the diff before mutating
   anything, so final correctness is enforced on the `apply` side.

`plan --out <file>` still writes the saved plan even when drift is
detected (the warning goes to stderr; the plan file is the command's
product). This is safe because the saved-plan apply path
(`run_apply_from_plan_locked`) records the plan's `state_lineage` /
`state_serial` and, under the apply lock, hard-errors on a lineage
mismatch and warns on a serial bump before mutating — so a stale saved
plan cannot be silently applied.

The contract is "best-effort snapshot with drift warning", not "locked
read". A failed re-read is also a warning, not an error. Regression
coverage: pure classification in `commands::plan::state_drift_tests`;
the real `carina plan` path (concurrent writer slipped into the `T0..T1`
window via a deterministic file-handshake seam) in
`carina-cli/tests/plan_state_drift_e2e.rs`.

## Directory-scoped, never single-file

**Carina configurations are directory units.** Every feature that reads DSL
source — completion, diagnostics, validation, formatting, hover, code lens,
etc. — must consider sibling `.crn` files in the same directory as
first-class input, not as an afterthought. A `let` binding in `main.crn`
must be visible from `exports.crn`; a `provider` block in `providers.crn`
must apply to `main.crn`. Anything that looks at only one file is a bug.

When implementing or modifying such a feature:

1. **Acceptance test must use multiple files.** A unit test built from a
   single string is not sufficient evidence. Write a `tempfile::tempdir()`
   fixture that mirrors the real `infra/aws/management/<dir>/` shape —
   typically `main.crn` + `exports.crn` + `providers.crn` + `backend.crn` —
   and assert behavior under that shape. The bare-string variant is fine
   *in addition to* the directory variant, never *instead of* it.
2. **Real-infra smoke test.** Where feasible, run the built binary
   (`carina fmt`, `carina validate`, etc.) against `carina-rs/infra/`
   directly before declaring the issue done. The acceptance condition
   in the original issue almost always names a real path; respect it.
3. **API signal.** Helpers that take a single `&str` of source text
   (`extract_resource_bindings`, `extract_let_bindings`, similar
   text-scan utilities) silently invite single-file thinking. When you
   see one in the call path, the question to ask is "does this caller
   need the sibling files too?" — most of the time the answer is yes.

> Past breakage: PR #2120 (closed #2043 too early) shipped `exports`
> value-position completion that scanned only the current buffer; the real
> `exports.crn` references bindings in `main.crn`, so users got zero LSP
> candidates. PR #2121 fixed it by threading `base_path` into the same
> handler. Same class of mistake recurred from PR #2118 (closed #2117
> prematurely) where the formatter fix passed unit tests but never ran
> against the real infra fixture.

> Past breakage: PR #3393 made directory parser Pass 2 value-aware for
> sibling `.crn` let bindings but left Pass 1 single-file, so a
> sibling-`let` chain (`let B = A` referencing a sibling `let A = '...'`)
> degraded to `(known after apply)` past one hop. PR #3394 replaced the
> fixed-pass scaffolding with a fixed-point loop seeded from sibling
> `let` values: every pass that consumes the directory binding scope
> must consume it from a representation that has converged, not from a
> per-file snapshot.

---

# Part 4 — Git & Worktree Workflow

## PR Drafting

**In this repo, open PRs as non-draft** (plain `gh pr create`). The global
default of `gh pr create --draft` does not apply here.

## Worktree Workflow (REQUIRED)

- All PR work must happen in a git worktree, not the main repo.
- After `cd` into a worktree, verify cwd before running build/test commands.
- Never stash changes without explicit user instruction.
- Clean up worktrees after PR merge.

```bash
# Create worktree for a new task
git worktree add .worktrees/<branch-name> -b <branch-name> main

git worktree list

# Delete worktree after PR is merged (from the main worktree, not the feature worktree)
git worktree remove .worktrees/<branch-name>
```

## Submodule Initialization

This repo uses a git submodule for `carina-plugin-wit/`. After `git pull` or creating a new worktree, initialize the submodule:

```bash
git submodule update --init --recursive
```

Without this, builds fail because `wit_bindgen::generate!` cannot find the WIT files.

## Branch Cleanup

After merging a PR:
```bash
git checkout main
git pull
git branch -d <feature-branch>    # Delete local branch
git remote prune origin           # Remove stale remote tracking branches
```

---

# Part 5 — Multi-Agent Workflows

Claude Code's `Workflow` tool runs a deterministic script that fans
work out across many subagents (parallel finders, adversarial
verifiers, pipeline stages). It can spawn dozens of agents and consume
a large amount of tokens, so it is **opt-in**: only run it when the
work explicitly calls for multi-agent orchestration. Default to a
single agent (or the `Agent` tool for one focused subtask) otherwise.

High-value patterns in this repo:

- **Broad audits / sweeps.** AWS-error-display style sweeps,
  enum-alias/`snake_case` convention sweeps, or any "apply the same
  mechanical change across N sibling sites" task. Fan out one agent
  per file/site, verify each independently. Pairs with the root-cause
  rule: use a workflow to *find every sibling site* so the one upstream
  fix is provably complete, not to patch each site.
- **Blast-radius investigation before a typed reshape.** When deciding
  whether a newtype/typestate reshape lands in-PR or as a follow-up,
  fan out readers across the affected crates to enumerate every call site
  in parallel.
- **Multi-repo checks.** pick-issue / meta-tracker work that must scan
  all three carina-rs repos — one agent per repo, then synthesize.
- **Adversarial self-review.** The 5-round self-review can be structured
  as a workflow: independent reviewers per dimension (correctness,
  type-safety, root-cause vs symptom), each finding verified by a skeptic
  prompted to refute it.

Constraints specific to this repo:

- A workflow does NOT relax any gate. Each agent still runs the local
  checks described in the Verify Protocol, and PR CI must be green. Findings
  still need a reproducing test before a fix lands.
- Agents that mutate files in parallel must use `isolation: 'worktree'`
  to avoid clobbering each other; read-only finders do not need it.
- Have agents return structured output (the `schema` option) rather
  than prose when you will post-process their results.
