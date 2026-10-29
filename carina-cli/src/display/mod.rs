use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use colored::{ColoredString, Colorize};

use carina_core::detail_rows::{
    DetailRow, HistoricalDisplayAttributes, ListOfMapsDiffField, ListOfMapsDiffItem,
    ListOfMapsDiffItemKind, ListOfMapsDiffModified, MapDiffEntryIR, build_detail_rows_by_instance,
    build_replace_detail_rows_from_display, hidden_unchanged_summary,
};
#[cfg(test)]
use carina_core::effect::DeletedInstanceKey;
use carina_core::effect::Effect;
use carina_core::plan::{DeferredSummaryAction, Plan, PlanSummaryPart, ReplaceDisplayInfo};
#[cfg(test)]
use carina_core::plan_tree::shorten_attr_name;
use carina_core::plan_tree::{
    ChildRenderItem, build_dependency_graph, build_single_parent_tree, child_render_items,
    deferred_for_detail_rows, deferred_for_display_name, deferred_for_source, deferred_for_verb,
    extract_compact_hint,
};
#[cfg(test)]
use carina_core::resource::Value;
use carina_core::resource::{ModuleSource, ResourceId};
use carina_core::schema::SchemaRegistry;
use carina_core::value::{RenderedValue, ValueSpanKind, render_export_json_value, render_value};

use crate::DetailLevel;
use crate::plan_summary::PlanChangeSummary;

/// Source for the historical values consumed by plan renderers.
///
/// Production has exactly one implementation: the display-safe type built by
/// the planning seam. The test-only legacy-map implementation keeps older
/// rendering fixtures concise without reopening the production boundary.
pub trait HistoricalDisplaySource<S = ModuleSource> {
    fn historical_display<'a>(&'a self, plan: &Plan<S>) -> Cow<'a, HistoricalDisplayAttributes>;
}

impl<S> HistoricalDisplaySource<S> for HistoricalDisplayAttributes {
    fn historical_display<'a>(&'a self, _plan: &Plan<S>) -> Cow<'a, HistoricalDisplayAttributes> {
        Cow::Borrowed(self)
    }
}

#[cfg(test)]
impl HistoricalDisplaySource for HashMap<DeletedInstanceKey, HashMap<String, Value>> {
    fn historical_display<'a>(&'a self, plan: &Plan) -> Cow<'a, HistoricalDisplayAttributes> {
        let mut display = HistoricalDisplayAttributes::for_plan(plan);
        let empty_nested_write_only = carina_core::provider::NestedWriteOnlyRecord::new();
        for (key, attributes) in self {
            display.set_delete_from_provider_and_persisted(
                key.clone(),
                attributes,
                None,
                &empty_nested_write_only,
            );
        }
        Cow::Owned(display)
    }
}

const LEFT_MARGIN: &str = "  ";
const ATTR_BASE: &str = "    ";
const VERTICAL_CONNECTOR: &str = "│";
const VERTICAL_CONTINUATION: &str = "│  ";
/// Width consumed by a child-list connector (`├─ ` / `└─ `).
const CONNECTOR_WIDTH: usize = 3;
/// `├─ ` — branch connector (non-last child).
const BRANCH_CONNECTOR: &str = "├─ ";
/// `└─ ` — corner connector (last child).
const CORNER_CONNECTOR: &str = "└─ ";
/// Spaces-only continuation when no `│` spine continues at this depth.
const SPACE_CONTINUATION: &str = "   ";

const fn utf8_char_count(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut count = 0;
    while i < bytes.len() {
        if bytes[i] & 0b1100_0000 != 0b1000_0000 {
            count += 1;
        }
        i += 1;
    }
    count
}

const _: () = assert!(utf8_char_count(BRANCH_CONNECTOR) == CONNECTOR_WIDTH);
const _: () = assert!(utf8_char_count(CORNER_CONNECTOR) == CONNECTOR_WIDTH);
const _: () = assert!(utf8_char_count(VERTICAL_CONTINUATION) == CONNECTOR_WIDTH);
const _: () = assert!(SPACE_CONTINUATION.len() == CONNECTOR_WIDTH);

/// A plan-display sigil. Construction is the only place that fixes both the
/// raw glyph (column width) and the colored rendering (visual output), so
/// callers cannot pass mismatched values.
struct Sigil {
    raw: &'static str,
    rendered: ColoredString,
}

impl Sigil {
    fn from_effect<S>(effect: &Effect<S>) -> Self {
        let raw = effect.display_glyph();
        let rendered = match effect {
            Effect::Create(_) | Effect::DeferredCreate { .. } => raw.green().bold(),
            Effect::ResumeCreate { .. } => raw.cyan().bold(),
            Effect::Update { .. } => raw.yellow().bold(),
            Effect::DeferredReplace(_) => raw.magenta().bold(),
            Effect::Delete { .. } => raw.red().bold(),
            Effect::Read { .. } | Effect::Import { .. } => raw.cyan().bold(),
            // carina#3332: the previous `x` (red, bold) shape-collides
            // with the `✗` failure indicator used in apply output.
            // Use `~` (yellow, bold) — the same family as Move's `->`
            // and the trailing `(remove from state)` annotation
            // disambiguates from Update's `~` line.
            Effect::Remove { .. } | Effect::Move { .. } => raw.yellow().bold(),
            Effect::Wait { .. } => raw.magenta().bold(),
        };
        Self { raw, rendered }
    }

    fn replacement(create_before_destroy: bool) -> Self {
        let raw = Effect::replace_display_glyph(create_before_destroy);
        Self {
            raw,
            rendered: raw.magenta().bold(),
        }
    }

    fn module_header() -> Self {
        Self {
            raw: "▾",
            rendered: "▾".cyan().bold(),
        }
    }

    fn deferred_for_expression() -> Self {
        Self {
            raw: "+",
            rendered: "+".green().bold(),
        }
    }

    fn export_added() -> Self {
        // Export rows are intentionally quieter than operation rows, so omit bold.
        Self {
            raw: "+",
            rendered: "+".green(),
        }
    }

    fn export_modified() -> Self {
        // Export rows are intentionally quieter than operation rows, so omit bold.
        Self {
            raw: "~",
            rendered: "~".yellow(),
        }
    }

    fn export_removed() -> Self {
        // Export rows are intentionally quieter than operation rows, so omit bold.
        Self {
            raw: "-",
            rendered: "-".red(),
        }
    }
}

/// Returns the full leading prefix for a top-level row.
fn top_level_sigil_prefix(sigil: &Sigil) -> String {
    debug_assert!(
        !sigil.raw.is_empty(),
        "plan sigil raw glyph must not be empty"
    );
    format!("{}{} ", LEFT_MARGIN, sigil.rendered,)
}

fn tree_sigil_prefix(indent: usize, is_last: bool, prefix: &str, sigil: &Sigil) -> String {
    if indent == 0 {
        top_level_sigil_prefix(sigil)
    } else {
        let connector = if is_last {
            format!("{}{}", prefix, CORNER_CONNECTOR)
        } else {
            format!("{}{}", prefix, BRANCH_CONNECTOR)
        };
        format!("{}{}{} ", LEFT_MARGIN, connector, sigil.rendered)
    }
}

fn vertical_connector_line(child_prefix: &str) -> String {
    format!("{}{}{}\n", LEFT_MARGIN, child_prefix, VERTICAL_CONNECTOR)
}

fn module_child_prefix() -> String {
    SPACE_CONTINUATION.to_string()
}

/// Check if a resource has a `let` binding (i.e., is not anonymous).
fn has_binding<S>(resource: carina_core::parser::ResourceRef<'_, S>) -> bool {
    resource.binding().is_some()
}

/// Format a compact resource identifier, showing either the binding name in quotes
/// or a hint in parentheses for anonymous resources.
///
/// `name` is the display name of the resource (typically `id.name`).
/// `parent_binding` is the binding name of the parent in the tree, used to skip
/// redundant ResourceRef hints.
fn format_compact_name<S>(
    resource: carina_core::parser::ResourceRef<'_, S>,
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
                "{}{}{} = {}",
                top_level_sigil_prefix(&Sigil::export_added()),
                name,
                type_str.dimmed(),
                colored_value(&render_value(new_value))
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
                "{}{}{} = {} {} {}",
                top_level_sigil_prefix(&Sigil::export_modified()),
                name,
                type_str.dimmed(),
                colored_value(&render_export_json_value(old_json)),
                "→".dimmed(),
                colored_value(&render_value(new_value))
            )
            .unwrap();
        }
        ExportChange::Removed { name, old_json } => {
            writeln!(
                out,
                "{}{} = {}",
                top_level_sigil_prefix(&Sigil::export_removed()),
                name,
                colored_value(&render_export_json_value(old_json))
            )
            .unwrap();
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn print_plan<S, D: HistoricalDisplaySource<S> + ?Sized>(
    plan: &Plan<S>,
    summary: &PlanChangeSummary,
    detail: DetailLevel,
    historical_display: &D,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    prev_explicit: Option<&HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) {
    print!(
        "{}",
        format_plan_with_delete_instances(
            plan,
            summary,
            detail,
            historical_display,
            schemas,
            moved_origins,
            prev_explicit,
            expansion_trace,
        )
    );
}

/// Format a plan as a string for display.
///
/// This is the core formatting logic used by `print_plan`. Returning a `String`
/// enables snapshot testing and other programmatic uses of the plan output.
///
/// When `prev_explicit` is provided, the actual-state side of each effect
/// is projected through the per-resource authoring tree before
/// unchanged-attribute counting (refs awscc#206), so server-side default
/// fields the user never wrote do not inflate the plan output.
#[allow(clippy::too_many_arguments)]
pub fn format_plan_with_delete_instances<S, D: HistoricalDisplaySource<S> + ?Sized>(
    plan: &Plan<S>,
    summary: &PlanChangeSummary,
    detail: DetailLevel,
    historical_display: &D,
    schemas: Option<&SchemaRegistry>,
    moved_origins: &HashMap<ResourceId, ResourceId>,
    prev_explicit: Option<&HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) -> String {
    let mut out = String::new();

    if !summary.has_changes() {
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

    let historical_display = historical_display.historical_display(plan);
    out.push_str(&format_plan_tree(
        plan,
        detail,
        &historical_display,
        schemas,
        moved_origins,
        prev_explicit,
        expansion_trace,
    ));

    // Show deferred for-expressions
    for deferred in summary.residual_deferred_for() {
        out.push_str(&format_deferred_for_expression(deferred));
    }

    // Show export changes if any
    let export_changes = summary.export_changes();
    if !export_changes.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "{}", "Exports:".cyan().bold()).unwrap();
        writeln!(out).unwrap();
        for change in export_changes {
            out.push_str(&format_export_change(change));
        }
    }

    writeln!(out).unwrap();
    let mut parts: Vec<String> = summary
        .text_parts()
        .iter()
        .copied()
        .map(|part| match part {
            PlanSummaryPart::Read { count } => format!("{} to read", count.to_string().cyan()),
            PlanSummaryPart::Import { count } => format!("{} to import", count.to_string().cyan()),
            PlanSummaryPart::Create { count } => {
                let count = count.to_string().green();
                format!("{} to add", count)
            }
            PlanSummaryPart::Update { count } => {
                format!("{} to change", count.to_string().yellow())
            }
            PlanSummaryPart::Replace { count } => {
                format!("{} to replace", count.to_string().magenta())
            }
            PlanSummaryPart::Delete { count } => {
                format!("{} to destroy", count.to_string().red())
            }
            PlanSummaryPart::Remove { count } => {
                // carina#3332: state-only removal is not a destructive failure;
                // color the count yellow to agree with the `~` Remove row in
                // the tree above instead of red (which pairs with `✗`/Delete).
                format!("{} to remove from state", count.to_string().yellow())
            }
            PlanSummaryPart::Move { count } => format!("{} to move", count.to_string().yellow()),
            PlanSummaryPart::Wait { count } => format!("{} to wait", count.to_string().magenta()),
        })
        .collect();
    if !export_changes.is_empty() {
        parts.push(format!(
            "{} export change(s)",
            export_changes.len().to_string().cyan()
        ));
    }
    writeln!(out, "Plan: {}.", parts.join(", ")).unwrap();
    for entry in summary.deferred_effects() {
        let action = match entry.action {
            DeferredSummaryAction::Add => "add".green(),
            DeferredSummaryAction::Replace => "replace".magenta(),
        };
        writeln!(
            out,
            "       {} to {} after {} {}.",
            "N".green(),
            action,
            entry.upstream_binding,
            entry.verb
        )
        .unwrap();
    }
    for deferred in summary.residual_deferred_for() {
        writeln!(
            out,
            "       {} to {} after {} resolves.",
            "N".green(),
            "add".green(),
            deferred_for_source(deferred)
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    out
}

/// Format a single deferred for-expression for plan display.
fn format_deferred_for_expression(deferred: &carina_core::parser::DeferredForExpression) -> String {
    let mut out = String::new();
    let upstream = deferred_for_source(deferred);
    let sigil = Sigil::deferred_for_expression();
    let line_prefix = top_level_sigil_prefix(&sigil);
    let attr_base = ATTR_BASE;
    writeln!(
        out,
        "{}{} {}",
        line_prefix,
        deferred.resource_type.cyan().bold(),
        format!("(N records after {upstream} resolves)").dimmed()
    )
    .unwrap();
    for row in deferred_for_detail_rows(deferred, &upstream, "resolves") {
        match row {
            DetailRow::Text { text } => {
                writeln!(out, "{}{}{}", LEFT_MARGIN, attr_base, text.dimmed()).unwrap();
            }
            DetailRow::Attribute { key, value, .. } => {
                writeln!(
                    out,
                    "{}{}{}: {}",
                    LEFT_MARGIN,
                    attr_base,
                    key.dimmed(),
                    value
                )
                .unwrap();
            }
            _ => {}
        }
    }
    out
}

pub fn format_destroy_plan_with_delete_instances<D: HistoricalDisplaySource + ?Sized>(
    plan: &Plan,
    detail: DetailLevel,
    historical_display: &D,
) -> String {
    let mut out = String::new();

    if plan.effects().is_empty() {
        return out;
    }

    writeln!(out, "{}", "Destroy Plan:".red().bold()).unwrap();
    writeln!(out).unwrap();

    let historical_display = historical_display.historical_display(plan);
    out.push_str(&format_plan_tree(
        plan,
        detail,
        &historical_display,
        None,
        &HashMap::new(),
        None,
        None, // ExpansionTrace doesn't apply to destroy plans (no compositions in state)
    ));

    out
}

/// Holds shared state and configuration for recursive tree rendering.
///
/// Groups the parameters that are threaded through every recursive call of
/// `format_effect_tree`, reducing parameter count from 13 to 6.
struct TreeRenderContext<'a, S> {
    out: String,
    printed: HashSet<usize>,
    plan: &'a Plan<S>,
    dependents: HashMap<usize, Vec<usize>>,
    replacement_create_info: HashMap<usize, ReplaceDisplayInfo<'a>>,
    replacement_delete_indices: HashSet<usize>,
    detail: DetailLevel,
    historical_display: &'a HistoricalDisplayAttributes,
    schemas: Option<&'a SchemaRegistry>,
    moved_origins: &'a HashMap<ResourceId, ResourceId>,
    /// Per-resource user-authoring trees, used by `build_detail_rows`
    /// to project the actual-state side before unchanged-attribute
    /// counting (refs awscc#206).
    prev_explicit: Option<&'a HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    /// ResourceIds that are targets of Update or Replace effects.
    /// Used to skip Move line display when the move is already shown via annotation.
    update_or_replace_targets: HashSet<ResourceId>,
}

impl<'a, S> TreeRenderContext<'a, S> {
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
        if self.replacement_delete_indices.contains(&idx) {
            self.printed.insert(idx);
            return false;
        }
        self.printed.insert(idx);

        let effect = &self.plan.effects()[idx];
        let replacement_info = self.replacement_create_info.get(&idx).copied();
        let sigil = replacement_info
            .map(|metadata| Sigil::replacement(metadata.create_before_destroy))
            .unwrap_or_else(|| Sigil::from_effect(effect));
        let line_prefix = tree_sigil_prefix(indent, is_last, prefix, &sigil);
        let base_indent = LEFT_MARGIN;
        let attr_base = ATTR_BASE;

        let mut has_displayed_attrs = false;

        // --- Resource header line ---
        match effect {
            Effect::Create(r) => {
                if let Some(metadata) = replacement_info {
                    let id = &r.id;
                    let replace_note = if metadata.create_before_destroy {
                        "(must be replaced, create before destroy)"
                    } else {
                        "(must be replaced)"
                    };
                    let moved_note = self
                        .moved_origins
                        .get(id)
                        .map(|from| format!(" (moved from: {})", from.identity_display()));
                    if self.detail == DetailLevel::None {
                        let name_part = format_compact_name(
                            carina_core::parser::ResourceRef::Resource(r),
                            r.identity_str(),
                            parent_binding,
                        );
                        writeln!(
                            self.out,
                            "{}{} {} {}{}",
                            line_prefix,
                            id.display_type().cyan().bold(),
                            name_part.magenta().bold(),
                            replace_note.magenta(),
                            moved_note.as_deref().unwrap_or("").magenta()
                        )
                        .unwrap();
                    } else {
                        writeln!(
                            self.out,
                            "{}{} {} {}{}",
                            line_prefix,
                            id.display_type().cyan().bold(),
                            r.identity_str().magenta().bold(),
                            replace_note.magenta(),
                            moved_note.as_deref().unwrap_or("").magenta()
                        )
                        .unwrap();
                    }
                } else if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(
                        carina_core::parser::ResourceRef::Resource(r),
                        r.identity_str(),
                        parent_binding,
                    );
                    writeln!(
                        self.out,
                        "{}{} {}",
                        line_prefix,
                        r.id.display_type().cyan().bold(),
                        name_part.white().bold()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        self.out,
                        "{}{} {}",
                        line_prefix,
                        r.id.display_type().cyan().bold(),
                        r.identity_str().white().bold()
                    )
                    .unwrap();
                }
            }
            Effect::ResumeCreate {
                resource,
                started_at,
            } => {
                let since = format!("(in flight since {started_at})");
                writeln!(
                    self.out,
                    "{}{} {} {}",
                    line_prefix,
                    "resume create".cyan().bold(),
                    resource.id.human().to_string().cyan().bold(),
                    since.dimmed(),
                )
                .unwrap();
            }
            Effect::Update { to, .. } => {
                let id = &to.id;
                let moved_note = self
                    .moved_origins
                    .get(id)
                    .map(|from| format!(" (moved from: {})", from.identity_display()));
                if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(
                        carina_core::parser::ResourceRef::Resource(to),
                        to.identity_str(),
                        parent_binding,
                    );
                    writeln!(
                        self.out,
                        "{}{} {}{}",
                        line_prefix,
                        id.display_type().cyan().bold(),
                        name_part.yellow().bold(),
                        moved_note.as_deref().unwrap_or("").yellow()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        self.out,
                        "{}{} {}{}",
                        line_prefix,
                        id.display_type().cyan().bold(),
                        to.identity_str().yellow().bold(),
                        moved_note.as_deref().unwrap_or("").yellow()
                    )
                    .unwrap();
                }
            }
            Effect::Delete {
                id,
                identifier,
                generation,
                binding,
                ..
            } => {
                let display_name = binding.as_deref().unwrap_or(id.identity_str());
                let deposed_note = if matches!(
                    generation,
                    carina_core::effect::EffectGeneration::Deposed(_)
                ) {
                    format!(" (deposed {identifier})")
                } else {
                    String::new()
                };
                writeln!(
                    self.out,
                    "{}{} {}{}",
                    line_prefix,
                    id.display_type().cyan().bold(),
                    display_name.red().bold().strikethrough(),
                    deposed_note.yellow()
                )
                .unwrap();
            }
            Effect::Read { resource } => {
                if self.detail == DetailLevel::None {
                    let name_part = format_compact_name(
                        carina_core::parser::ResourceRef::DataSource(resource),
                        resource.identity_str(),
                        parent_binding,
                    );
                    writeln!(
                        self.out,
                        "{}{} {} {}",
                        line_prefix,
                        resource.id.display_type().cyan().bold(),
                        name_part.cyan().bold(),
                        "(data source)".dimmed()
                    )
                    .unwrap();
                } else {
                    writeln!(
                        self.out,
                        "{}{} {} {}",
                        line_prefix,
                        resource.id.display_type().cyan().bold(),
                        resource.identity_str().cyan().bold(),
                        "(data source)".dimmed()
                    )
                    .unwrap();
                }
            }
            Effect::Import { id, identifier } => {
                // carina#3329: render the identifier through
                // `format_import_identifier` so a concrete cloud
                // identifier prints bare (`vpc-0abc…`) while a
                // deferred-upstream interpolation keeps its
                // `(known after upstream apply: …)` marker rather than
                // being silently substituted to empty.
                let identifier_str = carina_core::effect::format_import_identifier(identifier);
                writeln!(
                    self.out,
                    "{}{} {} {}",
                    line_prefix,
                    id.display_type().cyan().bold(),
                    id.identity_str().cyan().bold(),
                    format!("(import: {})", identifier_str).dimmed()
                )
                .unwrap();
            }
            Effect::Remove { id } => {
                writeln!(
                    self.out,
                    "{}{} {} {}",
                    line_prefix,
                    id.display_type().cyan().bold(),
                    // carina#3332: name was previously `red().bold()`,
                    // which pairs with `✗`/Delete and re-introduces the
                    // "state-only success looks like failure" misread
                    // even after the leading `x` glyph fix. Yellow
                    // matches the `~` symbol family and Move's row.
                    id.identity_str().yellow().bold(),
                    "(remove from state)".dimmed()
                )
                .unwrap();
                let abandoned: Vec<_> = self
                    .plan
                    .abandoned_deposed_generations()
                    .filter(|(abandoned_id, _)| *abandoned_id == id)
                    .collect();
                if !abandoned.is_empty() {
                    let attr_prefix = if indent == 0 {
                        format!("{}{}", base_indent, attr_base)
                    } else {
                        let continuation = if is_last {
                            format!("{}{}", prefix, SPACE_CONTINUATION)
                        } else {
                            format!("{}{}", prefix, VERTICAL_CONTINUATION)
                        };
                        format!("{}{}{}", base_indent, continuation, SPACE_CONTINUATION)
                    };
                    for (_, key) in abandoned {
                        writeln!(
                            self.out,
                            "{}abandon deposed generation `{}`",
                            attr_prefix, key
                        )
                        .unwrap();
                    }
                    has_displayed_attrs = true;
                }
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
                    "{}{} {} {}",
                    line_prefix,
                    to.display_type().cyan().bold(),
                    to.identity_str().yellow().bold(),
                    format!("(moved from: {})", from.identity_str()).dimmed()
                )
                .unwrap();
            }
            Effect::Wait {
                identity,
                until_surface,
                ..
            } => {
                let binding = identity.to_string();
                writeln!(
                    self.out,
                    "{}{} {}",
                    line_prefix,
                    binding.magenta().bold(),
                    format!("(until {})", until_surface).dimmed()
                )
                .unwrap();
            }
            Effect::DeferredCreate { template, .. } => {
                let verb = deferred_for_verb(self.plan, template);
                let display_name = deferred_for_display_name(template, verb);
                let display_name = display_name.green().bold();
                writeln!(
                    self.out,
                    "{}{} {}",
                    line_prefix,
                    template.resource_type.cyan().bold(),
                    display_name
                )
                .unwrap();
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}{}", prefix, SPACE_CONTINUATION)
                    } else {
                        format!("{}{}", prefix, VERTICAL_CONTINUATION)
                    };
                    format!("{}{}{}", base_indent, continuation, SPACE_CONTINUATION)
                };
                let upstream_binding = template.binding_description();
                for row in deferred_for_detail_rows(template, &upstream_binding, verb) {
                    render_detail_row(&mut self.out, &row, effect, &attr_prefix);
                }
                has_displayed_attrs = true;
            }
            Effect::DeferredReplace(payload) => {
                let template = payload.template.as_ref();
                let verb = deferred_for_verb(self.plan, template);
                let display_name = deferred_for_display_name(template, verb).magenta().bold();
                writeln!(
                    self.out,
                    "{}{} {}",
                    line_prefix,
                    template.resource_type.cyan().bold(),
                    display_name
                )
                .unwrap();
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}{}", prefix, SPACE_CONTINUATION)
                    } else {
                        format!("{}{}", prefix, VERTICAL_CONTINUATION)
                    };
                    format!("{}{}{}", base_indent, continuation, SPACE_CONTINUATION)
                };
                let upstream_binding = template.binding_description();
                for row in deferred_for_detail_rows(template, &upstream_binding, verb) {
                    render_detail_row(&mut self.out, &row, effect, &attr_prefix);
                }
                has_displayed_attrs = true;
            }
        }

        // --- Detail rows (attributes) ---
        if self.detail != DetailLevel::None {
            let attr_prefix = if indent == 0 {
                format!("{}{}", base_indent, attr_base)
            } else {
                let continuation = if is_last {
                    format!("{}{}", prefix, SPACE_CONTINUATION)
                } else {
                    format!("{}{}", prefix, VERTICAL_CONTINUATION)
                };
                format!("{}{}{}", base_indent, continuation, SPACE_CONTINUATION)
            };

            let detail_rows =
                if let (Effect::Create(r), Some(replacement)) = (effect, replacement_info) {
                    let explicit = self.prev_explicit.and_then(|map| map.get(&r.id));
                    build_replace_detail_rows_from_display(
                        self.plan,
                        r,
                        replacement,
                        self.historical_display,
                        self.schemas,
                        self.detail.to_core(),
                        explicit,
                    )
                } else {
                    build_detail_rows_by_instance(
                        effect,
                        self.schemas,
                        self.detail.to_core(),
                        self.historical_display,
                        self.prev_explicit,
                    )
                };

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
                effect
                    .as_resource_ref()
                    .and_then(|r| r.binding().map(str::to_string))
            }
        };

        let children = self.dependents.get(&idx).cloned().unwrap_or_default();
        let unprinted_children: Vec<_> = children
            .iter()
            .filter(|c| !self.printed.contains(c))
            .cloned()
            .collect();
        let child_render_items = self.child_render_items(&unprinted_children);

        has_displayed_attrs |= self.render_children(
            &child_render_items,
            ChildRenderOptions {
                parent_indent: indent,
                parent_is_last: is_last,
                parent_prefix: prefix,
                parent_binding: current_binding.as_deref(),
                parent_displayed_attrs: has_displayed_attrs,
                child_prefix_override: None,
            },
        );

        has_displayed_attrs
    }

    fn format_render_item(
        &mut self,
        item: &ChildRenderItem,
        indent: usize,
        is_last: bool,
        prefix: &str,
        parent_binding: Option<&str>,
    ) -> bool {
        match item {
            ChildRenderItem::Normal(idx) => {
                self.format_effect_tree(*idx, indent, is_last, prefix, parent_binding)
            }
        }
    }

    fn child_render_items(&self, child_indices: &[usize]) -> Vec<ChildRenderItem> {
        child_render_items(self.plan.effects(), child_indices)
    }

    fn will_render(&self, item: &ChildRenderItem) -> bool {
        match item {
            ChildRenderItem::Normal(idx) => self.will_render_effect_tree(*idx),
        }
    }

    fn will_render_effect_tree(&self, idx: usize) -> bool {
        if self.printed.contains(&idx) {
            return false;
        }
        if self.replacement_delete_indices.contains(&idx) {
            return false;
        }

        let Some(effect) = self.plan.effects().get(idx) else {
            return false;
        };

        match effect {
            Effect::Move { to, .. } => !self.update_or_replace_targets.contains(to),
            Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => true,
        }
    }

    fn consume_suppressed_item(&mut self, item: &ChildRenderItem) {
        match item {
            ChildRenderItem::Normal(idx) => {
                if self.replacement_delete_indices.contains(idx) {
                    self.printed.insert(*idx);
                    return;
                }
                if !self.will_render_effect_tree(*idx)
                    && !self.printed.contains(idx)
                    && matches!(
                        self.plan.effects().get(*idx),
                        Some(Effect::Move { to, .. })
                            if self.update_or_replace_targets.contains(to)
                    )
                {
                    self.printed.insert(*idx);
                }
            }
        }
    }

    fn render_children(
        &mut self,
        items: &[ChildRenderItem],
        options: ChildRenderOptions<'_>,
    ) -> bool {
        for item in items {
            self.consume_suppressed_item(item);
        }

        let rendering_items: Vec<_> = items.iter().filter(|item| self.will_render(item)).collect();
        if rendering_items.is_empty() {
            return false;
        }

        let child_prefix = options.child_prefix_override.unwrap_or_else(|| {
            child_prefix_for_parent(
                options.parent_indent,
                options.parent_is_last,
                options.parent_prefix,
            )
        });

        if options.parent_displayed_attrs {
            self.out.push_str(&vertical_connector_line(&child_prefix));
        }

        let mut any_child_displayed_attrs = false;
        for (i, item) in rendering_items.iter().enumerate() {
            let child_is_last = i == rendering_items.len() - 1;
            let child_had_attrs = self.format_render_item(
                item,
                options.parent_indent + 1,
                child_is_last,
                &child_prefix,
                options.parent_binding,
            );
            if child_had_attrs {
                any_child_displayed_attrs = true;
            }
            if child_had_attrs && !child_is_last {
                self.out.push_str(&vertical_connector_line(&child_prefix));
            }
        }

        any_child_displayed_attrs
    }
}

struct ChildRenderOptions<'a> {
    parent_indent: usize,
    parent_is_last: bool,
    parent_prefix: &'a str,
    parent_binding: Option<&'a str>,
    parent_displayed_attrs: bool,
    child_prefix_override: Option<String>,
}

fn child_prefix_for_parent(
    parent_indent: usize,
    parent_is_last: bool,
    parent_prefix: &str,
) -> String {
    if parent_indent == 0 {
        format!("{}  ", ATTR_BASE)
    } else {
        let continuation = if parent_is_last {
            format!("{}{}", parent_prefix, SPACE_CONTINUATION)
        } else {
            format!("{}{}", parent_prefix, VERTICAL_CONTINUATION)
        };
        format!("{}{}", continuation, SPACE_CONTINUATION)
    }
}

/// Format the tree body of a plan (no header, no summary).
///
/// `historical_display` provides display-safe historical values for Update,
/// Replace, and Delete effects.
#[allow(clippy::too_many_arguments)]
fn format_plan_tree<'a, S>(
    plan: &Plan<S>,
    detail: DetailLevel,
    historical_display: &'a HistoricalDisplayAttributes,
    schemas: Option<&'a SchemaRegistry>,
    moved_origins: &'a HashMap<ResourceId, ResourceId>,
    prev_explicit: Option<&'a HashMap<ResourceId, carina_core::explicit::ExplicitFields>>,
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) -> String {
    // Build dependency graph from effects
    let graph = build_dependency_graph(plan);

    // Build the single-parent tree with sorted siblings
    let (roots, dependents) = build_single_parent_tree(plan, &graph);
    let replacement_display: Vec<_> = plan.replace_display_info().collect();
    let replacement_delete_indices: HashSet<_> = replacement_display
        .iter()
        .map(|metadata| metadata.delete_idx)
        .collect();
    let replacement_create_info: HashMap<_, _> = replacement_display
        .iter()
        .map(|metadata| (metadata.create_idx, *metadata))
        .collect();

    let update_or_replace_targets: HashSet<ResourceId> = plan
        .effects()
        .iter()
        .enumerate()
        .filter_map(|(idx, e)| match e {
            Effect::Update { to, .. } => Some(to.id.clone()),
            Effect::Create(r) if replacement_create_info.contains_key(&idx) => Some(r.id.clone()),
            Effect::DeferredReplace(_) => None,
            _ => None,
        })
        .collect();

    let mut ctx = TreeRenderContext {
        out: String::new(),
        printed: HashSet::new(),
        plan,
        dependents,
        replacement_create_info,
        replacement_delete_indices,
        detail,
        historical_display,
        schemas,
        moved_origins,
        prev_explicit,
        update_or_replace_targets,
    };

    // #3307: when an ExpansionTrace is supplied AND it actually
    // records lineage for the plan's leaves, group root rows by their
    // outermost composition call site. Within each composition group,
    // a `module "<binding>" (<source_path>)` header is printed first,
    // followed by the leaf rows. Roots with no lineage entry render
    // at the top level as before. (carina#3307 introduced the
    // grouping; carina#3322 renamed the user-facing label from
    // `LiveComposition "<binding>"` to the DSL-visible
    // `module "<binding>" (<source_path>)` shape.)
    let composition_groups = group_roots_by_outermost_composition(plan, &roots, expansion_trace);

    if composition_groups.has_any_grouping() {
        // First: render ungrouped roots (declared at the DSL root).
        let ungrouped_items = ctx.child_render_items(&composition_groups.ungrouped);
        for (i, item) in ungrouped_items.iter().enumerate() {
            let last = i == ungrouped_items.len() - 1 && composition_groups.grouped.is_empty();
            ctx.format_render_item(item, 0, last, "", None);
        }
        // Then: each composition group with its header. The header is a
        // virtual parent, and the leaves render through the same child path
        // used by resource dependents.
        for group in &composition_groups.grouped {
            writeln!(
                ctx.out,
                "{}",
                format_composition_header(&group.binding, group.source_path.as_deref())
            )
            .unwrap();
            let leaf_items = ctx.child_render_items(&group.leaves);
            ctx.render_children(
                &leaf_items,
                ChildRenderOptions {
                    parent_indent: 0,
                    parent_is_last: true,
                    parent_prefix: "",
                    parent_binding: None,
                    parent_displayed_attrs: false,
                    child_prefix_override: Some(module_child_prefix()),
                },
            );
        }
    } else {
        // No trace, or trace empty for this plan's leaves — use the
        // pre-#3307 flat layout so existing snapshots remain valid.
        let root_items = ctx.child_render_items(&roots);
        for (i, item) in root_items.iter().enumerate() {
            ctx.format_render_item(item, 0, i == root_items.len() - 1, "", None);
        }
    }

    // Print any remaining effects that weren't reachable from roots
    // (e.g., circular dependencies or isolated resources)
    let remaining: Vec<_> = (0..plan.effects().len())
        .filter(|idx| !ctx.printed.contains(idx))
        .filter(|idx| !ctx.replacement_delete_indices.contains(idx))
        .collect();
    let remaining_items = ctx.child_render_items(&remaining);
    for (i, item) in remaining_items.iter().enumerate() {
        ctx.format_render_item(item, 0, i == remaining_items.len() - 1, "", None);
    }

    ctx.out
}

/// Header row for a composition group in the folded plan layout.
///
/// Renders `▾ module "<binding>" (<source_path>)`, dropping the
/// parenthesized suffix when the call site has no recorded
/// `source_path` (hand-built traces, test fixtures). The keyword
/// `module` matches the DSL `module_call` construct the operator
/// wrote (`let r = infra { ... }`), so it is something they can
/// trace back to their own `.crn` — unlike the previous internal
/// `LiveComposition` label (carina#3322).
fn format_composition_header(binding: &str, source_path: Option<&str>) -> String {
    let sigil = Sigil::module_header();
    let prefix = top_level_sigil_prefix(&sigil);
    match source_path {
        None => format!("{}module \"{}\"", prefix, binding.cyan().bold()),
        Some(path) => format!(
            "{}module \"{}\" {}",
            prefix,
            binding.cyan().bold(),
            format!("({})", path).dimmed(),
        ),
    }
}

/// Bucket of root effects, partitioned into the ones nested inside a
/// composition (grouped under their outermost call site) and the ones
/// declared at the DSL root (ungrouped).
struct CompositionGroups {
    ungrouped: Vec<usize>,
    grouped: Vec<CompositionGroup>,
}

impl CompositionGroups {
    fn has_any_grouping(&self) -> bool {
        !self.grouped.is_empty()
    }
}

struct CompositionGroup {
    /// Display label — the call site's binding (instance prefix), e.g. `cluster`.
    binding: String,
    /// `use { source = "..." }` path the module was loaded from.
    /// `None` when the trace was built without a recorded path (test
    /// fixtures, hand-built traces); the renderer drops the
    /// parenthesized suffix then.
    source_path: Option<String>,
    /// Root indices in `plan.effects()` that belong to this group.
    leaves: Vec<usize>,
}

/// Partition `roots` into composition-grouped vs ungrouped buckets
/// using the `ExpansionTrace`. A root whose effect's resource has an
/// outermost composition in the trace lands in the grouped bucket
/// under that call site; everything else stays ungrouped.
fn group_roots_by_outermost_composition<S>(
    plan: &Plan<S>,
    roots: &[usize],
    expansion_trace: Option<&carina_core::resource::ExpansionTrace>,
) -> CompositionGroups {
    let trace = match expansion_trace {
        Some(t) if !t.is_empty() => t,
        _ => {
            return CompositionGroups {
                ungrouped: roots.to_vec(),
                grouped: Vec::new(),
            };
        }
    };

    // Map: outermost call-site name → group leaves (preserves first
    // appearance order). `source_path` is recorded on first sight; the
    // expander stamps every leaf of the same call site with the same
    // path, so later sightings can be ignored.
    let mut order: Vec<String> = Vec::new();
    let mut by_call_site: HashMap<String, (Option<String>, Vec<usize>)> = HashMap::new();
    let mut ungrouped: Vec<usize> = Vec::new();

    for &root_idx in roots {
        let effect = &plan.effects()[root_idx];
        let outer = effect
            .as_resource_ref()
            .map(|rref| carina_core::resource::PersistentId::new(rref.id().clone()))
            .and_then(|pid| trace.call_sites_of(&pid).first().cloned());

        match outer {
            Some(site) => {
                let key = site.binding().to_string();
                if !by_call_site.contains_key(&key) {
                    order.push(key.clone());
                }
                by_call_site
                    .entry(key)
                    .or_insert_with(|| (site.source_path.clone(), Vec::new()))
                    .1
                    .push(root_idx);
            }
            None => ungrouped.push(root_idx),
        }
    }

    let grouped: Vec<CompositionGroup> = order
        .into_iter()
        .map(|binding| {
            let (source_path, leaves) = by_call_site.remove(&binding).unwrap_or_default();
            CompositionGroup {
                binding,
                source_path,
                leaves,
            }
        })
        .collect();

    CompositionGroups { grouped, ungrouped }
}

#[derive(Clone, Copy)]
enum AnsiValueStyle {
    Semantic,
    Dimmed,
    RedStrikethrough,
    Green,
    Magenta,
}

fn style_value_span(kind: ValueSpanKind, text: &str, style: AnsiValueStyle) -> String {
    if kind == ValueSpanKind::Whitespace {
        return text.to_string();
    }
    match style {
        AnsiValueStyle::Semantic => match kind {
            ValueSpanKind::StringLiteral => text.green().to_string(),
            ValueSpanKind::Bool => text.yellow().to_string(),
            ValueSpanKind::Number => text.white().to_string(),
            ValueSpanKind::Enum => text.magenta().to_string(),
            ValueSpanKind::Reference => text.cyan().to_string(),
            ValueSpanKind::Structure
            | ValueSpanKind::Key
            | ValueSpanKind::Other
            | ValueSpanKind::Whitespace => text.to_string(),
        },
        AnsiValueStyle::Dimmed => match kind {
            ValueSpanKind::StringLiteral => text.green().dimmed().to_string(),
            ValueSpanKind::Bool => text.yellow().dimmed().to_string(),
            ValueSpanKind::Number => text.white().dimmed().to_string(),
            ValueSpanKind::Enum => text.magenta().dimmed().to_string(),
            ValueSpanKind::Reference => text.cyan().dimmed().to_string(),
            ValueSpanKind::Structure | ValueSpanKind::Key | ValueSpanKind::Other => {
                text.dimmed().to_string()
            }
            ValueSpanKind::Whitespace => text.to_string(),
        },
        AnsiValueStyle::RedStrikethrough => text.red().strikethrough().to_string(),
        AnsiValueStyle::Green => text.green().to_string(),
        AnsiValueStyle::Magenta => text.magenta().to_string(),
    }
}

fn render_value_ansi(
    rendered: &RenderedValue,
    style: AnsiValueStyle,
    continuation_prefix: Option<&str>,
) -> String {
    let Some(prefix) = continuation_prefix else {
        return rendered
            .spans()
            .iter()
            .map(|span| style_value_span(span.kind(), span.text(), style))
            .collect();
    };

    let prefix_cols = prefix.chars().count();
    let mut out = String::new();
    let mut continuation = false;
    let mut pending_indent = String::new();

    for span in rendered.spans() {
        if span.kind() == ValueSpanKind::Whitespace {
            for ch in span.text().chars() {
                if ch == '\n' {
                    if continuation {
                        out.push_str(prefix.trim_end());
                        pending_indent.clear();
                    }
                    out.push('\n');
                    continuation = true;
                } else if continuation {
                    pending_indent.push(ch);
                } else {
                    out.push(ch);
                }
            }
            continue;
        }

        if continuation {
            out.push_str(prefix);
            let leading_spaces = pending_indent
                .bytes()
                .take_while(|byte| *byte == b' ')
                .count();
            let drop = leading_spaces.min(prefix_cols);
            out.push_str(&pending_indent[drop..]);
            pending_indent.clear();
            continuation = false;
        }
        out.push_str(&style_value_span(span.kind(), span.text(), style));
    }

    out
}

pub(crate) fn colored_value(rendered: &RenderedValue) -> String {
    render_value_ansi(rendered, AnsiValueStyle::Semantic, None)
}

fn forcing_value(rendered: &RenderedValue) -> String {
    render_value_ansi(rendered, AnsiValueStyle::Green, None)
}

/// Apply type-based coloring to a value, with dimmed modifier for default values.
fn colored_value_dimmed(rendered: &RenderedValue) -> String {
    render_value_ansi(rendered, AnsiValueStyle::Dimmed, None)
}

/// Render a single `DetailRow` into the output string with ANSI colors.
fn render_detail_row<S>(out: &mut String, row: &DetailRow, effect: &Effect<S>, attr_prefix: &str) {
    match row {
        DetailRow::Text { text } => {
            writeln!(out, "{}{}", attr_prefix, text).unwrap();
        }
        DetailRow::Attribute {
            key,
            value,
            ref_binding: _,
            annotation,
        } => {
            let cv = match effect {
                Effect::Delete { .. } => {
                    render_value_ansi(value, AnsiValueStyle::RedStrikethrough, None)
                }
                _ => colored_value(value),
            };
            let annotation_suffix = annotation_suffix(annotation.as_deref());
            writeln!(out, "{}{}: {}{}", attr_prefix, key, cv, annotation_suffix).unwrap();
        }
        DetailRow::PrettyAttribute { key, value } => {
            // attr_prefix may contain the tree glyph `│` (U+2502, 1 column
            // wide but 3 bytes in UTF-8), so use `chars().count()` for column
            // count, not `.len()`.
            let layout =
                carina_core::value::PrettyLayout::for_attribute(attr_prefix.chars().count(), key);
            let pretty = value.render(layout);
            let cv = match effect {
                Effect::Delete { .. } => {
                    render_value_ansi(&pretty, AnsiValueStyle::RedStrikethrough, Some(attr_prefix))
                }
                _ => render_value_ansi(&pretty, AnsiValueStyle::Semantic, Some(attr_prefix)),
            };
            writeln!(out, "{}{}: {}", attr_prefix, key, cv).unwrap();
        }
        DetailRow::MapExpanded { key, entries } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            let entry_indent_cols = attr_prefix.chars().count() + 2;
            let mut prev_needs_separator = false;
            for entry in entries {
                // Inject a blank line after a multi-element list-of-maps
                // before the next sibling key so the list boundary stays
                // visible — the `*` marker disambiguates element starts
                // but not element ends (#2555). The blank only fires
                // when a sibling actually follows. It carries the bare
                // gutter so the tree bar stays continuous (#3356).
                if prev_needs_separator {
                    writeln!(out, "{}", attr_prefix.trim_end()).unwrap();
                }
                prev_needs_separator = entry.value.needs_trailing_separator();
                let layout =
                    carina_core::value::PrettyLayout::for_map_key(entry_indent_cols, &entry.key);
                let pretty = entry.value.render(layout);
                let cv = match effect {
                    Effect::Delete { .. } => render_value_ansi(
                        &pretty,
                        AnsiValueStyle::RedStrikethrough,
                        Some(attr_prefix),
                    ),
                    _ => render_value_ansi(&pretty, AnsiValueStyle::Semantic, Some(attr_prefix)),
                };
                let annotation_suffix = annotation_suffix(entry.annotation.as_deref());
                writeln!(
                    out,
                    "{}  {}: {}{}",
                    attr_prefix, entry.key, cv, annotation_suffix
                )
                .unwrap();
            }
        }
        DetailRow::Changed { key, old, new } => {
            writeln!(
                out,
                "{}{}: {} → {}",
                attr_prefix,
                key,
                render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                colored_value(new),
            )
            .unwrap();
        }
        DetailRow::ChangedForcesReplacement { key, old, new } => {
            writeln!(
                out,
                "{}{}: {} → {} {}",
                attr_prefix,
                key,
                render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                forcing_value(new),
                "(forces replacement)".magenta()
            )
            .unwrap();
        }
        DetailRow::MapDiff { key, entries } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            render_map_diff_entries(out, entries.as_slice(), attr_prefix);
        }
        DetailRow::MapDiffForcesReplacement { key, entries } => {
            writeln!(
                out,
                "{}{}: {}",
                attr_prefix,
                key,
                "(forces replacement)".magenta()
            )
            .unwrap();
            render_map_diff_entries(out, entries.as_slice(), attr_prefix);
        }
        DetailRow::StringListDiff {
            key,
            unchanged,
            added,
            removed,
        } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            render_string_list_diff_entries(out, unchanged, added, removed, attr_prefix);
        }
        DetailRow::StringListDiffForcesReplacement {
            key,
            unchanged,
            added,
            removed,
        } => {
            writeln!(
                out,
                "{}{}: {}",
                attr_prefix,
                key,
                "(forces replacement)".magenta()
            )
            .unwrap();
            render_string_list_diff_entries(out, unchanged, added, removed, attr_prefix);
        }
        DetailRow::ListOfMapsDiff { key, block } => {
            writeln!(out, "{}{}:", attr_prefix, key).unwrap();
            render_list_of_maps_diff(
                out,
                block.unchanged(),
                block.modified(),
                block.added(),
                block.removed(),
                attr_prefix,
            );
        }
        DetailRow::ListOfMapsDiffForcesReplacement { key, block } => {
            writeln!(
                out,
                "{}{}: {}",
                attr_prefix,
                key,
                "(forces replacement)".magenta()
            )
            .unwrap();
            render_list_of_maps_diff(
                out,
                block.unchanged(),
                block.modified(),
                block.added(),
                block.removed(),
                attr_prefix,
            );
        }
        DetailRow::ForceReplaceMapHeader { key }
        | DetailRow::ForceReplaceListOfMapsHeader { key } => {
            writeln!(
                out,
                "{}{}: {}",
                attr_prefix,
                key,
                "(forces replacement)".magenta()
            )
            .unwrap();
        }
        DetailRow::Removed { key, old } => {
            writeln!(
                out,
                "{}{}: {} → {}",
                attr_prefix,
                key,
                render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
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
            writeln!(
                out,
                "{}{}",
                attr_prefix,
                hidden_unchanged_summary(*count, "attribute").dimmed()
            )
            .unwrap();
        }
        DetailRow::ReplaceRemoved { key, old } => {
            writeln!(
                out,
                "{}{}: {} → {} {}",
                attr_prefix,
                key,
                render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                "(removed)".red().strikethrough(),
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
                render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                render_value_ansi(new, AnsiValueStyle::Green, None),
                "(forces replacement, known after apply)".magenta()
            )
            .unwrap();
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
                    "{}  {} via temporary name {}, will rename back to {} after old resource is deleted",
                    attr_prefix,
                    "note:".magenta().bold(),
                    render_value_ansi(temporary_value, AnsiValueStyle::Magenta, None),
                    render_value_ansi(original_value, AnsiValueStyle::Green, None)
                )
                .unwrap();
            } else {
                writeln!(
                    out,
                    "{}  {} name will be {} (cannot rename create-only attribute \"{}\")",
                    attr_prefix,
                    "note:".magenta().bold(),
                    render_value_ansi(temporary_value, AnsiValueStyle::Magenta, None),
                    attribute.magenta()
                )
                .unwrap();
            }
        }
    }
}

/// Render map diff entries with ANSI colors.
fn render_map_diff_entries(out: &mut String, entries: &[MapDiffEntryIR], attr_prefix: &str) {
    for entry in entries {
        match entry {
            MapDiffEntryIR::Changed {
                key,
                old,
                new,
                annotation,
            } => {
                let annotation_suffix = annotation_suffix(annotation.as_deref());
                writeln!(
                    out,
                    "{}  {} {}: {} → {}{}",
                    attr_prefix,
                    "~".yellow(),
                    key,
                    render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                    colored_value(new),
                    annotation_suffix
                )
                .unwrap();
            }
            MapDiffEntryIR::Added {
                key,
                value,
                annotation,
            } => {
                let annotation_suffix = annotation_suffix(annotation.as_deref());
                writeln!(
                    out,
                    "{}  {} {}: {}{}",
                    attr_prefix,
                    "+".green(),
                    key,
                    colored_value(value),
                    annotation_suffix
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
                    render_value_ansi(value, AnsiValueStyle::RedStrikethrough, None)
                )
                .unwrap();
            }
            MapDiffEntryIR::NestedMapDiff { key, entries } => {
                writeln!(out, "{}    {}:", attr_prefix, key).unwrap();
                let nested_prefix = format!("{}    ", attr_prefix);
                render_map_diff_entries(out, entries.as_slice(), &nested_prefix);
            }
            MapDiffEntryIR::NestedListOfMapsDiff { key, block } => {
                writeln!(out, "{}    {}:", attr_prefix, key).unwrap();
                let nested_prefix = format!("{}    ", attr_prefix);
                render_list_of_maps_diff(
                    out,
                    block.unchanged(),
                    block.modified(),
                    block.added(),
                    block.removed(),
                    &nested_prefix,
                );
            }
            MapDiffEntryIR::StringListChanged {
                key,
                unchanged,
                added,
                removed,
            } => {
                // #3234: nested List<scalar> in a Map.
                writeln!(out, "{}    {}:", attr_prefix, key).unwrap();
                let nested_prefix = format!("{}    ", attr_prefix);
                render_string_list_diff_entries(out, unchanged, added, removed, &nested_prefix);
            }
        }
    }
}

fn annotation_suffix(annotation: Option<&str>) -> String {
    annotation.map_or_else(String::new, |annotation| {
        format!("  {}", annotation.dimmed())
    })
}

/// Render a string-list diff (#2943) with ANSI colors. The
/// `# (n unchanged elements hidden)` summary trails the diff lines to
/// match the placement of `# (n unchanged fields hidden)` in
/// `render_list_of_maps_diff` and `# (n unchanged attributes hidden)`
/// at the top level.
fn render_string_list_diff_entries(
    out: &mut String,
    unchanged: &[RenderedValue],
    added: &[RenderedValue],
    removed: &[RenderedValue],
    attr_prefix: &str,
) {
    for s in removed {
        writeln!(
            out,
            "{}  {} {}",
            attr_prefix,
            "-".red().strikethrough(),
            render_value_ansi(s, AnsiValueStyle::RedStrikethrough, None)
        )
        .unwrap();
    }
    for s in added {
        writeln!(
            out,
            "{}  {} {}",
            attr_prefix,
            "+".green(),
            render_value_ansi(s, AnsiValueStyle::Green, None)
        )
        .unwrap();
    }
    if !unchanged.is_empty() {
        writeln!(
            out,
            "{}  {}",
            attr_prefix,
            hidden_unchanged_summary(unchanged.len(), "element").dimmed()
        )
        .unwrap();
    }
}

/// Render a list-of-maps diff with ANSI colors.
fn render_list_of_maps_diff(
    out: &mut String,
    unchanged: &[RenderedValue],
    modified: &[ListOfMapsDiffModified],
    added: &[ListOfMapsDiffItem],
    removed: &[ListOfMapsDiffItem],
    attr_prefix: &str,
) {
    for item in unchanged {
        writeln!(out, "{}    {}", attr_prefix, item).unwrap();
    }
    for item in modified {
        // `StringListChanged` (#2943) forces the block layout for the
        // same reason `NestedMapChanged` (#2881) does: its rendering
        // spans multiple lines and cannot fit inside the inline
        // `~ {field: value, ...}` summary.
        let has_block_field = item.fields.iter().any(|f| {
            matches!(
                f,
                ListOfMapsDiffField::NestedMapChanged { .. }
                    | ListOfMapsDiffField::StringListChanged { .. }
            )
        });
        if has_block_field {
            writeln!(out, "{}  {} {{", attr_prefix, "~".yellow().bold()).unwrap();
            for field in item.fields.iter() {
                match field {
                    ListOfMapsDiffField::Changed { key, old, new } => {
                        writeln!(
                            out,
                            "{}    {} {}: {} → {}",
                            attr_prefix,
                            "~".yellow(),
                            key,
                            render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                            render_value_ansi(new, AnsiValueStyle::Green, None)
                        )
                        .unwrap();
                    }
                    ListOfMapsDiffField::NestedMapChanged { key, entries } => {
                        writeln!(out, "{}      {}:", attr_prefix, key).unwrap();
                        let nested_prefix = format!("{}      ", attr_prefix);
                        render_map_diff_entries(out, entries, &nested_prefix);
                    }
                    ListOfMapsDiffField::StringListChanged {
                        key,
                        unchanged,
                        added,
                        removed,
                    } => {
                        writeln!(out, "{}      {}:", attr_prefix, key).unwrap();
                        let nested_prefix = format!("{}      ", attr_prefix);
                        render_string_list_diff_entries(
                            out,
                            unchanged,
                            added,
                            removed,
                            &nested_prefix,
                        );
                    }
                }
            }
            // #2881: surface the number of unchanged sibling fields
            // (only set in Full mode by `compute_list_of_maps_diff_parts`).
            // Mirrors the top-level `# (n unchanged attributes hidden)` row.
            if item.hidden_unchanged_count > 0 {
                writeln!(
                    out,
                    "{}      {}",
                    attr_prefix,
                    hidden_unchanged_summary(item.hidden_unchanged_count, "field").dimmed()
                )
                .unwrap();
            }
            writeln!(out, "{}    }}", attr_prefix).unwrap();
        } else {
            // `item.fields` is `NonEmptyVec`, so the changed-field
            // string is statically non-empty — no need to guard the
            // separator (#2886).
            let rendered_fields = render_modified_fields(item.fields.as_slice());
            let summary = if item.hidden_unchanged_count > 0 {
                let s = hidden_unchanged_summary(item.hidden_unchanged_count, "field")
                    .dimmed()
                    .to_string();
                format!("{}, {}", rendered_fields, s)
            } else {
                rendered_fields
            };
            writeln!(
                out,
                "{}  {} {{{}}}",
                attr_prefix,
                "~".yellow().bold(),
                summary
            )
            .unwrap();
        }
    }
    for item in added {
        render_added_removed_block(out, item, attr_prefix, ListOfMapsDiffItemKind::Added);
    }
    for item in removed {
        render_added_removed_block(out, item, attr_prefix, ListOfMapsDiffItemKind::Removed);
    }
}

/// Render a wholly added or removed list-of-maps element as a multi-line
/// block (#2877). Mirrors the modified-with-nested layout
/// (`~ {\n  key: value\n  ...\n}`) so all three diff markers (`+`, `-`,
/// `~`) share the same visual shape.
///
/// Each field's value is laid out via `PrettyValue::render` so nested
/// long lists / maps wrap to multiple indented lines instead of dumping
/// inline. The pre-fix path stringified the whole element with
/// `format_value` and emitted it on one line (`+ {action: [...], ...}`),
/// which produced unreadable ~500-column lines for IAM policy statements.
fn render_added_removed_block(
    out: &mut String,
    item: &ListOfMapsDiffItem,
    attr_prefix: &str,
    kind: ListOfMapsDiffItemKind,
) {
    let marker = match kind {
        ListOfMapsDiffItemKind::Added => "+".green().bold().to_string(),
        ListOfMapsDiffItemKind::Removed => "-".red().bold().strikethrough().to_string(),
    };
    writeln!(out, "{}  {} {{", attr_prefix, marker).unwrap();
    // Fields render at `attr_prefix.cols + 6`: 2 leading spaces, "+ {" is 3
    // chars, then 1 more for the inner `  ` padding before the key — same
    // column the modified-with-nested branch above uses for its
    // `Unchanged` arm at `{attr_prefix}      `. The indent carries
    // `attr_prefix` (not bare spaces) so a nested resource's `│` gutter is
    // drawn on every field row, not dropped (#3356).
    let field_indent_cols = attr_prefix.chars().count() + 6;
    let field_indent = format!("{}      ", attr_prefix);
    let mut prev_needs_separator = false;
    for (key, value) in &item.fields {
        // Mirror `format_map_vertical` / `MapExpanded`: a multi-element
        // list-of-maps child needs a blank line before the next sibling
        // key so the boundary stays visible (#2555). The blank still
        // carries the bare gutter so the tree bar stays continuous.
        if prev_needs_separator {
            writeln!(out, "{}", attr_prefix.trim_end()).unwrap();
        }
        prev_needs_separator = value.needs_trailing_separator();
        let layout = carina_core::value::PrettyLayout::for_map_key(field_indent_cols, key);
        let pretty = value.render(layout);
        let cv = match kind {
            ListOfMapsDiffItemKind::Added => {
                render_value_ansi(&pretty, AnsiValueStyle::Semantic, Some(attr_prefix))
            }
            ListOfMapsDiffItemKind::Removed => {
                render_value_ansi(&pretty, AnsiValueStyle::RedStrikethrough, Some(attr_prefix))
            }
        };
        writeln!(out, "{}{}: {}", field_indent, key, cv).unwrap();
    }
    writeln!(out, "{}    }}", attr_prefix).unwrap();
}

/// Render modified fields from structured IR into colored output.
fn render_modified_fields(fields: &[ListOfMapsDiffField]) -> String {
    let mut result_parts = Vec::new();
    for field in fields {
        match field {
            ListOfMapsDiffField::Changed { key, old, new } => {
                result_parts.push(format!(
                    "{}: {} → {}",
                    key,
                    render_value_ansi(old, AnsiValueStyle::RedStrikethrough, None),
                    render_value_ansi(new, AnsiValueStyle::Green, None)
                ));
            }
            ListOfMapsDiffField::NestedMapChanged { key, .. } => {
                result_parts.push(format!("{}: (nested changes)", key));
            }
            ListOfMapsDiffField::StringListChanged { .. } => {
                // `has_block_field` in `render_list_of_maps_diff` keeps
                // `StringListChanged` out of the inline summary path.
                unreachable!(
                    "StringListChanged should be handled by the block layout, not the inline summary"
                );
            }
        }
    }
    result_parts.join(", ")
}

pub fn format_effect<S>(effect: &Effect<S>) -> String {
    match effect {
        Effect::Create(r) => format!("Create {}", r.id.human()),
        Effect::ResumeCreate { resource, .. } => {
            format!("Resume create {}", resource.id.human())
        }
        Effect::Update { to, .. } => format!("Update {}", to.id.human()),
        Effect::Delete {
            id,
            identifier,
            generation,
            binding,
            ..
        } => {
            let display_name = binding.as_deref().unwrap_or(id.identity_str());
            let deposed_note = if matches!(
                generation,
                carina_core::effect::EffectGeneration::Deposed(_)
            ) {
                format!(" (deposed {identifier})")
            } else {
                String::new()
            };
            format!(
                "Delete {} {}{}",
                id.display_type(),
                display_name,
                deposed_note
            )
        }
        Effect::Read { resource } => {
            format!("Read {}", resource.id.human())
        }
        Effect::Import { id, identifier } => {
            format!(
                "Import {} (id: {})",
                id.human(),
                carina_core::effect::format_import_identifier(identifier)
            )
        }
        Effect::Remove { id } => {
            format!("Remove {} from state", id.human())
        }
        Effect::Move { from, to } => {
            format!("Move {} -> {}", from.human(), to.human())
        }
        Effect::Wait {
            identity,
            until_surface,
            ..
        } => {
            let binding = identity.to_string();
            format!("Wait {} (until {})", binding, until_surface)
        }
        Effect::DeferredCreate { id, template } => {
            format!(
                "Expand deferred for {} (waits on {})",
                id.human(),
                template.binding_description()
            )
        }
        Effect::DeferredReplace(payload) => {
            format!(
                "Deferred replace {} (waits on {})",
                payload.id.human(),
                payload.template.binding_description()
            )
        }
    }
}

#[cfg(test)]
mod tests;
