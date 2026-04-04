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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
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
            cascading_updates[0].to.get_attr("vpc_id"),
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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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
        cascade_ref_hints: vec![],
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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        );

    let instance = Resource::new("ec2.instance", "my-instance")
        .with_binding("instance")
        .with_attribute(
            "subnet_id",
            Value::resource_ref("subnet".to_string(), "subnet_id".to_string(), vec![]),
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
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    // Anonymous subnet (no _binding) with a ResourceRef to the VPC
    let subnet = Resource::new("ec2.subnet", "my-subnet").with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
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
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
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
        cascade_ref_hints,
        ..
    } = &effects[1]
    {
        assert_eq!(id, &subnet_id);
        assert!(
            changed_create_only.contains(&"vpc_id".to_string()),
            "Subnet Replace should list vpc_id as a changed create-only attribute"
        );
        assert!(
            cascade_ref_hints.contains(&("vpc_id".to_string(), "vpc.vpc_id".to_string())),
            "Subnet Replace should have cascade_ref_hint for vpc_id → vpc.vpc_id, got: {:?}",
            cascade_ref_hints
        );
    } else {
        panic!(
            "Expected second effect to be Replace for subnet, got: {:?}",
            effects[1].kind()
        );
    }
}

#[test]
fn cascade_merges_with_existing_replace_direct_change_plus_cascade() {
    // Pattern 2: Direct change + cascade
    //
    // VPC cidr_block changes → VPC Replace (create_before_destroy)
    // Subnet availability_zone also changes (create-only) → Subnet Replace from differ
    // Subnet vpc_id (create-only) references VPC → cascade should ALSO add vpc_id
    //
    // Expected: Subnet Replace shows BOTH availability_zone AND vpc_id in changed_create_only

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // Unresolved resources (before ref resolution)
    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute("availability_zone", Value::String("us-east-1b".to_string()))
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
        "availability_zone".to_string(),
        Value::String("us-east-1a".to_string()),
    );
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Schema: vpc_id and availability_zone are both create-only on ec2.subnet
    let subnet_schema = ResourceSchema::new("ec2.subnet")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(
            AttributeSchema::new("availability_zone", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required());

    let mut schemas = HashMap::new();
    schemas.insert("ec2.subnet".to_string(), subnet_schema);

    // Build a plan:
    // - VPC Replace (create_before_destroy) due to cidr_block change
    // - Subnet Replace due to availability_zone change (direct change from differ)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Replace {
        id: subnet_id.clone(),
        from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
        to: subnet.clone().with_binding("subnet"),
        lifecycle: LifecycleConfig::default(),
        changed_create_only: vec!["availability_zone".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // Apply cascade
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // After cascading, the subnet Replace should have BOTH availability_zone AND vpc_id
    // in changed_create_only, because vpc_id is a create-only ref to the replaced VPC.
    let effects = plan.effects();
    assert_eq!(effects.len(), 2, "Should still have 2 effects");

    let subnet_effect = effects.iter().find(|e| *e.resource_id() == subnet_id);
    assert!(subnet_effect.is_some(), "Subnet effect should exist");

    if let Effect::Replace {
        changed_create_only,
        cascade_ref_hints,
        ..
    } = subnet_effect.unwrap()
    {
        assert!(
            changed_create_only.contains(&"availability_zone".to_string()),
            "changed_create_only should contain availability_zone (direct change), got: {:?}",
            changed_create_only
        );
        assert!(
            changed_create_only.contains(&"vpc_id".to_string()),
            "changed_create_only should contain vpc_id (cascade from VPC replace), got: {:?}",
            changed_create_only
        );
        assert!(
            cascade_ref_hints.contains(&("vpc_id".to_string(), "vpc.vpc_id".to_string())),
            "cascade_ref_hints should contain vpc_id → vpc.vpc_id, got: {:?}",
            cascade_ref_hints
        );
    } else {
        panic!("Expected subnet to be a Replace effect");
    }
}

#[test]
fn auto_detect_create_before_destroy_when_resource_has_dependents() {
    // Issue #947: When a resource being replaced is referenced by other resources,
    // automatically use create_before_destroy strategy instead of the default
    // delete-then-create.
    //
    // VPC cidr_block changes (create-only) → VPC Replace with default lifecycle
    // Subnet depends on VPC via vpc_id (ResourceRef)
    // Expected: VPC Replace should auto-detect create_before_destroy = true
    // because the subnet references it.

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // Unresolved resources (before ref resolution)
    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
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

    // Schema: cidr_block is create-only on ec2.vpc
    let vpc_schema = ResourceSchema::new("ec2.vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).create_only());
    let subnet_schema = ResourceSchema::new("ec2.subnet")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required());

    let mut schemas = HashMap::new();
    schemas.insert("ec2.vpc".to_string(), vpc_schema);
    schemas.insert("ec2.subnet".to_string(), subnet_schema);

    // Build a plan with Replace for VPC using DEFAULT lifecycle (no explicit CBD)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig::default(), // create_before_destroy = false (user didn't set it)
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // Apply cascade — this should auto-detect that VPC has dependents and
    // promote it to create_before_destroy
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // The VPC Replace should now have create_before_destroy = true
    // because the subnet references it
    let vpc_effect = plan
        .effects()
        .iter()
        .find(|e| *e.resource_id() == vpc_id)
        .expect("VPC Replace effect should exist");

    if let Effect::Replace { lifecycle, .. } = vpc_effect {
        assert!(
            lifecycle.create_before_destroy,
            "VPC Replace should have create_before_destroy auto-detected \
             because subnet references it, but got create_before_destroy = false"
        );
    } else {
        panic!("Expected Replace effect for VPC");
    }
}

#[test]
fn cascade_upgrades_update_to_replace_when_ref_is_create_only() {
    // Pattern 3: Direct non-create-only change + cascade
    //
    // VPC cidr_block changes → VPC Replace (create_before_destroy)
    // Subnet tags changes (NOT create-only) → Subnet Update from differ
    // Subnet vpc_id (create-only) references VPC → cascade should UPGRADE to Replace
    //
    // Expected: Subnet becomes Replace with vpc_id in changed_create_only

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    // Unresolved resources (before ref resolution)
    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute("tags", Value::String("new-tag".to_string()))
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
    subnet_attrs.insert("tags".to_string(), Value::String("old-tag".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Schema: vpc_id is create-only, tags is NOT create-only
    let subnet_schema = ResourceSchema::new("ec2.subnet")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("tags", AttributeType::String))
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required());

    let mut schemas = HashMap::new();
    schemas.insert("ec2.subnet".to_string(), subnet_schema);

    // Build a plan:
    // - VPC Replace (create_before_destroy) due to cidr_block change
    // - Subnet Update due to tags change (non-create-only, from differ)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Update {
        id: subnet_id.clone(),
        from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
        to: subnet.clone().with_binding("subnet"),
        changed_attributes: vec!["tags".to_string()],
    });

    // Apply cascade
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // After cascading, the subnet should be UPGRADED from Update to Replace,
    // because vpc_id is a create-only attribute referencing the replaced VPC.
    let effects = plan.effects();

    let subnet_effect = effects.iter().find(|e| *e.resource_id() == subnet_id);
    assert!(subnet_effect.is_some(), "Subnet effect should exist");

    match subnet_effect.unwrap() {
        Effect::Replace {
            changed_create_only,
            ..
        } => {
            assert!(
                changed_create_only.contains(&"vpc_id".to_string()),
                "Subnet Replace should list vpc_id as changed create-only (cascade from VPC replace), got: {:?}",
                changed_create_only
            );
        }
        other => {
            panic!(
                "Expected subnet to be upgraded to Replace, but got {:?}",
                other.kind()
            );
        }
    }
}

#[test]
fn cascade_prevent_destroy_blocks_promotion_to_replace() {
    // When resource A is being replaced and resource B depends on A
    // via a create-only attribute, B would normally be promoted to Replace.
    // But if B has prevent_destroy: true, it should generate a PlanError instead.

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let mut subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()));
    subnet.lifecycle.prevent_destroy = true;

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
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Schema: vpc_id is create-only on ec2.subnet
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
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // Apply cascade
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // The subnet should NOT be promoted to Replace because it has prevent_destroy.
    // Instead, a PlanError should be generated.
    assert!(
        plan.has_errors(),
        "Expected PlanError for subnet with prevent_destroy, but got no errors"
    );

    let errors = plan.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].resource_id, subnet_id);
    assert!(
        errors[0].message.contains("prevent_destroy"),
        "Error message should mention prevent_destroy, got: {}",
        errors[0].message
    );

    // The subnet should NOT appear as a Replace effect in the plan
    let subnet_effects: Vec<_> = plan
        .effects()
        .iter()
        .filter(|e| *e.resource_id() == subnet_id)
        .collect();
    assert!(
        subnet_effects.is_empty(),
        "Subnet should not have any effect when prevent_destroy blocks promotion"
    );
}

#[test]
fn cascade_prevent_destroy_blocks_merge_upgrade_to_replace() {
    // When resource B already has an Update effect in the plan and would be
    // upgraded to Replace via cascade (merge path), prevent_destroy should block it.

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let vpc_id = ResourceId::new("ec2.vpc", "my-vpc");
    let subnet_id = ResourceId::new("ec2.subnet", "my-subnet");

    let vpc = Resource::new("ec2.vpc", "my-vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let mut subnet = Resource::new("ec2.subnet", "my-subnet")
        .with_binding("subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute("tags", Value::String("new-tag".to_string()))
        .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()));
    subnet.lifecycle.prevent_destroy = true;

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
    subnet_attrs.insert("tags".to_string(), Value::String("old-tag".to_string()));
    subnet_attrs.insert(
        "cidr_block".to_string(),
        Value::String("10.1.1.0/24".to_string()),
    );
    current_states.insert(
        subnet_id.clone(),
        State::existing(subnet_id.clone(), subnet_attrs).with_identifier("subnet-123"),
    );

    // Schema: vpc_id is create-only, tags is NOT
    let subnet_schema = ResourceSchema::new("ec2.subnet")
        .attribute(
            AttributeSchema::new("vpc_id", AttributeType::String)
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("tags", AttributeType::String))
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String).required());

    let mut schemas = HashMap::new();
    schemas.insert("ec2.subnet".to_string(), subnet_schema);

    // Build a plan with Replace for VPC and Update for subnet (tags changed)
    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(current_states.get(&vpc_id).unwrap().clone()),
        to: vpc.clone().with_binding("vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Update {
        id: subnet_id.clone(),
        from: Box::new(current_states.get(&subnet_id).unwrap().clone()),
        to: subnet.clone().with_binding("subnet"),
        changed_attributes: vec!["tags".to_string()],
    });

    // Apply cascade
    cascade_dependent_updates(&mut plan, &unresolved_resources, &current_states, &schemas);

    // The subnet should NOT be upgraded to Replace because it has prevent_destroy.
    // A PlanError should be generated instead.
    assert!(
        plan.has_errors(),
        "Expected PlanError for subnet with prevent_destroy on merge upgrade"
    );

    let errors = plan.errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].resource_id, subnet_id);
    assert!(
        errors[0].message.contains("prevent_destroy"),
        "Error message should mention prevent_destroy, got: {}",
        errors[0].message
    );
}
