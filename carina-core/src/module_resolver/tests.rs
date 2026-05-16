//! Tests for the `module_resolver` module.
//!
//! This file is referenced from `mod.rs` as `#[cfg(test)] mod tests;` and
//! depends on the `#[cfg(test)] use expander::{parse_synthetic_instance_prefix,
//! substitute_arguments};` re-export there to bring those `pub(super)`
//! helpers into the `super::*` glob below.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use indexmap::IndexMap;

use super::*;
use crate::parser::{ArgumentParameter, ModuleCall, ParsedFile, ProviderContext, TypeExpr};
use crate::resource::{
    ConcreteValue, DeferredValue, Directives, Resource, ResourceId, ResourceKind, Value,
};

fn create_test_module() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        resources: vec![Resource {
            id: ResourceId::new("security_group", "sg"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("sg".to_string())),
                );
                attrs.insert(
                    "vpc_id".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "vpc_id".to_string(),
                    }),
                );
                attrs.insert(
                    "_type".to_string(),
                    Value::Concrete(ConcreteValue::String("aws.security_group".to_string())),
                );
                attrs.into_iter().collect()
            },
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "vpc_id".to_string(),
                type_expr: TypeExpr::String,
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "enable_flag".to_string(),
                type_expr: TypeExpr::Bool,
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_substitute_arguments() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
    );

    // Argument params are lexically scoped: binding_name is the param name itself
    let value = Value::Deferred(DeferredValue::BindingRef {
        binding: "vpc_id".to_string(),
    });
    let result = substitute_arguments(&value, &inputs);

    assert_eq!(
        result,
        Value::Concrete(ConcreteValue::String("vpc-123".to_string()))
    );
}

#[test]
fn test_substitute_arguments_nested() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "port".to_string(),
        Value::Concrete(ConcreteValue::Int(8080)),
    );

    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Deferred(DeferredValue::BindingRef {
            binding: "port".to_string(),
        }),
        Value::Concrete(ConcreteValue::Int(443)),
    ]));
    let result = substitute_arguments(&value, &inputs);

    match result {
        Value::Concrete(ConcreteValue::List(items)) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], Value::Concrete(ConcreteValue::Int(8080)));
            assert_eq!(items[1], Value::Concrete(ConcreteValue::Int(443)));
        }
        _ => panic!("Expected list"),
    }
}

/// Module containing one anonymous resource (`id.name` is `Pending`,
/// no `let` binding). Used to verify that expansion preserves
/// `Pending` rather than collapsing it to `Bound("<instance>.")` —
/// a `Bound` value with a trailing dot would slip past
/// `compute_anonymous_identifiers`'s `is_pending` filter and never
/// receive its hash-derived suffix (#2516).
fn create_test_module_with_anonymous_resource() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        resources: vec![Resource {
            id: ResourceId::with_provider("awscc", "iam.RolePolicy", "", None),
            attributes: {
                let mut attrs = IndexMap::new();
                attrs.insert(
                    "policy_name".to_string(),
                    Value::Concrete(ConcreteValue::String("inline".to_string())),
                );
                attrs
            },
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_expand_anonymous_resource_in_named_module_keeps_name_pending() {
    use crate::resource::ResourceName;

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "policy_module".to_string(),
            create_test_module_with_anonymous_resource(),
        );
        r
    };
    let call = ModuleCall {
        module_name: "policy_module".to_string(),
        binding_name: Some("bootstrap".to_string()),
        arguments: HashMap::new(),
    };

    let expanded = resolver
        .expand_module_call(&call, "bootstrap", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);
    let policy = &expanded[0];
    assert!(
        matches!(policy.id.name, ResourceName::Pending),
        "anonymous resource inside a module instance must remain Pending after expansion \
         (compute_anonymous_identifiers filters on Pending and would skip a Bound value); \
         got {:?}",
        policy.id.name,
    );
    assert_eq!(
        policy.module_source,
        Some(crate::resource::ModuleSource::Module {
            name: "policy_module".to_string(),
            instance: "bootstrap".to_string(),
        }),
        "module_source must be set so compute_anonymous_identifiers can prepend \
         the instance prefix when the Pending name is bound"
    );
}

#[test]
fn test_expand_module_call() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".to_string())),
            );
            args
        },
    };

    let expanded = resolver
        .expand_module_call(&call, "my_instance", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let sg = &expanded[0];
    assert_eq!(sg.id.name_str(), "my_instance.sg");
    assert_eq!(
        sg.get_attr("vpc_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "vpc-456".to_string()
        )))
    );
    assert_eq!(
        sg.module_source,
        Some(crate::resource::ModuleSource::Module {
            name: "test_module".to_string(),
            instance: "my_instance".to_string(),
        })
    );
    // Module info should NOT be in attributes
    assert!(!sg.attributes.contains_key("_module"));
    assert!(!sg.attributes.contains_key("_module_instance"));
}

/// Module whose resource targets a named provider instance via
/// `directives.provider_instance`. Reproduces carina#3038: expansion
/// rebuilt `id` with `with_provider(...)`, silently dropping the
/// `provider_instance` field. `ProviderRouter` keys on
/// `(id.provider, id.provider_instance)`, so the lost binding made
/// `create` dispatch to the kind's default instance even though
/// state-writeback (which reads `directives.provider_instance` from
/// the `Resource`, not the `id`) still recorded the routing
/// correctly. Net effect for users: an ACM cert that should live in
/// `us-east-1` (where CloudFront viewer certs *must* live) lands in
/// the default region instead, and subsequent `read` against the
/// recorded `us` instance fails with `ResourceNotFoundException`.
fn create_module_with_named_provider_instance() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        resources: vec![Resource {
            id: ResourceId::with_provider("aws", "acm.Certificate", "cert", Some("us".to_string())),
            attributes: {
                let mut attrs = IndexMap::new();
                attrs.insert(
                    "domain_name".to_string(),
                    Value::Concrete(ConcreteValue::String("example.com".to_string())),
                );
                attrs
            },
            kind: ResourceKind::Managed,
            directives: Directives {
                provider_instance: Some("us".to_string()),
                ..Directives::default()
            },
            prefixes: HashMap::new(),
            binding: Some("cert".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_expand_module_call_preserves_provider_instance_on_id() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "registry".to_string(),
            create_module_with_named_provider_instance(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "registry".to_string(),
        binding_name: Some("acme".to_string()),
        arguments: HashMap::new(),
    };

    let expanded = resolver
        .expand_module_call(&call, "acme", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);
    let cert = &expanded[0];
    assert_eq!(
        cert.id.provider_instance.as_deref(),
        Some("us"),
        "module expansion must preserve `id.provider_instance` so \
         ProviderRouter dispatches `create` through the named \
         instance (carina#3038)"
    );
    // The directives copy stays correct (and is what state writeback
    // already reads from) — assert it too so a regression that flips
    // *both* sides at once is still caught.
    assert_eq!(
        cert.directives.provider_instance.as_deref(),
        Some("us"),
        "directives.provider_instance must survive module expansion"
    );
}

#[test]
fn test_reconcile_anonymous_module_instances_preserves_provider_instance() {
    // The SimHash-remap path in `reconcile_anonymous_module_instances`
    // was the second `with_provider(...)` site that dropped
    // `provider_instance` before carina#3038. A close-but-different
    // SimHash in state triggers a name rewrite, so this test must
    // assert that the named-instance routing survives the rewrite.
    use crate::resource::{ConcreteValue, Resource, ResourceId, ResourceKind, Value};

    let current_prefix = format!("thing_{:016x}", 0xABCDu64);
    let state_hash = 0xABCDu64 ^ 1; // flip one bit — within SimHash threshold
    let state_name = format!("thing_{:016x}.role", state_hash);

    let mut resources = vec![Resource {
        id: ResourceId::with_provider(
            "aws",
            "iam.Role",
            format!("{}.role", current_prefix),
            Some("us".to_string()),
        ),
        attributes: {
            let mut attrs = IndexMap::new();
            attrs.insert(
                "role_name".to_string(),
                Value::Concrete(ConcreteValue::String("r".to_string())),
            );
            attrs
        },
        kind: ResourceKind::Managed,
        directives: Directives {
            provider_instance: Some("us".to_string()),
            ..Directives::default()
        },
        prefixes: HashMap::new(),
        binding: Some(format!("{}.role", current_prefix)),
        dependency_bindings: BTreeSet::new(),
        module_source: Some(crate::resource::ModuleSource::Module {
            name: "thing".to_string(),
            instance: current_prefix.clone(),
        }),
        quoted_string_attrs: std::collections::HashSet::new(),
    }];

    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];
    reconcile_anonymous_module_instances(&mut resources, &state_lookup);

    assert_eq!(
        resources[0].id.name_str(),
        state_name,
        "precondition: remap must actually have rewritten the name",
    );
    assert_eq!(
        resources[0].id.provider_instance.as_deref(),
        Some("us"),
        "reconcile_anonymous_module_instances must preserve \
         id.provider_instance through the SimHash prefix rewrite \
         (carina#3038)"
    );
}

/// Module with two resources where one references the other via _binding / ResourceRef.
fn create_module_with_intra_refs() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        resources: vec![
            Resource {
                id: ResourceId::new("ec2.Vpc", "main_vpc"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert(
                        "cidr_block".to_string(),
                        Value::Deferred(DeferredValue::BindingRef {
                            binding: "cidr".to_string(),
                        }),
                    );
                    attrs.into_iter().collect()
                },
                kind: ResourceKind::Managed,
                directives: Directives::default(),
                prefixes: HashMap::new(),
                binding: Some("vpc".to_string()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                quoted_string_attrs: std::collections::HashSet::new(),
            },
            Resource {
                id: ResourceId::new("ec2.Subnet", "sub"),
                attributes: {
                    let mut attrs = HashMap::new();
                    attrs.insert(
                        "vpc_id".to_string(),
                        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
                    );
                    attrs.into_iter().collect()
                },
                kind: ResourceKind::Managed,
                directives: Directives::default(),
                prefixes: HashMap::new(),
                binding: Some("subnet".to_string()),
                dependency_bindings: BTreeSet::new(),
                module_source: None,
                quoted_string_attrs: std::collections::HashSet::new(),
            },
        ],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "cidr".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_multiple_module_instances_no_collision() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("net".to_string(), create_module_with_intra_refs());
        r
    };

    let call_a = ModuleCall {
        module_name: "net".to_string(),
        binding_name: Some("prod".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            );
            args
        },
    };
    let call_b = ModuleCall {
        module_name: "net".to_string(),
        binding_name: Some("staging".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr".to_string(),
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
            );
            args
        },
    };

    let expanded_a = resolver
        .expand_module_call(&call_a, "prod", None)
        .unwrap()
        .resources;
    let expanded_b = resolver
        .expand_module_call(&call_b, "staging", None)
        .unwrap()
        .resources;

    // binding must be prefixed so they don't collide (using dot notation)
    assert_eq!(
        expanded_a[0].binding,
        Some("prod.vpc".to_string()),
        "Instance A vpc binding should use dot path"
    );
    assert_eq!(
        expanded_a[1].binding,
        Some("prod.subnet".to_string()),
        "Instance A subnet binding should use dot path"
    );
    assert_eq!(
        expanded_b[0].binding,
        Some("staging.vpc".to_string()),
        "Instance B vpc binding should use dot path"
    );
    assert_eq!(
        expanded_b[1].binding,
        Some("staging.subnet".to_string()),
        "Instance B subnet binding should use dot path"
    );

    // Intra-module ResourceRef must point to the dot-path binding
    assert_eq!(
        expanded_a[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "prod.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
        "Instance A subnet should reference prod.vpc, not bare vpc"
    );
    assert_eq!(
        expanded_b[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "staging.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
        "Instance B subnet should reference staging.vpc, not bare vpc"
    );

    // Resource names should also be distinct (dot notation)
    assert_eq!(expanded_a[0].id.name_str(), "prod.main_vpc");
    assert_eq!(expanded_b[0].id.name_str(), "staging.main_vpc");
}

/// Module with an attributes block that exposes a security_group binding.
fn create_module_with_attributes() -> ParsedFile {
    use crate::parser::AttributeParameter;

    ParsedFile {
        providers: vec![],
        resources: vec![Resource {
            id: ResourceId::new("security_group", "sg"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("sg".to_string())),
                );
                attrs.insert(
                    "_type".to_string(),
                    Value::Concrete(ConcreteValue::String("aws.security_group".to_string())),
                );
                attrs.into_iter().collect()
            },
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: Some("sg".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![],
        attribute_params: vec![AttributeParameter {
            name: "security_group".to_string(),
            type_expr: None,
            value: Some(Value::resource_ref(
                "sg".to_string(),
                "id".to_string(),
                vec![],
            )),
        }],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_expand_module_call_creates_virtual_resource() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall {
        module_name: "web_tier".to_string(),
        binding_name: Some("web".to_string()),
        arguments: HashMap::new(),
    };

    let expanded = resolver
        .expand_module_call(&call, "web", None)
        .unwrap()
        .resources;
    // 1 real resource + 1 virtual resource
    assert_eq!(expanded.len(), 2);

    // Find the virtual resource
    let virtual_res = expanded
        .iter()
        .find(|r| r.is_virtual())
        .expect("Virtual resource should exist");

    assert_eq!(virtual_res.binding, Some("web".to_string()));
    // Module info should be in the kind, not in attributes
    assert_eq!(
        virtual_res.kind,
        ResourceKind::Virtual {
            module_name: "web_tier".to_string(),
            instance: "web".to_string(),
        }
    );
    assert!(!virtual_res.attributes.contains_key("_module"));
    assert!(!virtual_res.attributes.contains_key("_module_instance"));
    // The security_group attribute should be a rewritten ResourceRef
    // pointing to the dot-path binding (web.sg)
    assert_eq!(
        virtual_res.get_attr("security_group"),
        Some(&Value::resource_ref(
            "web.sg".to_string(),
            "id".to_string(),
            vec![]
        ))
    );
}

#[test]
fn test_expand_module_call_without_binding_no_virtual() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    // Module call without binding_name
    let call = ModuleCall {
        module_name: "web_tier".to_string(),
        binding_name: None,
        arguments: HashMap::new(),
    };

    let expanded = resolver
        .expand_module_call(&call, "web_tier", None)
        .unwrap()
        .resources;
    // Only real resources, no virtual
    let virtual_count = expanded.iter().filter(|r| r.is_virtual()).count();
    assert_eq!(virtual_count, 0);
}

/// Regression fixtures for #2197. Writes a minimal `modules/thing` module
/// (one `awscc.iam.Role` whose `role_name` comes from a `name` argument)
/// and a `root/main.crn` with the caller-supplied body; returns the parsed
/// root with modules already resolved.
fn resolve_thing_fixture(root_body: &str) -> ParsedFile {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/thing");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");
    parsed
}

fn role_names(parsed: &ParsedFile) -> HashSet<String> {
    parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .filter_map(|r| match r.get_attr("role_name")? {
            Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn test_anonymous_module_calls_get_distinct_prefixes() {
    let call_a = ModuleCall {
        module_name: "github".to_string(),
        binding_name: None,
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "github_repo".to_string(),
                Value::Concrete(ConcreteValue::String("carina-rs/infra".to_string())),
            );
            args
        },
    };
    let call_b = ModuleCall {
        module_name: "github".to_string(),
        binding_name: None,
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "github_repo".to_string(),
                Value::Concrete(ConcreteValue::String("carina-rs/other".to_string())),
            );
            args
        },
    };

    let a = instance_prefix_for_call(&call_a);
    let b = instance_prefix_for_call(&call_b);
    assert_ne!(a, b);
    assert!(
        a.starts_with("github_"),
        "expected `github_<16hex>`, got {a}"
    );
    assert_eq!(
        a.len(),
        "github_".len() + 16,
        "expected 16 hex chars in {a}"
    );
}

// SimHash is locality-sensitive: editing one argument must flip only a few
// bits so reconciliation can find the state entry. Assert the Hamming
// distance is below the reconciliation threshold.
#[test]
fn test_anonymous_module_call_prefix_is_locality_sensitive() {
    let make = |repo: &str| ModuleCall {
        module_name: "github".to_string(),
        binding_name: None,
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "github_repo".to_string(),
                Value::Concrete(ConcreteValue::String(repo.to_string())),
            );
            args.insert(
                "role_name".to_string(),
                Value::Concrete(ConcreteValue::String("github-actions".to_string())),
            );
            args.insert(
                "managed_policy_arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String(
                        "arn:aws:iam::aws:policy/AdministratorAccess".to_string(),
                    ),
                )])),
            );
            args
        },
    };

    let a = instance_prefix_for_call(&make("carina-rs/infra"));
    let b = instance_prefix_for_call(&make("carina-rs/other"));
    let parse = |p: &str| parse_synthetic_instance_prefix(p).unwrap().1;
    let distance = (parse(&a) ^ parse(&b)).count_ones();
    assert!(
        distance < crate::identifier::SIMHASH_HAMMING_THRESHOLD,
        "small edit should stay inside the reconciliation threshold, got distance {distance}",
    );
}

#[test]
fn test_named_module_call_uses_binding_name() {
    let call = ModuleCall {
        module_name: "github".to_string(),
        binding_name: Some("prod".to_string()),
        arguments: HashMap::new(),
    };
    assert_eq!(instance_prefix_for_call(&call), "prod");
}

#[test]
fn test_anonymous_module_calls_expand_into_distinct_instances() {
    let parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'alpha' }
thing { name = 'beta'  }
"#,
    );

    let role_addresses: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.name_str().to_string())
        .collect();
    assert_eq!(role_addresses.len(), 2, "got {:?}", role_addresses);

    assert_eq!(
        role_names(&parsed),
        ["alpha".to_string(), "beta".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
    );
}

#[test]
fn test_mixed_named_and_anonymous_module_calls_coexist() {
    let parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

let named = thing { name = 'named-call' }
thing              { name = 'anon-call'  }
"#,
    );

    assert_eq!(
        role_names(&parsed),
        ["named-call".to_string(), "anon-call".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
    );

    let addrs: Vec<&str> = parsed
        .resources
        .iter()
        .map(|r| r.id.name.as_str())
        .collect();
    assert!(addrs.iter().any(|n| n.starts_with("named.")), "{:?}", addrs);
    assert!(addrs.iter().any(|n| n.starts_with("thing_")), "{:?}", addrs);
}

// Reconciliation: an argument edit moves the SimHash prefix by a few bits;
// if the old prefix is in state and the new one is not, the reconciler
// must rewrite the expanded resources to use the state address.
#[test]
fn test_reconcile_anonymous_module_instances_remaps_close_prefix() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    );

    let before: Vec<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.name_str().to_string())
        .collect();
    assert_eq!(before.len(), 1);
    let (new_prefix, _) = before[0].split_once('.').unwrap();
    let (module, new_hash) = parse_synthetic_instance_prefix(new_prefix).unwrap();
    assert_eq!(module, "thing");

    // Fabricate a state entry whose SimHash is within threshold of the
    // current one (flip one bit).
    let state_hash = new_hash ^ 1;
    let state_name = format!("thing_{:016x}.role", state_hash);
    let state_lookup = |_: &str, _: &str| vec![state_name.clone()];

    reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

    let after: Vec<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.name_str().to_string())
        .collect();
    assert_eq!(
        after,
        vec![state_name.clone()],
        "expected prefix to be remapped to state's",
    );
    let role = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap();
    assert_eq!(
        role.binding.as_deref(),
        Some(format!("thing_{:016x}.role", state_hash).as_str()),
        "binding should be remapped too",
    );
}

// Reconciliation must not cross module names: a `foo_<hash>` state entry
// has nothing to do with a current `bar_<hash>` DSL instance.
#[test]
fn test_reconcile_anonymous_module_instances_ignores_other_modules() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
    );

    let before_name: String = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .name_str()
        .to_string();

    // State entry uses a different module name.
    let state_lookup = |_: &str, _: &str| vec!["other_0000000000000001.role".to_string()];
    reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

    let after_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .name_str()
        .to_string();
    assert_eq!(before_name, after_name);
}

// Regression for #2211: a single anonymous module instance whose module
// expands to multiple resource types means the same state prefix shows up
// once per resource type when `find_state_names_by_type` is queried per
// (provider, type). The reconciler must treat repeated identical hashes
// as the same candidate, not as multiple ambiguous candidates.
#[test]
fn test_reconcile_anonymous_module_instances_dedups_state_prefixes_across_types() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/thing");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let provider_res = awscc.iam.OidcProvider {
  url             = 'https://example.com'
  client_id_list  = ['x']
  thumbprint_list = ['y']
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'after-edit' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");

    // Discover the new prefix from the parsed Role.
    let role_name_before = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .name_str()
        .to_string();
    let (new_prefix, _) = role_name_before.split_once('.').unwrap();
    let (_, new_hash) = parse_synthetic_instance_prefix(new_prefix).unwrap();

    // State holds the *same* instance prefix at two resource types, one
    // bit away from the current SimHash — i.e. a small argument edit.
    let state_hash = new_hash ^ 1;
    let state_lookup = move |_: &str, resource_type: &str| match resource_type {
        "iam.OidcProvider" => vec![format!("thing_{:016x}.provider_res", state_hash)],
        "iam.Role" => vec![format!("thing_{:016x}.role", state_hash)],
        _ => vec![],
    };

    reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

    let role_after = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap();
    assert_eq!(
        role_after.id.name_str(),
        format!("thing_{:016x}.role", state_hash),
        "Role address must be remapped to the state prefix",
    );
    let provider_after = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.OidcProvider")
        .unwrap();
    assert_eq!(
        provider_after.id.name_str(),
        format!("thing_{:016x}.provider_res", state_hash),
        "OidcProvider address must be remapped to the state prefix",
    );
}

// Reconciliation must not run when there are multiple candidate state
// prefixes within threshold — ambiguity means we can't tell which is the
// "same instance."
#[test]
fn test_reconcile_anonymous_module_instances_skips_ambiguous() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'a' }
"#,
    );

    let before_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .name_str()
        .to_string();
    let (prefix, _) = before_name.split_once('.').unwrap();
    let (_, cur_hash) = parse_synthetic_instance_prefix(prefix).unwrap();

    // Two state entries at the same Hamming distance — ambiguous.
    let state_lookup = move |_: &str, _: &str| {
        vec![
            format!("thing_{:016x}.role", cur_hash ^ 0b1),
            format!("thing_{:016x}.role", cur_hash ^ 0b10),
        ]
    };
    reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

    let after_name = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .unwrap()
        .id
        .name_str()
        .to_string();
    assert_eq!(before_name, after_name, "ambiguous match must not remap");
}

// When state holds prefix A and the DSL has both A (unchanged) and a new
// A' (a new anonymous call with similar args), A' must not be remapped
// onto A — they are two distinct instances even though their SimHashes
// are close. State prefixes already in use by current DSL must not serve
// as remap candidates.
#[test]
fn test_reconcile_anonymous_module_instances_does_not_steal_in_use_prefix() {
    let mut parsed = resolve_thing_fixture(
        r#"
let thing = use { source = '../modules/thing' }

thing { name = 'unchanged' }
thing { name = 'unchanged-but-different' }
"#,
    );

    let prefixes_before: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.name_str().split_once('.').unwrap().0.to_string())
        .collect();
    assert_eq!(prefixes_before.len(), 2);
    let mut iter = prefixes_before.iter();
    let first = iter.next().unwrap().clone();
    let _second = iter.next().unwrap().clone();

    // State only holds the *first* prefix. The reconciler must not
    // remap the second instance onto it.
    let first_clone = first.clone();
    let state_lookup = move |_: &str, _: &str| vec![format!("{}.role", first_clone)];
    reconcile_anonymous_module_instances(&mut parsed.resources, &state_lookup);

    let prefixes_after: HashSet<String> = parsed
        .resources
        .iter()
        .filter(|r| r.id.resource_type == "iam.Role")
        .map(|r| r.id.name_str().split_once('.').unwrap().0.to_string())
        .collect();
    assert_eq!(
        prefixes_after, prefixes_before,
        "in-use state prefix must not be reassigned to a different DSL instance",
    );
}

#[test]
fn test_missing_required_argument() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: HashMap::new(), // Missing vpc_id
    };

    let result = resolver.expand_module_call(&call, "my_instance", None);
    assert!(matches!(result, Err(ModuleError::MissingArgument { .. })));
}

#[test]
fn test_expand_module_call_uses_dot_path_addressing() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".to_string())),
            );
            args
        },
    };

    let expanded = resolver
        .expand_module_call(&call, "my_instance", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let sg = &expanded[0];
    // Resource name should use dot notation, not underscore
    assert_eq!(sg.id.name_str(), "my_instance.sg");
}

#[test]
fn test_module_dot_path_bindings_and_refs() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("net".to_string(), create_module_with_intra_refs());
        r
    };

    let call = ModuleCall {
        module_name: "net".to_string(),
        binding_name: Some("prod".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            );
            args
        },
    };

    let expanded = resolver
        .expand_module_call(&call, "prod", None)
        .unwrap()
        .resources;

    // Resource names should use dot notation
    assert_eq!(expanded[0].id.name_str(), "prod.main_vpc");
    assert_eq!(expanded[1].id.name_str(), "prod.sub");

    // binding should use dot notation
    assert_eq!(expanded[0].binding, Some("prod.vpc".to_string()));
    assert_eq!(expanded[1].binding, Some("prod.subnet".to_string()));

    // Intra-module ResourceRef should use dot notation
    assert_eq!(
        expanded[1].get_attr("vpc_id"),
        Some(&Value::resource_ref(
            "prod.vpc".to_string(),
            "id".to_string(),
            vec![]
        )),
    );
}

/// carina#3061: a `wait` block declared inside a module must survive
/// expansion. Every binding-name field is instance-prefixed, and a
/// module resource that references the wait binding has that reference
/// prefixed too (so the dependency edge to the `Effect::Wait` forms).
#[test]
fn test_expand_module_call_propagates_and_prefixes_wait_bindings() {
    use crate::parser::{UntilPredicateAst, WaitBinding};

    let module = {
        let mut m = create_module_with_intra_refs();
        // A resource that consumes the wait binding the same way the
        // real CloudFront Distribution consumes `cert_issued`.
        m.resources.push(Resource {
            id: ResourceId::new("cloudfront.Distribution", "distribution"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "acm_certificate_arn".to_string(),
                    Value::resource_ref(
                        "cert_issued".to_string(),
                        "certificate_arn".to_string(),
                        vec![],
                    ),
                );
                attrs.into_iter().collect()
            },
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: Some("distribution".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        });
        m.wait_bindings.push(WaitBinding {
            binding: "cert_issued".to_string(),
            target: "main_vpc".to_string(),
            until_raw: "main_vpc.state == \"available\"".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["main_vpc".to_string(), "state".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("available".to_string())),
            },
            timeout_secs: Some(300),
            depends_on: vec!["subnet".to_string()],
            line: 1,
        });
        m
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("net".to_string(), module);
        r
    };

    let call = ModuleCall {
        module_name: "net".to_string(),
        binding_name: Some("prod".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            );
            args
        },
    };

    let expanded = resolver.expand_module_call(&call, "prod", None).unwrap();

    // The wait binding survived expansion and every binding-name field
    // is instance-prefixed.
    assert_eq!(expanded.wait_bindings.len(), 1);
    let wb = &expanded.wait_bindings[0];
    assert_eq!(wb.binding, "prod.cert_issued");
    assert_eq!(wb.target, "prod.main_vpc");
    assert_eq!(
        wb.until_predicate.lhs_segments,
        vec!["prod.main_vpc".to_string(), "state".to_string()]
    );
    assert_eq!(wb.depends_on, vec!["prod.subnet".to_string()]);
    // RHS value and surface text are not binding names — unchanged.
    assert_eq!(
        wb.until_predicate.rhs,
        Value::Concrete(ConcreteValue::String("available".to_string()))
    );
    assert_eq!(wb.timeout_secs, Some(300));

    // The downstream resource's reference to the wait binding was
    // rewritten to the prefixed name, so the dependency edge to the
    // Effect::Wait can form at plan time.
    let dist = expanded
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("distribution resource present");
    assert_eq!(
        dist.get_attr("acm_certificate_arn"),
        Some(&Value::resource_ref(
            "prod.cert_issued".to_string(),
            "certificate_arn".to_string(),
            vec![]
        )),
        "the module resource's `cert_issued` ref must be instance-prefixed"
    );
}

#[test]
fn test_module_virtual_resource_dot_path_refs() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("web_tier".to_string(), create_module_with_attributes());
        r
    };

    let call = ModuleCall {
        module_name: "web_tier".to_string(),
        binding_name: Some("web".to_string()),
        arguments: HashMap::new(),
    };

    let expanded = resolver
        .expand_module_call(&call, "web", None)
        .unwrap()
        .resources;

    let virtual_res = expanded
        .iter()
        .find(|r| r.is_virtual())
        .expect("Virtual resource should exist");

    // The security_group attribute should reference dot-notation binding
    assert_eq!(
        virtual_res.get_attr("security_group"),
        Some(&Value::resource_ref(
            "web.sg".to_string(),
            "id".to_string(),
            vec![]
        ))
    );
}

#[test]
fn test_substitute_arguments_interpolation() {
    use crate::resource::InterpolationPart;

    let mut inputs = HashMap::new();
    inputs.insert(
        "env_name".to_string(),
        Value::Concrete(ConcreteValue::String("dev".to_string())),
    );

    // Interpolation like "prefix-${env_name}-suffix" where env_name is a module argument
    let value = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("prefix-".to_string()),
        InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
            binding: "env_name".to_string(),
        })),
        InterpolationPart::Literal("-suffix".to_string()),
    ]));
    let result = substitute_arguments(&value, &inputs);

    // After substitution, the ResourceRef should be replaced with the argument value
    assert_eq!(
        result,
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("dev".to_string()))),
            InterpolationPart::Literal("-suffix".to_string()),
        ]))
    );
}

#[test]
fn test_unknown_argument_rejected() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-456".to_string())),
            );
            // Unknown argument: not declared in the module
            args.insert(
                "unknown_arg".to_string(),
                Value::Concrete(ConcreteValue::String("should-fail".to_string())),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "my_instance", None);
    assert!(
        matches!(result, Err(ModuleError::UnknownArgument { .. })),
        "Expected UnknownArgument error, got {:?}",
        result
    );
}

#[test]
fn test_substitute_arguments_function_call() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "cidr".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );

    // FunctionCall like cidr_subnet(cidr, 8, 0) where cidr is a module argument
    let value = Value::Deferred(DeferredValue::FunctionCall {
        name: "cidr_subnet".to_string(),
        args: vec![
            Value::Deferred(DeferredValue::BindingRef {
                binding: "cidr".to_string(),
            }),
            Value::Concrete(ConcreteValue::Int(8)),
            Value::Concrete(ConcreteValue::Int(0)),
        ],
    });
    let result = substitute_arguments(&value, &inputs);

    assert_eq!(
        result,
        Value::Deferred(DeferredValue::FunctionCall {
            name: "cidr_subnet".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                Value::Concrete(ConcreteValue::Int(8)),
                Value::Concrete(ConcreteValue::Int(0)),
            ],
        })
    );
}

/// Module with interpolation in resource attributes to test argument substitution
fn create_module_with_interpolation() -> ParsedFile {
    use crate::resource::InterpolationPart;

    ParsedFile {
        providers: vec![],
        resources: vec![Resource {
            id: ResourceId::new("ec2.Vpc", "vpc"),
            attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "cidr_block".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "cidr_block".to_string(),
                    }),
                );
                attrs.insert(
                    "name".to_string(),
                    Value::Deferred(DeferredValue::Interpolation(vec![
                        InterpolationPart::Literal("test-".to_string()),
                        InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
                            binding: "env_name".to_string(),
                        })),
                    ])),
                );
                attrs.insert(
                    "env".to_string(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: "env_name".to_string(),
                    }),
                );
                attrs.into_iter().collect()
            },
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: Some("vpc".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: std::collections::HashSet::new(),
        }],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "cidr_block".to_string(),
                type_expr: TypeExpr::String,
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "env_name".to_string(),
                type_expr: TypeExpr::String,
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_expand_module_call_with_interpolation() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("vpc_mod".to_string(), create_module_with_interpolation());
        r
    };

    let call = ModuleCall {
        module_name: "vpc_mod".to_string(),
        binding_name: Some("dev_vpc".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            );
            args.insert(
                "env_name".to_string(),
                Value::Concrete(ConcreteValue::String("dev".to_string())),
            );
            args
        },
    };

    let expanded = resolver
        .expand_module_call(&call, "dev_vpc", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let vpc = &expanded[0];

    // Simple argument substitution should work
    assert_eq!(
        vpc.get_attr("cidr_block"),
        Some(&Value::Concrete(ConcreteValue::String(
            "10.0.0.0/16".to_string()
        )))
    );
    assert_eq!(
        vpc.get_attr("env"),
        Some(&Value::Concrete(ConcreteValue::String("dev".to_string())))
    );

    // Interpolation with argument substitutes and canonicalizes back to
    // a flat `String` so downstream `Value::Concrete(ConcreteValue::String)` consumers (state
    // diff, plan rendering) see the resolved value. Without the post-
    // substitution canonicalize, this would stay as
    // `Interpolation([Literal("test-"), Expr(String("dev"))])` (#2815, #2817).
    assert_eq!(
        vpc.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "test-dev".to_string()
        ))),
    );
}

#[test]
fn test_nested_module_two_level() {
    // outer_module imports inner_module
    // resolve_modules on root.crn should expand both levels
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    resolve_modules(&mut parsed, &fixtures_dir).unwrap();

    // Should have resources from both inner_module (vpc) and outer_module (sg)
    let resource_types: Vec<&str> = parsed
        .resources
        .iter()
        .filter_map(|r| {
            r.get_attr("_type").and_then(|v| match v {
                Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
                _ => None,
            })
        })
        .collect();

    assert!(
        resource_types.iter().any(|t| t.ends_with(".Vpc")),
        "Should contain VPC resource from inner module, got: {:?}",
        resource_types
    );
    assert!(
        resource_types.iter().any(|t| t.ends_with(".SecurityGroup")),
        "Should contain security group from outer module, got: {:?}",
        resource_types
    );
}

#[test]
fn test_nested_module_three_level() {
    // root -> middle_module -> inner_module
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root_three_level.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    resolve_modules(&mut parsed, &fixtures_dir).unwrap();

    // Should have the VPC resource from inner_module (through middle_module)
    let resource_types: Vec<&str> = parsed
        .resources
        .iter()
        .filter_map(|r| {
            r.get_attr("_type").and_then(|v| match v {
                Value::Concrete(ConcreteValue::String(s)) => Some(s.as_str()),
                _ => None,
            })
        })
        .collect();

    assert!(
        resource_types.iter().any(|t| t.ends_with(".Vpc")),
        "Should contain VPC resource from inner module (3 levels deep), got: {:?}",
        resource_types
    );
}

#[test]
fn test_nested_module_cycle_detection() {
    // cycle_a imports cycle_b, cycle_b imports cycle_a
    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nested_modules");
    let content = fs::read_to_string(fixtures_dir.join("root_cycle.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();

    let result = resolve_modules(&mut parsed, &fixtures_dir);
    assert!(
        result.is_err(),
        "Should detect circular import, but got: {:?}",
        result
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err, ModuleError::CircularImport(_)),
        "Expected CircularImport error, got: {:?}",
        err
    );
}

#[test]
fn test_expand_module_call_with_function_call_argument() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("vpc_mod".to_string(), create_module_with_interpolation());
        r
    };

    // Pass a FunctionCall as an argument value
    let call = ModuleCall {
        module_name: "vpc_mod".to_string(),
        binding_name: Some("dev_vpc".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "cidr_block".to_string(),
                Value::Deferred(DeferredValue::FunctionCall {
                    name: "cidr_subnet".to_string(),
                    args: vec![
                        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                        Value::Concrete(ConcreteValue::Int(8)),
                        Value::Concrete(ConcreteValue::Int(0)),
                    ],
                }),
            );
            args.insert(
                "env_name".to_string(),
                Value::Concrete(ConcreteValue::String("dev".to_string())),
            );
            args
        },
    };

    let expanded = resolver
        .expand_module_call(&call, "dev_vpc", None)
        .unwrap()
        .resources;
    assert_eq!(expanded.len(), 1);

    let vpc = &expanded[0];

    // FunctionCall argument should be substituted as-is (resolved at apply time)
    assert_eq!(
        vpc.get_attr("cidr_block"),
        Some(&Value::Deferred(DeferredValue::FunctionCall {
            name: "cidr_subnet".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
                Value::Concrete(ConcreteValue::Int(8)),
                Value::Concrete(ConcreteValue::Int(0)),
            ],
        }))
    );
}

#[test]
fn test_load_module_missing_path_cleans_resolving_set() {
    // Nonexistent import path => descriptive IO error (NotFound), not the
    // single-file-module contract error. The resolving set must be cleaned
    // up so a retry does not masquerade as a circular import.
    let tmp_dir = std::env::temp_dir().join("carina_test_missing_path_cleanup");
    let _ = fs::create_dir_all(&tmp_dir);

    let mut resolver = ModuleResolver::new(&tmp_dir);

    let err = resolver
        .load_module("nonexistent")
        .expect_err("expected error");
    assert!(
        matches!(&err, ModuleError::Io(_)),
        "expected Io error for a nonexistent path, got: {err:?}"
    );

    let err = resolver
        .load_module("nonexistent")
        .expect_err("expected error");
    assert!(
        matches!(&err, ModuleError::Io(_)),
        "expected Io error on second attempt, not CircularImport, got: {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_load_module_parse_error_cleans_resolving_set() {
    let tmp_root = std::env::temp_dir().join("carina_test_parse_error_cleanup");
    let _ = fs::remove_dir_all(&tmp_root);
    let bad_module_dir = tmp_root.join("bad_module");
    fs::create_dir_all(&bad_module_dir).unwrap();
    fs::write(
        bad_module_dir.join("main.crn"),
        "this is not valid carina syntax {{{{",
    )
    .unwrap();

    let mut resolver = ModuleResolver::new(&tmp_root);

    // First attempt: parse error on a directory module with a bad .crn file.
    let result = resolver.load_module("bad_module");
    assert!(
        result.is_err(),
        "expected error but got: {:?}",
        result.unwrap()
    );
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ModuleError::Parse(_)),
        "expected Parse error on first attempt, got: {err:?}"
    );

    // Second attempt: should still get parse error, not circular import
    // (the resolving set must have been cleaned up).
    let result = resolver.load_module("bad_module");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(&err, ModuleError::Parse(_)),
        "expected Parse error on second attempt, not CircularImport, got: {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_load_module_rejects_file_path() {
    // Issue #1997: Modules must be directories. A single `.crn` file as a
    // module target should be rejected with NotADirectory instead of being
    // parsed as a one-file module.
    let tmp_root = std::env::temp_dir().join("carina_test_module_rejects_file");
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).unwrap();
    fs::write(tmp_root.join("single.crn"), "arguments {\n  x: String\n}\n").unwrap();

    let mut resolver = ModuleResolver::new(&tmp_root);
    let err = resolver
        .load_module("single.crn")
        .expect_err("a single .crn file must not be loadable as a module");
    assert!(
        matches!(&err, ModuleError::NotADirectory { .. }),
        "expected NotADirectory, got {err:?}"
    );

    let _ = fs::remove_dir_all(&tmp_root);
}

/// Helper to create a module with a validated port argument
fn create_module_with_port_validation() -> ParsedFile {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "port".to_string(),
            type_expr: TypeExpr::Int,
            default: Some(Value::Concrete(ConcreteValue::Int(8080))),
            description: Some("Web server port".to_string()),
            validations: vec![ValidationBlock {
                condition: ValidateExpr::And(
                    Box::new(ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("port".to_string())),
                        op: CompareOp::Gte,
                        rhs: Box::new(ValidateExpr::Int(1)),
                    }),
                    Box::new(ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("port".to_string())),
                        op: CompareOp::Lte,
                        rhs: Box::new(ValidateExpr::Int(65535)),
                    }),
                ),
                error_message: Some("Port must be between 1 and 65535".to_string()),
            }],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn test_argument_validation_passes_with_valid_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "web_server".to_string(),
        binding_name: Some("web".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(443)));
            args
        },
    };

    let result = resolver.expand_module_call(&call, "web", None);
    assert!(result.is_ok());
}

#[test]
fn test_argument_validation_passes_with_default_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "web_server".to_string(),
        binding_name: Some("web".to_string()),
        arguments: HashMap::new(), // Uses default 8080
    };

    let result = resolver.expand_module_call(&call, "web", None);
    assert!(result.is_ok());
}

#[test]
fn test_argument_validation_fails_with_invalid_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "web_server".to_string(),
        binding_name: Some("web".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(0)));
            args
        },
    };

    let result = resolver.expand_module_call(&call, "web", None);
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        ModuleError::ArgumentValidationFailed {
            module,
            argument,
            message,
            actual,
        } => {
            assert_eq!(module, "web_server");
            assert_eq!(argument, "port");
            assert_eq!(message, "Port must be between 1 and 65535");
            assert_eq!(actual, "0");
        }
        other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
    }
}

#[test]
fn test_argument_validation_fails_with_negative_value() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "web_server".to_string(),
        binding_name: Some("web".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert("port".to_string(), Value::Concrete(ConcreteValue::Int(-1)));
            args
        },
    };

    let result = resolver.expand_module_call(&call, "web", None);
    assert!(result.is_err());
}

#[test]
fn test_argument_validation_fails_too_large() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "web_server".to_string(),
            create_module_with_port_validation(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "web_server".to_string(),
        binding_name: Some("web".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(70000)),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "web", None);
    assert!(result.is_err());
}

#[test]
fn test_argument_validation_no_message_uses_default() {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "count".to_string(),
            type_expr: TypeExpr::Int,
            default: None,
            description: None,
            validations: vec![ValidationBlock {
                condition: ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("count".to_string())),
                    op: CompareOp::Gt,
                    rhs: Box::new(ValidateExpr::Int(0)),
                },
                error_message: None,
            }],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("counter".to_string(), module);
        r
    };

    let call = ModuleCall {
        module_name: "counter".to_string(),
        binding_name: Some("c".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert("count".to_string(), Value::Concrete(ConcreteValue::Int(0)));
            args
        },
    };

    let result = resolver.expand_module_call(&call, "c", None);
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        ModuleError::ArgumentValidationFailed { message, .. } => {
            assert_eq!(message, "validation failed for argument 'count'");
        }
        other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
    }
}

#[test]
fn test_argument_validation_len_with_list() {
    use crate::parser::{CompareOp, ValidateExpr, ValidationBlock};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "tags".to_string(),
            type_expr: TypeExpr::List(Box::new(TypeExpr::String)),
            default: None,
            description: None,
            validations: vec![ValidationBlock {
                condition: ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::FunctionCall {
                        name: "len".to_string(),
                        args: vec![ValidateExpr::Var("tags".to_string())],
                    }),
                    op: CompareOp::Gte,
                    rhs: Box::new(ValidateExpr::Int(1)),
                },
                error_message: Some("At least one tag is required".to_string()),
            }],
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("tagged".to_string(), module);
        r
    };

    // Valid: non-empty list
    let call = ModuleCall {
        module_name: "tagged".to_string(),
        binding_name: Some("t".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("env:prod".to_string()),
                )])),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "t", None).is_ok());

    // Invalid: empty list
    let call = ModuleCall {
        module_name: "tagged".to_string(),
        binding_name: Some("t".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::List(vec![])),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call, "t", None);
    assert!(result.is_err());
    match result.unwrap_err() {
        ModuleError::ArgumentValidationFailed { message, .. } => {
            assert_eq!(message, "At least one tag is required");
        }
        other => panic!("Expected ArgumentValidationFailed, got {:?}", other),
    }
}

#[test]
fn test_require_block_passes() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "enable_https".to_string(),
                type_expr: TypeExpr::Bool,
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "cert_arn".to_string(),
                type_expr: TypeExpr::String,
                default: Some(Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:123:cert/abc".to_string(),
                ))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock {
            // !enable_https || cert_arn != null
            condition: ValidateExpr::Or(
                Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                    "enable_https".to_string(),
                )))),
                Box::new(ValidateExpr::Compare {
                    lhs: Box::new(ValidateExpr::Var("cert_arn".to_string())),
                    op: CompareOp::Ne,
                    rhs: Box::new(ValidateExpr::Null),
                }),
            ),
            error_message: "cert_arn is required when HTTPS is enabled".to_string(),
        }],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("web".to_string(), module);
        r
    };

    // HTTPS enabled with cert_arn provided: should pass
    let call = ModuleCall {
        module_name: "web".to_string(),
        binding_name: Some("w".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "enable_https".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            args.insert(
                "cert_arn".to_string(),
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:acm:us-east-1:123:cert/abc".to_string(),
                )),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "w", None).is_ok());
}

#[test]
fn test_require_block_fails_with_not_expr() {
    use crate::parser::{RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "enable_https".to_string(),
                type_expr: TypeExpr::Bool,
                default: Some(Value::Concrete(ConcreteValue::Bool(true))),
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "has_cert".to_string(),
                type_expr: TypeExpr::Bool,
                default: Some(Value::Concrete(ConcreteValue::Bool(false))),
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock {
            // !enable_https || has_cert
            condition: ValidateExpr::Or(
                Box::new(ValidateExpr::Not(Box::new(ValidateExpr::Var(
                    "enable_https".to_string(),
                )))),
                Box::new(ValidateExpr::Var("has_cert".to_string())),
            ),
            error_message: "cert is required when HTTPS is enabled".to_string(),
        }],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("web".to_string(), module);
        r
    };

    // HTTPS enabled but has_cert is false: should fail
    let call = ModuleCall {
        module_name: "web".to_string(),
        binding_name: Some("w".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "enable_https".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            args.insert(
                "has_cert".to_string(),
                Value::Concrete(ConcreteValue::Bool(false)),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call, "w", None);
    assert!(result.is_err());
    match result.unwrap_err() {
        ModuleError::RequireConstraintFailed { message, .. } => {
            assert_eq!(message, "cert is required when HTTPS is enabled");
        }
        other => panic!("Expected RequireConstraintFailed, got {:?}", other),
    }
}

#[test]
fn test_require_block_len_function() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "subnet_ids".to_string(),
            type_expr: TypeExpr::List(Box::new(TypeExpr::String)),
            default: None,
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock {
            // len(subnet_ids) >= 2
            condition: ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::FunctionCall {
                    name: "len".to_string(),
                    args: vec![ValidateExpr::Var("subnet_ids".to_string())],
                }),
                op: CompareOp::Gte,
                rhs: Box::new(ValidateExpr::Int(2)),
            },
            error_message: "ALB requires at least two subnets".to_string(),
        }],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("alb".to_string(), module);
        r
    };

    // Two subnets: should pass
    let call = ModuleCall {
        module_name: "alb".to_string(),
        binding_name: Some("lb".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "subnet_ids".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("subnet-a".to_string())),
                    Value::Concrete(ConcreteValue::String("subnet-b".to_string())),
                ])),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "lb", None).is_ok());

    // One subnet: should fail
    let call = ModuleCall {
        module_name: "alb".to_string(),
        binding_name: Some("lb".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "subnet_ids".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("subnet-a".to_string()),
                )])),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call, "lb", None);
    assert!(result.is_err());
    match result.unwrap_err() {
        ModuleError::RequireConstraintFailed { message, .. } => {
            assert_eq!(message, "ALB requires at least two subnets");
        }
        other => panic!("Expected RequireConstraintFailed, got {:?}", other),
    }
}

#[test]
fn test_require_block_multiple_constraints() {
    use crate::parser::{CompareOp, RequireBlock, ValidateExpr};
    let module = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![
            ArgumentParameter {
                name: "min_size".to_string(),
                type_expr: TypeExpr::Int,
                default: None,
                description: None,
                validations: Vec::new(),
            },
            ArgumentParameter {
                name: "max_size".to_string(),
                type_expr: TypeExpr::Int,
                default: None,
                description: None,
                validations: Vec::new(),
            },
        ],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![RequireBlock {
            // min_size <= max_size
            condition: ValidateExpr::Compare {
                lhs: Box::new(ValidateExpr::Var("min_size".to_string())),
                op: CompareOp::Lte,
                rhs: Box::new(ValidateExpr::Var("max_size".to_string())),
            },
            error_message: "min_size must be <= max_size".to_string(),
        }],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    };

    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert("asg".to_string(), module);
        r
    };

    // min_size < max_size: should pass
    let call = ModuleCall {
        module_name: "asg".to_string(),
        binding_name: Some("a".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "min_size".to_string(),
                Value::Concrete(ConcreteValue::Int(1)),
            );
            args.insert(
                "max_size".to_string(),
                Value::Concrete(ConcreteValue::Int(5)),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "a", None).is_ok());

    // min_size > max_size: should fail
    let call = ModuleCall {
        module_name: "asg".to_string(),
        binding_name: Some("a".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "min_size".to_string(),
                Value::Concrete(ConcreteValue::Int(10)),
            );
            args.insert(
                "max_size".to_string(),
                Value::Concrete(ConcreteValue::Int(5)),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call, "a", None);
    assert!(result.is_err());
    match result.unwrap_err() {
        ModuleError::RequireConstraintFailed { message, .. } => {
            assert_eq!(message, "min_size must be <= max_size");
        }
        other => panic!("Expected RequireConstraintFailed, got {:?}", other),
    }
}

#[test]
fn test_argument_type_mismatch_int_for_string() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: {
            let mut args = HashMap::new();
            // vpc_id expects string, pass int
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::Int(42)),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "my_instance", None);
    assert!(
        matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
        "Expected InvalidArgumentType error, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_mismatch_string_for_bool() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules
            .insert("test_module".to_string(), create_test_module());
        r
    };

    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("my_instance".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
            );
            // enable_flag expects bool, pass string
            args.insert(
                "enable_flag".to_string(),
                Value::Concrete(ConcreteValue::String("not-a-bool".to_string())),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "my_instance", None);
    assert!(
        matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
        "Expected InvalidArgumentType error, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_custom_validator() {
    use crate::parser::ValidatorFn;

    // Create a ProviderContext with a custom "arn" validator
    let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
    validators.insert(
        "arn".to_string(),
        Box::new(|s: &str| {
            if s.starts_with("arn:") {
                Ok(())
            } else {
                Err(format!("expected ARN format, got '{}'", s))
            }
        }),
    );
    let config = ProviderContext {
        decryptor: None,
        validators,
        custom_type_validator: None,
        schema_types: Default::default(),
    };

    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "policy_arn".to_string(),
        type_expr: TypeExpr::Simple("arn".to_string()),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut r = ModuleResolver::with_config(".", &config);
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    // Valid ARN passes
    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("a".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "policy_arn".to_string(),
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:iam::123456789012:policy/MyPolicy".to_string(),
                )),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "a", None).is_ok());

    // Invalid ARN fails
    let call_bad = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("b".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "policy_arn".to_string(),
                Value::Concrete(ConcreteValue::String("not-an-arn".to_string())),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call_bad, "b", None);
    assert!(
        matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
        "Expected InvalidArgumentType error for invalid ARN, got {:?}",
        result
    );
}

#[test]
fn test_argument_type_list_of_custom_type() {
    use crate::parser::ValidatorFn;

    let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
    validators.insert(
        "arn".to_string(),
        Box::new(|s: &str| {
            if s.starts_with("arn:") {
                Ok(())
            } else {
                Err(format!("expected ARN format, got '{}'", s))
            }
        }),
    );
    let config = ProviderContext {
        decryptor: None,
        validators,
        custom_type_validator: None,
        schema_types: Default::default(),
    };

    let mut module = create_test_module();
    module.arguments = vec![ArgumentParameter {
        name: "policy_arns".to_string(),
        type_expr: TypeExpr::List(Box::new(TypeExpr::Simple("arn".to_string()))),
        default: None,
        description: None,
        validations: Vec::new(),
    }];

    let resolver = {
        let mut r = ModuleResolver::with_config(".", &config);
        r.imported_modules.insert("test_module".to_string(), module);
        r
    };

    // Valid list of ARNs
    let call = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("a".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "policy_arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:iam::123:policy/A".to_string(),
                    )),
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:iam::123:policy/B".to_string(),
                    )),
                ])),
            );
            args
        },
    };
    assert!(resolver.expand_module_call(&call, "a", None).is_ok());

    // List with invalid ARN fails
    let call_bad = ModuleCall {
        module_name: "test_module".to_string(),
        binding_name: Some("b".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "policy_arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:iam::123:policy/A".to_string(),
                    )),
                    Value::Concrete(ConcreteValue::String("not-an-arn".to_string())),
                ])),
            );
            args
        },
    };
    let result = resolver.expand_module_call(&call_bad, "b", None);
    assert!(
        matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
        "Expected InvalidArgumentType for list with invalid ARN, got {:?}",
        result
    );
}

#[test]
fn test_load_module_directory_merges_sibling_files_with_main() {
    // A directory-based module that splits definitions across main.crn and
    // sibling files (arguments.crn, exports.crn, resources.crn) must be
    // parsed as a whole. The previous behavior returned only main.crn's
    // contents when main.crn existed, silently dropping siblings.
    let tmp_dir = std::env::temp_dir().join("carina_test_load_module_sibling_merge");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    fs::write(tmp_dir.join("main.crn"), "# main module file\n").unwrap();
    fs::write(
        tmp_dir.join("arguments.crn"),
        "arguments {\n  env: String\n}\n",
    )
    .unwrap();
    fs::write(
        tmp_dir.join("exports.crn"),
        "exports {\n  region = \"ap-northeast-1\"\n}\n",
    )
    .unwrap();

    let parsed = load_module(&tmp_dir)
        .expect("expected module to load because arguments.crn declares an argument");

    assert_eq!(
        parsed.arguments.len(),
        1,
        "arguments declared in arguments.crn must be preserved when main.crn exists"
    );
    assert_eq!(parsed.arguments[0].name, "env");
    assert_eq!(
        parsed.export_params.len(),
        1,
        "exports declared in exports.crn must be preserved when main.crn exists"
    );
    assert_eq!(parsed.export_params[0].name, "region");

    let _ = fs::remove_dir_all(&tmp_dir);
}

/// Helper for #2393 regression tests: write a module body and a calling
/// root body to a tempdir, parse and resolve, return the parsed root with
/// modules expanded. Mirrors the directory-scoped fixture shape required by
/// CLAUDE.md so single-file thinking can't sneak back in.
fn resolve_default_arg_fixture(module_body: &str, root_body: &str) -> ParsedFile {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/m");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(module_dir.join("main.crn"), module_body).unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(root_dir.join("main.crn"), root_body).unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");
    parsed
}

fn role_attr<'a>(parsed: &'a ParsedFile, attr: &str) -> &'a Value {
    parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "iam.Role")
        .expect("Role resource should exist")
        .get_attr(attr)
        .unwrap_or_else(|| panic!("Role.{attr} should exist"))
}

// Regression for #2393. A module argument default that interpolates another
// argument (`["repo:${github_repo}:*"]`) must resolve `${github_repo}` against
// the caller-provided value, not leave it as the literal binding name.
#[test]
fn test_argument_default_interpolates_other_arguments() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  github_repo     : String
  subject_patterns: list(String) = ["repo:${github_repo}:*"]
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {
    patterns = subject_patterns
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  github_repo = 'carina-rs/infra'
}
"#,
    );

    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("assume_role_policy_document should be a Map, got {policy:?}");
    };
    let patterns = policy_map.get("patterns").expect("patterns should exist");
    let Value::Concrete(ConcreteValue::List(items)) = patterns else {
        panic!("patterns should be a List, got {patterns:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0],
        Value::Concrete(ConcreteValue::String("repo:carina-rs/infra:*".to_string())),
        "default's ${{github_repo}} interpolation must resolve against the caller's value"
    );
}

// #2393 — block-form default with `${other_arg}` interpolation must also
// resolve, not just the simple `name: T = expr` form.
#[test]
fn test_argument_default_block_form_interpolates_other_arguments() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  github_repo: String
  subject_pattern: String {
    description = "subject pattern"
    default     = "repo:${github_repo}:*"
  }
}

let role = awscc.iam.Role {
  role_name                   = subject_pattern
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  github_repo = 'carina-rs/infra'
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("repo:carina-rs/infra:*".to_string())),
        "block-form default's `${{github_repo}}` must resolve against the caller's value"
    );
}

// #2393 — bare-identifier default (`b: String = a`, no `${}` wrapping) is a
// `Value::Deferred(DeferredValue::ResourceRef{ binding: "a" })` after parse and must resolve to the
// caller-supplied value of `a`.
#[test]
fn test_argument_default_bare_identifier_resolves() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  primary  : String
  secondary: String = primary
}

let role = awscc.iam.Role {
  role_name                   = secondary
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  primary = 'p'
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("p".to_string())),
        "bare-identifier default `secondary = primary` must resolve to caller's `primary`"
    );
}

// #2393 — transitive default chain `a → b → c`. Each default is resolved
// against arguments already in scope, so by the time `c = "${b}!"` is
// resolved, `b` has been canonicalized to a flat string `"X-X"`. Pinning
// this is important because removing the `canonicalize_in_place` call in
// the expander would produce a nested `Interpolation` in `c` that no
// downstream consumer flattens.
#[test]
fn test_argument_default_transitive_chain_resolves() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  a: String = 'X'
  b: String = "${a}-${a}"
  c: String = "${b}!"
}

let role = awscc.iam.Role {
  role_name                   = c
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("X-X!".to_string())),
        "transitive default chain `a → b → c` must collapse to a flat string"
    );
}

// #2393 — interpolation nested inside a Map value within a list default
// must be resolved by `substitute_arguments`'s recursion through
// List/Map/Interpolation arms.
#[test]
fn test_argument_default_nested_collection_interpolates() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  region : String
  tags   : map(String) = {
    managed_by = 'carina'
    region     = "${region}-tag"
  }
}

let role = awscc.iam.Role {
  role_name                   = 'r'
  assume_role_policy_document = {
    tags = tags
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  region = 'ap-northeast-1'
}
"#,
    );

    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("policy should be Map, got {policy:?}");
    };
    let tags = policy_map.get("tags").expect("tags should exist");
    let Value::Concrete(ConcreteValue::Map(tag_map)) = tags else {
        panic!("tags should be Map, got {tags:?}");
    };
    assert_eq!(
        tag_map.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1-tag".to_string()
        ))),
        "interpolation inside a nested map default must resolve recursively"
    );
}

// #2393 — module with arguments but no defaults at all must work; the new
// substitute+canonicalize pass is a no-op on caller-supplied scalars.
#[test]
fn test_argument_no_defaults_caller_scalars_pass_through() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  x: String
  y: Int
}

let role = awscc.iam.Role {
  role_name                   = x
  assume_role_policy_document = {
    y_value = y
  }
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {
  x = 'hello'
  y = 42
}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("hello".to_string())),
    );
    let policy = role_attr(&parsed, "assume_role_policy_document");
    let Value::Concrete(ConcreteValue::Map(policy_map)) = policy else {
        panic!("policy should be Map, got {policy:?}");
    };
    assert_eq!(
        policy_map.get("y_value"),
        Some(&Value::Concrete(ConcreteValue::Int(42)))
    );
}

// #2393 — argument defaults whose RHS references *another* argument
// (`prefix: String = later`) used to be resolved in strict declaration
// order by `substitute_arguments` in `expander.rs`. A forward
// reference therefore degraded — first to a literal
// `Value::Concrete(ConcreteValue::String("later"))` (pre-#2817), then to an unresolved
// `Value::Deferred(DeferredValue::ResourceRef("later"))` (post-#2817 PR1).
//
// This pass adds a fixed-point loop around `substitute_arguments`
// (#2817 follow-up): each iteration replaces every `ResourceRef`
// whose binding matches an already-resolved argument; the loop
// terminates when an iteration produces no changes. Cycles
// (`a = b`, `b = a`) hit a hard iteration cap and leave the still-
// unresolved refs in place for the post-merge scope check to flag.
#[test]
fn test_argument_default_forward_ref_resolves_under_fixpoint() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  prefix: String = later
  later : String = 'L'
}

let role = awscc.iam.Role {
  role_name                   = prefix
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    assert_eq!(
        role_attr(&parsed, "role_name"),
        &Value::Concrete(ConcreteValue::String("L".to_string())),
        "forward-ref default `prefix = later` resolves to `later`'s value `'L'` \
         once the fix-point loop runs in `expander.rs::substitute_arguments`"
    );
}

/// Argument-default fix-point loop must terminate even when the
/// dependency graph is cyclic. With `a: String = b` and `b: String = a`
/// neither side ever reduces, so each iteration leaves both as
/// `ResourceRef`. The hard iteration cap stops the loop and surfaces
/// the unresolved refs to the scope check.
#[test]
fn test_argument_default_cycle_terminates_with_unresolved_refs() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  a: String = b
  b: String = a
}

let role = awscc.iam.Role {
  role_name                   = a
  assume_role_policy_document = {}
}
"#,
        r#"
let m = use { source = '../modules/m' }

m {}
"#,
    );

    let role_name = role_attr(&parsed, "role_name");
    match role_name {
        // The cycle leaves both `a` and `b` as bare-binding refs; which
        // one surfaces as `role_name` depends on substitute_arguments's
        // walk order. Either is fine — the contract is that the loop
        // terminates without panicking and produces a structured ref
        // the scope check can flag. Since #2847 these bare refs are
        // `BindingRef`, not `ResourceRef` with empty attribute.
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            assert!(
                binding == "a" || binding == "b",
                "expected cyclic ref to point at `a` or `b`, got: {binding:?}"
            );
        }
        other => panic!("expected unresolved BindingRef from cycle; got {other:?}"),
    }
}

// #2393 — caller-supplied ResourceRefs (cross-module data flow) must NOT be
// substituted by the new default-resolution pass: their binding names are
// not argument names, so they should pass through untouched until the outer
// resolver rewrites them.
#[test]
fn test_argument_caller_resource_ref_passes_through() {
    let parsed = resolve_default_arg_fixture(
        r#"
arguments {
  source_arn: String
}

let role = awscc.iam.Role {
  role_name                   = source_arn
  assume_role_policy_document = {}
}
"#,
        r#"
let upstream = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

let m = use { source = '../modules/m' }

m {
  source_arn = upstream.arn
}
"#,
    );

    // Find the module's expanded `role` (the upstream Vpc has type Vpc, not Role).
    let role_name = role_attr(&parsed, "role_name");
    match role_name {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(
                path.binding(),
                "upstream",
                "caller's `upstream.arn` must remain a ResourceRef with binding=upstream"
            );
            assert_eq!(path.attribute(), "arn");
        }
        other => panic!("role_name should be a ResourceRef, got {other:?}"),
    }
}

#[test]
fn test_load_module_directory_merge_order_is_deterministic() {
    // Merged vectors must be ordered by file path so that downstream
    // first-match-wins lookups (hover, completion, diagnostics) do not
    // depend on filesystem iteration order.
    let tmp_dir = std::env::temp_dir().join("carina_test_load_module_merge_order");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // Create files out of lexicographic order to make the sort observable.
    fs::write(tmp_dir.join("z_last.crn"), "arguments {\n  c: String\n}\n").unwrap();
    fs::write(tmp_dir.join("a_first.crn"), "arguments {\n  a: String\n}\n").unwrap();
    fs::write(
        tmp_dir.join("m_middle.crn"),
        "arguments {\n  b: String\n}\n",
    )
    .unwrap();

    let parsed = load_module(&tmp_dir).expect("module should load");
    let names: Vec<&str> = parsed.arguments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a", "b", "c"],
        "arguments must be merged in sorted filename order"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}

/// Regression for #2549: a `list(String)` argument forwarded unchanged
/// from an outer (usecase) module to an inner module call must type-check.
#[test]
fn test_list_argument_passthrough_between_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Inner module: declares a list(String) argument.
    let inner_dir = tmp.path().join("modules/github_oidc");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  github_repo:      String
  subject_patterns: list(String) = ["repo:${github_repo}:*"]
}

let role = awscc.iam.Role {
  role_name = github_repo
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    // Outer (usecase) module: receives the same list(String) and forwards
    // it unchanged to the inner module.
    let outer_dir = tmp.path().join("usecases/bootstrap");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  github_repo:      String
  subject_patterns: list(String) = [
    'repo:carina-rs/infra:ref:refs/heads/main',
    'repo:carina-rs/infra:pull_request',
  ]
}

let github = use {
  source = '../../modules/github_oidc'
}

let bootstrap = github {
  github_repo      = github_repo
  subject_patterns = subject_patterns
}
"#,
    )
    .unwrap();

    // Caller of the usecase.
    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let bootstrap = use {
  source = '../usecases/bootstrap'
}

let prod = bootstrap {
  github_repo = 'carina-rs/infra'
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir)
        .expect("list(String) should flow through usecase to inner module");
}

/// Real infra shape: `let subjects = subject_patterns` rebinding before
/// pass-through. The forwarded value's binding name is `subjects`, not
/// `subject_patterns`, so the arg-table lookup must follow the alias.
#[test]
fn test_list_passthrough_through_let_alias() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/github_oidc");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  subject_patterns: list(String) = [
    'repo:carina-rs/infra:ref:refs/heads/main',
  ]
}

let github = use {
  source = '../../modules/github_oidc'
}

let subjects = subject_patterns
let bound = github {
  patterns = subjects
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir)
        .expect("list(String) via let alias should pass-through to inner module");
}

/// Same shape as the list-pass-through test but for `map(String)`. The fix
/// has to cover every container type, not only list — root cause is shared.
#[test]
fn test_map_argument_passthrough_between_modules() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  tags: map(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  tags: map(String) = { env = 'prod' }
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  tags = tags
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir)
        .expect("map(String) should flow through usecase to inner module");
}

/// A `String`-typed outer arg forwarded into a `list(String)` inner arg
/// is a type mismatch and must be rejected at the inner call site, even
/// though the inner call typechecks before the parent's arg substitution
/// runs. The inner typecheck looks the ref up in the enclosing module's
/// argument signatures and compares declared types.
#[test]
fn test_mismatched_argument_passthrough_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  s: String
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  patterns = s
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  s = 'hello'
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let result = resolve_modules(&mut parsed, &root_dir);
    assert!(
        matches!(&result, Err(ModuleError::InvalidArgumentType { .. })),
        "String forwarded to list(String) should be rejected, got {:?}",
        result
    );
}

/// Cross-scalar mismatch (Int → String) on a pass-through arg ref must
/// also be rejected — exercises `type_expr_compatible` on plain scalars,
/// which the list/map cases don't cover.
#[test]
fn test_int_argument_forwarded_to_string_arg_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  name: String
}

let role = awscc.iam.Role {
  role_name = name
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  count: Int
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  name = count
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  count = 3
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let result = resolve_modules(&mut parsed, &root_dir);
    assert!(
        matches!(&result, Err(ModuleError::InvalidArgumentType { .. })),
        "Int forwarded to String should be rejected, got {:?}",
        result
    );
}

/// Pass-through where the inner element type itself differs (`list(Int)`
/// → `list(String)`) must reject — exercises the recursive `(List, List)`
/// arm of `type_expr_compatible` rather than the top-level container
/// shape.
#[test]
fn test_list_inner_element_mismatch_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  numbers: list(Int)
}

let inner = use {
  source = '../../modules/inner'
}

let bound = inner {
  patterns = numbers
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  numbers = [1, 2, 3]
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let result = resolve_modules(&mut parsed, &root_dir);
    assert!(
        matches!(&result, Err(ModuleError::InvalidArgumentType { .. })),
        "list(Int) forwarded to list(String) should be rejected, got {:?}",
        result
    );
}

/// Three-deep pass-through: outer → middle → inner all forwarding the
/// same `list(String)` arg. Each enclosing scope contributes its own
/// arg signature; the chain must hold across both module-call hops.
#[test]
fn test_list_passthrough_three_levels_deep() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let inner_dir = tmp.path().join("modules/inner");
    fs::create_dir_all(&inner_dir).unwrap();
    fs::write(
        inner_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let role = awscc.iam.Role {
  role_name = 'r'
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let middle_dir = tmp.path().join("modules/middle");
    fs::create_dir_all(&middle_dir).unwrap();
    fs::write(
        middle_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let inner = use {
  source = '../inner'
}

let bound = inner {
  patterns = patterns
}
"#,
    )
    .unwrap();

    let outer_dir = tmp.path().join("usecases/outer");
    fs::create_dir_all(&outer_dir).unwrap();
    fs::write(
        outer_dir.join("main.crn"),
        r#"
arguments {
  patterns: list(String)
}

let middle = use {
  source = '../../modules/middle'
}

let bound = middle {
  patterns = patterns
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let outer = use {
  source = '../usecases/outer'
}

let prod = outer {
  patterns = ['a', 'b']
}
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir).expect("3-deep list pass-through should resolve");
}

// carina-rs/carina#2611: closed-set string types in `arguments`.
// These tests cover the *caller* side — when a module is declared
// with `env: 'dev' | 'prod' = 'dev'`, the call site
// `mod_call { env = 'dpv' }` must be rejected by typecheck. Without
// the StringLiteral/Union arms in `module_resolver::typecheck`, the
// typo would slip through to plan because the parser's pre-#2611
// fallback was `String`.

fn create_module_with_environment_union() -> ParsedFile {
    ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: IndexMap::new(),
        uses: vec![],
        module_calls: vec![],
        arguments: vec![ArgumentParameter {
            name: "environment".to_string(),
            type_expr: TypeExpr::Union(vec![
                TypeExpr::StringLiteral("dev".to_string()),
                TypeExpr::StringLiteral("prod".to_string()),
            ]),
            default: Some(Value::Concrete(ConcreteValue::String("dev".to_string()))),
            description: None,
            validations: Vec::new(),
        }],
        attribute_params: vec![],
        export_params: vec![],
        backend: None,
        state_blocks: vec![],
        user_functions: HashMap::new(),
        upstream_states: vec![],
        wait_bindings: vec![],
        requires: vec![],
        structural_bindings: HashSet::new(),
        warnings: vec![],
        deferred_for_expressions: vec![],
    }
}

#[test]
fn caller_side_typo_against_string_literal_union_is_rejected() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "env_module".to_string(),
            create_module_with_environment_union(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "env_module".to_string(),
        binding_name: Some("my_env".to_string()),
        arguments: {
            let mut args = HashMap::new();
            // Typo: 'dpv' is outside the declared 'dev' | 'prod' union.
            args.insert(
                "environment".to_string(),
                Value::Concrete(ConcreteValue::String("dpv".to_string())),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "my_env", None);
    assert!(
        matches!(result, Err(ModuleError::InvalidArgumentType { .. })),
        "Caller passing 'dpv' to `'dev' | 'prod'` must be rejected — \
         carina-rs/carina#2611. Got: {:?}",
        result
    );
}

#[test]
fn caller_side_value_in_string_literal_union_is_accepted() {
    let resolver = {
        let mut r = ModuleResolver::new(".");
        r.imported_modules.insert(
            "env_module".to_string(),
            create_module_with_environment_union(),
        );
        r
    };

    let call = ModuleCall {
        module_name: "env_module".to_string(),
        binding_name: Some("my_env".to_string()),
        arguments: {
            let mut args = HashMap::new();
            args.insert(
                "environment".to_string(),
                Value::Concrete(ConcreteValue::String("prod".to_string())),
            );
            args
        },
    };

    let result = resolver.expand_module_call(&call, "my_env", None);
    assert!(
        result.is_ok(),
        "Caller passing 'prod' (in the declared union) must be accepted. Got: {:?}",
        result
    );
}

/// Acceptance test for #2815 / #2817. An `arguments {}` block declared in
/// `main.crn` must be visible to identifier references in *sibling* `.crn`
/// files in the same module directory. Before #2817, the per-file
/// `ParseContext` could not see sibling-defined symbols, so `${env}` in
/// `role.crn` lowered to a literal `Value::Concrete(ConcreteValue::String("env"))` — `role_name`
/// rendered as `"test-role-env"` instead of `"test-role-dev"`. The
/// directory-aware parse pipeline seeds every file's `ParseContext` with
/// the union of declared names from all sibling files, so the normal
/// `ctx.get_variable` path resolves the reference uniformly.
#[test]
fn test_arguments_visible_to_sibling_crn_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/usecase");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  env: String
}
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("role.crn"),
        r#"
let role = awscc.iam.Role {
  role_name = "test-role-${env}"
  assume_role_policy_document = {}
}
"#,
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let uc = use { source = '../modules/usecase' }
let r  = uc { env = 'dev' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    resolve_modules(&mut parsed, &root_dir).expect("resolve_modules should succeed");

    assert_eq!(
        role_names(&parsed),
        ["test-role-dev".to_string()]
            .into_iter()
            .collect::<HashSet<_>>(),
        "argument `env` declared in `main.crn` must substitute into `${{env}}` \
         interpolation in sibling `role.crn`",
    );
}

/// Regression for the seeding-vs-local-duplicate corner of #2817.
///
/// Inside the directory-aware Pass-2 parse, every binding name from
/// the merged Pass-1 result is seeded into the per-file
/// `ParseContext`. The seeded name MUST NOT mask a real in-file
/// duplicate (`arguments { foo }` and `let foo = ...` in the same
/// file). The shadow logic is to drop the seed mark the moment a
/// local declaration overwrites it; if a *second* local declaration
/// then appears, the regular duplicate-binding error must still
/// trigger.
#[test]
fn test_local_duplicate_still_detected_under_seeded_parse() {
    // The duplicate is `let foo = "y"` colliding with `arguments { foo }`,
    // which both register `foo` as a binding. Seeding (a sibling file
    // with `let foo = ...`) primed the seed mark; the `arguments` block
    // must drop it so the subsequent `let foo` is still a duplicate.
    let tmp = tempfile::tempdir().expect("tempdir");
    let module_dir = tmp.path().join("modules/dup");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join("main.crn"),
        r#"
arguments {
  foo: String
}

let foo = "y"
"#,
    )
    .unwrap();
    fs::write(
        module_dir.join("sibling.crn"),
        "# also has foo via let later\nlet foo_other = \"z\"\n",
    )
    .unwrap();

    let root_dir = tmp.path().join("root");
    fs::create_dir_all(&root_dir).unwrap();
    fs::write(
        root_dir.join("main.crn"),
        r#"
let m = use { source = '../modules/dup' }
m { foo = 'x' }
"#,
    )
    .unwrap();

    let content = fs::read_to_string(root_dir.join("main.crn")).unwrap();
    let mut parsed = crate::parser::parse(&content, &ProviderContext::default()).unwrap();
    let err = resolve_modules(&mut parsed, &root_dir).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("DuplicateBinding") && msg.contains("foo"),
        "expected DuplicateBinding for `foo`; got {msg}"
    );
}
