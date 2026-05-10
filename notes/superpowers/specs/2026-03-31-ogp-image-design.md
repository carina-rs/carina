# OGP Image Design Spec

## Overview

Generate per-page OGP (Open Graph Protocol) images at build time for the Carina documentation site. Each page gets a unique 1200x630px PNG image that includes the Carina logo, project name, page title, and tagline.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Text content | Project name + page title + tagline | Balances brand identity with page-specific context |
| Tagline | "Strongly Typed Infrastructure as Code" | Shortened from hero tagline; fits small OGP previews |
| Generation method | Build-time (satori + resvg) | GitHub Pages is static-only; no server-side processing |
| Layout (pages) | 2-column split | Logo prominence + clear information hierarchy |
| Layout (top page) | Centered logo + tagline only | Brand-focused for project-level shares |

## Layout Specifications

### Regular Pages (2-Column Split)

```
┌──────────────┬──────────────────────────────┐
│              │                              │
│              │  CARINA          (Gold, 13px)│
│              │                              │
│   [Logo]     │  Page Title     (White, 30px)│
│   (large)    │  ───── (Cyan line)           │
│              │  Strongly Typed  (Slate, 14px)│
│              │  Infrastructure as Code      │
│              │                              │
└──────────────┴──────────────────────────────┘
 Left: 35%      Right: 65%
```

- Left panel: Logo centered, subtle radial glow behind it
- Right panel: Text left-aligned, vertical rhythm with Cyan separator line
- Divider: 1px `rgba(8,145,178,0.2)` vertical line between panels

### Top Page (Centered)

```
┌─────────────────────────────────────────────┐
│         ─── Gold gradient border ───        │
│                                             │
│              [Logo]                         │
│              (large)                        │
│                                             │
│              CARINA                         │
│   Strongly Typed Infrastructure as Code     │
│                                             │
│         ─── Cyan gradient border ───        │
└─────────────────────────────────────────────┘
```

- Logo centered, larger than regular pages
- Top border: Gold gradient (`transparent → #fbbf24 → transparent`)
- Bottom border: Cyan gradient (`transparent → #0891b2 → transparent`)

## Color Palette

| Element | Color | Value |
|---------|-------|-------|
| Background | Deep Navy gradient | `#0f172a` → `#1e293b` |
| Project name | Canopus Gold | `#fbbf24` |
| Page title | Slate 100 | `#f1f5f9` |
| Tagline | Slate 400 | `#94a3b8` |
| Accent / separator | Cyan 600 | `#0891b2` |
| Left panel divider | Cyan 600 @ 20% | `rgba(8,145,178,0.2)` |

## Image Specifications

- **Size**: 1200 x 630px (standard OGP)
- **Format**: PNG
- **Logo source**: `/docs/src/assets/favicon.png` (512x512)

## Technical Architecture

### Dependencies

- `satori` — Converts JSX/HTML-like markup to SVG. Supports a subset of CSS flexbox.
- `@resvg/resvg-js` — Converts SVG to PNG. Faster and more portable than sharp for SVG rendering.
- `sharp` — Already in package.json. Used for compositing the logo PNG onto the satori-generated background (satori cannot embed raster images directly).

### Build Integration

Create an Astro integration that runs during the build:

1. **Collect pages** — Hook into `astro:build:done` to get all generated routes from the `pages` list
2. **Generate images** — For each page, render OGP image using satori → resvg pipeline
3. **Write files** — Save PNGs to the `dist/` output directory (e.g., `dist/og/getting-started/installation.png`)
4. **Inject meta tags** — Use Starlight's `head` config or a `<Head>` component override to set the correct `og:image` URL per page

### File Structure

```
docs/
├── src/
│   ├── ogp/
│   │   ├── generate.ts       # Core generation logic (satori + resvg)
│   │   ├── templates.ts      # Layout templates (regular + top page)
│   │   └── integration.ts    # Astro integration hook
│   └── ...
├── public/
│   └── og.png                # Keep as fallback (existing)
└── ...
```

### OGP Meta Tag Strategy

Current state: A single global `og:image` meta tag in `astro.config.mjs` pointing to `/og.png`.

Target: Per-page `og:image` tags pointing to generated images (e.g., `/og/getting-started/installation.png`).

Approach:
- Override Starlight's `<Head>` component to inject page-specific `og:image` meta tag
- Use the page's slug/path to construct the image URL
- Top page uses `/og/index.png`
- Fallback: Keep `/public/og.png` for any page that fails generation

### Font

Use Inter (or a similar clean sans-serif) loaded via Google Fonts `.woff` file. satori requires font data as an ArrayBuffer at render time. Bundle the font file in the repo or download during build.

## Out of Scope

- Light mode variant (site is dark-only)
- Twitter/X-specific card images (use same OGP image)
- Dynamic runtime generation
- Localized images (docs are English-only)
