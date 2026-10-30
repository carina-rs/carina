---
title: "Docs Bundle"
description: "The archive format a Carina provider uses to ship its documentation, so a provider registry can render one page per resource for each version."
---

A provider registry shows each provider version's documentation: an overview page, and one page per resource type and data source. The official providers' pages at [registry.carina-rs.dev](https://registry.carina-rs.dev/providers/index.html) are built this way. The provider supplies them as a **docs bundle**, one archive built by the same release that builds the `.wasm`.

Publishing to the registry is not open to third-party providers yet (see [Publishing](/providers/writing/publishing/)), but the format is fixed, so you can write your docs and build the bundle now; the registry checks it only at publish (see below). The authoritative definition is the registry protocol's [Documentation source](/reference/registry/protocol/#documentation-source-the-docs-bundle) section; this page summarizes it.

## Layout

```text
carina-provider-hello-v0.1.0-docs.tar.gz
  index.md                 # required: provider overview and configuration
  resources/
    greeting.md            # one page per resource type
  data-sources/
    lookup.md              # one page per data source
```

`index.md` is the version's top page: what the provider manages, and how to configure its `provider` block. Each file under `resources/` and `data-sources/` becomes a page. A file's name without `.md` is its **slug**, and is the last part of its page URL: `resources/greeting.md` is served at `/providers/<namespace>/<name>/<version>/resources/greeting`. You choose the slugs; the registry does not derive them from resource type names.

## Rules

The registry checks every rule when a version is published and refuses a bundle that breaks one; it never repairs a bundle.

| What | Rule |
|------|------|
| Archive | One gzip member holding a POSIX tar archive (`ustar` or `pax`) |
| Entries | Regular files and directories only, no links; see below |
| Paths | Relative, `/`-separated, no empty, `.` or `..` component; one leading `./` is ignored |
| Files | `index.md` at the root, plus `resources/<slug>.md` and `data-sources/<slug>.md`; nothing else |
| Slugs | `[a-z0-9][a-z0-9_]*`, at most 128 bytes |
| Content | UTF-8 Markdown (GitHub Flavored), nested at most 64 levels deep |
| Sizes | At most 16 MiB compressed, 4096 entries, 256 KiB per file, 64 MiB of files in total, 80 MiB decompressed |

The tar format has a few more rules: a directory entry declares size 0, paths longer than `ustar` allows need `pax` rather than GNU long names, pax global headers are not allowed, each pax extended header is at most 64 KiB, and only zero bytes may follow the end-of-archive marker. The script below produces compliant entries.

Two more limits apply to rendering. A long run of table cells costs far more to render than its size suggests, so a file is refused when (largest number of `|` characters on any line + 1) × number of lines exceeds 250,000. Each page's Markdown may render to at most 2 MiB of HTML, and everything rendered for one version, pages, navigation and page template included, at 64 MiB in total. Ordinary documentation is far below all of these.

Docs are accepted only for stable versions. A pre-release publish that carries a docs bundle is refused as a whole, so send docs only when publishing a stable version; an archive left on a pre-release's GitHub release is harmless. A version's docs are fixed when it is first published: corrected docs ship with the next version.

## Grouping pages by service

The bundle does not control how the sidebar is organized. By default it lists every resource and data source in one flat list. The registry can group a provider's pages under collapsible service headings from navigation metadata it keeps for that provider. That metadata applies to every version, including ones already published, so a provider never needs a new release to change its grouping. See the protocol's [Provider Documentation](/reference/registry/protocol/#provider-documentation) section.

## How pages are rendered

The registry treats documentation as untrusted input. It renders GitHub Flavored Markdown with tables, strikethrough and autolinks, and then passes the HTML through an allow-list sanitizer.

- Raw HTML is dropped. An HTML block disappears entirely, and an inline tag is removed while the text around it stays.
- Images are not rendered. A bundle holds Markdown only.
- Links can go to `http`, `https` or `mailto` URLs, to a heading on the same page (`#arguments`), or to another page of the same bundle by a path relative to the linking file, without a query string. From `index.md` that is `[greeting](resources/greeting.md)`; from another resource page it is `[greeting](greeting.md)`. A bundle link is rewritten to that page's URL for the same version. Any other link, including one to a file the bundle does not hold, is removed and its text kept.
- Code blocks with a language the registry recognizes are syntax highlighted.

## Building the archive

Build the archive reproducibly: entries sorted by name, a fixed modification time, owner and group 0, and no timestamp in the gzip header. A publish that is retried compares the archive's digest, so a rebuild that produces different bytes from the same files is refused as a conflict.

This script uses only the Python standard library. It packs `index.md`, `resources/*.md` and `data-sources/*.md`, following symbolic links, and skips hidden files such as `.DS_Store`. It stops when `index.md` is missing or when any other file or an invalid slug is present, which the registry would refuse. It does not check file sizes, UTF-8 or the Markdown limits in the table above. Its output depends only on the files and on the zlib that Python uses, so build the archive once and reuse that file if you retry a publish, rather than rebuilding it on another machine.

```python
#!/usr/bin/env python3
"""Pack a docs directory into a reproducible docs bundle.

Usage: build-docs-bundle.py <docs-dir> <output.tar.gz>
"""
import gzip
import io
import os
import re
import sys
import tarfile
from pathlib import Path

src, out = Path(sys.argv[1]), Path(sys.argv[2])
allowed = re.compile(r"index\.md|(resources|data-sources)/[a-z0-9][a-z0-9_]{0,127}\.md")

files, unexpected = [], []
for root, dirs, names in os.walk(src, followlinks=True):
    dirs[:] = [d for d in dirs if not d.startswith(".")]
    for file_name in names:
        if file_name.startswith("."):
            continue  # editor and OS files such as .DS_Store
        path = Path(root, file_name)
        name = path.relative_to(src).as_posix()
        (files if allowed.fullmatch(name) else unexpected).append((name, path))
files.sort()
if unexpected:
    sys.exit("not allowed in a docs bundle: " + ", ".join(sorted(n for n, _ in unexpected)))
if not any(name == "index.md" for name, _ in files):
    sys.exit("index.md is missing")

buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
    for name, path in files:
        data = path.read_bytes()
        info = tarfile.TarInfo(name)
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

Attach the archive to the same GitHub release as the `.wasm`. A `github.com/...` source ignores it; the registry fetches it from that URL when the version is published. Registry versions can be signed keylessly by the release workflow run that built them. When a version is signed, the same job of the same workflow run attempt must also sign the archive's digest, in the same way as the `.wasm`. The registry refuses docs signed by another run, unsigned docs on a signed version, and signed docs on an unsigned one. The registry protocol's [Documentation provenance](/reference/registry/protocol/#documentation-provenance) section has the details.

If you release with [provider-release](/providers/writing/publishing/#the-release-pipeline), its build workflow packs and checks the bundle with a `scripts/docs-bundle.py` copied from carina-provider-aws instead of this script. That script expects carina-provider-aws's own source layout (generated pages as `<service>/<name>.md` with a front-matter title, and an index built from a hand-written `docs/registry/index.md` plus generated page lists), so adapt it as the provider-release README describes. It allows files up to 1 MiB where the registry allows 256 KiB, and like this script it does not check the rendering limits, so keep each page under 256 KiB and within the nesting and rendering limits yourself.

There is no standalone checker for these rules yet; the registry runs them at publish.

## Next steps

[Publishing](/providers/writing/publishing/) tracks when third-party providers can publish to the registry.
