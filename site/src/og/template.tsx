// Template for Open Graph card. Returns the JSON tree that satori
// expects (React-element shape: { type, props: { children, style, ... } }).
// Colors are hardcoded resolved values from src/styles/tokens.css; if a
// token there shifts, update the matching entry below.

const COLORS = {
  bg: '#020617',          // --bg / --slate-950
  fg1: '#f1f5f9',         // --fg1 / --slate-100
  fg3: '#94a3b8',         // --fg3 / --slate-400
  fg4: '#64748b',         // --fg4 / --slate-500
  accent: '#fbbf24',      // --accent / --gold-400 (--hero-divider-gold)
  cyan: '#0891b2',        // --cyan-600 (--hero-divider-cyan)
  haloOuter: 'transparent',
  haloInner: 'rgba(8, 145, 178, 0.18)', // approx --hero-halo
};

// Minimal React-element factory (satori only needs { type, props }).
type Node =
  | string
  | { type: string; props: { style?: Record<string, unknown>; children?: Node | Node[] } };

function el(
  type: string,
  props: { style?: Record<string, unknown>; children?: Node | Node[] } = {},
): Node {
  return { type, props };
}

function horizonLine(color: string, top: number): Node {
  return el('div', {
    style: {
      position: 'absolute',
      top,
      left: 0,
      right: 0,
      height: 2,
      background: `linear-gradient(to right, transparent, ${color} 18%, ${color} 82%, transparent)`,
    },
  });
}

function halo(): Node {
  return el('div', {
    style: {
      position: 'absolute',
      left: 360,
      top: 200,
      width: 480,
      height: 360,
      background: `radial-gradient(closest-side, ${COLORS.haloInner}, ${COLORS.haloOuter})`,
    },
  });
}

export interface CardProps {
  title: string;
  breadcrumb: string | null;
}

export function Card({ title, breadcrumb }: CardProps): Node {
  const center: Node[] = [
    el('div', {
      style: {
        fontSize: 88,
        fontWeight: 700,
        color: COLORS.fg1,
        letterSpacing: '-0.02em',
        lineHeight: 1.05,
        maxWidth: 980,
        textAlign: 'center',
        fontFamily: 'Space Grotesk',
      },
      children: title,
    }),
  ];
  if (breadcrumb) {
    center.push(
      el('div', {
        style: {
          fontFamily: 'JetBrains Mono',
          fontSize: 24,
          color: COLORS.fg3,
          letterSpacing: '0.04em',
        },
        children: breadcrumb,
      }),
    );
  }

  return el('div', {
    style: {
      width: 1200,
      height: 630,
      display: 'flex',
      flexDirection: 'column',
      background: COLORS.bg,
      color: COLORS.fg1,
      fontFamily: 'Inter',
      position: 'relative',
      padding: '60px 80px',
    },
    children: [
      horizonLine(COLORS.accent, 64),
      halo(),
      horizonLine(COLORS.cyan, 566),
      el('div', {
        style: {
          flex: 1,
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 24,
        },
        children: center,
      }),
      el('div', {
        style: {
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          fontSize: 28,
        },
        children: [
          el('div', {
            style: { display: 'flex', alignItems: 'center', gap: 12 },
            children: [
              el('span', {
                style: { color: COLORS.accent, fontSize: 32 },
                children: '★',
              }),
              el('span', {
                style: { color: COLORS.fg1, fontWeight: 700 },
                children: 'Carina',
              }),
            ],
          }),
          el('div', {
            style: {
              fontFamily: 'JetBrains Mono',
              fontSize: 20,
              color: COLORS.fg4,
            },
            children: 'carina-rs.dev',
          }),
        ],
      }),
    ],
  });
}
