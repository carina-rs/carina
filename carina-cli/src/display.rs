use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;

use colored::Colorize;

use carina_core::deps::get_resource_dependencies;
use carina_core::effect::{CascadingUpdate, Effect};
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

    // Add nested-under-dependent resources as children of the shallowest
    // referencing resource. This ensures resources like IGW are nested under
    // igw_attachment (depth 1) rather than route (depth 2+).
    for &idx in &nested_under_dependent {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        // Pick the shallowest dependent; break ties by (resource_type, binding_name)
        children.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        if let Some(&best_dependent) = children.first() {
            dependents.entry(best_dependent).or_default().push(idx);
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

/// Check if a resource has a `let` binding (i.e., is not anonymous).
fn has_binding(resource: &carina_core::resource::Resource) -> bool {
    resource.attributes.contains_key("_binding")
}

/// Extract a compact hint for anonymous resources.
///
/// Collects the first distinguishing string attribute (like `service_name`) and ALL
/// non-parent `Value::ResourceRef` attributes, then combines them into a comma-separated
/// hint. String hints appear first (most identifying), followed by ResourceRef hints.
///
/// `parent_binding` is the binding name of the parent resource in the tree, used to
/// skip ResourceRef attributes that redundantly reference the parent.
fn extract_compact_hint(
    resource: &carina_core::resource::Resource,
    parent_binding: Option<&str>,
) -> Option<String> {
    let mut keys: Vec<_> = resource
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    // Priority 1: First distinguishing string attribute (most identifying)
    for key in &keys {
        if let Some(Value::String(s)) = resource.attributes.get(*key)
            && !s.is_empty()
        {
            let short_key = shorten_attr_name(key);
            let display_value = shorten_service_name(key, s);
            return Some(format!("{}: {}", short_key, display_value));
        }
    }

    // Priority 2: First non-parent ResourceRef attribute (direct or inside a List)
    for key in &keys {
        match resource.attributes.get(*key) {
            Some(Value::ResourceRef { binding_name, .. }) => {
                if parent_binding == Some(binding_name.as_str()) {
                    continue;
                }
                let short_key = shorten_attr_name(key);
                return Some(format!("{}: {}", short_key, binding_name));
            }
            Some(Value::List(items)) => {
                for item in items {
                    if let Value::ResourceRef { binding_name, .. } = item {
                        if parent_binding == Some(binding_name.as_str()) {
                            continue;
                        }
                        let short_key = shorten_attr_name(key);
                        return Some(format!("{}: {}", short_key, binding_name));
                    }
                }
            }
            _ => {}
        }
    }

    None
}

/// Shorten common attribute name suffixes for compact display.
/// e.g., `subnet_id` -> `subnet`, `route_table_id` -> `route_table`,
///       `service_name` -> `service`, `group_name` -> `group`
fn shorten_attr_name(attr: &str) -> &str {
    attr.strip_suffix("_ids")
        .or_else(|| attr.strip_suffix("_id"))
        .or_else(|| attr.strip_suffix("_name"))
        .unwrap_or(attr)
}

/// For `service_name` attributes, extract just the service suffix from AWS endpoint names.
/// e.g., `com.amazonaws.ap-northeast-1.ecr.dkr` -> `ecr.dkr`
///       `com.amazonaws.ap-northeast-1.s3` -> `s3`
fn shorten_service_name<'a>(attr_name: &str, value: &'a str) -> Cow<'a, str> {
    if attr_name == "service_name" {
        // Match pattern: com.amazonaws.<region>.<service...>
        if let Some(rest) = value.strip_prefix("com.amazonaws.") {
            // Skip the region part (e.g., "ap-northeast-1") and take the rest
            if let Some(dot_pos) = rest.find('.') {
                let after_region = &rest[dot_pos + 1..];
                if !after_region.is_empty() {
                    return Cow::Borrowed(after_region);
                }
            }
        }
    }
    Cow::Borrowed(value)
}

/// Format a compact resource identifier, showing either the binding name in quotes
/// or a hint in parentheses for anonymous resources.
///
/// `name` is the display name of the resource (typically `id.name`).
/// `parent_binding` is the binding name of the parent in the tree, used to skip
/// redundant ResourceRef hints.
fn format_compact_name(
    resource: &carina_core::resource::Resource,
    name: &str,
    parent_binding: Option<&str>,
) -> String {
    if has_binding(resource) {
        name.to_string()
    } else if let Some(hint) = extract_compact_hint(resource, parent_binding) {
        format!("({})", hint)
    } else {
        name.to_string()
    }
}

pub fn print_plan(plan: &Plan, compact: bool) {
    print!("{}", format_plan(plan, compact));
}

/// Format a plan as a string for display.
///
/// This is the core formatting logic used by `print_plan`. Returning a `String`
/// enables snapshot testing and other programmatic uses of the plan output.
pub fn format_plan(plan: &Plan, compact: bool) -> String {
    let mut out = String::new();

    if plan.is_empty() {
        writeln!(
            out,
            "{}",
            "No changes. Infrastructure is up-to-date.".green()
        )
        .unwrap();
        return out;
    }

    // Build dependency graph from effects
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
            Effect::Delete {
                id,
                binding,
                dependencies,
                ..
            } => {
                let deps = dependencies.clone();
                if let Some(b) = binding {
                    binding_to_effect.insert(b.clone(), idx);
                    effect_bindings.insert(idx, b.clone());
                } else {
                    let fallback = id.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    effect_bindings.insert(idx, fallback);
                }
                effect_types.insert(idx, id.resource_type.clone());
                effect_deps.insert(idx, deps);
                continue;
            }
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

    // Build the single-parent tree with sorted siblings
    let (roots, dependents) = build_single_parent_tree(
        plan,
        &binding_to_effect,
        &effect_deps,
        &effect_bindings,
        &effect_types,
    );

    writeln!(out, "{}", "Execution Plan:".cyan().bold()).unwrap();
    writeln!(out).unwrap();

    // Track printed effects to avoid duplicates
    let mut printed: HashSet<usize> = HashSet::new();

    #[allow(clippy::too_many_arguments)]
    fn format_effect_tree(
        out: &mut String,
        idx: usize,
        plan: &Plan,
        dependents: &HashMap<usize, Vec<usize>>,
        printed: &mut HashSet<usize>,
        indent: usize,
        is_last: bool,
        prefix: &str,
        compact: bool,
        parent_binding: Option<&str>,
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

        let mut has_displayed_attrs = false;

        match effect {
            Effect::Create(r) => {
                if compact {
                    let name_part = format_compact_name(r, &r.id.name, parent_binding);
                    writeln!(
                        out,
                        "{}{}{} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        r.id.display_type().cyan().bold(),
                        name_part.white().bold()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        out,
                        "{}{}{} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        r.id.display_type().cyan().bold(),
                        r.id.name.white().bold()
                    )
                    .unwrap();
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
                    if !keys.is_empty() {
                        has_displayed_attrs = true;
                    }
                    for key in keys {
                        let value = &r.attributes[key];
                        if is_list_of_maps(value) {
                            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
                            writeln!(out, "{}", format_list_of_maps(value, &attr_prefix)).unwrap();
                        } else {
                            writeln!(
                                out,
                                "{}{}: {}",
                                attr_prefix,
                                key,
                                format_value_with_key(value, Some(key)).green()
                            )
                            .unwrap();
                        }
                    }
                }
            }
            Effect::Update {
                id,
                from,
                to,
                changed_attributes,
            } => {
                if compact {
                    let name_part = format_compact_name(to, &id.name, parent_binding);
                    writeln!(
                        out,
                        "{}{}{} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        name_part.yellow().bold()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        out,
                        "{}{}{} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        id.name.yellow().bold()
                    )
                    .unwrap();
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
                            has_displayed_attrs = true;
                            if is_list_of_maps(new_value) {
                                writeln!(out, "{}{}:", attr_prefix, key).unwrap();
                                writeln!(
                                    out,
                                    "{}",
                                    format_list_diff(old_value, new_value, &attr_prefix)
                                )
                                .unwrap();
                            } else if is_both_maps(old_value, new_value) {
                                writeln!(out, "{}{}:", attr_prefix, key).unwrap();
                                writeln!(
                                    out,
                                    "{}",
                                    format_map_diff(old_value, new_value, &attr_prefix)
                                )
                                .unwrap();
                            } else {
                                let old_str = old_value
                                    .map(|v| format_value_with_key(v, Some(key)))
                                    .unwrap_or_else(|| "(none)".to_string());
                                writeln!(
                                    out,
                                    "{}{}: {} → {}",
                                    attr_prefix,
                                    key,
                                    old_str.red().strikethrough(),
                                    format_value_with_key(new_value, Some(key)).green()
                                )
                                .unwrap();
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
                            has_displayed_attrs = true;
                            writeln!(
                                out,
                                "{}{}: {} → {}",
                                attr_prefix,
                                key,
                                format_value_with_key(old_value, Some(key))
                                    .red()
                                    .strikethrough(),
                                "(removed)".red().strikethrough()
                            )
                            .unwrap();
                        }
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
                cascade_ref_hints,
            } => {
                let replace_note = if lifecycle.create_before_destroy {
                    "(must be replaced, create before destroy)"
                } else {
                    "(must be replaced)"
                };
                if compact {
                    let name_part = format_compact_name(to, &id.name, parent_binding);
                    writeln!(
                        out,
                        "{}{}{} {} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        name_part.magenta().bold(),
                        replace_note.magenta()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        out,
                        "{}{}{} {} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        id.name.magenta().bold(),
                        replace_note.magenta()
                    )
                    .unwrap();
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
                    let replace_attrs_output = format_replace_changed_attrs(
                        &from.attributes,
                        &to.attributes,
                        changed_create_only,
                        &attr_prefix,
                        cascade_ref_hints,
                    );
                    if !replace_attrs_output.is_empty() {
                        has_displayed_attrs = true;
                        write!(out, "{}", replace_attrs_output).unwrap();
                    }
                    if let Some(temp) = temporary_name {
                        has_displayed_attrs = true;
                        if temp.can_rename {
                            writeln!(
                                out,
                                "{}  {} via temporary name \"{}\", will rename back to \"{}\" after old resource is deleted",
                                attr_prefix,
                                "note:".magenta().bold(),
                                temp.temporary_value.magenta(),
                                temp.original_value.green()
                            )
                            .unwrap();
                        } else {
                            writeln!(
                                out,
                                "{}  {} name will be \"{}\" (cannot rename create-only attribute \"{}\")",
                                attr_prefix,
                                "note:".magenta().bold(),
                                temp.temporary_value.magenta(),
                                temp.attribute.magenta()
                            )
                            .unwrap();
                        }
                    }
                    if !cascading_updates.is_empty() {
                        has_displayed_attrs = true;
                        writeln!(
                            out,
                            "{}  {} cascading update(s):",
                            attr_prefix,
                            cascading_updates.len()
                        )
                        .unwrap();
                        let replaced_binding = to
                            .attributes
                            .get("_binding")
                            .and_then(|v| match v {
                                Value::String(s) => Some(s.as_str()),
                                _ => None,
                            })
                            .unwrap_or("");
                        for cascade in cascading_updates {
                            writeln!(
                                out,
                                "{}    ~ {} {}",
                                attr_prefix,
                                cascade.id.display_type().cyan(),
                                cascade.id.name.magenta()
                            )
                            .unwrap();
                            let cascade_prefix = format!("{}    ", attr_prefix);
                            let diff = format_cascading_update_diff(
                                cascade,
                                &cascade_prefix,
                                replaced_binding,
                            );
                            if !diff.is_empty() {
                                writeln!(out, "{}", diff).unwrap();
                            }
                        }
                    }
                }
            }
            Effect::Delete { id, .. } => {
                writeln!(
                    out,
                    "{}{}{} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name.red().bold().strikethrough()
                )
                .unwrap();
            }
            Effect::Read { resource } => {
                if compact {
                    let name_part =
                        format_compact_name(resource, &resource.id.name, parent_binding);
                    writeln!(
                        out,
                        "{}{}{} {} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        resource.id.display_type().cyan().bold(),
                        name_part.cyan().bold(),
                        "(data source)".dimmed()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        out,
                        "{}{}{} {} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        resource.id.display_type().cyan().bold(),
                        resource.id.name.cyan().bold(),
                        "(data source)".dimmed()
                    )
                    .unwrap();
                }
            }
        }

        // Extract current effect's binding name for children
        let current_binding = {
            if let Effect::Delete { binding, .. } = effect {
                binding.clone()
            } else {
                let resource = match effect {
                    Effect::Create(r) => Some(r),
                    Effect::Update { to, .. } => Some(to),
                    Effect::Replace { to, .. } => Some(to),
                    Effect::Read { resource } => Some(resource),
                    Effect::Delete { .. } => None,
                };
                resource.and_then(|r| {
                    r.attributes.get("_binding").and_then(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                })
            }
        };

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

        // Insert tree continuation line between attribute block and child resources
        if has_displayed_attrs && !unprinted_children.is_empty() {
            writeln!(out, "{}{}│", base_indent, new_prefix).unwrap();
        }

        for (i, child_idx) in unprinted_children.iter().enumerate() {
            let child_is_last = i == unprinted_children.len() - 1;
            format_effect_tree(
                out,
                *child_idx,
                plan,
                dependents,
                printed,
                indent + 1,
                child_is_last,
                &new_prefix,
                compact,
                current_binding.as_deref(),
            );
        }
    }

    // Print from roots
    for (i, root_idx) in roots.iter().enumerate() {
        format_effect_tree(
            &mut out,
            *root_idx,
            plan,
            &dependents,
            &mut printed,
            0,
            i == roots.len() - 1,
            "",
            compact,
            None,
        );
    }

    // Print any remaining effects that weren't reachable from roots
    // (e.g., circular dependencies or isolated resources)
    let remaining: Vec<_> = (0..plan.effects().len())
        .filter(|idx| !printed.contains(idx))
        .collect();
    for idx in remaining {
        format_effect_tree(
            &mut out,
            idx,
            plan,
            &dependents,
            &mut printed,
            0,
            true,
            "",
            compact,
            None,
        );
    }

    writeln!(out).unwrap();
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
    writeln!(out, "Plan: {}.", parts.join(", ")).unwrap();

    out
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

/// Check if both old and new values are `Value::Map`.
fn is_both_maps(old_value: Option<&Value>, new_value: &Value) -> bool {
    matches!((old_value, new_value), (Some(Value::Map(_)), Value::Map(_)))
}

/// Format a key-level diff between two map values.
///
/// Shows only the keys that changed:
/// - `+ key: "value"` for added keys
/// - `- key: "value"` for removed keys
/// - `~ key: "old" -> "new"` for changed keys
///
/// Unchanged keys are not shown.
pub fn format_map_diff(old_value: Option<&Value>, new_value: &Value, attr_prefix: &str) -> String {
    let new_map = match new_value {
        Value::Map(m) => m,
        _ => return format_value(new_value),
    };
    let old_map = match old_value {
        Some(Value::Map(m)) => m,
        _ => {
            // No old map; treat all new keys as added
            let mut keys: Vec<_> = new_map.keys().collect();
            keys.sort();
            let mut lines = Vec::new();
            for key in keys {
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "+".green(),
                    key,
                    format_value_with_key(&new_map[key], Some(key)).green()
                ));
            }
            return lines.join("\n");
        }
    };

    let mut lines = Vec::new();

    // Collect all keys from both maps
    let mut all_keys: Vec<&String> = old_map.keys().chain(new_map.keys()).collect();
    all_keys.sort();
    all_keys.dedup();

    for key in all_keys {
        let old_val = old_map.get(key);
        let new_val = new_map.get(key);
        match (old_val, new_val) {
            (Some(ov), Some(nv)) => {
                if !ov.semantically_equal(nv) {
                    // Changed
                    lines.push(format!(
                        "{}  {} {}: {} → {}",
                        attr_prefix,
                        "~".yellow(),
                        key,
                        format_value_with_key(ov, Some(key)).red().strikethrough(),
                        format_value_with_key(nv, Some(key)).green()
                    ));
                }
                // Unchanged: skip
            }
            (None, Some(nv)) => {
                // Added
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "+".green(),
                    key,
                    format_value_with_key(nv, Some(key)).green()
                ));
            }
            (Some(ov), None) => {
                // Removed
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "-".red().strikethrough(),
                    key,
                    format_value_with_key(ov, Some(key)).red().strikethrough()
                ));
            }
            (None, None) => {}
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
                        format!("{}: {} → {}", k, old_v.red().strikethrough(), new_v.green())
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
                "-".red().bold().strikethrough(),
                fields.join(", ").red().strikethrough()
            ));
        }
    }

    lines.join("\n")
}

/// Format the attribute diffs for a cascading update.
///
/// Only shows attributes whose value in `cascade.to` is a `Value::ResourceRef`
/// (or a `Value::List` containing a `ResourceRef`) that references the
/// `replaced_binding`. This avoids false diffs caused by DSL vs AWS format
/// mismatches on unrelated attributes (issue #958).
/// Format the changed_create_only attributes for a Replace effect.
///
/// Only shows attributes listed in `changed_create_only` that exist in `to_attrs`.
/// When the old and new values are semantically equal (cascade-triggered replacement
/// where the new value is not yet known), the attribute is shown with
/// "(forces replacement, known after apply)" instead of being hidden.
fn format_replace_changed_attrs(
    from_attrs: &std::collections::HashMap<String, Value>,
    to_attrs: &std::collections::HashMap<String, Value>,
    changed_create_only: &[String],
    attr_prefix: &str,
    cascade_ref_hints: &[(String, String)],
) -> String {
    let mut lines = Vec::new();
    let mut keys: Vec<_> = changed_create_only
        .iter()
        .filter(|k| to_attrs.contains_key(k.as_str()))
        .collect();
    keys.sort();
    for key in keys {
        let new_value = &to_attrs[key.as_str()];
        let old_value = from_attrs.get(key.as_str());
        let is_same = old_value
            .map(|ov| ov.semantically_equal(new_value))
            .unwrap_or(false);
        if is_same {
            // Value hasn't visibly changed yet — this is a cascade-triggered
            // create-only attr whose new value is unknown until the
            // depended-upon resource is replaced.
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            // Use the original ResourceRef hint if available, otherwise show the resolved value
            let new_str = cascade_ref_hints
                .iter()
                .find(|(attr, _)| attr == key)
                .map(|(_, hint)| hint.clone())
                .unwrap_or_else(|| format_value_with_key(new_value, Some(key)));
            lines.push(format!(
                "{}{}: {} → {} {}\n",
                attr_prefix,
                key,
                old_str.red().strikethrough(),
                new_str.green(),
                "(forces replacement, known after apply)".magenta()
            ));
        } else if is_list_of_maps(new_value) {
            let suffix = format!(" {}", "(forces replacement)".magenta());
            lines.push(format!("{}{}:{}\n", attr_prefix, key, suffix));
            lines.push(format!(
                "{}\n",
                format_list_diff(old_value, new_value, attr_prefix)
            ));
        } else if is_both_maps(old_value, new_value) {
            let suffix = format!(" {}", "(forces replacement)".magenta());
            lines.push(format!("{}{}:{}\n", attr_prefix, key, suffix));
            lines.push(format!(
                "{}\n",
                format_map_diff(old_value, new_value, attr_prefix)
            ));
        } else {
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            lines.push(format!(
                "{}{}: {} → {} {}\n",
                attr_prefix,
                key,
                old_str.red().strikethrough(),
                format_value_with_key(new_value, Some(key)).green(),
                "(forces replacement)".magenta()
            ));
        }
    }
    lines.concat()
}

fn format_cascading_update_diff(
    cascade: &CascadingUpdate,
    attr_prefix: &str,
    replaced_binding: &str,
) -> String {
    let mut lines = Vec::new();
    let mut keys: Vec<_> = cascade
        .to
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();
    for key in keys {
        let new_value = &cascade.to.attributes[key];
        if !value_references_binding(new_value, replaced_binding) {
            continue;
        }
        let old_value = cascade.from.attributes.get(key);
        let old_str = old_value
            .map(|v| format_value_with_key(v, Some(key)))
            .unwrap_or_else(|| "(none)".to_string());
        let new_str = format_value_with_key(new_value, Some(key));
        lines.push(format!(
            "{}    {}: {} → {} {}",
            attr_prefix,
            key,
            old_str.red().strikethrough(),
            new_str.green(),
            "(known after apply)".dimmed()
        ));
    }
    lines.join("\n")
}

/// Check whether a Value references the given binding name.
///
/// Returns true for `Value::ResourceRef` with matching `binding_name`,
/// or `Value::List` / `Value::Map` containing such a reference.
fn value_references_binding(value: &Value, binding: &str) -> bool {
    match value {
        Value::ResourceRef { binding_name, .. } => binding_name == binding,
        Value::List(items) => items.iter().any(|v| value_references_binding(v, binding)),
        Value::Map(map) => map.values().any(|v| value_references_binding(v, binding)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use carina_core::effect::{CascadingUpdate, Effect};
    use carina_core::plan::Plan;
    use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

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
        print_plan(&plan, false);
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
        print_plan(&plan, false);
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
                Effect::Delete {
                    id,
                    binding,
                    dependencies,
                    ..
                } => {
                    let deps = dependencies.clone();
                    if let Some(b) = binding {
                        binding_to_effect.insert(b.clone(), idx);
                        effect_bindings.insert(idx, b.clone());
                    } else {
                        let fallback = id.to_string();
                        binding_to_effect.insert(fallback.clone(), idx);
                        effect_bindings.insert(idx, fallback);
                    }
                    effect_types.insert(idx, id.resource_type.clone());
                    effect_deps.insert(idx, deps);
                    continue;
                }
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

    /// Issue #933: A dependency-free resource that is referenced by multiple
    /// resources should be nested under the shallowest referencing resource.
    ///
    /// Scenario:
    ///   - vpc (root, depth 0)
    ///   - rt depends on vpc (depth 1)
    ///   - igw_attachment depends on vpc, igw (depth 1)
    ///   - route depends on rt, igw (depth 2, under rt)
    ///   - igw: no deps (referenced by route at depth 2, igw_attachment at depth 1)
    ///
    /// IGW should be nested under igw_attachment (depth 1), not route (depth 2).
    #[test]
    fn test_dependency_free_resource_nested_under_shallowest_referencing_resource() {
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

        let (roots, dependents, effect_bindings, _) = build_plan_tree(&plan);

        assert_eq!(roots, vec![0], "Only vpc should be root");

        // igw_attachment is index 4, route is index 3, igw is index 2
        // igw should be a child of igw_attachment (depth 1), NOT route (depth 2)
        let igw_attachment_children: Vec<String> = dependents
            .get(&4)
            .unwrap()
            .iter()
            .filter_map(|&idx| effect_bindings.get(&idx).cloned())
            .collect();
        assert!(
            igw_attachment_children.contains(&"igw".to_string()),
            "igw should be nested under igw_attachment (shallowest referencing resource at depth 1). \
             igw_attachment children: {:?}",
            igw_attachment_children
        );

        let route_children: Vec<String> = dependents
            .get(&3)
            .unwrap()
            .iter()
            .filter_map(|&idx| effect_bindings.get(&idx).cloned())
            .collect();
        assert!(
            !route_children.contains(&"igw".to_string()),
            "igw should NOT be nested under route (deeper at depth 2). \
             route children: {:?}",
            route_children
        );
    }

    // --- Compact mode tests ---

    /// Test that extract_compact_hint returns only the first non-parent ResourceRef hint.
    #[test]
    fn test_extract_compact_hint_resource_ref() {
        let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
        r.attributes.insert(
            "route_table_id".to_string(),
            Value::ResourceRef {
                binding_name: "public_rt".to_string(),
                attribute_name: "id".to_string(),
            },
        );
        r.attributes.insert(
            "subnet_id".to_string(),
            Value::ResourceRef {
                binding_name: "public_subnet_1a".to_string(),
                attribute_name: "id".to_string(),
            },
        );

        let hint = extract_compact_hint(&r, None);
        // Should return only the first ResourceRef alphabetically, with _id suffix stripped
        assert_eq!(hint, Some("route_table: public_rt".to_string()));
    }

    /// Test that extract_compact_hint skips ResourceRef that matches parent binding.
    #[test]
    fn test_extract_compact_hint_skips_parent_ref() {
        let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
        r.attributes.insert(
            "route_table_id".to_string(),
            Value::ResourceRef {
                binding_name: "database_rt".to_string(),
                attribute_name: "id".to_string(),
            },
        );
        r.attributes.insert(
            "subnet_id".to_string(),
            Value::ResourceRef {
                binding_name: "database_subnet_1a".to_string(),
                attribute_name: "id".to_string(),
            },
        );

        // When parent is database_rt, should skip route_table_id and show only subnet_id
        let hint = extract_compact_hint(&r, Some("database_rt"));
        assert_eq!(hint, Some("subnet: database_subnet_1a".to_string()));
    }

    /// Test that extract_compact_hint falls back to string values when no ResourceRef.
    #[test]
    fn test_extract_compact_hint_string_fallback() {
        let mut r = Resource::new("ec2.route", "hash456");
        r.attributes.insert(
            "destination_cidr_block".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );

        let hint = extract_compact_hint(&r, None);
        assert_eq!(hint, Some("destination_cidr_block: 0.0.0.0/0".to_string()));
    }

    /// Test that extract_compact_hint returns None when no useful attributes.
    #[test]
    fn test_extract_compact_hint_none() {
        let r = Resource::new("ec2.route", "hash789");
        let hint = extract_compact_hint(&r, None);
        assert_eq!(hint, None);
    }

    /// Test that extract_compact_hint prefers string over ResourceRef.
    #[test]
    fn test_extract_compact_hint_prefers_string_over_resource_ref() {
        let mut r = Resource::new("ec2.route", "hash_mixed");
        r.attributes.insert(
            "destination".to_string(),
            Value::String("10.0.0.0/8".to_string()),
        );
        r.attributes.insert(
            "gateway_id".to_string(),
            Value::ResourceRef {
                binding_name: "igw".to_string(),
                attribute_name: "id".to_string(),
            },
        );

        let hint = extract_compact_hint(&r, None);
        // String takes priority over ResourceRef
        assert_eq!(hint, Some("destination: 10.0.0.0/8".to_string()));
    }

    /// Test that extract_compact_hint shortens service_name values.
    #[test]
    fn test_extract_compact_hint_service_name_shortening() {
        let mut r = Resource::new("ec2.vpc_endpoint", "hash_svc");
        r.attributes.insert(
            "service_name".to_string(),
            Value::String("com.amazonaws.ap-northeast-1.ecr.dkr".to_string()),
        );

        let hint = extract_compact_hint(&r, None);
        assert_eq!(hint, Some("service: ecr.dkr".to_string()));

        // Single service component
        let mut r2 = Resource::new("ec2.vpc_endpoint", "hash_svc2");
        r2.attributes.insert(
            "service_name".to_string(),
            Value::String("com.amazonaws.ap-northeast-1.s3".to_string()),
        );

        let hint2 = extract_compact_hint(&r2, None);
        assert_eq!(hint2, Some("service: s3".to_string()));
    }

    /// Test that extract_compact_hint falls back to string when all ResourceRefs match parent.
    #[test]
    fn test_extract_compact_hint_all_refs_match_parent() {
        let mut r = Resource::new("ec2.security_group_ingress", "hash_sg");
        r.attributes.insert(
            "group_id".to_string(),
            Value::ResourceRef {
                binding_name: "endpoint_sg".to_string(),
                attribute_name: "id".to_string(),
            },
        );
        r.attributes.insert(
            "description".to_string(),
            Value::String("Allow HTTPS from VPC".to_string()),
        );

        // When parent is endpoint_sg, should skip group_id and use description
        let hint = extract_compact_hint(&r, Some("endpoint_sg"));
        assert_eq!(hint, Some("description: Allow HTTPS from VPC".to_string()));
    }

    /// Test that extract_compact_hint prefers string over ResourceRef for vpc_endpoint.
    #[test]
    fn test_extract_compact_hint_prefers_string_for_vpc_endpoint() {
        let mut r = Resource::new("ec2.vpc_endpoint", "hash_ep");
        r.attributes.insert(
            "service_name".to_string(),
            Value::String("com.amazonaws.ap-northeast-1.ecr.dkr".to_string()),
        );
        r.attributes.insert(
            "security_group_ids".to_string(),
            Value::List(vec![Value::ResourceRef {
                binding_name: "endpoint_sg".to_string(),
                attribute_name: "group_id".to_string(),
            }]),
        );
        r.attributes.insert(
            "vpc_id".to_string(),
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "id".to_string(),
            },
        );

        // String attribute (service_name) takes priority over ResourceRef
        let hint = extract_compact_hint(&r, Some("vpc"));
        assert_eq!(hint, Some("service: ecr.dkr".to_string()));
    }

    /// Test that has_binding correctly detects bound vs anonymous resources.
    #[test]
    fn test_has_binding() {
        let mut bound = Resource::new("ec2.vpc", "vpc");
        bound
            .attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));
        assert!(has_binding(&bound));

        let anonymous = Resource::new("ec2.vpc", "hash123");
        assert!(!has_binding(&anonymous));
    }

    /// Test that format_compact_name shows plain identifiers for bound resources and
    /// parenthesized hints for anonymous resources.
    #[test]
    fn test_format_compact_name_bound_resource() {
        let mut r = Resource::new("ec2.vpc", "vpc");
        r.attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));
        // For bound resources, should show name as plain identifier (no quotes)
        let result = format_compact_name(&r, "vpc", None);
        assert!(
            !result.contains('"'),
            "Bound resource name should not be quoted"
        );
        assert_eq!(result, "vpc", "Should be the plain binding name");
    }

    #[test]
    fn test_format_compact_name_anonymous_with_hint() {
        let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
        r.attributes.insert(
            "subnet_id".to_string(),
            Value::ResourceRef {
                binding_name: "database_subnet_1a".to_string(),
                attribute_name: "id".to_string(),
            },
        );
        let result = format_compact_name(&r, "hash123", None);
        assert!(
            result.contains('(') && result.contains(')'),
            "Anonymous resource should show hint in parentheses, got: {}",
            result
        );
        assert!(
            result.contains("subnet: database_subnet_1a"),
            "Should contain the ResourceRef hint, got: {}",
            result
        );
    }

    /// Test that print_plan with compact=true does not panic and does not
    /// print attribute lines.
    #[test]
    fn test_print_plan_compact_does_not_panic() {
        let vpc = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let rt = make_resource("ec2.route_table", "rt", "rt", &["vpc"]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(vpc));
        plan.add(Effect::Create(rt));

        // Should not panic
        print_plan(&plan, true);
    }

    /// Test compact mode skips attributes by checking that _binding attribute
    /// keys are not printed (attributes are hidden in compact mode).
    #[test]
    fn test_print_plan_compact_with_anonymous_resources() {
        let mut anon = Resource::new("ec2.route", "hash_anon");
        anon.attributes.insert(
            "destination_cidr_block".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        anon.attributes.insert(
            "route_table_id".to_string(),
            Value::ResourceRef {
                binding_name: "public_rt".to_string(),
                attribute_name: "id".to_string(),
            },
        );

        let mut plan = Plan::new();
        plan.add(Effect::Create(anon));

        // Should not panic; anonymous resources should show hints
        print_plan(&plan, true);
    }

    /// Test that extract_compact_hint extracts ResourceRef from inside a List value.
    /// e.g., security_group_ids = [endpoint_sg.group_id] should produce "security_group: endpoint_sg"
    #[test]
    fn test_extract_compact_hint_list_containing_resource_ref() {
        let mut r = Resource::new("ec2.vpc_endpoint", "hash_list_ref");
        r.attributes.insert(
            "security_group_ids".to_string(),
            Value::List(vec![Value::ResourceRef {
                binding_name: "endpoint_sg".to_string(),
                attribute_name: "group_id".to_string(),
            }]),
        );

        let hint = extract_compact_hint(&r, None);
        assert_eq!(
            hint,
            Some("security_group: endpoint_sg".to_string()),
            "Should extract ResourceRef from inside List and strip _ids suffix"
        );
    }

    /// Test that extract_compact_hint skips List<ResourceRef> when ref matches parent.
    #[test]
    fn test_extract_compact_hint_list_ref_skips_parent() {
        let mut r = Resource::new("ec2.vpc_endpoint", "hash_list_parent");
        r.attributes.insert(
            "security_group_ids".to_string(),
            Value::List(vec![Value::ResourceRef {
                binding_name: "endpoint_sg".to_string(),
                attribute_name: "group_id".to_string(),
            }]),
        );

        let hint = extract_compact_hint(&r, Some("endpoint_sg"));
        assert_eq!(
            hint, None,
            "Should skip List<ResourceRef> when ref matches parent"
        );
    }

    /// Test that shorten_attr_name handles _ids suffix (plural).
    #[test]
    fn test_shorten_attr_name_ids_suffix() {
        assert_eq!(shorten_attr_name("security_group_ids"), "security_group");
        assert_eq!(shorten_attr_name("subnet_ids"), "subnet");
        // _id still works
        assert_eq!(shorten_attr_name("subnet_id"), "subnet");
        // _name still works
        assert_eq!(shorten_attr_name("service_name"), "service");
    }

    /// Test that extract_compact_hint skips _-prefixed attributes.
    #[test]
    fn test_extract_compact_hint_skips_internal_attributes() {
        let mut r = Resource::new("ec2.vpc", "hash_internal");
        r.attributes
            .insert("_binding".to_string(), Value::String("vpc".to_string()));
        r.attributes
            .insert("_hash".to_string(), Value::String("abc123".to_string()));

        let hint = extract_compact_hint(&r, None);
        assert_eq!(hint, None, "Internal attributes should be skipped");
    }

    #[test]
    fn test_cascading_update_shows_attribute_diffs() {
        use std::collections::HashMap;

        // Build a Replace effect with a cascading update that changes vpc_id
        let vpc_from = State::existing(
            ResourceId::new("ec2.vpc", "vpc"),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]),
        );
        let vpc_to = Resource::new("ec2.vpc", "vpc")
            .with_attribute("_binding", Value::String("vpc".to_string()))
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

        let subnet_from = State::existing(
            ResourceId::new("ec2.subnet", "subnet"),
            HashMap::from([
                (
                    "vpc_id".to_string(),
                    Value::String("vpc-old123".to_string()),
                ),
                (
                    "cidr_block".to_string(),
                    Value::String("10.0.1.0/24".to_string()),
                ),
            ]),
        );
        let subnet_to = Resource::new("ec2.subnet", "subnet")
            .with_attribute(
                "vpc_id",
                Value::ResourceRef {
                    binding_name: "vpc".to_string(),
                    attribute_name: "vpc_id".to_string(),
                },
            )
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string()));

        let replace_effect = Effect::Replace {
            id: ResourceId::new("ec2.vpc", "vpc"),
            from: Box::new(vpc_from),
            to: vpc_to,
            lifecycle: LifecycleConfig {
                create_before_destroy: true,
                ..Default::default()
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![CascadingUpdate {
                id: ResourceId::new("ec2.subnet", "subnet"),
                from: Box::new(subnet_from),
                to: subnet_to,
            }],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };

        let mut plan = Plan::new();
        plan.add(replace_effect);

        // Should not panic and should display attribute diffs for cascading updates
        print_plan(&plan, false);
    }

    #[test]
    fn test_format_cascading_update_attr_diff() {
        use std::collections::HashMap;

        let cascade = CascadingUpdate {
            id: ResourceId::new("ec2.subnet", "subnet"),
            from: Box::new(State::existing(
                ResourceId::new("ec2.subnet", "subnet"),
                HashMap::from([
                    (
                        "vpc_id".to_string(),
                        Value::String("vpc-old123".to_string()),
                    ),
                    (
                        "cidr_block".to_string(),
                        Value::String("10.0.1.0/24".to_string()),
                    ),
                ]),
            )),
            to: Resource::new("ec2.subnet", "subnet")
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                )
                .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string())),
        };

        let output = format_cascading_update_diff(&cascade, "    ", "vpc");
        // vpc_id references the replaced binding "vpc", so it should appear
        assert!(
            output.contains("vpc_id"),
            "Expected vpc_id in diff output, got: {}",
            output
        );
        // cidr_block does not reference the replaced binding, so it should NOT appear
        assert!(
            !output.contains("cidr_block"),
            "cidr_block should not appear in diff output, got: {}",
            output
        );
        // Should show the old value and new value
        assert!(
            output.contains("vpc-old123"),
            "Expected old value in diff, got: {}",
            output
        );
        assert!(
            output.contains("vpc.vpc_id"),
            "Expected new value in diff, got: {}",
            output
        );
    }

    /// Test that a cascade-triggered changed_create_only attribute with the same old and new
    /// value is still displayed with "(forces replacement, known after apply)" annotation.
    ///
    /// This reproduces the real-world scenario where a cascade merge adds `vpc_id` to
    /// `changed_create_only`, but the ResourceRef in `to` resolves to the current VPC ID
    /// (same as `from`) because the new VPC hasn't been created yet. The attribute must
    /// NOT be hidden even though `semantically_equal` returns true.
    #[test]
    fn test_replace_changed_create_only_same_value_shown_as_known_after_apply() {
        use std::collections::HashMap;

        let from_attrs = HashMap::from([
            ("vpc_id".to_string(), Value::String("vpc-123".to_string())),
            (
                "cidr_block".to_string(),
                Value::String("10.0.1.0/24".to_string()),
            ),
        ]);
        let to_attrs = HashMap::from([
            ("_binding".to_string(), Value::String("subnet".to_string())),
            ("vpc_id".to_string(), Value::String("vpc-123".to_string())),
            (
                "cidr_block".to_string(),
                Value::String("10.0.1.0/24".to_string()),
            ),
        ]);

        // Verify the precondition: the values are semantically equal
        assert!(
            Value::String("vpc-123".to_string())
                .semantically_equal(&Value::String("vpc-123".to_string())),
            "precondition: old and new vpc_id should be semantically equal"
        );

        let output = format_replace_changed_attrs(
            &from_attrs,
            &to_attrs,
            &["vpc_id".to_string()],
            "    ",
            &[],
        );

        // vpc_id must appear in output, not be hidden
        assert!(
            output.contains("vpc_id"),
            "Expected vpc_id in output, got: {}",
            output
        );
        // Must show "known after apply" annotation
        assert!(
            output.contains("known after apply"),
            "Expected 'known after apply' in output, got: {}",
            output
        );
        // Must show "forces replacement" annotation
        assert!(
            output.contains("forces replacement"),
            "Expected 'forces replacement' in output, got: {}",
            output
        );
        // Must show the current value
        assert!(
            output.contains("vpc-123"),
            "Expected current value 'vpc-123' in output, got: {}",
            output
        );
    }

    /// Test that cascade_ref_hints causes the display to show the original ResourceRef
    /// binding instead of the resolved value for cascade-triggered same-value attributes.
    #[test]
    fn test_replace_cascade_ref_hints_show_binding() {
        use std::collections::HashMap;

        let from_attrs = HashMap::from([(
            "vpc_id".to_string(),
            Value::String("vpc-0bf023ff87bf1aa0c".to_string()),
        )]);
        let to_attrs = HashMap::from([(
            "vpc_id".to_string(),
            Value::String("vpc-0bf023ff87bf1aa0c".to_string()),
        )]);

        let hints = vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())];

        let output = format_replace_changed_attrs(
            &from_attrs,
            &to_attrs,
            &["vpc_id".to_string()],
            "    ",
            &hints,
        );

        // Must show the ResourceRef hint instead of the resolved value on the right side
        assert!(
            output.contains("vpc.vpc_id"),
            "Expected 'vpc.vpc_id' hint in output, got: {}",
            output
        );
        // Must still show the old resolved value
        assert!(
            output.contains("vpc-0bf023ff87bf1aa0c"),
            "Expected old value in output, got: {}",
            output
        );
        // Must show forces replacement annotation
        assert!(
            output.contains("forces replacement, known after apply"),
            "Expected 'forces replacement, known after apply' in output, got: {}",
            output
        );
    }

    /// Test that cascading update diff only shows attributes referencing the replaced binding,
    /// not attributes with false diffs due to DSL vs AWS format mismatch (issue #958).
    #[test]
    fn test_format_cascading_update_diff_excludes_non_ref_attributes() {
        use std::collections::HashMap;

        let cascade = CascadingUpdate {
            id: ResourceId::new("ec2.subnet", "subnet"),
            from: Box::new(State::existing(
                ResourceId::new("ec2.subnet", "subnet"),
                HashMap::from([
                    (
                        "vpc_id".to_string(),
                        Value::String("vpc-old123".to_string()),
                    ),
                    (
                        "availability_zone".to_string(),
                        Value::String("ap-northeast-1a".to_string()),
                    ),
                    (
                        "cidr_block".to_string(),
                        Value::String("10.0.1.0/24".to_string()),
                    ),
                ]),
            )),
            to: Resource::new("ec2.subnet", "subnet")
                .with_attribute(
                    "vpc_id",
                    Value::ResourceRef {
                        binding_name: "vpc".to_string(),
                        attribute_name: "vpc_id".to_string(),
                    },
                )
                .with_attribute(
                    "availability_zone",
                    Value::UnresolvedIdent(
                        "awscc.AvailabilityZone.ap_northeast_1a".to_string(),
                        None,
                    ),
                )
                .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string())),
        };

        let replaced_binding = "vpc";
        let output = format_cascading_update_diff(&cascade, "    ", replaced_binding);

        // vpc_id references the replaced binding "vpc", so it SHOULD appear
        assert!(
            output.contains("vpc_id"),
            "Expected vpc_id in diff output, got: {}",
            output
        );
        // availability_zone does NOT reference the replaced binding, so it should NOT appear
        // (this was the false diff in issue #958)
        assert!(
            !output.contains("availability_zone"),
            "availability_zone should not appear in diff (false diff), got: {}",
            output
        );
        // cidr_block does NOT reference the replaced binding, so it should NOT appear
        assert!(
            !output.contains("cidr_block"),
            "cidr_block should not appear in diff, got: {}",
            output
        );
    }

    /// Test that cascading update diff shows List attributes containing ResourceRef
    /// to the replaced binding (e.g., security_group_ids = [sg.group_id]).
    #[test]
    fn test_format_cascading_update_diff_includes_list_with_ref() {
        use std::collections::HashMap;

        let cascade = CascadingUpdate {
            id: ResourceId::new("ec2.instance", "instance"),
            from: Box::new(State::existing(
                ResourceId::new("ec2.instance", "instance"),
                HashMap::from([(
                    "security_group_ids".to_string(),
                    Value::List(vec![Value::String("sg-old123".to_string())]),
                )]),
            )),
            to: Resource::new("ec2.instance", "instance").with_attribute(
                "security_group_ids",
                Value::List(vec![Value::ResourceRef {
                    binding_name: "sg".to_string(),
                    attribute_name: "group_id".to_string(),
                }]),
            ),
        };

        let replaced_binding = "sg";
        let output = format_cascading_update_diff(&cascade, "    ", replaced_binding);

        assert!(
            output.contains("security_group_ids"),
            "Expected security_group_ids in diff output, got: {}",
            output
        );
    }

    /// Issue #949: Plan tree structure is lost for mixed effect plans.
    ///
    /// When a plan contains Delete effects, they have no resource (only `id`
    /// and `identifier`), so:
    /// - No binding is registered in `binding_to_effect`
    /// - No dependencies are extracted
    /// - No `effect_bindings` or `effect_types` entry is created
    ///
    /// This means Delete effects are invisible to the tree algorithm and
    /// cannot participate as roots or children. The tree degrades to a flat
    /// list for any plan containing Delete effects.
    ///
    /// Scenario:
    ///   - VPC (Update) — root, no dependencies
    ///   - SG (Replace) — depends on VPC via ResourceRef
    ///   - Subnet (Delete) — should be child of VPC, but Delete has no resource
    ///
    /// Expected tree:
    ///   vpc (update)
    ///   ├── sg (replace)
    ///   └── subnet (delete)
    ///
    /// Actual: Delete effect for subnet is either missing from the tree or
    /// appears as a disconnected root because it has no binding or dependency info.
    #[test]
    fn test_mixed_plan_tree_with_delete_effect() {
        // VPC: Update effect (has `to` resource with binding)
        let vpc_to = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let vpc_from = State::existing(
            ResourceId::new("ec2.vpc", "vpc"),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]),
        );

        // SG: Replace effect (has `to` resource that depends on VPC)
        let sg_to = make_resource("ec2.security_group", "sg", "sg", &["vpc"]);
        let sg_from = State::existing(
            ResourceId::new("ec2.security_group", "sg"),
            HashMap::from([(
                "ref_vpc".to_string(),
                Value::String("vpc-old123".to_string()),
            )]),
        );

        // Subnet: Delete effect (only has id and identifier — no resource, no deps)
        // In the original DSL, subnet depends on VPC, but Delete loses that info.
        let subnet_delete = Effect::Delete {
            id: ResourceId::new("ec2.subnet", "subnet"),
            identifier: "subnet-12345".to_string(),
            lifecycle: LifecycleConfig::default(),
            binding: Some("subnet".to_string()),
            dependencies: HashSet::from(["vpc".to_string()]),
        };

        let mut plan = Plan::new();
        plan.add(Effect::Update {
            id: ResourceId::new("ec2.vpc", "vpc"),
            from: Box::new(vpc_from),
            to: vpc_to,
            changed_attributes: vec!["cidr_block".to_string()],
        });
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.security_group", "sg"),
            from: Box::new(sg_from),
            to: sg_to,
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["ref_vpc".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });
        plan.add(subnet_delete);

        let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

        // VPC (idx 0) should be the only root
        assert_eq!(
            roots,
            vec![0],
            "VPC should be the only root. Got roots: {:?} (bindings: {:?})",
            roots,
            roots
                .iter()
                .filter_map(|i| effect_bindings.get(i))
                .collect::<Vec<_>>()
        );

        // SG (idx 1) should be a child of VPC
        let vpc_children: Vec<usize> = dependents.get(&0).cloned().unwrap_or_default();
        assert!(
            vpc_children.contains(&1),
            "SG (idx 1) should be a child of VPC. VPC children: {:?}",
            vpc_children
        );

        // Subnet Delete (idx 2) should also be a child of VPC.
        // Currently fails because Delete effects have no binding/dependency info.
        assert!(
            vpc_children.contains(&2),
            "Subnet Delete (idx 2) should be a child of VPC, but Delete effects \
             have no resource/binding/dependency info so the tree cannot place them. \
             VPC children: {:?}, all roots: {:?}",
            vpc_children,
            roots
        );
    }

    /// Issue #972: When ResourceRef values are resolved to literal strings
    /// (as happens with --refresh=false), the tree algorithm loses dependency
    /// information and places the resource as a top-level root instead of
    /// nesting it under its parent.
    ///
    /// Scenario (mixed_operations fixture):
    ///   - VPC: Update effect (existing, tags changed)
    ///   - SG: Create effect with vpc_id = "vpc-0123456789abcdef0"
    ///     (resolved from vpc.vpc_id by resolve_refs_with_state)
    ///
    /// The SG should be nested under VPC because in the DSL it has
    /// `vpc_id = vpc.vpc_id`, but after resolve_refs_with_state() the
    /// ResourceRef is replaced with a plain string, so
    /// get_resource_dependencies() finds no dependencies.
    #[test]
    fn test_resolved_ref_loses_dependency_for_tree_nesting() {
        // VPC: Update effect (tags changed)
        let vpc_to = make_resource("ec2.vpc", "vpc", "vpc", &[]);
        let vpc_from = State::existing(
            ResourceId::new("ec2.vpc", "vpc"),
            HashMap::from([(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]),
        );

        // SG: Create effect with RESOLVED ref (string instead of ResourceRef).
        // This is what happens after resolve_refs_with_state() runs:
        // vpc_id = vpc.vpc_id becomes vpc_id = "vpc-0123456789abcdef0"
        let mut sg = Resource::new("ec2.security_group", "sg");
        sg.attributes
            .insert("_binding".to_string(), Value::String("sg".to_string()));
        // This is the resolved value — a plain string, NOT a ResourceRef
        sg.attributes.insert(
            "vpc_id".to_string(),
            Value::String("vpc-0123456789abcdef0".to_string()),
        );
        sg.attributes.insert(
            "group_description".to_string(),
            Value::String("Test security group".to_string()),
        );
        // _dependency_bindings is saved by resolve_refs_with_state() before
        // ResourceRef values are resolved to strings.
        sg.attributes.insert(
            "_dependency_bindings".to_string(),
            Value::List(vec![Value::String("vpc".to_string())]),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Update {
            id: ResourceId::new("ec2.vpc", "vpc"),
            from: Box::new(vpc_from),
            to: vpc_to,
            changed_attributes: vec!["tags".to_string()],
        });
        plan.add(Effect::Create(sg));

        let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

        // VPC (idx 0) should be the only root.
        // SG has _dependency_bindings metadata that preserves the dependency
        // on "vpc", so get_resource_dependencies() recovers it.
        assert_eq!(
            roots,
            vec![0],
            "VPC should be the only root. SG should be nested under VPC \
             because it references vpc.vpc_id in the DSL. Got roots: {:?} \
             (bindings: {:?})",
            roots,
            roots
                .iter()
                .filter_map(|i| effect_bindings.get(i))
                .collect::<Vec<_>>()
        );

        // SG (idx 1) should be a child of VPC (idx 0)
        let vpc_children: Vec<usize> = dependents.get(&0).cloned().unwrap_or_default();
        assert!(
            vpc_children.contains(&1),
            "SG (idx 1) should be a child of VPC. VPC children: {:?}",
            vpc_children
        );
    }
}
