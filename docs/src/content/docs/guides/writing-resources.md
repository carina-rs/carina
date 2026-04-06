---
title: "Writing Resources"
description: "Learn how to define infrastructure resources in Carina using the .crn DSL, including anonymous resources, named bindings, attributes, references, nested blocks, and data sources."
---

This guide walks you through defining infrastructure resources in Carina's `.crn` files. You will learn the two ways to declare resources, how to set attributes, reference other resources, and use nested blocks.

## Provider configuration

Every `.crn` file that declares resources needs a provider block to specify which cloud provider and region to use:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}
```

## Anonymous resources

The simplest way to define a resource is an **anonymous resource**. Use this when no other resource needs to reference it:

```crn
awscc.ec2.vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true
  instance_tenancy     = default

  tags = {
    Environment = 'production'
  }
}
```

The resource type follows the pattern `<provider>.<service>.<resource_type>`. Carina derives the resource's identity from its `name` tag or other identifying attributes.

## Named resources with `let`

When another resource needs to reference attributes from a resource, use a `let` binding:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = '10.0.1.0/24'
  availability_zone = 'ap-northeast-1a'
}
```

The `let` binding gives the resource a name (`vpc`) so you can reference its attributes (like `vpc.vpc_id`) from other resources. Carina automatically determines the dependency order -- the subnet will be created after the VPC.

Use anonymous resources when the binding is unused. Unnecessary `let` bindings add noise.

## Attribute types

Resource attributes support several value types:

```crn
awscc.ec2.vpc {
  # String
  cidr_block = '10.0.0.0/16'

  # Boolean
  enable_dns_support = true

  # Integer
  # (used in some resource attributes)

  # Namespaced enum identifier
  instance_tenancy = default

  # Map
  tags = {
    Name        = 'my-vpc'
    Environment = 'production'
  }
}
```

### String interpolation

Strings support interpolation with `${}`:

```crn
let env = 'production'

awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = "vpc-${env}"
  }
}
```

## Nested blocks

Some resources have nested configuration blocks. Repeat the block name to add multiple entries:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.security_group {
  vpc_id            = vpc.vpc_id
  group_description = 'Web server security group'

  security_group_ingress {
    ip_protocol = 'tcp'
    from_port   = 80
    to_port     = 80
    cidr_ip     = '0.0.0.0/0'
    description = 'Allow HTTP'
  }

  security_group_ingress {
    ip_protocol = 'tcp'
    from_port   = 443
    to_port     = 443
    cidr_ip     = '0.0.0.0/0'
    description = 'Allow HTTPS'
  }

  tags = {
    Name = 'web-sg'
  }
}
```

## Local `let` bindings inside blocks

You can define local variables inside a resource block with `let`. These are scoped to the block and are **not** sent to the provider:

```crn
awscc.ec2.vpc {
  let env  = 'production'
  let name = "local-let-${env}"

  cidr_block = '10.0.0.0/16'

  tags = {
    Name = name
    Env  = upper(env)
  }
}
```

## Data sources with `read`

To look up an existing resource without managing it, use the `read` keyword:

```crn
let caller = read aws.sts.caller_identity {}
```

The `read` expression fetches resource data from the provider at plan/apply time. You can then reference its attributes just like a managed resource.

## Comments

Carina supports line comments with `//` or `#`, and block comments with `/* ... */`:

```crn
# This is a line comment
// This is also a line comment

/* This is a
   block comment */

awscc.ec2.vpc {
  cidr_block = '10.0.0.0/16'  # inline comment
}
```

## Putting it together

Here is a complete example that creates a VPC with a public subnet and a route table:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = {
    Name = 'my-vpc'
  }
}

awscc.ec2.subnet {
  vpc_id                  = vpc.vpc_id
  cidr_block              = '10.0.1.0/24'
  availability_zone       = 'ap-northeast-1a'
  map_public_ip_on_launch = true

  tags = {
    Name = 'public-subnet'
  }
}

awscc.ec2.route_table {
  vpc_id = vpc.vpc_id

  tags = {
    Name = 'public-rt'
  }
}
```

Run `carina plan main.crn` to preview the changes, and `carina apply main.crn` to create the resources.
