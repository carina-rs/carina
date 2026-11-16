//! The independent provider finalization owner, consuming root-body receipts.

use super::{EditorFunctionBodyJudgements, OwnedResolutionFinding, SourceOverrides, Value};
use crate::resource::{ConcreteValue, DeferredValue, RelativeBindingName};
use crate::validation::deferred_types::ProviderTypeCheckContext;
use std::collections::HashSet;

impl EditorFunctionBodyJudgements {
    /// Finalize provider values that remain available after the root judgement.
    /// Consuming the receipts issues this independent publication at most once.
    pub fn finalize_providers(
        self,
        base_path: &crate::paths::ResolvedPath,
        overrides: &SourceOverrides,
        session: &dyn ProviderTypeCheckContext,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Option<OwnedResolutionFinding> {
        let merged = crate::config_loader::parse_directory_for_metadata_report(
            base_path,
            session.catalog(),
            overrides,
            schemas,
        )
        .ok()?;
        let merged = self.provider_metadata(merged, session);
        // This is a projected provider-value query, not authored admission.
        // It returns only provider finalization errors and publishes no AST
        // findings from its deliberately separate metadata parse.
        match crate::module_resolver::finalize_provider_metadata_with_module_overrides(
            &merged, base_path, session, schemas, overrides,
        ) {
            crate::module_resolver::ProviderMetadataResolution::Expanded(result) => {
                return result.err().map(|error| OwnedResolutionFinding { error });
            }
            crate::module_resolver::ProviderMetadataResolution::Unavailable => {}
        }
        let mut merged = merged.into_parsed();
        {
            // The primary diagnostics pass owns the module error. A provider
            // value that reads a failed module's output cannot be finalized
            // soundly, but unrelated provider values still can (and must) be
            // diagnosed. Remove only the dependent values before finalizing
            // the surviving, context-free subset.
            let module_bindings = merged
                .module_calls
                .iter()
                .filter_map(|call| call.binding_name.clone())
                .collect::<HashSet<_>>();
            let variables = merged.variables.clone();
            for provider in &mut merged.providers {
                provider.unresolved_attributes.retain(|_, value| {
                    !value_depends_on_module_output(
                        value,
                        &module_bindings,
                        &variables,
                        &mut HashSet::new(),
                    )
                });
            }
        }
        if let Err(e) = crate::parser::resolve_provider_unresolved_attributes_for_analysis(
            &mut merged,
            session.catalog(),
        ) {
            return Some(OwnedResolutionFinding { error: e });
        }
        crate::parser::finalize_provider_configs_for_analysis(&mut merged)
            .err()
            .map(|error| OwnedResolutionFinding { error })
    }
}

fn value_depends_on_module_output(
    value: &Value,
    module_bindings: &HashSet<RelativeBindingName>,
    variables: &indexmap::IndexMap<String, Value>,
    visiting_variables: &mut HashSet<RelativeBindingName>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(|value| {
                value_depends_on_module_output(
                    value,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            if module_bindings.contains(path.binding()) {
                return true;
            }
            let binding = path.binding_address();
            visiting_variables.insert(binding.clone())
                && variables.get(binding.as_str()).is_some_and(|value| {
                    value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
                })
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            if module_bindings.contains(binding) {
                return true;
            }
            visiting_variables.insert(binding.clone())
                && variables.get(binding.as_str()).is_some_and(|value| {
                    value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
                })
        }
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Concrete(ConcreteValue::Map(entries)) => entries.values().any(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                crate::resource::InterpolationPart::Expr(value)
                    if value_depends_on_module_output(
                        value,
                        module_bindings,
                        variables,
                        visiting_variables,
                    )
            )
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().any(|argument| {
            argument.value().is_some_and(|value| {
                value_depends_on_module_output(
                    value,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
            })
        }),
        Value::Deferred(DeferredValue::Access(access)) => value_depends_on_module_output(
            access.base(),
            module_bindings,
            variables,
            visiting_variables,
        ),
        Value::Deferred(DeferredValue::Secret(inner)) => inner.visit(|value| {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
        }),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            value_depends_on_module_output(value, module_bindings, variables, visiting_variables)
                || value_depends_on_module_output(
                    default,
                    module_bindings,
                    variables,
                    visiting_variables,
                )
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            value_depends_on_module_output(
                expression.lhs(),
                module_bindings,
                variables,
                visiting_variables,
            ) || value_depends_on_module_output(
                expression.rhs(),
                module_bindings,
                variables,
                visiting_variables,
            )
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
        | Value::Deferred(DeferredValue::Unknown(_) | DeferredValue::Placeholder(_)) => false,
    }
}
