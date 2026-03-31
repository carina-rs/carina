import satori from "satori";
import { Resvg } from "@resvg/resvg-js";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, dirname } from "node:path";
import { regularPageTemplate, topPageTemplate } from "./templates.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

const interBold = readFileSync(join(__dirname, "fonts", "inter-bold.woff2"));
const interRegular = readFileSync(
  join(__dirname, "fonts", "inter-regular.woff2"),
);

const faviconData = readFileSync(
  join(__dirname, "..", "assets", "favicon.png"),
);
const logoBase64 = `data:image/png;base64,${faviconData.toString("base64")}`;

const FONTS = [
  {
    name: "Inter",
    data: interBold,
    weight: 700 as const,
    style: "normal" as const,
  },
  {
    name: "Inter",
    data: interRegular,
    weight: 400 as const,
    style: "normal" as const,
  },
];

export async function generateRegularOgp(
  pageTitle: string,
): Promise<Buffer> {
  const svg = await satori(regularPageTemplate(pageTitle, logoBase64), {
    width: 1200,
    height: 630,
    fonts: FONTS,
  });
  const resvg = new Resvg(svg, {
    fitTo: { mode: "width", value: 1200 },
  });
  return Buffer.from(resvg.render().asPng());
}

export async function generateTopOgp(): Promise<Buffer> {
  const svg = await satori(topPageTemplate(logoBase64), {
    width: 1200,
    height: 630,
    fonts: FONTS,
  });
  const resvg = new Resvg(svg, {
    fitTo: { mode: "width", value: 1200 },
  });
  return Buffer.from(resvg.render().asPng());
}
