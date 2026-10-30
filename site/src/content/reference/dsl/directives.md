---
title: Directives
description: Carina-side lifecycle, ordering, routing, and change-ownership controls for resources.
---

A `directives` block controls how Carina manages a resource. Directives are not
resource attributes and are never sent to the provider.

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
| `prevent_destroy` | `false` | Reject a plan that would destroy the resource |
| `depends_on` | `[]` | Add ordering dependencies on sibling `let` bindings |
| `provider` | provider-kind default | Route the resource to a [named provider instance](/reference/dsl/syntax/#named-provider-instances) |
| `ignore_changes` | `[]` | Leave selected, authored attribute paths under another system's control after creation |

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
