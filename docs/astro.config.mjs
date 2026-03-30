import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  integrations: [
    starlight({
      title: 'Carina',
      social: {
        github: 'https://github.com/carina-rs/carina',
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
