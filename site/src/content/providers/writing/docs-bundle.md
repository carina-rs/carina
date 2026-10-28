---
title: "Docs Bundle"
description: "The archive format a Carina provider uses to ship its documentation, so a provider registry can render one page per resource for each version."
---

A provider registry shows each provider version's documentation: an overview page, and one page per resource type and data source. The official providers' pages at [registry.carina-rs.dev](https://registry.carina-rs.dev/providers/index.html) are built this way. The provider supplies them as a **docs bundle**, one archive built by the same release that builds the `.wasm`.

Publishing to the registry is not open to third-party providers yet (see [Publishing](/providers/writing/publishing/)), but the format is fixed, so you can write and check your docs now. The authoritative definition is the registry protocol's [Documentation source](https://github.com/carina-rs/registry/blob/main/docs/protocol.md#documentation-source-the-docs-bundle) section; this page summarizes it.

## Layout

```text
carina-provider-hello-v0.1.0-docs.tar.gz
  index.md                 # required: provider overview and configuration
  resources/
    greeting.md            # one page per resource type
  data-sources/
    lookup.md              # one page per data source
```

`index.md` is the version's top page: what the provider manages, and how to configure its `provider` block. Each file under `resources/` and `data-sources/` becomes a page whose URL ends in its name without `.md`, the **slug**: `resources/greeting.md` is served at `/providers/<namespace>/<name>/<version>/resources/greeting`. You choose the slugs; the registry does not derive them from resource type names.

## Rules

The registry checks every rule when a version is published and refuses a bundle that breaks one; it never repairs a bundle.

| What | Rule |
|------|------|
| Archive | One gzip member holding a POSIX tar archive (`ustar` or `pax`) |
| Entries | Regular files and directories only: no symbolic or hard links. Paths longer than `ustar` allows need `pax`, not GNU long names |
| Paths | Relative, `/`-separated, no empty, `.` or `..` component; one leading `./` is ignored |
| Files | `index.md` at the root, plus `resources/<slug>.md` and `data-sources/<slug>.md`; nothing else |
| Slugs | `[a-z0-9][a-z0-9_]*`, at most 128 bytes |
| Content | UTF-8 Markdown (GitHub Flavored), nested at most 64 levels deep |
| Sizes | At most 16 MiB compressed, 4096 entries, 256 KiB per file, 64 MiB of files in total, 80 MiB decompressed |

Two more limits apply to rendering. A long run of table cells costs far more to render than its size suggests, so a file is refused when one plus the largest number of `|` characters on any line, multiplied by its number of lines, exceeds 250,000. Each page's rendered HTML is capped at 2 MiB, and the rendered pages of one version at 64 MiB in total. Ordinary documentation is far below all of these.

Docs are accepted only for stable versions, not pre-releases, and a version's docs are fixed when it is first published: corrected docs ship with the next version.

## How pages are rendered

The registry treats documentation as untrusted input. It renders GitHub Flavored Markdown with tables, strikethrough and autolinks, and then passes the HTML through an allow-list sanitizer.

- Raw HTML is dropped. An HTML block disappears entirely, and an inline tag is removed while the text around it stays.
- Images are not rendered. A bundle holds Markdown only.
- Links can go to `http`, `https` or `mailto` URLs, to a heading on the same page (`#arguments`), or to another page of the same bundle by its relative path (`[greeting](resources/greeting.md)`). A bundle link is rewritten to that page's URL for the same version. Any other link is removed and its text kept.
- Code blocks with a language the registry recognizes are syntax highlighted.

## Building the archive

Build the archive reproducibly: entries sorted by name, a fixed modification time, owner and group 0, and no timestamp in the gzip header. A publish that is retried compares the archive's digest, so a rebuild that produces different bytes from the same files is refused as a conflict.

This script uses only the Python standard library and produces the same bytes from the same files on any platform:

```python
#!/usr/bin/env python3
"""Pack a docs directory into a reproducible docs bundle.

Usage: build-docs-bundle.py <docs-dir> <output.tar.gz>
"""
import gzip
import io
import sys
import tarfile
from pathlib import Path

src, out = Path(sys.argv[1]), Path(sys.argv[2])
files = sorted(p for p in src.rglob("*") if p.is_file())

buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
    for path in files:
        data = path.read_bytes()
        info = tarfile.TarInfo(path.relative_to(src).as_posix())
        info.size = len(data)
        info.mode = 0o644
        info.mtime = 0
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        tar.addfile(info, io.BytesIO(data))

# mtime=0 and no file name in the gzip header keep the output byte-identical.
with open(out, "wb") as f, gzip.GzipFile(fileobj=f, mode="wb", mtime=0, filename="") as gz:
    gz.write(buf.getvalue())
```

```bash
python3 build-docs-bundle.py docs carina-provider-hello-v0.1.0-docs.tar.gz
```

Attach the archive to the same GitHub release as the `.wasm`. The registry fetches it from that URL when the version is published.

There is no standalone checker for these rules yet; the registry runs them at publish. Making the registry's own validator available to providers is tracked in [carina-rs/registry#139](https://github.com/carina-rs/registry/issues/139).

## Next steps

[Versioning and Compatibility](/providers/writing/versioning/) explains how protocol versions affect when you need to rebuild and release.
