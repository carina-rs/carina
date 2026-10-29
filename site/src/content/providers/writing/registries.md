---
title: "Registries"
description: "Where users install a provider from: a GitHub repository, the official registry at registry.carina-rs.dev, or a registry you host yourself."
---

A configuration names where each provider comes from in the `source` of its `provider` block. There are three kinds of source:

```hcl
provider aws {
  source  = "carina-rs/aws"                       # the official registry
  version = "0.7.3"
}

provider hello {
  source  = "github.com/you/carina-provider-hello" # a GitHub repository
  version = "0.1.0"
}

provider custom {
  source  = "registry.example.com/myorg/custom"   # a registry you host
  version = "1.0.0"
}
```

A GitHub source needs no registry at all; [Your First Provider](/providers/writing/getting-started/#use-it-from-github) shows how to use one. The other two are provider registries: servers that tell `carina init` which versions of a provider exist and where their bytes are. Both speak the same [Registry Protocol](/reference/registry/protocol/), so the client treats them the same way.

## The official registry

A source with no hostname, such as `carina-rs/aws`, is resolved against the official registry at `registry.carina-rs.dev`. It also hosts the documentation pages of each published version, built from the provider's [docs bundle](/providers/writing/docs-bundle/).

Publishing to the official registry is not open to third-party providers yet; only the official `carina-rs` providers are published there. [Publishing](/providers/writing/publishing/) describes the release pipeline those providers use and will describe the publishing process once it opens.

## Running your own registry

You can serve a registry yourself without any server software. Every file a client reads has a fixed path, so a registry can be a set of files on a static HTTPS host, such as an object store bucket behind a CDN. [Static Registry Layout](/reference/registry/static-registry/) lists the files, the paths to serve them at, what they must contain, and how to check a file set before users depend on it. Users then name your host in the `source`, as in the `custom` block above.

The hostname is part of the provider's identity. `carina-providers.lock` records it, and the same `namespace/name` on another host is a different provider, so choose a hostname you will keep. The client never reads documentation pages, so a self-hosted registry may serve its own pages for your docs, or none.

The client verifies a self-hosted registry the same way as the official one: it checks each binary's SHA-256 against the `shasum` the registry serves for that version, verifies a Sigstore signature when one is present, and pins what it saw in the lock file. Provider credentials are separate from this: environment credentials such as AWS keys are passed only to the official providers (see [The sandbox](/providers/writing/overview/#the-sandbox)), so a provider from your registry should take its settings as `provider` block attributes.

## Trusting a self-hosted host

Clients cannot yet declare which registry hosts they trust. Today any host named in a `source` is trusted on first use: the first `carina init` against it pins where the host's discovery document says its API is and, for a signed provider, its signer, and later responses must match those pins. Nothing stops a mistyped or look-alike hostname from being trusted the same way on its first use. A per-host trust configuration, under which an undeclared host is refused before any request and a declared host must present the expected signer, is planned in [carina#4029](https://github.com/carina-rs/carina/issues/4029) and is not implemented yet.
