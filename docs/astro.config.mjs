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
      social: {
        github: 'https://github.com/carina-rs/carina',
      },
      expressiveCode: {
        shiki: {
          langs: [crnGrammar],
        },
      },
      sidebar: [
        { label: 'Home', link: '/' },
        {
          label: 'Reference',
          items: [
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
