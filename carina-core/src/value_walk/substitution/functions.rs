//! Function substitution stays inside the kernel; no protected child crosses
//! an owner callback boundary.
use super::*;
use crate::{
    eval_value::{BareStructuralHandle, UnexpandedStructuralHandleKind},
    resource::Value as GenericValue,
};
type FunctionSubstitutionPosition = RewritePosition;
type ParserStageValue<B> = Value<ParserPlaceholder, B>;
type ParserStageFunctionArgument<B> = FunctionArgument<ParserPlaceholder, B>;
pub(crate) fn substitute_function_parameters<B: BindingAddress>(
    value: &ParserStageValue<B>,
    substitutions: &HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
    position: RewritePosition,
) -> Result<ParserStageValue<B>, ParseError> {
    rewrite_parser_value(
        value,
        position,
        &mut FunctionParameterRewrite {
            substitutions,
            source_line,
        },
    )
}
pub(crate) fn substitute_function_call_argument<B: BindingAddress>(
    argument: &ParserStageFunctionArgument<B>,
    substitutions: &HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
) -> Result<ParserStageFunctionArgument<B>, ParseError> {
    let mut policy = FunctionParameterRewrite {
        substitutions,
        source_line,
    };
    let mut algebra = ParserTransform::new(&mut policy, RewritePosition::Value);
    algebra.argument_line = source_line.into();
    engine::transform_argument_bound(argument.clone(), WalkRoot::syntactic().at(), &mut algebra)
        .map_err(|error| algebra.take_error(error))
}

fn substituted_function_argument<B: BindingAddress>(
    name: &str,
    argument: &ParserStageFunctionArgument<B>,
    position: FunctionSubstitutionPosition,
    source_line: usize,
) -> Result<ParserStageValue<B>, ParseError> {
    if let Some(value) = argument.value() {
        return Ok(value.clone());
    }
    match position {
        FunctionSubstitutionPosition::AccessBase => Ok(argument.walker_value().clone()),
        FunctionSubstitutionPosition::Value => Err(ParseError::from_eval_value(
            crate::eval_value::EvalValueError::BareStructuralHandle(BareStructuralHandle {
                binding: name.to_string(),
                kind: UnexpandedStructuralHandleKind::FunctionParameter,
            }),
            source_line,
        )),
    }
}

struct FunctionParameterRewrite<'a, B: BindingAddress> {
    substitutions: &'a HashMap<String, ParserStageFunctionArgument<B>>,
    source_line: usize,
}

impl<B: BindingAddress> ParserRewrite<B> for FunctionParameterRewrite<'_, B> {
    type Error = ParseError;

    fn error(&mut self, error: ParseError) -> ParseError {
        error
    }
    fn source_line(&self) -> usize {
        self.source_line
    }
    fn set_source_line(&mut self, line: usize) {
        self.source_line = line;
    }

    fn leaf(
        &mut self,
        input: &LeafInput<'_, Transform, ParserPlaceholder, B>,
        position: RewritePosition,
    ) -> Result<Option<ParserStageValue<B>>, ParseError> {
        let source_line = self.source_line;
        match input {
            LeafInput::Placeholder(
                ParserPlaceholder::FnParam { name } | ParserPlaceholder::FnLocal { name },
            ) => self
                .substitutions
                .get(name)
                .map(|argument| {
                    substituted_function_argument(name, argument, position, source_line)
                })
                .transpose(),
            LeafInput::ResourceRef(path) => {
                let Some(base) = self.substitutions.get(path.binding()) else {
                    return Ok(None);
                };
                substitute_reference_path(base.walker_value().clone(), path, source_line)
                    .map(Some)
                    .map_err(|error| ParseError::InvalidExpression {
                        line: source_line,
                        message: error.to_string(),
                    })
            }
            LeafInput::BindingRef(_)
            | LeafInput::Unknown(_)
            | LeafInput::Placeholder(
                ParserPlaceholder::ModuleArgument {
                    name: _,
                    diagnostic: _,
                }
                | ParserPlaceholder::Loop(_)
                | ParserPlaceholder::EmptyInterpolation { source: _ },
            ) => Ok(None),
        }
    }

    fn argument(
        &mut self,
        argument: &ParserStageFunctionArgument<B>,
        _source_line: crate::resource::DiagnosticSourceLine,
    ) -> Result<Option<ParserStageFunctionArgument<B>>, ParseError> {
        let Some(value) = argument.value() else {
            return Ok(None);
        };
        if let GenericValue::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::FnParam { name } | ParserPlaceholder::FnLocal { name },
        )) = value
            && let Some(substitution) = self.substitutions.get(name)
        {
            return Ok(Some(substitution.clone()));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AuthoredValue, RelativeBindingName};

    #[test]
    fn substitution_without_placeholders_keeps_arcs_and_reports_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut file = crate::parser::parse(
            "fn keep(input: String): String { 'constant' }",
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )?;
        let unchanged_expression = AuthoredValue::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![FunctionArgument::from_value(AuthoredValue::Concrete(
                ConcreteValue::String("literal".into()),
            ))],
            source_line: 3.into(),
        });
        for definition in file.user_functions.values_mut() {
            definition.body.value = unchanged_expression.clone();
        }
        let function = Arc::new(file.user_functions["keep"].clone());
        let functions = Arc::new(file.user_functions);
        let value = AuthoredValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: functions.clone(),
            args: vec![
                FunctionArgument::from_value(unchanged_expression),
                FunctionArgument::from_handle(
                    crate::eval_value::UnexpandedStructuralHandle::module_parameter(
                        "unchanged_handle",
                        crate::parser::ResourceTypePath::new("mock", "test.resource"),
                    ),
                ),
            ],
            source_line: 3.into(),
        });
        let substitutions = HashMap::new();
        let rewritten =
            substitute_function_parameters(&value, &substitutions, 3, RewritePosition::Value)?;
        assert!(matches!(
            &rewritten,
            AuthoredValue::Deferred(DeferredValue::ResolvedFunctionCall { .. })
        ));
        if let AuthoredValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function: new_function,
            functions: new_functions,
            ..
        }) = rewritten
        {
            assert!(Arc::ptr_eq(&function, &new_function));
            assert!(Arc::ptr_eq(&functions, &new_functions));
        }
        let mut policy = FunctionParameterRewrite::<RelativeBindingName> {
            substitutions: &substitutions,
            source_line: 3,
        };
        let mut algebra = ParserTransform::new(&mut policy, RewritePosition::Value);
        let product = engine::dispatch_bound(value, WalkRoot::syntactic().at(), &mut algebra)?;
        assert!(
            !product.changed,
            "an unchanged expression must not invalidate parent sharing"
        );
        Ok(())
    }
    #[test]
    fn root_argument_substitution_preserves_handles_and_secret_boundaries()
    -> Result<(), Box<dyn std::error::Error>> {
        let parameter =
            AuthoredValue::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnLocal {
                name: "local".into(),
            }));
        let handle = FunctionArgument::from_handle(
            crate::eval_value::UnexpandedStructuralHandle::module_parameter(
                "private-handle",
                crate::parser::ResourceTypePath::new("mock", "test.resource"),
            ),
        );
        let substitutions = HashMap::from([("local".into(), handle)]);
        let plain = substitute_function_call_argument(
            &FunctionArgument::from_value(parameter.clone()),
            &substitutions,
            7,
        )?;
        assert!(plain.handle().is_some());
        let protected = substitute_function_call_argument(
            &FunctionArgument::from_value(parameter.into_secret()),
            &substitutions,
            7,
        )
        .unwrap_err();
        assert!(
            protected
                .to_string()
                .contains(crate::resource::INSIDE_SECRET_MESSAGE)
        );
        assert!(!format!("{protected:?} {protected}").contains("private-handle"));
        Ok(())
    }
}
