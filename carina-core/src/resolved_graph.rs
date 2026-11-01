//! The single fallible transition from authored dependency operands to
//! complete scheduler addresses.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use crate::eval_value::UnexpandedResourceHandle;
use crate::parser::{
    File, ResourceTypePath, UnexpandedCompositionAttribute, UnexpandedDependencies,
    UnexpandedDependencyOperand, WaitBinding,
};
use crate::resource::{
    AbsoluteBindingAddress, AccessMissPolicy, AccessPath, BindingAddress, ConcreteValue,
    DataSource, DeferredValue, InterpolationPart, LiveCompositionAttribute, LoopSlot,
    NoPlaceholder, PathSegment, PersistedDestroyDependencies, PersistedDestroyDependencyProjection,
    PlaceholderKind, ResolvedDataSource, ResolvedDependencies, ResolvedDependencyReads,
    ResolvedResource, ResolvedResourceId, Resource, ResourceId, ResourceIdentity,
    SYNTHETIC_TYPE_PREFIX, Value, resolve_resource_ignore_changes,
};
use crate::schema::SchemaRegistry;
use crate::wait::{ManagedWaitTarget, ResolvedWaitBinding};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandedGraphResolutionError {
    #[error("expanded binding '{binding}' resolves to more than one graph address")]
    AmbiguousBinding { binding: String },
    #[error("{owner}: dependency target '{reference}' does not exist")]
    MissingDependency { owner: String, reference: String },
    #[error("{owner}: expected a managed resource handle; '{reference}' is a data source")]
    DataSourceHandle { owner: String, reference: String },
    #[error("{owner}: expected a managed resource handle; '{reference}' is a wait")]
    WaitHandle { owner: String, reference: String },
    #[error(
        "{owner}: expected resource handle {expected}; got resource handle {actual} from '{reference}'"
    )]
    HandleTypeMismatch {
        owner: String,
        reference: String,
        expected: Box<ResourceTypePath>,
        actual: Box<ResourceTypePath>,
    },
    #[error("module handle output '{output}' belongs to missing composition '{composition}'")]
    MissingComposition { output: String, composition: String },
    #[error("module handle output cycle: {cycle}")]
    HandleCycle { cycle: String },
    #[error("composition dependency cycle: {cycle}")]
    CompositionCycle { cycle: String },
    #[error("destroy-dependency projection cycle: {cycle}")]
    DestroyProjectionCycle { cycle: String },
    #[error(
        "{owner}: module handle output '{reference}' is a resource handle, not a value; select one of its attributes"
    )]
    BareHandleValue { owner: String, reference: String },
    #[error(
        "{owner}: cannot apply access {step} to resource handle '{reference}'; select a resource attribute"
    )]
    InvalidHandleValueAccess {
        owner: String,
        reference: String,
        step: PathSegment,
    },
    #[error("{owner}: failed to rebuild value access after handle resolution: {error}")]
    ValueAccess {
        owner: String,
        error: GraphExpressionError,
    },
    #[error(
        "evaluated resource '{evaluated}' does not match authored dependency source '{authored}'"
    )]
    MismatchedResourcePair {
        evaluated: Box<ResourceId>,
        authored: Box<ResourceId>,
    },
    #[error(transparent)]
    Resource(#[from] crate::resource::ResolvedResourceError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphExpressionError {
    #[error(transparent)]
    Access(crate::resource::ValueAccessError),
    #[error(transparent)]
    Integer(crate::integer_arithmetic::IntegerOperandError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpandedTarget {
    Managed {
        id: ResolvedResourceId,
        value_binding: AbsoluteBindingAddress,
    },
    DataSource {
        id: ResolvedResourceId,
    },
    Wait {
        id: ResolvedResourceId,
    },
}

impl ExpandedTarget {
    fn id(&self) -> &ResolvedResourceId {
        match self {
            Self::Managed { id, .. } | Self::DataSource { id } | Self::Wait { id } => id,
        }
    }
}

/// Addressable graph targets with the language's lookup precedence encoded
/// structurally: authored bindings win over identity spellings.
#[derive(Debug, Clone, Default)]
struct ExpandedTargets {
    bindings: HashMap<AbsoluteBindingAddress, ExpandedTarget>,
    identities: HashMap<AbsoluteBindingAddress, Vec<ExpandedTarget>>,
    prior_bindings: HashMap<AbsoluteBindingAddress, Vec<ExpandedTarget>>,
}

impl ExpandedTargets {
    fn insert_into(
        map: &mut HashMap<AbsoluteBindingAddress, ExpandedTarget>,
        name: AbsoluteBindingAddress,
        target: ExpandedTarget,
    ) -> Result<(), ExpandedGraphResolutionError> {
        if let Some(existing) = map.get(&name) {
            if existing == &target {
                return Ok(());
            }
            match (existing, &target) {
                // Preserve the pre-existing address lookup contract: a
                // managed target wins over a data source within the same
                // lookup class. Binding-vs-identity precedence is handled by
                // the separate maps above this method.
                (ExpandedTarget::Managed { .. }, ExpandedTarget::DataSource { .. }) => {
                    return Ok(());
                }
                (ExpandedTarget::DataSource { .. }, ExpandedTarget::Managed { .. }) => {
                    map.insert(name, target);
                    return Ok(());
                }
                (ExpandedTarget::Managed { .. }, ExpandedTarget::Managed { .. })
                | (ExpandedTarget::DataSource { .. }, ExpandedTarget::DataSource { .. })
                | (ExpandedTarget::Wait { .. }, ExpandedTarget::Wait { .. })
                | (ExpandedTarget::Managed { .. }, ExpandedTarget::Wait { .. })
                | (ExpandedTarget::DataSource { .. }, ExpandedTarget::Wait { .. })
                | (ExpandedTarget::Wait { .. }, ExpandedTarget::Managed { .. })
                | (ExpandedTarget::Wait { .. }, ExpandedTarget::DataSource { .. }) => {
                    return Err(ExpandedGraphResolutionError::AmbiguousBinding {
                        binding: name.to_string(),
                    });
                }
            }
        }
        map.insert(name, target);
        Ok(())
    }

    fn insert_binding(
        &mut self,
        binding: AbsoluteBindingAddress,
        target: ExpandedTarget,
    ) -> Result<(), ExpandedGraphResolutionError> {
        Self::insert_into(&mut self.bindings, binding, target)
    }

    fn insert_identity(
        &mut self,
        identity: AbsoluteBindingAddress,
        target: ExpandedTarget,
    ) -> Result<(), ExpandedGraphResolutionError> {
        let targets = self.identities.entry(identity).or_default();
        // v20 preferred a managed row over a data source with the same
        // identity spelling. Same-kind collisions remain ambiguous.
        match &target {
            ExpandedTarget::Managed { .. } => {
                targets.retain(|existing| !matches!(existing, ExpandedTarget::DataSource { .. }))
            }
            ExpandedTarget::DataSource { .. }
                if targets
                    .iter()
                    .any(|existing| matches!(existing, ExpandedTarget::Managed { .. })) =>
            {
                return Ok(());
            }
            ExpandedTarget::DataSource { .. } | ExpandedTarget::Wait { .. } => {}
        }
        if !targets.contains(&target) {
            targets.push(target);
        }
        Ok(())
    }

    fn get(
        &self,
        reference: &AbsoluteBindingAddress,
    ) -> Result<Option<&ExpandedTarget>, ExpandedGraphResolutionError> {
        if let Some(target) = self.bindings.get(reference) {
            return Ok(Some(target));
        }
        match self.prior_bindings.get(reference).map(Vec::as_slice) {
            Some([target]) => Ok(Some(target)),
            Some([_, _, ..]) => Err(ExpandedGraphResolutionError::AmbiguousBinding {
                binding: reference.to_string(),
            }),
            Some([]) | None => Ok(None),
        }
    }

    fn contains_key(&self, reference: &AbsoluteBindingAddress) -> bool {
        self.bindings.contains_key(reference) || self.prior_bindings.contains_key(reference)
    }

    fn managed(
        &self,
        reference: &AbsoluteBindingAddress,
        owner: &str,
    ) -> Result<ResolvedHandleTarget, ExpandedGraphResolutionError> {
        match self.get(reference)? {
            Some(ExpandedTarget::Managed { id, value_binding }) => Ok(ResolvedHandleTarget {
                id: id.clone(),
                value_binding: value_binding.clone(),
            }),
            Some(ExpandedTarget::DataSource { .. }) => {
                Err(ExpandedGraphResolutionError::DataSourceHandle {
                    owner: owner.to_string(),
                    reference: reference.to_string(),
                })
            }
            Some(ExpandedTarget::Wait { .. }) => Err(ExpandedGraphResolutionError::WaitHandle {
                owner: owner.to_string(),
                reference: reference.to_string(),
            }),
            None => Err(ExpandedGraphResolutionError::MissingDependency {
                owner: owner.to_string(),
                reference: reference.to_string(),
            }),
        }
    }
}

/// Complete address index built only after module prefixing, provider routing,
/// and anonymous identity assignment have finished.
#[derive(Debug, Clone)]
pub struct ExpandedBindingIndex {
    targets: ExpandedTargets,
    /// Every bound `for` name projected to all of its expanded managed rows.
    /// Exact resource bindings remain in `targets`; this map is consulted only
    /// when an explicit dependency names the collection binding itself.
    managed_groups: HashMap<AbsoluteBindingAddress, ResolvedDependencies>,
    handle_outputs: HashMap<AbsoluteBindingAddress, ResolvedHandleTarget>,
    composition_dependencies: HashMap<AbsoluteBindingAddress, ResolvedDependencies>,
    /// Managed-row closure for every plan-scoped data-source and wait ID.
    destroy_closures: HashMap<ResolvedResourceId, PersistedDestroyDependencies>,
    /// Instance-qualified module argument values used only by the v20 state
    /// migration classifier. Literal arguments are recorded explicitly as a
    /// no-local-edge projection.
    legacy_argument_projections:
        HashMap<AbsoluteBindingAddress, PersistedDestroyDependencyProjection>,
    managed_ids: HashSet<ResolvedResourceId>,
    /// Declared upstream-state bindings are value sources, not scheduler
    /// nodes. They are classified here so value-reference provenance can be
    /// consumed without treating a known non-node as a missing dependency.
    upstream_bindings: HashSet<AbsoluteBindingAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedHandleTarget {
    id: ResolvedResourceId,
    /// Instance-qualified binding used by ordinary deferred value references.
    /// Unlike dependency identity, value resolution still addresses the
    /// managed binding's attribute map by this canonical post-expansion name.
    value_binding: AbsoluteBindingAddress,
}

/// Replace `child.handle.attribute` with an ordinary reference to the
/// concrete managed resource selected by that handle output. The structural
/// `child.handle` prefix is consumed here and can therefore never enter a
/// provider value or a saved composition.
fn rewrite_handle_output_refs<P: PlaceholderKind>(
    value: &Value<P>,
    outputs: &HashMap<AbsoluteBindingAddress, ResolvedHandleTarget>,
    owner: &str,
) -> Result<Value<P>, ExpandedGraphResolutionError> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let reference = path.binding_address().with_attribute(path.attribute());
            let Some(target) = outputs.get(&reference) else {
                return Ok(value.clone());
            };
            let Some((first, rest)) = path.segments().split_first() else {
                return Err(ExpandedGraphResolutionError::BareHandleValue {
                    owner: owner.to_string(),
                    reference: reference.to_string(),
                });
            };
            let PathSegment::Field { name } = first else {
                return Err(ExpandedGraphResolutionError::InvalidHandleValueAccess {
                    owner: owner.to_string(),
                    reference: reference.to_string(),
                    step: first.clone(),
                });
            };
            Ok(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::from_binding(
                    target.value_binding.clone(),
                    name.clone(),
                    rest.to_vec(),
                ),
            }))
        }
        Value::Concrete(ConcreteValue::List(items)) => Ok(Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|item| rewrite_handle_output_refs(item, outputs, owner))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
        Value::Concrete(ConcreteValue::Map(entries)) => Ok(Value::Concrete(ConcreteValue::Map(
            entries
                .iter()
                .map(|(key, entry)| {
                    rewrite_handle_output_refs(entry, outputs, owner)
                        .map(|rewritten| (key.clone(), rewritten))
                })
                .collect::<Result<_, _>>()?,
        ))),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Ok(Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|part| match part {
                        InterpolationPart::Literal(literal) => {
                            Ok(InterpolationPart::Literal(literal.clone()))
                        }
                        InterpolationPart::Expr(expression) => {
                            rewrite_handle_output_refs(expression, outputs, owner)
                                .map(InterpolationPart::Expr)
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )))
        }
        Value::Deferred(DeferredValue::FunctionCall { .. } | DeferredValue::Unavailable { .. }) => {
            let mut rewritten = value.clone();
            let Value::Deferred(
                DeferredValue::FunctionCall { args, .. } | DeferredValue::Unavailable { args, .. },
            ) = &mut rewritten
            else {
                unreachable!("matched a built-in call")
            };
            *args = args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| rewrite_handle_output_refs(value, outputs, owner))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rewritten)
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: functions.clone(),
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| rewrite_handle_output_refs(value, outputs, owner))
                })
                .collect::<Result<Vec<_>, _>>()?,
            source_line: *source_line,
        })),
        Value::Deferred(DeferredValue::Access(access)) => {
            let base = rewrite_handle_output_refs(access.base(), outputs, owner)?;
            access
                .rebuild_with_base(base, AccessMissPolicy::Error)
                .map_err(|error| ExpandedGraphResolutionError::ValueAccess {
                    owner: owner.to_string(),
                    error: GraphExpressionError::Access(error),
                })
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(rewrite_handle_output_refs(value, outputs, owner)?),
                default: Box::new(rewrite_handle_output_refs(default, outputs, owner)?),
            }))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Ok(Value::Deferred(
            DeferredValue::Secret(Box::new(rewrite_handle_output_refs(inner, outputs, owner)?)),
        )),
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            Ok(Value::Deferred(DeferredValue::ModuleIf(Box::new(
                deferred.try_map(|value| rewrite_handle_output_refs(value, outputs, owner))?,
            ))))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = rewrite_handle_output_refs(expression.lhs(), outputs, owner)?;
            let rhs = rewrite_handle_output_refs(expression.rhs(), outputs, owner)?;
            let expression = expression.rebuild(lhs, rhs).map_err(|error| {
                ExpandedGraphResolutionError::ValueAccess {
                    owner: owner.to_string(),
                    error: GraphExpressionError::Integer(error),
                }
            })?;
            Ok(Value::Deferred(DeferredValue::IntegerArithmetic(
                expression,
            )))
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => Ok(value.clone()),
    }
}

impl ExpandedBindingIndex {
    fn managed_target(
        &self,
        reference: &AbsoluteBindingAddress,
        owner: &str,
    ) -> Result<(ResolvedResourceId, ResourceTypePath), ExpandedGraphResolutionError> {
        if let Some(target) = self.handle_outputs.get(reference) {
            return Ok((
                target.id.clone(),
                ResourceTypePath::new(target.id.provider(), target.id.resource_type()),
            ));
        }
        let target = self.targets.managed(reference, owner)?;
        let resource_type = ResourceTypePath::new(target.id.provider(), target.id.resource_type());
        Ok((target.id, resource_type))
    }

    pub fn resolve_handle(
        &self,
        handle: &UnexpandedResourceHandle<AbsoluteBindingAddress>,
        owner: &str,
    ) -> Result<ResolvedResourceId, ExpandedGraphResolutionError> {
        let reference = handle.canonical_binding();
        let (id, actual) = self.managed_target(reference, owner)?;
        if &actual != handle.resource_type() {
            return Err(ExpandedGraphResolutionError::HandleTypeMismatch {
                owner: owner.to_string(),
                reference: handle.diagnostic_name().to_string(),
                expected: Box::new(handle.resource_type().clone()),
                actual: Box::new(actual),
            });
        }
        Ok(id)
    }

    fn resolve_explicit_operand(
        &self,
        operand: &UnexpandedDependencyOperand<AbsoluteBindingAddress>,
        owner: &str,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        match operand {
            UnexpandedDependencyOperand::Handle(handle) => {
                Ok(std::iter::once(self.resolve_handle(handle, owner)?).collect())
            }
            UnexpandedDependencyOperand::BareBinding(binding) => {
                if let Some(group) = self.managed_groups.get(binding) {
                    return Ok(group.clone());
                }
                self.managed_target(binding, owner)
                    .map(|(id, _resource_type)| std::iter::once(id).collect())
            }
            UnexpandedDependencyOperand::AttributeSelection { binding, attribute } => {
                let reference = binding.with_attribute(attribute);
                self.managed_target(&reference, owner)
                    .map(|(id, _resource_type)| std::iter::once(id).collect())
            }
        }
    }

    fn resolve_explicit_operands(
        &self,
        operands: &UnexpandedDependencies<AbsoluteBindingAddress>,
        owner: &str,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        let mut dependencies = ResolvedDependencies::default();
        for operand in operands {
            dependencies.extend(self.resolve_explicit_operand(operand, owner)?);
        }
        Ok(dependencies)
    }

    fn resolve_implicit_binding(
        &self,
        binding: &AbsoluteBindingAddress,
        owner: &str,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        if let Some(target) = self.handle_outputs.get(binding) {
            return Ok(std::iter::once(target.id.clone()).collect());
        }
        if let Some(target) = self.targets.get(binding)? {
            return Ok(std::iter::once(target.id().clone()).collect());
        }
        if let Some(dependencies) = self.composition_dependencies.get(binding) {
            return Ok(dependencies.clone());
        }
        if self.upstream_bindings.contains(binding) {
            return Ok(ResolvedDependencies::default());
        }
        Err(ExpandedGraphResolutionError::MissingDependency {
            owner: owner.to_string(),
            reference: binding.to_string(),
        })
    }

    fn project_resolved_dependencies(
        &self,
        dependencies: &ResolvedDependencies,
        owner: &str,
    ) -> Result<PersistedDestroyDependencies, ExpandedGraphResolutionError> {
        let mut projected = PersistedDestroyDependencies::default();
        for dependency in dependencies.iter() {
            if self.managed_ids.contains(dependency) {
                projected.extend_projected(&PersistedDestroyDependencies::from_graph_projection(
                    std::iter::once(dependency.clone()),
                ));
                continue;
            }
            let closure = self.destroy_closures.get(dependency).ok_or_else(|| {
                ExpandedGraphResolutionError::MissingDependency {
                    owner: owner.to_string(),
                    reference: dependency.to_string(),
                }
            })?;
            projected.extend_projected(closure);
        }
        Ok(projected)
    }

    /// Classify one legacy binding snapshot against the expanded typed graph.
    ///
    /// The result is exhaustive: external values and literal module
    /// arguments are a typed `NoLocalDestroyEdge`, while managed resources,
    /// data sources, waits, and compositions produce their managed-row
    /// closure. Missing names remain errors for the state layer to classify as
    /// configured-row failures or orphan warnings.
    pub fn project_legacy_dependency(
        &self,
        reference: &str,
        owner: &str,
    ) -> Result<PersistedDestroyDependencyProjection, ExpandedGraphResolutionError> {
        let reference = self
            .targets
            .bindings
            .keys()
            .chain(self.targets.prior_bindings.keys())
            .chain(self.targets.identities.keys())
            .chain(self.handle_outputs.keys())
            .chain(self.managed_groups.keys())
            .chain(self.composition_dependencies.keys())
            .chain(self.legacy_argument_projections.keys())
            .chain(self.upstream_bindings.iter())
            .find(|binding| binding.as_str() == reference)
            .ok_or_else(|| ExpandedGraphResolutionError::MissingDependency {
                owner: owner.to_owned(),
                reference: reference.to_owned(),
            })?;
        let direct_target = self.targets.get(reference)?;
        let legacy_target = if direct_target.is_none() {
            match self.targets.identities.get(reference).map(Vec::as_slice) {
                Some([target]) => Some(target),
                Some([_, _, ..]) => {
                    return Err(ExpandedGraphResolutionError::AmbiguousBinding {
                        binding: reference.to_string(),
                    });
                }
                Some([]) | None => None,
            }
        } else {
            None
        };
        let dependencies = self
            .handle_outputs
            .get(reference)
            .map(|target| std::iter::once(target.id.clone()).collect())
            .or_else(|| {
                direct_target
                    .or(legacy_target)
                    .map(|target| std::iter::once(target.id().clone()).collect())
            })
            .or_else(|| self.managed_groups.get(reference).cloned())
            .or_else(|| self.composition_dependencies.get(reference).cloned());
        if let Some(dependencies) = dependencies {
            return self
                .project_resolved_dependencies(&dependencies, owner)
                .map(PersistedDestroyDependencyProjection::Managed);
        }
        if let Some(projection) = self.legacy_argument_projections.get(reference) {
            return Ok(projection.clone());
        }
        if self.upstream_bindings.contains(reference) {
            return Ok(PersistedDestroyDependencyProjection::NoLocalDestroyEdge);
        }
        Err(ExpandedGraphResolutionError::MissingDependency {
            owner: owner.to_string(),
            reference: reference.to_string(),
        })
    }

    fn resolve_value_dependencies<'a, P: PlaceholderKind + 'a>(
        &self,
        values: impl Iterator<Item = &'a Value<P>>,
        snapshots: impl Iterator<Item = &'a AbsoluteBindingAddress>,
        owner: &str,
    ) -> Result<(ResolvedDependencies, ResolvedDependencyReads), ExpandedGraphResolutionError> {
        let mut dependencies = ResolvedDependencies::default();
        let mut reads = ResolvedDependencyReads::default();

        for value in values {
            let mut paths = Vec::new();
            value.visit_resource_refs(&mut |path| paths.push(path.clone()));
            for path in paths {
                let binding = path.binding_address();
                let direct =
                    self.targets.contains_key(binding) || self.handle_outputs.contains_key(binding);
                for id in self.resolve_implicit_binding(binding, owner)? {
                    dependencies.insert(id.clone());
                    if direct {
                        reads.insert_known(id, path.attribute());
                    } else {
                        reads.insert_unknown(id);
                    }
                }
            }

            let mut bare_bindings = Vec::new();
            value.visit_binding_refs(&mut |binding| bare_bindings.push(binding.clone()));
            for binding in bare_bindings {
                for id in self.resolve_implicit_binding(&binding, owner)? {
                    dependencies.insert(id.clone());
                    reads.insert_unknown(id);
                }
            }
        }

        // A snapshot may retain an edge after evaluation rewrites its source
        // value. Keep it conservatively without erasing a surviving precise
        // ResourceRef access path for the same address.
        for binding in snapshots {
            for id in self.resolve_implicit_binding(binding, owner)? {
                dependencies.insert(id.clone());
                reads.ensure_unknown(id);
            }
        }

        Ok((dependencies, reads))
    }

    pub fn resolve_resource<S>(
        &self,
        mut resource: Resource<S>,
        schemas: &SchemaRegistry,
    ) -> Result<ResolvedResource<S>, ExpandedGraphResolutionError> {
        let owner = resource
            .binding
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| resource.id.to_string());
        for value in resource.attributes.values_mut() {
            *value = rewrite_handle_output_refs(value, &self.handle_outputs, &owner)?;
        }
        resolve_resource_ignore_changes(&mut resource, schemas)?;
        let (value_dependencies, value_dependency_reads) = self.resolve_value_dependencies(
            resource.attributes.values(),
            resource.dependencies.value_bindings.iter(),
            &owner,
        )?;
        let explicit_dependencies =
            self.resolve_explicit_operands(&resource.dependencies.depends_on, &owner)?;
        let persisted_value_destroy_dependencies =
            self.project_resolved_dependencies(&value_dependencies, &owner)?;
        let persisted_explicit_destroy_dependencies =
            self.project_resolved_dependencies(&explicit_dependencies, &owner)?;
        let resource = resource.with_dependency_phase(());
        Ok(ResolvedResource::from_resolved_parts_with_reads(
            resource,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        )
        .with_persisted_destroy_dependencies(
            persisted_value_destroy_dependencies,
            persisted_explicit_destroy_dependencies,
        ))
    }

    /// Resolve an evaluated provider payload together with the authored value
    /// tree that supplied its dependency provenance.
    ///
    /// Evaluation can replace `parent.attribute` with its concrete value while
    /// retaining only a conservative dependency snapshot. Scheduling still
    /// needs the authored access path to prove that two updates touch disjoint
    /// attributes. This transition keeps the evaluated payload and takes all
    /// graph metadata from its exactly paired authored resource.
    pub fn resolve_resource_with_dependency_source<S>(
        &self,
        evaluated: Resource<S>,
        authored: Resource<S>,
        schemas: &SchemaRegistry,
    ) -> Result<ResolvedResource<S>, ExpandedGraphResolutionError> {
        if evaluated.id != authored.id {
            return Err(ExpandedGraphResolutionError::MismatchedResourcePair {
                evaluated: Box::new(evaluated.id),
                authored: Box::new(authored.id),
            });
        }

        let evaluated = self.resolve_resource(evaluated, schemas)?;
        let authored = self.resolve_resource(authored, schemas)?;
        let (evaluated, _, _, _, _, _) = evaluated.into_parts();
        let (
            _,
            value_dependencies,
            explicit_dependencies,
            persisted_value_destroy_dependencies,
            persisted_explicit_destroy_dependencies,
            value_dependency_reads,
        ) = authored.into_parts();
        Ok(ResolvedResource::from_resolved_parts_with_reads(
            evaluated,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        )
        .with_persisted_destroy_dependencies(
            persisted_value_destroy_dependencies,
            persisted_explicit_destroy_dependencies,
        ))
    }

    pub fn resolve_data_source<S>(
        &self,
        mut data_source: DataSource<S>,
    ) -> Result<ResolvedDataSource<S>, ExpandedGraphResolutionError> {
        let owner = data_source
            .binding
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| data_source.id.to_string());
        for value in data_source.attributes.values_mut() {
            *value = rewrite_handle_output_refs(value, &self.handle_outputs, &owner)?;
        }
        let (value_dependencies, value_dependency_reads) = self.resolve_value_dependencies(
            data_source.attributes.values(),
            data_source.dependencies.value_bindings.iter(),
            &owner,
        )?;
        let explicit_dependencies =
            self.resolve_explicit_operands(&data_source.dependencies.depends_on, &owner)?;
        let data_source = data_source.with_dependency_phase(());
        Ok(ResolvedDataSource::from_resolved_parts_with_reads(
            data_source,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        ))
    }

    pub fn resolve_wait_binding(
        &self,
        wait: &WaitBinding<AbsoluteBindingAddress>,
    ) -> Result<ResolvedWaitBinding, ExpandedGraphResolutionError> {
        let owner = format!("wait '{}'", wait.binding);
        let target = self.targets.managed(&wait.target, &owner)?;
        let explicit_dependencies = self.resolve_explicit_operands(&wait.depends_on, &owner)?;
        let id = wait_scheduler_id(&wait.binding);
        Ok(ResolvedWaitBinding {
            id,
            target_id: ManagedWaitTarget::from_resolved_id(target.id),
            until_raw: wait.until_raw.clone(),
            until_predicate: wait.until_predicate.clone(),
            timeout_secs: wait.timeout_secs,
            explicit_dependencies,
            line: wait.line,
        })
    }

    pub fn compile_deferred_for<S>(
        &self,
        mut template: crate::parser::DeferredForExpression<S>,
    ) -> Result<
        (
            crate::parser::CompiledDeferredForExpression<S>,
            crate::resource::DeferredDependencyTemplate,
        ),
        ExpandedGraphResolutionError,
    > {
        let owner = format!("deferred for '{}'", template.target().scheduler_key());
        template.iterable =
            rewrite_handle_output_refs(&template.iterable, &self.handle_outputs, &owner)?;
        for value in template.template_attributes_mut().values_mut() {
            *value = rewrite_handle_output_refs(value, &self.handle_outputs, &owner)?;
        }
        let mut snapshots = template.root_bindings();
        snapshots.extend(
            template
                .template_resource()
                .dependencies()
                .value_bindings
                .iter()
                .cloned(),
        );
        let (value, reads) = self.resolve_value_dependencies(
            template.attributes().values(),
            snapshots.iter(),
            &owner,
        )?;
        let explicit = self.resolve_explicit_operands(
            &template.template_resource().dependencies().depends_on,
            &owner,
        )?;
        let persisted_value = self.project_resolved_dependencies(&value, &owner)?;
        let persisted_explicit = self.project_resolved_dependencies(&explicit, &owner)?;
        Ok((
            template.into_compiled(),
            crate::resource::DeferredDependencyTemplate::from_graph(
                value,
                explicit,
                persisted_value,
                persisted_explicit,
                reads,
            ),
        ))
    }

    /// Compile a retained deferred-for template's dependency provenance into
    /// concrete graph addresses. Upstream-state roots are value inputs rather
    /// than scheduler nodes; the graph's parsed upstream declaration set is
    /// the proof that an absent scheduler target is intentional.
    pub fn resolve_deferred_for_dependencies<S>(
        &self,
        template: &crate::parser::DeferredForExpression<S>,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        let owner = format!("deferred for '{}'", template.target().scheduler_key());
        let mut dependencies = ResolvedDependencies::default();
        let mut bindings = template.root_bindings();
        bindings.extend(
            template
                .template_resource()
                .as_resource()
                .dependencies
                .value_bindings
                .iter()
                .cloned(),
        );
        for binding in bindings {
            dependencies.extend(self.resolve_implicit_binding(&binding, &owner)?);
        }
        dependencies.extend(
            self.resolve_explicit_operands(
                &template
                    .template_resource()
                    .as_resource()
                    .dependencies
                    .depends_on,
                &owner,
            )?,
        );
        Ok(dependencies)
    }
}

pub fn wait_scheduler_id(binding: &AbsoluteBindingAddress) -> ResolvedResourceId {
    let identity = ResourceIdentity::try_from(binding.to_string())
        .expect("parser binding names are non-empty scheduler identities");
    ResolvedResourceId::with_identity(format!("{SYNTHETIC_TYPE_PREFIX}wait"), identity)
}

pub fn loop_group_bindings<B: BindingAddress>(binding: &B) -> Vec<B> {
    let mut groups = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in binding.as_str().char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '[' if index > 0 => groups.push(crate::resource::binding_address::binding_collection(
                binding, index,
            )),
            _ => {}
        }
    }
    groups
}

fn resolve_pending_output(
    key: &AbsoluteBindingAddress,
    pending: &HashMap<
        AbsoluteBindingAddress,
        UnexpandedCompositionAttribute<AbsoluteBindingAddress>,
    >,
    direct_targets: &ExpandedTargets,
    resolved: &mut HashMap<AbsoluteBindingAddress, ResolvedHandleTarget>,
    visiting: &mut Vec<AbsoluteBindingAddress>,
) -> Result<ResolvedHandleTarget, ExpandedGraphResolutionError> {
    if let Some(output) = resolved.get(key) {
        return Ok(output.clone());
    }
    if visiting.iter().any(|entry| entry == key) {
        visiting.push(key.clone());
        return Err(ExpandedGraphResolutionError::HandleCycle {
            cycle: visiting
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> "),
        });
    }
    let output =
        pending
            .get(key)
            .ok_or_else(|| ExpandedGraphResolutionError::MissingDependency {
                owner: format!("module output '{key}'"),
                reference: key.to_string(),
            })?;
    visiting.push(key.clone());
    let reference = output.handle.canonical_binding();
    // An output may deliberately reuse its local resource's name, for
    // example `attributes { validation_record = validation_record }`.
    // Expansion gives both the resource and the output the qualified spelling
    // `producer.validation_record`; in that one collision the local managed
    // target is the RHS, not a self-referential output. Other pending-output
    // references continue to resolve transitively.
    let names_colliding_local_target = reference == key && direct_targets.contains_key(reference);
    let actual = if pending.contains_key(reference) && !names_colliding_local_target {
        resolve_pending_output(reference, pending, direct_targets, resolved, visiting)?
    } else {
        direct_targets.managed(reference, &format!("module output '{key}'"))?
    };
    visiting.pop();
    let actual_resource_type =
        ResourceTypePath::new(actual.id.provider(), actual.id.resource_type());
    if &actual_resource_type != output.handle.resource_type() {
        return Err(ExpandedGraphResolutionError::HandleTypeMismatch {
            owner: format!("module output '{key}'"),
            reference: output.handle.diagnostic_name().to_string(),
            expected: Box::new(output.handle.resource_type().clone()),
            actual: Box::new(actual_resource_type),
        });
    }
    let result = ResolvedHandleTarget {
        id: actual.id,
        value_binding: actual.value_binding,
    };
    resolved.insert(key.clone(), result.clone());
    Ok(result)
}

fn resolve_composition_dependencies(
    binding: &AbsoluteBindingAddress,
    raw: &HashMap<AbsoluteBindingAddress, HashSet<AbsoluteBindingAddress>>,
    targets: &ExpandedTargets,
    outputs: &HashMap<AbsoluteBindingAddress, ResolvedHandleTarget>,
    upstream_bindings: &HashSet<AbsoluteBindingAddress>,
    resolved: &mut HashMap<AbsoluteBindingAddress, ResolvedDependencies>,
    visiting: &mut Vec<AbsoluteBindingAddress>,
) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
    if let Some(dependencies) = resolved.get(binding) {
        return Ok(dependencies.clone());
    }
    if visiting.iter().any(|entry| entry == binding) {
        visiting.push(binding.clone());
        return Err(ExpandedGraphResolutionError::CompositionCycle {
            cycle: visiting
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> "),
        });
    }
    visiting.push(binding.clone());
    let mut dependencies = ResolvedDependencies::default();
    for dependency in raw.get(binding).into_iter().flatten() {
        if let Some(target) = outputs.get(dependency) {
            dependencies.insert(target.id.clone());
        } else if let Some(target) = targets.get(dependency)? {
            dependencies.insert(target.id().clone());
        } else if raw.contains_key(dependency) {
            dependencies.extend(resolve_composition_dependencies(
                dependency,
                raw,
                targets,
                outputs,
                upstream_bindings,
                resolved,
                visiting,
            )?);
        } else if upstream_bindings.contains(dependency) {
            // Upstream state is an external value source, not an effect in
            // this plan. Its declaration is the typed proof that the absent
            // scheduler target is intentional rather than a dropped edge.
        } else {
            return Err(ExpandedGraphResolutionError::MissingDependency {
                owner: format!("composition '{binding}'"),
                reference: dependency.to_string(),
            });
        }
    }
    visiting.pop();
    resolved.insert(binding.clone(), dependencies.clone());
    Ok(dependencies)
}

fn project_destroy_closure(
    id: &ResolvedResourceId,
    managed_ids: &HashSet<ResolvedResourceId>,
    ephemeral_dependencies: &HashMap<ResolvedResourceId, ResolvedDependencies>,
    projected: &mut HashMap<ResolvedResourceId, PersistedDestroyDependencies>,
    visiting: &mut Vec<ResolvedResourceId>,
) -> Result<PersistedDestroyDependencies, ExpandedGraphResolutionError> {
    if managed_ids.contains(id) {
        return Ok(PersistedDestroyDependencies::from_graph_projection(
            std::iter::once(id.clone()),
        ));
    }
    if let Some(existing) = projected.get(id) {
        return Ok(existing.clone());
    }
    if visiting.iter().any(|entry| entry == id) {
        visiting.push(id.clone());
        return Err(ExpandedGraphResolutionError::DestroyProjectionCycle {
            cycle: visiting
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> "),
        });
    }
    let dependencies = ephemeral_dependencies.get(id).ok_or_else(|| {
        ExpandedGraphResolutionError::MissingDependency {
            owner: format!("destroy projection for '{id}'"),
            reference: id.to_string(),
        }
    })?;
    visiting.push(id.clone());
    let mut closure = PersistedDestroyDependencies::default();
    for dependency in dependencies.iter() {
        closure.extend_projected(&project_destroy_closure(
            dependency,
            managed_ids,
            ephemeral_dependencies,
            projected,
            visiting,
        )?);
    }
    visiting.pop();
    projected.insert(id.clone(), closure.clone());
    Ok(closure)
}

fn require_resolved_id(
    id: &ResourceId,
) -> Result<ResolvedResourceId, ExpandedGraphResolutionError> {
    ResolvedResourceId::try_new(id.clone()).ok_or_else(|| {
        crate::resource::ResolvedResourceError::from(
            crate::resource::PendingResourceIdentityError::new(id.clone()),
        )
        .into()
    })
}

/// Configuration-only membership for legacy migration.
/// State-row candidates can participate in address lookup, but cannot mint
/// configured membership through this type.
#[derive(Debug)]
pub struct ConfiguredResources(HashSet<ResolvedResourceId>);

impl ConfiguredResources {
    pub fn from_configuration<E, D>(
        parsed: &File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>,
    ) -> Self {
        Self::collect(&parsed.resources)
    }

    fn collect<S>(resources: &[Resource<S>]) -> Self {
        Self(
            resources
                .iter()
                .filter_map(|resource| ResolvedResourceId::try_new(resource.id.clone()))
                .collect(),
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn from_test_resources<S>(resources: &[Resource<S>]) -> Self {
        Self::collect(resources)
    }

    pub fn contains(&self, id: &ResolvedResourceId) -> bool {
        self.0.contains(id)
    }
}

/// Read-only configuration metadata retained after its graph input is consumed.
/// There is deliberately no transition back to a graph-resolution input.
#[derive(Debug)]
pub struct GraphConfiguration<E, D>(File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>);

impl<E, D> GraphConfiguration<E, D> {
    /// Seal metadata for a diagnostic outcome which has no executable graph.
    pub fn for_diagnostics(
        parsed: File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>,
    ) -> Self {
        Self(parsed)
    }

    pub fn parsed(&self) -> &File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress> {
        &self.0
    }
}

/// The one constructed graph, before its deferred wait errors are checked.
pub struct LegacyClassificationGraph<E, D> {
    configuration: GraphConfiguration<E, D>,
    configured: ConfiguredResources,
    index: ExpandedBindingIndex,
    wait_error: Option<ExpandedGraphResolutionError>,
}

impl<E, D> LegacyClassificationGraph<E, D> {
    pub fn classification_index(&self) -> &ExpandedBindingIndex {
        &self.index
    }

    pub fn configured(&self) -> &ConfiguredResources {
        &self.configured
    }

    pub fn validate(self) -> Result<ResolvedExpandedGraph<E, D>, ExpandedGraphResolutionError> {
        if let Some(error) = self.wait_error {
            return Err(error);
        }
        for resource in &self.configuration.0.resources {
            let owner = resource
                .binding
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| resource.id.to_string());
            let attributes = resource
                .attributes
                .values()
                .map(|value| rewrite_handle_output_refs(value, &self.index.handle_outputs, &owner))
                .collect::<Result<Vec<_>, _>>()?;
            self.index.resolve_value_dependencies(
                attributes.iter(),
                resource.dependencies.value_bindings.iter(),
                &owner,
            )?;
            self.index
                .resolve_explicit_operands(&resource.dependencies.depends_on, &owner)?;
        }
        Ok(ResolvedExpandedGraph {
            configuration: self.configuration,
            configured: self.configured,
            index: self.index,
        })
    }
}

/// A strictly validated graph and the metadata produced by that same resolution.
#[derive(Debug)]
pub struct ResolvedExpandedGraph<E, D> {
    configuration: GraphConfiguration<E, D>,
    configured: ConfiguredResources,
    index: ExpandedBindingIndex,
}

impl<E, D> ResolvedExpandedGraph<E, D> {
    pub fn configuration(&self) -> &File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress> {
        self.configuration.parsed()
    }

    pub fn configured(&self) -> &ConfiguredResources {
        &self.configured
    }

    pub fn into_parts(self) -> (GraphConfiguration<E, D>, ExpandedBindingIndex) {
        (self.configuration, self.index)
    }
}

impl<E, D> Deref for ResolvedExpandedGraph<E, D> {
    type Target = ExpandedBindingIndex;
    fn deref(&self) -> &Self::Target {
        &self.index
    }
}

/// Configuration and persisted-only binding targets remain separate: orphan
/// references are addressable without making those rows configured resources.
pub struct ConfiguredGraphInput<E, D> {
    configuration: File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>,
    prior_orphans: crate::differ::Orphans,
}

impl<E, D> ConfiguredGraphInput<E, D> {
    /// Add persisted managed aliases without granting configured membership.
    pub fn with_prior_orphans(mut self, prior_orphans: crate::differ::Orphans) -> Self {
        self.prior_orphans = prior_orphans;
        self
    }
}

impl<E, D> From<File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>>
    for ConfiguredGraphInput<E, D>
{
    fn from(configuration: File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>) -> Self {
        Self {
            configuration,
            prior_orphans: crate::differ::Orphans::new(),
        }
    }
}

/// Resolve all module handle outputs and build the address index used by every
/// subsequent resource, data-source, and wait transition.
///
/// ```compile_fail,E0382
/// use carina_core::{parser::ExpandedParsedFile, resolved_graph::resolve_expanded_graph};
/// let parsed = ExpandedParsedFile::default();
/// let first = resolve_expanded_graph(parsed.into());
/// let second = resolve_expanded_graph(parsed.into());
/// ```
pub fn resolve_expanded_graph<E, D>(
    configuration: ConfiguredGraphInput<E, D>,
) -> Result<ResolvedExpandedGraph<E, D>, ExpandedGraphResolutionError> {
    resolve_expanded_graph_for_legacy_classification(configuration)?.validate()
}

/// Build the graph used only to classify legacy state before terminal wait
/// validation. Invalid waits remain addressable but have no destroy closure,
/// so legacy references to them fail closed as unresolved names. The strict
/// validation consumes this graph before any migrated state is released.
pub fn resolve_expanded_graph_for_legacy_classification<E, D>(
    configuration: ConfiguredGraphInput<E, D>,
) -> Result<LegacyClassificationGraph<E, D>, ExpandedGraphResolutionError> {
    let ConfiguredGraphInput {
        configuration: mut parsed,
        prior_orphans,
    } = configuration;
    let configured = ConfiguredResources::from_configuration(&parsed);
    let upstream_bindings = parsed
        .upstream_states
        .iter()
        .map(|upstream| upstream.binding.clone())
        .collect::<HashSet<_>>();
    let mut targets = ExpandedTargets::default();
    let mut managed_groups = parsed
        .parser_binding_metadata
        .structural_handles()
        .iter()
        .filter(|(_binding, handle)| handle.is_for_binding())
        .map(|(binding, _handle)| (binding.clone(), ResolvedDependencies::default()))
        .collect::<HashMap<_, _>>();
    let mut managed_ids = HashSet::new();
    for resource in &parsed.resources {
        let id = require_resolved_id(&resource.id)?;
        managed_ids.insert(id.clone());
        let target = ExpandedTarget::Managed {
            value_binding: resource
                .binding
                .clone()
                .unwrap_or_else(|| id.binding_address()),
            id: id.clone(),
        };
        if let Some(binding) = &resource.binding {
            targets.insert_binding(binding.clone(), target.clone())?;
            for group in loop_group_bindings(binding) {
                managed_groups.entry(group).or_default().insert(id.clone());
            }
        }
        targets.insert_identity(id.binding_address(), target)?;
    }
    for data_source in &parsed.data_sources {
        let id = require_resolved_id(&data_source.id)?;
        let target = ExpandedTarget::DataSource { id: id.clone() };
        if let Some(binding) = &data_source.binding {
            targets.insert_binding(binding.clone(), target.clone())?;
        }
        targets.insert_identity(id.binding_address(), target)?;
    }
    for wait in &parsed.wait_bindings {
        targets.insert_binding(
            wait.binding.clone(),
            ExpandedTarget::Wait {
                id: wait_scheduler_id(&wait.binding),
            },
        )?;
    }
    // Value resolution already exposes persisted orphan bindings during
    // refresh. Carry the same managed targets into dependency resolution,
    // with configured bindings taking precedence. Ambiguous orphan aliases
    // fail only when used, rather than invalidating unrelated graph nodes.
    let configured_non_row_bindings = parsed
        .compositions
        .iter()
        .map(|composition| {
            composition
                .binding
                .clone()
                .unwrap_or_else(|| composition.id.binding_address())
        })
        .chain(upstream_bindings.iter().cloned())
        .chain(managed_groups.keys().cloned())
        .collect::<HashSet<_>>();
    for (id, orphan) in &prior_orphans {
        let id = require_resolved_id(id)?;
        managed_ids.insert(id.clone());
        let value_binding = orphan
            .binding_address()
            .cloned()
            .unwrap_or_else(|| id.binding_address());
        let target = ExpandedTarget::Managed {
            id: id.clone(),
            value_binding: value_binding.clone(),
        };
        for binding in [value_binding, id.binding_address()] {
            if configured_non_row_bindings.contains(&binding) {
                continue;
            }
            let candidates = targets.prior_bindings.entry(binding).or_default();
            if !candidates.contains(&target) {
                candidates.push(target.clone());
            }
        }
    }

    let pending = parsed
        .parser_binding_metadata
        .unexpanded_composition_attributes
        .iter()
        .cloned()
        .map(|attribute| {
            Ok((
                parsed
                    .compositions
                    .iter()
                    .find(|composition| composition.instance == attribute.composition_instance)
                    .map(|composition| {
                        composition
                            .binding
                            .clone()
                            .unwrap_or_else(|| composition.id.binding_address())
                            .with_attribute(&attribute.name)
                    })
                    .ok_or_else(|| ExpandedGraphResolutionError::MissingComposition {
                        output: attribute.name.clone(),
                        composition: attribute.composition_instance.clone(),
                    })?,
                attribute,
            ))
        })
        .collect::<Result<HashMap<_, _>, ExpandedGraphResolutionError>>()?;
    let mut handle_outputs = HashMap::new();
    for key in pending.keys() {
        resolve_pending_output(
            key,
            &pending,
            &targets,
            &mut handle_outputs,
            &mut Vec::new(),
        )?;
    }
    for (key, attribute) in &pending {
        let target = handle_outputs
            .get(key)
            .expect("every pending output was resolved")
            .clone();
        let composition = parsed
            .compositions
            .iter_mut()
            .find(|composition| composition.instance == attribute.composition_instance)
            .ok_or_else(|| ExpandedGraphResolutionError::MissingComposition {
                output: key.to_string(),
                composition: attribute.composition_instance.clone(),
            })?;
        composition.signature.attributes.insert(
            attribute.name.clone(),
            LiveCompositionAttribute::from_handle_with_static_type(
                target.id,
                attribute.declared_type.clone(),
                attribute.source_argument.clone(),
            ),
        );
    }
    for composition in &mut parsed.compositions {
        let owner = composition
            .binding
            .as_ref()
            .map(AbsoluteBindingAddress::as_str)
            .unwrap_or(composition.instance.as_str())
            .to_string();
        for attribute in composition.signature.attributes.values_mut() {
            let Some(value) = attribute.value() else {
                continue;
            };
            let rewritten = rewrite_handle_output_refs(&value, &handle_outputs, &owner)?;
            *attribute = attribute.with_value(rewritten);
        }
    }
    parsed
        .parser_binding_metadata
        .unexpanded_composition_attributes
        .clear();

    let raw_composition_dependencies = parsed
        .compositions
        .iter()
        .filter_map(|composition| {
            composition.binding.as_ref().map(|binding| {
                (
                    binding.clone(),
                    crate::deps::get_composition_dependencies(composition),
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let mut composition_dependencies = HashMap::new();
    for binding in raw_composition_dependencies.keys() {
        resolve_composition_dependencies(
            binding,
            &raw_composition_dependencies,
            &targets,
            &handle_outputs,
            &upstream_bindings,
            &mut composition_dependencies,
            &mut Vec::new(),
        )?;
    }

    let mut index = ExpandedBindingIndex {
        targets,
        managed_groups,
        handle_outputs,
        composition_dependencies,
        destroy_closures: HashMap::new(),
        legacy_argument_projections: HashMap::new(),
        managed_ids,
        upstream_bindings,
    };

    // Build scheduler dependencies for every plan-scoped node first, then
    // recursively project those nodes to managed state rows. Managed resource
    // references are leaves: a consumer depends on that row, not recursively
    // on everything the row itself consumes.
    let mut ephemeral_dependencies = HashMap::new();
    for data_source in &parsed.data_sources {
        let owner = data_source
            .binding
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| data_source.id.to_string());
        let (mut dependencies, _reads) = index.resolve_value_dependencies(
            data_source.attributes.values(),
            data_source.dependencies.value_bindings.iter(),
            &owner,
        )?;
        dependencies
            .extend(index.resolve_explicit_operands(&data_source.dependencies.depends_on, &owner)?);
        let id = require_resolved_id(&data_source.id)?;
        ephemeral_dependencies.insert(id, dependencies);
    }
    let mut invalid_ephemeral_ids = HashSet::new();
    let mut wait_error = None;
    for wait in &parsed.wait_bindings {
        let owner = format!("wait '{}'", wait.binding);
        let id = wait_scheduler_id(&wait.binding);
        let dependencies = (|| {
            let target = index.targets.managed(&wait.target, &owner)?;
            let mut dependencies: ResolvedDependencies = std::iter::once(target.id).collect();
            dependencies.extend(index.resolve_explicit_operands(&wait.depends_on, &owner)?);
            Ok(dependencies)
        })();
        match dependencies {
            Ok(dependencies) => {
                ephemeral_dependencies.insert(id, dependencies);
            }
            Err(error) => {
                wait_error.get_or_insert(error);
                invalid_ephemeral_ids.insert(id);
            }
        }
    }
    if wait_error.is_some() {
        loop {
            let newly_invalid = ephemeral_dependencies
                .iter()
                .filter(|(id, dependencies)| {
                    !invalid_ephemeral_ids.contains(*id)
                        && dependencies
                            .iter()
                            .any(|dependency| invalid_ephemeral_ids.contains(dependency))
                })
                .map(|(id, _dependencies)| id.clone())
                .collect::<Vec<_>>();
            if newly_invalid.is_empty() {
                break;
            }
            invalid_ephemeral_ids.extend(newly_invalid);
        }
        ephemeral_dependencies.retain(|id, _dependencies| !invalid_ephemeral_ids.contains(id));
    }
    let ephemeral_ids = ephemeral_dependencies.keys().cloned().collect::<Vec<_>>();
    for id in ephemeral_ids {
        project_destroy_closure(
            &id,
            &index.managed_ids,
            &ephemeral_dependencies,
            &mut index.destroy_closures,
            &mut Vec::new(),
        )?;
    }

    // Historical module argument snapshots were sometimes stored without
    // their instance prefix. The migration layer tries the owning row's scope
    // first; these qualified aliases map that spelling back to the actual
    // call-site value and its managed closure.
    for composition in &parsed.compositions {
        for (argument_name, argument) in &composition.signature.arguments {
            let reference = composition
                .binding
                .clone()
                .unwrap_or_else(|| composition.id.binding_address())
                .with_attribute(argument_name);
            let owner = format!("module argument '{reference}'");
            let (dependencies, _reads) = match index.resolve_value_dependencies(
                std::iter::once(argument.value()),
                std::iter::empty(),
                &owner,
            ) {
                Ok(resolved) => resolved,
                // This index entry exists only for legacy-state migration.
                // An otherwise unused unresolved module argument must retain
                // its normal terminal constraint error; if a legacy row
                // actually names this argument, the classifier below will
                // still fail closed because no projection was recorded.
                Err(ExpandedGraphResolutionError::MissingDependency { .. }) => continue,
                Err(error) => return Err(error),
            };
            if dependencies
                .iter()
                .any(|dependency| invalid_ephemeral_ids.contains(dependency))
            {
                continue;
            }
            let projected = index.project_resolved_dependencies(&dependencies, &owner)?;
            let projection = if projected.is_empty() {
                PersistedDestroyDependencyProjection::NoLocalDestroyEdge
            } else {
                PersistedDestroyDependencyProjection::Managed(projected)
            };
            index
                .legacy_argument_projections
                .insert(reference, projection);
        }
    }

    Ok(LegacyClassificationGraph {
        configuration: GraphConfiguration(parsed),
        configured,
        index,
        wait_error,
    })
}

#[cfg(any(test, feature = "test-utils"))]
impl ExpandedBindingIndex {
    pub fn for_test_resources<S>(
        resources: &[Resource<S>],
    ) -> Result<Self, ExpandedGraphResolutionError> {
        let configuration = crate::parser::ExpandedParsedFile::default().with_resources_for_test(
            resources
                .iter()
                .map(|resource| Resource {
                    id: resource.id.clone(),
                    attributes: resource.attributes.clone(),
                    directives: resource.directives.clone(),
                    resolved_ignore_changes: resource.resolved_ignore_changes.clone(),
                    prefixes: resource.prefixes.clone(),
                    binding: resource.binding.clone(),
                    dependencies: resource.dependencies.clone(),
                    module_source: None,
                    default_tag_keys: resource.default_tag_keys.clone(),
                    loop_provenance: resource.loop_provenance.clone(),
                    quoted_string_attrs: resource.quoted_string_attrs.clone(),
                })
                .collect(),
        );
        resolve_expanded_graph(configuration.into()).map(|graph| graph.into_parts().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_access_error_retains_typed_cause_without_duplicate_source() {
        for cause in [
            GraphExpressionError::Access(crate::resource::ValueAccessError::IndexOutOfRange {
                index: 2,
                len: 1,
            }),
            GraphExpressionError::Integer(
                crate::integer_arithmetic::IntegerOperandError::NonInteger {
                    side: crate::integer_arithmetic::IntegerOperandSide::Left,
                    actual: "String",
                },
            ),
        ] {
            let error = ExpandedGraphResolutionError::ValueAccess {
                owner: "consumer".to_string(),
                error: cause.clone(),
            };
            assert_eq!(
                error.to_string(),
                format!(
                    "consumer: failed to rebuild value access after handle resolution: {cause}"
                )
            );
            assert!(std::error::Error::source(&error).is_none());
            let ExpandedGraphResolutionError::ValueAccess {
                error: retained, ..
            } = error
            else {
                panic!("value access failures must retain their typed cause");
            };
            assert_eq!(retained, cause);
        }
    }

    #[test]
    fn deferred_for_over_upstream_state_dotted_root_resolves_without_external_binding_set() {
        let root = crate::resource::RootScope::new();
        let absolute = |name| root.resolve(crate::resource::RelativeBindingName::new(name));
        let mut parsed = crate::parser::ExpandedInferredFile::default();
        parsed.upstream_states.push(crate::parser::UpstreamState {
            binding: absolute("orgs"),
            source: "../organizations".into(),
        });
        let graph = resolve_expanded_graph(parsed.into()).expect("upstream-only graph");
        let template = crate::parser::ExpandedDeferredForExpression::new(
            Some("main.crn".into()),
            4,
            "for name in orgs.accounts.names".into(),
            crate::parser::DeferredForTarget::bound(absolute("children")).unwrap(),
            crate::resource::ExpandedValue::root_resource_ref(
                "orgs",
                "accounts",
                vec!["names".to_string()],
            ),
            crate::parser::ForBinding::Simple("name".into()),
            Resource::with_provider("mock", "test.resource", "child", None)
                .with_absolute_binding(absolute("children")),
        );
        let dependencies = graph
            .resolve_deferred_for_dependencies(&template)
            .expect("upstream declaration proves the source");
        assert!(dependencies.is_empty());
        let (_, template) = graph
            .compile_deferred_for(template)
            .expect("upstream loop compiles");
        assert!(template.dependencies().is_empty());
    }
}
