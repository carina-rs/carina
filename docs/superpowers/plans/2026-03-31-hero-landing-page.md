# Hero & Landing Page Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the default Starlight Hero with a custom design featuring side-by-side code/plan panels with Canopus gold accents, and add a "Why Carina?" section.

**Architecture:** Create a custom `Hero.astro` component that overrides Starlight's default Hero via the `components` config. Hero-specific CSS goes in `custom.css`. The "Why Carina?" section is added directly in `index.mdx` using HTML. No new dependencies.

**Tech Stack:** Astro components, CSS, Starlight component overrides

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `docs/src/components/Hero.astro` | Create | Custom Hero with code/plan panels and Canopus colors |
| `docs/astro.config.mjs` | Modify | Register Hero component override |
| `docs/src/content/docs/index.mdx` | Modify | Add "Why Carina?" section, keep existing cards |
| `docs/src/styles/custom.css` | Modify | "Why Carina?" section styles |

---

### Task 1: Custom Hero component

**Files:**
- Create: `docs/src/components/Hero.astro`
- Modify: `docs/astro.config.mjs`

- [ ] **Step 1: Create the custom Hero component**

Create `docs/src/components/Hero.astro`:

```astro
---
/**
 * Custom Hero component for Carina docs landing page.
 * Replaces Starlight's default Hero with code/plan panels and Canopus gold accent.
 */
import { PAGE_TITLE_ID } from '@astrojs/starlight/constants';
---

<div class="hero-custom">
  <div class="hero-header">
    <div class="hero-star"></div>
    <h1 id={PAGE_TITLE_ID} data-page-title>Carina</h1>
    <p class="hero-tagline">A strongly typed infrastructure management tool written in Rust.</p>
  </div>

  <div class="hero-panels">
    {/* Left: DSL code */}
    <div class="hero-panel">
      <div class="panel-header">
        <span class="panel-tab">main.crn</span>
      </div>
      <div class="panel-body">
        <pre><code><span class="kw">provider</span> <span class="id">aws</span> {'{'}{'\n'}  <span class="attr">region:</span> <span class="enum">aws.Region.ap_northeast_1</span>{'\n'}{'}'}{'\n'}{'\n'}<span class="kw">let</span> <span class="id">vpc</span> = <span class="type">awscc.ec2.vpc</span> {'{'}{'\n'}  <span class="attr">cidr_block:</span> <span class="str">"10.0.0.0/16"</span>{'\n'}  <span class="attr">enable_dns_support:</span> <span class="bool">true</span>{'\n'}{'}'}{'\n'}{'\n'}<span class="type">awscc.ec2.subnet</span> {'{'}{'\n'}  <span class="attr">vpc_id:</span> <span class="id">vpc</span>.vpc_id{'\n'}  <span class="attr">cidr_block:</span> <span class="str">"10.0.1.0/24"</span>{'\n'}{'}'}</code></pre>
      </div>
    </div>

    {/* Right: Plan output */}
    <div class="hero-panel">
      <div class="panel-header terminal-header">
        <span class="terminal-dot red"></span>
        <span class="terminal-dot yellow"></span>
        <span class="terminal-dot green"></span>
        <span class="panel-tab-terminal">terminal</span>
      </div>
      <div class="panel-body">
        <pre><code><span class="prompt">$</span> carina plan main.crn{'\n'}<span class="muted">Planning...</span>{'\n'}{'\n'}<span class="add">+</span> <span class="id">awscc.ec2.vpc</span>{'\n'}  <span class="attr">cidr_block:</span> <span class="str">"10.0.0.0/16"</span>{'\n'}  <span class="attr">enable_dns_support:</span> <span class="bool">true</span>{'\n'}{'\n'}<span class="add">+</span> <span class="id">awscc.ec2.subnet</span>{'\n'}  <span class="attr">vpc_id:</span> <span class="computed">(computed)</span>{'\n'}  <span class="attr">cidr_block:</span> <span class="str">"10.0.1.0/24"</span>{'\n'}{'\n'}<span class="plan-summary">Plan: 2 to create, 0 to update, 0 to delete.</span></code></pre>
      </div>
    </div>
  </div>

  <div class="hero-actions">
    <a href="/getting-started/installation/" class="hero-btn primary">Get Started</a>
    <a href="https://github.com/carina-rs/carina" class="hero-btn secondary">GitHub</a>
  </div>
</div>

<style>
  .hero-custom {
    padding: clamp(2rem, 6vw, 4rem) 0 2rem;
    text-align: center;
  }

  /* Canopus star */
  .hero-star {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: #fef3c7;
    box-shadow: 0 0 16px #fde68a, 0 0 40px rgba(253, 230, 138, 0.3);
    margin: 0 auto 1rem;
  }

  h1 {
    font-size: clamp(var(--sl-text-4xl), calc(0.25rem + 5vw), var(--sl-text-6xl));
    font-weight: 700;
    line-height: var(--sl-line-height-headings);
    color: #fbbf24;
    margin: 0;
  }

  .hero-tagline {
    font-size: clamp(var(--sl-text-base), calc(0.0625rem + 2vw), var(--sl-text-xl));
    color: var(--sl-color-gray-2);
    margin: 0.5rem 0 0;
  }

  /* Panels */
  .hero-panels {
    display: flex;
    gap: 0.75rem;
    margin: 2rem auto 0;
    max-width: 52rem;
    padding: 0 1rem;
  }

  .hero-panel {
    flex: 1;
    background: #1e293b;
    border: 1px solid #334155;
    border-radius: 0.5rem;
    overflow: hidden;
    text-align: left;
  }

  .panel-header {
    background: #162032;
    padding: 0.4rem 0.75rem;
    border-bottom: 1px solid #334155;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .panel-tab {
    color: #22d3ee;
    font-size: var(--sl-text-xs);
    font-weight: 600;
  }

  .terminal-header {
    gap: 0.25rem;
  }

  .terminal-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
  }
  .terminal-dot.red { background: #f43f5e; }
  .terminal-dot.yellow { background: #f59e0b; }
  .terminal-dot.green { background: #10b981; }

  .panel-tab-terminal {
    color: var(--sl-color-gray-3);
    font-size: var(--sl-text-xs);
    margin-left: 0.25rem;
  }

  .panel-body {
    padding: 0.75rem;
  }

  .panel-body pre {
    margin: 0;
    background: transparent !important;
    border: none !important;
    padding: 0 !important;
  }

  .panel-body code {
    font-family: var(--__sl-font-mono);
    font-size: var(--sl-text-xs);
    line-height: 1.7;
    color: #e2e8f0;
  }

  /* Syntax colors */
  .kw { color: #22d3ee; }
  .id { color: #e2e8f0; }
  .attr { color: #94a3b8; }
  .str { color: #86efac; }
  .bool { color: #fbbf24; }
  .enum { color: #a5f3fc; }
  .type { color: #94a3b8; }
  .prompt { color: #22d3ee; }
  .muted { color: #64748b; }
  .add { color: #10b981; font-weight: 700; }
  .computed { color: #67e8f9; }
  .plan-summary { color: #22d3ee; font-weight: 600; }

  /* CTA buttons */
  .hero-actions {
    display: flex;
    gap: 0.75rem;
    justify-content: center;
    margin-top: 1.5rem;
    flex-wrap: wrap;
  }

  .hero-btn {
    padding: 0.5rem 1.25rem;
    border-radius: 0.375rem;
    font-size: var(--sl-text-sm);
    font-weight: 600;
    text-decoration: none;
    transition: background-color 0.2s ease;
  }

  .hero-btn.primary {
    background: #d97706;
    color: #fff;
  }
  .hero-btn.primary:hover {
    background: #f59e0b;
  }

  .hero-btn.secondary {
    border: 1px solid #475569;
    color: #94a3b8;
  }
  .hero-btn.secondary:hover {
    border-color: #64748b;
    color: #e2e8f0;
  }

  /* Mobile: stack panels */
  @media (max-width: 50rem) {
    .hero-panels {
      flex-direction: column;
    }
  }

  /* Light mode overrides */
  :global([data-theme='light']) .hero-panel {
    background: #f1f5f9;
    border-color: #e2e8f0;
  }
  :global([data-theme='light']) .panel-header {
    background: #e2e8f0;
    border-color: #cbd5e1;
  }
  :global([data-theme='light']) .panel-body code {
    color: #0f172a;
  }
  :global([data-theme='light']) .kw { color: #0891b2; }
  :global([data-theme='light']) .id { color: #0f172a; }
  :global([data-theme='light']) .attr { color: #475569; }
  :global([data-theme='light']) .str { color: #16a34a; }
  :global([data-theme='light']) .bool { color: #d97706; }
  :global([data-theme='light']) .enum { color: #0e7490; }
  :global([data-theme='light']) .type { color: #475569; }
  :global([data-theme='light']) .prompt { color: #0891b2; }
  :global([data-theme='light']) .muted { color: #94a3b8; }
  :global([data-theme='light']) .add { color: #16a34a; }
  :global([data-theme='light']) .computed { color: #0891b2; }
  :global([data-theme='light']) .plan-summary { color: #0891b2; }
  :global([data-theme='light']) h1 { color: #d97706; }
  :global([data-theme='light']) .hero-star {
    box-shadow: 0 0 16px #fbbf24, 0 0 40px rgba(251, 191, 36, 0.3);
  }
  :global([data-theme='light']) .hero-btn.secondary {
    border-color: #cbd5e1;
    color: #475569;
  }
  :global([data-theme='light']) .hero-btn.secondary:hover {
    border-color: #94a3b8;
    color: #0f172a;
  }
</style>
```

- [ ] **Step 2: Register the Hero override in astro.config.mjs**

In `docs/astro.config.mjs`, add `components` to the starlight config. Change the starlight() call from:

```js
    starlight({
      title: 'Carina',
```

to:

```js
    starlight({
      title: 'Carina',
      components: {
        Hero: './src/components/Hero.astro',
      },
```

- [ ] **Step 3: Remove the hero frontmatter from index.mdx**

The custom Hero component reads from `Astro.locals.starlightRoute.entry.data.hero`. The existing `index.mdx` frontmatter defines `hero` with title, tagline, and actions. Since our custom component hard-codes these, remove the `hero` key from frontmatter but keep the `template: splash`.

Replace the full frontmatter in `docs/src/content/docs/index.mdx` from:

```yaml
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
```

to:

```yaml
---
title: Carina
description: A strongly typed infrastructure management tool written in Rust.
template: splash
hero:
  title: Carina
  tagline: ''
---
```

Note: We keep `hero` with a title so Starlight still renders the splash template and invokes our Hero override. The tagline is empty because our component provides its own.

- [ ] **Step 4: Verify the Hero renders**

Run: `cd docs && npm run dev`

Open http://localhost:4321 and check:
- Canopus gold title with star glow dot above it
- Two panels side by side: DSL code (left) and Plan output (right)
- "Get Started" button in amber, "GitHub" button outlined
- Toggle light/dark mode — both should work
- Resize to mobile width — panels should stack vertically

- [ ] **Step 5: Commit**

```bash
git add docs/src/components/Hero.astro docs/astro.config.mjs docs/src/content/docs/index.mdx
git commit -m "feat(docs): add custom Hero with code/plan panels and Canopus accent"
```

---

### Task 2: "Why Carina?" section

**Files:**
- Modify: `docs/src/content/docs/index.mdx`
- Modify: `docs/src/styles/custom.css`

- [ ] **Step 1: Add the "Why Carina?" HTML to index.mdx**

In `docs/src/content/docs/index.mdx`, add this block AFTER the frontmatter closing `---` and the component import, but BEFORE the existing `<CardGrid>`:

```mdx
<div class="why-carina">
  <h2>Why Carina?</h2>
  <div class="why-grid">
    <div class="why-item">
      <div class="why-icon">🔒</div>
      <h3>Type Safe</h3>
      <p>DSL validates at parse time. Catch errors before any infrastructure is touched.</p>
    </div>
    <div class="why-item">
      <div class="why-icon">📋</div>
      <h3>Effects as Values</h3>
      <p>Side effects are data. Inspect your Plan before execution, not after.</p>
    </div>
    <div class="why-item">
      <div class="why-icon">🔌</div>
      <h3>Pluggable Providers</h3>
      <p>AWS Cloud Control API, native AWS SDK, and more. One DSL, multiple backends.</p>
    </div>
  </div>
</div>
```

The full `index.mdx` should look like:

```mdx
---
title: Carina
description: A strongly typed infrastructure management tool written in Rust.
template: splash
hero:
  title: Carina
  tagline: ''
---

import { Card, CardGrid, LinkCard } from '@astrojs/starlight/components';

<div class="why-carina">
  <h2>Why Carina?</h2>
  <div class="why-grid">
    <div class="why-item">
      <div class="why-icon">🔒</div>
      <h3>Type Safe</h3>
      <p>DSL validates at parse time. Catch errors before any infrastructure is touched.</p>
    </div>
    <div class="why-item">
      <div class="why-icon">📋</div>
      <h3>Effects as Values</h3>
      <p>Side effects are data. Inspect your Plan before execution, not after.</p>
    </div>
    <div class="why-item">
      <div class="why-icon">🔌</div>
      <h3>Pluggable Providers</h3>
      <p>AWS Cloud Control API, native AWS SDK, and more. One DSL, multiple backends.</p>
    </div>
  </div>
</div>

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

## Providers

<CardGrid>
  <LinkCard title="AWSCC Provider" description="AWS Cloud Control API — EC2, S3, IAM, CloudWatch Logs" href="/reference/providers/awscc/" />
  <LinkCard title="AWS Provider" description="AWS SDK — EC2, S3, STS" href="/reference/providers/aws/" />
</CardGrid>
```

- [ ] **Step 2: Add "Why Carina?" styles to custom.css**

Add at the end of `docs/src/styles/custom.css`:

```css
/* "Why Carina?" section */
.why-carina {
  text-align: center;
  padding: 2rem 0;
  margin-bottom: 1rem;
}
.why-carina h2 {
  font-size: var(--sl-text-2xl);
  color: var(--sl-color-white);
  margin-bottom: 1.5rem;
}
.why-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 1.5rem;
  text-align: left;
}
.why-item {
  padding: 1.25rem;
  border: 1px solid var(--sl-color-gray-5);
  border-radius: 0.5rem;
  background: var(--sl-color-black);
}
.why-icon {
  font-size: 1.5rem;
  margin-bottom: 0.5rem;
}
.why-item h3 {
  font-size: var(--sl-text-lg);
  color: var(--sl-color-white);
  margin: 0 0 0.5rem;
}
.why-item p {
  font-size: var(--sl-text-sm);
  color: var(--sl-color-gray-2);
  margin: 0;
  line-height: 1.6;
}
@media (max-width: 50rem) {
  .why-grid {
    grid-template-columns: 1fr;
  }
}
```

- [ ] **Step 3: Verify the full landing page**

Open http://localhost:4321 and check:
- Hero at the top with code/plan panels
- "Why Carina?" section with 3 items in a row
- Existing Feature cards below
- Provider links at the bottom
- Both dark and light modes
- Mobile responsive (why-grid stacks to 1 column)

- [ ] **Step 4: Commit**

```bash
git add docs/src/content/docs/index.mdx docs/src/styles/custom.css
git commit -m "feat(docs): add Why Carina section to landing page"
```

---

### Task 3: Build verification

- [ ] **Step 1: Run the build**

Run: `cd docs && npm run build`

Expected: Build completes with no errors, 38+ pages built.

- [ ] **Step 2: Visual check of built output**

Run: `cd docs && npm run preview`

Open the preview URL and verify the same items as Task 2 Step 3.

- [ ] **Step 3: Commit any fixes if needed**

If any issues were found, fix and commit with an appropriate message.
