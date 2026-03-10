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
- Prefer `gh issue create --body-file <file>` or `gh pr create --body-file <file>` over inline multi-line shell quoting.

## High-Risk Areas

- Changes under `carina-state/` or `carina-cli` apply/save paths must preserve atomic lock behavior and avoid writing stale state after partial failures.
- Changes that affect DSL syntax or schema resolution should also be checked against LSP behavior and docs expectations.
