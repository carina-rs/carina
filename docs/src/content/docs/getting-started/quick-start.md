---
title: Quick Start
description: Create your first infrastructure with Carina in five minutes.
---

This guide walks through creating an S3 bucket with Carina using the AWSCC provider.

## 1. Write a `.crn` file

Create `main.crn`:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.s3.bucket {
  bucket_name = "my-first-carina-bucket"

  versioning_configuration = {
    status = Enabled
  }

  tags = {
    Environment = "dev"
  }
}
```

This declares a single S3 bucket with versioning enabled.

## 2. Validate

Check syntax and types without touching AWS:

```bash
carina validate main.crn
```

Validation catches typos, unknown attributes, and type mismatches at parse time -- before any API call.

## 3. Plan

Preview what Carina will do:

```bash
carina plan main.crn
```

The plan output shows each Effect (Create, Update, Delete) and the attributes involved. Nothing is executed yet -- the plan is just data you can inspect.

## 4. Apply

Execute the plan:

```bash
carina apply main.crn
```

Carina creates the S3 bucket and writes the resulting state.

## 5. Make a change

Edit `main.crn` to add a tag:

```crn
  tags = {
    Environment = "dev"
    Project     = "demo"
  }
```

Run `carina plan main.crn` again. The plan shows an Update effect with only the changed attributes. Run `carina apply main.crn` to apply.

## 6. Clean up

Remove all resources defined in the file:

```bash
carina destroy main.crn
```

## Next steps

- [Core Concepts](/getting-started/core-concepts/) -- understand Effects, Plans, and Providers
- [AWSCC Provider reference](/reference/providers/awscc/) -- see all supported resource types
