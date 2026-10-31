//! The single fallible transition from authored dependency operands to
//! complete scheduler addresses.

use std::collections::{HashMap, HashSet};

use crate::eval_value::UnexpandedResourceHandle;
use crate::parser::{
    File, ResourceTypePath, UnexpandedCompositionAttribute, UnexpandedDependencyOperand,
    WaitBinding,
};
use crate::resource::{
    AccessMissPolicy, AccessPath, ConcreteValue, DataSource, DeferredValue, InterpolationPart,
    LiveCompositionAttribute, PathSegment, ResolvedDataSource, ResolvedDependencies,
    ResolvedDependencyReads, ResolvedResource, ResolvedResourceId, Resource, ResourceId,
    ResourceIdentity, Value, resolve_resource_ignore_changes,
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
    #[error("{owner}: failed to rebuild value access after handle resolution: {message}")]
    ValueAccess { owner: String, message: String },
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpandedTarget {
    Managed {
        id: ResolvedResourceId,
        resource_type: ResourceTypePath,
        value_binding: String,
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
    bindings: HashMap<String, ExpandedTarget>,
    identities: HashMap<String, ExpandedTarget>,
}

impl ExpandedTargets {
    fn insert_into(
        map: &mut HashMap<String, ExpandedTarget>,
        name: String,
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
                    return Err(ExpandedGraphResolutionError::AmbiguousBinding { binding: name });
                }
            }
        }
        map.insert(name, target);
        Ok(())
    }

    fn insert_binding(
        &mut self,
        binding: String,
        target: ExpandedTarget,
    ) -> Result<(), ExpandedGraphResolutionError> {
        Self::insert_into(&mut self.bindings, binding, target)
    }

    fn insert_identity(
        &mut self,
        identity: String,
        target: ExpandedTarget,
    ) -> Result<(), ExpandedGraphResolutionError> {
        Self::insert_into(&mut self.identities, identity, target)
    }

    fn get(&self, reference: &str) -> Option<&ExpandedTarget> {
        self.bindings
            .get(reference)
            .or_else(|| self.identities.get(reference))
    }

    fn contains_key(&self, reference: &str) -> bool {
        self.bindings.contains_key(reference) || self.identities.contains_key(reference)
    }
}

/// Complete address index built only after module prefixing, provider routing,
/// and anonymous identity assignment have finished.
#[derive(Debug, Clone)]
pub struct ExpandedBindingIndex {
    targets: ExpandedTargets,
    handle_outputs: HashMap<String, ResolvedHandleTarget>,
    composition_dependencies: HashMap<String, ResolvedDependencies>,
    /// Declared upstream-state bindings are value sources, not scheduler
    /// nodes. They are classified here so value-reference provenance can be
    /// consumed without treating a known non-node as a missing dependency.
    upstream_bindings: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedHandleTarget {
    id: ResolvedResourceId,
    resource_type: ResourceTypePath,
    /// Instance-qualified binding used by ordinary deferred value references.
    /// Unlike dependency identity, value resolution still addresses the
    /// managed binding's attribute map by this canonical post-expansion name.
    value_binding: String,
}

/// Replace `child.handle.attribute` with an ordinary reference to the
/// concrete managed resource selected by that handle output. The structural
/// `child.handle` prefix is consumed here and can therefore never enter a
/// provider value or a saved composition.
fn rewrite_handle_output_refs(
    value: &Value,
    outputs: &HashMap<String, ResolvedHandleTarget>,
    owner: &str,
) -> Result<Value, ExpandedGraphResolutionError> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let reference = format!("{}.{}", path.binding(), path.attribute());
            let Some(target) = outputs.get(&reference) else {
                return Ok(value.clone());
            };
            let Some((first, rest)) = path.segments().split_first() else {
                return Err(ExpandedGraphResolutionError::BareHandleValue {
                    owner: owner.to_string(),
                    reference,
                });
            };
            let PathSegment::Field { name } = first else {
                return Err(ExpandedGraphResolutionError::InvalidHandleValueAccess {
                    owner: owner.to_string(),
                    reference,
                    step: first.clone(),
                });
            };
            Ok(Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::with_segments(
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
                    message: error.to_string(),
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
            | DeferredValue::Unknown(_),
        ) => Ok(value.clone()),
    }
}

impl ExpandedBindingIndex {
    fn managed_target(
        &self,
        reference: &str,
        owner: &str,
    ) -> Result<(ResolvedResourceId, ResourceTypePath), ExpandedGraphResolutionError> {
        if let Some(target) = self.handle_outputs.get(reference) {
            return Ok((target.id.clone(), target.resource_type.clone()));
        }
        match self.targets.get(reference) {
            Some(ExpandedTarget::Managed {
                id, resource_type, ..
            }) => Ok((id.clone(), resource_type.clone())),
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

    pub fn resolve_handle(
        &self,
        handle: &UnexpandedResourceHandle,
        owner: &str,
    ) -> Result<ResolvedResourceId, ExpandedGraphResolutionError> {
        let reference = handle.canonical_binding_name();
        let (id, actual) = self.managed_target(reference.as_ref(), owner)?;
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
        operand: &UnexpandedDependencyOperand,
        owner: &str,
    ) -> Result<ResolvedResourceId, ExpandedGraphResolutionError> {
        match operand {
            UnexpandedDependencyOperand::Handle(handle) => self.resolve_handle(handle, owner),
            UnexpandedDependencyOperand::BareBinding(binding) => self
                .managed_target(binding.as_str(), owner)
                .map(|(id, _resource_type)| id),
            UnexpandedDependencyOperand::AttributeSelection { binding, attribute } => {
                let reference = format!("{binding}.{attribute}");
                self.managed_target(&reference, owner)
                    .map(|(id, _resource_type)| id)
            }
        }
    }

    fn resolve_implicit_binding(
        &self,
        binding: &str,
        owner: &str,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        if let Some(target) = self.handle_outputs.get(binding) {
            return Ok(std::iter::once(target.id.clone()).collect());
        }
        if let Some(target) = self.targets.get(binding) {
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

    fn resolve_value_dependencies<'a>(
        &self,
        values: impl Iterator<Item = &'a Value>,
        snapshots: impl Iterator<Item = &'a String>,
        owner: &str,
    ) -> Result<(ResolvedDependencies, ResolvedDependencyReads), ExpandedGraphResolutionError> {
        let mut dependencies = ResolvedDependencies::default();
        let mut reads = ResolvedDependencyReads::default();

        for value in values {
            let mut paths = Vec::new();
            value.visit_resource_refs(&mut |path| paths.push(path.clone()));
            for path in paths {
                let binding = path.binding();
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
            value.visit_binding_refs(&mut |binding| bare_bindings.push(binding.to_string()));
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
            .clone()
            .unwrap_or_else(|| resource.id.to_string());
        for value in resource.attributes.values_mut() {
            *value = rewrite_handle_output_refs(value, &self.handle_outputs, &owner)?;
        }
        resolve_resource_ignore_changes(&mut resource, schemas)?;
        let (value_dependencies, value_dependency_reads) = self.resolve_value_dependencies(
            resource.attributes.values(),
            resource.dependency_bindings.iter(),
            &owner,
        )?;
        let mut explicit_dependencies = ResolvedDependencies::default();
        for operand in &resource.directives.depends_on {
            explicit_dependencies.insert(self.resolve_explicit_operand(operand, &owner)?);
        }
        resource.dependency_bindings.clear();
        resource.directives.depends_on = Default::default();
        Ok(ResolvedResource::from_resolved_parts_with_reads(
            resource,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
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
        let (evaluated, _, _, _) = evaluated.into_parts();
        let (_, value_dependencies, explicit_dependencies, value_dependency_reads) =
            authored.into_parts();
        Ok(ResolvedResource::from_resolved_parts_with_reads(
            evaluated,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        ))
    }

    pub fn resolve_data_source<S>(
        &self,
        mut data_source: DataSource<S>,
    ) -> Result<ResolvedDataSource<S>, ExpandedGraphResolutionError> {
        let owner = data_source
            .binding
            .clone()
            .unwrap_or_else(|| data_source.id.to_string());
        for value in data_source.attributes.values_mut() {
            *value = rewrite_handle_output_refs(value, &self.handle_outputs, &owner)?;
        }
        let (value_dependencies, value_dependency_reads) = self.resolve_value_dependencies(
            data_source.attributes.values(),
            data_source.dependency_bindings.iter(),
            &owner,
        )?;
        let mut explicit_dependencies = ResolvedDependencies::default();
        for operand in &data_source.directives.depends_on {
            explicit_dependencies.insert(self.resolve_explicit_operand(operand, &owner)?);
        }
        data_source.dependency_bindings.clear();
        data_source.directives.depends_on = Default::default();
        Ok(ResolvedDataSource::from_resolved_parts_with_reads(
            data_source,
            value_dependencies,
            explicit_dependencies,
            value_dependency_reads,
        ))
    }

    pub fn resolve_wait_binding(
        &self,
        wait: &WaitBinding,
    ) -> Result<ResolvedWaitBinding, ExpandedGraphResolutionError> {
        let owner = format!("wait '{}'", wait.binding);
        let (target_id, _resource_type) = self.managed_target(wait.target.as_str(), &owner)?;
        let mut explicit_dependencies = ResolvedDependencies::default();
        for operand in &wait.depends_on {
            explicit_dependencies.insert(self.resolve_explicit_operand(operand, &owner)?);
        }
        let id = wait_scheduler_id(wait.binding.as_str());
        Ok(ResolvedWaitBinding {
            id,
            target_id: ManagedWaitTarget::from_resolved_id(target_id),
            until_raw: wait.until_raw.clone(),
            until_predicate: wait.until_predicate.clone(),
            timeout_secs: wait.timeout_secs,
            explicit_dependencies,
            line: wait.line,
        })
    }

    /// Compile a retained deferred-for template's dependency provenance into
    /// concrete graph addresses. Upstream-state roots are value inputs rather
    /// than scheduler nodes and are admitted only when the caller proves they
    /// came from the parsed upstream declaration set.
    pub fn resolve_deferred_for_dependencies<S>(
        &self,
        template: &crate::parser::DeferredForExpression<S>,
        upstream_bindings: &HashSet<String>,
    ) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
        let owner = format!("deferred for '{}'", template.target.scheduler_key());
        let mut dependencies = ResolvedDependencies::default();
        let mut bindings = template.root_bindings();
        bindings.extend(
            template
                .template_resource
                .dependency_bindings
                .iter()
                .cloned(),
        );
        for binding in bindings {
            match self.resolve_implicit_binding(&binding, &owner) {
                Ok(resolved) => dependencies.extend(resolved),
                Err(ExpandedGraphResolutionError::MissingDependency { .. })
                    if upstream_bindings.contains(&binding) => {}
                Err(error) => return Err(error),
            }
        }
        for operand in &template.template_resource.directives.depends_on {
            dependencies.insert(self.resolve_explicit_operand(operand, &owner)?);
        }
        Ok(dependencies)
    }
}

pub fn wait_scheduler_id(binding: &str) -> ResolvedResourceId {
    let identity = ResourceIdentity::try_from(binding.to_string())
        .expect("parser binding names are non-empty scheduler identities");
    ResolvedResourceId::with_identity("__wait", identity)
}

fn resource_type_of(id: &ResolvedResourceId) -> ResourceTypePath {
    ResourceTypePath::new(id.provider(), id.resource_type())
}

fn resolve_pending_output(
    key: &str,
    pending: &HashMap<String, UnexpandedCompositionAttribute>,
    direct_targets: &ExpandedTargets,
    resolved: &mut HashMap<String, ResolvedHandleTarget>,
    visiting: &mut Vec<String>,
) -> Result<ResolvedHandleTarget, ExpandedGraphResolutionError> {
    if let Some(output) = resolved.get(key) {
        return Ok(output.clone());
    }
    if visiting.iter().any(|entry| entry == key) {
        visiting.push(key.to_string());
        return Err(ExpandedGraphResolutionError::HandleCycle {
            cycle: visiting.join(" -> "),
        });
    }
    let output =
        pending
            .get(key)
            .ok_or_else(|| ExpandedGraphResolutionError::MissingDependency {
                owner: format!("module output '{key}'"),
                reference: key.to_string(),
            })?;
    visiting.push(key.to_string());
    let reference = output.handle.canonical_binding_name();
    // An output may deliberately reuse its local resource's name, for
    // example `attributes { validation_record = validation_record }`.
    // Expansion gives both the resource and the output the qualified spelling
    // `producer.validation_record`; in that one collision the local managed
    // target is the RHS, not a self-referential output. Other pending-output
    // references continue to resolve transitively.
    let names_colliding_local_target =
        reference.as_ref() == key && direct_targets.contains_key(reference.as_ref());
    let actual = if pending.contains_key(reference.as_ref()) && !names_colliding_local_target {
        resolve_pending_output(
            reference.as_ref(),
            pending,
            direct_targets,
            resolved,
            visiting,
        )?
    } else {
        match direct_targets.get(reference.as_ref()) {
            Some(ExpandedTarget::Managed {
                id,
                resource_type,
                value_binding,
            }) => ResolvedHandleTarget {
                id: id.clone(),
                resource_type: resource_type.clone(),
                value_binding: value_binding.clone(),
            },
            Some(ExpandedTarget::DataSource { .. }) => {
                return Err(ExpandedGraphResolutionError::DataSourceHandle {
                    owner: format!("module output '{key}'"),
                    reference: reference.to_string(),
                });
            }
            Some(ExpandedTarget::Wait { .. }) => {
                return Err(ExpandedGraphResolutionError::WaitHandle {
                    owner: format!("module output '{key}'"),
                    reference: reference.to_string(),
                });
            }
            None => {
                return Err(ExpandedGraphResolutionError::MissingDependency {
                    owner: format!("module output '{key}'"),
                    reference: reference.to_string(),
                });
            }
        }
    };
    visiting.pop();
    if &actual.resource_type != output.handle.resource_type() {
        return Err(ExpandedGraphResolutionError::HandleTypeMismatch {
            owner: format!("module output '{key}'"),
            reference: output.handle.diagnostic_name().to_string(),
            expected: Box::new(output.handle.resource_type().clone()),
            actual: Box::new(actual.resource_type),
        });
    }
    let result = ResolvedHandleTarget {
        id: actual.id,
        resource_type: output.handle.resource_type().clone(),
        value_binding: actual.value_binding,
    };
    resolved.insert(key.to_string(), result.clone());
    Ok(result)
}

fn resolve_composition_dependencies(
    binding: &str,
    raw: &HashMap<String, HashSet<String>>,
    targets: &ExpandedTargets,
    outputs: &HashMap<String, ResolvedHandleTarget>,
    upstream_bindings: &HashSet<String>,
    resolved: &mut HashMap<String, ResolvedDependencies>,
    visiting: &mut Vec<String>,
) -> Result<ResolvedDependencies, ExpandedGraphResolutionError> {
    if let Some(dependencies) = resolved.get(binding) {
        return Ok(dependencies.clone());
    }
    if visiting.iter().any(|entry| entry == binding) {
        visiting.push(binding.to_string());
        return Err(ExpandedGraphResolutionError::CompositionCycle {
            cycle: visiting.join(" -> "),
        });
    }
    visiting.push(binding.to_string());
    let mut dependencies = ResolvedDependencies::default();
    for dependency in raw.get(binding).into_iter().flatten() {
        if let Some(target) = outputs.get(dependency) {
            dependencies.insert(target.id.clone());
        } else if let Some(target) = targets.get(dependency) {
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
                reference: dependency.clone(),
            });
        }
    }
    visiting.pop();
    resolved.insert(binding.to_string(), dependencies.clone());
    Ok(dependencies)
}

/// Resolve all module handle outputs and build the address index used by every
/// subsequent resource, data-source, and wait transition.
pub fn resolve_expanded_graph<E>(
    parsed: &mut File<E>,
    managed: &[Resource],
    data_sources: &[DataSource],
) -> Result<ExpandedBindingIndex, ExpandedGraphResolutionError> {
    let upstream_bindings = parsed
        .upstream_binding_names()
        .into_iter()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut targets = ExpandedTargets::default();
    for resource in managed {
        let Some(id) = ResolvedResourceId::try_new(resource.id.clone()) else {
            return Err(crate::resource::ResolvedResourceError::from(
                crate::resource::PendingResourceIdentityError::new(resource.id.clone()),
            )
            .into());
        };
        let target = ExpandedTarget::Managed {
            resource_type: resource_type_of(&id),
            value_binding: resource
                .binding
                .clone()
                .unwrap_or_else(|| id.identity_str().to_string()),
            id: id.clone(),
        };
        if let Some(binding) = &resource.binding {
            targets.insert_binding(binding.clone(), target.clone())?;
        }
        targets.insert_identity(id.identity_str().to_string(), target)?;
    }
    for data_source in data_sources {
        let Some(id) = ResolvedResourceId::try_new(data_source.id.clone()) else {
            return Err(crate::resource::ResolvedResourceError::from(
                crate::resource::PendingResourceIdentityError::new(data_source.id.clone()),
            )
            .into());
        };
        let target = ExpandedTarget::DataSource { id: id.clone() };
        if let Some(binding) = &data_source.binding {
            targets.insert_binding(binding.clone(), target.clone())?;
        }
        targets.insert_identity(id.identity_str().to_string(), target)?;
    }
    for wait in &parsed.wait_bindings {
        targets.insert_binding(
            wait.binding.to_string(),
            ExpandedTarget::Wait {
                id: wait_scheduler_id(wait.binding.as_str()),
            },
        )?;
    }

    let pending = parsed
        .unexpanded_composition_attributes
        .iter()
        .cloned()
        .map(|attribute| {
            (
                format!("{}.{}", attribute.composition_instance, attribute.name),
                attribute,
            )
        })
        .collect::<HashMap<_, _>>();
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
                output: key.clone(),
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
            .as_deref()
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
    parsed.unexpanded_composition_attributes.clear();

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

    Ok(ExpandedBindingIndex {
        targets,
        handle_outputs,
        composition_dependencies,
        upstream_bindings,
    })
}
