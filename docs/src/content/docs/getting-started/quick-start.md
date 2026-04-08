---
title: Quick Start
description: Create your first infrastructure resource with Carina in five minutes.
---

This guide walks you through creating an AWS VPC using Carina.

## Write a `.crn` file

Create a directory for your project and add a `main.crn` file:

```bash
mkdir my-infra && cd my-infra
```

```crn
// main.crn
provider awscc {
  source  = 'github.com/carina-rs/carina-provider-awscc'
  version = '0.3.0'
  region  = 'ap-northeast-1'
}

awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-first-vpc'
  }
}
```

Replace the `source` path with the actual path to your built WASM provider plugin.

## Validate

Check that the syntax and schema are correct:

```bash
carina validate
```

This parses the `.crn` files in the current directory and reports any errors. No AWS credentials are needed.

## Plan

Preview what Carina will create:

```bash
carina plan
```

The plan output shows each resource and the action Carina will take (Create, Update, Delete, or Replace).

## Apply

Create the resources:

```bash
carina apply
```

Carina executes the plan and records the result in `carina.state.json`. This state file tracks which resources Carina manages and their current attributes.

## Destroy

Tear down all managed resources:

```bash
carina destroy
```

This deletes every resource recorded in the state file.

## Next steps

- [Core Concepts](/getting-started/core-concepts/) -- understand effects, providers, and the DSL
- [Writing Resources](/guides/writing-resources/) -- learn `let` bindings, nested blocks, and data sources
- [State Management](/guides/state-management/) -- configure S3 backends and import existing resources
