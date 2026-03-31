# OGP Image Generation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate per-page OGP images at Astro build time so each documentation page has a unique social sharing preview.

**Architecture:** An Astro integration hooks into `astro:build:done`, iterates over generated pages, and renders a 1200x630 PNG for each using satori (HTML→SVG) + @resvg/resvg-js (SVG→PNG) + sharp (logo compositing). A Head.astro component override injects the per-page `og:image` meta tag.

**Tech Stack:** Astro 5.5.5, Starlight 0.32.5, satori, @resvg/resvg-js, sharp (already installed), Inter font (bundled .woff)

**Spec:** `docs/superpowers/specs/2026-03-31-ogp-image-design.md`

---

### Task 1: Install Dependencies and Bundle Font

**Files:**
- Modify: `docs/package.json`
- Create: `docs/src/ogp/fonts/inter-bold.woff`
- Create: `docs/src/ogp/fonts/inter-regular.woff`

- [ ] **Step 1: Install satori and @resvg/resvg-js**

```bash
cd docs && npm install satori @resvg/resvg-js
```

Expected: Both packages added to `dependencies` in package.json.

- [ ] **Step 2: Download Inter font files**

satori requires `.woff` font data. Download Inter Bold (for titles) and Inter Regular (for body text) from Google Fonts:

```bash
mkdir -p docs/src/ogp/fonts
curl -o docs/src/ogp/fonts/inter-bold.woff "https://fonts.gstatic.com/s/inter/v18/UcCO3FwrK3iLTeHuS_nVMrMxCp50SjIw2boKoduKmMEVuFuYMZhrib2Bg-4.woff"
curl -o docs/src/ogp/fonts/inter-regular.woff "https://fonts.gstatic.com/s/inter/v18/UcCO3FwrK3iLTeHuS_nVMrMxCp50SjIw2boKoduKmMEVuLyfMZhrib2Bg-4.woff"
```

- [ ] **Step 3: Verify font files are valid**

```bash
file docs/src/ogp/fonts/inter-bold.woff
file docs/src/ogp/fonts/inter-regular.woff
```

Expected: Both reported as "Web Open Font Format" or binary data (not HTML error pages).

- [ ] **Step 4: Commit**

```bash
git add docs/package.json docs/package-lock.json docs/src/ogp/fonts/
git commit -m "feat(docs): add satori, resvg-js deps and Inter font files for OGP generation"
```

---

### Task 2: OGP Template Functions

**Files:**
- Create: `docs/src/ogp/templates.ts`

These functions return satori-compatible JSX markup (plain objects with `type`, `props`, `children`) for each layout variant.

- [ ] **Step 1: Create the templates file**

Create `docs/src/ogp/templates.ts`:

```typescript
// satori uses a React-like element tree: { type, props, children }
// We use plain objects instead of JSX to avoid React dependency.

type SatoriNode = {
  type: string;
  props: Record<string, unknown> & { children?: (SatoriNode | string)[] | string };
};

const COLORS = {
  bgDark: '#0f172a',
  bgLight: '#1e293b',
  gold: '#fbbf24',
  white: '#f1f5f9',
  slate: '#94a3b8',
  cyan: '#0891b2',
  cyanFaint: 'rgba(8,145,178,0.2)',
  goldFaint: 'rgba(251,191,36,0.05)',
};

/**
 * Regular page layout: 2-column split with logo left, text right.
 * logoBase64 is a data URI string for the logo PNG.
 */
export function regularPageTemplate(pageTitle: string, logoBase64: string): SatoriNode {
  return {
    type: 'div',
    props: {
      style: {
        width: '1200px',
        height: '630px',
        display: 'flex',
        background: `linear-gradient(135deg, ${COLORS.bgDark} 0%, ${COLORS.bgLight} 100%)`,
      },
      children: [
        // Left panel: logo
        {
          type: 'div',
          props: {
            style: {
              width: '420px',
              height: '630px',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              borderRight: `1px solid ${COLORS.cyanFaint}`,
              position: 'relative',
            },
            children: [
              {
                type: 'img',
                props: {
                  src: logoBase64,
                  width: 200,
                  height: 200,
                  style: { objectFit: 'contain' },
                },
              },
            ],
          },
        },
        // Right panel: text
        {
          type: 'div',
          props: {
            style: {
              width: '780px',
              height: '630px',
              display: 'flex',
              flexDirection: 'column',
              justifyContent: 'center',
              padding: '0 60px',
            },
            children: [
              // Project name
              {
                type: 'div',
                props: {
                  style: {
                    fontSize: '20px',
                    color: COLORS.gold,
                    letterSpacing: '3px',
                    textTransform: 'uppercase',
                    fontWeight: 700,
                    marginBottom: '16px',
                  },
                  children: 'Carina',
                },
              },
              // Page title
              {
                type: 'div',
                props: {
                  style: {
                    fontSize: '48px',
                    color: COLORS.white,
                    fontWeight: 700,
                    lineHeight: 1.2,
                    marginBottom: '24px',
                  },
                  children: pageTitle,
                },
              },
              // Cyan separator
              {
                type: 'div',
                props: {
                  style: {
                    width: '80px',
                    height: '3px',
                    background: COLORS.cyan,
                    marginBottom: '24px',
                  },
                  children: [],
                },
              },
              // Tagline
              {
                type: 'div',
                props: {
                  style: {
                    fontSize: '20px',
                    color: COLORS.slate,
                    lineHeight: 1.5,
                  },
                  children: 'Strongly Typed Infrastructure as Code',
                },
              },
            ],
          },
        },
      ],
    },
  };
}

/**
 * Top page layout: centered logo + project name + tagline.
 */
export function topPageTemplate(logoBase64: string): SatoriNode {
  return {
    type: 'div',
    props: {
      style: {
        width: '1200px',
        height: '630px',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        background: `linear-gradient(160deg, ${COLORS.bgDark} 0%, ${COLORS.bgLight} 50%, ${COLORS.bgDark} 100%)`,
        position: 'relative',
      },
      children: [
        // Top gold border
        {
          type: 'div',
          props: {
            style: {
              position: 'absolute',
              top: '0',
              left: '10%',
              right: '10%',
              height: '3px',
              background: `linear-gradient(90deg, transparent, ${COLORS.gold}, transparent)`,
            },
            children: [],
          },
        },
        // Logo
        {
          type: 'img',
          props: {
            src: logoBase64,
            width: 160,
            height: 160,
            style: { objectFit: 'contain', marginBottom: '28px' },
          },
        },
        // Project name
        {
          type: 'div',
          props: {
            style: {
              fontSize: '28px',
              color: COLORS.gold,
              letterSpacing: '4px',
              textTransform: 'uppercase',
              fontWeight: 700,
              marginBottom: '20px',
            },
            children: 'Carina',
          },
        },
        // Tagline
        {
          type: 'div',
          props: {
            style: {
              fontSize: '22px',
              color: COLORS.slate,
              letterSpacing: '1px',
            },
            children: 'Strongly Typed Infrastructure as Code',
          },
        },
        // Bottom cyan border
        {
          type: 'div',
          props: {
            style: {
              position: 'absolute',
              bottom: '0',
              left: '10%',
              right: '10%',
              height: '3px',
              background: `linear-gradient(90deg, transparent, ${COLORS.cyan}, transparent)`,
            },
            children: [],
          },
        },
      ],
    },
  };
}
```

- [ ] **Step 2: Commit**

```bash
git add docs/src/ogp/templates.ts
git commit -m "feat(docs): add OGP image layout templates (regular + top page)"
```

---

### Task 3: Core Image Generation Function

**Files:**
- Create: `docs/src/ogp/generate.ts`

This file loads fonts, reads the logo, and renders a PNG using satori → resvg.

- [ ] **Step 1: Create the generate module**

Create `docs/src/ogp/generate.ts`:

```typescript
import satori from 'satori';
import { Resvg } from '@resvg/resvg-js';
import sharp from 'sharp';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join, dirname } from 'node:path';
import { regularPageTemplate, topPageTemplate } from './templates.js';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Load fonts once at module level
const interBold = readFileSync(join(__dirname, 'fonts', 'inter-bold.woff'));
const interRegular = readFileSync(join(__dirname, 'fonts', 'inter-regular.woff'));

// Load logo and convert to base64 data URI
const logoPng = readFileSync(join(__dirname, '..', 'assets', 'favicon.png'));
const logoBase64 = `data:image/png;base64,${logoPng.toString('base64')}`;

const FONTS = [
  { name: 'Inter', data: interBold, weight: 700 as const, style: 'normal' as const },
  { name: 'Inter', data: interRegular, weight: 400 as const, style: 'normal' as const },
];

const WIDTH = 1200;
const HEIGHT = 630;

/**
 * Render an OGP image for a regular documentation page.
 * Returns a PNG buffer.
 */
export async function generateRegularOgp(pageTitle: string): Promise<Buffer> {
  const markup = regularPageTemplate(pageTitle, logoBase64);

  const svg = await satori(markup as any, {
    width: WIDTH,
    height: HEIGHT,
    fonts: FONTS,
  });

  const resvg = new Resvg(svg, {
    fitTo: { mode: 'width', value: WIDTH },
  });
  return Buffer.from(resvg.render().asPng());
}

/**
 * Render an OGP image for the top/index page.
 * Returns a PNG buffer.
 */
export async function generateTopOgp(): Promise<Buffer> {
  const markup = topPageTemplate(logoBase64);

  const svg = await satori(markup as any, {
    width: WIDTH,
    height: HEIGHT,
    fonts: FONTS,
  });

  const resvg = new Resvg(svg, {
    fitTo: { mode: 'width', value: WIDTH },
  });
  return Buffer.from(resvg.render().asPng());
}
```

- [ ] **Step 2: Commit**

```bash
git add docs/src/ogp/generate.ts
git commit -m "feat(docs): add core OGP image generation (satori + resvg pipeline)"
```

---

### Task 4: Astro Integration for Build-Time Generation

**Files:**
- Create: `docs/src/ogp/integration.ts`
- Modify: `docs/astro.config.mjs`

The integration hooks into `astro:build:done`, iterates over generated pages, and writes OGP PNGs to the output directory.

- [ ] **Step 1: Create the Astro integration**

Create `docs/src/ogp/integration.ts`:

```typescript
import type { AstroIntegration } from 'astro';
import { mkdir, writeFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { generateRegularOgp, generateTopOgp } from './generate.js';

/**
 * Extract a human-readable page title from a route pathname.
 * "/reference/providers/awscc/" → "AWSCC"
 * "/getting-started/installation/" → "Installation"
 */
function titleFromPath(pathname: string): string {
  // Remove leading/trailing slashes, take the last segment
  const segments = pathname.replace(/^\/|\/$/g, '').split('/');
  const last = segments[segments.length - 1] || 'Home';

  // Convert kebab-case to Title Case
  return last
    .split('-')
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(' ');
}

export function ogpIntegration(): AstroIntegration {
  return {
    name: 'carina-ogp',
    hooks: {
      'astro:build:done': async ({ dir, pages }) => {
        const outDir = dir.pathname;

        for (const page of pages) {
          const pathname = '/' + page.pathname; // e.g., "" or "reference/providers/awscc/"
          const isIndex = pathname === '/' || pathname === '/index' || page.pathname === '';

          // Determine output path
          const ogDir = isIndex
            ? join(outDir, 'og')
            : join(outDir, 'og', page.pathname);
          const ogPath = isIndex
            ? join(outDir, 'og', 'index.png')
            : join(ogDir, 'index.png');

          await mkdir(dirname(ogPath), { recursive: true });

          // Generate image
          const png = isIndex
            ? await generateTopOgp()
            : await generateRegularOgp(titleFromPath(pathname));

          await writeFile(ogPath, png);
          console.log(`  OGP: ${ogPath.replace(outDir, '')}`);
        }
      },
    },
  };
}
```

- [ ] **Step 2: Register the integration in astro.config.mjs**

Add the import at the top of `docs/astro.config.mjs`, after the existing imports:

```javascript
import { ogpIntegration } from './src/ogp/integration.ts';
```

Add `ogpIntegration()` to the `integrations` array, after the `starlight(...)` entry:

```javascript
export default defineConfig({
  integrations: [
    starlight({
      // ... existing config ...
    }),
    ogpIntegration(),
  ],
});
```

- [ ] **Step 3: Remove the global og:image from Starlight head config**

In `docs/astro.config.mjs`, remove the static og:image meta tag from the `head` array since per-page tags will be injected by the Head component (Task 5):

Change:
```javascript
head: [
  { tag: 'meta', attrs: { property: 'og:image', content: '/og.png' } },
],
```
To:
```javascript
head: [],
```

- [ ] **Step 4: Commit**

```bash
git add docs/src/ogp/integration.ts docs/astro.config.mjs
git commit -m "feat(docs): add Astro integration for build-time OGP image generation"
```

---

### Task 5: Per-Page og:image Meta Tag Injection

**Files:**
- Create: `docs/src/components/Head.astro`
- Modify: `docs/astro.config.mjs`

Override Starlight's `<Head>` component to inject a page-specific `og:image` meta tag.

- [ ] **Step 1: Create the Head component override**

Create `docs/src/components/Head.astro`:

```astro
---
// Override Starlight's Head to inject per-page og:image meta tag.
// Re-exports default Starlight Head and appends our og:image tag.
import Default from '@astrojs/starlight/components/Head.astro';

const pathname = Astro.url.pathname; // e.g., "/" or "/reference/providers/awscc/"
const isIndex = pathname === '/' || pathname === '/index';

const ogImagePath = isIndex ? '/og/index.png' : `/og${pathname}index.png`;
---

<Default {...Astro.props}><slot /></Default>
<meta property="og:image" content={ogImagePath} />
```

- [ ] **Step 2: Register the Head override in astro.config.mjs**

Add `Head` to the `components` object in the Starlight config:

Change:
```javascript
components: {
  Hero: './src/components/Hero.astro',
},
```
To:
```javascript
components: {
  Hero: './src/components/Hero.astro',
  Head: './src/components/Head.astro',
},
```

- [ ] **Step 3: Commit**

```bash
git add docs/src/components/Head.astro docs/astro.config.mjs
git commit -m "feat(docs): add Head override for per-page og:image meta tags"
```

---

### Task 6: Build Verification and Manual Testing

**Files:** None (verification only)

- [ ] **Step 1: Run the Astro build**

```bash
cd docs && npm run build
```

Expected: Build completes without errors. Console output shows `OGP: /og/index.png` and similar lines for each page.

- [ ] **Step 2: Verify generated OGP images exist**

```bash
find docs/dist/og -name "*.png" | head -20
```

Expected: PNG files at paths like:
- `docs/dist/og/index.png`
- `docs/dist/og/reference/providers/awscc/index.png`
- etc.

- [ ] **Step 3: Verify image dimensions**

```bash
file docs/dist/og/index.png
file docs/dist/og/reference/providers/awscc/index.png
```

Expected: PNG images, 1200x630.

- [ ] **Step 4: Verify og:image meta tags in HTML output**

```bash
grep -r 'og:image' docs/dist/ --include="*.html" | head -5
```

Expected: Each HTML file contains `<meta property="og:image" content="/og/...">` with the correct path.

- [ ] **Step 5: Visual check with dev preview**

```bash
cd docs && npm run preview
```

Open the site in a browser, view page source, and confirm `og:image` meta tags point to correct paths. Optionally check the generated PNGs directly (e.g., open `http://localhost:4321/og/index.png`).

- [ ] **Step 6: Commit any fixes if needed, then final commit**

If everything looks good with no changes needed:
```bash
echo "Build verification passed — no changes needed"
```

If fixes were made, commit them with an appropriate message.

---

### Task 7: Clean Up and Final Verification

**Files:**
- Modify: `docs/public/og.png` (keep as fallback, no change needed)

- [ ] **Step 1: Verify fallback og.png is still in place**

```bash
ls -la docs/public/og.png
```

Expected: File exists. This serves as a fallback for any edge cases where the generated image is missing.

- [ ] **Step 2: Run full build one more time**

```bash
cd docs && npm run build
```

Expected: Clean build with no warnings or errors.

- [ ] **Step 3: Commit all remaining changes**

```bash
git status
```

If there are uncommitted changes, stage and commit them. Otherwise, the implementation is complete.
