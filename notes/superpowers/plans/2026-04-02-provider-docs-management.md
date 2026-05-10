# Provider Documentation Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable provider repos to independently generate and publish reference documentation to the main Carina docs site via CI-generated PRs.

**Architecture:** Each provider repo generates markdown docs using its own codegen, then a GitHub Actions workflow creates a PR against the main carina repo's `docs/src/content/docs/reference/providers/` directory. `carina-codegen-aws` and `carina-smithy` move from the main repo into the provider-aws repo.

**Tech Stack:** Rust (codegen), GitHub Actions, peter-evans/create-pull-request action, shell scripts

---

### Task 1: Move carina-smithy and carina-codegen-aws to carina-provider-aws

Both `carina-smithy` and `carina-codegen-aws` are only used by the aws provider. Move them into the `carina-provider-aws` repo and convert it to a Cargo workspace.

**Files (in carina-provider-aws repo):**
- Create: `carina-smithy/Cargo.toml` (copy from main repo)
- Create: `carina-smithy/src/` (copy from main repo)
- Create: `carina-codegen-aws/Cargo.toml` (copy from main repo, update carina-smithy path)
- Create: `carina-codegen-aws/src/` (copy from main repo)
- Modify: `Cargo.toml` (convert to workspace)

- [ ] **Step 1: Copy carina-smithy into provider-aws repo**

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws
cp -r /Users/mizzy/src/github.com/carina-rs/carina/carina-smithy .
```

- [ ] **Step 2: Copy carina-codegen-aws into provider-aws repo**

```bash
cp -r /Users/mizzy/src/github.com/carina-rs/carina/carina-codegen-aws .
```

- [ ] **Step 3: Update carina-codegen-aws/Cargo.toml path dependency**

The path to carina-smithy changes from `../carina-smithy` to `../carina-smithy` (same relative path, but now within the provider-aws workspace). Verify this is correct.

- [ ] **Step 4: Convert carina-provider-aws to a Cargo workspace**

Replace the top-level `Cargo.toml` with a workspace configuration. The current `Cargo.toml` has the provider package definition at the root. Convert to:

```toml
[workspace]
members = [
    "carina-provider-aws",
    "carina-codegen-aws",
    "carina-smithy",
]
resolver = "2"
```

Then move the existing provider code into a `carina-provider-aws/` subdirectory:

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws
mkdir -p carina-provider-aws/src
mv src/* carina-provider-aws/src/
# Move Cargo.toml provider package config to sub-crate
# (preserve the existing package definition, just move it)
```

Create `carina-provider-aws/Cargo.toml` with the existing package definition (move from root, update paths).

**Note:** This restructuring requires careful handling of:
- `scripts/` directory (stays at workspace root)
- `examples/` directory (move to sub-crate or keep at root)
- `acceptance-tests/` directory (stays at workspace root)
- `.github/workflows/` (stays at workspace root)

- [ ] **Step 5: Build and test the workspace**

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws
cargo build
cargo test
cargo build -p carina-codegen-aws
```

Expected: All builds and tests pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: convert to workspace, add carina-codegen-aws and carina-smithy"
```

### Task 2: Update generation scripts in carina-provider-aws

The existing scripts reference `cargo run -p carina-codegen-aws` which now works within the local workspace. Update output paths to be self-contained.

**Files (in carina-provider-aws repo):**
- Modify: `scripts/generate-docs.sh`
- Modify: `scripts/generate-provider.sh`
- Modify: `scripts/generate-schemas-smithy.sh`

- [ ] **Step 1: Update generate-docs.sh**

Key changes:
- `PROJECT_ROOT` should point to the provider-aws repo root (not a parent monorepo)
- `DOCS_DIR` should be `generated-docs/aws` (local output, not main repo path)
- `EXAMPLES_DIR` should point to `carina-provider-aws/examples` (within workspace)
- Remove the `cd "$PROJECT_ROOT"` that assumed monorepo context
- `cargo run -p carina-codegen-aws --bin smithy-codegen` remains the same (works in workspace)

```bash
#!/bin/bash
# Generate aws provider documentation from Smithy models
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DOCS_DIR="$PROJECT_ROOT/generated-docs/aws"
EXAMPLES_DIR="$PROJECT_ROOT/carina-provider-aws/examples"
rm -rf "$DOCS_DIR"
mkdir -p "$DOCS_DIR"

# Download models if needed
"$SCRIPT_DIR/download-smithy-models.sh"

echo "Generating aws provider documentation..."
echo "Output directory: $DOCS_DIR"
echo ""

cd "$PROJECT_ROOT"
cargo run -p carina-codegen-aws --bin smithy-codegen -- \
  --model-dir "$PROJECT_ROOT/carina-provider-aws/tests/fixtures/smithy" \
  --output-dir "$DOCS_DIR" \
  --format markdown

# Prepend Starlight frontmatter to all generated docs
for DOC_FILE in "$DOCS_DIR"/*/*.md; do
    [ -f "$DOC_FILE" ] || continue
    DSL_NAME=$(head -1 "$DOC_FILE" | sed 's/^# *//')
    SERVICE_DIR=$(basename "$(dirname "$DOC_FILE")")
    SERVICE_DISPLAY=$(echo "$SERVICE_DIR" | tr '[:lower:]' '[:upper:]')
    RESOURCE_NAME=$(basename "$DOC_FILE" .md)
    FRONTMATTER_TMPFILE=$(mktemp)
    {
        echo "---"
        echo "title: \"$DSL_NAME\""
        echo "description: \"AWS $SERVICE_DISPLAY $RESOURCE_NAME resource reference\""
        echo "---"
        echo ""
        sed '1{/^# /d;}' "$DOC_FILE"
    } > "$FRONTMATTER_TMPFILE"
    mv "$FRONTMATTER_TMPFILE" "$DOC_FILE"
done

# Insert examples into generated docs
for DOC_FILE in "$DOCS_DIR"/*/*.md; do
    SERVICE_DIR=$(basename "$(dirname "$DOC_FILE")")
    RESOURCE_NAME=$(basename "$DOC_FILE" .md)
    EXAMPLE_FILE="$EXAMPLES_DIR/${SERVICE_DIR}_${RESOURCE_NAME}/main.crn"
    if [ -f "$EXAMPLE_FILE" ]; then
        EXAMPLE_TMPFILE=$(mktemp)
        {
            echo "## Example"
            echo ""
            echo '```crn'
            sed -n '/^provider /,/^}/!p' "$EXAMPLE_FILE" | \
                sed '/^#/d' | \
                sed '/./,$!d'
            echo '```'
            echo ""
        } > "$EXAMPLE_TMPFILE"
        MERGED_TMPFILE=$(mktemp)
        while IFS= read -r line || [ -n "$line" ]; do
            if [ "$line" = "## Argument Reference" ]; then
                cat "$EXAMPLE_TMPFILE"
            fi
            printf '%s\n' "$line"
        done < "$DOC_FILE" > "$MERGED_TMPFILE"
        mv "$MERGED_TMPFILE" "$DOC_FILE"
        rm -f "$EXAMPLE_TMPFILE"
    fi
done

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
```

- [ ] **Step 2: Update generate-provider.sh**

Update `PROJECT_ROOT` and paths to work within the workspace. The `--model-dir` and `--output-dir` paths need updating to reflect the workspace structure where provider code is in `carina-provider-aws/`.

- [ ] **Step 3: Update generate-schemas-smithy.sh**

Same path updates as generate-provider.sh.

- [ ] **Step 4: Update download-smithy-models.sh**

Update the fixture path to `carina-provider-aws/tests/fixtures/smithy/` if the tests directory moved into the sub-crate.

- [ ] **Step 5: Add generated-docs/ to .gitignore**

```bash
echo "generated-docs/" >> .gitignore
```

- [ ] **Step 6: Test docs generation**

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-aws
./scripts/generate-docs.sh
ls generated-docs/aws/s3/
```

Expected: `bucket.md` exists with Starlight frontmatter and content.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: update generation scripts for workspace structure"
```

### Task 3: Add docs workflow to carina-provider-aws

**Files (in carina-provider-aws repo):**
- Create: `.github/workflows/docs.yml`

- [ ] **Step 1: Create the docs workflow**

```yaml
name: Update docs

on:
  push:
    branches: [main]
    paths:
      - "carina-provider-aws/src/**"
      - "carina-codegen-aws/**"
      - "carina-smithy/**"
      - "scripts/**"
  workflow_dispatch:

jobs:
  update-docs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

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
          mkdir -p carina-main/docs/src/content/docs/reference/providers/aws/
          cp -r generated-docs/aws/* carina-main/docs/src/content/docs/reference/providers/aws/

      - name: Create PR
        uses: peter-evans/create-pull-request@v7
        with:
          path: carina-main
          token: ${{ secrets.DOCS_PR_TOKEN }}
          branch: docs/update-aws-reference
          title: "docs: update aws provider reference"
          body: |
            Auto-generated from carina-provider-aws.

            Triggered by ${{ github.event.head_commit.url }}
          commit-message: "docs: update aws provider reference"
```

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/docs.yml
git commit -m "ci: add docs update workflow"
```

**Note:** The `DOCS_PR_TOKEN` secret must be configured in the repo settings. This requires a fine-grained PAT or GitHub App token with `contents: write` and `pull_requests: write` on `carina-rs/carina`.

### Task 4: Add docs generation to carina-provider-awscc

The awscc provider already has `src/bin/codegen.rs` with `--format markdown` support. Create a generation script and workflow.

**Files (in carina-provider-awscc repo):**
- Create: `scripts/generate-docs.sh`
- Create: `.github/workflows/docs.yml`

- [ ] **Step 1: Understand the awscc codegen CLI**

The codegen binary accepts CloudFormation type names and schema JSON. It processes one type at a time. The script needs to iterate over all supported resource types.

Check which resources are currently supported by looking at the existing docs in the main repo:

```bash
ls /Users/mizzy/src/github.com/carina-rs/carina/docs/src/content/docs/reference/providers/awscc/
```

And cross-reference with the schema cache:

```bash
ls /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc/cfn-schema-cache/
```

- [ ] **Step 2: Create scripts/generate-docs.sh**

The script must:
1. Iterate over supported CloudFormation resource types
2. Run `cargo run --bin codegen` with `--format markdown` for each
3. Add Starlight frontmatter to each generated file
4. Output to `generated-docs/awscc/`

```bash
#!/bin/bash
# Generate awscc provider documentation from CloudFormation schemas
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DOCS_DIR="$PROJECT_ROOT/generated-docs/awscc"
SCHEMA_DIR="$PROJECT_ROOT/cfn-schema-cache"
rm -rf "$DOCS_DIR"
mkdir -p "$DOCS_DIR"

cd "$PROJECT_ROOT"

# Generate docs for each schema file
for SCHEMA_FILE in "$SCHEMA_DIR"/*.json; do
    [ -f "$SCHEMA_FILE" ] || continue
    FILENAME=$(basename "$SCHEMA_FILE" .json)

    # Convert filename to CloudFormation type name
    # e.g., aws-ec2-vpc.json → AWS::EC2::VPC
    TYPE_NAME=$(echo "$FILENAME" | sed 's/-/::/g' | sed 's/\b\(.\)/\u\1/g')

    # Derive output path: aws-ec2-vpc → ec2/vpc
    SERVICE=$(echo "$FILENAME" | cut -d'-' -f2)
    RESOURCE=$(echo "$FILENAME" | cut -d'-' -f3-)
    mkdir -p "$DOCS_DIR/$SERVICE"
    OUTPUT_FILE="$DOCS_DIR/$SERVICE/$RESOURCE.md"

    echo "Generating: $TYPE_NAME → $OUTPUT_FILE"
    cargo run --bin codegen -- \
        --file "$SCHEMA_FILE" \
        --type-name "$TYPE_NAME" \
        --format markdown \
        --output "$OUTPUT_FILE" 2>/dev/null || {
        echo "  Warning: failed to generate docs for $TYPE_NAME, skipping"
        continue
    }

    # Add Starlight frontmatter
    if [ -f "$OUTPUT_FILE" ]; then
        DSL_NAME=$(head -1 "$OUTPUT_FILE" | sed 's/^# *//')
        SERVICE_DISPLAY=$(echo "$SERVICE" | tr '[:lower:]' '[:upper:]')
        FRONTMATTER_TMPFILE=$(mktemp)
        {
            echo "---"
            echo "title: \"$DSL_NAME\""
            echo "description: \"AWSCC $SERVICE_DISPLAY $RESOURCE resource reference\""
            echo "---"
            echo ""
            sed '1{/^# /d;}' "$OUTPUT_FILE"
        } > "$FRONTMATTER_TMPFILE"
        mv "$FRONTMATTER_TMPFILE" "$OUTPUT_FILE"
    fi
done

echo ""
echo "Done! Generated documentation in $DOCS_DIR"
```

**Note:** The exact iteration logic and type-name derivation may need adjustment based on the actual schema file naming convention and codegen CLI behavior. Verify by running manually first.

- [ ] **Step 3: Add generated-docs/ to .gitignore**

```bash
echo "generated-docs/" >> .gitignore
```

- [ ] **Step 4: Test docs generation**

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina-provider-awscc
chmod +x scripts/generate-docs.sh
./scripts/generate-docs.sh
ls generated-docs/awscc/ec2/
```

Expected: Markdown files for ec2 resources.

- [ ] **Step 5: Create .github/workflows/docs.yml**

```yaml
name: Update docs

on:
  push:
    branches: [main]
    paths:
      - "src/**"
      - "scripts/**"
      - "cfn-schema-cache/**"
  workflow_dispatch:

jobs:
  update-docs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Generate markdown docs
        run: ./scripts/generate-docs.sh

      - uses: actions/checkout@v4
        with:
          repository: carina-rs/carina
          token: ${{ secrets.DOCS_PR_TOKEN }}
          path: carina-main

      - name: Copy generated docs
        run: |
          rm -rf carina-main/docs/src/content/docs/reference/providers/awscc/
          mkdir -p carina-main/docs/src/content/docs/reference/providers/awscc/
          cp -r generated-docs/awscc/* carina-main/docs/src/content/docs/reference/providers/awscc/

      - name: Create PR
        uses: peter-evans/create-pull-request@v7
        with:
          path: carina-main
          token: ${{ secrets.DOCS_PR_TOKEN }}
          branch: docs/update-awscc-reference
          title: "docs: update awscc provider reference"
          body: |
            Auto-generated from carina-provider-awscc.

            Triggered by ${{ github.event.head_commit.url }}
          commit-message: "docs: update awscc provider reference"
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "ci: add docs generation script and workflow"
```

### Task 5: Clean up main repo

Remove `carina-codegen-aws` and `carina-smithy` from the main repo. Keep `carina-aws-types`.

**Files (in carina repo):**
- Modify: `Cargo.toml` (remove workspace members)
- Delete: `carina-codegen-aws/` directory
- Delete: `carina-smithy/` directory

- [ ] **Step 1: Remove workspace members from Cargo.toml**

In `/Users/mizzy/src/github.com/carina-rs/carina/Cargo.toml`, remove `"carina-codegen-aws"` and `"carina-smithy"` from the `[workspace] members` list.

- [ ] **Step 2: Delete the crate directories**

```bash
cd /Users/mizzy/src/github.com/carina-rs/carina
rm -rf carina-codegen-aws/
rm -rf carina-smithy/
```

- [ ] **Step 3: Build and test**

```bash
cargo build
cargo test
```

Expected: All builds and tests pass without the removed crates.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: remove carina-codegen-aws and carina-smithy (moved to carina-provider-aws)"
```

### Task 6: Set up GitHub authentication

This is a manual step (not code). Configure the `DOCS_PR_TOKEN` secret.

- [ ] **Step 1: Create a fine-grained PAT**

Go to GitHub Settings → Developer settings → Personal access tokens → Fine-grained tokens:
- Token name: `carina-docs-pr`
- Resource owner: `carina-rs`
- Repository access: Only select `carina-rs/carina`
- Permissions:
  - Contents: Read and write
  - Pull requests: Read and write

- [ ] **Step 2: Add secret to provider repos**

In each provider repo (Settings → Secrets and variables → Actions):
- Name: `DOCS_PR_TOKEN`
- Value: the PAT from step 1

### Task 7: Verify end-to-end

- [ ] **Step 1: Trigger docs workflow in carina-provider-aws**

Push a change to main or use workflow_dispatch to trigger the docs workflow. Verify that a PR is created in `carina-rs/carina` with updated aws provider docs.

- [ ] **Step 2: Trigger docs workflow in carina-provider-awscc**

Same verification for awscc provider.

- [ ] **Step 3: Merge a docs PR and verify deployment**

Merge one of the auto-generated PRs. Verify that the docs.yml workflow in the main repo triggers and deploys the updated site to carina-rs.github.io.
