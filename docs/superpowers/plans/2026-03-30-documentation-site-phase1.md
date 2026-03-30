# Documentation Site Phase 1: Starlight Setup + Provider Docs Migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace mdBook with Starlight, migrate existing provider docs, and deploy to carina-rs.github.io.

**Architecture:** Initialize a Starlight (Astro) project in `docs/`, create a custom `.crn` TextMate grammar for syntax highlighting, migrate 49 provider Markdown files with frontmatter, update generation scripts to target the new directory, and update CI to build with Node.js.

**Tech Stack:** Astro + Starlight, Node.js 22, Shiki (syntax highlighting), GitHub Actions, GitHub Pages

---

## File Map

### New files

- `docs/package.json` — Node.js project config
- `docs/astro.config.mjs` — Starlight configuration (sidebar, theme, custom language)
- `docs/tsconfig.json` — TypeScript config (required by Astro)
- `docs/src/content.config.ts` — Astro content collection config
- `docs/src/content/docs/index.mdx` — Landing page
- `docs/src/content/docs/reference/providers/awscc/index.md` — AWSCC provider overview (migrated)
- `docs/src/content/docs/reference/providers/awscc/{service}/{resource}.md` — AWSCC resource docs (migrated)
- `docs/src/content/docs/reference/providers/aws/{service}/{resource}.md` — AWS resource docs (migrated)
- `docs/custom-grammars/crn.tmLanguage.json` — `.crn` TextMate grammar for Shiki
- `docs/src/styles/custom.css` — Custom styling (Catppuccin colors, table layout)

### Modified files

- `carina-provider-awscc/scripts/generate-docs.sh` — Change output dir, add frontmatter, remove summary generation call
- `carina-provider-aws/scripts/generate-docs.sh` — Change output dir, add frontmatter, remove summary generation call
- `.github/workflows/docs.yml` — Replace mdBook with Node.js + Astro build
- `.gitignore` — Replace `docs/book/` with `docs/dist/` and add `docs/node_modules/`

### Deleted files

- `docs/book.toml`
- `docs/src/SUMMARY.md`
- `docs/src/introduction.md`
- `docs/src/providers/` (entire directory — replaced by new path)
- `docs/theme/` (entire directory)
- `docs/scripts/generate-summary.sh`
- `docs/book/` (build output, already in .gitignore)

---

### Task 1: Initialize Starlight project

**Files:**
- Create: `docs/package.json`
- Create: `docs/astro.config.mjs`
- Create: `docs/tsconfig.json`
- Create: `docs/src/content.config.ts`

- [ ] **Step 1: Create `docs/package.json`**

```json
{
  "name": "carina-docs",
  "type": "module",
  "scripts": {
    "dev": "astro dev",
    "build": "astro build",
    "preview": "astro preview"
  },
  "dependencies": {
    "@astrojs/starlight": "^0.34",
    "astro": "^5.7",
    "sharp": "^0.33"
  }
}
```

- [ ] **Step 2: Create `docs/astro.config.mjs`**

This is a minimal config — custom grammar and full sidebar will be added in later tasks.

```js
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  integrations: [
    starlight({
      title: 'Carina',
      social: {
        github: 'https://github.com/carina-rs/carina',
      },
      sidebar: [
        { label: 'Home', link: '/' },
        {
          label: 'Reference',
          items: [
            {
              label: 'Providers',
              autogenerate: { directory: 'reference/providers' },
            },
          ],
        },
      ],
    }),
  ],
});
```

- [ ] **Step 3: Create `docs/tsconfig.json`**

```json
{
  "extends": "astro/tsconfigs/strict"
}
```

- [ ] **Step 4: Create `docs/src/content.config.ts`**

```ts
import { defineCollection } from 'astro:content';
import { docsSchema } from '@astrojs/starlight/schema';

export const collections = {
  docs: defineCollection({ schema: docsSchema() }),
};
```

- [ ] **Step 5: Install dependencies**

Run: `cd docs && npm install`
Expected: `node_modules/` created, `package-lock.json` generated.

- [ ] **Step 6: Verify Starlight builds with empty content**

Create a minimal landing page first:

Create `docs/src/content/docs/index.mdx`:

```mdx
---
title: Carina
description: A strongly typed infrastructure management tool written in Rust.
template: splash
hero:
  title: Carina
  tagline: A strongly typed infrastructure management tool written in Rust.
  actions:
    - text: GitHub
      link: https://github.com/carina-rs/carina
      icon: github
---
```

Run: `cd docs && npm run build`
Expected: Build succeeds, output in `docs/dist/`.

- [ ] **Step 7: Commit**

```bash
git add docs/package.json docs/package-lock.json docs/astro.config.mjs docs/tsconfig.json docs/src/content.config.ts docs/src/content/docs/index.mdx
git commit -m "feat: initialize Starlight documentation site"
```

---

### Task 2: Create `.crn` TextMate grammar

**Files:**
- Create: `docs/custom-grammars/crn.tmLanguage.json`
- Modify: `docs/astro.config.mjs`

- [ ] **Step 1: Create `docs/custom-grammars/crn.tmLanguage.json`**

```json
{
  "$schema": "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
  "name": "Carina",
  "scopeName": "source.crn",
  "fileTypes": ["crn"],
  "patterns": [
    { "include": "#comments" },
    { "include": "#strings" },
    { "include": "#numbers" },
    { "include": "#keywords" },
    { "include": "#constants" },
    { "include": "#operators" },
    { "include": "#identifiers" }
  ],
  "repository": {
    "comments": {
      "patterns": [
        {
          "name": "comment.line.double-slash.crn",
          "match": "//.*$"
        },
        {
          "name": "comment.line.hash.crn",
          "match": "#.*$"
        }
      ]
    },
    "strings": {
      "patterns": [
        {
          "name": "string.quoted.double.crn",
          "begin": "\"",
          "end": "\"",
          "patterns": [
            {
              "name": "constant.character.escape.crn",
              "match": "\\\\."
            },
            {
              "name": "meta.embedded.expression.crn",
              "begin": "\\$\\{",
              "end": "\\}",
              "patterns": [
                { "include": "#identifiers" },
                { "include": "#numbers" }
              ]
            }
          ]
        }
      ]
    },
    "numbers": {
      "patterns": [
        {
          "name": "constant.numeric.crn",
          "match": "\\b\\d+(\\.\\d+)?\\b"
        }
      ]
    },
    "keywords": {
      "patterns": [
        {
          "name": "keyword.control.crn",
          "match": "\\b(if|else|for|in)\\b"
        },
        {
          "name": "keyword.declaration.crn",
          "match": "\\b(let|fn|import|module|provider|backend|arguments|attributes|removed|moved|lifecycle)\\b"
        },
        {
          "name": "storage.type.crn",
          "match": "\\b(string|int|float|bool|list|map)\\b"
        }
      ]
    },
    "constants": {
      "patterns": [
        {
          "name": "constant.language.crn",
          "match": "\\b(true|false)\\b"
        }
      ]
    },
    "operators": {
      "patterns": [
        {
          "name": "keyword.operator.pipe.crn",
          "match": "\\|>"
        },
        {
          "name": "keyword.operator.assignment.crn",
          "match": "="
        }
      ]
    },
    "identifiers": {
      "patterns": [
        {
          "name": "entity.name.type.resource.crn",
          "match": "\\b(awscc|aws)\\.[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)*\\b"
        },
        {
          "name": "variable.other.crn",
          "match": "\\b[a-z_][a-z0-9_]*\\b"
        }
      ]
    }
  }
}
```

- [ ] **Step 2: Register custom grammar in `docs/astro.config.mjs`**

Replace the entire file with:

```js
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { readFileSync } from 'node:fs';

const crnGrammar = JSON.parse(
  readFileSync(new URL('./custom-grammars/crn.tmLanguage.json', import.meta.url), 'utf-8')
);

export default defineConfig({
  integrations: [
    starlight({
      title: 'Carina',
      social: {
        github: 'https://github.com/carina-rs/carina',
      },
      expressiveCode: {
        shiki: {
          langs: [crnGrammar],
        },
      },
      sidebar: [
        { label: 'Home', link: '/' },
        {
          label: 'Reference',
          items: [
            {
              label: 'Providers',
              autogenerate: { directory: 'reference/providers' },
            },
          ],
        },
      ],
    }),
  ],
});
```

- [ ] **Step 3: Verify syntax highlighting works**

Create a temporary test file `docs/src/content/docs/reference/test-highlighting.md`:

```markdown
---
title: Syntax Highlighting Test
---

## CRN Code

\`\`\`crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
  tags = {
    Name = "test-${env}"
  }
}

fn tag_name(env: string, service: string): string {
  join("-", [env, service])
}

for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
  awscc.ec2.subnet {
    vpc_id     = vpc.vpc_id
    cidr_block = cidr_subnet(vpc.cidr_block, 8, i)
  }
}
\`\`\`
```

Run: `cd docs && npm run build`
Expected: Build succeeds. Check `docs/dist/reference/test-highlighting/` for colored syntax output.

- [ ] **Step 4: Remove test file and commit**

```bash
rm docs/src/content/docs/reference/test-highlighting.md
git add docs/custom-grammars/crn.tmLanguage.json docs/astro.config.mjs
git commit -m "feat: add .crn TextMate grammar for syntax highlighting"
```

---

### Task 3: Migrate provider docs

**Files:**
- Create: `docs/src/content/docs/reference/providers/awscc/` (migrated files with frontmatter)
- Create: `docs/src/content/docs/reference/providers/aws/` (migrated files with frontmatter)
- Create: `docs/scripts/migrate-provider-docs.sh` (one-time migration script)

- [ ] **Step 1: Create migration script `docs/scripts/migrate-provider-docs.sh`**

This script copies existing provider docs to the new location and prepends Starlight frontmatter.

```bash
#!/bin/bash
# One-time migration: copy provider docs from mdBook to Starlight layout
# and prepend frontmatter to each file.
set -e

SRC_BASE="docs/src/providers"
DST_BASE="docs/src/content/docs/reference/providers"

migrate_provider() {
    local PROVIDER="$1"
    local SRC_DIR="$SRC_BASE/$PROVIDER"
    local DST_DIR="$DST_BASE/$PROVIDER"

    if [ ! -d "$SRC_DIR" ]; then
        echo "Skipping $PROVIDER (no source directory)"
        return
    fi

    # Copy index.md with frontmatter
    if [ -f "$SRC_DIR/index.md" ]; then
        mkdir -p "$DST_DIR"
        local PROVIDER_UPPER
        PROVIDER_UPPER=$(echo "$PROVIDER" | tr '[:lower:]' '[:upper:]')
        {
            echo "---"
            echo "title: \"${PROVIDER_UPPER} Provider\""
            echo "---"
            echo ""
            cat "$SRC_DIR/index.md"
        } > "$DST_DIR/index.md"
        echo "Migrated $DST_DIR/index.md"
    fi

    # Copy resource docs with frontmatter
    for DOC_FILE in "$SRC_DIR"/*/*.md; do
        [ -f "$DOC_FILE" ] || continue

        local SERVICE_DIR
        SERVICE_DIR=$(basename "$(dirname "$DOC_FILE")")
        local RESOURCE_NAME
        RESOURCE_NAME=$(basename "$DOC_FILE" .md)

        # Extract DSL name from first heading (e.g., "# awscc.ec2.vpc" -> "awscc.ec2.vpc")
        local DSL_NAME
        DSL_NAME=$(head -1 "$DOC_FILE" | sed 's/^# *//')

        # Extract service display name from CloudFormation Type line
        local SERVICE_DISPLAY=""
        local CFN_LINE
        CFN_LINE=$(grep "^CloudFormation Type:" "$DOC_FILE" 2>/dev/null | head -1)
        if [ -n "$CFN_LINE" ]; then
            SERVICE_DISPLAY=$(echo "$CFN_LINE" | sed 's/.*`AWS::\([^:]*\)::.*/\1/')
        fi
        if [ -z "$SERVICE_DISPLAY" ]; then
            SERVICE_DISPLAY=$(echo "$SERVICE_DIR" | tr '[:lower:]' '[:upper:]')
        fi

        mkdir -p "$DST_DIR/$SERVICE_DIR"
        local DST_FILE="$DST_DIR/$SERVICE_DIR/$RESOURCE_NAME.md"

        {
            echo "---"
            echo "title: \"$DSL_NAME\""
            echo "description: \"${PROVIDER_UPPER:-${PROVIDER^^}} $SERVICE_DISPLAY ${RESOURCE_NAME} resource reference\""
            echo "---"
            echo ""
            cat "$DOC_FILE"
        } > "$DST_FILE"
        echo "Migrated $DST_FILE"
    done
}

migrate_provider "awscc"
migrate_provider "aws"

echo ""
echo "Migration complete. Verify with: cd docs && npm run build"
```

- [ ] **Step 2: Run the migration script**

```bash
chmod +x docs/scripts/migrate-provider-docs.sh
./docs/scripts/migrate-provider-docs.sh
```

Expected: Files created under `docs/src/content/docs/reference/providers/{awscc,aws}/` with frontmatter.

- [ ] **Step 3: Verify the build succeeds**

Run: `cd docs && npm run build`
Expected: Build succeeds. Provider docs appear in `docs/dist/reference/providers/`.

- [ ] **Step 4: Preview and check a provider page**

Run: `cd docs && npm run dev`
Open: `http://localhost:4321/reference/providers/awscc/ec2/vpc/`
Expected: VPC resource doc renders with syntax-highlighted `.crn` example, argument tables, and sidebar navigation.

Stop the dev server after verification.

- [ ] **Step 5: Commit**

```bash
git add docs/src/content/docs/reference/providers/ docs/scripts/migrate-provider-docs.sh
git commit -m "feat: migrate provider docs to Starlight with frontmatter"
```

---

### Task 4: Update provider doc generation scripts

**Files:**
- Modify: `carina-provider-awscc/scripts/generate-docs.sh`
- Modify: `carina-provider-aws/scripts/generate-docs.sh`

- [ ] **Step 1: Update AWSCC generation script**

In `carina-provider-awscc/scripts/generate-docs.sh`, make these changes:

**Change 1:** Update `DOCS_DIR` (line 38):

```bash
# Old:
DOCS_DIR="docs/src/providers/awscc"
# New:
DOCS_DIR="docs/src/content/docs/reference/providers/awscc"
```

**Change 2:** After generating each markdown file (after the `$CODEGEN_BIN ... > "$OUTPUT_FILE"` line around line 113), add frontmatter prepending:

```bash
    # Generate schema documentation
    "$CODEGEN_BIN" --type-name "$TYPE_NAME" --format markdown < "$CACHE_FILE" > "$OUTPUT_FILE"

    if [ $? -ne 0 ]; then
        echo "  ERROR: Failed to generate $TYPE_NAME"
        rm -f "$OUTPUT_FILE"
        continue
    fi

    # Prepend Starlight frontmatter
    DSL_NAME=$(head -1 "$OUTPUT_FILE" | sed 's/^# *//')
    FRONTMATTER_TMPFILE=$(mktemp)
    {
        echo "---"
        echo "title: \"$DSL_NAME\""
        echo "description: \"AWSCC $SERVICE $RESOURCE resource reference\""
        echo "---"
        echo ""
        cat "$OUTPUT_FILE"
    } > "$FRONTMATTER_TMPFILE"
    mv "$FRONTMATTER_TMPFILE" "$OUTPUT_FILE"
```

**Change 3:** Remove the summary generation call at the end (around line 150-152). Delete these lines:

```bash
# Generate SUMMARY.md (shared across all providers)
echo ""
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$(cd "$SCRIPT_DIR/../.." && pwd)/docs/scripts/generate-summary.sh"
```

**Change 4:** Update the index.md generation to use the new path and add frontmatter (around line 155-210). Replace `cat > "$DOCS_DIR/index.md" << 'EOF'` block with:

```bash
cat > "$DOCS_DIR/index.md" << 'EOF'
---
title: "AWSCC Provider"
description: "AWSCC provider resource reference"
---

# AWSCC Provider
EOF
```

Then append the rest of the content (configuration, usage, enum values sections) after the heredoc, unchanged.

**Change 5:** Update the final echo messages to remove mdbook references:

```bash
echo ""
echo "Done! Generated documentation in $DOCS_DIR"
```

- [ ] **Step 2: Update AWS generation script**

In `carina-provider-aws/scripts/generate-docs.sh`, make these changes:

**Change 1:** Update `DOCS_DIR` (line 14):

```bash
# Old:
DOCS_DIR="docs/src/providers/aws"
# New:
DOCS_DIR="docs/src/content/docs/reference/providers/aws"
```

**Change 2:** After the `smithy-codegen` run (after line 29), add frontmatter to all generated files:

```bash
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
        cat "$DOC_FILE"
    } > "$FRONTMATTER_TMPFILE"
    mv "$FRONTMATTER_TMPFILE" "$DOC_FILE"
done
```

Insert this block **before** the example insertion loop (before the `# Insert examples into generated docs` comment).

**Change 3:** Remove the summary generation call at the end (around line 62-64). Delete:

```bash
# Generate SUMMARY.md (shared across all providers)
echo ""
"$PROJECT_ROOT/docs/scripts/generate-summary.sh"
```

- [ ] **Step 3: Commit**

```bash
git add carina-provider-awscc/scripts/generate-docs.sh carina-provider-aws/scripts/generate-docs.sh
git commit -m "feat: update provider doc generation scripts for Starlight"
```

---

### Task 5: Update CI/CD and .gitignore

**Files:**
- Modify: `.github/workflows/docs.yml`
- Modify: `.gitignore`

- [ ] **Step 1: Update `.github/workflows/docs.yml`**

Replace the entire file with:

```yaml
name: Deploy Docs

on:
  push:
    branches: [main]
    paths:
      - "docs/**"
  workflow_dispatch:

concurrency:
  group: pages
  cancel-in-progress: false

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: 22

      - name: Install dependencies
        run: npm ci
        working-directory: docs

      - name: Build docs
        run: npm run build
        working-directory: docs

      - name: Deploy to carina-rs.github.io
        uses: peaceiris/actions-gh-pages@v4
        with:
          external_repository: carina-rs/carina-rs.github.io
          publish_branch: main
          publish_dir: ./docs/dist
          personal_token: ${{ secrets.DOCS_DEPLOY_TOKEN }}
```

- [ ] **Step 2: Update `.gitignore`**

Replace the mdBook entries with Starlight entries:

```
# Old (remove):
docs/book/

# New (add):
docs/dist/
docs/node_modules/
docs/.astro/
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/docs.yml .gitignore
git commit -m "feat: update CI and gitignore for Starlight docs"
```

---

### Task 6: Remove mdBook artifacts

**Files:**
- Delete: `docs/book.toml`
- Delete: `docs/src/SUMMARY.md`
- Delete: `docs/src/introduction.md`
- Delete: `docs/src/providers/` (entire old directory)
- Delete: `docs/theme/` (entire directory)
- Delete: `docs/scripts/generate-summary.sh`

- [ ] **Step 1: Remove old mdBook files**

```bash
rm docs/book.toml
rm docs/src/SUMMARY.md
rm docs/src/introduction.md
rm -rf docs/src/providers/
rm -rf docs/theme/
rm docs/scripts/generate-summary.sh
```

- [ ] **Step 2: Verify build still works**

Run: `cd docs && npm run build`
Expected: Build succeeds with only Starlight content.

- [ ] **Step 3: Commit**

```bash
git add -A docs/book.toml docs/src/SUMMARY.md docs/src/introduction.md docs/src/providers/ docs/theme/ docs/scripts/generate-summary.sh
git commit -m "chore: remove mdBook artifacts"
```

---

### Task 7: Update landing page and sidebar

**Files:**
- Modify: `docs/src/content/docs/index.mdx`
- Modify: `docs/astro.config.mjs`

- [ ] **Step 1: Update landing page `docs/src/content/docs/index.mdx`**

Replace with:

```mdx
---
title: Carina
description: A strongly typed infrastructure management tool written in Rust.
template: splash
hero:
  title: Carina
  tagline: A strongly typed infrastructure management tool written in Rust.
  actions:
    - text: Getting Started
      link: /getting-started/installation/
      icon: rocket
    - text: GitHub
      link: https://github.com/carina-rs/carina
      icon: github
      variant: minimal
---

import { Card, CardGrid } from '@astrojs/starlight/components';

<CardGrid>
  <Card title="Custom DSL" icon="pencil">
    Infrastructure definition with `.crn` files — strongly typed, validated at parse time.
  </Card>
  <Card title="Effects as Values" icon="list-format">
    Side effects are represented as data, inspectable before execution.
  </Card>
  <Card title="Provider Architecture" icon="setting">
    Pluggable providers — AWSCC (Cloud Control API) and AWS.
  </Card>
  <Card title="Modules" icon="puzzle">
    Reusable infrastructure components with typed arguments and attributes.
  </Card>
</CardGrid>
```

- [ ] **Step 2: Update sidebar in `docs/astro.config.mjs`**

Replace the sidebar section with a structure that includes placeholder sections for future content:

```js
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { readFileSync } from 'node:fs';

const crnGrammar = JSON.parse(
  readFileSync(new URL('./custom-grammars/crn.tmLanguage.json', import.meta.url), 'utf-8')
);

export default defineConfig({
  integrations: [
    starlight({
      title: 'Carina',
      social: {
        github: 'https://github.com/carina-rs/carina',
      },
      expressiveCode: {
        shiki: {
          langs: [crnGrammar],
        },
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Installation', link: '/getting-started/installation/', badge: 'Soon' },
            { label: 'Quick Start', link: '/getting-started/quick-start/', badge: 'Soon' },
            { label: 'Core Concepts', link: '/getting-started/core-concepts/', badge: 'Soon' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Writing Resources', link: '/guides/writing-resources/', badge: 'Soon' },
            { label: 'Using Modules', link: '/guides/using-modules/', badge: 'Soon' },
            { label: 'State Management', link: '/guides/state-management/', badge: 'Soon' },
            { label: 'For / If Expressions', link: '/guides/for-if-expressions/', badge: 'Soon' },
            { label: 'Functions', link: '/guides/functions/', badge: 'Soon' },
            { label: 'LSP Setup', link: '/guides/lsp-setup/', badge: 'Soon' },
          ],
        },
        {
          label: 'Reference',
          items: [
            {
              label: 'DSL Language',
              items: [
                { label: 'Syntax', link: '/reference/dsl/syntax/', badge: 'Soon' },
                { label: 'Types & Values', link: '/reference/dsl/types-and-values/', badge: 'Soon' },
                { label: 'Expressions', link: '/reference/dsl/expressions/', badge: 'Soon' },
                { label: 'Built-in Functions', link: '/reference/dsl/built-in-functions/', badge: 'Soon' },
                { label: 'Modules', link: '/reference/dsl/modules/', badge: 'Soon' },
              ],
            },
            {
              label: 'CLI Commands',
              items: [
                { label: 'plan', link: '/reference/cli/plan/', badge: 'Soon' },
                { label: 'apply', link: '/reference/cli/apply/', badge: 'Soon' },
                { label: 'validate', link: '/reference/cli/validate/', badge: 'Soon' },
                { label: 'state', link: '/reference/cli/state/', badge: 'Soon' },
                { label: 'module info', link: '/reference/cli/module-info/', badge: 'Soon' },
              ],
            },
            {
              label: 'Providers',
              autogenerate: { directory: 'reference/providers' },
            },
          ],
        },
      ],
    }),
  ],
});
```

- [ ] **Step 3: Build and preview**

Run: `cd docs && npm run build`
Expected: Build succeeds. Landing page has cards, sidebar shows all sections with "Soon" badges on placeholder items, providers are autogenerated.

- [ ] **Step 4: Commit**

```bash
git add docs/src/content/docs/index.mdx docs/astro.config.mjs
git commit -m "feat: update landing page and sidebar structure"
```

---

### Task 8: Add custom styles

**Files:**
- Create: `docs/src/styles/custom.css`
- Modify: `docs/astro.config.mjs`

- [ ] **Step 1: Create `docs/src/styles/custom.css`**

Starlight uses its own theming system, so we only need minimal custom CSS for provider doc tables:

```css
/* Provider doc table styling */
table {
  width: 100%;
  font-size: 0.9rem;
}

/* Enum value tables: Value and DSL Identifier columns */
table th:first-child,
table td:first-child {
  min-width: 120px;
}

/* Struct field tables: narrower Required column */
table th:nth-child(3),
table td:nth-child(3) {
  min-width: 80px;
  white-space: nowrap;
}
```

- [ ] **Step 2: Register custom CSS in `docs/astro.config.mjs`**

Add `customCss` to the Starlight config:

```js
      customCss: ['./src/styles/custom.css'],
```

Add this line after the `social` property in the starlight config object (inside the `starlight({...})` call in `astro.config.mjs`).

- [ ] **Step 3: Build and verify**

Run: `cd docs && npm run build`
Expected: Build succeeds with custom styles applied.

- [ ] **Step 4: Commit**

```bash
git add docs/src/styles/custom.css docs/astro.config.mjs
git commit -m "feat: add custom CSS for provider doc tables"
```

---

### Task 9: Final verification and cleanup

**Files:**
- Modify: `.gitignore` (if `.superpowers/` not already ignored)

- [ ] **Step 1: Full build from clean state**

```bash
cd docs && rm -rf node_modules dist .astro && npm install && npm run build
```

Expected: Clean build succeeds.

- [ ] **Step 2: Verify key pages**

```bash
cd docs && npm run dev
```

Check these pages in the browser:

1. `http://localhost:4321/` — Landing page with cards
2. `http://localhost:4321/reference/providers/awscc/ec2/vpc/` — AWSCC VPC page with syntax-highlighted example
3. `http://localhost:4321/reference/providers/aws/s3/bucket/` — AWS S3 bucket page
4. Sidebar navigation — all provider resources listed, "Soon" badges on placeholder sections
5. Search — type "vpc" and verify Pagefind finds provider docs

Stop the dev server after verification.

- [ ] **Step 3: Add `.superpowers/` to `.gitignore` if needed**

Check if `.superpowers/` is in `.gitignore`. If not, add it:

```
# Superpowers brainstorm sessions
.superpowers/
```

- [ ] **Step 4: Commit any remaining changes**

```bash
git add -A
git commit -m "chore: final cleanup for Starlight migration"
```
