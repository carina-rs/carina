---
title: "Schemas"
description: "How a Carina provider declares its resource types: the schema builders, input modes, write-only values, validators and type identities, declared validation, the validated schema witness, and effective modes below provider-populated values."
---

A provider describes every resource type it manages with a schema: the attributes, their types, which ones the configuration sets and which ones the provider fills in, and the rules a value must follow. The host uses the schema to check a configuration before anything reaches the provider, to compute plans, to drive the editor's completion and diagnostics, and to store state.

This page shows how to write schemas with the builder API in `carina-plugin-sdk`. Every Rust example on it is compiled and run as a doctest of `carina-plugin-sdk`, so the examples always match the SDK on the carina `main` branch. Earlier pages pin an older revision; the builder API on this page replaced the struct literals that [Your First Provider](/providers/writing/getting-started/) uses.

The schema types live in `carina_core::schema`. The SDK re-exports `carina_core` as `carina_plugin_sdk::core`, so a provider depends only on `carina-plugin-sdk`.

## A first schema

A schema starts with `ResourceSchema::new` and the resource type name, without the provider prefix: a provider named `widget` that declares `widget` gets `widget.widget` in the DSL. Each attribute is added with `.attribute(...)`, and the finished draft goes through `.validate()`:

```rust
use carina_plugin_sdk::core::resource::{ConcreteValue, Value};
use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedResourceSchema,
};

fn widget_schema() -> ValidatedResourceSchema {
    ResourceSchema::new("widget")
        .with_description("A widget")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .identity()
                .with_description("The widget's name. Changing it replaces the widget.")
                .build(),
        )
        .attribute(
            AttributeSchema::optional("size", AttributeType::int())
                .with_default(Value::Concrete(ConcreteValue::Int(1)))
                .build(),
        )
        .attribute(
            AttributeSchema::provider_populated("arn", AttributeType::string())
                .with_description("Assigned by the API when the widget is created.")
                .build(),
        )
        .validate()
        .expect("the widget schema is valid")
}

let schema = widget_schema();
assert_eq!(schema.as_schema().resource_type(), "widget");
assert!(schema.effective_attribute("name").unwrap().is_required());
assert!(schema.effective_attribute("arn").unwrap().is_read_only());
```

The provider returns these from `CarinaProvider::schemas`, whose return type is `Vec<ValidatedResourceSchema>`:

```rust ignore
fn schemas(&self) -> Vec<ValidatedResourceSchema> {
    vec![widget_schema()]
}
```

The provider also returns one provider-wide `ReferenceForms` table. Most
providers return an empty table. A provider uses it when one refined String
identity accepts a short name but an API read may return the corresponding
long reference:

```rust
use carina_plugin_sdk::core::schema::{
    ReferenceForm, ReferenceForms, ReferenceTemplate, TypeIdentity,
};

let identity = TypeIdentity::new(Some("widget"), ["widget"], "Name");
let forms = ReferenceForms::from([(
    identity.clone(),
    ReferenceForm::Template {
        long: ReferenceTemplate::new(
            "arn:{partition}:widget:{region}:{account}:widget/{name}",
        )
        .expect("the reference template is valid"),
    },
)]);

assert!(forms.contains_key(&identity));
```

The `{name}` placeholder must occur exactly once. The other supported
placeholders are `{partition}`, `{region}`, and `{account}`; an unknown
placeholder or stray brace is rejected when `ReferenceTemplate` is built or
deserialized. The table key must be the same `TypeIdentity` used by a refined
String in this provider's resource or config schema. The SDK validates
resources, config, and forms together, so an undeclared, Int, or Enum identity
cannot be emitted by `schemas()` JSON. Providers return the table from the
required `CarinaProvider::reference_forms` method rather than constructing
that JSON themselves.

What the attributes declare:

- `create_only` means the API cannot change the value in place, so a change replaces the resource.
- `identity` adds the value to the address of an anonymous resource (one written without `let`), so two anonymous widgets with different names get different addresses.
- `with_default` gives the value the host assumes when the configuration leaves the attribute out.
- `with_description` shows up in the editor and in the generated docs.

## Input modes

Every attribute and every struct field has one input mode, and the constructor picks it:

| Constructor | Mode | The configuration |
| --- | --- | --- |
| `AttributeSchema::optional` | `InputMode::Optional` | may set the value |
| `AttributeSchema::required` | `InputMode::Required` | must set the value |
| `AttributeSchema::provider_populated` | `InputMode::ProviderPopulated` | cannot set the value; the provider returns it |

The mode is chosen once, when the builder is created, and each constructor returns a different builder type that has only the methods that make sense for its mode. A provider-populated builder has no `create_only`, `identity`, `write_only` or `with_default`, because none of them mean anything for a value the user never writes. Only the optional builder has `with_default`, `removable` and `non_removable`. A combination such as "required and provider-populated" therefore does not compile, instead of failing at load time:

```rust compile_fail
use carina_plugin_sdk::core::schema::{AttributeSchema, AttributeType};

// A provider-populated attribute cannot be write-only.
let _ = AttributeSchema::provider_populated("token", AttributeType::string()).write_only();
```

Provider-populated attributes can still be marked `deferred_populate` when the API fills them in some time after create returns, such as a certificate's status. A downstream resource that reads such a value must then wait for it with a [wait expression](/reference/dsl/expressions/#wait-expression).

## Attribute types

`AttributeType` has a constructor for each shape:

- Scalars: `string()`, `int()`, `float()`, `bool()`, `duration()`.
- Refined scalars carry extra rules: `refined_string(identity, pattern, length, to_dsl)`, `refined_int(identity, range)` and `refined_float(identity, range)`, plus the `_with_validator` forms described in [Validators and type identities](#validators-and-type-identities).
- Collections: `list(inner)`, `unordered_list(inner)` for lists whose order the API does not keep, `map(value)` and `map_with_key(key, value)`.
- Enums: `enum_(identity, values, dsl_aliases, validate, to_dsl)`.
- Structs: `struct_(name, fields)`, with fields built from `StructField`.
- Unions: `union(members)`, for a value that may take one of several shapes.
- References: `ref_(name)` points at a definition registered with `ResourceSchema::with_def`, which is how recursive shapes are written.

Struct fields use the same three constructors as attributes, `StructField::optional`, `StructField::required` and `StructField::provider_populated`:

```rust
use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
};

let tier = AttributeType::enum_(
    TypeIdentity::new(Some("widget"), ["widget"], "Tier"),
    Some(vec!["standard".to_string(), "premium".to_string()]),
    vec![],
    None,
    None,
);

let schema = ResourceSchema::new("widget")
    .attribute(AttributeSchema::required("name", AttributeType::string()).build())
    .attribute(
        AttributeSchema::optional(
            "settings",
            AttributeType::struct_(
                "Settings",
                vec![
                    StructField::required("tier", tier).build(),
                    StructField::optional("labels", AttributeType::map(AttributeType::string()))
                        .build(),
                ],
            ),
        )
        .build(),
    )
    .validate()
    .expect("the schema is valid");

let settings = schema.effective_attribute("settings").unwrap();
let tier = settings
    .field_candidates()
    .into_iter()
    .find(|field| field.name() == "tier")
    .unwrap();
assert!(tier.is_required());
```

Enum values in the DSL follow the snake_case convention. When the API spells a value differently, map it with `dsl_aliases` (pairs of API value and DSL spelling) or with a `DslTransform` in `to_dsl`, such as `DslTransform::HyphenToUnderscore` for values like `us-east-1`.

## Write-only values

Some APIs accept a value on create or update but never return it on read, such as a password or a client secret. Mark these with `write_only`, on a top-level attribute or on a nested user-input struct field:

```rust
use carina_plugin_sdk::core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

let schema = ResourceSchema::new("widget")
    .attribute(AttributeSchema::required("name", AttributeType::string()).build())
    .attribute(
        AttributeSchema::optional("api_key", AttributeType::string())
            .write_only()
            .build(),
    )
    .attribute(
        AttributeSchema::optional(
            "webhook",
            AttributeType::struct_(
                "Webhook",
                vec![
                    StructField::required("url", AttributeType::string()).build(),
                    StructField::optional("secret", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    )
    .validate()
    .expect("the schema is valid");

assert!(schema.effective_attribute("api_key").unwrap().is_write_only());
// `webhook` is not write-only itself, but it carries a write-only field.
assert!(schema.as_schema().write_only_bearing("webhook"));
```

Write-only describes what the read API returns, not whether a value is secret. The host compares a write-only value against the value it recorded in state when the provider accepted it, because the read result never contains it. An unchanged write-only value therefore never causes an update on its own, and a changed one does. When an update happens for any reason, the host also re-sends every write-only value of the resource as a `Reassert` patch operation, because many APIs drop values a request leaves out. See [Implementing Operations](/providers/writing/implementing-operations/) for how a provider handles `Reassert`.

`validate()` rejects write-only on any path below a provider-populated value and inside a union, where the host could not track which member the value belongs to.

## Validators and type identities

A refined type can carry a validator: a function the host runs on each value before planning. A validator always comes with a `TypeIdentity`, the name of the type the validator defines, such as `widget.widget.Color`. There is no constructor for a validator without one:

```rust
use carina_plugin_sdk::core::resource::{ConcreteValue, Value};
use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeError, TypeIdentity, validator,
};

fn color() -> AttributeType {
    AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("widget"), ["widget"], "Color"),
        Some("^#[0-9a-f]{6}$".to_string()),
        None,
        validator(|value| match value {
            Value::Concrete(ConcreteValue::String(color))
                if color.expose_plaintext() != "#000000" =>
            {
                Ok(())
            }
            _ => Err(TypeError::ValidationFailed {
                message: "color must not be black".to_string(),
            }),
        }),
        None,
    )
}

let schema = ResourceSchema::new("widget")
    .attribute(AttributeSchema::optional("color", color()).build())
    .validate()
    .expect("the schema is valid");

let color = schema.effective_attribute("color").unwrap();
let black = Value::Concrete(ConcreteValue::String("#000000".into()));
let red = Value::Concrete(ConcreteValue::String("#ff0000".into()));
assert!(color.validate(&black).is_err());
assert!(color.validate(&red).is_ok());
```

`validator` takes a closure that returns a `TypeError`. `legacy_validator` takes one that returns `Result<(), String>` and wraps the message for you.

The closure runs inside the provider's WebAssembly component. When the host validates a value, it sends the type identity and the value to the provider, which runs every validator it registered for that identity. The identity is therefore the type's name across the whole provider, not just one attribute: if two schemas declare the same identity with different validators, a value of that type must pass both. Give each distinct type its own identity, reuse one only for types that mean the same thing, and use the same scalar kind for every declaration of it (`validate()` rejects an identity used as both a string and an int). Even without a validator, an identity names the type in diagnostics and in the editor.

## Declared validation

Some rules involve more than one attribute. A schema declares them as data, because a closure cannot cross the boundary between the provider and the host:

```rust
use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatorType,
};

let schema = ResourceSchema::new("widget")
    .attribute(AttributeSchema::optional("source_url", AttributeType::string()).build())
    .attribute(AttributeSchema::optional("source_file", AttributeType::string()).build())
    .attribute(
        AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
    )
    // Exactly one of the two must be set.
    .exclusive_required(&["source_url", "source_file"])
    // Reject `tags = { key = ..., value = ... }`, a common mistake for a tag list.
    .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck])
    .validate()
    .expect("the schema is valid");

assert_eq!(
    schema.as_schema().exclusive_required_groups(),
    [vec!["source_url".to_string(), "source_file".to_string()]]
);
```

The host runs these checks on every configuration before planning; a configuration that sets both `source_url` and `source_file`, or neither, fails validation. `exclusive_required` can be called more than once; each group is checked on its own. `ValidatorType` lists the declared checks the host knows how to run; a check it does not list cannot be expressed in a schema.

## The validated schema

`ResourceSchema` is a draft. `validate()` checks the whole draft and returns a `ValidatedResourceSchema`, the only type that `CarinaProvider::schemas` accepts. It reports every problem it finds at once, each with the schema path where it occurred:

```rust
use carina_plugin_sdk::core::schema::{AttributeSchema, AttributeType, ResourceSchema};

let error = ResourceSchema::new("widget")
    .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
    .exclusive_required(&["name", "missing"])
    .with_unique_name_attribute("also_missing")
    .validate()
    .unwrap_err();

let messages: Vec<&str> = error.issues().map(|(_path, message)| message).collect();
assert!(messages.iter().any(|message| message.contains("`missing`")));
assert!(messages.iter().any(|message| message.contains("`also_missing`")));
```

Among other things, `validate()` rejects duplicate attribute names, references to attributes that do not exist (in `exclusive_required` and in the unique-name attribute), `exclusive_required` groups that name a provider-populated attribute, defaults that cannot be sent over the wire, enum shapes the protocol cannot carry, and the write-only placements described above. Validating the schema while building it means a mistake shows up in the provider's own tests, not when a user loads the provider.

The provider's configuration (the attributes of the `provider` block) works the same way: `provider_config_attribute_types` returns a `ValidatedConfigSchema`, built with `ValidatedConfigSchema::validate` from a `BTreeMap` of attribute names to types.

Schemas cross the boundary as JSON. The host decodes them and validates them again when it loads the provider, so a schema that somehow bypassed `validate()` still fails at load, not halfway through an apply.

## Effective modes below provider-populated values

A nested field's declared mode is not always the mode it has. Below a provider-populated value, every field is provider-populated too, whatever its declaration says: the user cannot write any part of a value the provider owns. This matters when one struct type is shared between an input attribute and an output attribute:

```rust
use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, InputMode, ResourceSchema, StructField,
};

fn endpoint() -> AttributeType {
    AttributeType::struct_(
        "Endpoint",
        vec![
            StructField::required("host", AttributeType::string()).build(),
            StructField::optional("port", AttributeType::int()).build(),
        ],
    )
}

let schema = ResourceSchema::new("widget")
    .attribute(AttributeSchema::optional("listen", endpoint()).build())
    .attribute(AttributeSchema::provider_populated("published", endpoint()).build())
    .validate()
    .expect("the schema is valid");

let host_mode = |attribute: &str| {
    schema
        .effective_attribute(attribute)
        .unwrap()
        .field_candidates()
        .into_iter()
        .find(|field| field.name() == "host")
        .unwrap()
        .effective_input_mode()
};

assert_eq!(host_mode("listen"), InputMode::Required);
assert_eq!(host_mode("published"), InputMode::ProviderPopulated);
```

That is why a nested field's mode can only be read through an `EffectiveField`, which you get by walking down from a `ValidatedResourceSchema` (`effective_attribute`, then `field_candidates`). A `StructField` on its own does not know which path it was reached through, so it has no method that returns its mode. The same goes for a field's block name: an alias declared below a provider-populated value does nothing, and `EffectiveField::block_name` returns `None` there.

## Other schema settings

- `as_data_source()` declares a data source instead of a managed resource; the host reads it and never creates, updates or deletes it.
- The replacement settings tell the host what happens when a replacement creates the new resource before deleting the old one (`create_before_destroy`). `with_unique_name_attribute(name)` names the attribute the API requires to be unique, and the host gives the new resource a temporary value while both exist. `with_coexisting_replacement()` declares that the two can exist side by side without any change. The default, `with_conflicting_replacement()`, means they cannot, so `create_before_destroy` on that resource type is an error at plan time.
- `with_operation_config(OperationConfig { .. })` sets per-operation timeouts and retry counts for this resource type.
- `with_default_wait_timeout` and `with_default_wait_interval` set the defaults for a `wait` block on this resource type.
