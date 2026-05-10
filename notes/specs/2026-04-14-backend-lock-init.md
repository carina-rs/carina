# Make `carina init` mandatory for project initialization

## Goal

Make `carina init` the single entry point for project initialization: plugin installation (all source types) and backend lock creation. Remove implicit initialization from validate/plan/apply. Move backend lock to project root for git tracking.

## Problems

### Backend lock not tracked in git
`.carina/backend-lock.json` is created during apply, inside the `.carina/` directory which may be gitignored. In CI, the file is ephemeral, so backend change detection doesn't work across runs.

### Plugin auto-download
`resolve_single_config()` auto-downloads plugins during validate/plan/apply. This is non-deterministic, surprising for read-only operations, and inconsistent.

### `file://` plugins not normalized
`file://` source plugins are referenced by external path at runtime, creating a dependency on the build environment.

## Solution

### `carina init` does all initialization

1. **All plugins** — regardless of source (`github.com/...` or `file://`) — are resolved and placed in `.carina/providers/`
2. **Backend lock** created at `carina-backend.lock` (project root, not `.carina/`)

### validate/plan/apply/destroy require init

- Missing plugin → error: `Provider '{}' not installed. Run 'carina init'.`
- Missing backend lock + backend configured → error: `Backend lock not found. Run 'carina init'.`
- No backend configured + no remote plugins → init not required (pure local project)

### File layout

```
project/
├── main.crn
├── carina-providers.lock    ← git tracked (existing, version pins)
├── carina-backend.lock      ← git tracked (NEW location)
├── carina.state.json        ← git tracked (local backend)
├── .carina/                 ← fully gitignored
│   └── providers/           ← downloaded/copied plugin binaries
└── .gitignore
```

### CI workflow

```yaml
- run: carina init       # downloads plugins from lock, creates backend lock
- run: carina plan .
```

`carina-providers.lock` ensures reproducible plugin versions across environments.

## Changes

### Task 1: Move backend lock to project root

- `carina-state/src/backend_lock.rs` — change `LOCK_DIR` / `LOCK_FILE` to write `carina-backend.lock` at project root
- Migration: `check_backend_lock()` checks both old (`.carina/backend-lock.json`) and new location; reads from old if new doesn't exist
- Update tests

### Task 2: Create backend lock in init, remove from apply/destroy

- `carina-cli/src/commands/init.rs` — add `ensure_backend_lock()` call after plugin resolution
- `carina-cli/src/commands/apply.rs` — remove `ensure_backend_lock()` (2 call sites)
- `carina-cli/src/commands/destroy.rs` — remove `ensure_backend_lock()` (1 call site)
- `carina-cli/src/commands/mod.rs` — `check_backend_lock()` returns error when lock missing + backend configured
- Update tests

### Task 3: Copy `file://` plugins into `.carina/providers/`

- `carina-provider-resolver` — `resolve_all()` copies `file://` sources into `.carina/providers/`
- Runtime looks in `.carina/providers/` only, regardless of original source

### Task 4: Remove auto-download from validate/plan/apply

- `carina-cli/src/wiring.rs` — replace `resolve_single_config()` with find-only lookup in `.carina/providers/`
- `carina-provider-resolver` — add `find_installed()` that checks `.carina/providers/` without downloading
- On missing plugin, error: `Provider '{}' not installed. Run 'carina init'.`
- Update tests

## Migration

Existing projects will get an error on next validate/plan/apply. Fix: run `carina init` once. This is a breaking change but intentional — makes initialization explicit.
