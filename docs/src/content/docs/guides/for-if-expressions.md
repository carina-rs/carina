---
title: "For / If Expressions"
description: "Learn how to use for loops and if/else expressions in Carina to create multiple resources, iterate over lists and maps, and conditionally include resources."
---

Carina supports `for` and `if` expressions to dynamically generate resources. This guide shows you how to iterate over collections and conditionally create resources.

## For expressions

A `for` expression creates multiple resources by iterating over a list or map.

### Iterating over a list

The simplest form iterates over a list of values:

```crn
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpcs = for env in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"

    tags = {
      Name        = "vpc-${env}"
      Environment = env
    }
  }
}
```

This creates two VPCs, one for each environment.

### Indexed iteration

Use the `(index, value)` form to access the iteration index:

```crn
let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = cidr_subnet("10.0.0.0/8", 8, i)

    tags = {
      Name        = "vpc-${env}"
      Environment = env
    }
  }
}
```

The index `i` starts at 0. Here it is used with `cidr_subnet` to assign different CIDR blocks to each VPC.

### Iterating over a map

Use `key, value` binding to iterate over map entries:

```crn
let cidrs = {
  dev = "10.0.0.0/16"
  stg = "10.1.0.0/16"
}

let vpcs = for name, cidr in cidrs {
  awscc.ec2.vpc {
    cidr_block = cidr

    tags = {
      Name        = "vpc-${name}"
      Environment = name
    }
  }
}
```

### Accessing for expression results

The result of a `for` expression is a list. You can access individual elements with index syntax:

```crn
let vpcs = for env in ["dev", "stg"] {
  awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"

    tags = {
      Name = "vpc-${env}"
    }
  }
}

# Reference the first VPC's attributes
awscc.ec2.subnet {
  vpc_id            = vpcs[0].vpc_id
  cidr_block        = "10.0.1.0/24"
  availability_zone = "ap-northeast-1a"

  tags = {
    Name = "dev-subnet"
  }
}
```

### Local variables in for body

You can define local `let` bindings inside a `for` body:

```crn
let vpc = awscc.ec2.vpc {
  cidr_block = "10.0.0.0/16"
}

let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
  let cidr = cidr_subnet("10.0.0.0/16", 8, i)

  awscc.ec2.subnet {
    vpc_id            = vpc.vpc_id
    cidr_block        = cidr
    availability_zone = az

    tags = {
      Name = "subnet-${az}"
    }
  }
}
```

### For with modules

You can call modules inside `for` expressions:

```crn
let vpc_mod = import "./modules/vpc_only"

let cidrs = {
  dev = "10.0.0.0/16"
  stg = "10.1.0.0/16"
}

let networks = for name, cidr in cidrs {
  vpc_mod {
    cidr_block = cidr
    env_name   = name
  }
}
```

See the [Using Modules](/guides/using-modules/) guide for more on modules.

## If expressions

An `if` expression conditionally creates a resource or selects a value.

### Conditional resources

Create a resource only when a condition is true:

```crn
let enabled = true

let vpc = if enabled {
  awscc.ec2.vpc {
    cidr_block = "10.0.0.0/16"

    tags = {
      Name = "conditional-vpc"
    }
  }
}
```

When `enabled` is `false`, no VPC is created and the `let` binding is empty.

### Conditional values with if/else

Use `if`/`else` as a value expression to choose between two values:

```crn
let is_production = true

awscc.ec2.vpc {
  cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }

  tags = {
    Name = if is_production { "prod-vpc" } else { "dev-vpc" }
  }
}
```

This creates a single VPC but uses different CIDR blocks and names depending on the condition.

### Combining for and if

You can use `if` expressions inside `for` bodies and vice versa:

```crn
let environments = {
  dev = "10.0.0.0/16"
  stg = "10.1.0.0/16"
  prd = "10.2.0.0/16"
}

let vpcs = for name, cidr in environments {
  awscc.ec2.vpc {
    cidr_block           = cidr
    enable_dns_hostnames = if name == "prd" { true } else { false }

    tags = {
      Name        = "vpc-${name}"
      Environment = name
    }
  }
}
```
