//! Plan - Collection of Effects
//!
//! A Plan is an ordered list of Effects to be executed.
//! No side effects occur until the Plan is applied.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::effect::{ChangedCreateOnly, Effect};
use crate::module::DependencyGraph;
pub use crate::resource::ModuleSource;
use crate::resource::ResourceId;

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

/// Plan containing Effects to be executed
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    effects: Vec<Effect>,
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

    pub fn effects(&self) -> &[Effect] {
        &self.effects
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

    /// Set cascading updates on Replace effects that match the given replaced bindings.
    pub fn set_cascading_updates(
        &mut self,
        replaced_bindings: &std::collections::HashSet<String>,
        updates_by_binding: &std::collections::HashMap<String, Vec<crate::effect::CascadingUpdate>>,
    ) {
        for effect in &mut self.effects {
            if let Effect::Replace {
                to,
                cascading_updates,
                ..
            } = effect
            {
                let binding = to.binding.clone();
                if let Some(binding) = binding
                    && replaced_bindings.contains(&binding)
                    && let Some(updates) = updates_by_binding.get(&binding)
                {
                    *cascading_updates = updates.clone();
                }
            }
        }
    }

    /// Merge cascade-triggered create-only attributes into existing effects.
    ///
    /// For a resource already in the plan:
    /// - If it is a Replace, add the new create-only attrs to `changed_create_only`
    /// - If it is an Update, upgrade it to a Replace with the cascade attrs as `changed_create_only`
    /// - Other effect types (Create, Delete, Read) are left unchanged
    pub fn merge_cascade_create_only(
        &mut self,
        resource_id: &crate::resource::ResourceId,
        cascade_attrs: ChangedCreateOnly,
        directives: crate::resource::Directives,
        ref_hints: Vec<(String, String)>,
    ) {
        for effect in &mut self.effects {
            match effect {
                Effect::Replace {
                    id,
                    changed_create_only,
                    cascade_ref_hints,
                    ..
                } if id == resource_id => {
                    for attr in cascade_attrs.iter() {
                        if !changed_create_only.contains(attr) {
                            changed_create_only.push(attr.to_string());
                        }
                    }
                    for hint in &ref_hints {
                        if !cascade_ref_hints.contains(hint) {
                            cascade_ref_hints.push(hint.clone());
                        }
                    }
                    return;
                }
                Effect::Update { id, .. } if id == resource_id => {
                    // Take ownership of the Update fields and upgrade to Replace.
                    // The `Create` here is a throwaway placeholder overwritten
                    // on the next line.
                    let placeholder = crate::resource::Resource::new("", "");
                    let old = std::mem::replace(effect, Effect::Create(placeholder));
                    if let Effect::Update { id, from, to, .. } = old {
                        *effect = Effect::Replace {
                            id,
                            from,
                            to,
                            directives,
                            changed_create_only: cascade_attrs,
                            cascading_updates: vec![],
                            temporary_name: None,
                            cascade_ref_hints: ref_hints,
                        };
                    }
                    return;
                }
                Effect::DeferredReplace { .. } => {}
                _ => {}
            }
        }
    }

    /// Promote a Replace effect to create_before_destroy.
    ///
    /// This is used by auto-detection: when a resource being replaced is
    /// referenced by other resources, it should use create_before_destroy
    /// to avoid breaking dependents during replacement.
    pub fn promote_to_create_before_destroy(&mut self, resource_id: &crate::resource::ResourceId) {
        for effect in &mut self.effects {
            if let Effect::Replace { id, directives, .. } = effect
                && id == resource_id
            {
                directives.create_before_destroy = true;
                return;
            }
        }
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
        for effect in &self.effects {
            match effect {
                Effect::Read { .. } => summary.read += 1,
                Effect::Create(_) => summary.create += 1,
                Effect::Update { .. } => summary.update += 1,
                Effect::Replace {
                    cascading_updates, ..
                } => {
                    summary.replace += 1;
                    summary.update += cascading_updates.len();
                }
                Effect::Delete { .. } => {}
                Effect::DeferredReplace { .. } => {}
                Effect::Import { .. } => summary.import += 1,
                Effect::Remove { .. } => summary.remove += 1,
                Effect::Move { .. } => summary.moved += 1,
                Effect::Wait { .. } => summary.wait += 1,
                Effect::DeferredCreate { .. } => {}
            }
        }
        summary.delete += self
            .effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Delete { .. }))
            .count();
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
                Effect::Replace { to, .. } => Self::extract_source(&to.module_source),
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
        Effect::Update { id, .. } => format!("{} {}", effect.display_glyph(), id),
        Effect::Replace { id, .. } => format!("{} {}", effect.display_glyph(), id),
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
            binding,
            until_surface,
            ..
        } => format!(
            "{} {} (until {})",
            effect.display_glyph(),
            binding,
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
    use crate::resource::Resource;

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
        use crate::resource::DataSource;

        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: DataSource::with_provider("aws", "iam.Roles", "admin_access_roles", None),
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
        plan.add(Effect::Create(Resource::new("acm.Certificate", "cert")));
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
        let id = ResourceId::new("aws.route53.RecordSet", "aws_route53_record_set_7059de08");
        let s = format_effect_brief(&Effect::Remove { id: id.clone() });
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
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
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
        plan.add(Effect::Create(Resource::new("acm.Certificate", "cert")));
        plan.add(Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
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
        plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
        plan.add(Effect::Create(Resource::new("s3.Bucket", "b")));
        plan.add(Effect::Delete {
            id: crate::resource::ResourceId::new("s3.Bucket", "c"),
            identifier: String::new(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        });

        let summary = plan.summary();
        assert_eq!(summary.create, 2);
        assert_eq!(summary.delete, 1);
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
        plan.add(Effect::Create(
            Resource::new("acm.Certificate", "cert").with_binding("cert"),
        ));
        plan.add(Effect::DeferredCreate {
            id: ResourceId::new("route53.RecordSet", "validation_records"),
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
                id: ResourceId::new("route53.RecordSet", format!("validation_records[{idx}]")),
                identifier: format!("record-{idx}"),
                directives: Directives::default(),
                binding: Some(format!("validation_records[{idx}]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
            })
            .collect();

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace {
            deletes: NonEmptyDeletes::try_new(deletes).expect("fixture has deletes"),
            id: ResourceId::new("route53.RecordSet", "validation_records"),
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
        plan.add(Effect::Create(Resource::new("vpc", "main")));

        // Module resource
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
        plan.add(Effect::Create(module_resource));

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
        plan.add(Effect::Create(Resource::new("vpc", "main")));
        plan.add(Effect::Create(Resource::new("subnet", "public")));

        // Module resource
        let module_resource =
            Resource::new("security_group", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });
        plan.add(Effect::Create(module_resource));

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
    fn plan_summary_counts_cascading_updates() {
        use crate::effect::CascadingUpdate;
        use crate::resource::State;
        use crate::resource::{Directives, ResourceId};

        let mut plan = Plan::new();

        // A Replace effect with one cascading update
        let from = State::not_found(ResourceId::new("ec2.Vpc", "vpc")).with_identifier("vpc-123");
        let to = Resource::new("ec2.Vpc", "vpc");
        let cascading = CascadingUpdate {
            id: ResourceId::new("ec2.Subnet", "subnet"),
            from: Box::new(
                State::not_found(ResourceId::new("ec2.Subnet", "subnet"))
                    .with_identifier("subnet-123"),
            ),
            to: (Resource::new("ec2.Subnet", "subnet")),
        };
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.Vpc", "vpc"),
            from: Box::new(from),
            to,
            directives: Directives::default(),
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec![
                "cidr_block".to_string(),
            ])
            .unwrap(),
            cascading_updates: vec![cascading],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let summary = plan.summary();
        assert_eq!(summary.replace, 1);
        assert_eq!(summary.update, 1, "cascading updates should be counted");
        assert_eq!(summary.create, 0);
        assert_eq!(summary.delete, 0);
    }

    #[test]
    fn plan_summary_display_includes_cascading_updates() {
        use crate::effect::CascadingUpdate;
        use crate::resource::State;
        use crate::resource::{Directives, ResourceId};

        let mut plan = Plan::new();

        let from = State::not_found(ResourceId::new("ec2.Vpc", "vpc")).with_identifier("vpc-123");
        let to = Resource::new("ec2.Vpc", "vpc");
        let cascading = CascadingUpdate {
            id: ResourceId::new("ec2.Subnet", "subnet"),
            from: Box::new(
                State::not_found(ResourceId::new("ec2.Subnet", "subnet"))
                    .with_identifier("subnet-123"),
            ),
            to: (Resource::new("ec2.Subnet", "subnet")),
        };
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.Vpc", "vpc"),
            from: Box::new(from),
            to,
            directives: Directives::default(),
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec![
                "cidr_block".to_string(),
            ])
            .unwrap(),
            cascading_updates: vec![cascading],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let display = format!("{}", plan.summary());
        assert!(
            display.contains("1 to update"),
            "display should show cascading updates: {}",
            display
        );
        assert!(
            display.contains("1 to replace"),
            "display should show replace: {}",
            display
        );
    }

    #[test]
    fn plan_serde_round_trip() {
        use crate::resource::ResourceId;

        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.Bucket", "a")));
        plan.add(Effect::Delete {
            id: ResourceId::new("s3.Bucket", "b"),
            identifier: "b-id".to_string(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: std::collections::HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.effects().len(), deserialized.effects().len());
        assert_eq!(plan.effects(), deserialized.effects());
    }
}
