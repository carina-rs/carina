//! Bind admitted declarations to existing object instances. No predicates are
//! evaluated here; every instance goes through ApplicableInputRule::evaluate.
use super::*;
use crate::resource::knowledge::Classification;
use crate::resource::{DiagnosticScope, ValueString};
use crate::schema::AttrTypeKind;

#[derive(Debug, Clone)]
pub(super) enum Step {
    Field(String),
    Index(usize),
    Key(ValueString),
}

pub(super) fn diagnostic_path(owner: &[Step], path: &InputPath) -> InputDiagnosticPath {
    let mut rendered = String::new();
    for step in owner {
        match step {
            Step::Field(name) => {
                if !rendered.is_empty() {
                    rendered.push('.');
                }
                rendered.push_str(name);
            }
            Step::Index(index) => {
                use std::fmt::Write;
                write!(rendered, "[{index}]").expect("write String");
            }
            Step::Key(key) => {
                rendered.push('[');
                rendered.push_str(&format!(
                    "{:?}",
                    key.diagnostic_literal().unwrap_or("<secret>")
                ));
                rendered.push(']');
            }
        }
    }
    if !rendered.is_empty() {
        rendered.push('.');
    }
    rendered.push_str(path.as_str());
    let mut steps = owner
        .iter()
        .cloned()
        .chain(path.segments().map(|s| Step::Field(s.to_owned())));
    let Some(Step::Field(root)) = steps.next() else {
        unreachable!("instance starts at a resource attribute");
    };
    let relative = steps.fold(crate::schema::FieldPath::new(), |path, step| match step {
        Step::Field(name) => path.push_field(name),
        Step::Index(index) => path.push_index(index),
        Step::Key(key) => path.push_map_key(key.diagnostic_literal().unwrap_or("<secret>")),
    });
    InputDiagnosticPath::new(rendered, root, relative)
}

/// Comparison identity stays typed and protected, independently of rendering.
pub(super) fn canonical_path(owner: &[Step], path: &InputPath) -> crate::schema::LeafPath {
    use crate::schema::{LeafPath, LeafPathSegment};
    LeafPath::from_segments(
        owner
            .iter()
            .map(|step| match step {
                Step::Field(name) => LeafPathSegment::StructField(name.clone()),
                Step::Index(index) => LeafPathSegment::ListIndex(*index),
                Step::Key(key) => LeafPathSegment::MapKey(key.clone()),
            })
            .chain(
                path.segments()
                    .map(|name| LeafPathSegment::StructField(name.to_owned())),
            )
            .skip(1),
    )
}

fn active(site: &WriteSite<'_>, owner: &[Step], rule: &InputRule<Compiled>) -> bool {
    let Some(update) = &site.update else {
        return true;
    };
    if let Some(Step::Field(root)) = owner.first() {
        return update.changed.contains(root);
    }
    match (rule.condition(), rule.predicate()) {
        (
            InputCondition::Always,
            InputPredicate::Present | InputPredicate::Absent | InputPredicate::Equals(_),
        ) => update.changed.contains(rule.path().root()),
        (InputCondition::Always, _) => {
            update.changed.contains(rule.path().root())
                && !update.removed.contains(rule.path().root())
        }
        (
            InputCondition::Present(_) | InputCondition::Equals { .. } | InputCondition::Not(_),
            _,
        ) => {
            update.changed.contains(rule.path().root())
                || rule
                    .condition()
                    .path()
                    .is_some_and(|path| update.changed.contains(path.root()))
        }
    }
}

fn bind<'s, 'a>(
    site: &'s WriteSite<'a>,
    rule: &'a InputRule<Compiled>,
    owner_type: Option<&'a CompiledAttributeType>,
    owner: &[Step],
    pending_instance: bool,
    evidence: RuleEvidence<'s>,
) -> Option<ApplicableInputRule<'s, 'a>> {
    let kind = scope_classification(rule.scope(), site.context)?;
    if !active(site, owner, rule) {
        return None;
    }
    Some(ApplicableInputRule {
        site,
        rule,
        kind,
        owner_type,
        owner: owner.to_vec(),
        path: diagnostic_path(owner, rule.path()),
        pending_instance,
        evidence,
    })
}

pub(super) fn for_site<'s, 'a>(site: &'s WriteSite<'a>) -> Vec<ApplicableInputRule<'s, 'a>> {
    let mut instances = Vec::new();
    let mut fits = super::union_fits::UnionFits::default();
    let evidence = Arc::new(SiteEvidence::new(site));
    for &rule in &site.rules {
        instances.extend(bind(
            site,
            rule,
            None,
            &[],
            false,
            RuleEvidence::Resource(Arc::clone(&evidence)),
        ));
    }
    for (name, attribute) in &site.schema.as_schema().attributes {
        if attribute.input_mode.is_provider_populated() {
            continue;
        }
        let value = evidence.field(name);
        // Omitted objects have no instances. A deferred whole resource has
        // unknown membership, so its nested declarations retain obligations.
        if value.is_none() && evidence.membership_known() {
            continue;
        }
        let mut owner = vec![Step::Field(name.clone())];
        walk(
            site,
            &attribute.attr_type,
            value.as_ref(),
            &mut owner,
            &mut HashSet::new(),
            &mut instances,
            DiagnosticScope::Public,
            &mut fits,
        );
    }
    instances.sort_by(|a, b| (&a.path, a.rule.code()).cmp(&(&b.path, b.rule.code())));
    instances
}

#[allow(clippy::too_many_arguments)]
fn walk<'s, 'a>(
    site: &'s WriteSite<'a>,
    ty: &'a CompiledAttributeType,
    value: Option<&InputEvidence<'s>>,
    owner: &mut Vec<Step>,
    visited_refs: &mut HashSet<String>,
    out: &mut Vec<ApplicableInputRule<'s, 'a>>,
    scope: DiagnosticScope,
    fits: &mut super::union_fits::UnionFits<'a>,
) {
    let defs = site.schema.as_schema().defs();
    if !site
        .schema
        .input_rule_declarations()
        .reachable(ty, site.context)
    {
        return;
    }
    let scope = if value.is_some_and(|value| value.scope().is_secret()) {
        DiagnosticScope::Secret
    } else {
        scope
    };
    let pending = !value.is_some_and(InputEvidence::shape_known);
    match &ty.kind {
        AttrTypeKind::Ref(name) => {
            if !visited_refs.insert(name.clone()) {
                return;
            }
            if let Some(definition) = defs.get(name) {
                walk(
                    site,
                    definition,
                    value,
                    owner,
                    visited_refs,
                    out,
                    scope,
                    fits,
                );
            }
            visited_refs.remove(name);
        }
        AttrTypeKind::Struct {
            fields: declarations,
            input_rules,
            ..
        } => {
            for rule in input_rules {
                out.extend(bind(
                    site,
                    rule,
                    Some(ty),
                    owner,
                    pending,
                    RuleEvidence::Instance(value.cloned()),
                ));
            }
            for field in declarations {
                if field.parts().declared_input_mode.is_provider_populated() {
                    continue;
                }
                let child = value.and_then(|value| value.field(&field.name));
                if child.is_none() && !pending {
                    continue;
                }
                let consumes_value = child.is_some();
                let unselected = value.filter(|_| pending).map(InputEvidence::unselected);
                owner.push(Step::Field(field.name.clone()));
                let mut child_refs = HashSet::new();
                walk(
                    site,
                    &field.field_type,
                    child.as_ref().or(unselected.as_ref()),
                    owner,
                    if consumes_value {
                        &mut child_refs
                    } else {
                        visited_refs
                    },
                    out,
                    scope,
                    fits,
                );
                owner.pop();
            }
        }
        AttrTypeKind::List { element_type, .. } => {
            if let Some(count) = value.and_then(InputEvidence::list_len) {
                for index in 0..count {
                    let item = value.and_then(|value| value.index(index));
                    owner.push(Step::Index(index));
                    walk(
                        site,
                        element_type,
                        item.as_ref(),
                        owner,
                        &mut HashSet::new(),
                        out,
                        scope,
                        fits,
                    );
                    owner.pop();
                }
            } else if pending {
                let unselected = value.map(InputEvidence::unselected);
                walk(
                    site,
                    element_type,
                    unselected.as_ref(),
                    owner,
                    visited_refs,
                    out,
                    scope,
                    fits,
                );
            }
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            if let Some(fields) = value.and_then(InputEvidence::fields) {
                for (key, value) in fields {
                    owner.push(Step::Key(if scope.is_secret() {
                        ValueString::diagnostic_sensitive(key)
                    } else {
                        ValueString::plain(key)
                    }));
                    walk(
                        site,
                        value_type,
                        Some(&value),
                        owner,
                        &mut HashSet::new(),
                        out,
                        scope,
                        fits,
                    );
                    owner.pop();
                }
            } else if pending {
                let unselected = value.map(InputEvidence::unselected);
                walk(
                    site,
                    value_type,
                    unselected.as_ref(),
                    owner,
                    visited_refs,
                    out,
                    scope,
                    fits,
                );
            }
        }
        AttrTypeKind::Union(_) => {
            let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                unreachable!("union declaration");
            };
            match value.and_then(InputEvidence::proof) {
                Some(Classification::Known(_)) => {
                    let mut ref_path = visited_refs
                        .iter()
                        .filter_map(|name| defs.get_key_value(name).map(|(name, _)| name.as_str()))
                        .collect();
                    if let Some(member) =
                        value.and_then(|value| fits.select(ty, value, defs, &mut ref_path))
                    {
                        walk(site, member, value, owner, visited_refs, out, scope, fits);
                    }
                }
                // Branch membership remains unknown; an attempted failure
                // enclosing those possible branches remains a failure.
                Some(
                    Classification::ShapeKnown(_)
                    | Classification::Pending(_)
                    | Classification::Failed(_),
                )
                | None => {
                    let unselected = value.map(InputEvidence::unselected);
                    for member in members {
                        walk(
                            site,
                            member,
                            unselected.as_ref(),
                            owner,
                            visited_refs,
                            out,
                            scope,
                            fits,
                        );
                    }
                }
            }
        }
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::String { .. }
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => {}
    }
}

#[cfg(test)]
thread_local! { static SCHEMA_VISIT_BUDGET: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(super) fn with_schema_visit_budget<R>(limit: usize, operation: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCHEMA_VISIT_BUDGET.set(None);
        }
    }
    SCHEMA_VISIT_BUDGET.set(Some((0, limit)));
    let _reset = Reset;
    operation()
}

#[cfg(test)]
pub(super) fn schema_visit() {
    SCHEMA_VISIT_BUDGET.with(|budget| {
        if let Some((count, limit)) = budget.get() {
            assert!(
                count < limit,
                "input-rule reachability exceeded {limit} schema visits"
            );
            budget.set(Some((count + 1, limit)));
        }
    });
}

pub(super) fn has_declarations(site: &WriteSite<'_>) -> bool {
    site.schema
        .input_rule_declarations()
        .has_declarations(site.context)
}
