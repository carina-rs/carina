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
            { label: 'Writing Resources', link: '/guides/writing-resources/', badge: 'Soon' },
            { label: 'Using Modules', link: '/guides/using-modules/', badge: 'Soon' },
            { label: 'State Management', link: '/guides/state-management/', badge: 'Soon' },
            { label: 'For / If Expressions', link: '/guides/for-if-expressions/', badge: 'Soon' },
            { label: 'Functions', link: '/guides/functions/', badge: 'Soon' },
            { label: 'LSP Setup', link: '/guides/lsp-setup/', badge: 'Soon' },
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
                { label: 'plan', link: '/reference/cli/plan/', badge: 'Soon' },
                { label: 'apply', link: '/reference/cli/apply/', badge: 'Soon' },
                { label: 'validate', link: '/reference/cli/validate/', badge: 'Soon' },
                { label: 'state', link: '/reference/cli/state/', badge: 'Soon' },
                { label: 'module info', link: '/reference/cli/module-info/', badge: 'Soon' },
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
