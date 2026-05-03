type SatoriNode = {
  type: string;
  props: Record<string, unknown> & {
    children?: (SatoriNode | string)[] | string;
  };
};

const COLORS = {
  bg: "#0b1220",
  gold: "#fbbf24",
  goldSoft: "#fde68a",
  cyan: "#0891b2",
  fg1: "#f1f5f9",
  fg2: "#cbd5e1",
  fg3: "#94a3b8",
  // legacy aliases (kept for any external imports)
  white: "#f1f5f9",
  slate: "#94a3b8",
  navyDark: "#0f172a",
  navyLight: "#1e293b",
} as const;

const TAGLINE = "Strongly Typed Infrastructure as Code";

function cardFrame(children: SatoriNode[]): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        width: 1200,
        height: 630,
        display: "flex",
        flexDirection: "column",
        padding: "70px 80px",
        backgroundColor: COLORS.bg,
        backgroundImage: [
          "radial-gradient(ellipse 70% 60% at 30% 30%, rgba(251,191,36,0.10) 0%, transparent 60%)",
          "radial-gradient(ellipse 50% 50% at 80% 80%, rgba(8,145,178,0.10) 0%, transparent 60%)",
        ].join(", "),
        position: "relative",
        fontFamily: "Inter",
      },
      children,
    },
  };
}

function topHorizon(): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        position: "absolute",
        top: 50,
        left: 80,
        right: 80,
        height: 1,
        backgroundImage: `linear-gradient(90deg, transparent 0%, ${COLORS.gold} 50%, transparent 100%)`,
        opacity: 0.55,
      },
    },
  };
}

function bottomHorizon(): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        position: "absolute",
        bottom: 50,
        left: 80,
        right: 80,
        height: 1,
        backgroundImage: `linear-gradient(90deg, transparent 0%, ${COLORS.cyan} 50%, transparent 100%)`,
        opacity: 0.55,
      },
    },
  };
}

function canopusStar(marginBottom: number): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        width: 14,
        height: 14,
        borderRadius: 7,
        backgroundColor: COLORS.goldSoft,
        boxShadow: `0 0 24px ${COLORS.goldSoft}, 0 0 60px rgba(253,230,138,0.4)`,
        marginBottom,
      },
    },
  };
}

function wordmark(): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        fontSize: 22,
        fontWeight: 700,
        color: COLORS.gold,
        letterSpacing: 6.16,
        marginBottom: 20,
      },
      children: "CARINA",
    },
  };
}

function metaRow(url: string): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        position: "absolute",
        bottom: 70,
        left: 80,
        right: 80,
        display: "flex",
        justifyContent: "space-between",
        alignItems: "center",
        fontSize: 18,
        color: COLORS.fg3,
      },
      children: [
        {
          type: "div",
          props: {
            style: { color: COLORS.gold },
            children: url,
          },
        },
        {
          type: "div",
          props: {
            style: {
              letterSpacing: 3.24,
              textTransform: "uppercase",
              fontWeight: 600,
            },
            children: "Strongly typed IaC",
          },
        },
      ],
    },
  };
}

function regularPageTemplate(
  pageTitle: string,
  _logoBase64: string,
): SatoriNode {
  void _logoBase64;
  return cardFrame([
    topHorizon(),
    bottomHorizon(),
    canopusStar(36),
    wordmark(),
    {
      type: "div",
      props: {
        style: {
          fontSize: 96,
          fontWeight: 700,
          color: COLORS.gold,
          letterSpacing: -2.4,
          lineHeight: 1.05,
          marginBottom: 28,
        },
        children: pageTitle,
      },
    },
    {
      type: "div",
      props: {
        style: {
          fontSize: 32,
          color: COLORS.fg2,
          lineHeight: 1.4,
          maxWidth: 880,
        },
        children: TAGLINE,
      },
    },
    metaRow("carina-rs.dev"),
  ]);
}

function topPageTemplate(_logoBase64: string): SatoriNode {
  void _logoBase64;
  return cardFrame([
    topHorizon(),
    bottomHorizon(),
    canopusStar(36),
    wordmark(),
    {
      type: "div",
      props: {
        style: {
          fontSize: 96,
          fontWeight: 700,
          color: COLORS.gold,
          letterSpacing: -2.4,
          lineHeight: 1.05,
          marginBottom: 28,
        },
        children: "Carina",
      },
    },
    {
      type: "div",
      props: {
        style: {
          fontSize: 32,
          color: COLORS.fg2,
          lineHeight: 1.4,
          maxWidth: 880,
        },
        children: TAGLINE,
      },
    },
    metaRow("carina-rs.dev"),
  ]);
}

export { regularPageTemplate, topPageTemplate, COLORS };
export type { SatoriNode };
