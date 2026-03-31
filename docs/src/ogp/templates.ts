type SatoriNode = {
  type: string;
  props: Record<string, unknown> & {
    children?: (SatoriNode | string)[] | string;
  };
};

const COLORS = {
  gold: "#fbbf24",
  white: "#f1f5f9",
  cyan: "#0891b2",
  slate: "#94a3b8",
  navyDark: "#0f172a",
  navyLight: "#1e293b",
} as const;

function regularPageTemplate(
  pageTitle: string,
  logoBase64: string,
): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        display: "flex",
        width: 1200,
        height: 630,
        backgroundImage: `linear-gradient(135deg, ${COLORS.navyDark}, ${COLORS.navyLight})`,
      },
      children: [
        {
          type: "div",
          props: {
            style: {
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              width: 420,
              height: 630,
              backgroundImage:
                "radial-gradient(circle at center, rgba(8,145,178,0.08) 0%, transparent 70%)",
              borderRight: "1px solid rgba(8,145,178,0.2)",
            },
            children: [
              {
                type: "img",
                props: {
                  src: logoBase64,
                  style: { width: 200, height: 200 },
                },
              },
            ],
          },
        },
        {
          type: "div",
          props: {
            style: {
              display: "flex",
              flexDirection: "column",
              justifyContent: "center",
              width: 780,
              height: 630,
              paddingLeft: 60,
              paddingRight: 60,
            },
            children: [
              {
                type: "div",
                props: {
                  style: {
                    color: COLORS.gold,
                    fontSize: 20,
                    textTransform: "uppercase",
                    letterSpacing: 3,
                  },
                  children: "CARINA",
                },
              },
              {
                type: "div",
                props: {
                  style: {
                    color: COLORS.white,
                    fontSize: 48,
                    fontWeight: 700,
                    marginTop: 16,
                  },
                  children: pageTitle,
                },
              },
              {
                type: "div",
                props: {
                  style: {
                    width: 80,
                    height: 3,
                    backgroundColor: COLORS.cyan,
                    marginTop: 24,
                  },
                },
              },
              {
                type: "div",
                props: {
                  style: {
                    color: COLORS.slate,
                    fontSize: 20,
                    marginTop: 24,
                  },
                  children: "Strongly Typed Infrastructure as Code",
                },
              },
            ],
          },
        },
      ],
    },
  };
}

function topPageTemplate(logoBase64: string): SatoriNode {
  return {
    type: "div",
    props: {
      style: {
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        width: 1200,
        height: 630,
        backgroundImage: `linear-gradient(160deg, ${COLORS.navyDark}, ${COLORS.navyLight}, ${COLORS.navyDark})`,
      },
      children: [
        {
          type: "div",
          props: {
            style: {
              position: "absolute",
              top: 0,
              left: "10%",
              right: "10%",
              height: 3,
              backgroundImage: `linear-gradient(90deg, transparent, ${COLORS.gold}, transparent)`,
            },
          },
        },
        {
          type: "img",
          props: {
            src: logoBase64,
            style: { width: 160, height: 160 },
          },
        },
        {
          type: "div",
          props: {
            style: {
              color: COLORS.gold,
              fontSize: 28,
              textTransform: "uppercase",
              letterSpacing: 4,
              marginTop: 24,
            },
            children: "CARINA",
          },
        },
        {
          type: "div",
          props: {
            style: {
              color: COLORS.slate,
              fontSize: 22,
              marginTop: 16,
            },
            children: "Strongly Typed Infrastructure as Code",
          },
        },
        {
          type: "div",
          props: {
            style: {
              position: "absolute",
              bottom: 0,
              left: "10%",
              right: "10%",
              height: 3,
              backgroundImage: `linear-gradient(90deg, transparent, ${COLORS.cyan}, transparent)`,
            },
          },
        },
      ],
    },
  };
}

export { regularPageTemplate, topPageTemplate, COLORS };
export type { SatoriNode };
