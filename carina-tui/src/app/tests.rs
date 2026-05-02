use super::*;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::value::format_value;

#[test]
fn app_from_empty_plan() {
    let plan = Plan::new();
    let app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.nodes.len(), 0);
    assert_eq!(app.selected, 0);
}

#[test]
fn app_from_plan_with_effects() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "my-bucket")));
    plan.add(Effect::Delete {
        id: ResourceId::new("s3.Bucket", "old-bucket"),
        identifier: "old-bucket-id".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: None,
        dependencies: HashSet::new(),
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.nodes.len(), 2);
    assert_eq!(app.nodes[0].symbol, "+");
    assert_eq!(app.nodes[0].kind, EffectKind::Create);
    assert_eq!(app.nodes[1].symbol, "-");
    assert_eq!(app.nodes[1].kind, EffectKind::Delete);
}

#[test]
fn navigation() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
    plan.add(Effect::Create(Resource::new("s3.Bucket", "b")));
    plan.add(Effect::Create(Resource::new("s3.Bucket", "c")));

    let mut app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.selected, 0);

    app.move_down();
    assert_eq!(app.selected, 1);

    app.move_down();
    assert_eq!(app.selected, 2);

    // Should not go past end
    app.move_down();
    assert_eq!(app.selected, 2);

    app.move_up();
    assert_eq!(app.selected, 1);

    app.move_up();
    assert_eq!(app.selected, 0);

    // Should not go before start
    app.move_up();
    assert_eq!(app.selected, 0);
}

#[test]
fn update_effect_has_detail_rows() {
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ResourceId::new("s3.Bucket", "my-bucket"),
        from: Box::new(State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [(
                "versioning".to_string(),
                Value::String("Disabled".to_string()),
            )]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("versioning", Value::String("Enabled".to_string())),
        changed_attributes: vec!["versioning".to_string()],
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.nodes[0].kind, EffectKind::Update);
    // Should have a Changed detail row for versioning
    assert!(
        app.nodes[0]
            .detail_rows
            .iter()
            .any(|r| matches!(r, DetailRow::Changed { key, .. } if key == "versioning"))
    );
}

#[test]
fn internal_attributes_filtered() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("name", Value::String("test".to_string()))
            .with_binding("my_bucket")
            .with_module_source(carina_core::resource::ModuleSource::module("web", "web")),
    ));

    let app = App::new(&plan, &SchemaRegistry::new());
    // Only "name" should appear (not _binding or _module)
    let attr_rows: Vec<_> = app.nodes[0]
        .detail_rows
        .iter()
        .filter(|r| matches!(r, DetailRow::Attribute { .. }))
        .collect();
    assert_eq!(attr_rows.len(), 1);
    assert!(matches!(&attr_rows[0], DetailRow::Attribute { key, .. } if key == "name"));
}

#[test]
fn format_value_display() {
    assert_eq!(
        format_value(&Value::String("hello".to_string())),
        "\"hello\""
    );
    assert_eq!(format_value(&Value::Int(42)), "42");
    assert_eq!(format_value(&Value::Bool(true)), "true");
    assert_eq!(
        format_value(&Value::List(vec![Value::Int(1), Value::Int(2)])),
        "[1, 2]"
    );
}

#[test]
fn replace_effect_symbols() {
    let mut plan = Plan::new();
    let from = Box::new(State::existing(
        ResourceId::new("ec2.Vpc", "my-vpc"),
        [("cidr".to_string(), Value::String("10.0.0.0/16".to_string()))]
            .into_iter()
            .collect(),
    ));

    // create_before_destroy = true -> "+/-"
    plan.add(Effect::Replace {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        from: from.clone(),
        to: Resource::new("ec2.Vpc", "my-vpc"),
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // create_before_destroy = false -> "-/+"
    plan.add(Effect::Replace {
        id: ResourceId::new("ec2.Vpc", "my-vpc2"),
        from,
        to: Resource::new("ec2.Vpc", "my-vpc2"),
        lifecycle: LifecycleConfig::default(),
        changed_create_only: vec!["cidr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.nodes[0].symbol, "+/-");
    assert_eq!(app.nodes[1].symbol, "-/+");
}

#[test]
fn tree_structure_with_dependencies() {
    // Create a plan where subnet depends on vpc via ResourceRef
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_binding("subnet")
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));

    let app = App::new(&plan, &SchemaRegistry::new());

    // VPC should be root (depth 0) with subnet as child
    assert_eq!(app.nodes[0].depth, 0);
    assert!(app.nodes[0].parent.is_none());
    assert_eq!(app.nodes[0].children, vec![1]);

    // Subnet should be child (depth 1) with VPC as parent
    assert_eq!(app.nodes[1].depth, 1);
    assert_eq!(app.nodes[1].parent, Some(0));
    assert!(app.nodes[1].children.is_empty());
}

#[test]
fn selected_node_returns_correct_node() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("name", Value::String("test".to_string())),
    ));

    let app = App::new(&plan, &SchemaRegistry::new());
    let node = app.selected_node().unwrap();
    assert_eq!(node.kind, EffectKind::Create);
    // Detail rows should contain the name attribute
    assert!(!node.detail_rows.is_empty());
}

#[test]
fn toggle_focus_switches_panels() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
    let mut app = App::new(&plan, &SchemaRegistry::new());

    assert_eq!(app.focused_panel, FocusedPanel::Tree);
    app.toggle_focus();
    assert_eq!(app.focused_panel, FocusedPanel::Detail);
    app.toggle_focus();
    assert_eq!(app.focused_panel, FocusedPanel::Tree);
}

#[test]
fn detail_scroll_up_down() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
    let mut app = App::new(&plan, &SchemaRegistry::new());

    assert_eq!(app.detail_scroll, 0);
    app.detail_scroll_down();
    assert_eq!(app.detail_scroll, 1);
    app.detail_scroll_down();
    assert_eq!(app.detail_scroll, 2);
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 1);
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 0);
    // Should not underflow
    app.detail_scroll_up();
    assert_eq!(app.detail_scroll, 0);
}

#[test]
fn detail_scroll_resets_on_navigation() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
    plan.add(Effect::Create(Resource::new("s3.Bucket", "b")));
    let mut app = App::new(&plan, &SchemaRegistry::new());

    app.detail_scroll = 5;
    app.move_down();
    assert_eq!(app.detail_scroll, 0);

    app.detail_scroll = 3;
    app.move_up();
    assert_eq!(app.detail_scroll, 0);
}

#[test]
fn tree_scroll_cursor_moves_within_visible_area_before_scrolling() {
    // Create a plan with 10 items
    let mut plan = Plan::new();
    for i in 0..10 {
        plan.add(Effect::Create(Resource::new(
            "s3.Bucket",
            format!("bucket-{}", i),
        )));
    }
    let mut app = App::new(&plan, &SchemaRegistry::new());
    // Simulate a visible area of 5 items
    app.tree_area_height = 5;

    // Move down from 0 to 4: no scrolling needed (items 0-4 fit in view)
    for i in 1..=4 {
        app.move_down();
        assert_eq!(app.selected, i);
        assert_eq!(app.tree_scroll_offset, 0, "should not scroll at item {}", i);
    }

    // Move down to 5: now scroll offset should advance to 1
    app.move_down();
    assert_eq!(app.selected, 5);
    assert_eq!(app.tree_scroll_offset, 1);

    // Move down to 9
    for _ in 6..=9 {
        app.move_down();
    }
    assert_eq!(app.selected, 9);
    assert_eq!(app.tree_scroll_offset, 5); // items 5-9 visible

    // Now move up: cursor moves within visible area without scrolling
    app.move_up(); // selected=8, still in view (5-9)
    assert_eq!(app.selected, 8);
    assert_eq!(app.tree_scroll_offset, 5);

    app.move_up(); // selected=7
    assert_eq!(app.selected, 7);
    assert_eq!(app.tree_scroll_offset, 5);

    app.move_up(); // selected=6
    assert_eq!(app.selected, 6);
    assert_eq!(app.tree_scroll_offset, 5);

    app.move_up(); // selected=5, still at top of view
    assert_eq!(app.selected, 5);
    assert_eq!(app.tree_scroll_offset, 5);

    // Move up past the top of visible area: scroll offset decreases
    app.move_up(); // selected=4, scroll_offset=4
    assert_eq!(app.selected, 4);
    assert_eq!(app.tree_scroll_offset, 4);

    app.move_up(); // selected=3, scroll_offset=3
    assert_eq!(app.selected, 3);
    assert_eq!(app.tree_scroll_offset, 3);
}

#[test]
fn tree_scroll_zero_height_does_not_scroll_on_move_down() {
    // When tree_area_height is 0 (before first render), move_down should not scroll
    let mut plan = Plan::new();
    plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
    plan.add(Effect::Create(Resource::new("s3.Bucket", "b")));
    let mut app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.tree_area_height, 0);

    app.move_down();
    assert_eq!(app.selected, 1);
    assert_eq!(app.tree_scroll_offset, 0);
}

/// Helper to build a plan with vpc -> subnet dependency tree for filter tests.
fn make_tree_plan() -> Plan {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("ec2.Vpc", "my-vpc")
            .with_binding("vpc")
            .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
    ));
    plan.add(Effect::Create(
        Resource::new("ec2.Subnet", "my-subnet")
            .with_binding("subnet")
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));
    plan.add(Effect::Create(
        Resource::new("s3.Bucket", "my-bucket").with_binding("bucket"),
    ));
    plan
}

#[test]
fn filter_mode_hides_non_matching_nodes() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());

    // Before search, all 3 nodes visible
    assert_eq!(app.visible_count(), 3);

    // Search for "subnet" - should show subnet + its parent vpc
    app.search_query = "subnet".to_string();
    app.update_search_matches();

    let visible = app.visible_nodes();
    assert_eq!(visible.len(), 2); // vpc (ancestor) + subnet (match)

    // The s3.bucket should not be visible
    for &idx in &visible {
        assert_ne!(app.nodes[idx].resource_type, "s3.Bucket");
    }
}

#[test]
fn filter_mode_ancestor_shown_dimmed() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());

    app.search_query = "subnet".to_string();
    app.update_search_matches();

    let visible = app.visible_nodes();
    // vpc is ancestor-only (dimmed)
    let vpc_idx = visible
        .iter()
        .find(|&&idx| app.nodes[idx].resource_type == "ec2.Vpc")
        .unwrap();
    assert!(app.is_ancestor_only(*vpc_idx));

    // subnet is a match (not dimmed)
    let subnet_idx = visible
        .iter()
        .find(|&&idx| app.nodes[idx].resource_type == "ec2.Subnet")
        .unwrap();
    assert!(!app.is_ancestor_only(*subnet_idx));
}

#[test]
fn filter_mode_clear_query_restores_all() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());

    app.search_query = "subnet".to_string();
    app.update_search_matches();
    assert_eq!(app.visible_count(), 2);

    // Clear query
    app.search_query.clear();
    app.update_search_matches();
    assert_eq!(app.visible_count(), 3);
}

#[test]
fn filter_mode_no_matches_shows_all() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());

    app.search_query = "zzz_nonexistent".to_string();
    app.update_search_matches();

    // When nothing matches, show all nodes (don't hide everything)
    assert_eq!(app.visible_count(), 3);
}

#[test]
fn filter_mode_search_matches_are_non_ancestor_indices() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());

    app.search_query = "subnet".to_string();
    app.update_search_matches();

    // search_matches should contain only the visible index of the subnet node
    assert_eq!(app.search_matches.len(), 1);
    let visible = app.visible_nodes();
    let match_vis_idx = app.search_matches[0];
    let match_node_idx = visible[match_vis_idx];
    assert_eq!(app.nodes[match_node_idx].resource_type, "ec2.Subnet");
}

#[test]
fn tab_complete_basic() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "sub".to_string();

    app.tab_complete();

    // "sub" matches both "ec2.Subnet" (resource type) and "subnet" (binding);
    // sorted alphabetically, "ec2.Subnet" comes first
    assert_eq!(app.search_query, "ec2.Subnet");
}

#[test]
fn tab_complete_cycles_candidates() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "ec2".to_string();

    // First tab: should complete to first candidate starting with "ec2"
    app.tab_complete();
    let first = app.search_query.clone();

    // Second tab: should cycle to next candidate
    app.tab_complete();
    let second = app.search_query.clone();

    // There are two resource types: ec2.subnet and ec2.vpc
    assert!(first.starts_with("ec2"));
    assert!(second.starts_with("ec2"));
    assert_ne!(first, second);
}

#[test]
fn tab_complete_no_match() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "zzz".to_string();

    app.tab_complete();

    // No candidates match, query unchanged
    assert_eq!(app.search_query, "zzz");
}

#[test]
fn tab_complete_empty_query() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = String::new();

    app.tab_complete();

    // Empty query should not complete
    assert!(app.search_query.is_empty());
}

#[test]
fn tab_complete_case_insensitive() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "SUB".to_string();

    app.tab_complete();

    // "SUB" matches "ec2.Subnet" and "subnet" case-insensitively;
    // sorted alphabetically, "ec2.Subnet" comes first
    assert_eq!(app.search_query, "ec2.Subnet");
}

#[test]
fn tab_complete_matches_middle_of_word() {
    let plan = make_tree_plan();
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "net".to_string();

    app.tab_complete();

    // "net" matches "ec2.Subnet" (resource type) and "subnet" (binding)
    // via contains; sorted alphabetically, "ec2.Subnet" comes first
    assert_eq!(app.search_query, "ec2.Subnet");
}

#[test]
fn tab_complete_with_provider_prefix() {
    // Resource types with provider prefix (e.g., "awscc.ec2.Vpc")
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::with_provider("awscc", "ec2.Vpc", "my-vpc").with_binding("vpc"),
    ));
    plan.add(Effect::Create(
        Resource::with_provider("awscc", "ec2.Subnet", "my-subnet").with_binding("subnet"),
    ));
    let mut app = App::new(&plan, &SchemaRegistry::new());
    app.search_active = true;
    app.search_query = "ec".to_string();

    app.tab_complete();

    // Should match resource types containing "ec" even with provider prefix
    assert!(
        app.search_query.contains("ec2"),
        "expected query to contain 'ec2', got '{}'",
        app.search_query
    );
}

#[test]
fn format_value_resolves_dsl_enum_identifiers() {
    // 5-part DSL enum: should resolve to quoted value
    assert_eq!(
        format_value(&Value::String(
            "awscc.ec2.vpc_endpoint.VpcEndpointType.Interface".to_string()
        )),
        "\"Interface\""
    );

    // 4-part DSL enum
    assert_eq!(
        format_value(&Value::String(
            "aws.s3.VersioningStatus.Enabled".to_string()
        )),
        "\"Enabled\""
    );

    // 3-part DSL enum: namespace stripped, value returned as-is
    assert_eq!(
        format_value(&Value::String("aws.Region.ap_northeast_1".to_string())),
        "\"ap_northeast_1\""
    );

    // Regular string should be quoted as-is
    assert_eq!(
        format_value(&Value::String("my-bucket".to_string())),
        "\"my-bucket\""
    );

    // ResourceRef should NOT be resolved (not a DSL enum)
    assert_eq!(
        format_value(&Value::resource_ref(
            "vpc".to_string(),
            "vpc_id".to_string(),
            vec![]
        )),
        "vpc.vpc_id"
    );
}

#[test]
fn create_effect_attributes_resolve_enum_values() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        Resource::new("ec2.vpc_endpoint", "my-endpoint")
            .with_attribute(
                "vpc_endpoint_type",
                Value::String("awscc.ec2.vpc_endpoint.VpcEndpointType.Interface".to_string()),
            )
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            ),
    ));

    let app = App::new(&plan, &SchemaRegistry::new());
    let node = &app.nodes[0];

    // Enum value should be resolved in detail rows
    let enum_row = node
        .detail_rows
        .iter()
        .find(|r| matches!(r, DetailRow::Attribute { key, .. } if key == "vpc_endpoint_type"))
        .expect("vpc_endpoint_type detail row should exist");
    assert!(matches!(enum_row, DetailRow::Attribute { value, .. } if value == "\"Interface\""));

    // ResourceRef should remain unresolved
    let ref_row = node
        .detail_rows
        .iter()
        .find(|r| matches!(r, DetailRow::Attribute { key, .. } if key == "vpc_id"))
        .expect("vpc_id detail row should exist");
    assert!(matches!(ref_row, DetailRow::Attribute { value, .. } if value == "vpc.vpc_id"));
}

#[test]
fn move_suppressed_when_update_exists_for_same_target() {
    let mut plan = Plan::new();
    // Move from old name to new name
    plan.add(Effect::Move {
        from: ResourceId::new("s3.Bucket", "old-name"),
        to: ResourceId::new("s3.Bucket", "new-name"),
    });
    // Update for the same target
    plan.add(Effect::Update {
        id: ResourceId::new("s3.Bucket", "new-name"),
        from: Box::new(State::existing(
            ResourceId::new("s3.Bucket", "new-name"),
            [(
                "versioning".to_string(),
                Value::String("Disabled".to_string()),
            )]
            .into_iter()
            .collect(),
        )),
        to: Resource::new("s3.Bucket", "new-name")
            .with_attribute("versioning", Value::String("Enabled".to_string())),
        changed_attributes: vec!["versioning".to_string()],
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    // Move should be suppressed; only the Update node should remain
    assert_eq!(app.nodes.len(), 1);
    assert_eq!(app.nodes[0].kind, EffectKind::Update);
}

#[test]
fn move_suppressed_when_replace_exists_for_same_target() {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: ResourceId::new("ec2.Vpc", "old-vpc"),
        to: ResourceId::new("ec2.Vpc", "new-vpc"),
    });
    plan.add(Effect::Replace {
        id: ResourceId::new("ec2.Vpc", "new-vpc"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Vpc", "new-vpc"),
            [("cidr".to_string(), Value::String("10.0.0.0/16".to_string()))]
                .into_iter()
                .collect(),
        )),
        to: Resource::new("ec2.Vpc", "new-vpc"),
        lifecycle: LifecycleConfig::default(),
        changed_create_only: vec!["cidr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    assert_eq!(app.nodes.len(), 1);
    assert_eq!(app.nodes[0].symbol, "-/+");
}

#[test]
fn pure_move_not_suppressed() {
    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: ResourceId::new("s3.Bucket", "old-name"),
        to: ResourceId::new("s3.Bucket", "new-name"),
    });

    let app = App::new(&plan, &SchemaRegistry::new());
    // Pure move (no Update/Replace for same target) should be kept
    assert_eq!(app.nodes.len(), 1);
    assert_eq!(app.nodes[0].symbol, "->");
}
