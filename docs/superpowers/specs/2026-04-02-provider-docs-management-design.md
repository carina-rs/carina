# Provider Documentation Management After Repository Separation

## Background

Carina's AWS and AWSCC providers have been separated into their own repositories (`carina-rs/carina-provider-aws`, `carina-rs/carina-provider-awscc`). The documentation site (Starlight/Astro) remains in the main `carina-rs/carina` repository, and provider reference docs are still committed as static markdown files in `docs/src/content/docs/reference/providers/`.

Currently:

- `carina-provider-aws` has `scripts/generate-docs.sh` but it depends on `carina-codegen-aws` in the main repo and writes to the main repo's directory structure
- `carina-provider-awscc` has `src/bin/codegen.rs` with `--format markdown` support but no docs generation script or workflow
- `carina-codegen-aws` (Smithy-based codegen) remains in the main repo with no other consumer

## Decision

### Documentation site stays in the main repo

The docs site covers core Carina content (DSL syntax, CLI reference, guides) that naturally belongs in the main repo. Provider reference docs are a subset of the site, not independent sites.

### Provider repos push docs via CI-generated PRs

When a provider's schemas or codegen change, the provider repo's CI generates markdown and creates a PR against the main repo. This keeps generated docs in git (reviewable, diffable) and avoids build-time external dependencies.

### Codegen moves to provider repos

`carina-codegen-aws` moves into `carina-provider-aws`. This matches the pattern `carina-provider-awscc` already uses (codegen built into the repo). The main repo will have no provider-specific codegen.

## Architecture

```
carina-provider-aws (push to main)
  → CI: smithy-codegen --format markdown
  → CI: Create PR to carina/docs/src/content/docs/reference/providers/aws/
  → Review & merge in main repo
  → docs.yml: Starlight build → carina-rs.github.io

carina-provider-awscc (push to main)
  → CI: codegen --format markdown
  → CI: Create PR to carina/docs/src/content/docs/reference/providers/awscc/
  → Review & merge in main repo
  → docs.yml: Starlight build → carina-rs.github.io
```

## Changes Required

### Phase 1: Move codegen to carina-provider-aws

1. Move `carina-codegen-aws/` crate into `carina-provider-aws` repository
2. Update `carina-provider-aws/Cargo.toml` workspace to include the codegen crate
3. Update `scripts/generate-docs.sh` to:
   - Use the local codegen binary instead of the main repo's
   - Output to a local `generated-docs/` directory (not main repo paths)
   - Keep Starlight frontmatter injection and example insertion logic
4. Update `scripts/generate-provider.sh` and `scripts/generate-schemas-smithy.sh` for local codegen paths
5. Remove `carina-codegen-aws/` from the main repo's workspace

### Phase 2: Add docs workflow to carina-provider-aws

New workflow `.github/workflows/docs.yml`:

```yaml
name: Update docs
on:
  push:
    branches: [main]
    paths:
      - "src/**"
      - "scripts/**"
      - "carina-codegen-aws/**"

jobs:
  update-docs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Generate markdown docs
        run: ./scripts/generate-docs.sh

      - uses: actions/checkout@v4
        with:
          repository: carina-rs/carina
          token: ${{ secrets.DOCS_PR_TOKEN }}
          path: carina-main

      - name: Copy generated docs
        run: |
          rm -rf carina-main/docs/src/content/docs/reference/providers/aws/
          cp -r generated-docs/ carina-main/docs/src/content/docs/reference/providers/aws/

      - name: Create PR
        uses: peter-evans/create-pull-request@v7
        with:
          path: carina-main
          token: ${{ secrets.DOCS_PR_TOKEN }}
          branch: docs/update-aws-reference
          title: "docs: update aws provider reference"
          body: |
            Auto-generated from carina-provider-aws.
          commit-message: "docs: update aws provider reference"
```

Key behaviors:
- Branch name `docs/update-aws-reference` is fixed so subsequent pushes update the same PR instead of creating duplicates
- `rm -rf` before copy ensures deleted resources are reflected in docs

### Phase 3: Add docs generation to carina-provider-awscc

1. Create `scripts/generate-docs.sh` using the existing `codegen` binary with `--format markdown`
2. Add Starlight frontmatter post-processing (same logic as aws)
3. Add `.github/workflows/docs.yml` (same structure, targeting `providers/awscc/`)

### Phase 4: Clean up main repo

1. Remove `carina-codegen-aws/` from workspace members in `Cargo.toml`
2. Delete `carina-codegen-aws/` directory
3. Keep `carina-aws-types/` in the main repo (both provider repos depend on it via git)

## GitHub Authentication

- Create a fine-grained PAT or GitHub App with:
  - `contents: write` on `carina-rs/carina`
  - `pull_requests: write` on `carina-rs/carina`
- Store as `DOCS_PR_TOKEN` secret in both provider repos

## PR Conventions

| Field | Value |
|-------|-------|
| Branch | `docs/update-{provider}-reference` |
| Title | `docs: update {provider} provider reference` |
| Behavior | Force-push to same branch if PR already open |
