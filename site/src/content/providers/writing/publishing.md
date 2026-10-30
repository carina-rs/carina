---
title: "Publishing"
description: "Publishing third-party providers to the Carina provider registry is not open yet."
---

Publishing to the provider registry (`registry.carina-rs.dev`) is not open to third-party providers yet. Only the official `carina-rs/aws` and `carina-rs/awscc` providers are published there.

This page will describe the process once it opens. Until then you can run your own registry; see [Registries](/providers/writing/registries/).

## The release pipeline

The official providers release through [carina-rs/provider-release](https://github.com/carina-rs/provider-release), a shared set of GitHub Actions: a reusable workflow that builds the component and the docs bundle, and composite actions that create the GitHub release, sign and publish to the registry, and yank from it. Signing and publishing run inside the provider repository's own release job, so the signing certificate names that repository's `release.yml` and the tag being released, which is the identity the registry checks and clients pin.

Its [`examples/release.yml`](https://github.com/carina-rs/provider-release/blob/main/examples/release.yml) is the template for a third-party provider's release workflow, and its README lists what the provider repository must provide. Reference it by a full commit SHA, never a branch or tag, and before bumping that SHA review the diff and confirm it is on provider-release's `main`; its README's Pinning and Security sections explain why. Signing is part of the registry publish step, so until third-party publishing opens neither can be used. The build and GitHub release parts can be used now by a provider that meets the README's caller contract, apart from the registry's trusted-publisher entry, which only the publish step needs. For a `github.com/...` source, each release may use a tag with or without a `v` prefix and must contain exactly one `.wasm` asset; that asset may have any name. When a token is available, private repositories are supported, and the lock pins the exact tag and asset ID.

## Until then

Share a provider through its GitHub repository; a configuration can install it with a `github.com/<owner>/<repo>` source, as described in [Your First Provider](/providers/writing/getting-started/#use-it-from-github).
