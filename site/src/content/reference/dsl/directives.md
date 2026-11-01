---
title: Directives
description: Carina-side lifecycle, ordering, routing, and change-ownership controls for resources.
---

A `directives` block controls how Carina manages a resource. Directives are not
resource attributes. In provider protocol 16, only delete requests carry the
resolved provider-visible directives; other request resource records do not
transport authored directives.

```crn
cloud.storage.Bucket {
  name = 'artifacts'

  directives {
    prevent_destroy = true
  }
}
```

| Directive | Default | Effect |
|-----------|---------|--------|
| `force_delete` | `false` | Ask the provider to delete a resource even when it contains dependent data |
| `create_before_destroy` | `false` | Create a replacement before destroying the existing resource |
| `prevent_destroy` | resource schema (normally `false`) | Reject a plan that would destroy the resource |
| `depends_on` | `[]` | Add ordering dependencies on managed-resource handles |
| `provider` | provider-kind default | Route the resource to a [named provider instance](/reference/dsl/syntax/#named-provider-instances) |
| `ignore_changes` | `[]` | Leave selected, authored attribute paths under another system's control after creation |

## `depends_on`

Use `depends_on` when a provider attribute reference does not express all of
the ordering a resource needs. Each list item must be a managed-resource
handle, either a local binding or a resource-handle output from a module:

```crn
let record = aws.route53.RecordSet {
  # ...
}

let records = use { source = './modules/records' }
let producer = records {}

aws.acm.CertificateValidation {
  # ...

  directives {
    depends_on = [record, producer.validation_record]
  }
}
```

Carina resolves every operand to the target resource's complete,
instance-qualified address before planning. A literal, resource attribute,
value output, data source, provider, wait binding, or module call is not a
valid dependency handle. Missing or wrong-kind targets are errors; Carina does
not silently drop an edge.

Dependencies are resource-specific and affect ordering only. Depending on a
module call as a whole is not supported: that would wait for unrelated module
resources, reduce parallelism, and create false cycles. If the selected
resource is unchanged, the dependent resource remains unchanged and its
values do not become “known after apply.” When the selected resource changes,
the dependent effect is ordered after it.

## `prevent_destroy`

A provider can mark a dangerous resource type as protected by default. When it
does, omitting `prevent_destroy` keeps the protection enabled. To opt out, write
the literal value explicitly:

```crn
directives {
  prevent_destroy = false
}
```

To remove an existing resource that is protected by the schema default, opt
out in two separate applies:

1. Add `prevent_destroy = false` to the existing resource and apply that
   state-only directive change.
2. After the first apply succeeds, remove the resource block and apply the
   resulting destroy plan.

Do not add the opt-out and remove the resource block in the same change: once
the block is gone, Carina cannot read its new directive. Schema-default
protection for a removed resource also requires that resource's provider
schema to be available. If the schema is unavailable, an unspecified removed
resource is not protected by the schema default.

A non-literal expression that resolves to `true` enables protection. Any other
non-literal value is treated as unspecified and produces a warning, so it keeps
any schema-default protection enabled. Only the literal `false` opts out of a
schema default. An explicit `true` always enables protection, even for resource
types whose schema default is false.

## `ignore_changes`

Use `ignore_changes` when Carina creates a resource but another deployment
tool owns selected values afterward. The value is a list of attribute paths,
not a list of strings:

```crn
let api = cloud.function.Function {
  name = 'api'

  code = {
    image = 'registry.example.com/api:bootstrap'
  }

  memory_mb = 512

  directives {
    ignore_changes = [code.image]
  }
}

cloud.function.Alias {
  function_name = api.name
  name          = 'live'
  version       = '1'

  directives {
    ignore_changes = [version]
  }
}
```

An external deploy tool can publish a new image and move the `live` alias to a
new function version. A later Carina change to `memory_mb` will not resend the
bootstrap image or move the alias back to version `1`.

### Path syntax and validation

Paths use the authored snake-case names from the resource block. Use `.` for
struct fields and a quoted bracket key for a map entry:

```crn
directives {
  ignore_changes = [code.image, labels["Owner"]]
}
```

A struct-typed attribute may be authored with block syntax and traversed in
the same way. Lists are different: v1 does not support selecting a list index
or passing through a list at all. For example, neither `rules[0].action` nor
`rules.action` is valid. Position-based ownership would become incorrect if a
provider reordered the list.

Carina validates every path against the managed resource schema and the same
resource block. Validation reports an error when a path:

- does not exist in the schema;
- reaches a read-only or provider-populated node;
- contains a list index;
- passes through a list;
- passes through a union;
- targets a value that was not written in the resource block; or
- duplicates another `ignore_changes` entry.

### Create, update, and display behavior

Creation always sends the complete configured resource, including ignored
paths. Ignoring starts with later plans. A difference at an ignored path does
not cause an update or replacement, even when the schema marks that path as
create-only.

Readable ignored differences are omitted from normal change rows. The plan
instead displays `# N ignored attribute(s) differ` for the resource. If an
unrelated sibling change requires Carina to send the same struct or map,
Carina fills each readable ignored leaf from the latest provider read. This
prevents the update from writing the configured stale value over the value
owned by the external system.

Providers cannot return write-only values, so Carina cannot refill an ignored
write-only leaf from a read. During an unrelated update, Carina skips
reasserting a wholly ignored write-only attribute, or a containing attribute
whose write-only leaves are all ignored. If a non-ignored sibling requires the
same parent value to be sent, planning fails and identifies the resource,
parent attribute, and ignored path. Sending that parent would risk overwriting
an externally owned value that Carina cannot reconstruct. The accepted
create-time write-only record remains in state; a later configured value does
not replace it for an ignored leaf.

If another change replaces the resource, the replacement is a new create.
Carina therefore sends the currently configured values for every path,
including paths listed in `ignore_changes`.
