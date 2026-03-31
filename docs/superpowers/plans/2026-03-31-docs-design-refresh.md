# Documentation Site Design Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the default Starlight purple theme with an Ocean/Teal (Deep Navy) color scheme and improve code blocks, feature cards, and table styling.

**Architecture:** All changes go into `docs/src/styles/custom.css` by overriding Starlight's CSS custom properties. No HTML, config, or component changes needed. Starlight uses a `--sl-color-*` variable system where gray-1 is lightest and gray-6/black are darkest in dark mode (inverted in light mode).

**Tech Stack:** CSS custom properties, Starlight/Astro theming

---

### Task 1: Dark mode color scheme

**Files:**
- Modify: `docs/src/styles/custom.css`

- [ ] **Step 1: Add dark mode color overrides at the top of custom.css (before existing table rules)**

Add this CSS block at the very beginning of `docs/src/styles/custom.css`, before the existing `/* Provider doc table styling */` comment:

```css
/* ============================================
   Ocean/Teal Deep Navy Theme
   ============================================ */

/* Dark mode (default) */
:root {
  /* Gray scale: Slate palette (hue 215) */
  --sl-color-white: hsl(0, 0%, 100%);
  --sl-color-gray-1: hsl(214, 32%, 91%); /* #e2e8f0 Slate 200 */
  --sl-color-gray-2: hsl(215, 16%, 62%); /* #94a3b8 Slate 400 */
  --sl-color-gray-3: hsl(215, 14%, 46%); /* #64748b Slate 500 */
  --sl-color-gray-4: hsl(217, 19%, 35%); /* #475569 Slate 600 */
  --sl-color-gray-5: hsl(217, 33%, 17%); /* #1e293b Slate 800 */
  --sl-color-gray-6: hsl(222, 47%, 11%); /* #0f172a Slate 900 */
  --sl-color-black: hsl(229, 84%, 5%);   /* #020617 Slate 950 */

  /* Accent: Cyan */
  --sl-color-accent-low: hsl(192, 70%, 15%);  /* ~#164e63 Cyan 900 */
  --sl-color-accent: hsl(192, 91%, 36%);       /* ~#0891b2 Cyan 600 */
  --sl-color-accent-high: hsl(186, 78%, 58%);  /* ~#22d3ee Cyan 400 */
}
```

- [ ] **Step 2: Add light mode color overrides immediately after the dark mode block**

```css
/* Light mode */
:root[data-theme='light'] {
  /* Gray scale: Slate palette (light) */
  --sl-color-white: hsl(210, 40%, 8%);   /* #0f172a Slate 900 */
  --sl-color-gray-1: hsl(215, 25%, 17%); /* #1e293b Slate 800 */
  --sl-color-gray-2: hsl(215, 14%, 34%); /* #475569 Slate 600 */
  --sl-color-gray-3: hsl(215, 14%, 46%); /* #64748b Slate 500 */
  --sl-color-gray-4: hsl(215, 16%, 62%); /* #94a3b8 Slate 400 */
  --sl-color-gray-5: hsl(214, 32%, 91%); /* #e2e8f0 Slate 200 */
  --sl-color-gray-6: hsl(210, 40%, 96%); /* #f1f5f9 Slate 100 */
  --sl-color-gray-7: hsl(210, 40%, 98%); /* #f8fafc Slate 50 */
  --sl-color-black: hsl(0, 0%, 100%);    /* white */

  /* Accent: Cyan (darker for contrast on light BG) */
  --sl-color-accent-low: hsl(189, 94%, 93%);  /* ~#ecfeff Cyan 50 */
  --sl-color-accent: hsl(192, 82%, 31%);       /* ~#0e7490 Cyan 700 */
  --sl-color-accent-high: hsl(192, 91%, 36%);  /* ~#0891b2 Cyan 600 */
}
```

- [ ] **Step 3: Start dev server and verify colors**

Run: `cd docs && npm run dev`

Open http://localhost:4321 and check:
- Dark mode: deep navy background, cyan accent links, sidebar navigation
- Light mode (toggle in top right): light slate background, darker cyan links
- "Soon" badges should automatically be cyan (they use `--sl-color-accent-*`)

Expected: All accent-colored elements use cyan instead of purple. Gray tones are slate-tinted.

- [ ] **Step 4: Commit**

```bash
git add docs/src/styles/custom.css
git commit -m "style(docs): apply Ocean/Teal Deep Navy color scheme

Override Starlight CSS variables with Slate gray scale and
Cyan accent for both dark and light modes."
```

---

### Task 2: Code block theme

**Files:**
- Modify: `docs/src/styles/custom.css`

- [ ] **Step 1: Add code block overrides after the light mode block**

Add this CSS after the light mode `:root[data-theme='light']` block:

```css
/* Code block theme */
:root {
  --astro-code-color-background: hsl(217, 33%, 17%); /* Slate 800 - matches sidebar */
}
:root[data-theme='light'] {
  --astro-code-color-background: hsl(210, 40%, 96%); /* Slate 100 */
}
```

- [ ] **Step 2: Verify code blocks**

Open http://localhost:4321 and navigate to a provider reference page (e.g., AWSCC EC2 VPC).

Expected: Code blocks blend with the page — dark mode uses Slate 800 background (same as sidebar), light mode uses Slate 100.

- [ ] **Step 3: Commit**

```bash
git add docs/src/styles/custom.css
git commit -m "style(docs): match code block background to site palette"
```

---

### Task 3: Feature card styling

**Files:**
- Modify: `docs/src/styles/custom.css`

- [ ] **Step 1: Add card overrides after code block section**

Starlight's `Card` component uses `.card` class with `--sl-card-border` and `--sl-card-bg` CSS variables per nth-child. Override to use consistent cyan accent:

```css
/* Feature cards: consistent cyan accent */
.card {
  --sl-card-border: var(--sl-color-accent);
  --sl-card-bg: var(--sl-color-accent-low);
  border-color: var(--sl-color-gray-5);
  transition: border-color 0.2s ease;
}
.card:hover {
  border-color: var(--sl-color-accent);
}
/* Override per-card color rotation to use consistent cyan */
.card:nth-child(4n + 1) {
  --sl-card-border: var(--sl-color-accent);
  --sl-card-bg: var(--sl-color-accent-low);
}
.card:nth-child(4n + 3) {
  --sl-card-border: var(--sl-color-accent);
  --sl-card-bg: var(--sl-color-accent-low);
}
.card:nth-child(4n + 4) {
  --sl-card-border: var(--sl-color-accent);
  --sl-card-bg: var(--sl-color-accent-low);
}
.card:nth-child(4n + 5) {
  --sl-card-border: var(--sl-color-accent);
  --sl-card-bg: var(--sl-color-accent-low);
}
```

- [ ] **Step 2: Verify cards on the top page**

Open http://localhost:4321 (the splash/home page).

Expected: All 4 feature cards (Custom DSL, Effects as Values, Provider Architecture, Modules) have cyan-tinted icon borders/backgrounds. On hover, the card border brightens to cyan. Both dark and light modes.

- [ ] **Step 3: Commit**

```bash
git add docs/src/styles/custom.css
git commit -m "style(docs): unify feature card colors to cyan accent"
```

---

### Task 4: Table styling

**Files:**
- Modify: `docs/src/styles/custom.css`

- [ ] **Step 1: Add table styling improvements after the existing table rules**

Add after the existing `td, th { word-wrap: ... }` block (around line 19 of current custom.css):

```css
/* Table header and stripe rows */
table thead th {
  background-color: var(--sl-color-accent-low);
  color: var(--sl-color-white);
}
table tbody tr:nth-child(even) {
  background-color: var(--sl-color-gray-6);
}
table td, table th {
  border-color: var(--sl-color-hairline-light);
}
```

- [ ] **Step 2: Verify tables on a provider reference page**

Open a provider page with tables, e.g., http://localhost:4321/reference/providers/awscc/ec2/vpc/

Expected:
- Table header row has a subtle cyan-tinted background (accent-low)
- Even rows have a subtle alternating background
- Borders use the site's hairline color
- Works in both dark and light modes

- [ ] **Step 3: Commit**

```bash
git add docs/src/styles/custom.css
git commit -m "style(docs): add table header background and stripe rows"
```

---

### Task 5: Final visual check and squash

- [ ] **Step 1: Full visual review**

Open http://localhost:4321 and check all pages in both dark and light modes:

| Page | Check |
|------|-------|
| Home (splash) | Hero, feature cards, provider link cards |
| Provider index | Sidebar items, "Soon" badges |
| Provider resource (e.g., VPC) | Tables, code blocks, inline code |
| Search | Search bar colors |
| Theme toggle | Smooth transition between modes |

- [ ] **Step 2: Build to verify no errors**

Run: `cd docs && npm run build`

Expected: Build completes with no errors.

- [ ] **Step 3: If any issues found, fix and commit**

Fix CSS issues discovered in visual review and commit with an appropriate message.
