# Documentation Site Design: Deep Navy / Ocean Teal

## Background

Carina's documentation site uses Starlight (Astro) with the default purple accent theme. The default colors don't convey the tool's identity as an infrastructure management CLI. Beyond the color scheme, several UI elements (code blocks, tables, cards, badges) use default styling that could be improved for a more polished, cohesive look.

## Goal

1. Replace the default Starlight purple accent with an Ocean/Teal color scheme (Deep Navy variant) for both dark and light modes
2. Improve the visual design of code blocks, feature cards, tables, and badges to match the new color identity

## Design Decisions

- **Color direction**: Ocean / Teal — cool, trustworthy, infrastructure-tooling feel
- **Tone**: Deep Navy — subdued Cyan 600 accent, understated and professional
- **Scope**: Both dark and light modes customized
- **Implementation**: CSS custom property overrides only (no HTML or config changes)

## Color Palette

### Dark Mode

| Role | Color | Token |
|------|-------|-------|
| Background | `#0f172a` | Slate 900 |
| Sidebar BG | `#1e293b` | Slate 800 |
| Surface | `#334155` | Slate 700 |
| Accent | `#0891b2` | Cyan 600 |
| Accent hover | `#22d3ee` | Cyan 400 |
| Accent low (subtle BG) | `#164e63` | Cyan 900 |
| Text | `#e2e8f0` | Slate 200 |
| Text muted | `#94a3b8` | Slate 400 |

### Light Mode

| Role | Color | Token |
|------|-------|-------|
| Background | `#f8fafc` | Slate 50 |
| Sidebar BG | `#f1f5f9` | Slate 100 |
| Surface | `#e2e8f0` | Slate 200 |
| Accent | `#0e7490` | Cyan 700 |
| Accent hover | `#0891b2` | Cyan 600 |
| Accent low (subtle BG) | `#ecfeff` | Cyan 50 |
| Text | `#0f172a` | Slate 900 |
| Text muted | `#64748b` | Slate 500 |

## Changes

### 1. Color scheme (accent, background, text)

Override Starlight's CSS custom properties in `docs/src/styles/custom.css`:

- `--sl-color-accent-high` / `--sl-color-accent` / `--sl-color-accent-low` — accent colors (links, buttons, badges)
- `--sl-color-white` through `--sl-color-black` — gray scale (Slate hues instead of default 224-hue)
- `--sl-color-bg-nav` / `--sl-color-bg-sidebar` — navigation backgrounds
- `--sl-color-hairline-light` / `--sl-color-hairline-shade` — borders

### 2. Code blocks

Override ExpressiveCode/Shiki CSS variables to match the site palette:

- `--astro-code-background` — use Slate 800 (dark) / Slate 100 (light) to blend with the page
- `--astro-code-token-keyword` — use Cyan accent instead of default purple
- Keep other token colors (string=green, comment=gray) as reasonable defaults

### 3. Feature cards (top page)

Style the Starlight `<Card>` components on the splash page:

- Add a subtle left border in accent color
- Add hover effect: slight background color shift + border brightens
- Icon color inherits accent

### 4. Tables

Improve Provider reference tables (existing `custom.css` table rules are preserved):

- Table header row: accent-low background color
- Alternating row stripes for readability
- Border color using hairline variables for consistency

### 5. "Soon" badges

Override the badge color to use Cyan accent instead of the default purple:

- Background: accent-low
- Text: accent-high
- Consistent in both dark and light modes

## File to modify

`docs/src/styles/custom.css` — all changes in this single file.

No changes to `astro.config.mjs`, HTML templates, or other config files.

## Testing

- `npm run dev` in `docs/` and visually confirm:
  - Dark mode and light mode toggle
  - Top page: hero, feature cards, provider link cards
  - Sidebar: navigation items, "Soon" badges, active state
  - Provider reference pages: tables, code blocks, inline code
  - Search bar styling
  - Callouts/admonitions if present
