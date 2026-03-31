---
title: Core Concepts
description: Effects as values, Plans, Providers, and State management in Carina.
---

## Effects as values

In most infrastructure tools, calling "create a VPC" immediately performs the operation. Carina takes a different approach: side effects are represented as data structures called **Effects**.

An Effect describes *what to do* without doing it. There are four kinds:

| Effect | Meaning |
|--------|---------|
| **Create** | A new resource will be provisioned |
| **Update** | An existing resource's attributes will be changed |
| **Delete** | An existing resource will be removed |
| **Read** | An existing resource will be fetched (data source, read-only) |

Because Effects are values, you can inspect, serialize, and reason about them before anything happens.

## Plan before apply

A **Plan** is a collection of Effects. The `carina plan` command computes a Plan by comparing your `.crn` file (desired state) against the current state:

```
Desired state (.crn)  +  Current state  -->  Plan (list of Effects)
```

The Plan shows exactly what will change. Only when you run `carina apply` are the Effects executed. This two-step workflow prevents surprises:

1. `carina plan` -- review what will happen
2. `carina apply` -- execute the plan

## Providers

A **Provider** is the bridge between Carina and an infrastructure API. Each provider knows how to translate Effects into real API calls.

Carina ships with two AWS providers:

| Provider | Backend | Use case |
|----------|---------|----------|
| **awscc** | AWS Cloud Control API | Recommended default. Broad resource coverage. |
| **aws** | AWS SDK (direct) | Legacy. Fewer resource types. |

You select a provider in your `.crn` file:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}
```

Resource types are namespaced by provider: `awscc.ec2.vpc`, `awscc.s3.bucket`, etc.

## Resources and bindings

A **resource** block declares a piece of infrastructure you want to exist:

```crn
awscc.ec2.vpc {
  cidr_block           = "10.0.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
}
```

When one resource needs to reference another, use a **let binding**:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

awscc.ec2.subnet {
  vpc_id     = vpc.vpc_id
  cidr_block = "10.0.1.0/24"
}
```

The binding `vpc` is resolved at plan time. Carina automatically determines the correct dependency order.

## State management

Carina tracks what it has created in a **state file**. On each `plan` or `apply`, Carina reads the current state, compares it to your `.crn` declarations, and computes the necessary Effects.

For team workflows, store state remotely with the S3 backend:

```crn
backend s3 {
  bucket = "my-carina-state"
  key    = "infra/prod/carina.crnstate"
  region = awscc.Region.ap_northeast_1
}
```

The S3 backend supports locking to prevent concurrent modifications.
