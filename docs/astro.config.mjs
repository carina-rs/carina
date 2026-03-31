import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { readFileSync } from 'node:fs';

const crnGrammar = JSON.parse(
  readFileSync(new URL('./custom-grammars/crn.tmLanguage.json', import.meta.url), 'utf-8')
);

export default defineConfig({
  integrations: [
    starlight({
      title: 'Carina',
      components: {
        Hero: './src/components/Hero.astro',
      },
      favicon: '/favicon.png',
      head: [
        { tag: 'meta', attrs: { property: 'og:image', content: '/og.png' } },
      ],
      social: {
        github: 'https://github.com/carina-rs/carina',
      },
      customCss: ['./src/styles/custom.css'],
      expressiveCode: {
        themes: ['github-dark-high-contrast'],
        shiki: {
          langs: [crnGrammar],
        },
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Installation', link: '/getting-started/installation/', badge: 'Soon' },
            { label: 'Quick Start', link: '/getting-started/quick-start/', badge: 'Soon' },
            { label: 'Core Concepts', link: '/getting-started/core-concepts/', badge: 'Soon' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Writing Resources', link: '/guides/writing-resources/' },
            { label: 'Using Modules', link: '/guides/using-modules/' },
            { label: 'State Management', link: '/guides/state-management/' },
            { label: 'For / If Expressions', link: '/guides/for-if-expressions/' },
            { label: 'Functions', link: '/guides/functions/' },
            { label: 'LSP Setup', link: '/guides/lsp-setup/' },
          ],
        },
        {
          label: 'Reference',
          items: [
            {
              label: 'DSL Language',
              items: [
                { label: 'Syntax', link: '/reference/dsl/syntax/' },
                { label: 'Types & Values', link: '/reference/dsl/types-and-values/' },
                { label: 'Expressions', link: '/reference/dsl/expressions/' },
                { label: 'Built-in Functions', link: '/reference/dsl/built-in-functions/' },
                { label: 'Modules', link: '/reference/dsl/modules/' },
              ],
            },
            {
              label: 'CLI Commands',
              items: [
                { label: 'plan', link: '/reference/cli/plan/' },
                { label: 'apply', link: '/reference/cli/apply/' },
                { label: 'validate', link: '/reference/cli/validate/' },
                { label: 'state', link: '/reference/cli/state/' },
                { label: 'module info', link: '/reference/cli/module-info/' },
              ],
            },
            {
              label: 'Providers',
              autogenerate: { directory: 'reference/providers' },
            },
          ],
        },
      ],
    }),
  ],
});
