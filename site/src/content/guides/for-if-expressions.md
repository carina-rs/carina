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
  source  = 'carina-rs/awscc'
  version = '0.7.1'
  region  = awscc.Region.ap_northeast_1
}

let vpcs = for env in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = '10.0.0.0/16'

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
let vpcs = for (i, env) in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = cidr_subnet('10.0.0.0/8', 8, i)

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
  dev = '10.0.0.0/16'
  stg = '10.1.0.0/16'
}

let vpcs = for name, cidr in cidrs {
  awscc.ec2.Vpc {
    cidr_block = cidr

    tags = {
      Name        = "vpc-${name}"
      Environment = name
    }
  }
}
```

### Accessing for expression results

Iterating over a list produces a list, and iterating over a map produces a map with the same keys. Access individual elements with index syntax — `vpcs[0]` for a list result, `vpcs['dev']` for a map result:

```crn
let vpcs = for env in ['dev', 'stg'] {
  awscc.ec2.Vpc {
    cidr_block = '10.0.0.0/16'

    tags = {
      Name = "vpc-${env}"
    }
  }
}

# Reference the first VPC's attributes
awscc.ec2.Subnet {
  vpc_id            = vpcs[0].vpc_id
  cidr_block        = '10.0.1.0/24'
  availability_zone = 'ap-northeast-1a'

  tags = {
    Name = 'dev-subnet'
  }
}
```

### Local variables in for body

You can define local `let` bindings inside a `for` body:

```crn
let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

let subnets = for (i, az) in ['ap-northeast-1a', 'ap-northeast-1c'] {
  let cidr = cidr_subnet('10.0.0.0/16', 8, i)

  awscc.ec2.Subnet {
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

You can call modules inside `for` expressions to create multiple instances of a module. See the [Using Modules](/guides/using-modules/#using-modules-with-for-expressions) guide for a full example.

## If expressions

An `if` expression conditionally creates a resource or selects a value.

### Conditional resources

Create a resource only when a condition is true:

```crn
let enabled = true

let vpc = if enabled {
  awscc.ec2.Vpc {
    cidr_block = '10.0.0.0/16'

    tags = {
      Name = 'conditional-vpc'
    }
  }
}
```

When `enabled` is `false`, no VPC is created and the `let` binding is empty.

### Conditional values with if/else

Use `if`/`else` as a value expression to choose between two values:

```crn
let is_production = true

awscc.ec2.Vpc {
  cidr_block = if is_production { '10.0.0.0/16' } else { '172.16.0.0/16' }

  tags = {
    Name = if is_production { 'prod-vpc' } else { 'dev-vpc' }
  }
}
```

This creates a single VPC but uses different CIDR blocks and names depending on the condition.

Value-producing `if` expressions always require an `else` branch. A resource-producing `if` may omit `else`; when its condition is false, it creates no resource.

This is also enforced for literal conditions in root files. For example, `let x = if true { 'a' }` is now an error because there would be no value when the condition is false; fix it by adding an `else` branch, such as `let x = if true { 'a' } else { 'b' }`. This aligns statement-position values with value-position `if` expressions and prevents a binding from having no value.

### Module argument conditions

A reusable module may use a `Bool` argument, or a pure function of that argument, as an `if` condition. Carina keeps both branches in the module template and selects one for each module instance:

```crn
arguments {
  production: Bool
}

fn identity(value: Bool): Bool { value }

attributes {
  environment: String = if identity(production) { 'prod' } else { 'dev' }
}
```

The caller can instantiate the module with either value. Resource attributes are runtime values, so they cannot be used as `if` conditions; the condition must be known from literals, module arguments, and pure computations when the module is expanded.

### Combining for and if

You can use `if` value expressions inside `for` bodies. To conditionally create every resource from a `for`, put the `if` in the iterable, such as `for item in (if enabled { items } else { [] }) { ... }`. A resource-producing `for` cannot be used directly as an `if` branch.

An `if` condition must be a `Bool` value; comparison operators such as `==` are not part of the expression language (they are only available in `validation`, `require`, and `wait` conditions), so carry the flag in the data you iterate over:

```crn
let environments = {
  dev = { cidr = '10.0.0.0/16', production = false }
  stg = { cidr = '10.1.0.0/16', production = false }
  prd = { cidr = '10.2.0.0/16', production = true }
}

let vpcs = for name, env in environments {
  awscc.ec2.Vpc {
    cidr_block       = env.cidr
    instance_tenancy = if env.production { dedicated } else { default }

    tags = {
      Name        = "vpc-${name}"
      Environment = name
    }
  }
}
```
