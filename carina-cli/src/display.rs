use std::collections::{HashMap, HashSet, VecDeque};

use colored::Colorize;

use carina_core::deps::get_resource_dependencies;
use carina_core::effect::Effect;
use carina_core::plan::Plan;
use carina_core::resource::Value;
use carina_core::value::{format_value, format_value_with_key, is_list_of_maps, map_similarity};

/// Build a single-parent tree from the dependency graph.
///
/// Each resource is assigned to exactly one parent (or is a root). When a
/// resource depends on multiple resources in the plan, it is placed under the
/// shallowest (most ancestral) dependency. Ties are broken by
/// (resource_type, binding_name) for determinism.
///
/// Returns (roots, dependents) where roots are sorted and each parent's
/// children are sorted by (resource_type, binding_name).
fn build_single_parent_tree(
    plan: &Plan,
    binding_to_effect: &HashMap<String, usize>,
    effect_deps: &HashMap<usize, HashSet<String>>,
    effect_bindings: &HashMap<usize, String>,
    effect_types: &HashMap<usize, String>,
) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
    let effect_binding_set: HashSet<&str> = binding_to_effect.keys().map(|s| s.as_str()).collect();

    // Step 1: Build the full reverse dependency map (all parents)
    let mut all_dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        all_dependents.insert(idx, Vec::new());
    }
    for (idx, deps) in effect_deps {
        for dep in deps {
            if let Some(&dep_idx) = binding_to_effect.get(dep) {
                all_dependents.entry(dep_idx).or_default().push(*idx);
            }
        }
    }

    // Step 2: Identify initial roots (no deps in plan)
    let mut initial_roots: Vec<usize> = Vec::new();
    for (idx, deps) in effect_deps {
        let has_dep_in_plan = deps.iter().any(|d| binding_to_effect.contains_key(d));
        if !has_dep_in_plan {
            initial_roots.push(*idx);
        }
    }
    initial_roots.sort();

    // Step 3: Nest no-dep resources under their first dependent (Issue #928)
    // A no-dep resource can be nested only if every dependent has at least one
    // other dependency in the plan (besides this resource).
    let mut nested_under_dependent: HashSet<usize> = HashSet::new();
    let sort_key = |idx: &usize| -> (String, String) {
        let rtype = effect_types.get(idx).cloned().unwrap_or_default();
        let binding = effect_bindings.get(idx).cloned().unwrap_or_default();
        (rtype, binding)
    };
    for &idx in &initial_roots {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        children.sort_by_key(|a| sort_key(a));
        if !children.is_empty() {
            let binding_of_idx = effect_bindings.get(&idx).map(|s| s.as_str());
            let all_dependents_have_other_deps = children.iter().all(|&child_idx| {
                effect_deps.get(&child_idx).is_some_and(|child_deps| {
                    child_deps.iter().any(|d| {
                        effect_binding_set.contains(d.as_str())
                            && Some(d.as_str()) != binding_of_idx
                    })
                })
            });
            if all_dependents_have_other_deps {
                nested_under_dependent.insert(idx);
            }
        }
    }

    // Step 4: Compute final roots
    let roots: Vec<usize> = initial_roots
        .iter()
        .filter(|idx| !nested_under_dependent.contains(idx))
        .cloned()
        .collect();

    // Step 5: Compute depth for each resource via BFS from roots
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &root in &roots {
        depth.insert(root, 0);
        queue.push_back(root);
    }
    // Also set depth for nested-under-dependent resources (they will be
    // children of some non-root resource, but we need them reachable)
    while let Some(node) = queue.pop_front() {
        let d = depth[&node];
        if let Some(children) = all_dependents.get(&node) {
            for &child in children {
                if let std::collections::hash_map::Entry::Vacant(e) = depth.entry(child) {
                    e.insert(d + 1);
                    queue.push_back(child);
                }
            }
        }
    }

    // Step 6: For each non-root resource, select a single parent:
    // the dependency with the shallowest depth (most ancestral).
    // For nested-under-dependent resources, their parent is the first
    // dependent (by sort order).
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        dependents.insert(idx, Vec::new());
    }

    for (idx, deps) in effect_deps {
        if roots.contains(idx) || nested_under_dependent.contains(idx) {
            continue;
        }
        // Find deps that are in the plan
        let mut parent_candidates: Vec<usize> = deps
            .iter()
            .filter_map(|d| binding_to_effect.get(d).cloned())
            .collect();
        if parent_candidates.is_empty() {
            continue;
        }
        // Pick the shallowest parent; break ties by (resource_type, binding_name)
        parent_candidates.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        let parent = parent_candidates[0];
        dependents.entry(parent).or_default().push(*idx);
    }

    // Add nested-under-dependent resources as children of their first dependent
    for &idx in &nested_under_dependent {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        children.sort_by_key(|a| sort_key(a));
        if let Some(&first_dependent) = children.first() {
            dependents.entry(first_dependent).or_default().push(idx);
        }
    }

    // Step 7: Sort each parent's children by (resource_type, binding_name)
    for children in dependents.values_mut() {
        children.sort_by_key(|a| sort_key(a));
    }

    // Also sort roots by (resource_type, binding_name)
    let mut sorted_roots = roots;
    sorted_roots.sort_by_key(|a| sort_key(a));

    (sorted_roots, dependents)
}

pub fn print_plan(plan: &Plan) {
    if plan.is_empty() {
        println!("{}", "No changes. Infrastructure is up-to-date.".green());
        return;
    }

    // Build dependency graph from effects
    let mut binding_to_effect: HashMap<String, usize> = HashMap::new();
    let mut effect_deps: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut effect_bindings: HashMap<usize, String> = HashMap::new();

    for (idx, effect) in plan.effects().iter().enumerate() {
        let (resource, deps) = match effect {
            Effect::Create(r) => (Some(r), get_resource_dependencies(r)),
            Effect::Update { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Replace { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Read { resource } => (Some(resource), get_resource_dependencies(resource)),
            Effect::Delete { .. } => (None, HashSet::new()),
        };

        if let Some(r) = resource {
            let binding = r
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| r.id.to_string());
            binding_to_effect.insert(binding.clone(), idx);
            effect_bindings.insert(idx, binding);
        }
        effect_deps.insert(idx, deps);
    }

    // Build effect_types map for sorting
    let mut effect_types: HashMap<usize, String> = HashMap::new();
    for (idx, effect) in plan.effects().iter().enumerate() {
        let resource = match effect {
            Effect::Create(r) => Some(r),
            Effect::Update { to, .. } => Some(to),
            Effect::Replace { to, .. } => Some(to),
            Effect::Read { resource } => Some(resource),
            Effect::Delete { .. } => None,
        };
        if let Some(r) = resource {
            effect_types.insert(idx, r.id.resource_type.clone());
        }
    }

    // Build the single-parent tree with sorted siblings
    let (roots, dependents) = build_single_parent_tree(
        plan,
        &binding_to_effect,
        &effect_deps,
        &effect_bindings,
        &effect_types,
    );

    println!("{}", "Execution Plan:".cyan().bold());
    println!();

    // Track printed effects to avoid duplicates
    let mut printed: HashSet<usize> = HashSet::new();

    fn print_effect_tree(
        idx: usize,
        plan: &Plan,
        dependents: &HashMap<usize, Vec<usize>>,
        printed: &mut HashSet<usize>,
        indent: usize,
        is_last: bool,
        prefix: &str,
    ) {
        if printed.contains(&idx) {
            return;
        }
        printed.insert(idx);

        let effect = &plan.effects()[idx];
        let colored_symbol = match effect {
            Effect::Create(_) => "+".green().bold(),
            Effect::Update { .. } => "~".yellow().bold(),
            Effect::Replace { lifecycle, .. } => {
                if lifecycle.create_before_destroy {
                    "+/-".magenta().bold()
                } else {
                    "-/+".magenta().bold()
                }
            }
            Effect::Delete { .. } => "-".red().bold(),
            Effect::Read { .. } => "<=".cyan().bold(),
        };

        // Build the tree connector (shown before child resources)
        let connector = if indent == 0 {
            "".to_string()
        } else if is_last {
            format!("{}└─ ", prefix)
        } else {
            format!("{}├─ ", prefix)
        };

        // Base indentation for this resource
        let base_indent = "  ";
        // Attribute indentation (4 spaces from resource line)
        let attr_base = "    ";

        match effect {
            Effect::Create(r) => {
                println!(
                    "{}{}{} {} \"{}\"",
                    base_indent,
                    connector,
                    colored_symbol,
                    r.id.display_type().cyan().bold(),
                    r.id.name.white().bold()
                );
                // Attribute prefix aligns with the resource content
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                let mut keys: Vec<_> = r
                    .attributes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .collect();
                keys.sort();
                for key in keys {
                    let value = &r.attributes[key];
                    if is_list_of_maps(value) {
                        println!("{}{}:", attr_prefix, key);
                        println!("{}", format_list_of_maps(value, &attr_prefix));
                    } else {
                        println!(
                            "{}{}: {}",
                            attr_prefix,
                            key,
                            format_value_with_key(value, Some(key)).green()
                        );
                    }
                }
            }
            Effect::Update {
                id,
                from,
                to,
                changed_attributes,
            } => {
                println!(
                    "{}{}{} {} \"{}\"",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name.yellow().bold()
                );
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                let mut keys: Vec<_> = to
                    .attributes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .collect();
                keys.sort();
                for key in keys {
                    let new_value = &to.attributes[key];
                    let old_value = from.attributes.get(key);
                    let is_same = old_value
                        .map(|ov| ov.semantically_equal(new_value))
                        .unwrap_or(false);
                    if !is_same {
                        if is_list_of_maps(new_value) {
                            println!("{}{}:", attr_prefix, key);
                            println!("{}", format_list_diff(old_value, new_value, &attr_prefix));
                        } else {
                            let old_str = old_value
                                .map(|v| format_value_with_key(v, Some(key)))
                                .unwrap_or_else(|| "(none)".to_string());
                            println!(
                                "{}{}: {} → {}",
                                attr_prefix,
                                key,
                                old_str.red(),
                                format_value_with_key(new_value, Some(key)).green()
                            );
                        }
                    }
                }
                // Show removed attributes (in changed_attributes but not in to)
                let mut removed_keys: Vec<_> = changed_attributes
                    .iter()
                    .filter(|k| !to.attributes.contains_key(k.as_str()))
                    .collect();
                removed_keys.sort();
                for key in removed_keys {
                    if let Some(old_value) = from.attributes.get(key.as_str()) {
                        println!(
                            "{}{}: {} → {}",
                            attr_prefix,
                            key,
                            format_value_with_key(old_value, Some(key)).red(),
                            "(removed)".red()
                        );
                    }
                }
            }
            Effect::Replace {
                id,
                from,
                to,
                changed_create_only,
                lifecycle,
                cascading_updates,
                temporary_name,
                ..
            } => {
                let replace_note = if lifecycle.create_before_destroy {
                    "(must be replaced, create before destroy)"
                } else {
                    "(must be replaced)"
                };
                println!(
                    "{}{}{} {} \"{}\" {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name.magenta().bold(),
                    replace_note.magenta()
                );
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                let mut keys: Vec<_> = to
                    .attributes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .collect();
                keys.sort();
                for key in keys {
                    let new_value = &to.attributes[key];
                    let old_value = from.attributes.get(key);
                    let forces_replacement = changed_create_only.contains(key);
                    let is_same = old_value
                        .map(|ov| ov.semantically_equal(new_value))
                        .unwrap_or(false);
                    if !is_same {
                        if is_list_of_maps(new_value) {
                            let suffix = if forces_replacement {
                                format!(" {}", "(forces replacement)".magenta())
                            } else {
                                String::new()
                            };
                            println!("{}{}:{}", attr_prefix, key, suffix);
                            println!("{}", format_list_diff(old_value, new_value, &attr_prefix));
                        } else {
                            let old_str = old_value
                                .map(|v| format_value_with_key(v, Some(key)))
                                .unwrap_or_else(|| "(none)".to_string());
                            if forces_replacement {
                                println!(
                                    "{}{}: {} → {} {}",
                                    attr_prefix,
                                    key,
                                    old_str.red(),
                                    format_value_with_key(new_value, Some(key)).green(),
                                    "(forces replacement)".magenta()
                                );
                            } else {
                                println!(
                                    "{}{}: {} → {}",
                                    attr_prefix,
                                    key,
                                    old_str.red(),
                                    format_value_with_key(new_value, Some(key)).green()
                                );
                            }
                        }
                    }
                }
                if let Some(temp) = temporary_name {
                    if temp.can_rename {
                        println!(
                            "{}  {} via temporary name \"{}\", will rename back to \"{}\" after old resource is deleted",
                            attr_prefix,
                            "note:".magenta().bold(),
                            temp.temporary_value.magenta(),
                            temp.original_value.green()
                        );
                    } else {
                        println!(
                            "{}  {} name will be \"{}\" (cannot rename create-only attribute \"{}\")",
                            attr_prefix,
                            "note:".magenta().bold(),
                            temp.temporary_value.magenta(),
                            temp.attribute.magenta()
                        );
                    }
                }
                if !cascading_updates.is_empty() {
                    println!(
                        "{}  {} cascading update(s):",
                        attr_prefix,
                        cascading_updates.len()
                    );
                    for cascade in cascading_updates {
                        println!(
                            "{}  ~ {} \"{}\"",
                            attr_prefix,
                            cascade.id.display_type().cyan(),
                            cascade.id.name.magenta()
                        );
                    }
                }
            }
            Effect::Delete { id, .. } => {
                println!(
                    "{}{}{} {} \"{}\"",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name.red().bold()
                );
            }
            Effect::Read { resource } => {
                println!(
                    "{}{}{} {} \"{}\" {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    resource.id.display_type().cyan().bold(),
                    resource.id.name.cyan().bold(),
                    "(data source)".dimmed()
                );
            }
        }

        // Print children (dependents)
        let children = dependents.get(&idx).cloned().unwrap_or_default();
        let unprinted_children: Vec<_> = children
            .iter()
            .filter(|c| !printed.contains(c))
            .cloned()
            .collect();

        // New prefix for children: align with attribute indentation
        let new_prefix = if indent == 0 {
            format!("{}  ", attr_base)
        } else {
            let continuation = if is_last {
                format!("{}   ", prefix)
            } else {
                format!("{}│  ", prefix)
            };
            format!("{}   ", continuation)
        };

        for (i, child_idx) in unprinted_children.iter().enumerate() {
            let child_is_last = i == unprinted_children.len() - 1;
            print_effect_tree(
                *child_idx,
                plan,
                dependents,
                printed,
                indent + 1,
                child_is_last,
                &new_prefix,
            );
        }
    }

    // Print from roots
    for (i, root_idx) in roots.iter().enumerate() {
        print_effect_tree(
            *root_idx,
            plan,
            &dependents,
            &mut printed,
            0,
            i == roots.len() - 1,
            "",
        );
    }

    // Print any remaining effects that weren't reachable from roots
    // (e.g., circular dependencies or isolated resources)
    let remaining: Vec<_> = (0..plan.effects().len())
        .filter(|idx| !printed.contains(idx))
        .collect();
    for idx in remaining {
        print_effect_tree(idx, plan, &dependents, &mut printed, 0, true, "");
    }

    println!();
    let summary = plan.summary();
    let mut parts = Vec::new();
    if summary.read > 0 {
        parts.push(format!("{} to read", summary.read.to_string().cyan()));
    }
    parts.push(format!("{} to add", summary.create.to_string().green()));
    parts.push(format!("{} to change", summary.update.to_string().yellow()));
    if summary.replace > 0 {
        parts.push(format!(
            "{} to replace",
            summary.replace.to_string().magenta()
        ));
    }
    parts.push(format!("{} to destroy", summary.delete.to_string().red()));
    println!("Plan: {}.", parts.join(", "));
}

pub fn format_effect(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("Create {}", r.id),
        Effect::Update { id, .. } => format!("Update {}", id),
        Effect::Replace {
            id,
            lifecycle,
            cascading_updates,
            ..
        } => {
            if lifecycle.create_before_destroy {
                if cascading_updates.is_empty() {
                    format!("Replace {} (create-before-destroy)", id)
                } else {
                    format!(
                        "Replace {} (create-before-destroy, {} cascade)",
                        id,
                        cascading_updates.len()
                    )
                }
            } else {
                format!("Replace {}", id)
            }
        }
        Effect::Delete { id, .. } => format!("Delete {}", id),
        Effect::Read { resource } => {
            format!("Read {}", resource.id)
        }
    }
}

/// Format a list-of-maps for Create effect display (multi-line with + prefix)
pub fn format_list_of_maps(value: &Value, attr_prefix: &str) -> String {
    let items = match value {
        Value::List(items) => items,
        _ => return format_value(value),
    };
    let mut lines = Vec::new();
    for item in items {
        if let Value::Map(map) = item {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            lines.push(format!(
                "{}  {} {{{}}}",
                attr_prefix,
                "+".green().bold(),
                fields.join(", ")
            ));
        }
    }
    lines.join("\n")
}

/// Format a list-of-maps diff for Update effect display.
/// Uses content-matched comparison (multiset matching) instead of index-based.
/// 1. Find exact matches between old and new items
/// 2. Pair remaining unmatched items by similarity for field-level diffs
/// 3. Display unchanged, modified (~), added (+), and removed (-) items
pub fn format_list_diff(old_value: Option<&Value>, new_value: &Value, attr_prefix: &str) -> String {
    let new_items = match new_value {
        Value::List(items) => items,
        _ => return format_value(new_value),
    };
    let old_items = match old_value {
        Some(Value::List(items)) => items,
        _ => &vec![] as &Vec<Value>,
    };

    let mut old_matched = vec![false; old_items.len()];
    let mut new_matched = vec![false; new_items.len()];

    // Phase 1: Find exact matches (semantically equal items)
    for (ni, new_item) in new_items.iter().enumerate() {
        for (oi, old_item) in old_items.iter().enumerate() {
            if !old_matched[oi] && old_item.semantically_equal(new_item) {
                old_matched[oi] = true;
                new_matched[ni] = true;
                break;
            }
        }
    }

    // Collect unmatched items
    let unmatched_old: Vec<usize> = old_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();
    let unmatched_new: Vec<usize> = new_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();

    // Phase 2: Pair unmatched items by similarity (most shared key-value pairs)
    let mut paired: Vec<(usize, usize)> = Vec::new();
    let mut paired_old = vec![false; unmatched_old.len()];
    let mut paired_new = vec![false; unmatched_new.len()];

    for (ui_new, &ni) in unmatched_new.iter().enumerate() {
        let mut best_oi_idx = None;
        let mut best_sim = 0usize;
        for (ui_old, &oi) in unmatched_old.iter().enumerate() {
            if paired_old[ui_old] {
                continue;
            }
            let sim = map_similarity(&old_items[oi], &new_items[ni]);
            if sim > best_sim {
                best_sim = sim;
                best_oi_idx = Some(ui_old);
            }
        }
        if let Some(ui_old) = best_oi_idx.filter(|_| best_sim > 0) {
            paired.push((unmatched_old[ui_old], ni));
            paired_old[ui_old] = true;
            paired_new[ui_new] = true;
        }
    }

    // Remaining truly added/removed items
    let added: Vec<usize> = unmatched_new
        .iter()
        .enumerate()
        .filter(|(i, _)| !paired_new[*i])
        .map(|(_, &ni)| ni)
        .collect();
    let removed: Vec<usize> = unmatched_old
        .iter()
        .enumerate()
        .filter(|(i, _)| !paired_old[*i])
        .map(|(_, &oi)| oi)
        .collect();

    // Phase 3: Build output
    let mut lines = Vec::new();

    // Show unchanged items (exact matches from new list order)
    for (ni, new_item) in new_items.iter().enumerate() {
        if let Value::Map(map) = new_item
            && new_matched[ni]
        {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            lines.push(format!("{}    {{{}}}", attr_prefix, fields.join(", ")));
        }
    }

    // Show modified items (paired by similarity)
    for &(oi, ni) in &paired {
        if let (Value::Map(old_map), Value::Map(new_map)) = (&old_items[oi], &new_items[ni]) {
            let mut keys: Vec<_> = new_map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| {
                    let new_v = format_value(&new_map[*k]);
                    let field_same = old_map
                        .get(*k)
                        .map(|ov| ov.semantically_equal(&new_map[*k]))
                        .unwrap_or(false);
                    if !field_same {
                        let old_v = old_map
                            .get(*k)
                            .map(format_value)
                            .unwrap_or_else(|| "(none)".to_string());
                        format!("{}: {} → {}", k, old_v.red(), new_v.green())
                    } else {
                        format!("{}: {}", k, new_v)
                    }
                })
                .collect();
            lines.push(format!(
                "{}  {} {{{}}}",
                attr_prefix,
                "~".yellow().bold(),
                fields.join(", ")
            ));
        }
    }

    // Show added items
    for &ni in &added {
        if let Value::Map(map) = &new_items[ni] {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            lines.push(format!(
                "{}  {} {{{}}}",
                attr_prefix,
                "+".green().bold(),
                fields.join(", ")
            ));
        }
    }

    // Show removed items
    for &oi in &removed {
        if let Value::Map(map) = &old_items[oi] {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            lines.push(format!(
                "{}  {} {{{}}}",
                attr_prefix,
                "-".red().bold(),
                fields.join(", ")
            ));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    use carina_core::effect::Effect;
    use carina_core::plan::Plan;
    use carina_core::resource::{Resource, Value};

    fn make_resource(resource_type: &str, name: &str, binding: &str, deps: &[&str]) -> Resource {
        let mut r = Resource::new(resource_type, name);
        r.attributes
            .insert("_binding".to_string(), Value::String(binding.to_string()));
        for dep in deps {
            r.attributes.insert(
                format!("ref_{}", dep),
                Value::ResourceRef {
                    binding_name: dep.to_string(),
                    attribute_name: "id".to_string(),
                },
            );
        }
        r
    }

    /// Test that print_plan does not panic when a resource has a dependency
    /// on a binding that is not present in the plan (external dependency).
    /// This exercises the dependency graph construction code path where
    /// `.unwrap()` could theoretically panic if `dep_idx` were invalid.
    #[test]
    fn test_print_plan_with_external_dependency_does_not_panic() {
        // Resource "b" depends on "a", but "a" is NOT in the plan.
        // This simulates an external/unresolved dependency.
        let b = make_resource("test.resource", "b", "b", &["a"]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(b));

        // Should not panic
        print_plan(&plan);
    }

    /// Test that print_plan handles the dependency graph correctly when
    /// dependents map is accessed with valid indices.
    #[test]
    fn test_print_plan_with_internal_dependency_does_not_panic() {
        let a = make_resource("test.resource", "a", "a", &[]);
        let b = make_resource("test.resource", "b", "b", &["a"]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(a));
        plan.add(Effect::Create(b));

        // Should not panic
        print_plan(&plan);
    }

    /// Helper: compute root indices using the same algorithm as print_plan.
    fn compute_roots(plan: &Plan) -> Vec<usize> {
        let (roots, _, _, _) = build_plan_tree(plan);
        roots
    }

    /// Helper: build the full plan tree structure using the same algorithm as
    /// print_plan. Returns (roots, dependents, effect_bindings, effect_types)
    /// so tests can inspect the tree structure.
    #[allow(clippy::type_complexity)]
    fn build_plan_tree(
        plan: &Plan,
    ) -> (
        Vec<usize>,
        HashMap<usize, Vec<usize>>,
        HashMap<usize, String>,
        HashMap<usize, String>,
    ) {
        let mut binding_to_effect: HashMap<String, usize> = HashMap::new();
        let mut effect_deps: HashMap<usize, HashSet<String>> = HashMap::new();
        let mut effect_bindings: HashMap<usize, String> = HashMap::new();
        let mut effect_types: HashMap<usize, String> = HashMap::new();

        for (idx, effect) in plan.effects().iter().enumerate() {
            let (resource, deps) = match effect {
                Effect::Create(r) => (Some(r), get_resource_dependencies(r)),
                Effect::Update { to, .. } => (Some(to), get_resource_dependencies(to)),
                Effect::Replace { to, .. } => (Some(to), get_resource_dependencies(to)),
                Effect::Read { resource } => (Some(resource), get_resource_dependencies(resource)),
                Effect::Delete { .. } => (None, HashSet::new()),
            };

            if let Some(r) = resource {
                let binding = r
                    .attributes
                    .get("_binding")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| r.id.to_string());
                binding_to_effect.insert(binding.clone(), idx);
                effect_bindings.insert(idx, binding);
                effect_types.insert(idx, r.id.resource_type.clone());
            }
            effect_deps.insert(idx, deps);
        }

        let (roots, dependents) = build_single_parent_tree(
            plan,
            &binding_to_effect,
            &effect_deps,
            &effect_bindings,
            &effect_types,
        );

        (roots, dependents, effect_bindings, effect_types)
    }

    /// Helper: walk the tree from roots in print order and collect binding names
    /// in the order they would be printed. This replicates the traversal logic
    /// from print_effect_tree.
    fn collect_print_order(
        roots: &[usize],
        dependents: &HashMap<usize, Vec<usize>>,
        effect_bindings: &HashMap<usize, String>,
    ) -> Vec<String> {
        let mut printed: HashSet<usize> = HashSet::new();
        let mut result: Vec<String> = Vec::new();

        fn walk(
            idx: usize,
            dependents: &HashMap<usize, Vec<usize>>,
            effect_bindings: &HashMap<usize, String>,
            printed: &mut HashSet<usize>,
            result: &mut Vec<String>,
        ) {
            if printed.contains(&idx) {
                return;
            }
            printed.insert(idx);
            if let Some(binding) = effect_bindings.get(&idx) {
                result.push(binding.clone());
            }
            let children = dependents.get(&idx).cloned().unwrap_or_default();
            let unprinted: Vec<_> = children
                .iter()
                .filter(|c| !printed.contains(c))
                .cloned()
                .collect();
            for child in unprinted {
                walk(child, dependents, effect_bindings, printed, result);
            }
        }

        for &root in roots {
            walk(root, dependents, effect_bindings, &mut printed, &mut result);
        }
        result
    }

    /// Issue #933 (part 1): Siblings under the same parent should be sorted
    /// by (resource_type, binding_name) for deterministic, grouped output.
    ///
    /// Scenario: VPC has children of different types (subnets, route tables).
    /// Same-type resources should be grouped together, and within a type,
    /// sorted by binding name.
    #[test]
    fn test_siblings_sorted_by_resource_type_and_binding() {
        // VPC is root. Under it: 2 subnets and 2 route tables, added in
        // interleaved order to expose HashMap non-determinism.
        let vpc = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let rt_b = make_resource("ec2.route_table", "rt_b", "rt_b", &["vpc"]);
        let subnet_b = make_resource("ec2.subnet", "subnet_b", "subnet_b", &["vpc"]);
        let rt_a = make_resource("ec2.route_table", "rt_a", "rt_a", &["vpc"]);
        let subnet_a = make_resource("ec2.subnet", "subnet_a", "subnet_a", &["vpc"]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(rt_b));
        plan.add(Effect::Create(subnet_b));
        plan.add(Effect::Create(rt_a));
        plan.add(Effect::Create(subnet_a));

        let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

        assert_eq!(roots, vec![0], "VPC should be the only root");

        // Get the children of VPC (index 0)
        let vpc_children = dependents.get(&0).unwrap();
        let child_labels: Vec<(String, String)> = vpc_children
            .iter()
            .map(|&idx| {
                let binding = effect_bindings.get(&idx).unwrap().clone();
                let effect = &plan.effects()[idx];
                let rtype = match effect {
                    Effect::Create(r) => r.id.resource_type.clone(),
                    _ => String::new(),
                };
                (rtype, binding)
            })
            .collect();

        // Expected: sorted by (resource_type, binding_name)
        // ec2.route_table comes before ec2.subnet alphabetically
        let expected = vec![
            ("ec2.route_table".to_string(), "rt_a".to_string()),
            ("ec2.route_table".to_string(), "rt_b".to_string()),
            ("ec2.subnet".to_string(), "subnet_a".to_string()),
            ("ec2.subnet".to_string(), "subnet_b".to_string()),
        ];

        assert_eq!(
            child_labels, expected,
            "Siblings should be sorted by (resource_type, binding_name). \
             Got: {:?}",
            child_labels
        );
    }

    /// Issue #933 (part 2): When a resource depends on multiple resources in
    /// the tree, it should be placed under the dependency that is closest to
    /// the root (most ancestral), not an arbitrary one.
    ///
    /// Scenario:
    ///   - vpc (root)
    ///   - sg depends on vpc
    ///   - vpc_endpoint depends on both vpc and sg
    ///
    /// VPC is an ancestor of SG. The endpoint should be placed under VPC
    /// (closer to root), not under SG. Currently, the endpoint is added as
    /// a child of BOTH vpc and sg in the dependents map, and whichever is
    /// traversed first claims it. This test verifies deterministic placement
    /// under the most ancestral dependency.
    #[test]
    fn test_parent_selection_prefers_most_ancestral_dependency() {
        let vpc = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let sg = make_resource("ec2.security_group", "sg", "sg", &["vpc"]);
        let endpoint = make_resource("ec2.vpc_endpoint", "endpoint", "endpoint", &["vpc", "sg"]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(sg));
        plan.add(Effect::Create(endpoint));

        let (roots, dependents, effect_bindings, _) = build_plan_tree(&plan);
        let _print_order = collect_print_order(&roots, &dependents, &effect_bindings);

        assert_eq!(roots, vec![0], "VPC should be the only root");

        // The endpoint (idx 2) should be a direct child of VPC (idx 0),
        // NOT a child of SG (idx 1). VPC is the most ancestral dependency.
        //
        // Expected tree:
        //   vpc
        //   ├── sg
        //   └── endpoint
        //
        // NOT:
        //   vpc
        //   └── sg
        //       └── endpoint
        //
        // Check via print order: vpc -> sg -> endpoint (sg has no children
        // because endpoint is under vpc). If endpoint were under sg, we'd
        // get vpc -> sg -> endpoint too, but we verify via direct children.
        let vpc_children: Vec<String> = dependents
            .get(&0)
            .unwrap()
            .iter()
            .filter_map(|&idx| effect_bindings.get(&idx).cloned())
            .collect();
        assert!(
            vpc_children.contains(&"endpoint".to_string()),
            "endpoint should be a direct child of vpc (most ancestral), \
             not nested under sg. VPC children: {:?}",
            vpc_children
        );

        // sg should NOT have endpoint as a child (it should only be under vpc)
        let sg_children: Vec<String> = dependents
            .get(&1)
            .unwrap()
            .iter()
            .filter_map(|&idx| effect_bindings.get(&idx).cloned())
            .collect();
        assert!(
            !sg_children.contains(&"endpoint".to_string()),
            "endpoint should NOT be a child of sg. SG children: {:?}. \
             When a resource depends on multiple resources, it should only \
             be placed under the most ancestral one (vpc), not all of them.",
            sg_children
        );
    }

    /// Issue #928: A resource that has no dependencies but IS referenced by
    /// other resources should NOT appear as a disconnected root-level item.
    /// It should be nested under the resource that references it.
    ///
    /// Scenario (from the issue):
    ///   - vpc: no deps
    ///   - rt: depends on vpc
    ///   - route: depends on rt, igw
    ///   - igw_attachment: depends on vpc, igw
    ///   - igw: no deps (but referenced by route and igw_attachment)
    ///
    /// Current (buggy): igw appears as a separate root alongside vpc.
    /// Expected: igw should be nested under igw_attachment (or route),
    ///           so only vpc is a root.
    #[test]
    fn test_referenced_resource_without_deps_should_not_be_root() {
        let vpc = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let rt = make_resource("ec2.route_table", "rt", "rt", &["vpc"]);
        let igw = make_resource("ec2.internet_gateway", "igw", "igw", &[]);
        let route = make_resource("ec2.route", "route", "route", &["rt", "igw"]);
        let igw_attachment = make_resource(
            "ec2.vpc_gateway_attachment",
            "igw_attachment",
            "igw_attachment",
            &["vpc", "igw"],
        );

        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(rt));
        plan.add(Effect::Create(igw));
        plan.add(Effect::Create(route));
        plan.add(Effect::Create(igw_attachment));

        let roots = compute_roots(&plan);

        // IGW (index 2) should NOT be a root because it is referenced by
        // other resources in the plan (route and igw_attachment).
        // Only VPC (index 0) should be a root.
        assert_eq!(
            roots,
            vec![0],
            "Only vpc should be a root. igw (index 2) is referenced by other resources \
             and should be nested, not a disconnected root. Got roots: {:?}",
            roots
        );
    }
}
