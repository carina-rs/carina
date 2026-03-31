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
  const segments = pathname.replace(/^\/|\/$/g, '').split('/');
  const last = segments[segments.length - 1] || 'Home';
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
          const pathname = '/' + page.pathname;
          const isIndex = pathname === '/' || pathname === '/index' || page.pathname === '';

          const ogDir = isIndex
            ? join(outDir, 'og')
            : join(outDir, 'og', page.pathname);
          const ogPath = isIndex
            ? join(outDir, 'og', 'index.png')
            : join(ogDir, 'index.png');

          await mkdir(dirname(ogPath), { recursive: true });

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
