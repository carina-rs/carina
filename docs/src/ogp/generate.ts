import satori from "satori";
import { Resvg } from "@resvg/resvg-js";
import sharp from "sharp";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, dirname } from "node:path";
import { regularPageTemplate, topPageTemplate } from "./templates.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

const interBold = readFileSync(join(__dirname, "fonts", "inter-bold.ttf"));
const interRegular = readFileSync(
  join(__dirname, "fonts", "inter-regular.ttf"),
);

// Remove black background from logo by making near-black pixels transparent
async function makeLogoTransparent(): Promise<string> {
  const logoPath = join(__dirname, "..", "assets", "favicon.png");
  const { data, info } = await sharp(logoPath)
    .ensureAlpha()
    .raw()
    .toBuffer({ resolveWithObject: true });

  // Set pixels with low brightness to transparent
  for (let i = 0; i < data.length; i += 4) {
    const r = data[i];
    const g = data[i + 1];
    const b = data[i + 2];
    if (r < 30 && g < 30 && b < 30) {
      data[i + 3] = 0; // Set alpha to 0
    }
  }

  const transparentPng = await sharp(data, {
    raw: { width: info.width, height: info.height, channels: 4 },
  })
    .png()
    .toBuffer();

  return `data:image/png;base64,${transparentPng.toString("base64")}`;
}

let logoBase64: string | null = null;

async function getLogoBase64(): Promise<string> {
  if (!logoBase64) {
    logoBase64 = await makeLogoTransparent();
  }
  return logoBase64;
}

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
  const logo = await getLogoBase64();
  const svg = await satori(regularPageTemplate(pageTitle, logo), {
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
  const logo = await getLogoBase64();
  const svg = await satori(topPageTemplate(logo), {
    width: 1200,
    height: 630,
    fonts: FONTS,
  });
  const resvg = new Resvg(svg, {
    fitTo: { mode: "width", value: 1200 },
  });
  return Buffer.from(resvg.render().asPng());
}
