---
title: "Writing Resources"
description: "Learn how to define infrastructure resources in Carina using the .crn DSL, including anonymous resources, named bindings, attributes, references, nested blocks, and data sources."
---

This guide walks you through defining infrastructure resources in Carina's `.crn` files. You will learn the two ways to declare resources, how to set attributes, reference other resources, and use nested blocks.

## Provider configuration

A configuration that declares resources needs a `provider` block for each provider it uses. The block says where to get the provider plugin (`source` and a `version` constraint) and configures it (here, the region):

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}
```

The provider block can live in any `.crn` file of the directory; all `.crn` files in a directory form one configuration. Run `carina init` once to download the plugin before `validate`, `plan`, or `apply`.

## Anonymous resources

The simplest way to define a resource is an **anonymous resource**. Use this when no other resource needs to reference it:

```crn
awscc.ec2.Vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true
  instance_tenancy     = default

  tags = {
    Environment = 'production'
  }
}
```

The resource type follows the pattern `<provider>.<service>.<resource_type>`. Anonymous resources need their provider declared and loaded so its schema is available. If the schema declares identity paths, Carina derives the address only from their configured values; a path may point to a top-level attribute or a nested struct field. Every declared identity value must be present and known at plan time. Omitted, unknown, deferred, and apply-time-only reference values are errors; write the identity value or use a `let` binding. A concrete module argument is known at plan time and can supply the value. Without declared identity, Carina falls back to configured top-level create-only attributes. Only that legacy fallback derives identity from the declaration's other attributes when no create-only value is configured.

## Named resources with `let`

When another resource needs to reference attributes from a resource, use a `let` binding:

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.Subnet {
  vpc_id            = vpc.vpc_id
  cidr_block        = '10.0.1.0/24'
  availability_zone = 'ap-northeast-1a'
}
```

The `let` binding gives the resource a name (`vpc`) so you can reference its attributes (like `vpc.vpc_id`) from other resources. Carina automatically determines the dependency order -- the subnet will be created after the VPC.

Use anonymous resources when the binding is unused and every declared identity value is known during planning. Unnecessary `let` bindings add noise. However, two anonymous resources of the same type and provider route cannot have equal declared identity values. If an identity value is available only after apply, give the resource a `let` binding. If the schema has no declared identity, multiple resources need distinct configured create-only values or `let` bindings so their identities stay stable.

## Attribute types

Resource attributes support several value types:

```crn
awscc.ec2.Vpc {
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

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'

  tags = {
    Name = "vpc-${env}"
  }
}
```

## Nested blocks

Some resources have nested configuration blocks. Repeat the block name to add multiple entries:

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.SecurityGroup {
  vpc_id            = vpc.vpc_id
  group_description = 'Web server security group'

  security_group_ingress {
    ip_protocol = tcp
    from_port   = 80
    to_port     = 80
    cidr_ip     = '0.0.0.0/0'
    description = 'Allow HTTP'
  }

  security_group_ingress {
    ip_protocol = tcp
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
awscc.ec2.Vpc {
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
let caller = read aws.sts.CallerIdentity {}
```

The `read` expression fetches resource data from the provider at plan/apply time. You can then reference its attributes just like a managed resource.

## Comments

Carina supports line comments with `//` or `#`, and block comments with `/* ... */`:

```crn
# This is a line comment
// This is also a line comment

/* This is a
   block comment */

awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'  # inline comment
}
```

## Putting it together

Here is a complete example that creates a VPC with a public subnet and a route table:

```crn
provider awscc {
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block           = '10.0.0.0/16'
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = {
    Name = 'my-vpc'
  }
}

awscc.ec2.Subnet {
  vpc_id                  = vpc.vpc_id
  cidr_block              = '10.0.1.0/24'
  availability_zone       = 'ap-northeast-1a'
  map_public_ip_on_launch = true

  tags = {
    Name = 'public-subnet'
  }
}

awscc.ec2.RouteTable {
  vpc_id = vpc.vpc_id

  tags = {
    Name = 'public-rt'
  }
}
```

Run `carina init` once in the directory containing your `.crn` files to install the provider, then `carina plan` to preview the changes and `carina apply` to create the resources.
