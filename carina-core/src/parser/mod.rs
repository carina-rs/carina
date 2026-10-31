//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

mod ast;
mod blocks;
mod config;
mod context;
mod deferred_if;
mod entry;
mod error;
mod expressions;
mod functions;
mod grammar;
mod let_binding;
mod resolve;
mod static_eval;
mod types;
mod util;

pub use crate::eval_value::StructuralHandleKind;
pub use ast::{
    ArgumentOrigin, ArgumentParameter, AttributeParameter, AttributePath, AttributePathSegment,
    AuthoredArgumentRef, BackendConfig, BareBindingKind, BareBindingUse, BindingName,
    BindingScopeId, BindingUseOwner, CallExpansion, ConsumerArgumentParameter,
    ConsumerAttributeParameter, DeferredForBody, DeferredForExpansionError, DeferredForExpression,
    DeferredForPhaseTraversal, DeferredForSite, DeferredForTarget, ExpansionKey, ExportParamLike,
    ExportParameter, File, FnParam, InferredExportParam, InferredFile, ModuleArgument, ModuleCall,
    ModuleCallDiagnosticSource, ModuleCallId, ModuleResolvedFile, ModuleResolvedInferredFile,
    ModuleResolvedParsedFile, MovedAddresses, ParsedBindingUses, ParsedDeferredForExpression,
    ParsedDeferredForPhase, ParsedExportParam, ParsedFile, ParserBindingMetadata, PendingValueSlot,
    ProviderConfig, RequireBlock, ResolvedDeferredForExpressions, ResolvedDeferredForPhase,
    ResourceAddress, ResourceContext, ResourceMut, ResourceRef, ResourceTypePath,
    SavedDeferredForExpression, ShapeMismatch, SourceFileId, SourceSpan, StateBlock,
    StateBlockAddress, TypeExpr, UntilPredicateAst, UpstreamState, UseStatement, UserFunction,
    UserFunctionBody, ValidateExpr, ValidationBlock, WaitBinding, expand_deferred_children,
};
pub(crate) use ast::{
    BareBindingSource, DeferredForResourceExpressionError, ParsedBareBindingUse,
    ParsedDeferredForIterableState,
};
pub(crate) use config::ProviderCustomTypeSource;
pub use config::{
    CustomTypeResolver, CustomTypeValidationError, CustomTypeValidationTarget,
    CustomTypeValidatorFn, DecryptorFn, IdentifiedCustomTypeDeclaration, LoadedProviderContext,
    NonEmptySchemaTypeOwners, OwnedTypeIdentity, ProviderContext, ProviderCustomTypes,
    SchemaTypeOwner,
};
#[cfg(test)]
pub(crate) use config::{fixture_custom_type_rejection, fixture_invalid_custom_type_value};
pub use deferred_if::{DeferredIfBody, DeferredIfCondition, DeferredStructuralIf, DeferredValueIf};
pub use entry::{
    AnonymousResourceSourceSpan, TopLevelBlockSourceSpan, parse_file, parse_file_and_resolve,
    top_level_anonymous_resource_spans, top_level_backend_block_spans,
    top_level_exports_block_spans, top_level_state_block_spans, top_level_upstream_state_spans,
};
pub(crate) use entry::{
    BindingSeed, parse_with_directory_context, top_level_argument_names,
    top_level_let_binding_names, top_level_user_functions_in_files,
};
#[cfg(test)]
pub(crate) use entry::{parse, parse_and_resolve, parse_with_seeded_bindings};
pub(crate) use entry::{
    parse_expression, parse_expression_eval, parse_expression_eval_with_mode,
    parse_expression_in_sink,
};
pub use error::{
    DeferredForBodyKind, DeferredForBodyUnsupportedError, DeferredForIterableResolution,
    NON_LITERAL_PREVENT_DESTROY_WARNING_MESSAGE, ParseError, ParseWarning, ParseWarningSpan,
    SINGLE_QUOTED_INTERPOLATION_WARNING_MESSAGE, UnresolvedTypeToken, WarningKind,
};
pub(crate) use expressions::string_literal::{
    display_double_quoted_literal_part, escape_double_quoted_character, unescape_string,
};
pub use expressions::string_literal::{display_string_literal, encode_string_literal};
pub use functions::{
    contains_function_call, custom_type_lookup_for_factory, validate_custom_type,
    validate_schema_custom_type,
};
pub(crate) use functions::{
    custom_type_lookup_for_schema, evaluate_resolved_user_function, function_argument_as_eval,
    validate_schema_custom_type_with_context,
};
pub use resolve::{
    ProviderConfigInvariantError, ProviderConfigResolutionError, ResolvedProviderConfig,
    ResolvedProviderConfigs, SavedProviderConfigs, check_identifier_scope,
    check_provider_instance_routing, collect_known_bindings_merged, finalize_provider_configs,
    resolve_provider_attributes_with_remote, resolve_provider_unresolved_attributes,
    resolve_resource_refs, resolve_resource_refs_with_config,
};
pub(crate) use resolve::{
    collect_seed_bindings_from_parts, reject_cyclic_let_bindings_in_variables,
    resolve_resource_refs_defer_unknown_functions, resolve_resource_refs_for_diagnostics,
    resolve_snapshot_function_calls_with_config,
};
pub use static_eval::evaluate_static_value_for_validation;
pub use types::BUILTIN_BARE_CUSTOM_TYPES;
pub use types::parse_type_expr_str;
pub(crate) use types::{resolve_known_bare_custom_type, unknown_custom_type_error};
pub(crate) use util::eval_type_name;
pub use util::{pascal_to_snake, snake_to_pascal};

pub use grammar::Rule;
pub(crate) use grammar::{PreprocessedFile, preprocess_file};

pub(crate) use blocks::module_call::parse_module_call;
pub(crate) use blocks::resource::{
    ResourceParseTarget, parse_block_contents_with_mode, parse_read_resource_expr,
    parse_resource_for_target,
};
pub(crate) use context::{ParseContext, extract_key_string, first_inner, next_pair};
pub use expressions::for_expr::ForBinding;
pub use expressions::validate_expr::CompareOp;
pub(crate) use let_binding::{LetBindingKind, LetBindingRhs, if_structural_handle};
pub(crate) use static_eval::{
    decide_if_condition, evaluate_static_value, is_static_eval, is_static_value,
};

#[cfg(test)]
mod tests;
