use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use colored::Colorize;

use carina_core::detail_rows::{
    DetailRow, ListOfMapsDiffField, ListOfMapsDiffModified, MapDiffEntryIR, build_detail_rows,
};
#[cfg(test)]
use carina_core::diff_helpers::compute_map_diff;
#[cfg(test)]
use carina_core::effect::CascadingUpdate;
use carina_core::effect::Effect;
use carina_core::plan::Plan;
#[cfg(test)]
use carina_core::plan_tree::shorten_attr_name;
use carina_core::plan_tree::{
    build_dependency_graph, build_single_parent_tree, extract_compact_hint,
};
use carina_core::resource::{ResourceId, Value};
use carina_core::schema::SchemaRegistry;
use carina_core::value::format_value_pretty;
#[cfg(test)]
use carina_core::value::{format_value, format_value_with_key, is_list_of_maps, map_similarity};

use crate::DetailLevel;

/// Check if a resource has a `let` binding (i.e., is not anonymous).
fn has_binding(resource: &carina_core::resource::Resource) -> bool {
    resource.binding.is_some()
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

/// Format a value in a deferred for-expression template.
/// Placeholder values (`Value::Unknown`) are shown dimmed; resolved
/// values are shown normally.
fn format_deferred_value(value: &Value) -> String {
    /// Catch-all placeholder text for value variants whose contents
    /// cannot be sensibly inlined into a deferred-for template
    /// (e.g. `Interpolation`, `FunctionCall`, `Secret`). The wording
    /// matches `render_unknown(ForValue)` so display stays uniform
    /// across the deferred and resolved-Unknown paths.
    const DEFERRED_FALLBACK: &str = "(known after upstream apply)";

    match value {
        Value::Unknown(reason) => {
            format!("{}", carina_core::value::render_unknown(reason).dimmed())
        }
        Value::String(s) => format!("'{}'", s),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::ResourceRef { path } => path.to_dot_string().to_string(),
        Value::List(items) => {
            let formatted: Vec<String> = items.iter().map(format_deferred_value).collect();
            format!("[{}]", formatted.join(", "))
        }
        Value::Map(map) => {
            let formatted: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{} = {}", k, format_deferred_value(v)))
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
        _ => format!("{}", DEFERRED_FALLBACK.dimmed()),
    }
}

/// Format an export value for plan display.
fn format_export_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("'{}'", s),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::ResourceRef { path } => path.to_dot_string().to_string(),
        Value::List(items) => {
            let formatted: Vec<String> = items.iter().map(format_export_value).collect();
            format!("[{}]", formatted.join(", "))
        }
        Value::Map(map) => {
            let formatted: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{} = {}", k, format_export_value(v)))
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
        _ => "(known after apply)".to_string(),
    }
}

/// Format a `serde_json::Value` for export display (old values stored in state).
fn format_json_export_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("'{}'", s),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(items) => {
            let formatted: Vec<String> = items.iter().map(format_json_export_value).collect();
            format!("[{}]", formatted.join(", "))
        }
        serde_json::Value::Object(map) => {
            let formatted: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{} = {}", k, format_json_export_value(v)))
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
    }
}

/// Format a single export change entry (add/modify/remove) as a displayable line.
fn format_export_change(change: &crate::commands::plan::ExportChange) -> String {
    use crate::commands::plan::ExportChange;
    let mut out = String::new();
    match change {
        ExportChange::Added {
            name,
            type_expr,
            new_value,
        } => {
            let type_str = type_expr
                .as_ref()
                .map(|t| format!(": {}", t))
                .unwrap_or_default();
            writeln!(
                out,
                "  {} {}{} = {}",
                "+".green(),
                name,
                type_str.dimmed(),
                format_export_value(new_value)
            )
            .unwrap();
        }
        ExportChange::Modified {
            name,
            type_expr,
            old_json,
            new_value,
        } => {
            let type_str = type_expr
                .as_ref()
                .map(|t| format!(": {}", t))
                .unwrap_or_default();
            writeln!(
                out,
                "  {} {}{} = {} {} {}",
                "~".yellow(),
                name,
                type_str.dimmed(),
                format_json_export_value(old_json),
                "→".dimmed(),
                format_export_value(new_value)
            )
            .unwrap();
        }
        ExportChange::Removed { name, old_json } => {
            writeln!(
                out,
                "  {} {} = {}",
                "-".red(),
                name,
                format_json_export_value(old_json)
            )
            .unwrap();
        }
    }
    out
}

pub fn print_plan(
    plan: &Plan,
    detail: DetailLevel,
    delete_attributes: &HashMap<ResourceId, HashMap<String, Value>>,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    export_changes: &[crate::commands::plan::ExportChange],
    deferred_for_expressions: &[carina_core::parser::DeferredForExpression],
) {
    print!(
        "{}",
        format_plan(
            plan,
            detail,
            delete_attributes,
            schemas,
            moved_origins,
            export_changes,
            deferred_for_expressions,
        )
    );
}

/// Format a plan as a string for display.
///
/// This is the core formatting logic used by `print_plan`. Returning a `String`
/// enables snapshot testing and other programmatic uses of the plan output.
pub fn format_plan(
    plan: &Plan,
    detail: DetailLevel,
    delete_attributes: &HashMap<ResourceId, HashMap<String, Value>>,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    export_changes: &[crate::commands::plan::ExportChange],
    deferred_for_expressions: &[carina_core::parser::DeferredForExpression],
) -> String {
    let mut out = String::new();

    if plan.is_empty() && deferred_for_expressions.is_empty() && export_changes.is_empty() {
        writeln!(
            out,
            "{}",
            "No changes. Infrastructure is up-to-date.".green()
        )
        .unwrap();
        return out;
    }

    writeln!(out, "{}", "Execution Plan:".cyan().bold()).unwrap();
    writeln!(out).unwrap();

    let attrs = if delete_attributes.is_empty() {
        None
    } else {
        Some(delete_attributes)
    };
    out.push_str(&format_plan_tree(
        plan,
        detail,
        attrs,
        schemas,
        moved_origins,
    ));

    // Show deferred for-expressions
    for deferred in deferred_for_expressions {
        out.push_str(&format_deferred_for_expression(deferred));
    }

    // Show export changes if any
    if !export_changes.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "{}", "Exports:".cyan().bold()).unwrap();
        writeln!(out).unwrap();
        for change in export_changes {
            out.push_str(&format_export_change(change));
        }
    }

    writeln!(out).unwrap();
    let summary = plan.summary();
    let mut parts = Vec::new();
    if summary.read > 0 {
        parts.push(format!("{} to read", summary.read.to_string().cyan()));
    }
    if summary.import > 0 {
        parts.push(format!("{} to import", summary.import.to_string().cyan()));
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
    if summary.remove > 0 {
        parts.push(format!(
            "{} to remove from state",
            summary.remove.to_string().red()
        ));
    }
    if summary.moved > 0 {
        parts.push(format!("{} to move", summary.moved.to_string().yellow()));
    }
    if !deferred_for_expressions.is_empty() {
        parts.push(format!(
            "{} deferred",
            deferred_for_expressions.len().to_string().cyan()
        ));
    }
    if !export_changes.is_empty() {
        parts.push(format!(
            "{} export change(s)",
            export_changes.len().to_string().cyan()
        ));
    }
    writeln!(out, "Plan: {}.", parts.join(", ")).unwrap();
    writeln!(out).unwrap();

    out
}

/// Format a single deferred for-expression for plan display.
fn format_deferred_for_expression(deferred: &carina_core::parser::DeferredForExpression) -> String {
    let mut out = String::new();
    writeln!(out, "  {} {}", "?".cyan(), deferred.header).unwrap();
    writeln!(
        out,
        "      {}",
        format!(
            "expands to one {} per element (count known after upstream apply)",
            deferred.resource_type
        )
        .dimmed()
    )
    .unwrap();
    // Sort attributes for deterministic output
    let mut attrs: Vec<_> = deferred.attributes.iter().collect();
    attrs.sort_by_key(|(k, _)| k.clone());
    for (key, value) in &attrs {
        let formatted = format_deferred_value(value);
        writeln!(out, "      {}: {}", key.dimmed(), formatted).unwrap();
    }
    writeln!(out).unwrap();
    out
}

/// Format a destroy plan for display.
///
/// Uses the same tree-building logic as `format_plan` but with a
/// "Destroy Plan:" header and no summary line.
///
/// `delete_attributes` maps each resource's `ResourceId` to its current state
/// attributes, so the display can show what will be deleted.
pub fn format_destroy_plan(
    plan: &Plan,
    detail: DetailLevel,
    delete_attributes: &HashMap<ResourceId, HashMap<String, Value>>,
) -> String {
    let mut out = String::new();

    if plan.is_empty() {
        return out;
    }

    writeln!(out, "{}", "Destroy Plan:".red().bold()).unwrap();
    writeln!(out).unwrap();

    out.push_str(&format_plan_tree(
        plan,
        detail,
        Some(delete_attributes),
        None,
        &HashMap::new(),
    ));

    out
}

/// Holds shared state and configuration for recursive tree rendering.
///
/// Groups the parameters that are threaded through every recursive call of
/// `format_effect_tree`, reducing parameter count from 13 to 6.
struct TreeRenderContext<'a> {
    out: String,
    printed: HashSet<usize>,
    plan: &'a Plan,
    dependents: HashMap<usize, Vec<usize>>,
    detail: DetailLevel,
    delete_attributes: Option<&'a HashMap<ResourceId, HashMap<String, Value>>>,
    schemas: Option<&'a SchemaRegistry>,
    moved_origins: &'a HashMap<ResourceId, ResourceId>,
    /// ResourceIds that are targets of Update or Replace effects.
    /// Used to skip Move line display when the move is already shown via annotation.
    update_or_replace_targets: HashSet<ResourceId>,
}

impl<'a> TreeRenderContext<'a> {
    fn format_effect_tree(
        &mut self,
        idx: usize,
        indent: usize,
        is_last: bool,
        prefix: &str,
        parent_binding: Option<&str>,
    ) -> bool {
        if self.printed.contains(&idx) {
            return false;
        }
        self.printed.insert(idx);

        let effect = &self.plan.effects()[idx];
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
            Effect::Import { .. } => "<-".cyan().bold(),
            Effect::Remove { .. } => "x".red().bold(),
            Effect::Move { .. } => "->".yellow().bold(),
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

        // --- Resource header line ---
        match effect {
            Effect::Create(r) => {
                if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(r, r.id.name_str(), parent_binding);
                    writeln!(
                        self.out,
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
                        self.out,
                        "{}{}{} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        r.id.display_type().cyan().bold(),
                        r.id.name_str().white().bold()
                    )
                    .unwrap();
                }
            }
            Effect::Update { id, to, .. } => {
                let moved_note = self
                    .moved_origins
                    .get(id)
                    .map(|from| format!(" (moved from: {})", from.name_str()));
                if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(to, id.name_str(), parent_binding);
                    writeln!(
                        self.out,
                        "{}{}{} {} {}{}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        name_part.yellow().bold(),
                        moved_note.as_deref().unwrap_or("").yellow()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        self.out,
                        "{}{}{} {} {}{}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        id.name_str().yellow().bold(),
                        moved_note.as_deref().unwrap_or("").yellow()
                    )
                    .unwrap();
                }
            }
            Effect::Replace {
                id, to, lifecycle, ..
            } => {
                let replace_note = if lifecycle.create_before_destroy {
                    "(must be replaced, create before destroy)"
                } else {
                    "(must be replaced)"
                };
                let moved_note = self
                    .moved_origins
                    .get(id)
                    .map(|from| format!(" (moved from: {})", from.name_str()));
                if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(to, id.name_str(), parent_binding);
                    writeln!(
                        self.out,
                        "{}{}{} {} {} {}{}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        name_part.magenta().bold(),
                        replace_note.magenta(),
                        moved_note.as_deref().unwrap_or("").magenta()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        self.out,
                        "{}{}{} {} {} {}{}",
                        base_indent,
                        connector,
                        colored_symbol,
                        id.display_type().cyan().bold(),
                        id.name_str().magenta().bold(),
                        replace_note.magenta(),
                        moved_note.as_deref().unwrap_or("").magenta()
                    )
                    .unwrap();
                }
            }
            Effect::Delete { id, binding, .. } => {
                let display_name = binding.as_deref().unwrap_or(id.name_str());
                writeln!(
                    self.out,
                    "{}{}{} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    display_name.red().bold().strikethrough()
                )
                .unwrap();
            }
            Effect::Read { resource } => {
                if self.detail == DetailLevel::None {
                    let name_part =
                        format_compact_name(resource, resource.id.name_str(), parent_binding);
                    writeln!(
                        self.out,
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
                        self.out,
                        "{}{}{} {} {} {}",
                        base_indent,
                        connector,
                        colored_symbol,
                        resource.id.display_type().cyan().bold(),
                        resource.id.name_str().cyan().bold(),
                        "(data source)".dimmed()
                    )
                    .unwrap();
                }
            }
            Effect::Import { id, identifier } => {
                writeln!(
                    self.out,
                    "{}{}{} {} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name_str().cyan().bold(),
                    format!("(import: {})", identifier).dimmed()
                )
                .unwrap();
            }
            Effect::Remove { id } => {
                writeln!(
                    self.out,
                    "{}{}{} {} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.display_type().cyan().bold(),
                    id.name_str().red().bold(),
                    "(remove from state)".dimmed()
                )
                .unwrap();
            }
            Effect::Move { from, to } => {
                // Skip Move line display when an Update/Replace effect already exists
                // for this target — those effects show "(moved from: ...)" annotation.
                // Pure moves (no Update/Replace) must be displayed.
                if self.update_or_replace_targets.contains(to) {
                    return false;
                }
                writeln!(
                    self.out,
                    "{}{}{} {} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    to.display_type().cyan().bold(),
                    to.name_str().yellow().bold(),
                    format!("(moved from: {})", from.name_str()).dimmed()
                )
                .unwrap();
            }
        }

        // --- Detail rows (attributes) ---
        if self.detail != DetailLevel::None {
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

            let detail_rows = build_detail_rows(
                effect,
                self.schemas,
                self.detail.to_core(),
                self.delete_attributes,
            );

            if !detail_rows.is_empty() {
                has_displayed_attrs = true;
            }

            for row in &detail_rows {
                render_detail_row(&mut self.out, row, effect, &attr_prefix);
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
                    Effect::Delete { .. }
                    | Effect::Import { .. }
                    | Effect::Remove { .. }
                    | Effect::Move { .. } => None,
                };
                resource.and_then(|r| r.binding.clone())
            }
        };

        // Print children (dependents)
        let children = self.dependents.get(&idx).cloned().unwrap_or_default();
        let unprinted_children: Vec<_> = children
            .iter()
            .filter(|c| !self.printed.contains(c))
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
            writeln!(self.out, "{}{}│", base_indent, new_prefix).unwrap();
        }

        for (i, child_idx) in unprinted_children.iter().enumerate() {
            let child_is_last = i == unprinted_children.len() - 1;
            let child_had_attrs = self.format_effect_tree(
                *child_idx,
                indent + 1,
                child_is_last,
                &new_prefix,
                current_binding.as_deref(),
            );
            // Add separator line between siblings when previous sibling displayed attributes
            if child_had_attrs && !child_is_last {
                let separator_continuation = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                let separator_prefix = if indent == 0 {
                    format!("{}  ", attr_base)
                } else {
                    format!("{}   ", separator_continuation)
                };
                writeln!(self.out, "{}{}│", base_indent, separator_prefix).unwrap();
            }
            if child_had_attrs {
                has_displayed_attrs = true;
            }
        }

        has_displayed_attrs
    }
}

/// Format the tree body of a plan (no header, no summary).
///
/// `delete_attributes` optionally provides current state attributes for Delete
/// effects, allowing the display to show what will be deleted.
fn format_plan_tree(
    plan: &Plan,
    detail: DetailLevel,
    delete_attributes: Option<&HashMap<ResourceId, HashMap<String, Value>>>,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
) -> String {
    // Build dependency graph from effects
    let graph = build_dependency_graph(plan);

    // Build the single-parent tree with sorted siblings
    let (roots, dependents) = build_single_parent_tree(plan, &graph);

    let update_or_replace_targets: HashSet<ResourceId> = plan
        .effects()
        .iter()
        .filter_map(|e| match e {
            Effect::Update { to, .. } => Some(to.id.clone()),
            Effect::Replace { to, .. } => Some(to.id.clone()),
            _ => None,
        })
        .collect();

    let mut ctx = TreeRenderContext {
        out: String::new(),
        printed: HashSet::new(),
        plan,
        dependents,
        detail,
        delete_attributes,
        schemas,
        moved_origins,
        update_or_replace_targets,
    };

    // Print from roots
    for (i, root_idx) in roots.iter().enumerate() {
        ctx.format_effect_tree(*root_idx, 0, i == roots.len() - 1, "", None);
    }

    // Print any remaining effects that weren't reachable from roots
    // (e.g., circular dependencies or isolated resources)
    let remaining: Vec<_> = (0..plan.effects().len())
        .filter(|idx| !ctx.printed.contains(idx))
        .collect();
    for idx in remaining {
        ctx.format_effect_tree(idx, 0, true, "", None);
    }

    ctx.out
}

/// Split a string by `, ` at the top level, respecting nested brackets, braces, and quotes.
fn split_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut in_quote = false;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'[' | b'{' if !in_quote => depth += 1,
            b']' | b'}' if !in_quote => depth -= 1,
            b',' if !in_quote && depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b' ' => {
                parts.push(&s[start..i]);
                start = i + 2;
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if start < s.len() {
        parts.push(&s[start..]);
    }
    parts
}

/// Color a single atomic value (not a list or map).
fn color_atom(rendered: &str, ref_binding: bool) -> String {
    if ref_binding {
        return rendered.cyan().to_string();
    }
    if rendered.starts_with('"') && rendered.ends_with('"') {
        return rendered.green().to_string();
    }
    if rendered == "true" || rendered == "false" {
        return rendered.yellow().to_string();
    }
    if !rendered.is_empty()
        && rendered
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        let first = rendered.chars().next().unwrap();
        if first.is_ascii_digit() || first == '-' {
            return rendered.white().to_string();
        }
    }
    if rendered.contains('.') && !rendered.contains(' ') && !rendered.starts_with('{') {
        return rendered.magenta().to_string();
    }
    rendered.to_string()
}

/// Color a single atomic value with dimmed modifier.
fn color_atom_dimmed(rendered: &str) -> String {
    if rendered.starts_with('"') && rendered.ends_with('"') {
        return rendered.green().dimmed().to_string();
    }
    if rendered == "true" || rendered == "false" {
        return rendered.yellow().dimmed().to_string();
    }
    if !rendered.is_empty()
        && rendered
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        let first = rendered.chars().next().unwrap();
        if first.is_ascii_digit() || first == '-' {
            return rendered.white().dimmed().to_string();
        }
    }
    if rendered.contains('.') && !rendered.contains(' ') && !rendered.starts_with('{') {
        return rendered.magenta().dimmed().to_string();
    }
    rendered.dimmed().to_string()
}

/// Determine ANSI color for a rendered value string based on its type.
///
/// Mirrors the `value_color()` logic in `carina-tui/src/ui.rs`:
/// - Quoted strings (`"..."`) → green
/// - Booleans (`true`/`false`) → yellow
/// - Numbers → white
/// - DSL identifiers (dot-notation, e.g. `awscc.Region.ap_northeast_1`) → magenta
/// - ResourceRef values → cyan (handled separately via `ref_binding`)
/// - Lists (`[...]`) → each element colored individually
/// - Maps (`{...}`) → each value colored individually
fn colored_value(rendered: &str, ref_binding: bool) -> String {
    // Multi-line input from `format_value_pretty`'s vertical layout
    // (`format_list_of_scalars_vertical` / `format_map_vertical`). The inline
    // `starts_with('[') && ends_with(']')` branch below would `split_top_level`
    // the body and re-`join(", ")` it, collapsing the layout back to inline.
    // Color atoms in place and preserve newlines + leading indentation +
    // structural punctuation (`[`, `]`, `{`, `}`, `,`) verbatim instead.
    if rendered.contains('\n') {
        return color_lines(rendered, ref_binding);
    }
    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return rendered.to_string();
        }
        let elements = split_top_level(inner);
        let colored_elements: Vec<String> = elements
            .iter()
            .map(|e| colored_value(e.trim(), ref_binding))
            .collect();
        return format!("[{}]", colored_elements.join(", "));
    }
    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return rendered.to_string();
        }
        let entries = split_top_level(inner);
        let colored_entries: Vec<String> = entries
            .iter()
            .map(|entry| {
                if let Some(colon_pos) = entry.find(": ") {
                    let key = &entry[..colon_pos];
                    let val = &entry[colon_pos + 2..];
                    format!("{}: {}", key, colored_value(val, false))
                } else {
                    entry.to_string()
                }
            })
            .collect();
        return format!("{{{}}}", colored_entries.join(", "));
    }
    color_atom(rendered, ref_binding)
}

/// Color a multi-line `format_value_pretty` payload while preserving its
/// vertical layout. For each line, separate leading whitespace and a trailing
/// `,`, then color the middle. Atoms that look like list items (`"x"`, `42`)
/// or `key: value` map entries get atom-coloring; structural lines like `[`,
/// `]`, `{`, `}` fall through `color_atom` unchanged.
fn color_lines(rendered: &str, ref_binding: bool) -> String {
    let mut out = String::with_capacity(rendered.len());
    for (i, line) in rendered.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let indent_end = line
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(line.len());
        let (indent, body) = line.split_at(indent_end);
        out.push_str(indent);
        if body.is_empty() {
            continue;
        }
        let (core, trailing_comma) = if let Some(stripped) = body.strip_suffix(',') {
            (stripped, ",")
        } else {
            (body, "")
        };
        // Split `key: value` to color only the value half. Safe because keys
        // emitted by `format_map_vertical` are bare attribute names (never
        // quoted strings that could contain `": "`).
        if let Some(colon_pos) = core.find(": ") {
            let key = &core[..colon_pos];
            let val = &core[colon_pos + 2..];
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&color_atom(val, ref_binding));
        } else {
            out.push_str(&color_atom(core, ref_binding));
        }
        out.push_str(trailing_comma);
    }
    out
}

/// Apply type-based coloring to a value, with dimmed modifier for default values.
fn colored_value_dimmed(rendered: &str) -> String {
    // List: color each element individually
    if rendered.starts_with('[') && rendered.ends_with(']') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return rendered.dimmed().to_string();
        }
        let elements = split_top_level(inner);
        let colored_elements: Vec<String> = elements
            .iter()
            .map(|e| colored_value_dimmed(e.trim()))
            .collect();
        return format!(
            "{}{}{}",
            "[".dimmed(),
            colored_elements.join(&", ".dimmed().to_string()),
            "]".dimmed()
        );
    }
    // Map: color each value individually
    if rendered.starts_with('{') && rendered.ends_with('}') {
        let inner = &rendered[1..rendered.len() - 1];
        if inner.is_empty() {
            return rendered.dimmed().to_string();
        }
        let entries = split_top_level(inner);
        let colored_entries: Vec<String> = entries
            .iter()
            .map(|entry| {
                if let Some(colon_pos) = entry.find(": ") {
                    let key = &entry[..colon_pos];
                    let val = &entry[colon_pos + 2..];
                    format!(
                        "{}{} {}",
                        key.dimmed(),
                        ":".dimmed(),
                        colored_value_dimmed(val)
                    )
                } else {
                    entry.dimmed().to_string()
                }
            })
            .collect();
        return format!(
            "{}{}{}",
            "{".dimmed(),
            colored_entries.join(&", ".dimmed().to_string()),
            "}".dimmed()
        );
    }
    color_atom_dimmed(rendered)
}

/// Render a single `DetailRow` into the output string with ANSI colors.
fn render_detail_row(out: &mut String, row: &DetailRow, effect: &Effect, attr_prefix: &str) {
    match row {
        DetailRow::Attribute {
            key,
            value,
            ref_binding,
            annotation,
        } => {
            let cv = match effect {
                Effect::Delete { .. } => value.red().strikethrough().to_string(),
                _ => colored_value(value, ref_binding.is_some()),
            };
            if let Some(ann) = annotation {
                writeln!(out, "{}{}: {}  {}", attr_prefix, key, cv, ann.dimmed()).unwrap();
            } else {
                writeln!(out, "{}{}: {}", attr_prefix, key, cv).unwrap();
            }
        }
        DetailRow::PrettyAttribute { key, value } => {
            // attr_prefix may contain the tree glyph `│` (U+2502, 1 column
            // wide but 3 bytes in UTF-8), so use `chars().count()` for column
            // count, not `.len()`.
            let layout = carina_core::value::PrettyLayout {
                parent_indent_cols: attr_prefix.chars().count(),
                key,
            };
            let pretty = format_value_pretty(value, layout);
            let cv = match effect {
                Effect::Delete { .. } => pretty.red().strikethrough().to_string(),
                _ => colored_value(&pretty, false),
            };
            writeln!(out, "{}{}: {}", attr_prefix, key, cv).unwrap();
        }
        DetailRow::MapExpanded { key, entries } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            let entry_indent_cols = attr_prefix.chars().count() + 2;
            for entry in entries {
                let layout = carina_core::value::PrettyLayout {
                    parent_indent_cols: entry_indent_cols,
                    key: &entry.key,
                };
                let pretty = carina_core::value::format_value_pretty(&entry.value, layout);
                let cv = match effect {
                    Effect::Delete { .. } => pretty.red().strikethrough().to_string(),
                    _ => colored_value(&pretty, false),
                };
                if let Some(ann) = &entry.annotation {
                    writeln!(
                        out,
                        "{}  {}: {}  {}",
                        attr_prefix,
                        entry.key,
                        cv,
                        ann.dimmed()
                    )
                    .unwrap();
                } else {
                    writeln!(out, "{}  {}: {}", attr_prefix, entry.key, cv).unwrap();
                }
            }
        }
        DetailRow::Changed { key, old, new } => {
            let new_colored = colored_value(new, false);
            writeln!(
                out,
                "{}{}: {} → {}",
                attr_prefix,
                key,
                old.red().strikethrough(),
                new_colored
            )
            .unwrap();
        }
        DetailRow::MapDiff { key, entries } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            render_map_diff_entries(out, entries, attr_prefix);
        }
        DetailRow::ListOfMapsDiff {
            key,
            unchanged,
            modified,
            added,
            removed,
        } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            render_list_of_maps_diff(out, unchanged, modified, added, removed, attr_prefix);
        }
        DetailRow::Removed { key, old } => {
            writeln!(
                out,
                "{}{}: {} → {}",
                attr_prefix,
                key,
                old.red().strikethrough(),
                "(removed)".red().strikethrough()
            )
            .unwrap();
        }
        DetailRow::Default { key, value } => {
            writeln!(
                out,
                "{}{}: {}  {}",
                attr_prefix,
                key.dimmed(),
                colored_value_dimmed(value),
                "# default".dimmed()
            )
            .unwrap();
        }
        DetailRow::ReadOnly { key } => {
            writeln!(
                out,
                "{}{}: {}",
                attr_prefix,
                key.dimmed(),
                "(known after apply)".dimmed()
            )
            .unwrap();
        }
        DetailRow::HiddenUnchanged { count } => {
            let noun = if *count == 1 {
                "attribute"
            } else {
                "attributes"
            };
            writeln!(
                out,
                "{}{}",
                attr_prefix,
                format!("# ({} unchanged {} hidden)", count, noun).dimmed()
            )
            .unwrap();
        }
        DetailRow::ReplaceChanged { key, old, new } => {
            writeln!(
                out,
                "{}{}: {} → {} {}",
                attr_prefix,
                key,
                old.red().strikethrough(),
                new.green(),
                "(forces replacement)".magenta()
            )
            .unwrap();
        }
        DetailRow::ReplaceCascade { key, old, new } => {
            writeln!(
                out,
                "{}{}: {} → {} {}",
                attr_prefix,
                key,
                old.red().strikethrough(),
                new.green(),
                "(forces replacement, known after apply)".magenta()
            )
            .unwrap();
        }
        DetailRow::ReplaceListOfMapsDiff {
            key,
            unchanged,
            modified,
            added,
            removed,
        } => {
            let suffix = format!(" {}", "(forces replacement)".magenta());
            writeln!(out, "{}{}:{}", attr_prefix, key, suffix).unwrap();
            render_list_of_maps_diff(out, unchanged, modified, added, removed, attr_prefix);
        }
        DetailRow::ReplaceMapDiff { key, entries } => {
            let suffix = format!(" {}", "(forces replacement)".magenta());
            writeln!(out, "{}{}:{}", attr_prefix, key, suffix).unwrap();
            render_map_diff_entries(out, entries, attr_prefix);
        }
        DetailRow::TemporaryNameNote {
            can_rename,
            temporary_value,
            original_value,
            attribute,
        } => {
            if *can_rename {
                writeln!(
                    out,
                    "{}  {} via temporary name \"{}\", will rename back to \"{}\" after old resource is deleted",
                    attr_prefix,
                    "note:".magenta().bold(),
                    temporary_value.magenta(),
                    original_value.green()
                )
                .unwrap();
            } else {
                writeln!(
                    out,
                    "{}  {} name will be \"{}\" (cannot rename create-only attribute \"{}\")",
                    attr_prefix,
                    "note:".magenta().bold(),
                    temporary_value.magenta(),
                    attribute.magenta()
                )
                .unwrap();
            }
        }
        DetailRow::CascadingUpdates { count, updates } => {
            writeln!(out, "{}  {} cascading update(s):", attr_prefix, count).unwrap();
            for update in updates {
                writeln!(
                    out,
                    "{}    ~ {} {}",
                    attr_prefix,
                    update.display_type.as_str().cyan(),
                    update.name.as_str().magenta()
                )
                .unwrap();
                for attr in &update.changed_attrs {
                    writeln!(
                        out,
                        "{}        {}: {} → {} {}",
                        attr_prefix,
                        attr.key,
                        attr.old.as_str().red().strikethrough(),
                        attr.new.as_str().green(),
                        "(known after apply)".dimmed()
                    )
                    .unwrap();
                }
            }
        }
    }
}

/// Render map diff entries with ANSI colors.
fn render_map_diff_entries(out: &mut String, entries: &[MapDiffEntryIR], attr_prefix: &str) {
    for entry in entries {
        match entry {
            MapDiffEntryIR::Changed { key, old, new } => {
                writeln!(
                    out,
                    "{}  {} {}: {} → {}",
                    attr_prefix,
                    "~".yellow(),
                    key,
                    old.red().strikethrough(),
                    colored_value(new, false)
                )
                .unwrap();
            }
            MapDiffEntryIR::Added { key, value } => {
                writeln!(
                    out,
                    "{}  {} {}: {}",
                    attr_prefix,
                    "+".green(),
                    key,
                    colored_value(value, false)
                )
                .unwrap();
            }
            MapDiffEntryIR::Removed { key, value } => {
                writeln!(
                    out,
                    "{}  {} {}: {}",
                    attr_prefix,
                    "-".red().strikethrough(),
                    key,
                    value.red().strikethrough()
                )
                .unwrap();
            }
            MapDiffEntryIR::NestedMapDiff { key, entries } => {
                writeln!(out, "{}    {}:", attr_prefix, key).unwrap();
                let nested_prefix = format!("{}    ", attr_prefix);
                render_map_diff_entries(out, entries, &nested_prefix);
            }
            MapDiffEntryIR::NestedListOfMapsDiff {
                key,
                modified,
                added,
                removed,
            } => {
                writeln!(out, "{}    {}:", attr_prefix, key).unwrap();
                let nested_prefix = format!("{}    ", attr_prefix);
                render_list_of_maps_diff(out, &[], modified, added, removed, &nested_prefix);
            }
        }
    }
}

/// Render a list-of-maps diff with ANSI colors.
fn render_list_of_maps_diff(
    out: &mut String,
    unchanged: &[String],
    modified: &[ListOfMapsDiffModified],
    added: &[String],
    removed: &[String],
    attr_prefix: &str,
) {
    for item in unchanged {
        writeln!(out, "{}    {}", attr_prefix, item).unwrap();
    }
    for item in modified {
        let has_nested = item
            .fields
            .iter()
            .any(|f| matches!(f, ListOfMapsDiffField::NestedMapChanged { .. }));
        if has_nested {
            writeln!(out, "{}  {} {{", attr_prefix, "~".yellow().bold()).unwrap();
            for field in &item.fields {
                match field {
                    ListOfMapsDiffField::Unchanged { key, value } => {
                        writeln!(out, "{}      {}: {}", attr_prefix, key, value).unwrap();
                    }
                    ListOfMapsDiffField::Changed { key, old, new } => {
                        writeln!(
                            out,
                            "{}    {} {}: {} → {}",
                            attr_prefix,
                            "~".yellow(),
                            key,
                            old.red().strikethrough(),
                            new.green()
                        )
                        .unwrap();
                    }
                    ListOfMapsDiffField::NestedMapChanged { key, entries } => {
                        writeln!(out, "{}      {}:", attr_prefix, key).unwrap();
                        let nested_prefix = format!("{}      ", attr_prefix);
                        render_map_diff_entries(out, entries, &nested_prefix);
                    }
                }
            }
            writeln!(out, "{}    }}", attr_prefix).unwrap();
        } else {
            let rendered_fields = render_modified_fields(&item.fields);
            writeln!(
                out,
                "{}  {} {{{}}}",
                attr_prefix,
                "~".yellow().bold(),
                rendered_fields
            )
            .unwrap();
        }
    }
    for item in added {
        writeln!(out, "{}  {} {}", attr_prefix, "+".green().bold(), item).unwrap();
    }
    for item in removed {
        writeln!(
            out,
            "{}  {} {}",
            attr_prefix,
            "-".red().bold().strikethrough(),
            item.red().strikethrough()
        )
        .unwrap();
    }
}

/// Render modified fields from structured IR into colored output.
fn render_modified_fields(fields: &[ListOfMapsDiffField]) -> String {
    let mut result_parts = Vec::new();
    for field in fields {
        match field {
            ListOfMapsDiffField::Unchanged { key, value } => {
                result_parts.push(format!("{}: {}", key, value));
            }
            ListOfMapsDiffField::Changed { key, old, new } => {
                result_parts.push(format!(
                    "{}: {} → {}",
                    key,
                    old.red().strikethrough(),
                    new.green()
                ));
            }
            ListOfMapsDiffField::NestedMapChanged { key, .. } => {
                result_parts.push(format!("{}: (nested changes)", key));
            }
        }
    }
    result_parts.join(", ")
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
        Effect::Delete { id, binding, .. } => {
            let display_name = binding.as_deref().unwrap_or(id.name_str());
            format!("Delete {}.{}", id.display_type(), display_name)
        }
        Effect::Read { resource } => {
            format!("Read {}", resource.id)
        }
        Effect::Import { id, identifier } => {
            format!("Import {} (id: {})", id, identifier)
        }
        Effect::Remove { id } => {
            format!("Remove {} from state", id)
        }
        Effect::Move { from, to } => {
            format!("Move {} -> {}", from, to)
        }
    }
}

/// Check if both old and new values are `Value::Map`.
#[cfg(test)]
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
#[cfg(test)]
fn format_map_diff(old_value: Option<&Value>, new_value: &Value, attr_prefix: &str) -> String {
    let new_map = match new_value {
        Value::Map(m) => m,
        _ => return format_value(new_value),
    };
    let old_map = match old_value {
        Some(Value::Map(m)) => m,
        _ => {
            // No old map; treat all new keys as added
            let empty = indexmap::IndexMap::new();
            let diff = compute_map_diff(&empty, new_map);
            let mut lines = Vec::new();
            for entry in &diff.added {
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "+".green(),
                    entry.key,
                    format_value_with_key(&entry.value, Some(&entry.key)).green()
                ));
            }
            return lines.join("\n");
        }
    };

    let diff = compute_map_diff(old_map, new_map);
    let mut lines = Vec::new();

    // Merge all entries into a single list sorted by key (preserving original ordering)
    for entry in diff.iter_by_key() {
        match entry {
            carina_core::diff_helpers::MapDiffItem::Changed(e) => {
                lines.push(format!(
                    "{}  {} {}: {} → {}",
                    attr_prefix,
                    "~".yellow(),
                    e.key,
                    format_value_with_key(&e.old_value, Some(&e.key))
                        .red()
                        .strikethrough(),
                    format_value_with_key(&e.new_value, Some(&e.key)).green()
                ));
            }
            carina_core::diff_helpers::MapDiffItem::Added(e) => {
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "+".green(),
                    e.key,
                    format_value_with_key(&e.value, Some(&e.key)).green()
                ));
            }
            carina_core::diff_helpers::MapDiffItem::Removed(e) => {
                lines.push(format!(
                    "{}  {} {}: {}",
                    attr_prefix,
                    "-".red().strikethrough(),
                    e.key,
                    format_value_with_key(&e.value, Some(&e.key))
                        .red()
                        .strikethrough()
                ));
            }
        }
    }

    lines.join("\n")
}

/// Format a list-of-maps diff for Update effect display.
/// Uses content-matched comparison (multiset matching) instead of index-based.
/// 1. Find exact matches between old and new items
/// 2. Pair remaining unmatched items by similarity for field-level diffs
/// 3. Display unchanged, modified (~), added (+), and removed (-) items
#[cfg(test)]
fn format_list_diff(old_value: Option<&Value>, new_value: &Value, attr_prefix: &str) -> String {
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

/// Format the changed_create_only attributes for a Replace effect.
///
/// Only shows attributes listed in `changed_create_only` that exist in `to_attrs`.
/// When the old and new values are semantically equal (cascade-triggered replacement
/// where the new value is not yet known), the attribute is shown with
/// "(forces replacement, known after apply)" instead of being hidden.
#[cfg(test)]
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

#[cfg(test)]
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
#[cfg(test)]
fn value_references_binding(value: &Value, binding: &str) -> bool {
    match value {
        Value::ResourceRef { path } => path.binding() == binding,
        Value::List(items) => items.iter().any(|v| value_references_binding(v, binding)),
        Value::Map(map) => map.values().any(|v| value_references_binding(v, binding)),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
