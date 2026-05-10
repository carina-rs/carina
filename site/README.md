# Carina site

The Astro project that builds <https://carina-rs.dev>. Plain Astro 4
with `@astrojs/mdx`, no Starlight, no Tailwind. Design-system tokens
live in `src/styles/tokens.css`; everything else is plain CSS.

## Local development

```bash
npm install
npm run dev      # serves on http://localhost:4321
npm run build    # static output to dist/
npm run preview  # serves the built dist/
```

## Code-block syntax highlighting

Markdown code fences are highlighted by Astro's built-in Shiki, wired
in `astro.config.mjs`:

- Custom `crn` grammar — `src/grammars/crn.tmLanguage.json`.
- Custom dark theme — `src/grammars/carina-dark.theme.json`. Hex
  values mirror the resolved `--code-*` tokens in
  `src/styles/tokens.css`; if the design system shifts a `--code-*`
  value, update the matching entry in this JSON to keep them in sync.
- `bash`, `json`, `lua` and other built-in grammars inherit the same
  theme.

Shiki emits inline `style="color:#…"` per span. There are no
`--shiki-*` or `--astro-code-*` CSS overrides — earlier iterations
tried that path and were removed once the JSON theme was wired.

### Span-merging gotcha

Shiki collapses **adjacent spans that resolve to the same color**
into one. If two TextMate scopes you care about distinguishing both
map to the same hex in the theme, the visual result will look as if
the scope match never happened. The fix is theme-side: give the two
scopes different hex values. See PR
[#2947](https://github.com/carina-rs/carina/pull/2947) for the
incident where `entity.name.type.resource.*` and `variable.other.*`
were both mapped to body color and rendered as one body-colored span.

## Troubleshooting

### Code blocks render with stale colors

Astro / Vite caches Shiki output across builds. After editing
`carina-dark.theme.json` or `crn.tmLanguage.json`, clear the cache
before rebuilding:

```bash
rm -rf .astro dist node_modules/.vite
npm run build
```

`npm run dev` also benefits from this when the hot-reload server
keeps serving the old highlight HTML.

### Verifying which theme is active

Open any built page that contains a `crn` code block (e.g.
`dist/guides/writing-resources/index.html`) and grep for the `<pre>`:

```bash
grep -oE 'class="astro-code [^"]*"' dist/guides/writing-resources/index.html | head -1
# Expected: class="astro-code carina-dark"
```

If the class is `astro-code css-variables` or `astro-code
github-dark`, the build is on a different theme — check
`astro.config.mjs`.

### Dumping Shiki scopes for a snippet

When a token is rendering with the wrong color, ask Shiki what
TextMate scopes it actually emits before assuming the theme is
broken:

```bash
node --input-type=module -e "
  const { getHighlighter } = await import('shiki');
  const fs = await import('node:fs/promises');
  const grammar = JSON.parse(await fs.readFile('src/grammars/crn.tmLanguage.json', 'utf8'));
  const theme = JSON.parse(await fs.readFile('src/grammars/carina-dark.theme.json', 'utf8'));
  const hl = await getHighlighter({ themes: [theme], langs: [grammar] });
  const code = \`provider awscc {\\n  region = awscc.Region.ap_northeast_1\\n}\\n\`;
  const r = hl.codeToTokens(code, { lang: 'crn', theme: 'carina-dark', includeExplanation: true });
  for (const line of r.tokens) {
    for (const t of line) {
      const scopes = t.explanation?.map(e => e.scopes.map(s => s.scopeName).join('/')).join(' | ');
      console.log(JSON.stringify(t.content), '->', t.color, scopes);
    }
    console.log('---');
  }
"
```

If the dump shows the right scopes but the page still looks wrong,
the theme JSON's `tokenColors` is the bug. If the dump shows the
wrong scopes, that's a grammar bug — file a follow-up issue, do not
patch the theme to compensate.
