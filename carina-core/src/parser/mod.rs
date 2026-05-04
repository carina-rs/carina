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
    ArgumentParameter, AttributeParameter, BackendConfig, DeferredForExpression, ExportParamLike,
    ExportParameter, File, FnParam, InferredExportParam, InferredFile, ModuleCall,
    ParsedExportParam, ParsedFile, ProviderConfig, RequireBlock, ResourceContext, ResourceTypePath,
    StateBlock, TypeExpr, UpstreamState, UseStatement, UserFunction, UserFunctionBody,
    ValidateExpr, ValidationBlock,
};
pub use config::{DecryptorFn, ProviderContext, ValidatorFn};
pub use entry::{parse, parse_and_resolve};
pub(crate) use entry::{parse_expression, parse_expression_eval};
pub use error::{ParseError, ParseWarning};
pub(crate) use functions::evaluate_user_function;
pub use functions::{provider_context_lookup, validate_custom_type};
pub use resolve::{
    check_identifier_scope, collect_known_bindings_merged, resolve_resource_refs,
    resolve_resource_refs_with_config,
};
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
