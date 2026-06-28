# Resource Identity as a First-Class Concept

This spec defines carina's resource identity as a concept independent of any schema attribute. It answers seven questions posed in #3632.

## 1. What "resource identity" means

Identity is how carina internally refers to a resource across time and across layers (parser, plan, scheduler, executor, state, display). It determines sameness: two references with the same identity denote the same infrastructure resource regardless of attribute changes; a changed identity means a different resource (delete + create).

Identity is independent of any schema attribute. `name` is one schema attribute among many (`bucket`, `region`, `tags`) and has no special relationship to identity.

A resource's identity is exactly one of two things, mutually exclusive:

- **Binding name** — the identifier in a `let` declaration (`let foo = ...` → identity is `foo`).
- **System-generated hash** — a simhash computed by the resolver pass for anonymous resources (those without a `let` binding).

A resource never carries both.

For module-expanded resources, identity is a dot-separated concatenation of segments. Each segment is itself a binding name or a system-generated hash. Module calls follow the same rule: named calls (`let x = module { ... }`) contribute their binding name as a segment; anonymous calls contribute a system-generated hash.

Example: `let o = outer { ... }` containing `let net = inner { ... }` containing `let vpc = aws.ec2.Vpc { ... }` produces the identity `o.net.vpc`.

## 2. Where identity comes from

| Resource kind | Identity | Assigned by |
|---|---|---|
| `let`-bound | Binding name | Parser |
| Anonymous | System-generated simhash | Resolver pass |
| Module-internal `let`-bound | Dot-separated: each module call's identity segment + binding name | Parser + module expander |
| Module-internal anonymous | Dot-separated: each module call's identity segment + simhash | Module expander + resolver pass |

`state import` does not assign identity. The imported resource's identity comes from the DSL (its `let` binding or anonymous hash), not from the import operation.

## 3. Which consumers use identity vs attributes

All carina-internal consumers refer to resources by identity:

- Scheduler binding index
- Plan-diff key (matching DSL resources to state entries)
- State-lookup key
- `dependency_bindings` and `depends_on`
- LSP (hover, completion, diagnostics)
- Display (plan output, progress)
- Provider WIT calls

The `name` attribute is not special in carina-core. It is one schema attribute among many. If a provider needs the `name` attribute (e.g., to pass to a cloud API), it reads it from the resource's attributes, the same way it reads `bucket`, `region`, or any other attribute. The current `unique_name_attribute` mechanism in carina-core that promotes a schema attribute value into the identity slot is eliminated.

## 4. Lifecycle

Identity becomes available at different points depending on the resource kind:

- **Parser output**: `let`-bound resources have identity immediately (the binding name). Anonymous resources do not yet have identity.
- **Module expander output**: Qualifies binding names with dot-separated module scope. Anonymous resources still lack identity.
- **Resolver pass output**: Computes simhash for anonymous resources. After this pass, every resource has identity.

All downstream consumers (differ, scheduler, state, provider) require identity to be present. A resource without identity must not reach these layers.

The resolver pass completion is the boundary. Before it, identity may be absent. After it, identity is always present. The type system enforces this boundary.

## 5. Type representation

Two separate types enforce the resolver-pass boundary:

**Before the resolver pass.** A type that permits identity to be absent (for anonymous resources not yet resolved). Only the parser and module expander work with this type.

**After the resolver pass.** `ResourceIdentity` — a resolved type that always carries an identity. All downstream consumers receive only this type. A resource without identity cannot be represented in this type, so the class of bug where an unresolved identity leaks downstream becomes a compile error.

`ResourceIdentity` is opaque. Its internal representation is a string (private field). Its public API is limited to `Display`, `Eq`, `Hash`, and similar traits — no method exposes the internal string or its structure. Only the resolver pass can construct a `ResourceIdentity`; downstream consumers cannot fabricate one.

If structural access (segment decomposition, binding-vs-hash distinction) is needed in the future, the internal representation can change without affecting downstream consumers, because the field is private and construction is restricted.

The current `ResourceName` enum (`Bound` / `Pending`) and the `name` field on `ResourceId` are replaced by this design. The name `ResourceName` is retired because it conflates identity with the `name` attribute.

## 6. State-file backward compatibility

The `name` field in `ResourceState` is replaced by the identity representation. Backward compatibility with old state files is not preserved. Old state files that stored attribute values in the `name` slot will break. If a state version bump is needed, it is done without a compatibility shim or migration layer for the old format.

## 7. Provider-side impact

The WIT `resource-id` record's `name` field is renamed to reflect identity (not an attribute). Providers that need the `name` attribute read it from the resource's `attributes`, the same as any other attribute.

This is a breaking change to the WIT boundary. Both `carina-provider-aws` and `carina-provider-awscc` require counterpart updates. Those updates are separate PRs that follow after this spec lands.

## Non-goals

This spec does not include:

- Implementation PRs. Those follow after the spec is reviewed and merged.
- A decision on sequencing relative to #3625.
- Per-symptom fixes for #3015, #2733, #2730, #2716, #2695, or carina-provider-awscc#47. These close via the consolidated identity work.

---

Refs #3632
