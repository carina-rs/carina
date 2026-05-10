import type { APIRoute, GetStaticPaths } from 'astro';
import satori from 'satori';
import sharp from 'sharp';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { Card } from '../../og/template.tsx';

interface PageEntry {
  slug: string;            // e.g. "", "getting-started/installation", "reference/dsl/syntax"
  title: string;
  breadcrumb: string | null;
}

interface Frontmatter {
  title?: string;
  description?: string;
}

// Read font binaries from the source tree at build time. cwd is the
// site/ root because that's where `astro build` is invoked.
const FONT_DIR = resolve(process.cwd(), 'src/og/fonts');
const FONTS = [
  { name: 'Space Grotesk', data: readFileSync(resolve(FONT_DIR, 'SpaceGrotesk-Bold.ttf')),    weight: 700 as const, style: 'normal' as const },
  { name: 'Inter',         data: readFileSync(resolve(FONT_DIR, 'Inter-Regular.ttf')),        weight: 400 as const, style: 'normal' as const },
  { name: 'Inter',         data: readFileSync(resolve(FONT_DIR, 'Inter-Bold.ttf')),           weight: 700 as const, style: 'normal' as const },
  { name: 'JetBrains Mono',data: readFileSync(resolve(FONT_DIR, 'JetBrainsMono-Regular.ttf')),weight: 400 as const, style: 'normal' as const },
];

// Acronyms we want to render in upper-case in breadcrumbs.
const ACRONYMS = new Set(['dsl', 'cli', 'lsp', 'tui', 'aws', 'awscc', 'sdk', 'api']);

function titleCase(segment: string): string {
  if (ACRONYMS.has(segment.toLowerCase())) {
    return segment.toUpperCase();
  }
  return segment
    .split('-')
    .map((s) => (ACRONYMS.has(s.toLowerCase()) ? s.toUpperCase() : s.charAt(0).toUpperCase() + s.slice(1)))
    .join(' ');
}

function breadcrumbFromSlug(slug: string): string | null {
  if (!slug) return null;
  const parts = slug.split('/');
  // Drop the last part (the page itself); breadcrumb = section path.
  // For top-level pages (no parent), still show the single section name.
  const sectionParts = parts.length > 1 ? parts.slice(0, -1) : parts;
  return sectionParts.map(titleCase).join(' / ');
}

function collectPages(): PageEntry[] {
  // import.meta.glob runs at build time; { eager: true } loads frontmatter directly.
  const md  = import.meta.glob<{ frontmatter: Frontmatter }>('../../content/**/*.md',  { eager: true });
  const mdx = import.meta.glob<{ frontmatter: Frontmatter }>('../../content/**/*.mdx', { eager: true });
  const all: Record<string, { frontmatter: Frontmatter }> = { ...md, ...mdx };

  const pages: PageEntry[] = [];
  for (const [path, mod] of Object.entries(all)) {
    const m = path.match(/\/content\/(.+)\.(md|mdx)$/);
    if (!m) continue;
    const slug = m[1];
    const fm = mod.frontmatter ?? {};
    const title = fm.title ?? titleCase(slug.split('/').pop() ?? '');
    pages.push({ slug, title, breadcrumb: breadcrumbFromSlug(slug) });
  }

  // Home page: slug "index", no breadcrumb.
  pages.push({ slug: 'index', title: 'Carina', breadcrumb: null });
  return pages;
}

const PAGES = collectPages();

export const getStaticPaths: GetStaticPaths = () =>
  PAGES.map((p) => ({
    params: { slug: p.slug },
    props: { title: p.title, breadcrumb: p.breadcrumb },
  }));

export const GET: APIRoute = async ({ props }) => {
  const { title, breadcrumb } = props as { title: string; breadcrumb: string | null };

  const svg = await satori(Card({ title, breadcrumb }), {
    width: 1200,
    height: 630,
    fonts: FONTS,
  });

  const png = await sharp(Buffer.from(svg)).png().toBuffer();

  return new Response(new Uint8Array(png), {
    headers: { 'Content-Type': 'image/png' },
  });
};
