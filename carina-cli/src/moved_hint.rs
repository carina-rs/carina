use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::output_style::Colorize;
use carina_core::effect::{Effect, EffectGeneration};
use carina_core::identifier::{DesiredCreateOnlyValues, StateCreateOnlyValues};
use carina_core::plan::Plan;
use carina_core::provider::{ComparisonResolutionError, ProviderRouter};
use carina_core::resource::{Current, ResolvedResource, ResolvedResourceId, ResourceId, Value};
use carina_core::schema::{SchemaRegistry, ValidatedResourceSchema};

/// One plan-time suggestion. Rendering belongs to this typed value rather
/// than to any individual CLI print site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovedBlockHint {
    /// Exactly one destroyed row pairs with exactly one created resource.
    Suggest(MatchedPair),
    /// A connected group for which Carina cannot choose pairs safely.
    Ambiguous(AmbiguousGroup),
}

/// One uniquely matched destroyed row and created resource.
///
/// Its fields and constructor are private so a caller cannot fabricate a
/// cross-provider or cross-type suggestion outside detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPair {
    from: ResolvedResourceId,
    to: ResolvedResourceId,
}

impl MatchedPair {
    fn new(from: ResolvedResourceId, to: ResolvedResourceId) -> Option<Self> {
        (from.provider() == to.provider()
            && from.resource_type() == to.resource_type()
            && from.provider_instance() == to.provider_instance())
        .then_some(Self { from, to })
    }

    pub fn from(&self) -> &ResolvedResourceId {
        &self.from
    }

    pub fn to(&self) -> &ResolvedResourceId {
        &self.to
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NonEmptyResourceIds(Box<[ResolvedResourceId]>);

impl NonEmptyResourceIds {
    fn new(ids: Vec<ResolvedResourceId>) -> Option<Self> {
        (!ids.is_empty()).then(|| Self(ids.into_boxed_slice()))
    }

    fn as_slice(&self) -> &[ResolvedResourceId] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MultipleResourceIds(Box<[ResolvedResourceId]>);

impl MultipleResourceIds {
    fn new(ids: Vec<ResolvedResourceId>) -> Option<Self> {
        (ids.len() >= 2).then(|| Self(ids.into_boxed_slice()))
    }

    fn as_slice(&self) -> &[ResolvedResourceId] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AmbiguousMembers {
    MultipleFrom {
        from: MultipleResourceIds,
        to: NonEmptyResourceIds,
    },
    MultipleTo {
        from: NonEmptyResourceIds,
        to: MultipleResourceIds,
    },
}

/// A connected match component with at least one candidate on both sides and
/// at least two candidates on one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousGroup(AmbiguousMembers);

impl AmbiguousGroup {
    fn new(from: Vec<ResolvedResourceId>, to: Vec<ResolvedResourceId>) -> Option<Self> {
        if from.len() >= 2 {
            return Some(Self(AmbiguousMembers::MultipleFrom {
                from: MultipleResourceIds::new(from)?,
                to: NonEmptyResourceIds::new(to)?,
            }));
        }
        Some(Self(AmbiguousMembers::MultipleTo {
            from: NonEmptyResourceIds::new(from)?,
            to: MultipleResourceIds::new(to)?,
        }))
    }

    pub fn from(&self) -> &[ResolvedResourceId] {
        match &self.0 {
            AmbiguousMembers::MultipleFrom { from, .. } => from.as_slice(),
            AmbiguousMembers::MultipleTo { from, .. } => from.as_slice(),
        }
    }

    pub fn to(&self) -> &[ResolvedResourceId] {
        match &self.0 {
            AmbiguousMembers::MultipleFrom { to, .. } => to.as_slice(),
            AmbiguousMembers::MultipleTo { to, .. } => to.as_slice(),
        }
    }
}

/// Deterministically ordered moved-block diagnostics for one plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MovedBlockHints(Vec<MovedBlockHint>);

impl MovedBlockHints {
    /// Whether the plan has no moved-block diagnostics.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the individual typed diagnostics.
    pub fn as_slice(&self) -> &[MovedBlockHint] {
        &self.0
    }

    /// Print these diagnostics to stderr using the CLI's warning color.
    pub fn eprint(&self, color_mode: crate::output_style::ColorMode) {
        if !self.is_empty() {
            eprintln!("{}", self.to_string().yellow(color_mode));
        }
    }
}

impl fmt::Display for MovedBlockHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Suggest(pair) => {
                let from = render_address(pair.from());
                let to = render_address(pair.to());
                writeln!(
                    f,
                    "Warning: this plan destroys {from} and creates {to}, whose configured create-only values match. If they are the same resource, add this block to keep it instead of replacing it:"
                )?;
                writeln!(f)?;
                writeln!(f, "  moved {{")?;
                writeln!(f, "    from = {from}")?;
                writeln!(f, "    to   = {to}")?;
                write!(f, "  }}")
            }
            Self::Ambiguous(group) => {
                writeln!(
                    f,
                    "Warning: the following destroyed and created resources have matching create-only values, but Carina cannot tell which pairs belong together:"
                )?;
                writeln!(f, "Destroyed:")?;
                for id in group.from() {
                    writeln!(f, "  - {}", render_address(id))?;
                }
                writeln!(f, "Created:")?;
                for id in group.to() {
                    writeln!(f, "  - {}", render_address(id))?;
                }
                write!(
                    f,
                    "Add a moved block for each pair that is the same resource to keep those resources."
                )
            }
        }
    }
}

impl fmt::Display for MovedBlockHints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, hint) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
                writeln!(f)?;
            }
            write!(f, "{hint}")?;
        }
        Ok(())
    }
}

/// Detect possible explicit state moves without changing any plan effect.
///
/// Candidates must have the same provider, resource type, and provider
/// instance. For each concrete create-only attribute configured on an
/// [`Effect::Create`], detection compares its canonical value with the prior
/// current state's value canonicalized through that same schema attribute.
/// Resource identities and bindings are output only and never match keys.
/// Create-only attributes omitted by configuration are ignored.
/// `DeferredCreate` effects (including loops whose iterables remain unknown at
/// plan time) are not candidates, and a resource with no configured
/// create-only values never matches.
pub fn detect_moved_block_hints<S>(
    plan: &Plan<S>,
    prior: &HashMap<ResourceId, Current>,
    schemas: &SchemaRegistry,
    comparison_provider: &ProviderRouter,
) -> Result<MovedBlockHints, ComparisonResolutionError> {
    let mut replacement_creates = HashSet::new();
    let mut replacement_deletes = HashSet::new();
    for replacement in plan.replace_display_info() {
        replacement_creates.insert(replacement.create_idx);
        replacement_deletes.insert(replacement.delete_idx);
    }

    let destroyed = plan
        .effects()
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| match effect {
            Effect::Delete { id, generation, .. }
                if matches!(generation, EffectGeneration::Current)
                    && !replacement_deletes.contains(&index) =>
            {
                prior
                    .get(id.as_inner())
                    .and_then(current_attributes)
                    .filter(|attributes| !attributes.is_empty())
                    .map(|attributes| DestroyCandidate { id, attributes })
            }
            Effect::Delete { .. }
            | Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => None,
        })
        .collect::<Vec<_>>();
    let mut created = Vec::new();
    for (index, effect) in plan.effects().iter().enumerate() {
        match effect {
            Effect::Create(resource) if !replacement_creates.contains(&index) => {
                if let Some(candidate) = create_candidate(resource, schemas, comparison_provider)? {
                    created.push(candidate);
                }
            }
            Effect::Create(_)
            | Effect::Read { .. }
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => {}
        }
    }

    let mut destroyed_to_created = vec![Vec::new(); destroyed.len()];
    let mut created_to_destroyed = vec![Vec::new(); created.len()];
    for (destroyed_index, destroyed_resource) in destroyed.iter().enumerate() {
        for (created_index, created_resource) in created.iter().enumerate() {
            if resources_match(destroyed_resource, created_resource) {
                destroyed_to_created[destroyed_index].push(created_index);
                created_to_destroyed[created_index].push(destroyed_index);
            }
        }
    }

    let mut visited_destroyed = vec![false; destroyed.len()];
    let mut visited_created = vec![false; created.len()];
    let mut components = Vec::new();
    for start in 0..destroyed.len() {
        if visited_destroyed[start] || destroyed_to_created[start].is_empty() {
            continue;
        }

        let mut from_indices = Vec::new();
        let mut to_indices = Vec::new();
        let mut stack = vec![GraphNode::Destroyed(start)];
        while let Some(node) = stack.pop() {
            match node {
                GraphNode::Destroyed(index) => {
                    if visited_destroyed[index] {
                        continue;
                    }
                    visited_destroyed[index] = true;
                    from_indices.push(index);
                    stack.extend(
                        destroyed_to_created[index]
                            .iter()
                            .copied()
                            .map(GraphNode::Created),
                    );
                }
                GraphNode::Created(index) => {
                    if visited_created[index] {
                        continue;
                    }
                    visited_created[index] = true;
                    to_indices.push(index);
                    stack.extend(
                        created_to_destroyed[index]
                            .iter()
                            .copied()
                            .map(GraphNode::Destroyed),
                    );
                }
            }
        }

        let mut from = from_indices
            .into_iter()
            .map(|index| destroyed[index].id.clone())
            .collect::<Vec<_>>();
        let mut to = to_indices
            .into_iter()
            .map(|index| created[index].id.clone())
            .collect::<Vec<_>>();
        from.sort_by_key(render_address);
        to.sort_by_key(render_address);
        let sort_key = render_address(&from[0]);
        let hint = if from.len() == 1 && to.len() == 1 {
            MovedBlockHint::Suggest(
                MatchedPair::new(
                    from.pop().expect("one source"),
                    to.pop().expect("one destination"),
                )
                .expect("a graph edge has one provider, type, and provider instance"),
            )
        } else {
            MovedBlockHint::Ambiguous(
                AmbiguousGroup::new(from, to)
                    .expect("a non-unique connected component is ambiguous"),
            )
        };
        components.push((sort_key, hint));
    }
    components.sort_by(|(left, _), (right, _)| left.cmp(right));

    Ok(MovedBlockHints(
        components
            .into_iter()
            .map(|(_sort_key, hint)| hint)
            .collect(),
    ))
}

struct DestroyCandidate<'a> {
    id: &'a ResolvedResourceId,
    attributes: &'a HashMap<String, Value>,
}

struct CreateCandidate<'schema> {
    id: ResolvedResourceId,
    schema: &'schema ValidatedResourceSchema,
    comparison: carina_core::differ::ObservationComparison<'schema>,
    create_only_values: DesiredCreateOnlyValues,
}

enum GraphNode {
    Destroyed(usize),
    Created(usize),
}

fn current_attributes(current: &Current) -> Option<&HashMap<String, Value>> {
    match current {
        Current::Absent(state) => Some(&state.state().attributes),
        Current::Present(present) => Some(&present.state().attributes),
        Current::CreateInFlight(_pending) => None,
        Current::CreateOutcomeUnknown(_unknown) => None,
        Current::MissingIdentifier(state) => Some(&state.state().attributes),
    }
}

fn create_candidate<'schema, S>(
    created: &ResolvedResource<S>,
    schemas: &'schema SchemaRegistry,
    comparison_provider: &'schema ProviderRouter,
) -> Result<Option<CreateCandidate<'schema>>, ComparisonResolutionError> {
    let Some(schema) = schemas
        .get_for(created.as_resource())
        .map(|schema| schema.schema())
    else {
        return Ok(None);
    };
    let comparison = comparison_provider.comparison_for(schemas, &created.as_resource().id)?;
    let create_only_values = DesiredCreateOnlyValues::without_binding_resolution(
        created.as_resource(),
        schema.as_schema(),
    );
    if !create_only_values.has_configured_values()
        || !create_only_values.all_configured_values_resolved()
    {
        return Ok(None);
    }

    Ok(Some(CreateCandidate {
        id: created.resolved_id(),
        schema,
        comparison: comparison.context().observation(),
        create_only_values,
    }))
}

/// Match only the create-only attributes set on the desired resource.
/// Desired attributes omitted by configuration are intentionally ignored, so
/// a match means the compared values agree, not that the full states are
/// identical.
fn resources_match(destroyed: &DestroyCandidate<'_>, created: &CreateCandidate<'_>) -> bool {
    if destroyed.id.provider() != created.id.provider()
        || destroyed.id.resource_type() != created.id.resource_type()
        || destroyed.id.provider_instance() != created.id.provider_instance()
    {
        return false;
    }

    let prior_values =
        StateCreateOnlyValues::from_state_attributes(created.schema, destroyed.attributes);
    created.create_only_values.all_match(
        &prior_values,
        created.schema.as_schema(),
        &created.comparison,
    )
}

fn render_address(id: &ResolvedResourceId) -> String {
    let resource_type = if id.provider().is_empty() {
        id.resource_type().to_string()
    } else {
        format!("{}.{}", id.provider(), id.resource_type())
    };
    format!(
        "{} {}",
        resource_type,
        render_identity_literal(id.identity_str())
    )
}

fn render_identity_literal(identity: &str) -> String {
    if !identity.contains('\'') && !identity.contains("${") {
        return format!("'{}'", identity.replace('\\', "\\\\"));
    }

    let mut escaped = String::with_capacity(identity.len());
    let mut chars = identity.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '$' if chars.peek() == Some(&'{') => escaped.push_str("\\$"),
            character => escaped.push(character),
        }
    }
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use carina_core::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema,
    };

    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::parser::{ProviderContext, StateBlock, parse_file};
    use carina_core::plan::Plan;
    use carina_core::provider::AppliedIdentifier;
    use carina_core::resource::{
        ApplyTimeUnknown, ConcreteValue, Current, DeferredValue, DeposedKey, Directives,
        EnumValueResolver, ResolvedResource, ResolvedResourceId, Resource, ResourceId,
        ResourceIdentity, State, Value,
    };
    use carina_core::schema::{SchemaRegistry, TypeIdentity};

    use super::{
        MovedBlockHint, MovedBlockHints,
        detect_moved_block_hints as detect_moved_block_hints_with_provider,
    };

    const PROVIDER: &str = "awscc";
    const RESOURCE_TYPE: &str = "ec2.Vpc";
    const CREATE_ONLY_ATTRIBUTE: &str = "cidr_block";
    const CIDR: &str = "10.0.0.0/16";

    fn detect_moved_block_hints<S>(
        plan: &Plan<S>,
        prior: &HashMap<ResourceId, Current>,
        schemas: &SchemaRegistry,
    ) -> MovedBlockHints {
        let (_, comparison_provider) = carina_core::provider::comparison_fixture_for_test(
            schemas,
            plan.effects().iter().filter_map(|effect| match effect {
                Effect::Create(resource) => Some(&resource.as_resource().id),
                _ => None,
            }),
        );
        detect_moved_block_hints_with_provider(plan, prior, schemas, &comparison_provider)
            .expect("moved-hint comparison fixture must resolve")
    }

    fn identity(value: &str) -> ResourceIdentity {
        ResourceIdentity::try_from(value.to_string()).expect("test identity must be non-empty")
    }

    fn id(name: &str, provider_instance: Option<&str>) -> ResolvedResourceId {
        ResolvedResourceId::with_provider_identity(
            PROVIDER,
            RESOURCE_TYPE,
            identity(name),
            provider_instance.map(str::to_string),
        )
    }

    fn concrete_string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    fn schemas(create_only: bool) -> SchemaRegistry {
        let attribute =
            PatternAttributeSchema::optional(CREATE_ONLY_ATTRIBUTE, PatternAttributeType::string());
        let attribute = if create_only {
            attribute.create_only().build()
        } else {
            attribute.build()
        };
        let schema = PatternResourceSchema::new(RESOURCE_TYPE)
            .attribute(attribute)
            .validate()
            .expect("test schema must validate");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(PROVIDER, schema);
        schemas
    }

    fn add_delete(
        plan: &mut Plan,
        prior: &mut HashMap<ResourceId, Current>,
        resource_id: ResolvedResourceId,
        value: &str,
        generation: EffectGeneration,
    ) {
        let state = State::existing(
            resource_id.as_inner().clone(),
            HashMap::from([(CREATE_ONLY_ATTRIBUTE.to_string(), concrete_string(value))]),
        )
        .with_identifier(format!("provider-id-{}", resource_id.identity_str()));
        prior.insert(resource_id.as_inner().clone(), Current::from_state(state));
        plan.add(Effect::Delete {
            id: resource_id,
            identifier: AppliedIdentifier::new("provider-id").expect("identifier is non-empty"),
            generation,
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
    }

    fn add_create(plan: &mut Plan, resource_id: ResolvedResourceId, value: Value) {
        let resource = Resource::from_id(resource_id.into_inner())
            .with_attribute(CREATE_ONLY_ATTRIBUTE, value);
        plan.add(Effect::Create(ResolvedResource::new(resource)));
    }

    fn hint_addresses(hints: &[MovedBlockHint]) -> (Vec<String>, Vec<String>) {
        let [MovedBlockHint::Ambiguous(group)] = hints else {
            panic!("expected exactly one ambiguous hint, got {hints:?}");
        };
        (
            group
                .from()
                .iter()
                .map(|id| id.identity_str().to_string())
                .collect(),
            group
                .to()
                .iter()
                .map(|id| id.identity_str().to_string())
                .collect(),
        )
    }

    #[test]
    fn unique_match_suggests_moved_block() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(&mut plan, id("vpcs[0]", None), concrete_string(CIDR));

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        let [MovedBlockHint::Suggest(pair)] = hints.as_slice() else {
            panic!("expected one suggestion, got {hints:?}");
        };
        assert_eq!(pair.from().identity_str(), "old_vpc");
        assert_eq!(pair.to().identity_str(), "vpcs[0]");
    }

    #[test]
    fn differing_create_only_value_produces_no_hint() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(
            &mut plan,
            id("vpcs[0]", None),
            concrete_string("10.1.0.0/16"),
        );

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert!(hints.is_empty());
    }

    #[test]
    fn schema_without_create_only_attributes_produces_no_hint() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(&mut plan, id("vpcs[0]", None), concrete_string(CIDR));

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(false));

        assert!(hints.is_empty());
    }

    #[test]
    fn apply_time_unknown_desired_create_only_value_produces_no_hint() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(
            &mut plan,
            id("vpcs[0]", None),
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: carina_core::resource::AccessPath::with_fields(
                    "bootstrap",
                    "network",
                    vec!["cidr".into()],
                )
                .resolve_root_for_test(),
            })),
        );

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert!(hints.is_empty());
    }

    #[test]
    fn different_provider_instance_produces_no_hint() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(
            &mut plan,
            id("vpcs[0]", Some("secondary")),
            concrete_string(CIDR),
        );

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert!(hints.is_empty());
    }

    #[test]
    fn enum_create_only_value_matches_raw_state_api_string() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        let old_id = id("validation_record", None);
        add_delete(
            &mut plan,
            &mut prior,
            old_id.clone(),
            CIDR,
            EffectGeneration::Current,
        );
        let prior_attributes = prior
            .get_mut(old_id.as_inner())
            .and_then(Current::attributes_mut)
            .expect("test prior has attributes");
        prior_attributes.clear();
        prior_attributes.insert("type".to_string(), concrete_string("CNAME"));

        let enum_identity = TypeIdentity::new(Some(PROVIDER), ["route53", "RecordSet"], "Type");
        let enum_type = PatternAttributeType::enum_(
            enum_identity,
            Some(vec!["CNAME".to_string()]),
            vec![("CNAME".to_string(), "cname".to_string())],
            None,
            None,
        );
        let compiled = carina_core::schema::ValidatedConfigSchema::validate(
            std::collections::BTreeMap::from([("value".to_string(), enum_type.clone())]),
        )
        .unwrap();
        let canonical = EnumValueResolver::new(&compiled.parts()["value"])
            .resolve_state_text("CNAME")
            .expect("CNAME is a declared enum API value");
        let desired = Resource::from_id(id("validation_records[0]", None).into_inner())
            .with_attribute(
                "type",
                Value::Concrete(ConcreteValue::CanonicalEnum(canonical)),
            );
        plan.add(Effect::Create(ResolvedResource::new(desired)));

        let schema = PatternResourceSchema::new(RESOURCE_TYPE)
            .attribute(
                PatternAttributeSchema::optional("type", enum_type)
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("test enum schema must validate");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(PROVIDER, schema);

        let hints = detect_moved_block_hints(&plan, &prior, &schemas);

        assert!(matches!(hints.as_slice(), [MovedBlockHint::Suggest(_)]));
    }

    #[test]
    fn reordered_unordered_list_create_only_value_suggests_moved_block() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        let old_id = id("old_vpc", None);
        add_delete(
            &mut plan,
            &mut prior,
            old_id.clone(),
            CIDR,
            EffectGeneration::Current,
        );
        let prior_attributes = prior
            .get_mut(old_id.as_inner())
            .and_then(Current::attributes_mut)
            .expect("test prior has attributes");
        prior_attributes.clear();
        prior_attributes.insert(
            "zones".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                concrete_string("zone-a"),
                concrete_string("zone-b"),
            ])),
        );
        let desired = Resource::from_id(id("vpcs[0]", None).into_inner()).with_attribute(
            "zones",
            Value::Concrete(ConcreteValue::List(vec![
                concrete_string("zone-b"),
                concrete_string("zone-a"),
            ])),
        );
        plan.add(Effect::Create(ResolvedResource::new(desired)));

        let schema = PatternResourceSchema::new(RESOURCE_TYPE)
            .attribute(
                PatternAttributeSchema::optional(
                    "zones",
                    PatternAttributeType::unordered_list(PatternAttributeType::string()),
                )
                .create_only()
                .build(),
            )
            .validate()
            .expect("test unordered-list schema must validate");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(PROVIDER, schema);

        let hints = detect_moved_block_hints(&plan, &prior, &schemas);

        let [MovedBlockHint::Suggest(pair)] = hints.as_slice() else {
            panic!("expected one suggestion, got {hints:?}");
        };
        assert_eq!(pair.from().identity_str(), "old_vpc");
        assert_eq!(pair.to().identity_str(), "vpcs[0]");
    }

    #[test]
    fn differing_create_only_name_value_produces_no_hint() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        let old_id = id("old_vpc", None);
        add_delete(
            &mut plan,
            &mut prior,
            old_id.clone(),
            CIDR,
            EffectGeneration::Current,
        );
        prior
            .get_mut(old_id.as_inner())
            .and_then(Current::attributes_mut)
            .expect("test prior has attributes")
            .insert("name".to_string(), concrete_string("old-provider-name"));
        let desired = Resource::from_id(id("vpcs[0]", None).into_inner())
            .with_attribute(CREATE_ONLY_ATTRIBUTE, concrete_string(CIDR))
            .with_attribute("name", concrete_string("new-provider-name"));
        plan.add(Effect::Create(ResolvedResource::new(desired)));

        let schema = PatternResourceSchema::new(RESOURCE_TYPE)
            .attribute(
                PatternAttributeSchema::optional(
                    CREATE_ONLY_ATTRIBUTE,
                    PatternAttributeType::string(),
                )
                .create_only()
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("test schema must validate");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(PROVIDER, schema);

        let hints = detect_moved_block_hints(&plan, &prior, &schemas);

        assert!(hints.is_empty());
    }

    #[test]
    fn equal_name_as_only_create_only_value_suggests_moved_block() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        let old_id = id("old_vpc", None);
        add_delete(
            &mut plan,
            &mut prior,
            old_id.clone(),
            CIDR,
            EffectGeneration::Current,
        );
        let prior_attributes = prior
            .get_mut(old_id.as_inner())
            .and_then(Current::attributes_mut)
            .expect("test prior has attributes");
        prior_attributes.clear();
        prior_attributes.insert("name".to_string(), concrete_string("record.example.com"));
        let desired = Resource::from_id(id("records[0]", None).into_inner())
            .with_attribute("name", concrete_string("record.example.com"));
        plan.add(Effect::Create(ResolvedResource::new(desired)));

        let schema = PatternResourceSchema::new(RESOURCE_TYPE)
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .expect("test schema must validate");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(PROVIDER, schema);

        let hints = detect_moved_block_hints(&plan, &prior, &schemas);

        assert!(matches!(hints.as_slice(), [MovedBlockHint::Suggest(_)]));
    }

    #[test]
    fn replacement_create_and_delete_are_already_claimed() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        add_create(&mut plan, id("vpcs[0]", None), concrete_string(CIDR));
        let mut encoded = serde_json::to_value(&plan).expect("test plan must serialize");
        let previous_attributes =
            HashMap::from([(CREATE_ONLY_ATTRIBUTE.to_string(), concrete_string(CIDR))]);
        encoded["replace_display"] = serde_json::json!([{
            "create_idx": 1,
            "delete_idx": 0,
            "create_before_destroy": false,
            "changed_create_only": [CREATE_ONLY_ATTRIBUTE],
            "cascade_ref_hints": [],
            "temporary_name": null,
            "previous_attributes": previous_attributes,
        }]);
        let plan: carina_core::plan::SavedPlan =
            serde_json::from_value(encoded).expect("replacement plan must deserialize");

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert!(hints.is_empty());
    }

    #[test]
    fn two_destroyed_rows_and_one_create_are_ambiguous() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        for name in ["old_b", "old_a"] {
            add_delete(
                &mut plan,
                &mut prior,
                id(name, None),
                CIDR,
                EffectGeneration::Current,
            );
        }
        add_create(&mut plan, id("vpcs[0]", None), concrete_string(CIDR));

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert_eq!(
            hint_addresses(hints.as_slice()),
            (
                vec!["old_a".to_string(), "old_b".to_string()],
                vec!["vpcs[0]".to_string()]
            )
        );
    }

    #[test]
    fn one_destroyed_row_and_two_creates_are_ambiguous() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Current,
        );
        for name in ["vpcs[1]", "vpcs[0]"] {
            add_create(&mut plan, id(name, None), concrete_string(CIDR));
        }

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert_eq!(
            hint_addresses(hints.as_slice()),
            (
                vec!["old_vpc".to_string()],
                vec!["vpcs[0]".to_string(), "vpcs[1]".to_string()]
            )
        );
    }

    #[test]
    fn deposed_delete_is_ignored() {
        let mut plan = Plan::new();
        let mut prior = HashMap::new();
        add_delete(
            &mut plan,
            &mut prior,
            id("old_vpc", None),
            CIDR,
            EffectGeneration::Deposed(DeposedKey::new_unique()),
        );
        add_create(&mut plan, id("vpcs[0]", None), concrete_string(CIDR));

        let hints = detect_moved_block_hints(&plan, &prior, &schemas(true));

        assert!(hints.is_empty());
    }

    #[test]
    fn suggested_moved_block_addresses_round_trip_through_real_parser() {
        let cases = [
            ("old_vpc", "vpcs[0]"),
            ("_for0[0]", "subnets.a"),
            ("subnets['a b']", "owner's ${vpc}"),
            ("literal${vpc}", "vpcs[0]"),
        ];

        for (from_name, to_name) in cases {
            let hint = MovedBlockHint::Suggest(
                super::MatchedPair::new(id(from_name, None), id(to_name, None))
                    .expect("test addresses share a provider, type, and instance"),
            );
            let rendered = hint.to_string();
            let block = rendered
                .lines()
                .skip_while(|line| *line != "  moved {")
                .take(5)
                .map(|line| line.strip_prefix("  ").unwrap_or(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                block.starts_with("moved {"),
                "missing moved block:\n{rendered}"
            );
            if from_name.contains("${") || to_name.contains("${") {
                assert!(
                    block.contains("\\${"),
                    "double-quoted identity must escape interpolation:\n{block}"
                );
            }

            let source = format!("provider {PROVIDER} {{}}\n\n{block}\n");
            let parsed = parse_file(
                &source,
                Path::new("moved-hint-round-trip.crn"),
                &ProviderContext::default(),
            )
            .unwrap_or_else(|error| panic!("rendered block did not parse:\n{block}\n{error}"));
            assert!(
                parsed.warnings.is_empty(),
                "rendered block produced parser warnings:\n{block}\n{:?}",
                parsed.warnings
            );
            let [StateBlock::Moved { addresses }] = parsed.state_blocks.as_slice() else {
                panic!("expected one parsed moved block: {:?}", parsed.state_blocks);
            };
            let from = addresses.from();
            let to = addresses.to();
            assert_eq!(
                (from.provider.as_str(), from.resource_type.as_str()),
                (PROVIDER, RESOURCE_TYPE)
            );
            assert_eq!(
                (to.provider.as_str(), to.resource_type.as_str()),
                (PROVIDER, RESOURCE_TYPE)
            );
            assert_eq!(from.name_str(), from_name);
            assert_eq!(to.name_str(), to_name);
        }
    }
}
