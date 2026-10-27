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

function wrapTables() {
  return function visit(node) {
    if (!node.children) return;

    node.children = node.children.map((child) => {
      visit(child);
      if (child.tagName !== 'table') return child;

      return {
        type: 'element',
        tagName: 'div',
        properties: { className: ['table-scroll'] },
        children: [child],
      };
    });
  };
}

export default defineConfig({
  site: 'https://carina-rs.dev',
  integrations: [mdx()],
  markdown: {
    rehypePlugins: [wrapTables],
    shikiConfig: {
      theme: carinaDark,
      langs: [crnGrammar],
    },
  },
});
