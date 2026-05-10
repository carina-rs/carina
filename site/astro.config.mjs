import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';

export default defineConfig({
  site: 'https://carina-rs.dev',
  integrations: [mdx()],
});
