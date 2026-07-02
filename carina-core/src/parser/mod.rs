//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

mod ast;
mod blocks;
mod config;
mod context;
mod entry;
mod error;
mod expressions;
mod functions;
mod let_binding;
mod resolve;
mod static_eval;
mod types;
mod util;

pub use ast::{
    ArgumentParameter, AttributeParameter, BackendConfig, BindingName, DeferredForExpression,
    ExportParamLike, ExportParameter, File, FnParam, InferredExportParam, InferredFile, ModuleCall,
    ParsedExportParam, ParsedFile, ProviderConfig, RequireBlock, ResourceContext, ResourceRef,
    ResourceTypePath, ShapeMismatch, StateBlock, StateBlockAddress, TypeExpr, UntilPredicateAst,
    UpstreamState, UseStatement, UserFunction, UserFunctionBody, ValidateExpr, ValidationBlock,
    WaitBinding, expand_deferred_children,
};
pub use config::{DecryptorFn, ProviderContext, ValidatorFn};
pub(crate) use entry::{
    BindingSeed, parse_with_seeded_bindings, parse_with_seeded_bindings_without_literal_warnings,
};
pub use entry::{parse, parse_and_resolve};
pub(crate) use entry::{parse_expression, parse_expression_eval};
pub use error::{
    ParseError, ParseWarning, ParseWarningSpan, SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE,
    WarningKind,
};
pub(crate) use functions::evaluate_user_function;
pub use functions::{provider_context_lookup, validate_custom_type};
pub use resolve::{
    check_identifier_scope, check_provider_instance_routing, collect_known_bindings_merged,
    finalize_provider_configs, resolve_provider_attributes_with_remote,
    resolve_provider_unresolved_attributes, resolve_resource_refs,
    resolve_resource_refs_with_config,
};
pub(crate) use resolve::{
    collect_seed_bindings_from_parts, reject_cyclic_let_bindings_in_variables,
};
pub use types::BUILTIN_BARE_CUSTOM_TYPES;
pub use types::parse_type_expr_str;
pub(crate) use types::{is_known_bare_custom_type, unknown_custom_type_message};
pub(crate) use util::{eval_type_name, value_type_name};
pub use util::{pascal_to_snake, snake_to_pascal};

pub(crate) use blocks::module_call::parse_module_call;
pub(crate) use blocks::resource::{
    parse_block_contents, parse_read_resource_expr, parse_resource_expr,
};
pub(crate) use context::{ParseContext, extract_key_string, first_inner, next_pair};
pub use expressions::for_expr::ForBinding;
pub use expressions::validate_expr::CompareOp;
pub(crate) use let_binding::LetBindingRhs;
pub(crate) use static_eval::{evaluate_static_value, is_static_eval, is_static_value};

use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "parser/carina.pest"]
pub(super) struct CarinaParser;

#[cfg(test)]
mod tests;
