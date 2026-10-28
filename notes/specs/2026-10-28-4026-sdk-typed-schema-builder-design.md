# Typed Provider Schema Authoring and One Protocol Conversion Seam: Design

Tracking: carina-rs/carina#4026; documentation follow-up: carina-rs/carina#4018

## Problem

The provider-facing schema API exposes transport records instead of the core schema model. `CarinaProvider::schemas` returns protocol `ResourceSchema`, and `provider_config_attribute_types` returns protocol `AttributeType` (`carina-plugin-sdk/src/lib.rs:699-724`). Authors can therefore construct `required = true, read_only = true` or combine provider-populated with write-only. Those states have no core meaning.

The protocol permits them because top-level attributes and nested fields carry independent booleans (`carina-provider-protocol/src/types.rs:1160-1190,1414-1435`). The host finds the contradiction only after JSON crosses the guest boundary (`carina-plugin-host/src/wasm_convert.rs:2012-2090,2300-2360`). Core uses one `InputMode`, but its transition methods still panic at runtime (`carina-core/src/schema/mod.rs:491-525,679-733,6512-6545`).

Local typestate cannot see an ancestor's mode. The shipped AWS ACM schema has the optional field `domain_validation_options[].resource_record.deferred_populate` beneath a read-only attribute, so the whole subtree is output-only even though descendant wire bits still say optional or required (`carina-provider-aws/src/schemas/generated/acm/certificate.rs:154-205`; `carina-codegen-aws/src/main.rs:773-779,1670-1681`). AWSCC has the same legacy shape in generated output. More importantly, one definition can be reached through both writable and read-only attributes (for example, a `Tag` definition used by writable `tags` and read-only `effective_tags`). Effective descendant mode must therefore be derived from each reference path; mutating or validating a definition as globally provider-populated corrupts its writable use.

Resource validators have a second invalid seam. `ResourceValidation` is `None | Declared(Vec<ValidatorType>) | InProcess(fn)`, and `with_validator` and `with_declared_validators` replace the same field (`carina-core/src/schema/mod.rs:6794-6815,7627-7635`). Both provider converters silently map `InProcess(_)` to an empty validator list (`carina-provider-aws/src/convert.rs:633-671`, `carina-provider-awscc/src/convert.rs:846-895`). A native validator can therefore pass core checks and disappear at WASM serialization.

Refined attribute types have the same loss. `RawShape::String`, `Int`, `Float`, `List`, and `Enum` expose a `CustomValidator`, but both `core_to_proto_attribute_type` implementations hide it behind `..` and emit no executable validator (`carina-provider-aws/src/convert.rs:469-526`; `carina-provider-awscc/src/convert.rs:690-758`). Identified scalar/enum validators can be replayed through `CarinaProvider::validate_custom_type`; an anonymous validator cannot. Today two generated AWSCC integer enums are anonymous. Numeric identities are also blocked at three host seams: parser validation extracts only string/enum text, `Simple`/`SchemaType` typechecking admits only string-like values, and plan/apply lookup returns early for non-strings (`carina-core/src/parser/functions.rs:443-532`; `carina-core/src/resource/mod.rs:3741-3748`; `carina-core/src/validation/typecheck.rs:209-216,264-279`; `carina-core/src/executor/provider_ready.rs:777-806`). This is already latent for core `PositiveInt`, not only the AWSCC integer enums.

Schema defaults are also not faithful through JSON. Protocol `Float` delegates to the JSON serializer, while protocol deserialization maps JSON `null` to `String("")` and every array to `List` (`carina-provider-protocol/src/types.rs:55-130`). The host parses schema JSON into protocol records and canonicalizes defaults through JSON (`carina-plugin-host/src/wasm_convert.rs:855-899,1905-1909,2043-2050`). Direct provider conversion instead preserves `StringList` (`carina-provider-aws/src/convert.rs:146-163`, `carina-provider-awscc/src/convert.rs:151-168`). Thus a typed-only round trip does not model the SDK/host seam, and non-finite floats can become `null` and then an empty string.

Finally, whole-schema validity is not an admission requirement. The current write-only graph walk treats a dangling `Ref` as containing no write-only field, attribute insertion overwrites a duplicate name, and unique-name/exclusive-required references are not definition-checked (`carina-core/src/schema/mod.rs:6867-6994,7380-7405,7643-7668`). Registries and factories still accept unchecked `ResourceSchema` (`carina-core/src/schema/mod.rs:9346-9367`; `carina-core/src/provider.rs:4476-4478`).

### Concrete inventory

Snapshot: carina `6a7a4c3202`, AWS `c7bb523cd3`, AWSCC `8e32867f0f`, and GitHub `41bc09a548`.

| Surface | Current owner and behavior |
|---|---|
| SDK schema signatures | Protocol types at `carina-plugin-sdk/src/lib.rs:699-724` |
| SDK metadata export | Direct `serde_json` at `carina-plugin-sdk/src/wasm_guest.rs:1594-1619` |
| Wire input mode | Independent booleans at `carina-provider-protocol/src/types.rs:1160-1190,1414-1435` |
| Core builders | One `InputMode`, but transition/assertion methods at `carina-core/src/schema/mod.rs:491-525,620-733,6423-6603` |
| Resource validators | Native-or-declared enum and two replacing setters at `carina-core/src/schema/mod.rs:6794-6845,7627-7641` |
| Whole-schema graph check | Write-only-only walk at `carina-core/src/schema/mod.rs:6860-6994,7395-7405` |
| Host schema/config decode | `carina-plugin-host/src/wasm_convert.rs:1905-1999,2012-2090,2300-2360` |
| Registry/factory admission | Unchecked schemas at `carina-core/src/schema/mod.rs:9346-9367` and `carina-core/src/provider.rs:4476-4478` |

The dependency direction permits one shared conversion module: core already depends on protocol, while SDK depends on protocol but not core (`carina-core/Cargo.toml:15-16`; `carina-plugin-sdk/Cargo.toml:8-17`). Conversion cannot live in protocol without a cycle or only in SDK because the host must share it.

### Builder-call inventory

Counts include executable/test schema-builder calls plus emitter branches, not comments or golden-string assertions. Generated calls and their emitters are separate migration work.

| Repository / source class | `.required()` | `.read_only()` | `.write_only()` |
|---|---:|---:|---:|
| carina, Rust schema builders | 131 | 76 | 90 |
| AWS, 42 generated schema files | 74 | 47 | 0 |
| AWS, handwritten code/tests | 31 | 2 | 1 |
| AWS, codegen emitter branches | 3 | 1 | 0 |
| **AWS migration total** | **108** | **50** | **1** |
| AWSCC, 49 generated schema files | 1,206 | 137 | 41 |
| AWSCC, handwritten code/tests | 4 | 4 | 2 |
| AWSCC, codegen emitter branches | 2 | 3 | 2 |
| **AWSCC migration total** | **1,212** | **144** | **45** |
| GitHub provider | 0 | 0 | 0 |

Additional migration inventory:

- AWS has 20 generated `.with_validator(validate_tags_map)` calls plus one emitter site, 21 sites total (`carina-codegen-aws/src/main.rs:1384-1397`). AWS's `validate_tags_map` and core's `validate_tags_key_value` have the same case-insensitive `tags` map test and the same error (`carina-aws-types/src/common/tags.rs:11-30`; `carina-core/src/schema/mod.rs:6827-6845`).
- AWSCC has 33 generated `.with_validator(|attrs| ...)` calls plus one emitter site, 34 sites total. All 33 generated bodies only call `carina_aws_types::validate_tags_map`; there is no second check and therefore no additional `ValidatorType` variant (`carina-provider-awscc/src/bin/codegen.rs:3766-3776`).
- Carina has ten resource-level `.with_validator` test calls: eight core, one CLI, and one LSP. They migrate to the real `TagsKeyValueCheck` declaration or to existing `exclusive_required`; no test-only validator variant is added.
- AWS and AWSCC each hand-build the four-field protocol `assume_role` config struct. These eight raw `proto::StructField` literals are part of the migration (`carina-provider-aws/src/main.rs:774-836`; `carina-provider-awscc/src/main.rs:1051-1112`).

Refined-validator call inventory, excluding the four carina constructor definitions:

| Repository / source class | String callback | Int callback | Float callback | List callback |
|---|---:|---:|---:|---:|
| carina, all direct calls | 109 | 9 | 3 | 6 |
| carina, production non-noop callbacks | 11 | 1 | 0 | 0 |
| AWS, direct scalar/list callbacks | 0 | 0 | 0 | 0 |
| AWSCC, generated schemas | 14 | 2 | 0 | 112 |
| AWSCC, codegen emitter/golden sites | 2 | 3 | 0 | 3 |
| AWSCC, handwritten/converter tests | 3 | 0 | 0 | 2 |

Carina's 12 production callbacks are the 11 built-in string types plus `PositiveInt` (`carina-core/src/schema/mod.rs:8694-8945`); its remaining calls are callback-free constructor plumbing, host/LSP reconstruction, or tests. Tests with a real callback gain a test identity, while noop callers use callback-free constructors. AWS has no affected scalar/list caller; its two callback-bearing enums, Region and Availability Zone, already require identities and its validator registry is keyed by full `TypeIdentity` (`carina-aws-types/src/common/region.rs:126-140`; `carina-aws-types/src/common/availability_zone.rs:64-80`; `carina-provider-aws/src/schemas/config.rs:80-157`).

All `enum_`/`enum_with_base` callback callers in the three repositories already pass the constructor's mandatory `TypeIdentity`, so they require no signature migration and are not included in the table. The two crate-private carina constructors that install only noop callbacks lose those fields.

AWSCC's 14 generated string callbacks are service-specific ARN validators and already have identities. Its two generated integer callbacks are `ec2.FlowLog.max_aggregation_interval` and `logs.LogGroup.retention_in_days`, both currently constructed with `None`. The 112 list callbacks, emitted from one codegen branch, only repeat the same min/max already stored in `length`; they are removed, not bridged. The three handwritten string sites and two converter list sites are noop reconstruction/tests and switch to callback-free constructors. The codegen row counts the Rust emitter/golden strings plus the legacy shell emitter; the integer helper feeds the schema-enum and override branches (`carina-provider-awscc/src/bin/codegen.rs:5203-5217,6620-6632,6768-6779,6887-6905`; `carina-provider-awscc/scripts/generate-schemas.sh:137`).

Generated fields below provider-populated ancestors form this path-validation inventory:

| Provider | Declared user-input descendants whose effective mode is provider-populated on these paths |
|---|---|
| AWS | 28 ACM field occurrences: 12 under `Certificate.domain_validation_options[]` (4 required, 8 optional) and 16 under `Certificate.renewal_summary` (7 required, 9 optional). The former's `resource_record` also keeps `deferred_populate` (`carina-provider-aws/src/schemas/generated/acm/certificate.rs:154-205,220-284`). |
| AWSCC | Four semantic paths: optional `ec2.Subnet.block_public_access_states.internet_gateway_block_mode`, plus optional/required/optional `s3.Bucket.metadata_configuration.destination.{table_bucket_arn,table_bucket_type,table_namespace}`. The S3 shape is emitted inline and in `MetadataConfiguration`, so this is seven generated occurrences (`carina-provider-awscc/src/schemas/generated/ec2/subnet.rs:64-65`; `carina-provider-awscc/src/schemas/generated/s3/bucket.rs:353-355,564-566`). |

`cloudfront.Function.function_metadata.function_arn` is already read-only and needs only constructor syntax migration (`carina-provider-awscc/src/schemas/generated/cloudfront/function.rs:51-52,86`). No generated AWS or AWSCC field has `write_only` beneath a provider-populated ancestor.

### Conversion inventory and semantic diff

The issue text says 24 shared functions. The inspected sources have **25** same-named counterparts: 24 `core_to_proto_*`/`proto_to_core_*` functions plus `core_input_mode_to_proto` (`carina-provider-aws/src/convert.rs:44-682`; `carina-provider-awscc/src/convert.rs:53-944`). They are:

- schema: `core_input_mode_to_proto`, `core_to_proto_attribute_schema`, `core_to_proto_attribute_type`, `core_to_proto_schema`, `core_to_proto_struct_field`, `core_to_proto_unique_name`;
- values: `core_to_proto_value`, `core_to_proto_value_map`, `proto_to_core_value`, `proto_to_core_value_map`;
- persistence: `core_to_proto_persisted_value`, `core_to_proto_persisted_value_map`, `proto_to_core_persisted_value`, `proto_to_core_persisted_value_map`, `proto_to_core_persisted_state`;
- resources: `proto_to_core_resource_id`, `core_to_proto_resource`, `proto_to_core_resource`, `proto_to_core_data_source`, `core_to_proto_directives`;
- outcomes/state: `core_to_proto_state`, `core_to_proto_applied_state`, `core_to_proto_read_outcome`, `core_to_proto_update_state`, `core_to_proto_update_outcome`.

The paired bodies have no provider-specific behavior. The shared implementation selects these existing differences deliberately:

| Difference | Shared choice |
|---|---|
| AWS calls `ResourceId::to_wire`; AWSCC wraps it | One named helper |
| Persisted map helpers differ only in visibility | Export only required helpers |
| Data-source construction differs syntactically | Convert the ID once and construct from it |
| Closed/open enum matches differ syntactically | One exhaustive arm with an explicit `values` branch |
| AWS uses a schema-kind helper; AWSCC matches inline | One exhaustive inline match |
| AWS clones re-exported `OperationConfig`; AWSCC rebuilds it | Clone the shared type |
| Both silently drop `ResourceValidation::InProcess` | Delete the native variant; no lossy arm exists |
| Both hide refined `validate` fields behind `..` | Bind every field; bridge identity-bearing scalar/enum callbacks and remove list callbacks |
| Direct value decode preserves `StringList`; host schema JSON does not | Use a schema-default-specific canonical decoder; generic value decode remains separate |

AWSCC's extra reverse schema helpers are not a second authority. They panic on unsupported legacy `Custom` bases, while the host recursively handles them, returns contextual errors, installs `defs` before attributes, sorts keys, and verifies key/name agreement (`carina-provider-awscc/src/convert.rs:464-681`; `carina-plugin-host/src/wasm_convert.rs:1927-1999,2217-2297`). The shared decoder takes the host's strict behavior.

## Goals

- Contradictory local input modes are unrepresentable to SDK authors.
- Every resource validator is a declared, portable `ValidatorType`; a validator that cannot cross WASM is unrepresentable.
- Every refined attribute callback has a `TypeIdentity` and a host implementation reachable through the existing validation bridge; anonymous callbacks are unrepresentable.
- `write_only` exists only for optional/required user input, including nested fields.
- Resource and provider-config schemas cannot enter registries or SDK serialization without whole-graph validation.
- SDK authoring uses core schema types; operation/value APIs retain the wire-safe protocol domain.
- Core/protocol conversion exists once in core; AWS/AWSCC delete their copies without changing valid wire behavior.
- The host still rejects malformed non-SDK guests; protocol JSON and component interfaces remain compatible.

## Non-goals

- Replacing wire booleans with an enum or changing protocol/state versions.
- Making the whole SDK core-typed.
- Transporting core-only completions or wait defaults.
- Changing DSL syntax, provider operation WIT, or GitHub resources.

## Decisions

1. `carina_core::protocol_convert` is the sole core/protocol conversion seam.
2. SDK schema-authoring methods use validated core types; operation/value methods stay protocol-typed.
3. Input mode is selected once by constructor; required builders have no default, and each builder exposes only meaningful modifiers.
4. Provider-populated ancestry belongs to a reference path, not to a definition or field. Below such an ancestor, declared user-input modes and authoring-only metadata are inert on that path; validation derives the effective mode independently for every path without rewriting the draft. After validation, nested mode-sensitive reads are available only through a path-carrying effective view rooted at a resource attribute or provider-config root.
5. Definition-dependent validity is represented by `ValidatedResourceSchema` and `ValidatedConfigSchema`; admission boundaries consume witnesses, not drafts.
6. `ResourceValidation::InProcess` and `with_validator` are deleted; declared validators are the only resource-validator form.
7. A `CustomValidator` can be attached only to an identity-bearing string, int, float, or enum; refined lists use declarative length/element constraints only.
8. One value-to-bridge-text function handles string, int, finite float, and resolved enum values for parser validation, typechecking, and plan/apply.
9. AWS's full-`TypeIdentity` registry moves to `carina-aws-types`; AWS and AWSCC share it, and AWSCC overlays only service-specific ARN and integer-enum entries.
10. Provider validation routes to the schema-owning factory, not every factory and not `TypeIdentity.provider`; native callbacks and decoded bridge callbacks are mutually exclusive execution paths.
11. Provider callbacks are omitted from schema JSON only after their full identity is serialized and the owning provider bridge is guaranteed; core built-ins are reconstructed locally by identity.
12. Protocol records and version 12 remain unchanged. Host decode preserves each field's declared mode and metadata; the shared core traversal applies the same path-relative rules used for native authoring.
13. Ordinary core-to-protocol value conversion becomes fallible; schema-default decoding is a separate JSON-compatible projection.

### Amendment: provider-populated ancestry is path-relative

This amendment supersedes the original Decision 4, descendant normalization rule, and code-generation guidance. Review found that definition-level normalization cannot represent a shared definition reached from both a writable attribute and a provider-populated attribute. Rewriting the definition for the latter path silently makes the former path read-only; rejecting its declared user-input fields makes the valid shared definition impossible to admit.

The corrected invariant is:

- with no provider-populated ancestor, a field's effective mode is its declared mode and all ordinary local modifier rules apply;
- below a provider-populated ancestor, the effective mode is provider-populated regardless of a field's declared optional, required, or provider-populated mode; declared user-input metadata such as defaults and `block_name` is inert on that path rather than invalid (the current schema grammar exposes defaults only on top-level attributes, so a nested default is not presently representable);
- `write_only` below a provider-populated ancestor is always invalid;
- `deferred_populate` is invalid if the field has a user-input effective mode on any reachable path, including a shared definition also reached through a writable root;
- definitions are checked structurally on their own, then mode rules are checked again through every attribute/config reference path with cycle-safe `(definition, ancestry)` state;
- host decode neither records provider-populated definitions nor rewrites their fields. It decodes declared modes and metadata, and `validate()` is the single path-aware authority.

The rule intentionally permits a draft builder to express a path-dependent combination that cannot be decided locally. Only `validate()` can turn that draft into a witness.

### Amendment: effective mode is the only nested-field read API

This amendment extends corrected Decision 4 after coordinator review. Path-aware admission alone is insufficient if a later consumer can read a shared definition's declared field mode without its reference path: completion, diagnostics, differ, state, docs, or a future nested-assignment pass could then treat a declared-required field below provider output as writable. The type system must retain the path after admission, not merely prove that every path was valid once.

`ValidatedResourceSchema` therefore produces `EffectiveAttribute`, `EffectiveField`, `EffectiveType`, and `EffectiveSchemaNode` views. Attribute lookup starts at a top-level root, and every field descent carries provider-populated ancestry and the canonical path. `ValidatedConfigSchema` provides the same ancestry-carrying type/field traversal from a config root. These views expose `effective_input_mode()`, `is_required()`, `is_read_only()`, `is_write_only()`, effective `block_name()`, deferred-populate state, and child traversal. They have no public constructors, so an effective nested-field mode cannot be manufactured without choosing a validated root path.

`StructField` no longer exposes declared-mode readers (`input_mode()`, `is_required()`, `is_read_only()`, or mode-adjacent `is_write_only()`); path-dependent block/deferred metadata is likewise read through the effective view. Core's validation and traversal implementation may inspect the private declaration. The exhaustive conversion seam remains `StructField::parts()`, whose mode member is named `declared_input_mode` to make its non-effective meaning explicit. `AttributeSchemaParts` uses the same name for consistency. Public `AttributeSchema` mode readers remain: a top-level attribute has no schema ancestor, so its declared and effective modes cannot diverge. Registry consumers receive a validation witness and use `as_schema()` only for mode-independent structural/conversion work.

The same path witness governs public value walkers. Resource values are validated and state-canonicalized only through `EffectiveAttribute` / `EffectiveType`; genuinely pathless values use an owned `RootlessType` obtained from a pathless owner such as `ValidatedConfigSchema`. `RootlessType` has no public constructor that accepts an arbitrary `AttributeType`, including a clone returned by `EffectiveType::as_type()`. The raw writable-rooted walker is crate-private. Provider-config collection canonicalization accepts `ValidatedConfigSchema` witnesses rather than callbacks that return bare `AttributeType` values, so no public walker can silently reset a resource-path type to writable ancestry.

Lookup is also split by provenance. `effective_attribute(name)` and `effective_node_at_path(path)` accept only canonical schema names and are the provider/state/refresh/differ entry points. `effective_attribute_by_authored_name(name)` and `effective_node_at_authored_path(path)` may additionally resolve an active `block_name` alias. This prevents a stray provider-state key that happens to equal an authoring alias from being canonicalized as the corresponding canonical attribute.

## Design

### Select input mode once

Replace transition methods with mode-selecting constructors:

```rust
AttributeSchema::optional(name, ty)
AttributeSchema::required(name, ty)
AttributeSchema::provider_populated(name, ty)

StructField::optional(name, ty)
StructField::required(name, ty)
StructField::provider_populated(name, ty)
```

Each constructor returns a mode-specific builder and `build()` returns the opaque immutable component. Every public modifier is placed explicitly:

| Builder | Methods, in addition to `build()` |
|---|---|
| Optional user-input attribute | `with_default`, `write_only`, `create_only`, `identity`, `with_description`, `with_provider_name`, `with_block_name`, `with_completions`, `removable`, `non_removable` |
| Required user-input attribute | `write_only`, `create_only`, `identity`, `with_description`, `with_provider_name`, `with_block_name`, `with_completions` |
| Provider-populated attribute | `with_description`, `with_provider_name`, `deferred_populate` |
| Optional or required user-input struct field | `write_only`, `with_description`, `with_provider_name`, `with_block_name`, `deferred_populate` |
| Provider-populated struct field | `with_description`, `with_provider_name`, `with_block_name`, `deferred_populate` |

Methods absent from a row do not exist for that builder. Required input is intrinsically non-removable, so only optional input gets removability overrides. Required input also has no default: core currently treats a missing required attribute as valid when a default exists, making the combination optional-with-default in behavior, and no generated schema uses it (`carina-core/src/schema/mod.rs:7919-7921`). Struct fields continue to have no default, create-only, identity, completion, or removability API because the protocol has no such fields (`carina-provider-protocol/src/types.rs:1414-1435`).

The placement follows runtime meaning, not merely current call patterns:

- defaults fill omitted user input; write-only describes accepted input; block names and completions are authoring aids; removability applies to an optional desired attribute;
- create-only and removability classify desired-side changes, while identity hashes desired values; provider-only output has no stable authored value for any of them (`carina-core/src/differ/plan.rs:46-130`; `carina-core/src/schema/mod.rs:7745-7769`; `carina-core/src/identifier/mod.rs:130-170`);
- deferred-populate marks a chained read of asynchronously produced output, so it is legal only on a field whose effective mode is provider-populated on every reachable path (`carina-core/src/validation/deferred_populate.rs:305-351`);
- description is useful for either mode, and provider-name maps both request and response properties, so both remain shared.

Per-builder typestate enforces combinations that are intrinsically local; `ResourceSchema::validate()` supplies the missing path context. An optional or required field may carry `deferred_populate` in a draft because the same declaration is valid when all paths to it are provider-populated. Conversely, a provider-populated field may retain `block_name` when it is shadowed by an ancestor, although validation rejects that modifier when the field itself is the first provider-populated node. AWS/AWSCC codegen preserves declared field modes and metadata rather than propagating an ancestry flag into shared definitions.

Protocol decode selects the declared builder strictly at every location:

| `required` | `read_only` | Builder |
|---:|---:|---|
| false | false | optional user input |
| true | false | required user input |
| false | true | provider-populated |
| true | true | typed decode error |

The host uses this same strict declared-mode table at every location and does not carry ancestry while decoding. A declared `(true, true)` remains a local typed decode error. All other descendant bits and metadata are preserved exactly; after the entire graph is built, core validation derives effective modes per reference path. `write_only` beneath provider output and `deferred_populate` on any writable path are therefore diagnosed by the same traversal for native and decoded schemas. Remove `new().required()` and `new().read_only()` after migration; compatibility shims are not exposed to authors.

### Identity-bound attribute-type validators

Replace the always-present noop callback on refined primitives with a private binding that makes the invariant structural:

```rust
enum RefinementBinding {
    MetadataOnly(Option<TypeIdentity>),
    Validator {
        identity: TypeIdentity,
        callback: CustomValidator,
    },
}
```

The callback-bearing constructors take `TypeIdentity`, not `Option<TypeIdentity>`:

```rust
AttributeType::refined_string_with_validator(identity: TypeIdentity, ...)
AttributeType::refined_int_with_validator(identity: TypeIdentity, ...)
AttributeType::refined_float_with_validator(identity: TypeIdentity, ...)
```

Callback-free `refined_string`, `refined_int`, and `refined_float` retain `Option<TypeIdentity>` because a host-decoded provider type has an identity but deliberately no process-local callback. `enum_` and `enum_with_base` already require `TypeIdentity` whenever their optional callback is present. `refined_list` becomes callback-free; `length` and the element type express every production list constraint, including all 112 AWSCC sites. No identity-bearing list-callback constructor is added while the bridge accepts only scalar text, and the crate-private resource-handle/string-literal variants lose their noop callback fields.

The host replays an identified provider validator through `CustomTypeLookup -> ProviderFactory::validate_custom_type -> CarinaProvider::validate_custom_type`. The WIT method remains string-valued. Canonical bridge text is:

| Core value | Provider text |
|---|---|
| String | Literal provider-facing text |
| Int | Base-10 `i64::to_string()` with no whitespace or leading `+` |
| Float | Finite shortest round-trippable `f64::to_string()` text |
| Enum | Resolved API spelling |

One core value-to-bridge-text function produces `CanonicalCustomTypeValue` and is the only scalar canonicalization gate. It is used by (1) `validate_custom_type`/`provider_context_lookup`, which feed `carina validate` and LSP resource validation, (2) `Simple`/`SchemaType` typechecking, and (3) `provider_custom_type_lookup` for plan/apply (`carina-core/src/validation/mod.rs:1583`; `carina-lsp/src/diagnostics/mod.rs:1084,1228`; `carina-core/src/validation/typecheck.rs:209-216,264-279`; `carina-core/src/executor/provider_ready.rs:777-806`). Shape checks still reject a number for a string identity; numeric identities now reach the same canonicalizer. A provider-side integer validator parses the canonical base-10 text with `str::parse::<i64>()`. Thus FlowLog `60` reaches AWSCC as `"60"`, while `61` reaches it as `"61"` and is rejected.

The transport path no longer extracts schema callbacks into the string-only `ProviderContext.validators` adapter, which currently rebuilds string/int/float inputs as `ConcreteValue::String` (`carina-core/src/provider.rs:4654-4677`). A native schema executes its typed local callback and does not call the factory bridge; a host-decoded schema has no local provider callback and calls its owning factory once. A counter-based test proves exactly one invocation for each path and zero invocations in non-owning factories; this is the required behavior, not a claim about the current implementation.

Core-owned bare identities use one built-in registry, including `PositiveInt` and `Email`; protocol decode and parser dispatch share it. The full AWS `HashMap<TypeIdentity, CustomValidatorFn>` moves from `carina-provider-aws` into `carina-aws-types` and remains built from the same constructors as the schemas (`carina-provider-aws/src/schemas/config.rs:80-157`). AWS uses that registry directly. AWSCC reuses it and overlays only its generated service-specific ARN predicates and two integer-enum predicates, all keyed by exact identity. The generic identity returned by `carina_aws_types::arn()` remains in the shared map, so one exact entry continues to cover the 100 current generated `carina_aws_types::arn()` occurrences.

This re-keys the whole AWSCC map rather than adding a short ARN list. Today both AWSCC entry points reduce identity to `pascal_to_snake(kind)` (`carina-provider-awscc/src/lib.rs:142-160`; `carina-provider-awscc/src/main.rs:607-625`). Of the 37 string keys in `awscc_validators`, only `arn` is reachable in the inspected snapshot: for example, `aws_account_id()` identifies `AccountId`, which projects to `account_id`, not the map's `aws_account_id`; Region and Availability Zone identify `Region` and `ZoneName`, so their 11 and 3 generated uses lose callbacks after WASM decode (`carina-provider-awscc/src/schemas/config.rs:74-139`; `carina-aws-types/src/common/account.rs:22-28`; `carina-aws-types/src/common/region.rs:125-140`; `carina-aws-types/src/common/availability_zone.rs:63-80`).

The host routes an identity only to the factory that owns the schema or type expression. Broadcasting is incorrect because AWS and AWSCC can use the same shared identity, causing duplicate predicates and making one provider's unknown-type behavior part of another provider's validation. Routing must not use `identity.provider`: AWSCC intentionally uses shared identities in the `aws` type namespace. Resource/config validation already knows the registry owner, and `SchemaType` carries its provider; that owner is threaded through the same lookup seam used by CLI, LSP, and executor. This replaces the current broadcast loops (`carina-core/src/executor/provider_ready.rs:798-803`; `carina-cli/src/commands/mod.rs:255-262`; `carina-lsp/src/diagnostics/mod.rs:115-122`).

No admission-time call can prove that an arbitrary provider registered an identity: the trait returns `Ok(())` for unknown types and exposes no registry. Core construction therefore proves only callback implies identity. Provider coverage recursively walks every generated resource/config schema, follows definitions with cycle guards, collects callback-bearing identities, and asserts an exact registry entry; it does not use a hand-written identity list. A separate invariant keeps the generic `arn()` identity registered. The strict protocol decoder rejects non-null legacy opaque `String.validate`, `List.validate`, and `Custom.validate` payloads instead of silently ignoring them.

### Whole-schema validation witnesses

`ResourceSchema` remains a draft. Its existing runtime value-validation methods move to the witness so the draft can use the name `validate` for admission:

```rust
impl ResourceSchema {
    pub fn validate(self)
        -> Result<ValidatedResourceSchema, ResourceSchemaValidationError>;
}

impl ValidatedResourceSchema {
    pub fn validate(&self, attributes: &HashMap<String, Value>)
        -> Result<(), Vec<TypeError>>;
    pub fn as_schema(&self) -> &ResourceSchema;
    pub fn parts(&self) -> ResourceSchemaParts<'_>;
    pub fn effective_attributes(&self) -> impl Iterator<Item = EffectiveAttribute<'_>>;
    pub fn effective_attribute(&self, name: &str) -> Option<EffectiveAttribute<'_>>;
    pub fn effective_attribute_by_authored_name(&self, name: &str)
        -> Option<EffectiveAttribute<'_>>;
    pub fn effective_node_at_path(&self, path: &[String])
        -> Option<EffectiveSchemaNode<'_>>;
    pub fn effective_node_at_authored_path(&self, path: &[String])
        -> Option<EffectiveSchemaNode<'_>>;
}
```

`ValidatedResourceSchema` has a private field and no unchecked constructor, `From<ResourceSchema>`, mutable accessor, `Deref`, or owned inner extraction. The permanent `as_schema`/`parts` view supports mode-independent read-only algorithms and exhaustive conversion seams; mode-sensitive algorithms use the effective traversal. It cannot bypass admission: an admission point requires the witness, while cloning the borrowed `ResourceSchema` yields a draft that must be validated again. Cloning the witness itself, if exposed, preserves the witness.

Validation runs after all definitions and attributes are present. It **must** reject:

- `write_only` below a provider-populated ancestor, and `deferred_populate` on a field whose effective mode is user-input on any reachable path; declared user-input mode and authoring metadata below a provider-populated ancestor are inert rather than errors;
- every dangling `Ref`, including references reached from `defs`; a missing target is an error, not the current no-write-only result (`carina-core/src/schema/mod.rs:6867-6883`);
- an `exclusive_required` name that is unknown or provider-populated;
- `UniqueNameSpec::Attribute` naming a missing top-level attribute;
- duplicate top-level attribute names and duplicate struct-field names at any nested or referenced type;
- every default containing a non-wire/deferred value or a non-finite float, recursively through lists and maps;
- every malformed local mode/modifier combination received from protocol decode, evaluated with the same path-relative effective-mode rules.

Draft insertion records duplicate attribute names instead of silently overwriting them as `try_attribute` does today (`carina-core/src/schema/mod.rs:7380-7393`); validation reports the duplicate before producing the canonical attribute map. Struct fields remain a vector until validation, so duplicate field names are likewise observable. Errors include resource type, top-level attribute, and nested field/ref path.

Protocol decode preserves declared modes and modifiers, then submits the draft to this validator. No decoder or authoring path mutates a definition based on one of its callers; the validation witness is the only proof that every reachable path is coherent.

`ValidatedConfigSchema` wraps the provider-config `HashMap<String, AttributeType>` and exposes only read-only parts plus `effective_type()` / `effective_field_at_path()` traversal. It uses the same recursive type-graph, mode, and duplicate-field validator. Config JSON carries no `defs`, so any `Ref` in a config type is dangling and rejected. SDK serialization and `json_to_attribute_types` both require/return this witness rather than a raw map (`carina-plugin-host/src/wasm_convert.rs:1912-1923`).

Admission boundaries are:

- `SchemaRegistry` stores `ValidatedResourceSchema`; its single `get`/`get_for`/`get_for_data_source` getter set returns `&ValidatedResourceSchema`, and read-only consumers explicitly call `as_schema()`.
- `ProviderFactory::schemas` and `CarinaProvider::schemas` return resource witnesses.
- core-to-protocol schema conversion accepts a resource witness.
- protocol-to-core resource and config conversion returns a witness only after decode and validation.

### Declared resource validators

`ResourceValidation` becomes only `None | Declared(Vec<ValidatorType>)`. Delete its `InProcess` arm, public `ResourceValidator` authoring path, and `with_validator`; keep `with_declared_validators` as the sole attachment API. Executable functions reconstructed from `ValidatorType` are private implementation details.

Only the existing `ValidatorType::TagsKeyValueCheck` variant is needed (`carina-provider-protocol/src/types.rs:987-997`). The production inventory found no other resource-level check: AWS's 21 generated/emitter sites and AWSCC's 34 generated/emitter sites all implement the same tags map rule. Provider codegen emits:

```rust
.with_declared_validators(vec![ValidatorType::TagsKeyValueCheck])
```

directly on the core draft before `validate()`. AWS/AWSCC `main.rs` no longer appends validators after core-to-protocol conversion (`carina-provider-aws/src/main.rs:336-347`; `carina-provider-awscc/src/main.rs:487-498`). Core, CLI, and LSP tests use `TagsKeyValueCheck` or `exclusive_required`; a test-only enum variant is forbidden. Compile-fail coverage proves `with_validator` is unavailable.

Comparing the named fields of `ResourceSchemaParts`, `AttributeSchemaParts`, and `StructFieldParts` with their wire records leaves exactly two schema-record omissions: attribute completions and resource wait defaults (`carina-core/src/schema/mod.rs:609-618,6407-6420,6780-6792`; `carina-provider-aws/src/convert.rs:565-671`). Native resource validators no longer exist, and attribute callbacks remain enforceable through their identity rather than being schema-record omissions.

### One shared conversion module

Add `carina_core::protocol_convert`. It owns typed conversion between core and `carina_provider_protocol`; JSON serialization stays at the SDK edge and JSON parse/context wrapping stays at the host edge. Generated WIT types remain host glue.

The module owns both directions for validated resource/config schemas, attribute types, fields, modes, unique names, declared validators, ordinary and persisted values/maps, resource IDs/resources/data sources, directives, state/outcomes, and patches. Every protocol record and core `parts()`/`RawShape` view is exhaustively destructured; enum matches have no wildcard and record patterns have no `_` or `..`. Named omissions such as `_completions` and wait defaults make additions compile-time failures.

For `RawShape::String`, `Int`, `Float`, and `Enum`, conversion binds `validate` by name. `Some(callback)` is legal only with an identity; core-to-protocol serializes that identity, names the bound callback `_validate_via_identity`, and deliberately emits no function payload because the host must call the identity bridge. `None` is an ordinary callback-free refinement. `List` has no core callback after this change. In the reverse direction, every legacy protocol `validate` field is also named explicitly; non-null opaque payloads fail decode, while a core-built typed provider always emits `None`.

Ordinary core-to-protocol values use `try_core_to_protocol_value`. It rejects refs, interpolation, functions, access, unknowns, secrets, redacted persisted secrets, and any future non-wire variant with `ProtocolConvertError`; persisted conversion separately supports persisted secrets. Generic protocol-to-core ordinary conversion remains exhaustive and preserves the in-memory `StringList` variant.

Schema defaults use a separate `protocol_schema_default_to_core` decoder matching the actual JSON boundary. It recursively maps both protocol `StringList` and `List` to core `List`, rejects a non-finite in-memory protocol float, and otherwise decodes the JSON value domain. This choice matches host behavior instead of the providers' direct `proto_to_core_value`. A snapshot-wide search found 12 production defaults, all AWSCC bool/int/string scalars; no production schema requires a `StringList` default. `StringList -> List` is therefore the documented portable projection, not an accidental equality exception.

The resource-schema property test runs the real path:

```text
validated SDK core schema
  -> protocol conversion
  -> serde_json serialization
  -> host JSON parse + shared decode + validation
  -> portable core projection
```

It does not substitute a typed protocol-to-core call. Validation rejects nested NaN and positive/negative infinity before SDK serialization, so serde cannot turn them into `null`. Explicit projection tests cover completions and wait defaults.

### SDK trait split

```rust
trait CarinaProvider {
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema>;

    fn provider_config_attribute_types(
        &self,
    ) -> carina_core::schema::ValidatedConfigSchema;

    // info and all value/resource/state/operation methods stay protocol-typed.
}
```

SDK gains a normal core dependency. Guest glue converts witnesses through `protocol_convert`, then serializes the same protocol records. Providers author config types with core `AttributeType`/typed `StructField` builders and validate the map once; the AWS/AWSCC hand-built protocol `assume_role` structs migrate here.

Keeping operations protocol-typed makes their accepted value domain exactly the wire domain. Core-typed provider internals cross through explicit fallible conversion. Provider info, requests/outcomes, state, persisted state, and patch signatures do not change.

### Host trust boundary

The host continues to deserialize and validate JSON because a component may implement WIT without SDK. `json_to_schemas` returns resource witnesses and `json_to_attribute_types` returns `ValidatedConfigSchema`; both delegate typed conversion and graph validation to core.

The shared decoder rejects local contradictory modes, key/name mismatches, unsupported legacy bases, opaque validator payloads, dangling refs, duplicates, invalid exclusive/unique names, non-portable defaults, and write-only graph conflicts. It preserves every declared descendant mode and modifier. The shared core validator then applies path-relative effective modes, rejecting `write_only` beneath provider output and `deferred_populate` on any writable path. It preserves every refined identity, reconstructs core built-ins locally, and leaves provider callbacks absent so `walk_custom_lookup` invokes only the owning factory bridge. `WasmProviderFactory` retains witnesses rather than unwrapping them.

### AWS and AWSCC migration

Each provider PR must:

- update codegen to choose `optional`, `required`, or `provider_populated` from each field's declaration, preserve path-dependent modifiers, emit only that builder's methods, and call `build()`;
- emit `with_declared_validators` in generated schemas and delete tag callback imports/closures and post-conversion appends;
- migrate callback-bearing refined types to mandatory identities, exact-identity bridge registries, and callback-free list refinements;
- regenerate schemas/docs and migrate handwritten builders, including the four `assume_role` config fields;
- return validated resource and config witnesses;
- replace resource/value/state/patch adapters with core's module;
- delete `src/convert.rs` after equivalent core tests exist;
- retain cloud/model conversion that is not a core/protocol seam.

AWS has no scalar/list callback constructor to migrate. Its Phase 2 PR moves the whole constructor-keyed validator registry into `carina-aws-types`, including the generic `Arn` entry, and both providers consume that authority. AWSCC re-keys its entire map on full `TypeIdentity`, overlays predicates generated from the service-specific ARN and integer-enum source tables, and gives the integer enums identities `aws.ec2.FlowLog.MaxAggregationInterval` and `aws.logs.LogGroup.RetentionInDays`. It removes all 112 redundant list callback functions/calls before regeneration. AWSCC's unique patch helpers move to core; AWS inline equivalents use them, and AWSCC's partial reverse schema helpers disappear in favor of the host-derived decoder.

### Third-party provider and #4018

The GitHub provider is still a scaffold with no carina dependency (`carina-provider-github/Cargo.toml:21-30`; `carina-provider-github/src/main.rs:1-8`). Its first resource should pin core/SDK/protocol together, return validated resource/config witnesses, keep operation values protocol-typed, and never add local transport conversion.

The #4018 provider guide documents the complete method matrix, final validation errors, declared resource validators, identity-bound refined validators and their bridge registry, core schema versus protocol operation values, refs/defs, and coordinated revision pins. It must not teach transition methods, native resource callbacks, anonymous type callbacks, protocol booleans, or handwritten schema JSON.

## Protocol and compatibility

The wire format is unchanged. Protocol schema records retain their fields and serde names; WIT metadata exports remain JSON strings; modes retain the same `(required, read_only)` pairs; defaults, defs, validator declarations, operation config, and unique names retain their representations. String/int/float identities and enum name/namespace already cross version 12, and `validate-custom-type` already carries `(TypeIdentity, string)`, so numeric canonicalization needs no WIT change. List callbacks disappear because their production behavior is already represented by `length`.

`MIN_SUPPORTED_PROTOCOL_VERSION` and `PROTOCOL_VERSION` both remain 12 (`carina-provider-protocol/src/lib.rs:6-11`). State version also remains unchanged. The SDK Rust trait change is source-breaking and requires rebuilds, but is not a wire break.

The portable default projection is explicit: finite bool/int/float/string/map/list values survive, and `StringList` normalizes to `List` because JSON arrays have no variant tag. A typed SDK cannot emit non-finite schema defaults. A raw non-SDK guest remains subject to protocol JSON parsing and strict host schema validation.

An old version-12 provider remains loadable if its schema satisfies the path-relative contract. In particular, the currently released AWS and AWSCC providers remain loadable by the Phase 1b host. Optional/required declarations beneath provider-populated ancestors remain intact but are effectively provider-populated on those paths. Real contradictions such as `write_only` beneath that ancestor still fail, as does `deferred_populate` when the same field is also reachable through a writable path. A host fixture loads the current ACM JSON shape with optional `resource_record.deferred_populate` beneath read-only `domain_validation_options`; the existing AWS converter test at `carina-provider-aws/src/convert.rs:947-991` uses a different synthetic shape and is not sufficient.

## Alternatives rejected

| Alternative | Why rejected |
|---|---|
| Conversion in protocol | Dependency cycle |
| Conversion in SDK | Host cannot share it |
| Conversion in host | Providers still need core-to-protocol copies |
| Wire enum/version bump | Unnecessary break; does not prove whole-schema validity |
| SDK validates raw schemas internally | Signature does not prove admission; registries remain open to drafts |
| Deprecated transition or callback shims | Preserve runtime-invalid or non-portable paths |
| Host-only validation | Bugs cross WASM first; native registries remain unchecked |
| Entire SDK core-typed | Core `Value` contains non-wire variants |
| Public schema fields | Violates builder-only schema rules |
| `_`/`..` at the seam | New fields can be silently dropped |
| Incremental graph validation | Refs/cycles make validity construction-order-dependent |

## Rollout

Merge order: carina, AWS, AWSCC, then provider-author documentation.

### Phase 1a: carina schema typestate PR

1. Add mode-specific attribute/field builders, remove `with_default` from required builders, enforce provider-populated ancestry at final validation, and add compile-fail coverage.
2. Add `RefinementBinding`, mandatory-identity callback constructors, callback-free lists, and the single core built-in validator registry; migrate all 127 affected in-repo constructor calls.
3. Delete `ResourceValidation::InProcess`/`with_validator`; migrate all in-repo tests to real declarations.
4. Add resource/config validation witnesses and the shared graph validator.
5. Make registry/factory APIs carry resource witnesses.
6. Migrate all in-repo transition calls, make host construction preserve declared descendant modes without ancestry tracking or definition rewriting, and remove transition methods.

Phase 1a **does touch host decode source** because it currently calls `.required()`/`.read_only()` for attributes and fields. It changes those construction calls without changing protocol records, JSON, WIT, or transport behavior.

The effective-mode amendment is also Phase 1a: remove public declared-mode accessors from `StructField`, name exhaustive seam fields `declared_input_mode`, add ancestry-carrying traversal to both validation witnesses, and migrate every in-repo mode-sensitive consumer. This prevents later work (including #3772, differ, LSP, docs, and state) from accidentally treating a declaration as a path-independent mode.

Explicit test migrations are part of this phase. The invalid-order tests at `carina-core/src/schema/tests.rs:87-105,121-139` are `#[should_panic]` tests and become compile-fail cases. The required-plus-default test at `carina-core/src/schema/tests.rs:4345-4356` becomes optional-with-default. The provider-populated/create-only/removability and read-only/block-name cases at `carina-core/src/schema/tests.rs:4360-4410,4416-4440,7554-7580` are ordinary tests, not panic tests; they move to allowed-builder or malformed-wire coverage. Deferred-populate fixtures at `carina-core/src/validation/deferred_populate.rs:380-390,547-559` use provider-populated builders.

### Phase 1b: carina conversion seam + SDK PR

1. Add both conversion directions, explicit validator binding, schema-default projection, and typed errors in core.
2. Port host typed resource/config decode, path-preserving field construction, built-in reconstruction, shared string/int/float/enum canonicalization, and owning-factory routing; keep JSON/WIT glue.
3. Split SDK schema signatures and add the core dependency.
4. Add exact version-12 JSON fixtures, including the current ACM shape and released AWS/AWSCC metadata, plus SDK-to-JSON-to-host properties.
5. Merge and select the exact provider pin.

### Coordinator gate

Before either provider rev bump, obtain approval for the exact carina revision and merge order. AWS/AWSCC currently pin core, SDK, and protocol together; do not begin independent bumps.

### Phase 2: AWS counterpart PR

1. Bump the approved carina revision.
2. Migrate four input-mode emitter branches, including preserving all 28 inventoried ACM descendant declarations, the tag-validator emitter, 42 generated schemas/docs, handwritten builders, and the four config struct fields.
3. Migrate `carina-aws-types` in this PR, move the full constructor-keyed validator registry into it, and add generated-schema registry coverage.
4. Return validated resource/config witnesses with no validator append in `main.rs`.
5. Replace adapters and delete the 993-line `convert.rs`.
6. Verify native tests, deterministic codegen, and release WASM build.
7. Merge before AWSCC because AWSCC pins `carina-aws-types` from AWS.

### Phase 3: AWSCC counterpart PR

1. Obtain approval, bump carina, and **mandatorily** bump AWS to the Phase 2 revision.
2. Migrate seven input-mode emitter branches, including preserving the seven inventoried descendant declarations, the tag-validator emitter, 49 generated schemas/docs, handwritten builders, and the four config struct fields.
3. Re-key the whole validator surface on exact `TypeIdentity`: reuse the common registry, generate the service-specific ARN/integer overlay from source tables, remove the list-validator emitter and 112 generated callbacks, then regenerate schemas/docs.
4. Return witnesses and use owning-factory lookup.
5. Replace shared operation/state/patch adapters and delete the 1,361-line `convert.rs`.
6. Verify native tests, deterministic codegen, WASM host validation, and release WASM build.

The AWS bump is not conditional. AWSCC's direct core/SDK/protocol pins and `carina-aws-types` must resolve the coordinated carina revision (`carina-provider-awscc/Cargo.toml:26-29`; `carina-aws-types/Cargo.toml:17`), and the Phase 2 aws-types migration removes transition calls still present in shared constructors such as `services/sqs.rs:14-22` and `services/s3/notification.rs:5-13`.

### Phase 4: docs and third-party proof

1. Land #4018 with compile-checked snippets.
2. Add an SDK fixture covering nested/top-level modes, declared validators, an identity-bridged refined validator, defaults, and config structs without protocol schema imports.
3. Apply the shape when GitHub's first resource lands; do not expand #4026 into that implementation.

## Test plan

### Core and conversion

- Compile-fail: transition methods and `with_validator` do not exist. Required attributes lack `with_default`; provider-populated attributes lack default/write-only/create-only/identity/removability/block/completion methods; user-input attributes lack deferred-populate. Struct-field builders lack every attribute-only method, and provider-populated fields lack write-only. Path-dependent field modifiers remain builder-accessible and are decided by final validation.
- Compile-fail: `refined_string_with_validator`, `refined_int_with_validator`, and `refined_float_with_validator` cannot be called without a `TypeIdentity`; `refined_list` has no callback argument.
- Compile-fail: raw resource/config drafts cannot enter registry, factory, converter, or serializer APIs.
- Compile-fail: `StructField` declared-mode and mode-adjacent readers are not public; nested modes are obtained from an effective view.
- Unit: every listed attribute/field builder method survives `build()`; methods absent from each row remain unavailable.
- Unit: final resource validation rejects dangling refs, unknown/provider-populated exclusive-required names, missing unique-name attributes, duplicate attributes, duplicate nested/definition struct fields, `write_only` below provider output, `deferred_populate` on any writable path, all write-only graph conflicts, and malformed local modifier placement with stable paths. A shared definition reached through writable and provider-populated roots remains writable on the first path and output-only on the second.
- Unit: final config validation applies the same ref/field/mode checks with an empty defs map.
- Unit: a required field in one shared definition reports effective `Required` through a writable root and effective `ProviderPopulated` through a provider-populated root; config traversal carries the same ancestry.
- Unit: protocol decode preserves shared-definition field modes and modifiers, the exact ACM optional/deferred shape remains valid beneath its read-only root, and core validation rejects descendant `write_only` or a deferred field also reachable through a writable root.
- Unit: defaults reject every deferred/non-wire variant and top-level or nested NaN/positive-infinity/negative-infinity.
- Unit: declared tags validation executes after direct core construction and after JSON host decode; no test-only `ValidatorType` exists.
- Unit: every core built-in callback, including numeric `PositiveInt` and `Email`, is reconstructed by identity after protocol JSON round trip; `PositiveInt` accepts `1` and rejects `0` through resource validation and `Simple`/`SchemaType` typechecking.
- Unit: the one canonicalizer is exercised through parser/provider-context validation, typechecking, and executor lookup; integer text uses canonical base-10, finite float text round-trips, and string/enum behavior is unchanged. Non-null opaque protocol `validate` payloads fail decode.
- Unit: native callback and decoded bridge paths each increment the owning factory counter once; non-owning factories remain at zero.
- Property: generated portable witnesses run `SDK core -> protocol -> serde_json -> host decode -> portable projection`; it explicitly covers `StringList -> List` and never substitutes typed-only conversion.
- Unit: generic ordinary decode preserves `StringList`; schema-default decode normalizes it; non-wire ordinary values return `ProtocolConvertError`; persisted secrets use only persisted conversion.
- Compile forcing: any added core parts/protocol record/validator variant, including a refined `validate` field, breaks conversion until handled; no seam match uses `_` or `..`.

### Host and SDK

- Typed guest resource/config witnesses serialize and load into validated host registries.
- `json_to_attribute_types` rejects dangling refs, duplicate struct fields, and malformed nested modes with attribute paths.
- A non-SDK guest with local contradictory flags or modifiers on the wrong effective path fails component load; legacy descendant declarations remain unchanged.
- Key/name, legacy-base, validator, graph, and default errors retain resource/attribute/path context.
- Current released AWS/AWSCC version-12 fixtures load, including the exact shipped ACM ancestor/optional/deferred shape; SDK operations retain protocol signatures.
- LSP completion omits fields reached through a provider-populated path, while diagnostics accept the shared definition through its writable root and reject assignment through its read-only root.
- CLI `validate` and LSP diagnostics both accept FlowLog `max_aggregation_interval = 60`, reject `61`, and retain the `PositiveInt` numeric regression.

Run crate-scoped tests first: `cargo test -p carina-core`, `-p carina-plugin-host`, `-p carina-plugin-sdk`, `-p carina-provider-mock`, and relevant `carina-lsp`/CLI tests. Because the API crosses crates, finish with timeout-bounded `cargo test --workspace` and the existing WASM fixture build.

### Providers

- Golden tests prove each source mode emits exactly one constructor and only its allowed methods; a second codegen run has no diff.
- Registry coverage walks every generated schema and proves every callback-bearing identity has an exact entry, with no hand-written identity list. The common registry separately asserts that the identity from `arn()` remains registered, covering all generic ARN uses; Region and Availability Zone bridge tests cover their generated sites.
- AWSCC builds the component, loads it through `WasmProviderFactory`, accepts `ec2.FlowLog.max_aggregation_interval = 60`, and rejects `61` before an operation call. Companion assertions prove the guest received canonical text `"60"`/`"61"` and parsed each as `i64`.
- Search finds no resource `.with_validator`, `validate_tags_map` import/closure, post-conversion validator append, identityless refined callback, generated list callback/function, raw protocol config `StructField`, or local core/protocol converter.
- Generated schema counts/fingerprints stay stable except intentional API syntax, refined identities/list-callback removal, and declared-validator representation.
- Existing schema/operation suites pass through shared conversion.
- AWS/AWSCC release-build for `wasm32-wasip2` with core pulled through SDK.
- No cloud acceptance run is required; any such run still needs explicit authorization.

## Resolved questions

1. **Ordinary-value conversion is fallible.** Existing providers stringify or panic on core-only variants; the shared converter rejects every non-wire ordinary value. Correct providers are unchanged, and future core variants become compile errors at the exhaustive match.
2. **Resource validation is portable or unrepresentable.** All production native resource callbacks in AWS/AWSCC implement the same tags check. They become `TagsKeyValueCheck`; `InProcess`, `with_validator`, the silent-drop arms, and post-conversion appends are deleted.
3. **Refined callbacks are identity-bound and numeric.** String/int/float callbacks require `TypeIdentity`; enums already did; list callbacks are removed because every production case duplicates `length`. One scalar canonicalizer serves parser validation, typechecking, and plan/apply, encoding ints as base-10 text and routing provider identities only to the owning factory.
4. **Round-trip equality follows the JSON seam.** Schema defaults use the host-compatible decoder, non-finite floats fail admission, and `StringList` normalizes to `List`. Completions and wait defaults are the only schema-record field omissions; executable validation is either declared or recovered by identity.
5. **Builder placement follows ownership and effective ancestry.** Required-plus-default is optional behavior, so only optional attributes expose defaults. Path-dependent field modifiers are retained in drafts, while `validate()` proves deferred-populate is output-only and rejects `write_only` below provider output. Definitions are never rewritten from one caller's ancestry.
6. **Provider config is admitted too.** `ValidatedConfigSchema` uses the same recursive field/ref checks, and both SDK export and host JSON decode carry the witness.
7. **AWS validation has one registry authority.** `carina-aws-types` owns the constructor-keyed common registry; AWSCC reuses it and adds generated service-specific predicates. Generated-schema walking proves callback coverage without a hand-written list.
8. **GitHub is not a build proof.** It has no carina dependency; the SDK fixture proves third-party ergonomics until its first resource lands.

**Summary (four lines):**

<!-- derived-from #declared-resource-validators -->
<!-- derived-from #identity-bound-attribute-type-validators -->
<!-- derived-from #one-shared-conversion-module -->
<!-- derived-from #rollout -->
<!-- derived-from #test-plan -->
- Resource callbacks become declared validators; refined scalar/enum callbacks become identity-bound and list callbacks become declarative constraints.
- Provider-output ancestry is derived per reference path; version-12 decode preserves shared definitions while final validation rejects path-specific `write_only` and deferred conflicts.
- One numeric-capable canonicalizer and one constructor-keyed AWS registry feed only the owning provider factory; AWSCC adds generated service-specific entries.
- Verification: cited source anchors and rollout inventories were checked by read/grep; bridge execution and the `60`/`61` outcomes remain required tests and were not run here.
