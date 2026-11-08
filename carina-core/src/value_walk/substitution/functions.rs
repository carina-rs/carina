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
                let mut substituted = crate::resource::apply_value_path_segment(
                    base.walker_value().clone(),
                    crate::resource::PathSegment::Field {
                        name: path.attribute().to_string(),
                    },
                    source_line,
                    crate::resource::AccessMissPolicy::Error,
                )
                .and_then(crate::resource::ValuePathSegmentResult::into_value)
                .map_err(|error| ParseError::InvalidExpression {
                    line: source_line,
                    message: error.to_string(),
                })?;
                for segment in path.segments() {
                    substituted = crate::resource::apply_value_path_segment(
                        substituted,
                        segment.clone(),
                        source_line,
                        crate::resource::AccessMissPolicy::Error,
                    )
                    .and_then(crate::resource::ValuePathSegmentResult::into_value)
                    .map_err(|error| ParseError::InvalidExpression {
                        line: source_line,
                        message: error.to_string(),
                    })?;
                }
                Ok(Some(substituted))
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
            return Ok(Some(argument.clone()));
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
