//! Bind admitted declarations to existing object instances. No predicates are
//! evaluated here; every instance goes through ApplicableInputRule::evaluate.
use super::*;
use crate::resource::DiagnosticScope;
use crate::schema::AttrTypeKind;

#[derive(Debug, Clone)]
pub(super) enum Step {
    Field(String),
    Index(usize),
    Key(crate::schema::InputDiagnosticKey),
}

/// Shared immutable prefixes cost one segment per traversed edge. Binding a
/// successful rule clones only an Arc; diagnostics alone render the prefix.
#[derive(Clone, Default)]
pub(super) struct OwnerPath(Option<Arc<OwnerPrefix>>);
struct OwnerPrefix {
    parent: OwnerPath,
    step: Step,
    root: Arc<str>,
}
impl OwnerPath {
    fn root(&self) -> Option<&str> {
        self.0.as_ref().map(|prefix| prefix.root.as_ref())
    }
    fn push(&mut self, step: Step) {
        let root = self.0.as_ref().map_or_else(
            || match &step {
                Step::Field(name) => Arc::from(name.as_str()),
                _ => unreachable!("resource attribute first"),
            },
            |prefix| Arc::clone(&prefix.root),
        );
        *self = Self(Some(Arc::new(OwnerPrefix {
            parent: self.clone(),
            step,
            root,
        })));
    }
    fn pop(&mut self) {
        *self = self.0.as_ref().expect("nonempty owner").parent.clone();
    }
    fn segments(&self) -> Vec<&Step> {
        let mut segments = Vec::new();
        let mut current = self;
        while let Some(prefix) = &current.0 {
            segments.push(&prefix.step);
            current = &prefix.parent;
        }
        segments.reverse();
        segments
    }
}

pub(super) fn diagnostic_path(owner: &OwnerPath, path: &InputPath) -> InputDiagnosticPath {
    let owner = owner.segments();
    let mut steps = owner
        .iter()
        .map(|step| (*step).clone())
        .chain(path.segments().map(|s| Step::Field(s.to_owned())));
    let Some(Step::Field(root)) = steps.next() else {
        unreachable!("instance starts at a resource attribute");
    };
    InputDiagnosticPath::from_segments(
        root,
        steps.map(|step| match step {
            Step::Field(name) => crate::schema::InputDiagnosticSegment::Field(name),
            Step::Index(index) => crate::schema::InputDiagnosticSegment::Index(index),
            Step::Key(key) => crate::schema::InputDiagnosticSegment::Key(key),
        }),
    )
}

fn active(site: &WriteSite<'_>, owner: &OwnerPath, rule: &InputRule<Compiled>) -> bool {
    if !site.input.is_update() {
        return true;
    }
    if let Some(root) = owner.root() {
        return site.input.changed(root);
    }
    match (rule.condition(), rule.predicate()) {
        (
            InputCondition::Always,
            InputPredicate::Present | InputPredicate::Absent | InputPredicate::Equals(_),
        ) => site.input.changed(rule.path().root()),
        (InputCondition::Always, _) => {
            site.input.changed(rule.path().root())
                && !matches!(
                    site.input.patch_read(rule.path().root()),
                    PatchRead::Remove(_)
                )
        }
        (
            InputCondition::Present(_) | InputCondition::Equals { .. } | InputCondition::Not(_),
            _,
        ) => {
            site.input.changed(rule.path().root())
                || rule
                    .condition()
                    .path()
                    .is_some_and(|path| site.input.changed(path.root()))
        }
    }
}

fn bind<'s, 'a>(
    site: &'s WriteSite<'a>,
    rule: &'a InputRule<Compiled>,
    owner_type: Option<&'a CompiledAttributeType>,
    owner: &OwnerPath,
    pending_instance: bool,
    instance: InstanceRef,
) -> Option<BoundInputRule<'s, 'a>> {
    let kind = scope_classification(rule.scope(), site.context)?;
    if !active(site, owner, rule) {
        return None;
    }
    Some(BoundInputRule {
        site,
        rule,
        kind,
        owner_type,
        path: OnceLock::new(),
        owner: owner.clone(),
        pending_instance,
        instance,
    })
}

pub(super) fn for_site<'s, 'a>(site: &'s WriteSite<'a>) -> Vec<ApplicableInputRule<'s, 'a>> {
    let mut instances = Vec::new();
    let mut fits = site.union_fits.borrow_mut();
    for &rule in &site.rules {
        instances.extend(
            bind(
                site,
                rule,
                None,
                &OwnerPath::default(),
                false,
                InstanceRef::Resource,
            )
            .map(ApplicableInputRule::instance),
        );
    }
    for (name, attribute) in &site.schema.as_schema().attributes {
        if attribute.input_mode.is_provider_populated()
            || !site
                .schema
                .input_rule_declarations()
                .reachable(&attribute.attr_type, site.context)
        {
            continue;
        }
        let read = site.input.root(name);
        if matches!(read, InputRead::Absent(_)) {
            continue;
        }
        let value = read.instance();
        // Only proven omissions have no instances. Missing update evidence
        // and deferred resources retain a subtree obligation.
        let mut owner = OwnerPath::default();
        owner.push(Step::Field(name.clone()));
        walk(
            site,
            &attribute.attr_type,
            value,
            &mut owner,
            &mut HashSet::new(),
            &mut instances,
            DiagnosticScope::Public,
            &mut fits,
        );
    }
    instances
}

#[allow(clippy::too_many_arguments)]
fn walk<'s, 'a>(
    site: &'s WriteSite<'a>,
    ty: &'a CompiledAttributeType,
    value: InstanceRef,
    owner: &mut OwnerPath,
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
    let scope = if value.scope(&site.input).is_secret() {
        DiagnosticScope::Secret
    } else {
        scope
    };
    let unresolved_union = matches!(ty.kind(), AttrTypeKind::Union(_))
        && !value.node(&site.input).is_some_and(NodeRef::known);
    if !value.node(&site.input).is_some_and(NodeRef::shape_known) || unresolved_union {
        let mut declarations = Vec::new();
        collect_deferred(
            site,
            ty,
            value,
            owner,
            &mut HashSet::new(),
            &mut declarations,
        );
        out.extend(ApplicableInputRule::deferred(declarations));
        return;
    }
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
                out.extend(
                    bind(site, rule, Some(ty), owner, false, value)
                        .map(ApplicableInputRule::instance),
                );
            }
            for field in declarations {
                if field.parts().declared_input_mode.is_provider_populated()
                    || !site
                        .schema
                        .input_rule_declarations()
                        .reachable(&field.field_type, site.context)
                {
                    continue;
                }
                let child = match value.node(&site.input).map(|node| node.field(field)) {
                    Some(InputRead::Absent(_)) => continue,
                    Some(read) => read.instance(),
                    None => value.deferred(&site.input),
                };
                let consumes_value = matches!(child, InstanceRef::Node(_));
                owner.push(Step::Field(field.name.clone()));
                let mut child_refs = HashSet::new();
                walk(
                    site,
                    &field.field_type,
                    child,
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
            if let Some(count) = value.node(&site.input).and_then(NodeRef::list_len) {
                for index in 0..count {
                    let item = value
                        .node(&site.input)
                        .and_then(|node| node.index(index))
                        .map_or_else(|| value.deferred(&site.input), NodeRef::instance);
                    owner.push(Step::Index(index));
                    walk(
                        site,
                        element_type,
                        item,
                        owner,
                        &mut HashSet::new(),
                        out,
                        scope,
                        fits,
                    );
                    owner.pop();
                }
            }
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            if let Some(fields) = value.node(&site.input).and_then(NodeRef::fields) {
                for (ordinal, (key, value)) in fields.into_iter().enumerate() {
                    owner.push(Step::Key(if scope.is_secret() {
                        crate::schema::InputDiagnosticKey::Protected { ordinal }
                    } else {
                        crate::schema::InputDiagnosticKey::Public(key.into())
                    }));
                    walk(
                        site,
                        value_type,
                        value.input_read().instance(),
                        owner,
                        &mut HashSet::new(),
                        out,
                        scope,
                        fits,
                    );
                    owner.pop();
                }
            }
        }
        AttrTypeKind::Union(_) => {
            let mut ref_path = visited_refs
                .iter()
                .filter_map(|name| defs.get_key_value(name).map(|(name, _)| name.as_str()))
                .collect();
            if let Some(member) = value
                .node(&site.input)
                .and_then(|value| fits.select(ty, &value, defs, &mut ref_path))
            {
                walk(site, member, value, owner, visited_refs, out, scope, fits);
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

fn collect_deferred<'s, 'a>(
    site: &'s WriteSite<'a>,
    ty: &'a CompiledAttributeType,
    value: InstanceRef,
    owner: &mut OwnerPath,
    visited: &mut HashSet<declarations::DeclarationNode>,
    out: &mut Vec<BoundInputRule<'s, 'a>>,
) {
    if !site
        .schema
        .input_rule_declarations()
        .reachable(ty, site.context)
        || !visited.insert(declarations::DeclarationNode::of(ty))
    {
        return;
    }
    let defs = site.schema.as_schema().defs();
    match ty.kind() {
        AttrTypeKind::Ref(name) => {
            if let Some(ty) = defs.get(name) {
                collect_deferred(site, ty, value, owner, visited, out);
            }
        }
        AttrTypeKind::Struct {
            fields,
            input_rules,
            ..
        } => {
            for rule in input_rules {
                out.extend(bind(site, rule, Some(ty), owner, true, value));
            }
            let unselected = value.deferred(&site.input);
            for field in fields {
                if field.parts().declared_input_mode.is_provider_populated() {
                    continue;
                }
                owner.push(Step::Field(field.name.clone()));
                collect_deferred(site, &field.field_type, unselected, owner, visited, out);
                owner.pop();
            }
        }
        AttrTypeKind::List { element_type, .. } => {
            let unselected = value.deferred(&site.input);
            collect_deferred(site, element_type, unselected, owner, visited, out);
        }
        AttrTypeKind::Map {
            value: element_type,
            ..
        } => {
            let unselected = value.deferred(&site.input);
            collect_deferred(site, element_type, unselected, owner, visited, out);
        }
        AttrTypeKind::Union(_) => {
            let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                unreachable!("union declaration");
            };
            let unselected = value.deferred(&site.input);
            for member in members {
                collect_deferred(site, member, unselected, owner, visited, out);
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
