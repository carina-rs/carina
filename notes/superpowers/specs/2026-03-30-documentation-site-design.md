# Documentation Site Design

## Background

Carina's current documentation site (carina-rs.github.io) is built with mdBook and contains only auto-generated provider reference docs (AWSCC 23 resources, AWS 13 resources) and a brief introduction page. There are no guides, tutorials, DSL language reference, or CLI command reference. Users must rely on the README or read source code to understand how to use Carina.

## Goal

Build a comprehensive documentation site that serves two primary audiences:

1. **New users with IaC experience** — need to learn Carina's DSL, concepts, and workflow
2. **Existing users** — need a reference for DSL syntax, CLI commands, and provider resources

## Tool: Starlight (Astro)

Migrate from mdBook to Starlight for:

- Rich components (tabs, callouts, code groups) useful for multi-provider examples and warnings
- Built-in search (Pagefind — fast, local, no external service)
- Auto-generated sidebar from file structure (replaces manual SUMMARY.md)
- Shiki syntax highlighting with custom grammar support (for `.crn` files)

The Node.js dependency only affects the docs build pipeline, not the Rust codebase.

## Site Structure

```
Getting Started
├── Installation
├── Quick Start (first resource creation)
└── Core Concepts (Effect, Plan, Provider — minimal explanation)

Guides
├── Writing Resources
├── Using Modules
├── State Management
├── For / If Expressions
├── Functions
└── LSP Setup

Reference
├── DSL Language
│   ├── Syntax Overview
│   ├── Types & Values
│   ├── Expressions (for, if, let, pipe)
│   ├── Built-in Functions
│   └── Modules (arguments, attributes)
├── CLI Commands
│   ├── plan
│   ├── apply
│   ├── validate
│   ├── state (import, remove, move)
│   └── module info
└── Providers
    ├── AWSCC (migrated auto-generated docs)
    └── AWS (migrated auto-generated docs)
```

**Guides vs Reference:**

- Guides = "how to do X" (task-oriented, read in order)
- Reference = "specification of X" (dictionary-style, look up as needed)

## Directory Layout

```
docs/
├── astro.config.mjs
├── package.json
├── src/
│   └── content/
│       └── docs/
│           ├── index.mdx
│           ├── getting-started/
│           │   ├── installation.md
│           │   ├── quick-start.md
│           │   └── core-concepts.md
│           ├── guides/
│           │   ├── writing-resources.md
│           │   ├── using-modules.md
│           │   ├── state-management.md
│           │   ├── for-if-expressions.md
│           │   ├── functions.md
│           │   └── lsp-setup.md
│           └── reference/
│               ├── dsl/
│               │   ├── syntax.md
│               │   ├── types-and-values.md
│               │   ├── expressions.md
│               │   ├── built-in-functions.md
│               │   └── modules.md
│               ├── cli/
│               │   ├── plan.md
│               │   ├── apply.md
│               │   ├── validate.md
│               │   ├── state.md
│               │   └── module-info.md
│               └── providers/
│                   ├── awscc/
│                   │   └── (migrated 36 files)
│                   └── aws/
│                       └── (migrated 13 files)
├── public/
└── custom-grammars/
    └── crn.tmLanguage.json
```

## Migration Plan

### Phase 1: Starlight setup + provider docs migration

- Initialize Starlight project in `docs/`
- Create `.crn` TextMate grammar for syntax highlighting
  - Keywords: `provider`, `let`, `import`, `fn`, `for`, `if`, `module`, `arguments`, `attributes`, `backend`, `removed`, `moved`
  - Register as custom Shiki language in `astro.config.mjs`
- Migrate existing provider Markdown files to `docs/src/content/docs/reference/providers/`
- Add Starlight frontmatter (title, description) to each provider doc
- Update provider doc generation scripts:
  - Change output directory to Starlight path
  - Add frontmatter generation to each file
  - Remove `generate-summary.sh` (Starlight auto-generates sidebar)
- Update CI (`.github/workflows/docs.yml`):
  - Add Node.js setup step
  - Change build command to `npm run build` in `docs/`
  - Deploy target remains `carina-rs/carina-rs.github.io`
- Remove mdBook config (`book.toml`, `theme/`, `book/`)
- Create minimal `index.mdx` landing page

### Phase 2: Getting Started + Guides

- Write Getting Started section (installation, quick start, core concepts)
- Write Guides section (writing resources, using modules, state management, for/if expressions, functions, LSP setup)

### Phase 3: DSL Language Reference

- Write DSL reference (syntax, types & values, expressions, built-in functions, modules)

### Phase 4: CLI Command Reference

- Write CLI reference (plan, apply, validate, state, module info)

Each phase produces a deployable site. Content priority: Guides > DSL Reference > CLI Reference.

## Provider Doc Generation Changes

Current scripts (`carina-provider-awscc/scripts/generate-docs.sh`, `carina-provider-aws/scripts/generate-docs.sh`) output Markdown to `docs/src/providers/`. Changes needed:

- Output to `docs/src/content/docs/reference/providers/{awscc,aws}/`
- Prepend Starlight frontmatter to each generated file:
  ```
  ---
  title: "EC2 VPC"
  description: "AWSCC EC2 VPC resource reference"
  ---
  ```
- Remove calls to `generate-summary.sh`
- Example injection from `examples/` directories continues unchanged

## CI/CD

```yaml
# .github/workflows/docs.yml
name: Deploy docs
on:
  push:
    branches: [main]
    paths: ['docs/**']

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
      - run: npm ci
        working-directory: docs
      - run: npm run build
        working-directory: docs
      - uses: peaceiris/actions-gh-pages@v4
        with:
          external_repository: carina-rs/carina-rs.github.io
          publish_branch: main
          publish_dir: docs/dist
          personal_token: ${{ secrets.DOCS_DEPLOY_TOKEN }}
```

## Scope

### In scope

- Starlight project setup and configuration
- `.crn` syntax highlighting (TextMate grammar)
- Provider docs migration (file move + frontmatter addition)
- Provider doc generation script updates
- CI/CD pipeline update
- Landing page
- Getting Started section content
- Guides section content
- DSL Language Reference content
- CLI Command Reference content

### Out of scope

- Doc versioning (can be added later when Carina has stable releases)
- i18n / Japanese translation (can be added later using Starlight's i18n support)
- API documentation for crate internals (for contributors, separate concern)
- New provider resource documentation (separate from site infrastructure)

## Key Files

- `docs/astro.config.mjs` — Starlight configuration, sidebar, custom language
- `docs/custom-grammars/crn.tmLanguage.json` — .crn syntax highlighting
- `carina-provider-awscc/scripts/generate-docs.sh` — AWSCC doc generation
- `carina-provider-aws/scripts/generate-docs.sh` — AWS doc generation
- `.github/workflows/docs.yml` — CI/CD pipeline
