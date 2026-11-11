//! Module substitution/evaluation policies are kernel-private. Owners supply
//! their declaration inventories through concrete operations, never callbacks.
use super::*;
#[cfg(test)]
use crate::resource::RelativeBindingName;
use crate::{
    binding_index::{RefType, RefTypeError},
    eval_value::{BareStructuralHandle, UnexpandedStructuralHandleKind},
    module_resolver::expander::{
        ArgumentSubstitutionError, ArgumentSubstitutions, append_reference_path,
        decide_expanded_if_condition, function_substitution_error, module_parameter_postfix,
    },
    resource::{AccessMissPolicy, LoopSlot, StructuralOperandRef},
};
type ArgumentSubstitutionPosition = RewritePosition;
type ModuleResolvedValue = Value<ParserPlaceholder, AbsoluteBindingAddress>;
pub(crate) fn substitute_module_arguments<B: BindingAddress>(
    value: &Value<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
    position: RewritePosition,
) -> Result<Value<ParserPlaceholder, B>, ArgumentSubstitutionError> {
    rewrite_parser_value(value, position, &mut ModuleArgumentRewrite { arguments })
}
/// Preserve decisions from the existing selection pass without evaluating again.
pub(crate) fn select_expanded_module_ifs_with_conditions(
    value: &ModuleResolvedValue,
) -> Result<
    (
        ModuleResolvedValue,
        HashMap<crate::parser::retained::NodeId, bool>,
    ),
    ArgumentSubstitutionError,
> {
    let mut policy = ExpandedModuleRewrite {
        config: None,
        conditions: HashMap::new(),
    };
    let mut selected = rewrite_parser_value(value, RewritePosition::Value, &mut policy)?;
    selected.canonicalize_in_place();
    Ok((selected, policy.conditions))
}
pub(crate) fn evaluate_expanded_module_functions(
    value: &ModuleResolvedValue,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ModuleResolvedValue, ArgumentSubstitutionError> {
    rewrite_parser_value(
        value,
        RewritePosition::Value,
        &mut ExpandedModuleRewrite {
            config: Some(config),
            conditions: HashMap::new(),
        },
    )
}
#[cfg(test)]
pub(crate) fn select_authored_module_ifs(
    value: &Value<ParserPlaceholder, RelativeBindingName>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<Value<ParserPlaceholder, RelativeBindingName>, ArgumentSubstitutionError> {
    rewrite_parser_value(value, RewritePosition::Value, &mut SelectAuthoredIf(config))
}
struct ExpandedModuleRewrite<'a> {
    // Evaluation and conditional selection are separate passes, as before.
    config: Option<&'a dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    conditions: HashMap<crate::parser::retained::NodeId, bool>,
}

impl ParserRewrite<AbsoluteBindingAddress> for ExpandedModuleRewrite<'_> {
    type Error = ArgumentSubstitutionError;

    fn error(&mut self, error: ParseError) -> Self::Error {
        function_substitution_error(error, 0)
    }
    fn source_line(&self) -> usize {
        0
    }

    fn call(
        &mut self,
        name: crate::builtins::BuiltinFunction,
        args: &[FunctionArgument<ParserPlaceholder, AbsoluteBindingAddress>],
        source_line: DiagnosticSourceLine,
    ) -> Result<Option<ModuleResolvedValue>, Self::Error> {
        let Some(config) = self.config else {
            return Ok(None);
        };
        if !args.iter().all(|argument| {
            argument.handle().is_none()
                && argument
                    .walker_value()
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .is_ok_and(|value| value.into_known().is_ok())
        }) {
            return Ok(None);
        }
        let eval_args = args
            .iter()
            .map(crate::parser::function_argument_as_eval)
            .collect::<Vec<_>>();
        crate::builtins::evaluate_builtin_with_config(
            name.as_str(),
            &eval_args,
            config.catalog(),
            source_line,
        )
        .map_err(|message| ArgumentSubstitutionError::Evaluation {
            source_file: None,
            line: source_line.get().unwrap_or(0),
            message,
            parse_error: None,
        })?
        .into_value()
        .map(Some)
        .map_err(|error| ArgumentSubstitutionError::Evaluation {
            source_file: None,
            line: source_line.get().unwrap_or(0),
            message: error.to_string().into(),
            parse_error: None,
        })
    }
    fn resolved_call(
        &mut self,
        function: &Arc<crate::parser::UserFunction<AbsoluteBindingAddress>>,
        _functions: &Arc<HashMap<String, crate::parser::UserFunction<AbsoluteBindingAddress>>>,
        args: &[FunctionArgument<ParserPlaceholder, AbsoluteBindingAddress>],
        source_line: DiagnosticSourceLine,
    ) -> Result<Option<ModuleResolvedValue>, Self::Error> {
        let Some(config) = self.config else {
            return Ok(None);
        };
        let line = source_line.get().unwrap_or(function.body.source_line);
        crate::resolver::evaluate_module_resolved_user_function(function, args, config, line)
            .map(Some)
            .map_err(|error| function_substitution_error(error, line))
    }

    fn select(
        &mut self,
        kind: crate::value_walk::ExpressionKind,
        condition: &ModuleResolvedValue,
        location: Option<(&std::path::Path, usize)>,
    ) -> Result<Option<usize>, Self::Error> {
        if self.config.is_some() || kind != crate::value_walk::ExpressionKind::ModuleIf {
            return Ok(None);
        }
        let Some((source_file, line)) = location else {
            return Err(function_substitution_error(
                ParseError::UserFunctionError("module-if condition location is missing".into()),
                self.source_line(),
            ));
        };
        decide_expanded_if_condition(condition)
            .map(|selected| selected.map(|selected| if selected { 1 } else { 2 }))
            .map_err(|message| ArgumentSubstitutionError::Evaluation {
                source_file: Some(source_file.to_path_buf()),
                line,
                message,
                parse_error: None,
            })
    }

    fn selected_condition(
        &mut self,
        node_id: Option<crate::parser::retained::NodeId>,
        selected: bool,
    ) {
        if let Some(node_id) = node_id {
            self.conditions.insert(node_id, selected);
        }
    }

    fn access(
        &mut self,
        access: crate::resource::AccessExpr<ParserPlaceholder, AbsoluteBindingAddress>,
        base: ModuleResolvedValue,
    ) -> Result<ModuleResolvedValue, Self::Error> {
        access
            .rebuild_with_base(base, AccessMissPolicy::Error)
            .map_err(|error| ArgumentSubstitutionError::Evaluation {
                source_file: None,
                line: access.source_line(),
                message: error.to_string().into(),
                parse_error: None,
            })
    }

    fn arithmetic(
        &mut self,
        expression: crate::integer_arithmetic::ValidatedIntegerExpr<
            ParserPlaceholder,
            AbsoluteBindingAddress,
        >,
        lhs: ModuleResolvedValue,
        rhs: ModuleResolvedValue,
    ) -> Result<ModuleResolvedValue, Self::Error> {
        expression
            .evaluate_with_operands(lhs, rhs)
            .map_err(|error| ArgumentSubstitutionError::Evaluation {
                source_file: None,
                line: expression.operator_span().line().unwrap_or(0),
                message: error.to_string().into(),
                parse_error: None,
            })
    }
}

fn substitute_function_argument<B: BindingAddress>(
    argument: &crate::resource::FunctionArgument<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
) -> Result<
    Option<crate::resource::FunctionArgument<ParserPlaceholder, B>>,
    ArgumentSubstitutionError,
> {
    if let Some(name) = argument.pending_parameter()
        && let Some(substitution) = arguments.arguments.get(name)
    {
        return match substitution.value_ref() {
            StructuralOperandRef::Handle {
                handle,
                access_base: _,
            } => Ok(Some(crate::resource::FunctionArgument::from_handle(
                handle.clone(),
            ))),
            StructuralOperandRef::Value(value) => Ok(Some(
                crate::resource::FunctionArgument::from_value(value.clone()),
            )),
            StructuralOperandRef::Parameter {
                name: _,
                access_base: _,
            } => rewrite_parser_value(
                argument.walker_value(),
                ArgumentSubstitutionPosition::Value,
                &mut ModuleArgumentRewrite { arguments },
            )
            .map(crate::resource::FunctionArgument::from_value)
            .map(Some),
        };
    }

    if let Some(handle) = argument.handle()
        && let Some(substitution) = arguments.arguments.get(handle.root_binding().as_str())
    {
        return Ok(Some(match substitution.value_ref() {
            StructuralOperandRef::Handle {
                handle,
                access_base: _,
            } => crate::resource::FunctionArgument::from_handle(handle.clone()),
            StructuralOperandRef::Value(value) => {
                crate::resource::FunctionArgument::from_value(value.clone())
            }
            StructuralOperandRef::Parameter {
                name: _,
                access_base,
            } => crate::resource::FunctionArgument::from_value(access_base.clone()),
        }));
    }

    Ok(None)
}

struct ModuleArgumentRewrite<'a, B: BindingAddress> {
    arguments: ArgumentSubstitutions<'a, B>,
}

impl<B: BindingAddress> ParserRewrite<B> for ModuleArgumentRewrite<'_, B> {
    type Error = ArgumentSubstitutionError;

    fn error(&mut self, error: ParseError) -> Self::Error {
        function_substitution_error(error, self.arguments.default_line)
    }
    fn source_line(&self) -> usize {
        self.arguments.default_line
    }

    fn branches_inherit_position(&self) -> bool {
        true
    }

    fn condition_error(&mut self, error: Self::Error, file: &std::path::Path) -> Self::Error {
        error.with_source_file(file)
    }

    fn leaf(
        &mut self,
        input: &LeafInput<'_, Transform, ParserPlaceholder, B>,
        position: ArgumentSubstitutionPosition,
    ) -> Result<Option<crate::resource::Value<ParserPlaceholder, B>>, Self::Error> {
        use crate::resource::{DeferredValue, Value};
        let arguments = self.arguments;
        let mut error_location = (None, arguments.default_line);
        let substituted = match input {
            LeafInput::Placeholder(ParserPlaceholder::ModuleArgument { name, diagnostic }) => {
                let Some(argument) = arguments.arguments.get(name) else {
                    return Ok(None);
                };
                let line = diagnostic
                    .source_line()
                    .unwrap_or_else(|| argument.source_line(arguments.default_line));
                let source_file = diagnostic
                    .authored_location()
                    .map(|(source_file, _)| source_file.to_path_buf());
                error_location = (
                    source_file.clone(),
                    diagnostic.source_line().unwrap_or(arguments.default_line),
                );
                match argument.value_ref() {
                    StructuralOperandRef::Value(value) => value.clone(),
                    StructuralOperandRef::Handle { .. }
                        if matches!(position, ArgumentSubstitutionPosition::Value) =>
                    {
                        return Err(ArgumentSubstitutionError::Evaluation {
                            source_file,
                            line,
                            message: BareStructuralHandle {
                                binding: diagnostic.diagnostic_name().unwrap_or(name).to_string(),
                                kind: UnexpandedStructuralHandleKind::ModuleParameter,
                            }
                            .to_string()
                            .into(),
                            parse_error: None,
                        });
                    }
                    StructuralOperandRef::Handle { access_base, .. } => access_base.clone(),
                    StructuralOperandRef::Parameter {
                        name: _,
                        access_base,
                    } => {
                        let mut forwarded_value = access_base.clone();
                        if let Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::ModuleArgument {
                                diagnostic: forwarded_diagnostic,
                                ..
                            },
                        )) = &mut forwarded_value
                        {
                            forwarded_diagnostic.forward_from(diagnostic, name);
                        }
                        forwarded_value
                    }
                }
            }
            LeafInput::Placeholder(
                ParserPlaceholder::FnParam { name: _ }
                | ParserPlaceholder::FnLocal { name: _ }
                | ParserPlaceholder::Loop(LoopSlot::Key)
                | ParserPlaceholder::Loop(LoopSlot::Index)
                | ParserPlaceholder::Loop(LoopSlot::Value)
                | ParserPlaceholder::EmptyInterpolation { source: _ },
            ) => return Ok(None),
            LeafInput::BindingRef(binding) => {
                let Some(argument) = arguments.arguments.get(binding.as_str()) else {
                    return Ok(None);
                };
                let source_line = argument.source_line(arguments.default_line);
                argument
                    .substitution(binding.as_str(), ArgumentSubstitutionPosition::Value)
                    .map_err(|message| ArgumentSubstitutionError::Evaluation {
                        source_file: None,
                        line: source_line,
                        message: message.into(),
                        parse_error: None,
                    })?
                    .value()
                    .clone()
            }
            LeafInput::ResourceRef(path) => {
                let Some(argument) = arguments.arguments.get(path.binding()) else {
                    return Ok(None);
                };
                let source_line = argument.source_line(arguments.default_line);
                let substitution = argument
                    .substitution(path.binding(), ArgumentSubstitutionPosition::AccessBase)
                    .map_err(|message| ArgumentSubstitutionError::Evaluation {
                        source_file: None,
                        line: source_line,
                        message: message.into(),
                        parse_error: None,
                    })?;
                substitute_reference_path(substitution.value().clone(), path, source_line).map_err(
                    |error| ArgumentSubstitutionError::Evaluation {
                        source_file: None,
                        line: source_line,
                        message: error.to_string().into(),
                        parse_error: None,
                    },
                )?
            }
            LeafInput::Unknown(_) => return Ok(None),
        };
        validate_substituted_argument(
            &input.cloned_value(),
            substituted,
            arguments,
            error_location,
        )
        .map(Some)
    }

    fn argument(
        &mut self,
        argument: &crate::resource::FunctionArgument<ParserPlaceholder, B>,
        _: crate::resource::DiagnosticSourceLine,
    ) -> Result<Option<crate::resource::FunctionArgument<ParserPlaceholder, B>>, Self::Error> {
        substitute_function_argument(argument, self.arguments)
    }

    fn select(
        &mut self,
        kind: crate::value_walk::ExpressionKind,
        first: &crate::resource::Value<ParserPlaceholder, B>,
        _: Option<(&std::path::Path, usize)>,
    ) -> Result<Option<usize>, Self::Error> {
        Ok((kind == crate::value_walk::ExpressionKind::Coalesce
            && matches!(first, crate::resource::Value::Concrete(_)))
        .then_some(0))
    }

    fn access(
        &mut self,
        access: crate::resource::AccessExpr<ParserPlaceholder, B>,
        base: crate::resource::Value<ParserPlaceholder, B>,
    ) -> Result<crate::resource::Value<ParserPlaceholder, B>, Self::Error> {
        let substituted = access
            .rebuild_with_base(base, AccessMissPolicy::Error)
            .map_err(|error| ArgumentSubstitutionError::Evaluation {
                source_file: None,
                line: access.source_line(),
                message: error.to_string().into(),
                parse_error: None,
            })?;
        let error_location = (None, access.source_line());
        validate_substituted_argument(
            &crate::resource::Value::Deferred(crate::resource::DeferredValue::Access(access)),
            substituted,
            self.arguments,
            error_location,
        )
    }

    fn arithmetic(
        &mut self,
        expression: crate::integer_arithmetic::ValidatedIntegerExpr<ParserPlaceholder, B>,
        lhs: crate::resource::Value<ParserPlaceholder, B>,
        rhs: crate::resource::Value<ParserPlaceholder, B>,
    ) -> Result<crate::resource::Value<ParserPlaceholder, B>, Self::Error> {
        expression
            .evaluate_with_operands(lhs, rhs)
            .map_err(|error| ArgumentSubstitutionError::Evaluation {
                source_file: None,
                line: expression
                    .operator_span()
                    .line()
                    .unwrap_or(self.arguments.default_line),
                message: error.to_string().into(),
                parse_error: None,
            })
    }
}

fn validate_substituted_argument<B: BindingAddress>(
    value: &crate::resource::Value<ParserPlaceholder, B>,
    substituted: crate::resource::Value<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
    error_location: (Option<std::path::PathBuf>, usize),
) -> Result<crate::resource::Value<ParserPlaceholder, B>, ArgumentSubstitutionError> {
    if let Some(binding_index) = arguments.binding_index
        && let Some((argument, postfix)) = module_parameter_postfix(value, arguments.arguments)
        && let Some(reference) = arguments
            .arguments
            .get(&argument)
            .and_then(|resolved| resolved.caller_reference.clone())
        && let RefType::UnknownAttribute(
            error @ RefTypeError::InvalidAccess {
                path: _,
                step: _,
                actual: _,
            },
        ) = binding_index.postfix_ref_type(append_reference_path(reference, &postfix).as_ref())
    {
        let (source_file, line) = error_location;
        return Err(ArgumentSubstitutionError::Evaluation {
            source_file,
            line,
            message: error.to_string().into(),
            parse_error: None,
        });
    }

    Ok(substituted)
}

#[cfg(test)]
struct SelectAuthoredIf<'a>(&'a dyn crate::validation::deferred_types::ProviderTypeCheckContext);
#[cfg(test)]
impl ParserRewrite<RelativeBindingName> for SelectAuthoredIf<'_> {
    type Error = ArgumentSubstitutionError;
    fn error(&mut self, error: ParseError) -> Self::Error {
        function_substitution_error(error, 1)
    }
    fn source_line(&self) -> usize {
        1
    }
    fn select(
        &mut self,
        kind: crate::value_walk::ExpressionKind,
        condition: &Value<ParserPlaceholder, RelativeBindingName>,
        location: Option<(&std::path::Path, usize)>,
    ) -> Result<Option<usize>, Self::Error> {
        if kind != crate::value_walk::ExpressionKind::ModuleIf {
            return Ok(None);
        }
        let Some((source_file, line)) = location else {
            return Err(function_substitution_error(
                ParseError::UserFunctionError("module-if condition location is missing".into()),
                self.source_line(),
            ));
        };
        crate::parser::decide_if_condition(condition, self.0.catalog())
            .map(|selected| selected.map(|selected| if selected { 1 } else { 2 }))
            .map_err(|error| ArgumentSubstitutionError::Evaluation {
                source_file: Some(source_file.to_path_buf()),
                line,
                message: error.to_string().into(),
                parse_error: None,
            })
    }
}
