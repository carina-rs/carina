---
title: "Publishing"
description: "Publishing third-party providers to the Carina provider registry is not open yet."
---

Publishing to the provider registry (`registry.carina-rs.dev`) is not open to third-party providers yet. Only the official `carina-rs/aws` and `carina-rs/awscc` providers are published there.

Opening it is tracked in [carina-rs/registry#16](https://github.com/carina-rs/registry/issues/16). This page will describe the process once that work is done.

## The release pipeline

The official providers release through [carina-rs/provider-release](https://github.com/carina-rs/provider-release), a shared set of GitHub Actions: a reusable workflow that builds the component and the docs bundle, and composite actions that create the GitHub release, sign, and publish to or yank from the registry. Signing and publishing run inside the provider repository's own release job, so the signing certificate names that repository's `release.yml` and the tag being released, which is the identity the registry checks and clients pin.

Its [`examples/release.yml`](https://github.com/carina-rs/provider-release/blob/main/examples/release.yml) is the template for a third-party provider's release workflow, and its README lists what the provider repository must provide. Reference it by a full commit SHA, never a branch or tag. Until third-party publishing opens, the registry steps cannot be used, but the build and GitHub release parts work for any provider.

## Until then

Share a provider through its GitHub repository; a configuration can install it with a `github.com/<owner>/<repo>` source, as described in [Your First Provider](/providers/writing/getting-started/#use-it-from-github).
