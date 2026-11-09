//! Bind admitted declarations to existing object instances. No predicates are
//! evaluated here; every instance goes through ApplicableInputRule::evaluate.
use super::*;
use crate::resource::knowledge::{Classification, ClassifiedRef, KnowledgeContext};
use crate::resource::{DiagnosticScope, ValueString};
use crate::schema::{AttrTypeKind, PatternAttributeType};

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
    if !scope_applies(rule.scope(), site.context) {
        return false;
    }
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
        (InputCondition::Always, _) => site
            .attributes
            .as_deref()
            .is_some_and(|attrs| attrs.contains_key(rule.path().root())),
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
) -> ApplicableInputRule<'s, 'a> {
    ApplicableInputRule {
        site,
        rule,
        owner_type,
        owner: owner.to_vec(),
        path: diagnostic_path(owner, rule.path()),
        pending_instance,
    }
}

pub(super) fn for_site<'s, 'a>(site: &'s WriteSite<'a>) -> Vec<ApplicableInputRule<'s, 'a>> {
    let mut instances = Vec::new();
    for &rule in &site.rules {
        if active(site, &[], rule) {
            instances.push(bind(site, rule, None, &[], false));
        }
    }
    let values = site
        .update
        .as_ref()
        .map(|update| &update.projected)
        .or(site.attributes.as_deref());
    for (name, attribute) in &site.schema.as_schema().attributes {
        if attribute.input_mode.is_provider_populated() {
            continue;
        }
        let value = values.and_then(|values| values.get(name));
        // Omitted objects have no instances. A deferred whole resource has
        // unknown membership, so its nested declarations retain obligations.
        if value.is_none() && values.is_some() {
            continue;
        }
        let mut owner = vec![Step::Field(name.clone())];
        walk(
            site,
            &attribute.attr_type,
            value,
            &mut owner,
            &mut HashSet::new(),
            &mut instances,
            DiagnosticScope::Public,
        );
    }
    instances.sort_by(|a, b| (&a.path, a.rule.code()).cmp(&(&b.path, b.rule.code())));
    instances
}

fn walk<'s, 'a>(
    site: &'s WriteSite<'a>,
    ty: &'a CompiledAttributeType,
    value: Option<&Value>,
    owner: &mut Vec<Step>,
    pending_refs: &mut HashSet<String>,
    out: &mut Vec<ApplicableInputRule<'s, 'a>>,
    scope: DiagnosticScope,
) {
    let defs = site.schema.as_schema().defs();
    if !has_struct_rules(ty, defs, site.context, &mut HashSet::new()) {
        return;
    }
    // Inspect membership through the shared knowledge boundary, including
    // concrete containers whose child payloads are still pending.
    let evidence = value
        .and_then(|value| value.knowledge(&KnowledgeContext::validation()).ok())
        .map(|k| k.into_classification());
    let protected = evidence
        .as_ref()
        .is_some_and(|e| ClassifiedRef::from(e).secret().is_some());
    let scope = if protected {
        DiagnosticScope::Secret
    } else {
        scope
    };
    // This evaluator owns the protected computation. Diagnostic paths inherit
    // its scope; neither the proof nor a protected key is exposed to clients.
    let evidence = if protected {
        value
            .map(peel_secret)
            .and_then(|value| value.knowledge(&KnowledgeContext::validation()).ok())
            .map(|k| k.into_classification())
    } else {
        evidence
    };
    let fields = evidence.as_ref().and_then(|e| ClassifiedRef::from(e).map());
    let items = evidence
        .as_ref()
        .and_then(|e| ClassifiedRef::from(e).list());
    let pending = !matches!(
        &evidence,
        Some(Classification::Known(_) | Classification::ShapeKnown(_))
    );
    match &ty.kind {
        AttrTypeKind::Ref(name) => {
            if pending && !pending_refs.insert(name.clone()) {
                return;
            }
            if let Some(definition) = defs.get(name) {
                walk(site, definition, value, owner, pending_refs, out, scope);
            }
            if pending {
                pending_refs.remove(name);
            }
        }
        AttrTypeKind::Struct {
            fields: declarations,
            input_rules,
            ..
        } => {
            for rule in input_rules {
                if active(site, owner, rule) {
                    out.push(bind(site, rule, Some(ty), owner, pending));
                }
            }
            for field in declarations {
                if field.parts().declared_input_mode.is_provider_populated() {
                    continue;
                }
                let child = fields
                    .as_ref()
                    .and_then(|fields| fields.get(field.name.as_str()))
                    .map(|value| value.diagnostic_value());
                if child.is_none() && !pending {
                    continue;
                }
                owner.push(Step::Field(field.name.clone()));
                walk(
                    site,
                    &field.field_type,
                    child,
                    owner,
                    pending_refs,
                    out,
                    scope,
                );
                owner.pop();
            }
        }
        AttrTypeKind::List { element_type, .. } => {
            if let Some(items) = items {
                for (index, item) in items.into_iter().enumerate() {
                    owner.push(Step::Index(index));
                    walk(
                        site,
                        element_type,
                        Some(item.diagnostic_value()),
                        owner,
                        pending_refs,
                        out,
                        scope,
                    );
                    owner.pop();
                }
            } else if pending {
                walk(site, element_type, None, owner, pending_refs, out, scope);
            }
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            if let Some(fields) = fields {
                for (key, value) in fields {
                    owner.push(Step::Key(if scope.is_secret() {
                        ValueString::diagnostic_sensitive(key)
                    } else {
                        ValueString::plain(key)
                    }));
                    walk(
                        site,
                        value_type,
                        Some(value.diagnostic_value()),
                        owner,
                        pending_refs,
                        out,
                        scope,
                    );
                    owner.pop();
                }
            } else if pending {
                walk(site, value_type, None, owner, pending_refs, out, scope);
            }
        }
        AttrTypeKind::Union(_) => {
            let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                unreachable!("union declaration");
            };
            // A pending union cannot select an object branch yet. Preserve all
            // branch obligations; known branches use the existing fit judgement.
            for member in members {
                if pending
                    || value.is_some_and(|value| {
                        crate::schema::validate_attr_with_defs(
                            member,
                            value,
                            defs,
                            crate::schema::FieldAncestry::WRITABLE,
                        )
                        .is_ok()
                    })
                {
                    walk(site, member, value, owner, pending_refs, out, scope);
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

fn has_struct_rules(
    ty: &PatternAttributeType<Compiled>,
    defs: &BTreeMap<String, CompiledAttributeType>,
    context: InputWriteContext,
    seen: &mut HashSet<String>,
) -> bool {
    match &ty.kind {
        AttrTypeKind::Struct {
            fields,
            input_rules,
            ..
        } => {
            input_rules
                .iter()
                .any(|r| scope_applies(r.scope(), context))
                || fields
                    .iter()
                    .any(|f| has_struct_rules(&f.field_type, defs, context, seen))
        }
        AttrTypeKind::List { element_type, .. } => {
            has_struct_rules(element_type, defs, context, seen)
        }
        AttrTypeKind::Map { value, .. } => has_struct_rules(value, defs, context, seen),
        AttrTypeKind::Union(_) => {
            let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                unreachable!("union declaration");
            };
            members
                .iter()
                .any(|t| has_struct_rules(t, defs, context, seen))
        }
        AttrTypeKind::Ref(name) => {
            if !seen.insert(name.clone()) {
                return false;
            }
            let result = defs
                .get(name)
                .is_some_and(|t| has_struct_rules(t, defs, context, seen));
            seen.remove(name);
            result
        }
        _ => false,
    }
}

pub(super) fn has_declarations(site: &WriteSite<'_>) -> bool {
    !site.rules.is_empty()
        || site.schema.as_schema().attributes.values().any(|a| {
            has_struct_rules(
                &a.attr_type,
                site.schema.as_schema().defs(),
                site.context,
                &mut HashSet::new(),
            )
        })
}
