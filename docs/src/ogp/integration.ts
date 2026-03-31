import type { AstroIntegration } from 'astro';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { generateRegularOgp, generateTopOgp } from './generate.js';

/**
 * Extract the page title from the built HTML file's <title> tag.
 * Starlight generates titles like "Page Title | Site Name" — we take the part before " | ".
 */
async function titleFromHtml(htmlPath: string): Promise<string> {
  const html = await readFile(htmlPath, 'utf-8');
  const match = html.match(/<title>(.*?)<\/title>/);
  if (!match) return 'Carina';
  const full = match[1];
  // Starlight format: "Page Title | Site Name"
  const parts = full.split(' | ');
  return parts[0] || full;
}

export function ogpIntegration(): AstroIntegration {
  return {
    name: 'carina-ogp',
    hooks: {
      'astro:build:done': async ({ dir, pages }) => {
        const outDir = dir.pathname;

        for (const page of pages) {
          const pathname = '/' + page.pathname;
          const isIndex = pathname === '/' || pathname === '/index' || page.pathname === '';

          const ogDir = isIndex
            ? join(outDir, 'og')
            : join(outDir, 'og', page.pathname);
          const ogPath = isIndex
            ? join(outDir, 'og', 'index.png')
            : join(ogDir, 'index.png');

          await mkdir(dirname(ogPath), { recursive: true });

          let png: Buffer;
          if (isIndex) {
            png = await generateTopOgp();
          } else {
            // Try standard path first, fall back to flat file (e.g., 404.html)
            let htmlPath = join(outDir, page.pathname, 'index.html');
            try {
              await readFile(htmlPath);
            } catch {
              htmlPath = join(outDir, page.pathname.replace(/\/$/, '') + '.html');
            }
            let title: string;
            try {
              title = await titleFromHtml(htmlPath);
            } catch {
              // Skip pages without HTML (e.g., 404)
              continue;
            }
            png = await generateRegularOgp(title);
          }

          await writeFile(ogPath, png);
          console.log(`  OGP: ${ogPath.replace(outDir, '')}`);
        }
      },
    },
  };
}
