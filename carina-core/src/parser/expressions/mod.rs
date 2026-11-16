//! Expression parsers — extracted from `parser/mod.rs` per #2262 (part 1/2).
//!
//! These submodules implement the per-expression-form parsing functions
//! (primary values, pipe / compose / coalesce, for, if, validate, string
//! literals). They depend back on `super::*` for shared helpers such as
//! `ParseContext`, `parse_expression`, and the AST types — those move in
//! part 2.

pub(in crate::parser) mod dependency;
pub(super) mod for_expr;
pub(super) mod if_expr;
pub(super) mod integer_arithmetic;
pub(super) mod pipe;
pub(super) mod primary;
pub(super) mod string_literal;
pub(super) mod validate_expr;
pub(super) mod wait_expr;

/// Grammar roles which can bypass recursive precedence/primary frames.
/// New grammar rules must choose a role explicitly in this exhaustive match.
pub(super) enum EvaluationForm {
    Forwarding,
    Postfix,
    List,
    Map,
    If,
    Expression,
    Other,
}
pub(super) fn evaluation_form(rule: super::Rule) -> EvaluationForm {
    use super::Rule;
    match rule {
        Rule::coalesce_expr
        | Rule::pipe_expr
        | Rule::compose_expr
        | Rule::additive_expr
        | Rule::multiplicative_expr => EvaluationForm::Forwarding,
        Rule::postfix_expr => EvaluationForm::Postfix,
        Rule::list => EvaluationForm::List,
        Rule::map => EvaluationForm::Map,
        Rule::if_expr => EvaluationForm::If,
        Rule::expression => EvaluationForm::Expression,
        Rule::EOI
        | Rule::coalesce_separator
        | Rule::coalesce_token
        | Rule::file
        | Rule::statement
        | Rule::require_head
        | Rule::require_statement
        | Rule::upstream_state_expr
        | Rule::wait_expr
        | Rule::wait_attr
        | Rule::wait_until_attr
        | Rule::wait_timeout_attr
        | Rule::wait_depends_on_attr
        | Rule::fn_def
        | Rule::fn_params
        | Rule::fn_param
        | Rule::fn_body
        | Rule::fn_local_let
        | Rule::arguments_block
        | Rule::arguments_param
        | Rule::arguments_param_block
        | Rule::arguments_param_attr
        | Rule::arg_description_attr
        | Rule::arg_default_attr
        | Rule::arg_validation_block
        | Rule::validation_block_attr
        | Rule::validation_condition_attr
        | Rule::validation_error_message_attr
        | Rule::validate_expr
        | Rule::validate_or_expr
        | Rule::validate_and_expr
        | Rule::validate_not_expr
        | Rule::validate_comparison
        | Rule::validate_additive_expr
        | Rule::validate_multiplicative_expr
        | Rule::compare_op
        | Rule::validate_primary
        | Rule::null_literal
        | Rule::validate_function_call
        | Rule::attributes_block
        | Rule::attributes_param
        | Rule::exports_block
        | Rule::exports_param
        | Rule::type_expr
        | Rule::type_expr_atom
        | Rule::type_simple
        | Rule::type_generic
        | Rule::type_ref
        | Rule::resource_type_path
        | Rule::type_struct
        | Rule::struct_field_list
        | Rule::struct_field
        | Rule::type_string_literal
        | Rule::use_expr
        | Rule::backend_block
        | Rule::resource_address
        | Rule::import_state_block
        | Rule::import_state_attr
        | Rule::import_to_attr
        | Rule::import_id_attr
        | Rule::removed_block
        | Rule::removed_attr
        | Rule::moved_block
        | Rule::moved_attr
        | Rule::moved_from_attr
        | Rule::moved_to_attr
        | Rule::module_call
        | Rule::use_keyword
        | Rule::module_call_arg
        | Rule::anonymous_resource
        | Rule::provider_block
        | Rule::provider_expr
        | Rule::let_head
        | Rule::let_binding
        | Rule::discard_pattern
        | Rule::block_content
        | Rule::local_binding
        | Rule::attribute
        | Rule::nested_block
        | Rule::identifier
        | Rule::namespaced_id
        | Rule::reference_prefix
        | Rule::reference_field
        | Rule::numeric_tail
        | Rule::subscripted_id
        | Rule::as_keyword
        | Rule::conversion_type_path
        | Rule::add_op
        | Rule::multiplicative_op
        | Rule::plus_op
        | Rule::minus_op
        | Rule::multiply_op
        | Rule::divide_op
        | Rule::remainder_op
        | Rule::function_call
        | Rule::primary
        | Rule::for_expr
        | Rule::for_head
        | Rule::for_binding
        | Rule::for_simple_binding
        | Rule::for_indexed_binding
        | Rule::for_map_binding
        | Rule::for_iterable
        | Rule::for_iterable_primary
        | Rule::for_body
        | Rule::for_local_binding
        | Rule::if_head
        | Rule::else_clause
        | Rule::if_body
        | Rule::if_local_binding
        | Rule::map_entry
        | Rule::resource_expr
        | Rule::read_resource_expr
        | Rule::variable_ref
        | Rule::field_access
        | Rule::index_access
        | Rule::boolean
        | Rule::float
        | Rule::number
        | Rule::duration_literal
        | Rule::duration_unit
        | Rule::string
        | Rule::double_quoted_string
        | Rule::string_part
        | Rule::interpolation
        | Rule::interpolation_expr
        | Rule::inline_ws
        | Rule::string_literal
        | Rule::string_char
        | Rule::single_quoted_string
        | Rule::single_quoted_content
        | Rule::single_quoted_char
        | Rule::WHITESPACE
        | Rule::COMMENT
        | Rule::line_comment
        | Rule::block_comment
        | Rule::identifier_continue
        | Rule::binding_keyword
        | Rule::iteration_keyword
        | Rule::require_keyword
        | Rule::conditional_keyword
        | Rule::iterable_keyword
        | Rule::call_open
        | Rule::pipe_step
        | Rule::paren_open
        | Rule::paren_close
        | Rule::pipe_token
        | Rule::single_quote
        | Rule::double_quote
        | Rule::escape_start
        | Rule::interpolation_open
        | Rule::interpolation_close
        | Rule::block_comment_open
        | Rule::block_comment_close
        | Rule::block_comment_content
        | Rule::line_comment_open => EvaluationForm::Other,
    }
}
