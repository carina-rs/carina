import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const crnGrammar = JSON.parse(
  readFileSync(
    fileURLToPath(new URL('./src/grammars/crn.tmLanguage.json', import.meta.url)),
    'utf-8',
  ),
);

const carinaDark = JSON.parse(
  readFileSync(
    fileURLToPath(new URL('./src/grammars/carina-dark.theme.json', import.meta.url)),
    'utf-8',
  ),
);

export default defineConfig({
  site: 'https://carina-rs.dev',
  integrations: [mdx()],
  markdown: {
    shikiConfig: {
      theme: carinaDark,
      langs: [crnGrammar],
    },
  },
});
