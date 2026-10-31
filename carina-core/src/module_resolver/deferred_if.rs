//! Selection and prefixing for structural `if` templates in reusable modules.
//!
//! Nested conditional module calls are expanded by `resolver` while their
//! owning module's binding/type scope is still available. This module later
//! selects one prepared contribution at the argument-substitution seam.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::parser::{
    DeferredForBody, DeferredIfBody, DeferredIfCondition, DeferredStructuralIf, ParsedFile,
};
use crate::resource::{
    AccessMissPolicy, CompositionCall, ConcreteValue, DataSource, DeferredValue, LiveComposition,
    ModuleInstanceScope, Resource, Value,
};

use super::error::ModuleError;
use super::expander::{
    ArgumentSubstitutionError, ArgumentSubstitutionPosition, ArgumentSubstitutions,
    ParserMetadataOverlay, TemplateValueCarrier, apply_instance_prefix, build_expansion_trace,
    parsed_deferred_for_body_id, prefix_attr_value, prefix_module_composition,
    prefix_module_data_source, prefix_module_resource, prefix_parsed_deferred_for_expression,
    prefix_wait_binding, settle_prefixed_composition_constraints,
    substitute_arguments_with_position,
};
use super::resolver::{ModuleResolver, TypeResolutionPolicy};

#[derive(Debug)]
pub(super) struct SelectedDeferredIfs {
    /// Only fields contributed by selected structural branches. The imported
    /// module itself remains borrowed by the expander.
    pub(super) contribution: ParsedFile,
    /// Every binding declared by any branch, including branches not selected.
    /// The expander needs the complete set to distinguish module-local names
    /// from caller names before it rewrites references.
    pub(super) conditional_bindings: HashSet<String>,
    /// Structural conditionals whose condition has already crossed this
    /// module boundary's argument-substitution seam. Keeping these separate
    /// from parser-authored [`DeferredStructuralIf`] nodes prevents a later
    /// prefix pass from applying the same argument map twice.
    pub(super) forwarded: Vec<ForwardedStructuralIf>,
    dropped: HashMap<String, ConditionalBindingOrigin>,
}

#[derive(Debug, Clone)]
struct ConditionalBindingOrigin {
    condition: String,
    source_file: std::path::PathBuf,
    source_line: usize,
}

pub(super) enum ConditionSelection {
    Then,
    Else,
    Forwarded(SubstitutedCondition),
}

/// A condition that has consumed exactly one module-call argument map.
///
/// The value is intentionally private. Callers can either reconstitute a
/// value-level deferred condition at the substitution return boundary, or
/// apply reference prefixing for a structural condition. Neither operation
/// accepts another argument map.
#[derive(Debug)]
pub(super) struct SubstitutedCondition {
    value: Value,
    source_file: std::path::PathBuf,
    source_line: usize,
    source_expression: String,
}

impl SubstitutedCondition {
    fn new(value: Value, source: &DeferredIfCondition) -> Self {
        Self {
            value,
            source_file: source.source_file().to_path_buf(),
            source_line: source.source_line(),
            source_expression: source.source_expression().to_string(),
        }
    }

    pub(super) fn into_deferred(self) -> DeferredIfCondition {
        DeferredIfCondition::new_with_source_expression(
            self.value,
            self.source_file,
            self.source_line,
            self.source_expression,
        )
    }

    fn prefix_references(
        &self,
        instance_prefix: &str,
        intra_module_bindings: &HashSet<String>,
    ) -> DeferredIfCondition {
        DeferredIfCondition::new_with_source_expression(
            super::expander::rewrite_intra_module_refs(
                &self.value,
                instance_prefix,
                intra_module_bindings,
            ),
            self.source_file.clone(),
            self.source_line,
            self.source_expression.clone(),
        )
    }
}

#[derive(Debug)]
pub(super) struct ForwardedStructuralIf {
    condition: SubstitutedCondition,
    then_branch: DeferredIfBody,
    else_branch: Option<DeferredIfBody>,
}

impl ForwardedStructuralIf {
    fn branches(&self) -> impl Iterator<Item = &DeferredIfBody> {
        std::iter::once(&self.then_branch).chain(self.else_branch.iter())
    }
}

/// Substitute and decide one parser-template condition.
///
/// This is the only expansion-time condition evaluator. A residual module
/// argument means an outer module still owns the decision; every other
/// residual dependency is diagnosed using the parser's shared decision seam.
pub(super) fn substitute_and_decide_if(
    condition: &DeferredIfCondition,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<ConditionSelection, ArgumentSubstitutionError> {
    let substituted = substitute_arguments_with_position(
        condition.value(),
        arguments,
        ArgumentSubstitutionPosition::Value,
    )?;
    match crate::parser::decide_if_condition(&substituted, arguments.config) {
        Ok(Some(true)) => Ok(ConditionSelection::Then),
        Ok(Some(false)) => Ok(ConditionSelection::Else),
        Ok(None) => Ok(ConditionSelection::Forwarded(SubstitutedCondition::new(
            substituted,
            condition,
        ))),
        Err(error) => Err(ArgumentSubstitutionError {
            source_file: Some(condition.source_file().to_path_buf()),
            line: condition.source_line(),
            message: error.to_string(),
            parse_error: None,
        }),
    }
}

pub(super) fn select_deferred_module_ifs(
    module: &ParsedFile,
    arguments: ArgumentSubstitutions<'_>,
    contextualize: &impl Fn(ArgumentSubstitutionError) -> ModuleError,
) -> Result<SelectedDeferredIfs, ModuleError> {
    let mut origins = HashMap::new();
    collect_conditional_bindings(&module.deferred_module_ifs, &mut origins);

    let mut contribution = ParsedFile::default();
    let mut forwarded = Vec::new();
    select_ifs_into(
        &module.deferred_module_ifs,
        arguments,
        contextualize,
        &mut contribution,
        &mut origins,
        &mut forwarded,
    )?;

    let mut available = runtime_binding_names(module);
    available.extend(runtime_binding_names(&contribution));
    for deferred in &forwarded {
        for branch in deferred.branches() {
            collect_branch_binding_names(branch, &mut available);
        }
    }

    let dropped = origins
        .iter()
        .filter(|(binding, _)| !available.contains(*binding))
        .map(|(binding, origin)| (binding.clone(), origin.clone()))
        .collect::<HashMap<_, _>>();

    Ok(SelectedDeferredIfs {
        contribution,
        conditional_bindings: origins.into_keys().collect(),
        forwarded,
        dropped,
    })
}

fn select_ifs_into(
    deferred_ifs: &[DeferredStructuralIf],
    arguments: ArgumentSubstitutions<'_>,
    contextualize: &impl Fn(ArgumentSubstitutionError) -> ModuleError,
    contribution: &mut ParsedFile,
    origins: &mut HashMap<String, ConditionalBindingOrigin>,
    forwarded: &mut Vec<ForwardedStructuralIf>,
) -> Result<(), ModuleError> {
    for deferred in deferred_ifs {
        let selected = match substitute_and_decide_if(deferred.condition(), arguments)
            .map_err(contextualize)?
        {
            ConditionSelection::Then => Some(deferred.then_branch()),
            ConditionSelection::Else => deferred.else_branch(),
            ConditionSelection::Forwarded(condition) => {
                forwarded.push(ForwardedStructuralIf {
                    condition,
                    then_branch: deferred.then_branch().clone(),
                    else_branch: deferred.else_branch().cloned(),
                });
                None
            }
        };

        let Some(selected) = selected else {
            continue;
        };
        match selected {
            DeferredIfBody::Resource(resource) => {
                contribution.resources.push((**resource).clone());
            }
            DeferredIfBody::DataSource(data_source) => {
                contribution.data_sources.push((**data_source).clone());
            }
            DeferredIfBody::ExpandedModuleCall(expanded) => {
                let mut selected = expanded.as_ref().clone();
                let nested = std::mem::take(&mut selected.deferred_module_ifs);
                contribution.merge_from(selected);
                collect_conditional_bindings(&nested, origins);
                select_ifs_into(
                    &nested,
                    arguments,
                    contextualize,
                    contribution,
                    origins,
                    forwarded,
                )?;
            }
            DeferredIfBody::ModuleCall(_) => {
                return Err(ModuleError::Parse(
                    crate::parser::ParseError::InternalError {
                        expected: "a conditional module call prepared in its owning module scope"
                            .to_string(),
                        context: deferred.condition().source_file().display().to_string(),
                    },
                ));
            }
        }
    }
    Ok(())
}

fn collect_conditional_bindings(
    deferred_ifs: &[DeferredStructuralIf],
    origins: &mut HashMap<String, ConditionalBindingOrigin>,
) {
    for deferred in deferred_ifs {
        let origin = ConditionalBindingOrigin {
            condition: deferred.condition().source_expression().to_string(),
            source_file: deferred.condition().source_file().to_path_buf(),
            source_line: deferred.condition().source_line(),
        };
        for branch in deferred.branches() {
            let mut names = HashSet::new();
            collect_branch_binding_names(branch, &mut names);
            for name in names {
                origins.insert(name, origin.clone());
            }
        }
    }
}

fn collect_branch_binding_names(body: &DeferredIfBody, names: &mut HashSet<String>) {
    match body {
        DeferredIfBody::Resource(resource) => {
            names.extend(resource.binding.iter().cloned());
        }
        DeferredIfBody::DataSource(data_source) => {
            names.extend(data_source.binding.iter().cloned());
        }
        DeferredIfBody::ModuleCall(call) => {
            names.extend(call.binding_name.iter().cloned());
        }
        DeferredIfBody::ExpandedModuleCall(contribution) => {
            names.extend(runtime_binding_names(contribution));
            for deferred in &contribution.deferred_module_ifs {
                collect_branch_binding_names(deferred.then_branch(), names);
                if let Some(branch) = deferred.else_branch() {
                    collect_branch_binding_names(branch, names);
                }
            }
        }
    }
}

fn runtime_binding_names(file: &ParsedFile) -> HashSet<String> {
    file.resources
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .chain(
            file.data_sources
                .iter()
                .filter_map(|data_source| data_source.binding.clone()),
        )
        .chain(
            file.wait_bindings
                .iter()
                .map(|wait| wait.binding.as_str().to_string()),
        )
        .chain(
            file.module_calls
                .iter()
                .filter_map(|call| call.binding_name.clone()),
        )
        .chain(
            file.compositions
                .iter()
                .filter_map(|composition| composition.binding.clone()),
        )
        .collect()
}

pub(super) fn reject_dropped_references(
    selected: &SelectedDeferredIfs,
    expanded: &ParsedFile,
    instance_prefix: &str,
) -> Result<(), ModuleError> {
    if selected.dropped.is_empty() {
        return Ok(());
    }
    let prefixed = selected
        .dropped
        .iter()
        .map(|(binding, origin)| {
            (
                apply_instance_prefix(instance_prefix, binding),
                (binding, origin),
            )
        })
        .collect::<HashMap<_, _>>();
    let Some(binding) = first_dropped_reference(expanded, &prefixed) else {
        return Ok(());
    };
    let origin = selected
        .dropped
        .get(&binding)
        .expect("reported dropped binding has an origin");
    Err(ModuleError::DroppedConditionalBinding {
        binding,
        condition: origin.condition.clone(),
        source_path: origin.source_file.clone(),
        line: origin.source_line,
    })
}

fn first_dropped_reference(
    file: &ParsedFile,
    dropped: &HashMap<String, (&String, &ConditionalBindingOrigin)>,
) -> Option<String> {
    let find_binding = |binding: &str| {
        dropped
            .get(binding)
            .map(|(original, _)| (*original).clone())
    };
    let find = |value: &Value| {
        value
            .root_bindings()
            .into_iter()
            .find_map(|binding| find_binding(&binding))
    };

    for resource in &file.resources {
        if let Some(binding) = resource.attributes.values().find_map(&find) {
            return Some(binding);
        }
    }
    for data_source in &file.data_sources {
        if let Some(binding) = data_source.attributes.values().find_map(&find) {
            return Some(binding);
        }
    }
    for parameter in &file.attribute_params {
        if let Some(binding) = parameter.value.as_ref().and_then(&find) {
            return Some(binding);
        }
    }
    for composition in &file.compositions {
        for argument in composition.signature.arguments.values() {
            if let Some(binding) = find(argument.value()) {
                return Some(binding);
            }
        }
        for attribute in composition.signature.attributes.values() {
            if let Some(binding) = find(&attribute.to_value()) {
                return Some(binding);
            }
        }
    }
    for wait in &file.wait_bindings {
        if let Some(binding) = find_binding(wait.target.as_str()) {
            return Some(binding);
        }
        if let Some(binding) = wait
            .depends_on
            .iter()
            .find_map(|binding| find_binding(binding.as_str()))
        {
            return Some(binding);
        }
        if let Some(binding) = find(&wait.until_predicate.rhs) {
            return Some(binding);
        }
    }
    for deferred in file.parsed_deferred_for_expressions() {
        if let Some(binding) = find(&deferred.iterable) {
            return Some(binding);
        }
        match &deferred.body {
            DeferredForBody::Resource(resource) => {
                if let Some(binding) = resource.attributes.values().find_map(&find) {
                    return Some(binding);
                }
            }
            DeferredForBody::Read(data_source) => {
                if let Some(binding) = data_source.attributes.values().find_map(&find) {
                    return Some(binding);
                }
            }
            DeferredForBody::ModuleCall(call) => {
                let mut referenced = None;
                let _ = call.try_visit_values_for_module_if_audit(&mut |value| {
                    if let Some(binding) = find(value) {
                        referenced = Some(binding);
                        Err(())
                    } else {
                        Ok(())
                    }
                });
                if referenced.is_some() {
                    return referenced;
                }
            }
        }
    }
    let mut metadata_binding = None;
    let _ = file
        .parser_binding_metadata
        .try_visit_values(&mut |value| match find(value) {
            Some(binding) => {
                metadata_binding = Some(binding);
                Err(())
            }
            None => Ok(()),
        });
    metadata_binding
}

/// Allocation-free predicate used on hot parser-metadata paths. Error-context
/// strings are intentionally built only by the publication audit.
pub(super) fn value_contains_module_if(value: &Value) -> bool {
    value_contains_module_if_inner(value, &mut HashSet::new(), &mut HashSet::new())
}

fn value_contains_module_if_inner(
    value: &Value,
    functions: &mut HashSet<usize>,
    function_scopes: &mut HashSet<usize>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => true,
        Value::Concrete(crate::resource::ConcreteValue::List(values)) => {
            values
                .iter()
                .any(|value| value_contains_module_if_inner(value, functions, function_scopes))
        }
        Value::Concrete(crate::resource::ConcreteValue::Map(values)) => {
            values
                .values()
                .any(|value| value_contains_module_if_inner(value, functions, function_scopes))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, crate::resource::InterpolationPart::Expr(value) if value_contains_module_if_inner(value, functions, function_scopes))
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. },
        ) => args.iter().any(|argument| {
            argument
                .try_visit_values(&mut |value| {
                    if value_contains_module_if_inner(value, functions, function_scopes) {
                        Err(())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: scope,
            args,
            source_line: _,
        }) => {
            if args.iter().any(|argument| {
                argument
                    .try_visit_values(&mut |value| {
                        if value_contains_module_if_inner(value, functions, function_scopes) {
                            Err(())
                        } else {
                            Ok(())
                        }
                    })
                    .is_err()
            }) {
                return true;
            }
            let function_id = std::sync::Arc::as_ptr(function) as usize;
            if functions.insert(function_id)
                && user_function_contains_module_if(function, functions, function_scopes)
            {
                return true;
            }
            let scope_id = std::sync::Arc::as_ptr(scope) as usize;
            function_scopes.insert(scope_id)
                && scope.values().any(|function| {
                    user_function_contains_module_if(function, functions, function_scopes)
                })
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            value_contains_module_if_inner(access.base(), functions, function_scopes)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            value_contains_module_if_inner(inner, functions, function_scopes)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_contains_module_if_inner(value, functions, function_scopes)
                || value_contains_module_if_inner(default, functions, function_scopes)
        }
        Value::Concrete(_)
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => false,
    }
}

fn user_function_contains_module_if(
    function: &crate::parser::UserFunction,
    functions: &mut HashSet<usize>,
    function_scopes: &mut HashSet<usize>,
) -> bool {
    function.params.iter().any(|parameter| {
        parameter
            .default
            .as_ref()
            .is_some_and(|value| value_contains_module_if_inner(value, functions, function_scopes))
    }) || function
        .local_lets
        .iter()
        .any(|(_, value, _)| value_contains_module_if_inner(value, functions, function_scopes))
        || value_contains_module_if_inner(&function.body.value, functions, function_scopes)
}

/// Select parser-only `ModuleIf` nodes under a secret without broadening the
/// historical substitution boundary for other secret payloads.
pub(super) fn substitute_module_ifs_in_secret(
    value: &Value,
    arguments: ArgumentSubstitutions<'_>,
    position: ArgumentSubstitutionPosition,
) -> Result<Value, ArgumentSubstitutionError> {
    if matches!(value, Value::Deferred(DeferredValue::ModuleIf(_))) {
        return substitute_arguments_with_position(value, arguments, position);
    }

    match value {
        Value::Concrete(ConcreteValue::List(values)) => Ok(Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .map(|value| {
                    if value_contains_module_if(value) {
                        substitute_module_ifs_in_secret(value, arguments, position)
                    } else {
                        Ok(value.clone())
                    }
                })
                .collect::<Result<_, _>>()?,
        ))),
        Value::Concrete(ConcreteValue::Map(values)) => {
            let mut rewritten = IndexMap::with_capacity(values.len());
            for (key, value) in values {
                rewritten.insert(
                    key.clone(),
                    if value_contains_module_if(value) {
                        substitute_module_ifs_in_secret(value, arguments, position)?
                    } else {
                        value.clone()
                    },
                );
            }
            Ok(Value::Concrete(ConcreteValue::Map(rewritten)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut rewritten = parts.clone();
            for part in &mut rewritten {
                if let crate::resource::InterpolationPart::Expr(value) = part
                    && value_contains_module_if(value)
                {
                    *value = substitute_module_ifs_in_secret(value, arguments, position)?;
                }
            }
            Ok(Value::Deferred(DeferredValue::Interpolation(rewritten)))
        }
        Value::Deferred(DeferredValue::FunctionCall { .. } | DeferredValue::Unavailable { .. }) => {
            let mut rewritten = value.clone();
            let args = match &mut rewritten {
                Value::Deferred(
                    DeferredValue::FunctionCall { args, .. }
                    | DeferredValue::Unavailable { args, .. },
                ) => args,
                _ => unreachable!("matched a function-shaped value"),
            };
            for argument in args {
                *argument = argument.try_map_value(|value| {
                    if value_contains_module_if(value) {
                        substitute_module_ifs_in_secret(value, arguments, position)
                    } else {
                        Ok(value.clone())
                    }
                })?;
            }
            Ok(rewritten)
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }) => {
            substitute_arguments_with_position(value, arguments, position)
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let base = substitute_module_ifs_in_secret(access.base(), arguments, position)?;
            access
                .rebuild_with_base(base, AccessMissPolicy::Error)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: access.source_line(),
                    message: error.to_string(),
                    parse_error: None,
                })
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            Ok(Value::Deferred(DeferredValue::Secret(Box::new(
                substitute_module_ifs_in_secret(inner, arguments, position)?,
            ))))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(if value_contains_module_if(value) {
                    substitute_module_ifs_in_secret(value, arguments, position)?
                } else {
                    (**value).clone()
                }),
                default: Box::new(if value_contains_module_if(default) {
                    substitute_module_ifs_in_secret(default, arguments, position)?
                } else {
                    (**default).clone()
                }),
            }))
        }
        _ => Ok(value.clone()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prefix_forwarded_module_ifs<P: TypeResolutionPolicy>(
    resolver: &ModuleResolver<'_, P>,
    deferred_ifs: &[ForwardedStructuralIf],
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    source_metadata: &crate::parser::ParserBindingMetadata,
    selected_metadata: &crate::parser::ParserBindingMetadata,
    expanded_metadata: &mut crate::parser::ParserBindingMetadata,
    contextualize: &impl Fn(ArgumentSubstitutionError) -> ModuleError,
) -> Result<Vec<DeferredStructuralIf>, ModuleError> {
    deferred_ifs
        .iter()
        .map(|deferred| {
            let condition = deferred
                .condition
                .prefix_references(instance_prefix, intra_module_bindings);
            let then_branch = prefix_deferred_if_body(
                resolver,
                &deferred.then_branch,
                instance_prefix,
                module_name,
                module_instance_scope,
                module_call,
                intra_module_bindings,
                arguments,
                resolved_bindings,
                source_metadata,
                selected_metadata,
                expanded_metadata,
                contextualize,
            )?;
            let else_branch = deferred
                .else_branch
                .as_ref()
                .map(|branch| {
                    prefix_deferred_if_body(
                        resolver,
                        branch,
                        instance_prefix,
                        module_name,
                        module_instance_scope,
                        module_call,
                        intra_module_bindings,
                        arguments,
                        resolved_bindings,
                        source_metadata,
                        selected_metadata,
                        expanded_metadata,
                        contextualize,
                    )
                })
                .transpose()?;
            Ok(DeferredStructuralIf::new(
                condition,
                then_branch,
                else_branch,
            ))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn prefix_deferred_if_body<P: TypeResolutionPolicy>(
    resolver: &ModuleResolver<'_, P>,
    body: &DeferredIfBody,
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    source_metadata: &crate::parser::ParserBindingMetadata,
    selected_metadata: &crate::parser::ParserBindingMetadata,
    expanded_metadata: &mut crate::parser::ParserBindingMetadata,
    contextualize: &impl Fn(ArgumentSubstitutionError) -> ModuleError,
) -> Result<DeferredIfBody, ModuleError> {
    let template_metadata = ParserMetadataOverlay::new(source_metadata, selected_metadata);
    match body {
        DeferredIfBody::Resource(resource) => {
            let resource_metadata = template_metadata.resource(&resource.id);
            let expanded = prefix_module_resource(
                TemplateValueCarrier::new(resource, resource_metadata.dependency_values),
                instance_prefix,
                module_name,
                module_instance_scope,
                module_call,
                intra_module_bindings,
                arguments,
            )
            .map_err(contextualize)?;
            Ok(DeferredIfBody::Resource(Box::new(
                expanded.into_value(expanded_metadata),
            )))
        }
        DeferredIfBody::DataSource(data_source) => {
            let resource_metadata = template_metadata.resource(&data_source.id);
            let expanded = prefix_module_data_source(
                TemplateValueCarrier::new(data_source, resource_metadata.dependency_values),
                instance_prefix,
                module_name,
                module_instance_scope,
                module_call,
                intra_module_bindings,
                arguments,
            )
            .map_err(contextualize)?;
            Ok(DeferredIfBody::DataSource(Box::new(
                expanded.into_value(expanded_metadata),
            )))
        }
        DeferredIfBody::ExpandedModuleCall(contribution) => prefix_expanded_contribution(
            resolver,
            contribution,
            instance_prefix,
            module_name,
            module_instance_scope,
            module_call,
            intra_module_bindings,
            arguments,
            resolved_bindings,
            contextualize,
        )
        .map(std::sync::Arc::new)
        .map(DeferredIfBody::ExpandedModuleCall),
        DeferredIfBody::ModuleCall(_) => Err(ModuleError::Parse(
            crate::parser::ParseError::InternalError {
                expected: "a conditional module call prepared in its owning module scope"
                    .to_string(),
                context: "prefixing a forwarded structural if".to_string(),
            },
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn prefix_expanded_contribution<P: TypeResolutionPolicy>(
    resolver: &ModuleResolver<'_, P>,
    contribution: &ParsedFile,
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    contextualize: &impl Fn(ArgumentSubstitutionError) -> ModuleError,
) -> Result<ParsedFile, ModuleError> {
    // A prepared conditional module call can itself contain conditionals
    // forwarded from a deeper module. Consume the current owning module's
    // argument map exactly once before applying this instance's reference
    // prefix. Selected fields are chained below without cloning the whole
    // prepared contribution.
    let selected_ifs = select_deferred_module_ifs(contribution, arguments, contextualize)?;
    let selected = &selected_ifs.contribution;
    let template_metadata = ParserMetadataOverlay::new(
        &contribution.parser_binding_metadata,
        &selected.parser_binding_metadata,
    );
    let mut binding_uses = contribution.bare_binding_uses().clone();
    binding_uses.extend(selected.bare_binding_uses().clone());
    binding_uses
        .try_rewrite_values(|value| {
            if value_contains_module_if(value) {
                prefix_attr_value(value, instance_prefix, intra_module_bindings, arguments)
            } else {
                Ok(value.clone())
            }
        })
        .map_err(contextualize)?;

    let mut metadata = crate::parser::ParserBindingMetadata::default();
    let mut resources =
        Vec::<Resource>::with_capacity(contribution.resources.len() + selected.resources.len());
    for resource in contribution.resources.iter().chain(&selected.resources) {
        let resource_metadata = template_metadata.resource(&resource.id);
        let expanded = prefix_module_resource(
            TemplateValueCarrier::new(resource, resource_metadata.dependency_values),
            instance_prefix,
            module_name,
            module_instance_scope,
            module_call,
            intra_module_bindings,
            arguments,
        )
        .map_err(|error| {
            contextualize(error.with_optional_source_file(resource_metadata.source_file))
        })?;
        let expanded_id = expanded.value().id.clone();
        if let Some(source_file) = resource_metadata.source_file {
            metadata.insert_resource_source_file(expanded_id.clone(), source_file.to_path_buf());
        }
        binding_uses.remap_resource_owner(&resource.id, &expanded_id);
        resources.push(expanded.into_value(&mut metadata));
    }

    let mut data_sources = Vec::<DataSource>::with_capacity(
        contribution.data_sources.len() + selected.data_sources.len(),
    );
    for data_source in contribution
        .data_sources
        .iter()
        .chain(&selected.data_sources)
    {
        let resource_metadata = template_metadata.resource(&data_source.id);
        let expanded = prefix_module_data_source(
            TemplateValueCarrier::new(data_source, resource_metadata.dependency_values),
            instance_prefix,
            module_name,
            module_instance_scope,
            module_call,
            intra_module_bindings,
            arguments,
        )
        .map_err(|error| {
            contextualize(error.with_optional_source_file(resource_metadata.source_file))
        })?;
        let expanded_id = expanded.value().id.clone();
        if let Some(source_file) = resource_metadata.source_file {
            metadata.insert_resource_source_file(expanded_id.clone(), source_file.to_path_buf());
        }
        binding_uses.remap_resource_owner(&data_source.id, &expanded_id);
        data_sources.push(expanded.into_value(&mut metadata));
    }

    let mut compositions = Vec::<LiveComposition>::with_capacity(
        contribution.compositions.len() + selected.compositions.len(),
    );
    for composition in contribution
        .compositions
        .iter()
        .chain(&selected.compositions)
    {
        let mut expanded = prefix_module_composition(
            composition,
            instance_prefix,
            intra_module_bindings,
            arguments,
            module_call,
            resolver.provider_context,
            resolved_bindings,
        )
        .map_err(contextualize)?;
        if let Some(report) = settle_prefixed_composition_constraints(
            &mut expanded,
            resolver.records_constraint_reports(),
        )? {
            resolver.record_constraint_report(report);
        }
        compositions.push(expanded);
    }

    let deferred_for_expressions = contribution
        .parsed_deferred_for_expressions()
        .iter()
        .chain(selected.parsed_deferred_for_expressions())
        .map(|deferred| {
            let body_id = parsed_deferred_for_body_id(deferred);
            let resource_metadata = body_id.map(|id| template_metadata.resource(id));
            let expanded = prefix_parsed_deferred_for_expression(
                TemplateValueCarrier::new(
                    deferred,
                    resource_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.dependency_values),
                ),
                instance_prefix,
                module_name,
                module_instance_scope,
                module_call,
                intra_module_bindings,
                arguments,
            )
            .map_err(|error| {
                contextualize(
                    error.with_optional_source_file(
                        resource_metadata
                            .as_ref()
                            .and_then(|metadata| metadata.source_file),
                    ),
                )
            })?;
            if let (Some(source_id), Some(expanded_id)) =
                (body_id, parsed_deferred_for_body_id(expanded.value()))
            {
                if let Some(source_file) = resource_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.source_file)
                {
                    metadata.insert_resource_source_file(
                        expanded_id.clone(),
                        source_file.to_path_buf(),
                    );
                }
                binding_uses.remap_resource_owner(source_id, expanded_id);
            }
            Ok(expanded.into_value(&mut metadata))
        })
        .collect::<Result<Vec<_>, ModuleError>>()?;
    let deferred_module_ifs = prefix_forwarded_module_ifs(
        resolver,
        &selected_ifs.forwarded,
        instance_prefix,
        module_name,
        module_instance_scope,
        module_call,
        intra_module_bindings,
        arguments,
        resolved_bindings,
        &contribution.parser_binding_metadata,
        &selected.parser_binding_metadata,
        &mut metadata,
        contextualize,
    )?;
    let wait_bindings = contribution
        .wait_bindings
        .iter()
        .chain(&selected.wait_bindings)
        .map(|wait| prefix_wait_binding(wait, instance_prefix))
        .collect();
    let mut inner_trace = contribution.expansion_trace.clone();
    for (leaf, call_sites) in selected.expansion_trace.iter() {
        inner_trace.record(leaf.clone(), call_sites.clone());
    }
    let expansion_trace = build_expansion_trace(
        instance_prefix,
        module_call.module_source.as_deref(),
        &inner_trace,
        &resources,
        &data_sources,
    );

    let mut expanded = ParsedFile::empty_with_parsed_binding_uses(binding_uses);
    expanded.resources = resources;
    expanded.data_sources = data_sources;
    expanded.compositions = compositions;
    expanded.wait_bindings = wait_bindings;
    *expanded.parsed_deferred_for_expressions_mut() = deferred_for_expressions;
    expanded.deferred_module_ifs = deferred_module_ifs;
    expanded.parser_binding_metadata = metadata;
    expanded.warnings = contribution
        .warnings
        .iter()
        .chain(&selected.warnings)
        .cloned()
        .collect();
    expanded.expansion_trace = expansion_trace;
    reject_dropped_references(&selected_ifs, &expanded, instance_prefix)?;
    Ok(expanded)
}
