use super::*;

#[test]
fn cascade_dependent_updates_adds_update_for_dependent() {
    // VPC is being replaced with create_before_destroy
    // Subnet depends on VPC via ResourceRef
    // cascade_dependent_updates should add a CascadingUpdate to the Replace

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // Unresolved resources (before ref resolution)
    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        )
        .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()));

    let unresolved_resources = vec![vpc.clone(), subnet.clone()];

    // Current states
    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );

    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Build a plan with Replace for VPC (create_before_destroy)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc
            .clone()
            .with_attribute("_binding", Value::String("vpc".to_string())),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });

    // Apply cascade
    let schemas = HashMap::new();
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // Verify the Replace effect now has a cascading update for the subnet
    let effects = plan.effects();
    assert_eq!(effects.len(), 1);
    if let Effect::Replace {
        cascading_updates, ..
    } = &effects[0]
    {
        assert_eq!(cascading_updates.len(), 1);
        assert_eq!(cascading_updates[0].id, subnet_id);
        // The `to` should have unresolved ResourceRef
        assert!(matches!(
            cascading_updates[0].to.attributes.get("vpc_id"),
            Some(Value::ResourceRef { .. })
        ));
        // The `from` should have the current state
        assert_eq!(
            cascading_updates[0].from.attributes.get("vpc_id"),
            Some(&Value::String("vpc-old".to_string()))
        );
    } else {
        panic!("Expected Replace effect");
    }
}

#[test]
fn cascade_skips_resources_already_in_plan() {
    // If the dependent resource already has its own effect (e.g., Update),
    // cascade should not add a duplicate

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        )
        .with_attribute("cidr_block", Value::String("10.1.2.0/24".to_string()));

    let unresolved_resources = vec![vpc.clone(), subnet.clone()];

    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );
    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs.clone()).with_identifier("subnet-123"),
    );

    // Plan with both Replace for VPC and Update for subnet
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone(),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });
    plan.add(Effect::Update {
        id: subnet_id.clone(),
        from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
        to: subnet.clone(),
        changed_attributes: vec!["cidr_block".to_string()],
    });

    let schemas = HashMap::new();
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // The Replace should have NO cascading updates since subnet already has an Update
    if let Effect::Replace {
        cascading_updates, ..
    } = &plan.effects()[0]
    {
        assert!(
            cascading_updates.is_empty(),
            "Expected no cascading updates when dependent already has an effect"
        );
    } else {
        panic!("Expected Replace effect");
    }
}

#[test]
fn cascade_no_op_without_create_before_destroy() {
    // Replace without create_before_destroy should not trigger cascading

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        );

    let unresolved_resources = vec![vpc.clone(), subnet.clone()];

    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone(),
        lifecycle: LifecycleConfig::default(), // create_before_destroy = false
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });

    let schemas = HashMap::new();
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    if let Effect::Replace {
        cascading_updates, ..
    } = &plan.effects()[0]
    {
        assert!(cascading_updates.is_empty());
    }
}

#[test]
fn cascade_transitive_dependencies() {
    // VPC → Subnet → Instance (transitive chain)
    // Only Subnet directly depends on VPC, so only Subnet gets cascading update

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");
    let instance_id = ResourceId::new("ec2.instance", "my-instance");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        );

    let instance = Resource::new("ec2.instance", "my-instance")
        .with_attribute("_binding", Value::String("instance".to_string()))
        .with_attribute(
            "subnet_id",
            Value::ResourceRef {
                binding_name: "subnet".to_string(),
                attribute_name: "subnet_id".to_string(),
            },
        );

    let unresolved_resources = vec![vpc.clone(), subnet.clone(), instance.clone()];

    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );
    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    subnet_attrs.insert(
        "subnet_id".to_string(),
        Value::String("subnet-123".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );
    let mut instance_attrs = HashMap::new();
    instance_attrs.insert(
        "subnet_id".to_string(),
        Value::String("subnet-123".to_string()),
    );
    current_states.insert(
        instance_id.clone(),
        State::existing(instance_id.clone(), instance_attrs).with_identifier("i-123"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone(),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });

    let schemas = HashMap::new();
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // Only subnet directly depends on VPC, so only subnet gets cascading update
    // Instance depends on subnet, not VPC directly
    if let Effect::Replace {
        cascading_updates, ..
    } = &plan.effects()[0]
    {
        assert_eq!(cascading_updates.len(), 1);
        assert_eq!(cascading_updates[0].id, subnet_id);
    } else {
        panic!("Expected Replace effect");
    }
}

#[test]
fn cascade_anonymous_resource_dependent() {
    // Anonymous resource (no _binding) that depends on a replaced resource
    // should still get a cascading update

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    // Anonymous subnet (no _binding) with a ResourceRef to the VPC
    let subnet = Resource::new("ec2.subnet", "my-subnet").with_attribute(
        "vpc_id",
        Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "vpc_id".to_string(),
        },
    );

    let unresolved_resources = vec![vpc.clone(), subnet.clone()];

    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );

    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone(),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });

    let schemas = HashMap::new();
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    if let Effect::Replace {
        cascading_updates, ..
    } = &plan.effects()[0]
    {
        assert_eq!(
            cascading_updates.len(),
            1,
            "Anonymous resource should get cascading update"
        );
        assert_eq!(cascading_updates[0].id, subnet_id);
    } else {
        panic!("Expected Replace effect for anonymous resource test");
    }
}

#[test]
fn cascade_generates_replace_when_dependent_attribute_is_create_only() {
    // When VPC is replaced, subnet's vpc_id changes.
    // Since vpc_id is a create-only attribute on ec2.subnet,
    // the subnet cannot be updated in-place — it must also be replaced.
    // Currently, cascading updates always generate CascadingUpdate (Update),
    // but this test asserts the correct behavior: a Replace effect for the subnet.

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // Unresolved resources (before ref resolution)
    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_attribute("_binding", Value::String("vpc".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_attribute("_binding", Value::String("subnet".to_string()))
        .with_attribute(
            "vpc_id",
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "vpc_id".to_string(),
            },
        )
        .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()));

    let unresolved_resources = vec![vpc.clone(), subnet.clone()];

    // Current states
    let mut current_states = HashMap::new();
    let mut vpc_attrs = HashMap::new();
    vpc_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.0.0.0/16".to_string()),
    );
    vpc_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    current_states.insert(
        vpc_id.clone(),
        State::existing(vpc_id.clone(), vpc_attrs).with_identifier("vpc-old"),
    );

    let mut subnet_attrs = HashMap::new();
    subnet_attrs.insert("vpc_id".to_string(), Value::String("vpc-old".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Schema for ec2.subnet with vpc_id as create-only
    let subnet_schema = ResourceSchema::new("ec2.subnet")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required());

    let mut schemas = HashMap::new();
    schemas.insert("ec2.subnet".to_string(), subnet_schema);

    // Build a plan with Replace for VPC (create_before_destroy)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc
            .clone()
            .with_attribute("_binding", Value::String("vpc".to_string())),
        lifecycle: LifecycleConfig {
            force_delete: false,
            create_before_destroy: true,
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
    });

    // Apply cascade with schemas so it can detect create-only attributes
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // After cascading, the subnet should appear as a separate Replace effect in the plan,
    // NOT as a CascadingUpdate inside the VPC's Replace effect.
    // This is because vpc_id is create-only on subnet — an in-place update is impossible.
    let effects = plan.effects();

    // We expect 2 effects: Replace for VPC and Replace for subnet
    assert_eq!(
        effects.len(),
        2,
        "Expected 2 effects (VPC Replace + subnet Replace), got {}: {:?}",
        effects.len(),
        effects
            .iter()
            .map(|e| format!("{} {}", e.kind(), e.resource_id()))
            .collect::<Vec<_>>()
    );

    // The VPC Replace should have no cascading updates (subnet is promoted to its own Replace)
    if let Effect::Replace {
        id,
        cascading_updates,
        ..
    } = &effects[0]
    {
        assert_eq!(id, &vpc_id);
        assert!(
            cascading_updates.is_empty(),
            "VPC Replace should have no cascading updates; subnet should be a separate Replace"
        );
    } else {
        panic!("Expected first effect to be Replace for VPC");
    }

    // The subnet should be a Replace effect (not an Update)
    if let Effect::Replace {
        id,
        changed_create_only,
        ..
    } = &effects[1]
    {
        assert_eq!(id, &subnet_id);
        assert!(
            changed_create_only.contains(&"vpc_id".to_string()),
            "Subnet Replace should list vpc_id as a changed create-only attribute"
        );
    } else {
        panic!(
            "Expected second effect to be Replace for subnet, got: {:?}",
            effects[1].kind()
        );
    }
}
