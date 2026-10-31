//! Binary integer-arithmetic expression parsing and constant folding.

use crate::eval_value::{EvalValue, StructuralHandleMetadata};
use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values};
use crate::parser::context::ExpressionMode;
use crate::parser::expressions::primary::parse_postfix_eval_with_mode;
use crate::parser::{ParseContext, ParseError, Rule, next_pair};
use crate::resource::{DeferredValue, Value};

pub(crate) fn parse_additive_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    parse_integer_expression_layer(
        pair,
        ctx,
        mode,
        "multiplicative expression",
        "additive expression",
        parse_multiplicative_expr_with_mode,
    )
}

pub(crate) fn parse_multiplicative_expr_with_mode(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<EvalValue, ParseError> {
    parse_integer_expression_layer(
        pair,
        ctx,
        mode,
        "postfix expression",
        "multiplicative expression",
        parse_postfix_eval_with_mode,
    )
}

fn parse_integer_expression_layer<'i>(
    pair: pest::iterators::Pair<'i, Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
    child_name: &'static str,
    context: &'static str,
    parse_child: impl Fn(
        pest::iterators::Pair<'i, Rule>,
        &ParseContext,
        &ExpressionMode<'_>,
    ) -> Result<EvalValue, ParseError>,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let first = next_pair(&mut inner, child_name, context)?;
    let has_operator = inner.clone().next().is_some();
    let operand_mode = has_operator.then(|| mode.for_integer_operand());
    let operand_mode = operand_mode.as_ref().unwrap_or(mode);
    let mut value = parse_child(first, ctx, operand_mode)?;

    while let Some(operator) = inner.next() {
        let rhs = next_pair(&mut inner, child_name, &format!("{context} rhs"))?;
        let (op, span) = parse_integer_operator(operator, ctx)?;
        let rhs = parse_child(rhs, ctx, operand_mode)?;
        value = combine_integer_operands(value, op, rhs, span)?;
    }
    Ok(value)
}

pub(crate) fn parse_integer_operator(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<(IntegerOp, IntegerOperatorSpan), ParseError> {
    let original_position = ctx.original_position(&pair)?;
    let context = match pair.as_rule() {
        Rule::add_op => "additive operator",
        Rule::multiplicative_op => "multiplicative operator",
        other => {
            return Err(ParseError::InternalError {
                expected: "named integer operator".to_string(),
                context: format!("integer expression contained {other:?}"),
            });
        }
    };
    let mut inner = pair.into_inner();
    let named = next_pair(&mut inner, "named integer operator", context)?;
    let op = match named.as_rule() {
        Rule::plus_op => IntegerOp::Add,
        Rule::minus_op => IntegerOp::Subtract,
        Rule::multiply_op => IntegerOp::Multiply,
        Rule::divide_op => IntegerOp::Divide,
        Rule::remainder_op => IntegerOp::Remainder,
        other => {
            return Err(ParseError::InternalError {
                expected: "plus, minus, multiply, divide, or remainder operator".to_string(),
                context: format!("{context} contained {other:?}"),
            });
        }
    };
    if inner.next().is_some() {
        return Err(ParseError::InternalError {
            expected: "one named integer operator".to_string(),
            context: context.to_string(),
        });
    }
    Ok((
        op,
        IntegerOperatorSpan::from_original_position(original_position),
    ))
}

pub(crate) fn combine_integer_operands(
    lhs: EvalValue,
    op: IntegerOp,
    rhs: EvalValue,
    span: IntegerOperatorSpan,
) -> Result<EvalValue, ParseError> {
    let (lhs, lhs_metadata) = lower_integer_operand(lhs, span)?;
    let (rhs, rhs_metadata) = lower_integer_operand(rhs, span)?;
    let value = evaluate_integer_values(lhs, op, rhs, span)
        .map_err(|error| arithmetic_error(op, span, error.to_string()))?;
    Ok(EvalValue::computed(
        value,
        StructuralHandleMetadata::from_expression_operands([lhs_metadata, rhs_metadata]),
    ))
}

fn lower_integer_operand(
    value: EvalValue,
    span: IntegerOperatorSpan,
) -> Result<(Value, Option<StructuralHandleMetadata>), ParseError> {
    match value {
        EvalValue::ComputedWithStructuralHandles { value, metadata } => Ok((value, Some(metadata))),
        EvalValue::ProvisionalReference { binding } => {
            let value = Value::Deferred(DeferredValue::BindingRef { binding });
            Ok((
                value.clone(),
                Some(StructuralHandleMetadata::Handle {
                    access_base: Some(value),
                }),
            ))
        }
        other => other
            .into_value()
            .map(|value| (value, None))
            .map_err(|error| arithmetic_error_text(span, error.to_string())),
    }
}

fn arithmetic_error(op: IntegerOp, span: IntegerOperatorSpan, message: String) -> ParseError {
    arithmetic_error_text(span, format!("operator '{op}': {message}"))
}

fn arithmetic_error_text(span: IntegerOperatorSpan, message: String) -> ParseError {
    ParseError::integer_arithmetic(span, 0, message)
}
