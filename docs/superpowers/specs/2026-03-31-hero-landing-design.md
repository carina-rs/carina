# Hero & Landing Page Design

## Background

Carina's documentation site has the Ocean/Teal Deep Navy color scheme applied, but the landing page (splash) is still minimal — a plain title, tagline, two buttons, four feature cards, and two provider links. The first impression doesn't convey what the tool does or why it's interesting.

## Goal

1. Replace the default Hero with a custom design featuring side-by-side DSL code and Plan output
2. Use Canopus (α Carinae) star colors for the Hero accent, keeping Cyan for the rest of the docs
3. Add a "Why Carina?" section between the Hero and existing Feature cards

## Design Decisions

### Hero Section

- **Layout**: Centered title + subtitle at top, then two panels side by side:
  - Left: DSL code (`main.crn`) with syntax highlighting — shows a VPC + Subnet definition
  - Right: Terminal mockup with `carina plan main.crn` output — shows Create effects with computed references
- **Background**: Dark gradient (`#020617` → `#0f172a`)
- **Canopus color**: Hero title and star accent use warm gold (`#fbbf24` Yellow 400, `#fef3c7` Yellow 100). This is applied only to the Hero section, not the rest of the site.
- **CTA buttons**: "Get Started" (filled, gold accent) + "GitHub" (outline)
- **Code panels**: Slate 800 background (`#1e293b`) with Slate 700 border, matching the site's code block style. Tab-like headers showing filename (`main.crn`) and terminal indicator.

### "Why Carina?" Section

Three features presented as icon + title + short description:

1. **Type Safe** — DSL validates at parse time. Catch errors before any infrastructure is touched.
2. **Effects as Values** — Side effects are data. Inspect your Plan before execution, not after.
3. **Pluggable Providers** — AWS Cloud Control API, native AWS SDK, and more. One DSL, multiple backends.

### Existing Content

Feature cards (Custom DSL, Effects as Values, Provider Architecture, Modules) and Provider link cards are preserved as-is below the new sections.

## Color Palette (Hero only)

| Role | Color | Token |
|------|-------|-------|
| Star glow | `#fef3c7` | Yellow 100 |
| Title accent | `#fbbf24` | Yellow 400 |
| CTA button | `#d97706` | Amber 600 |
| CTA hover | `#f59e0b` | Amber 500 |

The rest of the site continues to use the existing Cyan/Slate palette.

## Implementation

### Files to modify

- `docs/src/content/docs/index.mdx` — Replace default Hero with custom HTML content, add "Why Carina?" section
- `docs/src/styles/custom.css` — Add Hero-specific styles (gradient background, code panels, Canopus colors, "Why Carina?" layout)

### Files to create

- `docs/src/components/Hero.astro` — Custom Hero component (Starlight supports component overrides via `components` config in `astro.config.mjs`)

### Config change

- `docs/astro.config.mjs` — Register the custom Hero component override if needed

### Approach

Starlight's splash template uses a Hero component. Two options:

1. **Component override**: Create a custom `Hero.astro` and register it in `astro.config.mjs` under `components: { Hero: './src/components/Hero.astro' }`. This replaces the Hero across all splash pages.
2. **MDX inline**: Write the Hero content directly in `index.mdx` using HTML/Astro components, disabling the default Hero.

Option 1 is cleaner — it keeps the MDX file simple and the Hero logic isolated.

## Testing

- `npm run dev` in `docs/` and visually confirm:
  - Hero: gradient background, Canopus gold title, code + plan panels side by side
  - "Why Carina?" section with three features
  - Existing feature cards and provider links unchanged below
  - Dark and light mode both work
  - Mobile responsive: panels stack vertically on narrow screens
- `npm run build` passes with no errors
