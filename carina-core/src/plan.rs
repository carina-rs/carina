//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::effect::{ChangedCreateOnly, Effect, TemporaryName};
use crate::module::DependencyGraph;
use crate::name_override::NameOverride;
pub use crate::resource::ModuleSource;
use crate::resource::{
    Directives, ResolvedResource, ResolvedResourceId, ResourceId, ResourceIdentity, Value,
};

/// Error when a plan would violate a directive constraint
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanError {
    /// The resource that triggered the error
    pub resource_id: ResourceId,
    /// Human-readable description of the violation
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.resource_id, self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PermanentNameOverride {
    /// Identifies which resource the override targets.
    pub resource_id: ResolvedResourceId,
    /// The schema unique-name attribute whose value is overridden.
    pub attribute: String,
    /// The temporary value used on the cloud side.
    pub temp_value: String,
    /// The DSL value at the time the override was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplaceDisplayMetadata {
    /// Index into `Plan.effects` for the Create half of this replace.
    pub create_idx: usize,
    /// Index into `Plan.effects` for the Delete half of this replace.
    pub delete_idx: usize,
    /// Whether this was a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes whose change drove the replacement.
    pub changed_create_only: ChangedCreateOnly,
    /// Hints mapping attribute names to their original ResourceRef
    /// expressions (e.g. `("vpc_id", "vpc.vpc_id")`). Used by display
    /// to show the binding reference instead of the resolved value
    /// for cascade-triggered replacements.
    pub cascade_ref_hints: Vec<(String, String)>,
    /// Temporary name when CBD used a name swap. `None` for DBC.
    pub temporary_name: Option<TemporaryName>,
    /// Pre-replace state values. Used by display and snapshot tests;
    /// may contain secrets (the redaction walker visits this field
    /// in Phase 7).
    pub previous_attributes: HashMap<String, Value>,
}

/// Borrowed public view of replacement display metadata.
///
/// The plan still owns the serializable metadata internally, but display
/// frontends in other crates need enough information to render decomposed
/// replacement Create/Delete pairs as a single replacement row.
#[derive(Debug, Clone, Copy)]
pub struct ReplaceDisplayInfo<'a> {
    /// Index into `Plan.effects` for the Create half of this replace.
    pub create_idx: usize,
    /// Index into `Plan.effects` for the Delete half of this replace.
    pub delete_idx: usize,
    /// Whether this was a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes whose change drove the replacement.
    pub changed_create_only: &'a [String],
    /// Hints mapping attribute names to their original ResourceRef expressions.
    pub cascade_ref_hints: &'a [(String, String)],
    /// Temporary name when CBD used a name swap. `None` for DBC.
    pub temporary_name: Option<&'a TemporaryName>,
    /// Pre-replace state values used as the "from" side of display diffs.
    pub previous_attributes: &'a HashMap<String, Value>,
}

impl<'a> ReplaceDisplayInfo<'a> {
    fn from_metadata(metadata: &'a ReplaceDisplayMetadata) -> Self {
        Self {
            create_idx: metadata.create_idx,
            delete_idx: metadata.delete_idx,
            create_before_destroy: metadata.create_before_destroy,
            changed_create_only: &metadata.changed_create_only,
            cascade_ref_hints: &metadata.cascade_ref_hints,
            temporary_name: metadata.temporary_name.as_ref(),
            previous_attributes: &metadata.previous_attributes,
        }
    }
}

pub(crate) struct ReplacementGroup {
    /// Create-side payload (identity-resolved).
    pub create: ResolvedResource,
    /// Delete-side payload.
    pub delete: ReplacementDelete,
    /// Whether this is a create-before-destroy replacement.
    pub create_before_destroy: bool,
    /// Create-only attributes that drove the replacement.
    pub changed_create_only: ChangedCreateOnly,
    /// Cascade ref hints for display.
    pub cascade_ref_hints: Vec<(String, String)>,
    /// Temporary name when CBD swaps the name attribute. `None` for DBC.
    pub temporary_name: Option<TemporaryName>,
    /// Permanent name override recorded when CBD swaps the name attribute.
    /// `None` for DBC and CBD paths that did not need a temporary name.
    pub permanent_name_override: Option<PermanentNameOverride>,
    /// Identities of consumer effects (typically Updates) the Delete
    /// must wait for during apply. Populated into
    /// `Effect::Delete.blocked_by_updates`.
    pub consumer_updates: HashSet<ResourceIdentity>,
    /// Pre-replace attribute values, captured for display + snapshot.
    /// The redaction walker visits this in Phase 7.
    pub previous_attributes: HashMap<String, Value>,
}

pub(crate) struct ReplacementDelete {
    pub id: ResolvedResourceId,
    pub identifier: String,
    pub directives: Directives,
    pub binding: Option<String>,
    pub dependencies: HashSet<String>,
    pub explicit_dependencies: HashSet<String>,
}

/// Plan containing Effects to be executed
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    effects: Vec<Effect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) replace_display: Vec<ReplaceDisplayMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) permanent_name_overrides: Vec<PermanentNameOverride>,
    /// Directive constraint violations detected during plan generation
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<PlanError>,
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    pub(crate) fn add_replacement(&mut self, group: ReplacementGroup) {
        let delete = group.delete;
        let delete_effect = Effect::Delete {
            id: delete.id,
            identifier: delete.identifier,
            directives: delete.directives,
            binding: delete.binding,
            dependencies: delete.dependencies,
            explicit_dependencies: delete.explicit_dependencies,
            blocked_by_updates: group.consumer_updates,
        };

        let (create_idx, delete_idx) = if group.create_before_destroy {
            let create_idx = self.effects.len();
            self.effects.push(Effect::Create(group.create));
            let delete_idx = self.effects.len();
            self.effects.push(delete_effect);
            (create_idx, delete_idx)
        } else {
            let delete_idx = self.effects.len();
            self.effects.push(delete_effect);
            let create_idx = self.effects.len();
            self.effects.push(Effect::Create(group.create));
            (create_idx, delete_idx)
        };

        self.replace_display.push(ReplaceDisplayMetadata {
            create_idx,
            delete_idx,
            create_before_destroy: group.create_before_destroy,
            changed_create_only: group.changed_create_only,
            cascade_ref_hints: group.cascade_ref_hints,
            temporary_name: group.temporary_name,
            previous_attributes: group.previous_attributes,
        });

        if let Some(override_) = group.permanent_name_override {
            self.permanent_name_overrides.push(override_);
        }
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    /// Replacement display metadata for decomposed Create/Delete replacement pairs.
    pub fn replace_display_info(&self) -> impl Iterator<Item = ReplaceDisplayInfo<'_>> {
        self.replace_display
            .iter()
            .map(ReplaceDisplayInfo::from_metadata)
    }

    pub(crate) fn replace_display_mut(&mut self) -> &mut Vec<ReplaceDisplayMetadata> {
        &mut self.replace_display
    }

    pub fn permanent_name_overrides_for_state(
        &self,
    ) -> HashMap<ResourceId, HashMap<String, NameOverride>> {
        let mut overrides: HashMap<ResourceId, HashMap<String, NameOverride>> = HashMap::new();
        for override_ in &self.permanent_name_overrides {
            overrides
                .entry(override_.resource_id.as_inner().clone())
                .or_default()
                .insert(
                    override_.attribute.clone(),
                    NameOverride {
                        temp_value: override_.temp_value.clone(),
                        original_value: override_.original_value.clone(),
                    },
                );
        }
        overrides
    }

    pub fn is_replacement_delete_index(&self, idx: usize) -> bool {
        self.replace_display
            .iter()
            .any(|metadata| metadata.delete_idx == idx)
    }

    pub(crate) fn effects_mut(&mut self) -> &mut Vec<Effect> {
        &mut self.effects
    }

    // No `Plan::is_empty()`: it ambiguously straddled "no effects at
    // all" (display semantics) and "nothing to apply" (routing
    // semantics). Read/Wait effects are non-mutating but non-empty,
    // and routing on `is_empty()` mis-routed export-only configs
    // through the resource-apply pipeline (carina#3270, carina#3275).
    // Call sites must say what they mean:
    //   - display "no effects at all" → `plan.effects().is_empty()`
    //   - routing "nothing to apply"   → `!plan.has_mutations()`

    /// Add a directive constraint violation error
    pub fn add_error(&mut self, error: PlanError) {
        self.errors.push(error);
    }

    /// Returns directive constraint violation errors
    pub fn errors(&self) -> &[PlanError] {
        &self.errors
    }

    /// Returns true if there are directive constraint violations
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Remove effects that don't satisfy the predicate
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&Effect) -> bool,
    {
        self.effects.retain(f);
    }

    /// Number of mutating Effects
    pub fn mutation_count(&self) -> usize {
        self.effects.iter().filter(|e| e.is_mutating()).count()
    }

    /// True iff the plan contains at least one mutating effect.
    ///
    /// This is the **only** correct routing predicate for "does this
    /// plan need the resource-apply pipeline?". `Read`/`Wait` effects
    /// do not mutate infrastructure, so a plan that holds only those
    /// must take the export-only fast path (`persist_exports_only`)
    /// or short-circuit with "no changes". A predicate built on
    /// `effects().is_empty()` mis-routes any plan that carries a
    /// data-source read — every config with `let x = read aws.*`
    /// produces one (carina#3270 source-driven apply, carina#3275
    /// saved-plan apply).
    pub fn has_mutations(&self) -> bool {
        self.effects.iter().any(|e| e.is_mutating())
    }

    /// Generate a summary of the Plan for display
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        let deferred_summary = crate::plan_tree::deferred_summary_for_plan(self);
        let replacement_create_indices: HashSet<usize> = self
            .replace_display
            .iter()
            .map(|metadata| metadata.create_idx)
            .collect();
        let replacement_delete_indices: HashSet<usize> = self
            .replace_display
            .iter()
            .map(|metadata| metadata.delete_idx)
            .collect();

        summary.replace += self.replace_display.len();

        for (idx, effect) in self.effects.iter().enumerate() {
            match effect {
                Effect::Read { .. } => summary.read += 1,
                Effect::Create(_) => {
                    if !replacement_create_indices.contains(&idx) {
                        summary.create += 1;
                    }
                }
                Effect::Update { .. } => summary.update += 1,
                Effect::Delete { .. } => {
                    if !replacement_delete_indices.contains(&idx) {
                        summary.delete += 1;
                    }
                }
                Effect::DeferredReplace { .. } => {}
                Effect::Import { .. } => summary.import += 1,
                Effect::Remove { .. } => summary.remove += 1,
                Effect::Move { .. } => summary.moved += 1,
                Effect::Wait { .. } => summary.wait += 1,
                Effect::DeferredCreate { .. } => {}
            }
        }
        summary.deferred = deferred_summary.entries;
        summary
    }
}

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub read: usize,
    pub create: usize,
    pub update: usize,
    pub replace: usize,
    pub delete: usize,
    pub deferred: Vec<DeferredSummaryEntry>,
    pub import: usize,
    pub remove: usize,
    pub moved: usize,
    pub wait: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredSummaryEntry {
    pub upstream_binding: String,
    pub verb: String,
    pub action: DeferredSummaryAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredSummaryAction {
    Add,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSummaryPart {
    Read { count: usize },
    Import { count: usize },
    Create { count: usize },
    Update { count: usize },
    Replace { count: usize },
    Delete { count: usize },
    Remove { count: usize },
    Move { count: usize },
    Wait { count: usize },
}

impl PlanSummary {
    pub fn parts(&self) -> Vec<PlanSummaryPart> {
        let mut parts = Vec::new();
        if self.read > 0 {
            parts.push(PlanSummaryPart::Read { count: self.read });
        }
        if self.import > 0 {
            parts.push(PlanSummaryPart::Import { count: self.import });
        }
        parts.push(PlanSummaryPart::Create { count: self.create });
        parts.push(PlanSummaryPart::Update { count: self.update });
        if self.replace > 0 {
            parts.push(PlanSummaryPart::Replace {
                count: self.replace,
            });
        }
        parts.push(PlanSummaryPart::Delete { count: self.delete });
        if self.remove > 0 {
            parts.push(PlanSummaryPart::Remove { count: self.remove });
        }
        if self.moved > 0 {
            parts.push(PlanSummaryPart::Move { count: self.moved });
        }
        if self.wait > 0 {
            parts.push(PlanSummaryPart::Wait { count: self.wait });
        }
        parts
    }

    pub fn render_line(&self) -> String {
        format!("Plan: {}", self.render_body())
    }

    pub fn render_body(&self) -> String {
        self.parts()
            .into_iter()
            .map(|part| match part {
                PlanSummaryPart::Read { count } => format!("{count} to read"),
                PlanSummaryPart::Import { count } => format!("{count} to import"),
                PlanSummaryPart::Create { count } => format!("{count} to create"),
                PlanSummaryPart::Update { count } => format!("{count} to update"),
                PlanSummaryPart::Replace { count } => format!("{count} to replace"),
                PlanSummaryPart::Delete { count } => format!("{count} to delete"),
                PlanSummaryPart::Remove { count } => format!("{count} to remove from state"),
                PlanSummaryPart::Move { count } => format!("{count} to move"),
                PlanSummaryPart::Wait { count } => format!("{count} to wait"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn deferred_lines(&self) -> Vec<String> {
        self.deferred
            .iter()
            .map(|entry| {
                let action = match entry.action {
                    DeferredSummaryAction::Add => "add",
                    DeferredSummaryAction::Replace => "replace",
                };
                format!(
                    "N to {action} after {} {}.",
                    entry.upstream_binding, entry.verb
                )
            })
            .collect()
    }
}

impl std::fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.render_line())
    }
}

/// A Plan with module source information
#[derive(Debug, Clone, Default)]
pub struct ModularPlan {
    /// The underlying plan
    pub plan: Plan,
    /// Effect index -> module source mapping
    pub effect_sources: HashMap<usize, ModuleSource>,
    /// Module name -> dependency graph
    pub module_graphs: HashMap<String, DependencyGraph>,
}

impl ModularPlan {
    /// Create a new empty modular plan
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a modular plan from a regular plan with source extraction
    pub fn from_plan(plan: Plan) -> Self {
        let mut modular = Self {
            plan: plan.clone(),
            effect_sources: HashMap::new(),
            module_graphs: HashMap::new(),
        };

        // Extract module sources from effect resources
        for (idx, effect) in plan.effects().iter().enumerate() {
            let source = match effect {
                Effect::Create(r) => Self::extract_source(&r.module_source),
                Effect::Update { to, .. } => Self::extract_source(&to.module_source),
                Effect::Read { resource } => Self::extract_source(&resource.module_source),
                Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::Wait { .. }
                | Effect::DeferredCreate { .. }
                | Effect::DeferredReplace { .. } => ModuleSource::Root,
            };
            modular.effect_sources.insert(idx, source);
        }

        modular
    }

    fn extract_source(module_source: &Option<ModuleSource>) -> ModuleSource {
        module_source.clone().unwrap_or(ModuleSource::Root)
    }

    /// Get the source for an effect by index
    pub fn source_of(&self, effect_idx: usize) -> &ModuleSource {
        self.effect_sources
            .get(&effect_idx)
            .unwrap_or(&ModuleSource::Root)
    }

    /// Group effects by module source
    pub fn group_by_module(&self) -> HashMap<ModuleSource, Vec<usize>> {
        let mut groups: HashMap<ModuleSource, Vec<usize>> = HashMap::new();

        for (idx, source) in &self.effect_sources {
            groups.entry(source.clone()).or_default().push(*idx);
        }

        // Sort indices within each group
        for indices in groups.values_mut() {
            indices.sort();
        }

        groups
    }

    /// Display effects grouped by module
    pub fn display_by_module(&self) -> String {
        let mut output = String::new();
        let groups = self.group_by_module();

        // Display root effects first
        if let Some(indices) = groups.get(&ModuleSource::Root) {
            output.push_str("Root:\n");
            for idx in indices {
                if let Some(effect) = self.plan.effects().get(*idx) {
                    output.push_str(&format!("  {}\n", format_effect_brief(effect)));
                }
            }
            output.push('\n');
        }

        // Display module effects
        let mut module_sources: Vec<_> = groups.keys().filter(|s| !s.is_root()).cloned().collect();
        module_sources.sort_by(|a, b| match (a, b) {
            (
                ModuleSource::Module {
                    name: n1,
                    instance: i1,
                },
                ModuleSource::Module {
                    name: n2,
                    instance: i2,
                },
            ) => (n1, i1).cmp(&(n2, i2)),
            _ => std::cmp::Ordering::Equal,
        });

        for source in module_sources {
            if let ModuleSource::Module { name, instance } = &source {
                output.push_str(&format!("Module: {} (instance: {})\n", name, instance));

                if let Some(indices) = groups.get(&source) {
                    for idx in indices {
                        if let Some(effect) = self.plan.effects().get(*idx) {
                            output.push_str(&format!("  {}\n", format_effect_brief(effect)));
                        }
                    }
                }
                output.push('\n');
            }
        }

        // Add summary
        let summary = self.plan.summary();
        output.push_str(&format!("Summary: {}\n", summary.render_body()));

        output
    }
}

/// Format an effect briefly for display
fn format_effect_brief(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("{} {}", effect.display_glyph(), r.id),
        Effect::Update { to, .. } => format!("{} {}", effect.display_glyph(), to.id),
        Effect::Delete { id, .. } => format!("{} {}", effect.display_glyph(), id),
        Effect::Read { resource } => {
            format!("{} {} (data source)", effect.display_glyph(), resource.id)
        }
        Effect::Import { id, identifier } => format!(
            "{} {} (import: {})",
            effect.display_glyph(),
            id,
            crate::effect::format_import_identifier(identifier)
        ),
        // carina#3332: leading `x` shape-collides with the `✗` failure
        // indicator used elsewhere in apply output. Use `~` here too —
        // matches the `display`/TUI plan-tree Remove symbol and the
        // operation word disambiguates from Update.
        Effect::Remove { id } => format!("{} {} (remove from state)", effect.display_glyph(), id),
        Effect::Move { from, to } => format!("{} {} (from: {})", effect.display_glyph(), to, from),
        Effect::Wait {
            identity,
            until_surface,
            ..
        } => format!(
            "{} {} (until {})",
            effect.display_glyph(),
            identity,
            until_surface
        ),
        Effect::DeferredCreate {
            id,
            upstream_binding,
            ..
        } => format!(
            "{} {} (deferred for: waits on {})",
            effect.display_glyph(),
            id,
            upstream_binding
        ),
        Effect::DeferredReplace {
            id,
            upstream_binding,
            ..
        } => format!(
            "{} {} (deferred for replace: waits on {})",
            effect.display_glyph(),
            id,
            upstream_binding
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{
        DataSource, ResolvedDataSource, ResolvedResource, Resource, ResourceIdentity,
    };

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
        ResolvedDataSource::new(resource)
    }

    fn changed_create_only() -> ChangedCreateOnly {
        ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap()
    }

    fn cascade_ref_hints() -> Vec<(String, String)> {
        vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())]
    }

    fn temporary_name() -> TemporaryName {
        TemporaryName {
            attribute: "name".to_string(),
            original_value: "main".to_string(),
            temporary_value: "main-cbd".to_string(),
            can_rename: false,
        }
    }

    fn previous_attributes() -> HashMap<String, Value> {
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(crate::resource::ConcreteValue::String(
                "10.0.0.0/16".to_string(),
            )),
        )])
    }

    fn permanent_name_override() -> PermanentNameOverride {
        PermanentNameOverride {
            resource_id: ResolvedResourceId::new(ResourceId::with_identity("ec2.Vpc", "vpc")),
            attribute: "name".to_string(),
            temp_value: "main-cbd".to_string(),
            original_value: Some("main".to_string()),
        }
    }

    fn replacement_group(consumer_updates: HashSet<ResourceIdentity>) -> ReplacementGroup {
        ReplacementGroup {
            create: resolved(Resource::new("ec2.Vpc", "vpc").with_binding("vpc")),
            delete: ReplacementDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "ec2.Vpc", "vpc-old",
                )),
                identifier: "vpc-123".to_string(),
                directives: Directives::default(),
                binding: Some("vpc".to_string()),
                dependencies: HashSet::from(["internet_gateway".to_string()]),
                explicit_dependencies: HashSet::from(["internet_gateway".to_string()]),
            },
            create_before_destroy: true,
            changed_create_only: changed_create_only(),
            cascade_ref_hints: cascade_ref_hints(),
            temporary_name: Some(temporary_name()),
            permanent_name_override: None,
            consumer_updates,
            previous_attributes: previous_attributes(),
        }
    }

    #[test]
    fn empty_plan() {
        let plan = Plan::new();
        assert!(plan.effects().is_empty());
        assert_eq!(plan.mutation_count(), 0);
        assert!(!plan.has_mutations());
    }

    /// carina#3270: a plan that holds only `Read` effects (data-source
    /// reads, with no managed-resource mutation) must report
    /// `has_mutations() == false`. The export-only apply path
    /// (`persist_exports_only`) gates on this; the old `is_empty()`
    /// check returned false for the same plan and mis-routed the
    /// run through the full resource-apply pipeline.
    #[test]
    fn read_only_plan_has_no_mutations() {
        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: resolved_data_source(DataSource::with_provider(
                "aws",
                "iam.Roles",
                "admin_access_roles",
                None,
            )),
        });
        // `effects().is_empty()` is false — Read counts as a present effect.
        assert!(!plan.effects().is_empty());
        // But there is no mutation, so the export-only fast path
        // must take this plan.
        assert!(!plan.has_mutations());
        assert_eq!(plan.mutation_count(), 0);
    }

    #[test]
    fn plan_with_create_has_mutations() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "acm.Certificate",
            "cert",
        ))));
        assert!(plan.has_mutations());
        assert_eq!(plan.mutation_count(), 1);
    }

    /// carina#3332: `Effect::Remove` in the brief renderer must not
    /// lead with `x` (shape-collides with the `✗` failure indicator
    /// used in apply output). Pin the new `~ ... (remove from state)`
    /// shape so a future tweak does not silently re-introduce the
    /// confusing glyph.
    #[test]
    fn format_effect_brief_remove_has_no_failure_shaped_glyph() {
        use crate::resource::ResourceId;
        let id =
            ResourceId::with_identity("aws.route53.RecordSet", "aws_route53_record_set_7059de08");
        let s = format_effect_brief(&Effect::Remove {
            id: crate::resource::ResolvedResourceId::new(id.clone()),
        });
        assert!(!s.contains('x'), "must not contain `x`; got: {s:?}");
        assert!(!s.contains('✗'), "must not contain `✗`; got: {s:?}");
        assert!(
            s.contains("(remove from state)"),
            "must name the operation; got: {s:?}"
        );
        assert!(
            s.contains(&id.to_string()),
            "must include the resource id; got: {s:?}"
        );
    }

    #[test]
    fn format_effect_brief_renders_wait() {
        use crate::resource::{ConcreteValue, ResourceId, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "acm.Certificate",
                "cert",
            )),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(75 * 60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert_eq!(
            format_effect_brief(&e),
            "> cert_issued (until cert.status == aws.acm.Certificate.Status.Issued)"
        );
    }

    #[test]
    fn plan_summary_counts_wait() {
        use crate::resource::{ConcreteValue, ResourceId, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "acm.Certificate",
            "cert",
        ))));
        plan.add(Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "acm.Certificate",
                "cert",
            )),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        });
        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.wait, 1);
    }

    #[test]
    fn plan_summary() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "a"))));
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "b"))));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(
                crate::resource::ResourceId::with_identity("s3.Bucket", "c"),
            ),
            identifier: String::new(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        });

        let summary = plan.summary();
        assert_eq!(summary.create, 2);
        assert_eq!(summary.delete, 1);
    }

    #[test]
    fn plan_summary_counts_decomposed_replacement_as_replace() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet",
        ))));
        plan.add_replacement(replacement_group(HashSet::new()));

        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.replace, 1);
        assert_eq!(summary.delete, 0);
    }

    #[test]
    fn plan_summary_records_deferred_adds() {
        use crate::parser::ForBinding;
        use crate::resource::ResourceId;

        let template_resource = Resource::new("route53.RecordSet", "validation_records[?]");
        let deferred = crate::parser::DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.RecordSet".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable_binding: "cert".to_string(),
            iterable_attr: "domain_validation_options".to_string(),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(
            Resource::new("acm.Certificate", "cert").with_binding("cert"),
        )));
        plan.add(Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.RecordSet",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(deferred),
        });

        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.deferred.len(), 1);
        assert_eq!(summary.deferred[0].upstream_binding, "cert");
        assert_eq!(summary.deferred[0].action, DeferredSummaryAction::Add);
        assert!(summary.to_string().contains("1 to create"));
        assert_eq!(
            summary.deferred_lines(),
            vec!["N to add after cert applies."]
        );
    }

    #[test]
    fn plan_summary_excludes_deferred_replace_from_totals() {
        use crate::effect::{DeferredReplaceDelete, NonEmptyDeletes};
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::resource::{Directives, ResourceId};
        use std::collections::HashSet;

        let template_resource = Resource::new("route53.RecordSet", "validation_records[?]");
        let template = DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.RecordSet".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable_binding: "cert".to_string(),
            iterable_attr: "domain_validation_options".to_string(),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };
        let deletes = (0..3)
            .map(|idx| DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.RecordSet",
                    format!("validation_records[{idx}]"),
                )),
                identifier: format!("record-{idx}"),
                directives: Directives::default(),
                binding: Some(format!("validation_records[{idx}]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            })
            .collect();

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace {
            deletes: NonEmptyDeletes::try_new(deletes).expect("fixture has deletes"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.RecordSet",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(template),
        });

        let summary = plan.summary();
        assert_eq!(summary.replace, 0);
        assert_eq!(summary.delete, 0);
        assert_eq!(summary.deferred.len(), 1);
        assert_eq!(summary.deferred[0].upstream_binding, "cert");
        assert_eq!(summary.deferred[0].action, DeferredSummaryAction::Replace);
        assert_eq!(
            summary.deferred_lines(),
            vec!["N to replace after cert resolves."]
        );
    }

    #[test]
    fn modular_plan_from_plan() {
        let mut plan = Plan::new();

        // Root resource
        plan.add(Effect::Create(resolved(Resource::new("vpc", "main"))));

        // Module resource
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
        plan.add(Effect::Create(resolved(module_resource)));

        let modular = ModularPlan::from_plan(plan);

        assert_eq!(modular.source_of(0), &ModuleSource::Root);
        assert_eq!(
            modular.source_of(1),
            &ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string()
            }
        );
    }

    #[test]
    fn modular_plan_group_by_module() {
        let mut plan = Plan::new();

        // Two root resources
        plan.add(Effect::Create(resolved(Resource::new("vpc", "main"))));
        plan.add(Effect::Create(resolved(Resource::new("subnet", "public"))));

        // Module resource
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
        plan.add(Effect::Create(resolved(module_resource)));

        let modular = ModularPlan::from_plan(plan);
        let groups = modular.group_by_module();

        assert_eq!(groups.get(&ModuleSource::Root).unwrap().len(), 2);
        assert_eq!(
            groups
                .get(&ModuleSource::module("web_tier", "web"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn plan_add_replacement_registers_create_delete_and_display_atomically() {
        let consumer_updates = HashSet::from([ResourceIdentity::new("subnet_update")]);
        let group = replacement_group(consumer_updates.clone());
        let expected_changed_create_only = group.changed_create_only.clone();
        let expected_cascade_ref_hints = group.cascade_ref_hints.clone();
        let expected_temporary_name = group.temporary_name.clone();
        let expected_previous_attributes = group.previous_attributes.clone();

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert_eq!(plan.effects().len(), 2);
        assert!(matches!(plan.effects()[0], Effect::Create(_)));
        match &plan.effects()[1] {
            Effect::Delete {
                blocked_by_updates, ..
            } => assert_eq!(blocked_by_updates, &consumer_updates),
            other => panic!("expected Delete effect, got {other:?}"),
        }

        assert_eq!(plan.replace_display.len(), 1);
        let metadata = &plan.replace_display[0];
        assert_eq!(metadata.create_idx, 0);
        assert_eq!(metadata.delete_idx, 1);
        assert!(metadata.create_before_destroy);
        assert_eq!(metadata.changed_create_only, expected_changed_create_only);
        assert_eq!(metadata.cascade_ref_hints, expected_cascade_ref_hints);
        assert_eq!(metadata.temporary_name, expected_temporary_name);
        assert_eq!(metadata.previous_attributes, expected_previous_attributes);
    }

    #[test]
    fn replace_display_info_exposes_display_fields_without_metadata_visibility() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(HashSet::new()));

        let info = plan
            .replace_display_info()
            .next()
            .expect("replacement display info");
        let expected_changed = changed_create_only();
        let expected_hints = cascade_ref_hints();
        let expected_temporary_name = temporary_name();
        let expected_previous_attributes = previous_attributes();

        assert_eq!(info.create_idx, 0);
        assert_eq!(info.delete_idx, 1);
        assert!(info.create_before_destroy);
        assert_eq!(info.changed_create_only, &*expected_changed);
        assert_eq!(info.cascade_ref_hints, expected_hints.as_slice());
        assert_eq!(info.temporary_name, Some(&expected_temporary_name));
        assert_eq!(info.previous_attributes, &expected_previous_attributes);
    }

    #[test]
    fn plan_add_replacement_preserves_existing_effects_indices() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet-a",
        ))));
        plan.add(Effect::Create(resolved(Resource::new(
            "ec2.Subnet",
            "subnet-b",
        ))));

        plan.add_replacement(replacement_group(HashSet::new()));

        assert_eq!(plan.effects().len(), 4);
        assert_eq!(plan.replace_display.len(), 1);
        assert_eq!(plan.replace_display[0].create_idx, 2);
        assert_eq!(plan.replace_display[0].delete_idx, 3);
    }

    #[test]
    fn plan_replace_display_round_trips_through_serde() {
        let mut plan = Plan::new();
        plan.add_replacement(replacement_group(HashSet::from([ResourceIdentity::new(
            "subnet_update",
        )])));
        let expected_replace_display = plan.replace_display.clone();

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.effects().len(), 2);
        assert_eq!(deserialized.replace_display, expected_replace_display);
    }

    #[test]
    fn plan_add_replacement_pushes_permanent_name_override_on_cbd() {
        let override_ = permanent_name_override();
        let mut group = replacement_group(HashSet::new());
        group.permanent_name_override = Some(override_.clone());

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert_eq!(plan.permanent_name_overrides, vec![override_]);
    }

    #[test]
    fn plan_add_replacement_skips_permanent_name_override_on_dbc() {
        let mut group = replacement_group(HashSet::new());
        group.create_before_destroy = false;
        group.temporary_name = None;
        group.permanent_name_override = None;

        let mut plan = Plan::new();
        plan.add_replacement(group);

        assert!(plan.permanent_name_overrides.is_empty());
    }

    #[test]
    fn plan_permanent_name_overrides_round_trips_through_serde() {
        let mut group = replacement_group(HashSet::new());
        group.permanent_name_override = Some(permanent_name_override());
        let mut plan = Plan::new();
        plan.add_replacement(group);

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized, plan);
    }

    #[test]
    fn plan_without_permanent_name_overrides_field_deserializes_to_empty() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "s3.Bucket",
            "bucket",
        ))));
        plan.add_error(PlanError {
            resource_id: ResourceId::with_identity("s3.Bucket", "bucket"),
            message: "fixture error".to_string(),
        });

        let mut json = serde_json::to_value(&plan).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("permanent_name_overrides");
        assert!(!object.contains_key("permanent_name_overrides"));

        let deserialized: Plan = serde_json::from_value(json).unwrap();
        assert!(deserialized.permanent_name_overrides.is_empty());
        assert_eq!(deserialized.effects().len(), 1);
        assert_eq!(deserialized.errors().len(), 1);
    }

    #[test]
    fn plan_without_replace_display_field_deserializes_to_empty() {
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new(
            "s3.Bucket",
            "bucket",
        ))));
        plan.add_error(PlanError {
            resource_id: ResourceId::with_identity("s3.Bucket", "bucket"),
            message: "fixture error".to_string(),
        });

        let mut json = serde_json::to_value(&plan).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("replace_display");
        assert!(!object.contains_key("replace_display"));

        let deserialized: Plan = serde_json::from_value(json).unwrap();
        assert!(deserialized.replace_display.is_empty());
        assert_eq!(deserialized.effects().len(), 1);
        assert_eq!(deserialized.errors().len(), 1);
    }

    #[test]
    fn plan_serde_round_trip() {
        use crate::resource::ResourceId;

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(Resource::new("s3.Bucket", "a"))));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
            identifier: "b-id".to_string(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.effects().len(), deserialized.effects().len());
        assert_eq!(plan.effects(), deserialized.effects());
    }
}
