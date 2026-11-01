---
title: Directives
description: Carina-side lifecycle, ordering, routing, and change-ownership controls for resources.
---

A `directives` block controls how Carina manages a resource. Directives are not
resource attributes. In provider protocol 19, only delete requests carry the
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
| `depends_on` | `[]` | Add ordering dependencies on sibling `let` bindings |
| `provider` | provider-kind default | Route the resource to a [named provider instance](/reference/dsl/syntax/#named-provider-instances) |
| `ignore_changes` | `[]` | Leave selected, authored attribute paths under another system's control after creation |

## `prevent_destroy`

A provider can mark a dangerous resource type as protected by default. When it
does, omitting `prevent_destroy` keeps the protection enabled. To opt out, set
`prevent_destroy` to a value that resolves to `false`:

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

The three boolean directives (`prevent_destroy`, `create_before_destroy`, and
`force_delete`) accept expressions that resolve to a boolean,
including local `let` values, `Bool` module arguments, and loop variables.
Carina resolves module arguments after substitution and loop variables for
each element when the loop expands:

```crn
arguments { flags: list(Bool) }

for index, flag in flags {
  mock.test.resource {
    name = 'resource-${index}'
    directives { prevent_destroy = flag }
  }
}
```

A resolved `false` explicitly disables
`prevent_destroy`, including schema-default protection; an absent setting
continues to use the schema default. A resolved `true` enables protection.

For a type protected by its schema, a module argument's default cannot opt out
of that protection. If `protect: Bool = false` supplies `prevent_destroy` and
the caller omits `protect`, validation fails. The caller must pass the value
explicitly, for example `module { protect = false }`.

A value remains defaulted if **any** argument default contributes to it.
Carina combines contributors through nested calls, conditional value expressions,
coalescing, and user functions. A defaulted loop iterable contributes only to
values that reference a loop variable, including arguments passed to another
module. An explicit contributor does not erase another contributor's default
origin. Diagnostics list every contributing default argument and point to the
calls that omitted those arguments.

A literal `prevent_destroy = false` stays explicit inside a loop, even when the
iterable comes from a default. By contrast, `prevent_destroy = flag` over a
defaulted list containing `false` requires an explicit caller value. A condition
that selects whether a resource or module call exists does not contribute its
origin to values inside that body; a literal opt-out there also stays explicit.

An argument whose default refers to another argument still uses its own default.
For example, with `first: Bool = false` and `protect: Bool = first`, passing only
`first = false` leaves `protect` defaulted. Passing `protect = false` replaces
that default expression explicitly. A literal `false` passed to an inner module
is explicit when no other default contributes to the value.

This restriction applies only to `prevent_destroy` on schema-protected types.

Standalone module validation accepts a bare `Bool` argument or a bare loop
variable over a `list(Bool)` or `map(Bool)` argument. Computed expressions that
merely mention a Bool argument do not qualify for that exemption. Loop directive
expressions must depend only on the current element and local values; resource,
binding, upstream, unknown, coalesce, and value-level `if` dependencies are validation errors.

Non-boolean values and expressions that remain deferred or unknown, such as
resource output references, are validation errors. The error names the
directive and its source location; Carina never substitutes a default for an
unresolved expression.

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
