---
title: Quick Start
description: Create your first infrastructure resource with Carina in five minutes.
---

This guide walks you through creating an AWS VPC using Carina.

<div class="cn-stepper">

<div class="cn-step">
  <div class="cn-step-marker">1</div>
  <p class="cn-step-eyebrow">Step 1 · Author</p>
  <h3 class="cn-step-title">Write a `.crn` file</h3>

Create a directory for your project and add a `main.crn` file:

```bash
mkdir my-infra && cd my-infra
```

```crn
// main.crn
provider awscc {
  source  = 'github.com/carina-rs/carina-provider-awscc'
  version = '0.5.0'
  region  = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = 'my-first-vpc'
  }
}
```

Carina downloads the WASM provider plugin from the release matching `version` (or use `revision = 'main'` to track the latest commit). The plugin is cached under `.carina/providers/`.

</div>

<div class="cn-step">
  <div class="cn-step-marker">2</div>
  <p class="cn-step-eyebrow">Step 2 · Validate</p>
  <h3 class="cn-step-title">Validate</h3>

Check that the syntax and schema are correct:

```bash
carina validate
```

This parses the `.crn` files in the current directory and reports any errors. No AWS credentials are needed.

</div>

<div class="cn-step">
  <div class="cn-step-marker">3</div>
  <p class="cn-step-eyebrow">Step 3 · Plan</p>
  <h3 class="cn-step-title">Plan</h3>

Preview what Carina will create:

```bash
carina plan
```

The plan output shows each resource and the action Carina will take (Create, Update, Delete, or Replace).

</div>

<div class="cn-step">
  <div class="cn-step-marker">4</div>
  <p class="cn-step-eyebrow">Step 4 · Apply</p>
  <h3 class="cn-step-title">Apply</h3>

Create the resources:

```bash
carina apply
```

Carina executes the plan and records the result in `carina.state.json`. This state file tracks which resources Carina manages and their current attributes.

</div>

<div class="cn-step">
  <div class="cn-step-marker">5</div>
  <p class="cn-step-eyebrow">Step 5 · Destroy</p>
  <h3 class="cn-step-title">Destroy</h3>

Tear down all managed resources:

```bash
carina destroy
```

This deletes every resource recorded in the state file.

</div>

</div>

## Next steps

- [Core Concepts](/getting-started/core-concepts/) -- understand effects, providers, and the DSL
- [Writing Resources](/guides/writing-resources/) -- learn `let` bindings, nested blocks, and data sources
- [State Management](/guides/state-management/) -- configure S3 backends and import existing resources
