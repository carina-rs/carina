# AGENTS.md

## Command Execution

- Prefer crate-specific commands over workspace-wide commands unless the change spans multiple crates.
- For long-running Rust commands executed via Codex shell tools, increase the initial wait time instead of polling too aggressively.
- Use `yield_time_ms >= 30000` for crate-scoped `cargo test` / `cargo build`.
- Use `yield_time_ms >= 120000` for `cargo test --workspace`, `cargo build`, and other workspace-wide commands.
- Wrap long-running test/build commands in `timeout` when possible so hung runs fail explicitly instead of leaving an open session.
- If a shell command returns a running session, poll that session instead of starting the same command again.

## Rust Test Strategy

- Start with `cargo test -p <crate>` whenever possible.
- Use `cargo test --workspace` only for cross-crate changes or final verification.
- After changing state-management or apply/destroy flows, run at least `cargo test -p carina-state` and `cargo test -p carina-cli`.
- After changing parser, schema, formatter, or completion/diagnostics behavior, run the most relevant crate tests first (`carina-core`, `carina-lsp`, provider crate) before considering a workspace run.

## AWS And Acceptance Tests

- Use `aws-vault exec mizzy --` for commands that require AWS credentials.
- Do not run acceptance tests or real cloud-mutating commands unless the user explicitly asks for them.

## Reviews And Issue Filing

- For review requests, prioritize correctness risks in state persistence, locking, provider error handling, and apply/destroy recovery paths.
- When creating a GitHub issue or PR, search existing issues/PRs first to avoid duplicates.
- Write all GitHub issues and pull requests in English, including titles and bodies.
- Prefer `gh issue create --body-file <file>` or `gh pr create --body-file <file>` over inline multi-line shell quoting.

## High-Risk Areas

- Changes under `carina-state/` or `carina-cli` apply/save paths must preserve atomic lock behavior and avoid writing stale state after partial failures.
- Changes that affect DSL syntax or schema resolution should also be checked against LSP behavior and docs expectations.

<!-- dagayn MCP tools -->
## MCP Tools: dagayn

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
dagayn MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
| ------ | ---------- |
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.

<!-- dagayn markdown policy -->
## Markdown documentation policy: declare dependencies via directive comments

When authoring or editing a Markdown document in this repository, declare
inter-section and inter-document dependencies as HTML directive comments so
they are captured by the dagayn graph (`DEPENDS_ON` / `IMPORTS_FROM` edges)
and discoverable via `query_graph` / `get_impact_radius`.

### Required form

```markdown
<!-- <kind> <target> -->
```

`<kind>` MUST be one of: `constrained-by`, `blocked-by`, `supersedes`,
`derived-from`. Choose the kind whose semantics best match the dependency:

| Kind | Use when |
| ---- | -------- |
| `constrained-by` | This section's design is bounded by the referenced document/section |
| `blocked-by` | This item cannot proceed until the referenced item resolves |
| `supersedes` | This document replaces the referenced content |
| `derived-from` | This section is derived from the referenced source |

### Three target shapes

| Dependency type | Target syntax | Example |
| --------------- | ------------- | ------- |
| Within-document section | `#section-slug` | `<!-- derived-from #background -->` |
| Other document (whole file) | `./relative/path.md` | `<!-- blocked-by ./specs/open-issue.md -->` |
| Other document + section | `./path.md#slug` | `<!-- constrained-by ./adr.md#context -->` |

Slugs follow GitHub Markdown rules: lowercase, non-alphanumerics removed,
spaces and hyphens collapsed to `-`. Place the directive immediately under
the heading whose content depends on the target. External URLs
(`http://`, `https://`) are not graph-resolvable — keep them as ordinary
Markdown links, not directive targets.

### When to add a directive

- Section design references an ADR, spec, or research note → `constrained-by` or `derived-from`.
- A document replaces an older one → `supersedes` (place in the new document).
- A spec/task section is blocked on another being resolved → `blocked-by`.
- A later section extends an earlier one non-obviously → `derived-from #earlier-section`.

If no real dependency exists, do not invent one. Directives are signal, not decoration.
